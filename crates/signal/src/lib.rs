use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bits_proto::signal::{self, ClientEvent, MediaServer, Participant, ServerEvent, SfuHeartbeat};
use bits_proto::{ParticipantId, token};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const SFU_TTL: Duration = Duration::from_secs(15);
const TOKEN_TTL: Duration = Duration::from_secs(24 * 3600);

pub struct Signal {
    secret: Vec<u8>,
    next_id: AtomicU32,
    state: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    rooms: HashMap<String, Room>,
    sfus: HashMap<String, Sfu>,
}

struct Room {
    sfu: String,
    members: HashMap<ParticipantId, Member>,
}

struct Member {
    participant: Participant,
    events: mpsc::UnboundedSender<ServerEvent>,
}

struct Sfu {
    media: MediaServer,
    connections: u32,
    last_seen: Instant,
}

impl Signal {
    pub fn new(secret: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            secret: secret.to_vec(),
            next_id: AtomicU32::new(1),
            state: Mutex::new(Inner::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn router(signal: Arc<Signal>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/rooms/{room}", get(room_info))
        .route("/rooms/{room}/ws", get(join))
        .route("/internal/sfu/heartbeat", post(heartbeat))
        .with_state(signal)
}

#[derive(Deserialize)]
struct JoinQuery {
    name: Option<String>,
}

async fn join(
    State(signal): State<Arc<Signal>>,
    Path(room): Path<String>,
    Query(query): Query<JoinQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !signal::valid_room(&room) {
        return (StatusCode::BAD_REQUEST, "invalid room name").into_response();
    }
    let name = clean_name(query.name.as_deref());
    ws.on_upgrade(move |socket| session(signal, room, name, socket))
}

async fn room_info(
    State(signal): State<Arc<Signal>>,
    Path(room): Path<String>,
) -> Json<Vec<Participant>> {
    let inner = signal.lock();
    let mut list: Vec<Participant> = inner
        .rooms
        .get(&room)
        .map(|r| r.members.values().map(|m| m.participant.clone()).collect())
        .unwrap_or_default();
    list.sort_by_key(|p| p.id);
    Json(list)
}

async fn heartbeat(
    State(signal): State<Arc<Signal>>,
    headers: HeaderMap,
    Json(beat): Json<SfuHeartbeat>,
) -> StatusCode {
    let authorized = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), &signal.secret));
    if !authorized {
        return StatusCode::UNAUTHORIZED;
    }
    let mut inner = signal.lock();
    if !inner.sfus.contains_key(&beat.id) {
        info!(id = %beat.id, addr = %beat.media.addr, "media server registered");
    }
    inner.sfus.insert(
        beat.id,
        Sfu {
            media: beat.media,
            connections: beat.connections,
            last_seen: Instant::now(),
        },
    );
    StatusCode::NO_CONTENT
}

async fn session(signal: Arc<Signal>, room: String, name: String, mut socket: WebSocket) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let id = signal.next_id.fetch_add(1, Ordering::Relaxed);
    let me = Participant {
        id,
        name,
        audio_muted: false,
        video_off: false,
    };

    let welcome = match admit(&signal, &room, me.clone(), tx) {
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
    events: mpsc::UnboundedSender<ServerEvent>,
) -> Result<ServerEvent, String> {
    let mut inner = signal.lock();
    inner.sfus.retain(|_, s| s.last_seen.elapsed() < SFU_TTL);

    let current = inner.rooms.get(room_name).map(|r| r.sfu.clone());
    let sfu_id = match current.filter(|id| inner.sfus.contains_key(id)) {
        Some(id) => id,
        None => {
            let id = inner
                .sfus
                .iter()
                .min_by_key(|(_, s)| s.connections)
                .map(|(id, _)| id.clone())
                .ok_or("no media server available")?;
            if let Some(room) = inner.rooms.get_mut(room_name) {
                warn!(room = room_name, sfu = %id, "room's media server is gone; moving room");
                room.sfu = id.clone();
            }
            id
        }
    };
    let media = inner.sfus[&sfu_id].media.clone();
    let room = inner
        .rooms
        .entry(room_name.to_string())
        .or_insert_with(|| Room {
            sfu: sfu_id,
            members: HashMap::new(),
        });

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
            expires_unix: unix_now() + TOKEN_TTL.as_secs(),
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
    }
}

fn leave(signal: &Signal, room_name: &str, id: ParticipantId) {
    let mut inner = signal.lock();
    let Some(room) = inner.rooms.get_mut(room_name) else {
        return;
    };
    room.members.remove(&id);
    if room.members.is_empty() {
        inner.rooms.remove(room_name);
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
