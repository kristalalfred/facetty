use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::HOST;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use facetty_proto::signal::{self, CallInvite, ClientEvent, MediaServer, Participant, ServerEvent};
use facetty_proto::{ParticipantId, token};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const INVITE_TTL: Duration = Duration::from_secs(24 * 3600);
const INVALID_CODE: &str = "invalid or expired call code";

pub struct Signal {
    secret: Vec<u8>,
    host_key: Vec<u8>,
    media: MediaServer,
    next_id: AtomicU32,
    state: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    rooms: HashMap<String, Room>,
}

struct Room {
    expires_unix: u64,
    members: HashMap<ParticipantId, Member>,
}

struct Member {
    participant: Participant,
    events: mpsc::UnboundedSender<ServerEvent>,
}

impl Signal {
    /// `media` is the media server handed to every caller. An `addr` of
    /// `:port` means that port on the host the caller used to reach us.
    pub fn new(secret: &[u8], host_key: &[u8], media: MediaServer) -> Arc<Self> {
        assert!(!secret.is_empty(), "server secret cannot be empty");
        assert!(!host_key.is_empty(), "host key cannot be empty");
        Arc::new(Self {
            secret: secret.to_vec(),
            host_key: host_key.to_vec(),
            media,
            next_id: AtomicU32::new(1),
            state: Mutex::new(Inner::default()),
        })
    }

    fn create_call(&self) -> Result<CallInvite, getrandom::Error> {
        let mut inner = self.lock();
        let now = unix_now();
        inner
            .rooms
            .retain(|_, room| room.expires_unix > now || !room.members.is_empty());
        let code = loop {
            let code = invite_code()?;
            if !inner.rooms.contains_key(&code) {
                break code;
            }
        };
        let expires_unix = now + INVITE_TTL.as_secs();
        inner.rooms.insert(
            code.clone(),
            Room {
                expires_unix,
                members: HashMap::new(),
            },
        );
        Ok(CallInvite { code, expires_unix })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn router(signal: Arc<Signal>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/rooms", post(create_call))
        .route("/rooms/{room}", get(room_info))
        .route("/rooms/{room}/ws", get(join))
        .with_state(signal)
}

async fn create_call(State(signal): State<Arc<Signal>>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &signal.host_key) {
        return (
            StatusCode::UNAUTHORIZED,
            "host key required to create a call",
        )
            .into_response();
    }
    match signal.create_call() {
        Ok(invite) => (StatusCode::CREATED, Json(invite)).into_response(),
        Err(e) => {
            warn!("cannot generate a call code: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "cannot create a call").into_response()
        }
    }
}

#[derive(Deserialize)]
struct JoinQuery {
    name: Option<String>,
}

async fn join(
    State(signal): State<Arc<Signal>>,
    Path(room): Path<String>,
    Query(query): Query<JoinQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let room = room.to_ascii_lowercase();
    if signal
        .lock()
        .rooms
        .get(&room)
        .is_none_or(|r| r.expires_unix <= unix_now())
    {
        return (StatusCode::NOT_FOUND, INVALID_CODE).into_response();
    }
    let name = clean_name(query.name.as_deref());
    let host = headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| strip_port(h).to_string());
    ws.on_upgrade(move |socket| session(signal, room, name, host, socket))
}

async fn room_info(State(signal): State<Arc<Signal>>, Path(room): Path<String>) -> Response {
    let room = room.to_ascii_lowercase();
    let inner = signal.lock();
    let Some(room) = inner
        .rooms
        .get(&room)
        .filter(|r| r.expires_unix > unix_now())
    else {
        return (StatusCode::NOT_FOUND, INVALID_CODE).into_response();
    };
    let mut list: Vec<Participant> = room
        .members
        .values()
        .map(|m| m.participant.clone())
        .collect();
    list.sort_by_key(|p| p.id);
    Json(list).into_response()
}

async fn session(
    signal: Arc<Signal>,
    room: String,
    name: String,
    host: Option<String>,
    mut socket: WebSocket,
) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let id = signal.next_id.fetch_add(1, Ordering::Relaxed);
    let me = Participant {
        id,
        name,
        audio_muted: false,
        video_off: false,
    };

    let welcome = match admit(&signal, &room, me.clone(), host.as_deref(), tx) {
        Ok(w) => w,
        Err(message) => {
            let _ = send(&mut socket, &ServerEvent::Error { message }).await;
            return;
        }
    };
    info!(%room, id, name = %me.name, "joined");
    if send(&mut socket, &welcome).await.is_err() {
        leave(&signal, &room, id);
        return;
    }

    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(event) = outgoing else { break };
                if send(&mut socket, &event).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { break };
                match msg {
                    Message::Text(text) => match serde_json::from_str::<ClientEvent>(&text) {
                        Ok(event) => handle(&signal, &room, id, event),
                        Err(e) => debug!(id, "bad client event: {e}"),
                    },
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }
    leave(&signal, &room, id);
    info!(%room, id, "left");
}

fn admit(
    signal: &Signal,
    room_name: &str,
    me: Participant,
    host: Option<&str>,
    events: mpsc::UnboundedSender<ServerEvent>,
) -> Result<ServerEvent, String> {
    let mut media = signal.media.clone();
    if media.addr.starts_with(':') {
        let host = host.ok_or("cannot tell which host the media server is on")?;
        media.addr = format!("{host}{}", media.addr);
    }
    let mut inner = signal.lock();
    let room = inner
        .rooms
        .get_mut(room_name)
        .filter(|r| r.expires_unix > unix_now())
        .ok_or(INVALID_CODE)?;

    let mut participants: Vec<Participant> = room
        .members
        .values()
        .map(|m| m.participant.clone())
        .collect();
    participants.sort_by_key(|p| p.id);
    room.broadcast(&ServerEvent::Joined {
        participant: me.clone(),
    });
    room.members.insert(
        me.id,
        Member {
            participant: me.clone(),
            events,
        },
    );

    let token = token::sign(
        &signal.secret,
        &token::Claims {
            room: room_name.to_string(),
            participant: me.id,
            expires_unix: room.expires_unix,
        },
    );
    Ok(ServerEvent::Welcome {
        you: me,
        token,
        media,
        participants,
    })
}

fn handle(signal: &Signal, room_name: &str, id: ParticipantId, event: ClientEvent) {
    let mut inner = signal.lock();
    let Some(room) = inner.rooms.get_mut(room_name) else {
        return;
    };
    match event {
        ClientEvent::SetState {
            audio_muted,
            video_off,
        } => {
            let Some(member) = room.members.get_mut(&id) else {
                return;
            };
            member.participant.audio_muted = audio_muted;
            member.participant.video_off = video_off;
            let participant = member.participant.clone();
            room.broadcast(&ServerEvent::Updated { participant });
        }
        ClientEvent::Chat { text } => {
            let text: String = text.trim().chars().take(signal::MAX_CHAT_LEN).collect();
            let Some(member) = room.members.get(&id) else {
                return;
            };
            if text.is_empty() {
                return;
            }
            let name = member.participant.name.clone();
            room.broadcast(&ServerEvent::Chat {
                from: id,
                name,
                text,
            });
        }
        ClientEvent::React { emoji } => {
            if room.members.contains_key(&id) && signal::REACTIONS.contains(&emoji.as_str()) {
                room.broadcast(&ServerEvent::Reaction { from: id, emoji });
            }
        }
    }
}

fn leave(signal: &Signal, room_name: &str, id: ParticipantId) {
    let mut inner = signal.lock();
    let Some(room) = inner.rooms.get_mut(room_name) else {
        return;
    };
    room.members.remove(&id);
    if room.members.is_empty() {
        if room.expires_unix <= unix_now() {
            inner.rooms.remove(room_name);
        }
    } else {
        room.broadcast(&ServerEvent::Left { id });
    }
}

impl Room {
    fn broadcast(&self, event: &ServerEvent) {
        for member in self.members.values() {
            let _ = member.events.send(event.clone());
        }
    }
}

async fn send(socket: &mut WebSocket, event: &ServerEvent) -> Result<(), axum::Error> {
    let json = serde_json::to_string(event).expect("server events serialize");
    socket.send(Message::Text(json.into())).await
}

fn clean_name(name: Option<&str>) -> String {
    let name: String = name
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(signal::MAX_NAME_LEN)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        "guest".to_string()
    } else {
        name.to_string()
    }
}

/// `host:port` or `[v6]:port` from a Host header, without the port.
fn strip_port(host: &str) -> &str {
    if let Some(end) = host.find(']') {
        return &host[..=end];
    }
    match host.split_once(':') {
        Some((h, port)) if !port.contains(':') => h,
        _ => host,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorized(headers: &HeaderMap, key: &[u8]) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), key))
}

fn invite_code() -> Result<String, getrandom::Error> {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)?;
    let mut code = String::with_capacity(19);
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            code.push('-');
        }
        code.push(ALPHABET[(byte & 31) as usize] as char);
    }
    Ok(code)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ports_from_hosts() {
        assert_eq!(strip_port("10.4.0.86:8740"), "10.4.0.86");
        assert_eq!(strip_port("[::1]:8740"), "[::1]");
        assert_eq!(strip_port("localhost"), "localhost");
        assert_eq!(strip_port("[::1]"), "[::1]");
    }

    fn participant(id: ParticipantId) -> Participant {
        Participant {
            id,
            name: "guest".into(),
            audio_muted: false,
            video_off: false,
        }
    }

    fn signal() -> Arc<Signal> {
        Signal::new(
            b"server-secret",
            b"host-key",
            MediaServer {
                addr: "localhost:8741".into(),
                cert_sha256: "test".into(),
            },
        )
    }

    #[tokio::test]
    async fn only_the_host_key_creates_calls() {
        let signal = signal();
        for key in [None, Some("wrong"), Some("server-secret")] {
            let mut headers = HeaderMap::new();
            if let Some(key) = key {
                headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
            }
            assert_eq!(
                create_call(State(signal.clone()), headers).await.status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert!(signal.lock().rooms.is_empty());

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer host-key".parse().unwrap());
        assert_eq!(
            create_call(State(signal.clone()), headers).await.status(),
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn expiry_denies_new_admissions_and_rosters_but_keeps_active_calls() {
        let signal = signal();
        let invite = signal.create_call().unwrap();
        assert_eq!(invite.code.len(), 19);
        assert!(signal::valid_room(&invite.code));
        assert!(invite.expires_unix > unix_now());
        let (events, _rx) = mpsc::unbounded_channel();
        let welcome = admit(&signal, &invite.code, participant(1), None, events).unwrap();
        let ServerEvent::Welcome { token, .. } = welcome else {
            panic!("expected welcome");
        };
        let claims = token::verify(b"server-secret", &token, unix_now()).unwrap();
        assert_eq!(claims.room, invite.code);
        assert_eq!(claims.expires_unix, invite.expires_unix);

        signal
            .lock()
            .rooms
            .get_mut(&invite.code)
            .unwrap()
            .expires_unix = unix_now();
        let (events, _rx) = mpsc::unbounded_channel();
        assert_eq!(
            admit(&signal, &invite.code, participant(2), None, events),
            Err(INVALID_CODE.into())
        );
        assert_eq!(
            room_info(State(signal.clone()), Path(invite.code.clone()))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        signal.create_call().unwrap();
        assert!(signal.lock().rooms.contains_key(&invite.code));
        leave(&signal, &invite.code, 1);
        assert!(!signal.lock().rooms.contains_key(&invite.code));
    }

    #[test]
    fn empty_calls_keep_their_code_until_expiry() {
        let signal = signal();
        let invite = signal.create_call().unwrap();
        let (events, _rx) = mpsc::unbounded_channel();
        admit(&signal, &invite.code, participant(1), None, events).unwrap();
        leave(&signal, &invite.code, 1);
        assert!(signal.lock().rooms[&invite.code].members.is_empty());
        let (events, _rx) = mpsc::unbounded_channel();
        assert!(admit(&signal, &invite.code, participant(2), None, events).is_ok());
        leave(&signal, &invite.code, 2);

        signal
            .lock()
            .rooms
            .get_mut(&invite.code)
            .unwrap()
            .expires_unix = unix_now();
        signal.create_call().unwrap();
        assert!(!signal.lock().rooms.contains_key(&invite.code));
    }
}
