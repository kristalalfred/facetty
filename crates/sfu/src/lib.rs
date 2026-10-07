mod room;
mod tls;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bits_proto::media::{self, AudioPacket, ClientControl, ServerControl, VideoFrame};
use bits_proto::token;
use bytes::Bytes;
use quinn::{Connection, Endpoint};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use room::Rooms;

pub use tls::Identity;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Sfu {
    endpoint: Endpoint,
    identity: Identity,
    rooms: Arc<Rooms>,
    secret: Arc<[u8]>,
}

impl Sfu {
    pub fn bind(listen: SocketAddr, secret: &[u8]) -> Result<Self> {
        let identity = Identity::generate()?;
        let endpoint = Endpoint::server(tls::server_config(&identity)?, listen)
            .with_context(|| format!("binding QUIC endpoint on {listen}"))?;
        Ok(Self {
            endpoint,
            identity,
            rooms: Arc::new(Rooms::default()),
            secret: secret.into(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    pub fn cert_sha256(&self) -> &str {
        &self.identity.sha256_hex
    }

    pub fn connections(&self) -> u32 {
        self.rooms.connections()
    }

    pub fn rooms(&self) -> Arc<Rooms> {
        self.rooms.clone()
    }

    pub async fn run(&self) {
        while let Some(incoming) = self.endpoint.accept().await {
            let rooms = self.rooms.clone();
            let secret = self.secret.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        debug!(%remote, "handshake failed: {e}");
                        return;
                    }
                };
                if let Err(e) = serve_connection(conn, rooms, &secret).await {
                    debug!(%remote, "connection ended: {e:#}");
                }
            });
        }
    }
}

async fn serve_connection(conn: Connection, rooms: Arc<Rooms>, secret: &[u8]) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(HELLO_TIMEOUT, conn.accept_bi())
        .await
        .context("no control stream")??;
    let hello = tokio::time::timeout(HELLO_TIMEOUT, media::read_control(&mut recv))
        .await
        .context("no hello")??;
    let Some(ClientControl::Hello { token }) = hello else {
        bail!("expected hello");
    };
    let claims = match token::verify(secret, &token, unix_now()) {
        Ok(c) => c,
        Err(e) => {
            let _ = media::write_control(
                &mut send,
                &ServerControl::Error {
                    message: e.to_string(),
                },
            )
            .await;
            let _ = send.finish();
            bail!("rejected token: {e}");
        }
    };
    let (room, me) = (claims.room, claims.participant);
    info!(%room, participant = me, remote = %conn.remote_address(), "joined");

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    control_tx.send(ServerControl::Welcome { participant: me })?;
    let membership = rooms.join(&room, me, conn.clone(), control_tx);

    let writer = tokio::spawn(async move {
        while let Some(msg) = control_rx.recv().await {
            if media::write_control(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });

    let result = tokio::select! {
        r = read_control(&mut recv, &membership) => r,
        r = read_video(&conn, &membership) => r,
        r = read_audio(&conn, &membership) => r,
    };
    writer.abort();
    drop(membership);
    info!(%room, participant = me, "left");
    result
}

async fn read_control(recv: &mut quinn::RecvStream, membership: &room::Membership) -> Result<()> {
    while let Some(msg) = media::read_control::<_, ClientControl>(recv).await? {
        match msg {
            ClientControl::Subscribe { publisher, rung } => membership.subscribe(publisher, rung),
            ClientControl::Hello { .. } => warn!("duplicate hello"),
        }
    }
    Ok(())
}

async fn read_video(conn: &Connection, membership: &room::Membership) -> Result<()> {
    loop {
        let mut stream = conn.accept_uni().await?;
        let bytes = match stream.read_to_end(media::MAX_VIDEO_FRAME).await {
            Ok(b) => b,
            Err(e) => {
                debug!("dropped video stream: {e}");
                continue;
            }
        };
        let mut frame: VideoFrame = media::decode(&bytes)?;
        frame.publisher = membership.participant();
        membership.publish_video(frame.rung, Bytes::from(media::encode(&frame)));
    }
}

async fn read_audio(conn: &Connection, membership: &room::Membership) -> Result<()> {
    loop {
        let datagram = conn.read_datagram().await?;
        let Ok(mut packet) = media::decode::<AudioPacket>(&datagram) else {
            continue;
        };
        packet.publisher = membership.participant();
        membership.publish_audio(Bytes::from(media::encode(&packet)));
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
