//! JSON messages on the signaling WebSocket, and the SFU heartbeat.

use serde::{Deserialize, Serialize};

use crate::ParticipantId;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    pub id: ParticipantId,
    pub name: String,
    pub audio_muted: bool,
    pub video_off: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaServer {
    /// `host:port` of the SFU's QUIC endpoint. From an SFU, `:port` means
    /// "the host the client reached the signaling server on"; the signaling
    /// server fills it in before clients see it.
    pub addr: String,
    /// Hex SHA-256 of the SFU's self-signed certificate; clients pin it.
    pub cert_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Welcome {
        you: Participant,
        token: String,
        media: MediaServer,
        participants: Vec<Participant>,
    },
    Joined {
        participant: Participant,
    },
    Left {
        id: ParticipantId,
    },
    Updated {
        participant: Participant,
    },
    Chat {
        from: ParticipantId,
        name: String,
        text: String,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    SetState { audio_muted: bool, video_off: bool },
    Chat { text: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SfuHeartbeat {
    pub id: String,
    pub media: MediaServer,
    pub connections: u32,
}

pub const MAX_NAME_LEN: usize = 32;
pub const MAX_CHAT_LEN: usize = 500;
pub const MAX_ROOM_LEN: usize = 64;

pub fn valid_room(room: &str) -> bool {
    !room.is_empty()
        && room.len() <= MAX_ROOM_LEN
        && room
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_tagged_json() {
        let json = serde_json::to_string(&ClientEvent::Chat { text: "hi".into() }).unwrap();
        assert_eq!(json, r#"{"type":"chat","text":"hi"}"#);
    }

    #[test]
    fn room_names() {
        assert!(valid_room("standup-42"));
        assert!(!valid_room(""));
        assert!(!valid_room("a/b"));
    }
}
