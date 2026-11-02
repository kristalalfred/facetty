mod room;
mod tls;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use facetty_proto::media::{self, AudioPacket, ClientControl, ServerControl, VideoFrame};
use facetty_proto::{quic, token};
use quinn::Endpoint;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use web_transport_quinn::Session;

use room::Rooms;

pub use tls::Identity;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Sfu {
    endpoint: Endpoint,
    certificate: watch::Sender<String>,
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
            certificate: watch::Sender::new(identity.sha256_hex),
            rooms: Arc::new(Rooms::default()),
            secret: secret.into(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Claude: hex SHA-256 of the certificate new connections get. It changes
    /// daily.
    pub fn certificate(&self) -> watch::Receiver<String> {
        self.certificate.subscribe()
    }

    pub fn rooms(&self) -> Arc<Rooms> {
        self.rooms.clone()
    }

    pub async fn run(&self) {
        tokio::select! {
            () = self.accept() => {}
            () = self.rotate_certificates() => {}
        }
    }

    async fn accept(&self) {
        while let Some(incoming) = self.endpoint.accept().await {
            let rooms = self.rooms.clone();
            let secret = self.secret.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                let session =
                    match tokio::time::timeout(HELLO_TIMEOUT, open_session(incoming)).await {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            debug!(%remote, "handshake failed: {e:#}");
                            return;
                        }
                        Err(_) => {
                            debug!(%remote, "handshake timed out");
                            return;
                        }
                    };
                if let Err(e) = serve_connection(session, rooms, &secret).await {
                    debug!(%remote, "connection ended: {e:#}");
                }
            });
        }
    }

    /// Claude: connections keep the certificate they started with.
    async fn rotate_certificates(&self) {
        let mut interval = tokio::time::interval(tls::ROTATE_EVERY);
        interval.tick().await;
        loop {
            interval.tick().await;
            let rotated = Identity::generate().and_then(|identity| {
                self.endpoint
                    .set_server_config(Some(tls::server_config(&identity)?));
                Ok(identity.sha256_hex)
            });
            match rotated {
                Ok(hash) => {
                    info!("media certificate rotated");
                    self.certificate.send_replace(hash);
                }
                Err(e) => warn!("cannot rotate the media certificate: {e:#}"),
            }
        }
    }
}

/// Claude: native clients speak the media protocol over raw QUIC; browsers
/// reach it through a WebTransport session.
async fn open_session(incoming: quinn::Incoming) -> Result<Session> {
    let conn = incoming.await?;
    let alpn = conn
        .handshake_data()
        .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|d| d.protocol);
    match alpn.as_deref() {
        Some(facetty_proto::ALPN) => Ok(Session::raw(conn)),
        Some(alpn) if alpn == web_transport_quinn::ALPN.as_bytes() => {
            let request = web_transport_quinn::Request::accept(conn).await?;
            Ok(request.ok().await?)
        }
        _ => bail!("unexpected ALPN"),
    }
}

async fn serve_connection(conn: Session, rooms: Arc<Rooms>, secret: &[u8]) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(HELLO_TIMEOUT, conn.accept_bi())
        .await
        .context("no control stream")??;
    let hello = tokio::time::timeout(HELLO_TIMEOUT, quic::read_control(&mut recv))
        .await
        .context("no hello")??;
    let Some(ClientControl::Hello { token }) = hello else {
        bail!("expected hello");
    };
    let claims = match token::verify(secret, &token, unix_now()) {
        Ok(c) => c,
        Err(e) => {
            let _ = quic::write_control(
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
    let membership = Arc::new(rooms.join(&room, me, conn.clone(), control_tx));

    let writer = tokio::spawn(async move {
        while let Some(msg) = control_rx.recv().await {
            if quic::write_control(&mut send, &msg).await.is_err() {
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

async fn read_control(
    recv: &mut web_transport_quinn::RecvStream,
    membership: &room::Membership,
) -> Result<()> {
    while let Some(msg) = quic::read_control::<_, ClientControl>(recv).await? {
        match msg {
            ClientControl::Subscribe { publisher, rung } => membership.subscribe(publisher, rung),
            ClientControl::Hello { .. } => warn!("duplicate hello"),
        }
    }
    Ok(())
}

/// Each frame is read in its own task so a stream stalled on loss does not
/// hold up the frames behind it.
async fn read_video(conn: &Session, membership: &Arc<room::Membership>) -> Result<()> {
    loop {
        let mut stream = conn.accept_uni().await?;
        let membership = membership.clone();
        tokio::spawn(async move {
            let bytes = match stream.read_to_end(media::MAX_VIDEO_FRAME).await {
                Ok(b) => b,
                Err(e) => {
                    debug!("dropped video stream: {e}");
                    return;
                }
            };
            let Ok(mut frame) = media::decode::<VideoFrame>(&bytes) else {
                debug!("undecodable video frame");
                return;
            };
            frame.publisher = membership.participant();
            membership.publish_video(frame);
        });
    }
}

async fn read_audio(conn: &Session, membership: &room::Membership) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_certificate_is_replaced_daily() {
        let sfu = Arc::new(Sfu::bind("127.0.0.1:0".parse().unwrap(), b"secret").unwrap());
        let mut certificate = sfu.certificate();
        let first = certificate.borrow_and_update().clone();
        let running = sfu.clone();
        tokio::spawn(async move { running.run().await });
        tokio::time::sleep(tls::ROTATE_EVERY + Duration::from_secs(1)).await;
        certificate.changed().await.unwrap();
        assert_ne!(*certificate.borrow(), first);
    }
}
