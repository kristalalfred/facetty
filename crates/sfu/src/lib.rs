mod room;
mod tls;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use facetty_proto::ladder::{self, Rung};
use facetty_proto::media::{self, AudioPacket, ClientControl, ServerControl, VideoFrame};
use facetty_proto::rate::Rate;
use facetty_proto::{ParticipantId, quic, token};
use quinn::Endpoint;
use tokio::io::AsyncReadExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::{timeout, timeout_at};
use tracing::{debug, info, warn};
use web_transport_quinn::{RecvStream, SendStream, Session};

use room::{Membership, Outbox, Rooms};

pub use tls::Identity;

/// Claude: the handshake, the hello, and any rejection share it.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Claude: connections still handshaking or waiting for a hello. Admitted
/// ones are bounded by the signaling server's participant caps instead.
const MAX_PENDING_CONNECTIONS: usize = 64;
const FRAMES_PER_SECOND_PER_RUNG: f64 = 60.0;
const FRAME_BURST: f64 = 30.0;
const AUDIO_PACKETS_PER_SECOND: f64 = 100.0;
const AUDIO_BURST: f64 = 50.0;
const CONTROL_MESSAGES_PER_SECOND: f64 = 16.0;
const CONTROL_BURST: f64 = 64.0;

type Departure = Pin<Box<dyn Future<Output = ()> + Send>>;
type Roster = Arc<dyn Fn(&str, ParticipantId) -> Option<Departure> + Send + Sync>;

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

    /// Claude: serves media to participants `roster` has in a call. It gives
    /// the future that resolves when one leaves, or `None` for someone not
    /// in the call.
    pub async fn run<D>(
        &self,
        roster: impl Fn(&str, ParticipantId) -> Option<D> + Send + Sync + 'static,
    ) where
        D: Future<Output = ()> + Send + 'static,
    {
        let roster: Roster =
            Arc::new(move |room, id| roster(room, id).map(|d| Box::pin(d) as Departure));
        tokio::select! {
            () = self.accept(roster) => {}
            () = self.rotate_certificates() => {}
        }
    }

    async fn accept(&self, roster: Roster) {
        let pending = Arc::new(Semaphore::new(MAX_PENDING_CONNECTIONS));
        while let Some(incoming) = self.endpoint.accept().await {
            if !incoming.remote_address_validated() {
                let _ = incoming.retry();
                continue;
            }
            let Ok(permit) = pending.clone().try_acquire_owned() else {
                debug!(remote = %incoming.remote_address(), "too many pending connections");
                incoming.refuse();
                continue;
            };
            let rooms = self.rooms.clone();
            let secret = self.secret.clone();
            let roster = roster.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                if let Err(e) = connection(incoming, rooms, &secret, &roster, permit).await {
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

/// Claude: closes the QUIC connection on every exit, since a WebTransport
/// session keeps tasks of its own running on it.
async fn connection(
    incoming: quinn::Incoming,
    rooms: Arc<Rooms>,
    secret: &[u8],
    roster: &Roster,
    pending: OwnedSemaphorePermit,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + ADMISSION_TIMEOUT;
    let conn = timeout_at(deadline, incoming.accept()?)
        .await
        .context("handshake timed out")??;
    let result = async {
        let admitted = timeout_at(deadline, admit(&conn, secret, roster))
            .await
            .context("admission timed out")??;
        drop(pending);
        serve(admitted, rooms).await
    }
    .await;
    let reason: &[u8] = if result.is_ok() {
        b"left the call"
    } else {
        b""
    };
    conn.close(0u32.into(), reason);
    result
}

struct Admitted {
    session: Session,
    send: SendStream,
    recv: RecvStream,
    room: String,
    participant: ParticipantId,
    departed: Departure,
}

async fn admit(conn: &quinn::Connection, secret: &[u8], roster: &Roster) -> Result<Admitted> {
    let session = open_session(conn.clone()).await?;
    let (mut send, mut recv) = session.accept_bi().await?;
    let Some(ClientControl::Hello { token }) = quic::read_control(&mut recv).await? else {
        bail!("expected hello");
    };
    let claims = match token::verify(secret, &token, unix_now()) {
        Ok(c) => c,
        Err(e) => {
            reject(&mut send, e.to_string()).await;
            bail!("rejected token: {e}");
        }
    };
    let Some(departed) = roster(&claims.room, claims.participant) else {
        reject(&mut send, "not in this call".into()).await;
        bail!("participant {} is not in the call", claims.participant);
    };
    Ok(Admitted {
        session,
        send,
        recv,
        room: claims.room,
        participant: claims.participant,
        departed,
    })
}

/// Claude: native clients speak the media protocol over raw QUIC; browsers
/// reach it through a WebTransport session.
async fn open_session(conn: quinn::Connection) -> Result<Session> {
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

/// Claude: waits for the client to take the error, since the connection
/// closes when this returns.
async fn reject(send: &mut SendStream, message: String) {
    let _ = quic::write_control(send, &ServerControl::Error { message }).await;
    let _ = send.finish();
    let _ = send.stopped().await;
}

async fn serve(admitted: Admitted, rooms: Arc<Rooms>) -> Result<()> {
    let Admitted {
        session,
        mut send,
        mut recv,
        room,
        participant: me,
        departed,
    } = admitted;
    info!(%room, participant = me, remote = %session.remote_address(), "joined");
    let outbox = Arc::new(Outbox::default());
    let membership = Arc::new(rooms.join(&room, me, session.clone(), outbox.clone()));
    let result = tokio::select! {
        r = write_controls(&mut send, me, &outbox) => r,
        r = read_control(&mut recv, &membership) => r,
        r = read_video(&session, &membership) => r,
        r = read_audio(&session, &membership) => r,
        () = departed => Ok(()),
    };
    membership.leave();
    info!(%room, participant = me, "left");
    result
}

async fn write_controls(send: &mut SendStream, me: ParticipantId, outbox: &Outbox) -> Result<()> {
    let mut messages = vec![ServerControl::Welcome { participant: me }];
    loop {
        for message in &messages {
            timeout(CONTROL_WRITE_TIMEOUT, quic::write_control(send, message))
                .await
                .context("control stream stalled")??;
        }
        messages = outbox.next().await;
    }
}

/// Claude: a client over the rate waits rather than losing messages, since
/// each subscription is state that is not sent again.
async fn read_control(recv: &mut RecvStream, membership: &Membership) -> Result<()> {
    let mut rate = Rate::new(CONTROL_MESSAGES_PER_SECOND, CONTROL_BURST);
    while let Some(msg) = quic::read_control::<_, ClientControl>(recv).await? {
        while !rate.allow(Instant::now()) {
            tokio::time::sleep(rate.ready_in(Instant::now())).await;
        }
        match msg {
            ClientControl::Subscribe { publisher, rung }
                if rung.is_none_or(|r| ladder::size(r).is_some()) =>
            {
                membership.subscribe(publisher, rung)
            }
            ClientControl::Subscribe { .. } => debug!("subscription to a rung off the ladder"),
            ClientControl::Hello { .. } => warn!("duplicate hello"),
        }
    }
    Ok(())
}

/// Each frame is read in its own task so a stream stalled on loss does not
/// hold up the frames behind it.
async fn read_video(session: &Session, membership: &Arc<Membership>) -> Result<()> {
    let rates: Arc<Mutex<HashMap<Rung, Rate>>> = Arc::default();
    let mut frames = JoinSet::new();
    loop {
        let mut stream = session.accept_uni().await?;
        while frames.try_join_next().is_some() {}
        let membership = membership.clone();
        let rates = rates.clone();
        frames.spawn(async move {
            let bytes = match timeout(FRAME_TIMEOUT, read_frame(&mut stream)).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    debug!("dropped video stream: {e:#}");
                    return;
                }
                Err(_) => {
                    debug!("video stream timed out");
                    return;
                }
            };
            let Ok(mut frame) = media::decode::<VideoFrame>(&bytes) else {
                debug!("undecodable video frame");
                return;
            };
            if !fits_rung(&frame) {
                debug!("video frame does not match its rung");
                return;
            }
            let allowed = rates
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(frame.rung)
                .or_insert_with(|| Rate::new(FRAMES_PER_SECOND_PER_RUNG, FRAME_BURST))
                .allow(Instant::now());
            if !allowed {
                debug!("video frame over the rate limit");
                return;
            }
            frame.publisher = membership.participant();
            membership.publish_video(frame);
        });
    }
}

/// Claude: copies into one buffer, since quinn's own read_to_end keeps every
/// received chunk, and with it the packet each arrived in.
async fn read_frame(stream: &mut RecvStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut limited = (&mut *stream).take(media::MAX_VIDEO_FRAME as u64 + 1);
    limited.read_to_end(&mut bytes).await?;
    ensure!(
        bytes.len() <= media::MAX_VIDEO_FRAME,
        "video frame over {} bytes",
        media::MAX_VIDEO_FRAME
    );
    Ok(bytes)
}

fn fits_rung(frame: &VideoFrame) -> bool {
    ladder::size(frame.rung)
        .is_some_and(|size| facetty_ascii::frame_size(&frame.payload) == Ok((size.cols, size.rows)))
}

async fn read_audio(session: &Session, membership: &Membership) -> Result<()> {
    let mut rate = Rate::new(AUDIO_PACKETS_PER_SECOND, AUDIO_BURST);
    loop {
        let datagram = session.read_datagram().await?;
        if !rate.allow(Instant::now()) {
            continue;
        }
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
    use facetty_ascii::{Cell, Encoder, Frame};

    use super::*;

    #[test]
    fn the_largest_rung_fits_a_video_frame_and_sizes_must_match_rungs() {
        let rung = ladder::rungs().last().unwrap();
        let size = ladder::size(rung).unwrap();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut noise = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut picture = || Frame {
            cols: size.cols,
            rows: size.rows,
            cells: (0..size.cols as usize * size.rows as usize)
                .map(|_| {
                    let v = noise();
                    Cell {
                        glyph: (v % 14) as u8,
                        rgb: [(v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8],
                    }
                })
                .collect(),
        };
        let mut encoder = Encoder::default();
        for payload in [encoder.encode(&picture()), encoder.encode(&picture())] {
            let frame = VideoFrame {
                publisher: ParticipantId::MAX,
                rung,
                seq: u32::MAX,
                payload,
            };
            assert!(media::encode(&frame).len() <= media::MAX_VIDEO_FRAME);
            assert!(fits_rung(&frame));
            assert!(!fits_rung(&VideoFrame {
                rung: rung - 1,
                ..frame.clone()
            }));
            assert!(!fits_rung(&VideoFrame {
                rung: rung + 1,
                ..frame
            }));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_certificate_is_replaced_daily() {
        let sfu = Arc::new(Sfu::bind("127.0.0.1:0".parse().unwrap(), b"secret").unwrap());
        let mut certificate = sfu.certificate();
        let first = certificate.borrow_and_update().clone();
        let running = sfu.clone();
        tokio::spawn(async move { running.run(|_, _| None::<std::future::Ready<()>>).await });
        tokio::time::sleep(tls::ROTATE_EVERY + Duration::from_secs(1)).await;
        certificate.changed().await.unwrap();
        assert_ne!(*certificate.borrow(), first);
    }
}
