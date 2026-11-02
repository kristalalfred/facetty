//! Messages between clients and the SFU over QUIC.
//!
//! - One bidirectional control stream per connection, opened by the client,
//!   carrying length-prefixed [`ClientControl`] / [`ServerControl`].
//! - Video: one unidirectional stream per [`VideoFrame`], so a lost packet
//!   only delays the frame it belongs to.
//! - Audio: one datagram per [`AudioPacket`].

use serde::{Deserialize, Serialize, de::DeserializeOwned};

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
    /// A new subscriber needs this rung's next frame in full.
    Refresh {
        rung: Rung,
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
