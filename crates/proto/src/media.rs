//! Messages between clients and the SFU over QUIC.
//!
//! - One bidirectional control stream per connection, opened by the client,
//!   carrying length-prefixed [`ClientControl`] / [`ServerControl`].
//! - Video: one unidirectional stream per [`VideoFrame`], so a lost packet
//!   only delays the frame it belongs to.
//! - Audio: one datagram per [`AudioPacket`].

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::ParticipantId;
use crate::ladder::Rung;

pub const MAX_CONTROL_MESSAGE: usize = 64 * 1024;
pub const MAX_VIDEO_FRAME: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClientControl {
    Hello {
        token: String,
    },
    /// `rung: None` stops video from that publisher.
    Subscribe {
        publisher: ParticipantId,
        rung: Option<Rung>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerControl {
    Welcome {
        participant: ParticipantId,
    },
    /// The rungs some subscriber currently wants from this publisher.
    EncodeRungs {
        rungs: Vec<Rung>,
    },
    Error {
        message: String,
    },
}

/// `publisher` is ignored on the way in; the SFU stamps it before forwarding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoFrame {
    pub publisher: ParticipantId,
    pub rung: Rung,
    pub seq: u32,
    pub payload: Vec<u8>,
}

/// `publisher` is ignored on the way in; the SFU stamps it before forwarding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioPacket {
    pub publisher: ParticipantId,
    pub seq: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(#[from] postcard::Error),
    #[error("message of {0} bytes exceeds limit")]
    TooLarge(usize),
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_stdvec(msg).expect("postcard serialization into Vec cannot fail")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    Ok(postcard::from_bytes(bytes)?)
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
