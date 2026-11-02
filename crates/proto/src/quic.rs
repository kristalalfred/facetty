//! Claude: the media protocol's streams over a QUIC connection, raw from
//! native clients or as a WebTransport session from browsers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use web_transport_quinn::{Session, WriteError};

use crate::media::{Error, MAX_CONTROL_MESSAGE, VideoFrame, decode, encode};

/// Round-trip time above the lowest seen, past which [`VideoSender`] lets
/// only one frame into the network at a time. The lowest is kept for the
/// whole connection, so a lasting rise in the path's own delay past this
/// also leaves one frame at a time.
const QUEUE_BUDGET: Duration = Duration::from_millis(100);

pub async fn write_control<W, T>(w: &mut W, msg: &T) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = encode(msg);
    w.write_all(&(body.len() as u32).to_le_bytes()).await?;
    w.write_all(&body).await?;
    Ok(())
}

/// Returns `Ok(None)` when the stream ends cleanly between messages.
pub async fn read_control<R, T>(r: &mut R) -> Result<Option<T>, Error>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_CONTROL_MESSAGE {
        return Err(Error::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(decode(&body)?))
}

/// Sends video frames on one connection, each on its own stream.
///
/// Quinn takes writes into a send buffer of megabytes, many seconds of video
/// on a slow link, so a write returning says nothing about the link keeping
/// up. [`ready`](Self::ready) instead waits until the video not yet
/// acknowledged fits in the congestion window, which leaves at most about one
/// frame queued in quinn. The congestion window itself grows until a router
/// queue on the path overflows, so `ready` also holds back while the round
/// trip shows such a queue. Callers fold the frames that arrive meanwhile
/// into the next one they send.
pub struct VideoSender {
    session: Session,
    unacked: AtomicU64,
    acked: Notify,
}

impl VideoSender {
    pub fn new(session: Session) -> Arc<Self> {
        Arc::new(Self {
            session,
            unacked: AtomicU64::new(0),
            acked: Notify::new(),
        })
    }

    /// Waits until the connection can take another frame without queueing it.
    pub async fn ready(&self) {
        loop {
            let acked = self.acked.notified();
            if self.has_room() {
                return;
            }
            acked.await;
        }
    }

    fn has_room(&self) -> bool {
        let unacked = self.unacked.load(Ordering::Relaxed);
        let path = quinn::Connection::stats(&self.session).path;
        unacked == 0 || (unacked < path.cwnd && path.rtt < path.min_rtt + QUEUE_BUDGET)
    }

    pub async fn send(self: &Arc<Self>, frame: &VideoFrame) -> Result<(), WriteError> {
        let bytes = encode(frame);
        let len = bytes.len() as u64;
        let mut stream = self.session.open_uni().await?;
        self.unacked.fetch_add(len, Ordering::Relaxed);
        let written = match stream.write_all(&bytes).await {
            Ok(()) => stream.finish().map_err(|_| WriteError::ClosedStream),
            Err(e) => Err(e),
        };
        let this = self.clone();
        tokio::spawn(async move {
            let _ = stream.stopped().await;
            this.unacked.fetch_sub(len, Ordering::Relaxed);
            this.acked.notify_waiters();
        });
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ClientControl;

    #[tokio::test]
    async fn control_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let sent = ClientControl::Subscribe {
            publisher: 7,
            rung: Some(3),
        };
        write_control(&mut a, &sent).await.unwrap();
        drop(a);
        let got: Option<ClientControl> = read_control(&mut b).await.unwrap();
        assert_eq!(got, Some(sent));
        let end: Option<ClientControl> = read_control(&mut b).await.unwrap();
        assert_eq!(end, None);
    }
}
