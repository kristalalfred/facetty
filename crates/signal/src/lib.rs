use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::HOST;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use facetty_proto::rate::Rate;
use facetty_proto::signal::{self, CallInvite, ClientEvent, MediaServer, Participant, ServerEvent};
use facetty_proto::{ParticipantId, token};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use tracing::{debug, info, warn};

const INVITE_TTL: Duration = Duration::from_secs(24 * 3600);
const INVALID_CODE: &str = "invalid or expired call code";
const CALL_FULL: &str = "this call is full";
const SERVER_FULL: &str = "this server is full; try again later";
/// Claude: events waiting for one participant's socket. A participant this
/// far behind has stopped reading and is dropped from the call.
const EVENT_QUEUE: usize = 64;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CLIENT_MESSAGE: usize = 16 * 1024;
/// Claude: tungstenite queues automatic Pong replies here too, so a client
/// that pings without reading fills it and is disconnected.
const MAX_WRITE_BUFFER: usize = 64 * 1024;
const CLIENT_EVENTS_PER_SECOND: f64 = 5.0;
const CLIENT_EVENT_BURST: f64 = 20.0;
/// Claude: frames of any kind, pings included, past which a client is
/// flooding rather than chatting and is disconnected.
const CLIENT_FRAMES_PER_SECOND: f64 = 50.0;
const CLIENT_FRAME_BURST: f64 = 100.0;

pub struct Config {
    /// Key required to create calls. `None` lets anyone create them.
    pub host_key: Option<Vec<u8>>,
    /// Most calls held at once. When full, creating a call drops the oldest
    /// call nobody is in, and fails if every call has someone in it.
    pub max_calls: usize,
    /// Claude: most people in calls at once, across all calls.
    pub max_participants: usize,
    /// Claude: media server address handed to every caller. `:port` means
    /// that port on the host the caller used to reach us.
    pub media_addr: String,
    /// Claude: hex SHA-256 of the media server's current certificate.
    pub media_cert: watch::Receiver<String>,
}

pub struct Signal {
    secret: Vec<u8>,
    config: Config,
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
    events: mpsc::Sender<ServerEvent>,
    /// Claude: dropped with the member, which tells [`Signal::departure`]
    /// futures that the participant left.
    present: watch::Sender<()>,
}

#[derive(Debug)]
enum CreateError {
    Full,
    Random(getrandom::Error),
}

impl Signal {
    pub fn new(secret: &[u8], config: Config) -> Arc<Self> {
        assert!(!secret.is_empty(), "server secret cannot be empty");
        assert!(
            config.host_key.as_ref().is_none_or(|k| !k.is_empty()),
            "host key cannot be empty"
        );
        assert!(config.max_calls > 0, "max calls must be at least 1");
        assert!(
            config.max_participants > 0,
            "max participants must be at least 1"
        );
        Arc::new(Self {
            secret: secret.to_vec(),
            config,
            next_id: AtomicU32::new(1),
            state: Mutex::new(Inner::default()),
        })
    }

    fn create_call(&self) -> Result<CallInvite, CreateError> {
        let mut inner = self.lock();
        let now = unix_now();
        inner
            .rooms
            .retain(|_, room| room.expires_unix > now || !room.members.is_empty());
        if inner.rooms.len() >= self.config.max_calls {
            let oldest_empty = inner
                .rooms
                .iter()
                .filter(|(_, room)| room.members.is_empty())
                .min_by_key(|(_, room)| room.expires_unix)
                .map(|(code, _)| code.clone())
                .ok_or(CreateError::Full)?;
            inner.rooms.remove(&oldest_empty);
        }
        let code = loop {
            let code = invite_code().map_err(CreateError::Random)?;
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

    /// Claude: resolves when the participant leaves the call, or `None` if
    /// they are not in it.
    pub fn departure(
        &self,
        room: &str,
        participant: ParticipantId,
    ) -> Option<impl Future<Output = ()> + Send + use<>> {
        Some(
            self.lock()
                .rooms
                .get(room)?
                .members
                .get(&participant)?
                .departure(),
        )
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
    if let Some(key) = &signal.config.host_key
        && !authorized(&headers, key)
    {
        return (
            StatusCode::UNAUTHORIZED,
            "host key required to create a call",
        )
            .into_response();
    }
    match signal.create_call() {
        Ok(invite) => (StatusCode::CREATED, Json(invite)).into_response(),
        Err(CreateError::Full) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "every call on this server is in use; try again later",
        )
            .into_response(),
        Err(CreateError::Random(e)) => {
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
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .max_frame_size(MAX_CLIENT_MESSAGE)
        .read_buffer_size(MAX_CLIENT_MESSAGE)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_WRITE_BUFFER)
        .on_upgrade(move |socket| session(signal, room, name, host, socket))
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
    let (tx, mut rx) = mpsc::channel(EVENT_QUEUE);
    let id = signal.next_id.fetch_add(1, Ordering::Relaxed);
    let me = Participant {
        id,
        name,
        audio_muted: false,
        video_off: false,
    };

    let (welcome, departed) = match admit(&signal, &room, me.clone(), host.as_deref(), tx) {
        Ok(admitted) => admitted,
        Err(message) => {
            send(&mut socket, &ServerEvent::Error { message }).await;
            return;
        }
    };
    info!(%room, id, name = %me.name, "joined");
    if !send(&mut socket, &welcome).await {
        leave(&signal, &room, id);
        return;
    }

    let mut throttle = Throttle::new();
    let mut frames = Rate::new(CLIENT_FRAMES_PER_SECOND, CLIENT_FRAME_BURST);
    tokio::pin!(departed);
    loop {
        let retry = throttle.retry_in(Instant::now());
        tokio::select! {
            () = &mut departed => break,
            () = tokio::time::sleep(retry.unwrap_or_default()), if retry.is_some() => {
                if let Some(event) = throttle.retry(Instant::now()) {
                    handle(&signal, &room, id, event);
                }
            }
            outgoing = rx.recv() => {
                let Some(event) = outgoing else { break };
                if !send(&mut socket, &event).await {
                    break;
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { break };
                if !frames.allow(Instant::now()) {
                    debug!(id, "disconnected a client sending too many frames");
                    break;
                }
                match msg {
                    Message::Text(text) => match serde_json::from_str::<ClientEvent>(&text) {
                        Ok(event) => {
                            if let Some(event) = throttle.take(event, Instant::now()) {
                                handle(&signal, &room, id, event);
                            }
                        }
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
    events: mpsc::Sender<ServerEvent>,
) -> Result<(ServerEvent, impl Future<Output = ()> + Send + use<>), String> {
    let mut media = MediaServer {
        addr: signal.config.media_addr.clone(),
        cert_sha256: signal.config.media_cert.borrow().clone(),
    };
    if media.addr.starts_with(':') {
        let host = host.ok_or("cannot tell which host the media server is on")?;
        media.addr = format!("{}{}", media_host(host), media.addr);
    }
    let mut inner = signal.lock();
    let in_calls: usize = inner.rooms.values().map(|r| r.members.len()).sum();
    let room = inner
        .rooms
        .get_mut(room_name)
        .filter(|r| r.expires_unix > unix_now())
        .ok_or(INVALID_CODE)?;
    if room.members.len() >= signal::MAX_CALL_PARTICIPANTS {
        return Err(CALL_FULL.into());
    }
    if in_calls >= signal.config.max_participants {
        return Err(SERVER_FULL.into());
    }

    room.broadcast(&ServerEvent::Joined {
        participant: me.clone(),
    });
    let mut participants: Vec<Participant> = room
        .members
        .values()
        .map(|m| m.participant.clone())
        .collect();
    participants.sort_by_key(|p| p.id);
    let member = Member {
        participant: me.clone(),
        events,
        present: watch::Sender::new(()),
    };
    let departed = member.departure();
    room.members.insert(me.id, member);

    let token = token::sign(
        &signal.secret,
        &token::Claims {
            room: room_name.to_string(),
            participant: me.id,
            expires_unix: room.expires_unix,
        },
    );
    let welcome = ServerEvent::Welcome {
        you: me,
        token,
        media,
        participants,
    };
    Ok((welcome, departed))
}

fn handle(signal: &Signal, room_name: &str, id: ParticipantId, event: ClientEvent) {
    let mut inner = signal.lock();
    let Some(room) = inner.rooms.get_mut(room_name) else {
        return;
    };
    let Some(member) = room.members.get_mut(&id) else {
        return;
    };
    let event = match event {
        ClientEvent::SetState {
            audio_muted,
            video_off,
        } => {
            member.participant.audio_muted = audio_muted;
            member.participant.video_off = video_off;
            ServerEvent::Updated {
                participant: member.participant.clone(),
            }
        }
        ClientEvent::Chat { text } => {
            let text = clean_chat(&text);
            if text.is_empty() {
                return;
            }
            ServerEvent::Chat {
                from: id,
                name: member.participant.name.clone(),
                text,
            }
        }
        ClientEvent::React { emoji } => {
            if !signal::REACTIONS.contains(&emoji.as_str()) {
                return;
            }
            ServerEvent::Reaction { from: id, emoji }
        }
    };
    room.broadcast(&event);
}

fn leave(signal: &Signal, room_name: &str, id: ParticipantId) {
    let mut inner = signal.lock();
    let Some(room) = inner.rooms.get_mut(room_name) else {
        return;
    };
    if room.members.remove(&id).is_none() {
        return;
    }
    if room.members.is_empty() {
        if room.expires_unix <= unix_now() {
            inner.rooms.remove(room_name);
        }
    } else {
        room.broadcast(&ServerEvent::Left { id });
    }
}

/// Claude: chat and reactions over the rate are dropped. A state change over
/// the rate waits, replaced by any later one, so the last state still
/// arrives.
struct Throttle {
    rate: Rate,
    waiting: Option<ClientEvent>,
}

impl Throttle {
    fn new() -> Self {
        Self {
            rate: Rate::new(CLIENT_EVENTS_PER_SECOND, CLIENT_EVENT_BURST),
            waiting: None,
        }
    }

    fn take(&mut self, event: ClientEvent, now: Instant) -> Option<ClientEvent> {
        let is_state = matches!(event, ClientEvent::SetState { .. });
        if is_state {
            self.waiting = None;
        }
        if self.rate.allow(now) {
            Some(event)
        } else {
            if is_state {
                self.waiting = Some(event);
            }
            None
        }
    }

    fn retry_in(&self, now: Instant) -> Option<Duration> {
        self.waiting.as_ref().map(|_| self.rate.ready_in(now))
    }

    fn retry(&mut self, now: Instant) -> Option<ClientEvent> {
        if self.waiting.is_some() && self.rate.allow(now) {
            self.waiting.take()
        } else {
            None
        }
    }
}

impl Member {
    fn departure(&self) -> impl Future<Output = ()> + Send + use<> {
        let mut present = self.present.subscribe();
        async move { while present.changed().await.is_ok() {} }
    }
}

impl Room {
    /// Claude: a member whose queue is full or closed is dropped from the
    /// call, which ends its session and its media connection.
    fn broadcast(&mut self, event: &ServerEvent) {
        let stalled: Vec<ParticipantId> = self
            .members
            .iter()
            .filter(|(_, member)| member.events.try_send(event.clone()).is_err())
            .map(|(&id, _)| id)
            .collect();
        for id in stalled {
            if self.members.remove(&id).is_some() {
                debug!(id, "dropped a participant that stopped reading");
                self.broadcast(&ServerEvent::Left { id });
            }
        }
    }
}

/// Claude: false when the client is gone or has not read for
/// `SEND_TIMEOUT`.
async fn send(socket: &mut WebSocket, event: &ServerEvent) -> bool {
    let json = serde_json::to_string(event).expect("server events serialize");
    matches!(
        timeout(SEND_TIMEOUT, socket.send(Message::Text(json.into()))).await,
        Ok(Ok(()))
    )
}

fn clean_chat(text: &str) -> String {
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    text.trim().chars().take(signal::MAX_CHAT_LEN).collect()
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

/// Claude: `localhost` can resolve to `::1` first, where the media listener
/// is not by default, and browsers cannot fall back to IPv4 for WebTransport.
fn media_host(host: &str) -> &str {
    if host.eq_ignore_ascii_case("localhost") {
        "127.0.0.1"
    } else {
        host
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
    use std::pin::Pin;

    use super::*;

    #[test]
    fn strips_ports_from_hosts() {
        assert_eq!(strip_port("10.4.0.86:8740"), "10.4.0.86");
        assert_eq!(strip_port("[::1]:8740"), "[::1]");
        assert_eq!(strip_port("localhost"), "localhost");
        assert_eq!(strip_port("[::1]"), "[::1]");
    }

    fn config() -> Config {
        Config {
            host_key: Some(b"host-key".to_vec()),
            max_calls: 100,
            max_participants: 100,
            media_addr: "localhost:8741".into(),
            media_cert: watch::channel("test".to_string()).1,
        }
    }

    fn signal() -> Arc<Signal> {
        Signal::new(b"server-secret", config())
    }

    struct Joined {
        welcome: ServerEvent,
        events: mpsc::Receiver<ServerEvent>,
        departed: Pin<Box<dyn Future<Output = ()> + Send>>,
    }

    fn join_from(
        signal: &Signal,
        code: &str,
        id: ParticipantId,
        host: Option<&str>,
    ) -> Result<Joined, String> {
        let (tx, events) = mpsc::channel(EVENT_QUEUE);
        let me = Participant {
            id,
            name: "guest".into(),
            audio_muted: false,
            video_off: false,
        };
        let (welcome, departed) = admit(signal, code, me, host, tx)?;
        Ok(Joined {
            welcome,
            events,
            departed: Box::pin(departed),
        })
    }

    fn join(signal: &Signal, code: &str, id: ParticipantId) -> Result<Joined, String> {
        join_from(signal, code, id, None)
    }

    async fn resolves(departed: &mut Pin<Box<dyn Future<Output = ()> + Send>>) -> bool {
        tokio::time::timeout(Duration::from_millis(50), departed)
            .await
            .is_ok()
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

    #[test]
    fn media_is_on_the_host_callers_used_with_localhost_as_ipv4() {
        let signal = Signal::new(
            b"server-secret",
            Config {
                media_addr: ":8741".into(),
                ..config()
            },
        );
        let call = signal.create_call().unwrap();
        for (id, (host, addr)) in [
            ("calls.example.com", "calls.example.com:8741"),
            ("localhost", "127.0.0.1:8741"),
            ("[::1]", "[::1]:8741"),
        ]
        .into_iter()
        .enumerate()
        {
            let joined = join_from(&signal, &call.code, id as ParticipantId, Some(host)).unwrap();
            let ServerEvent::Welcome { media, .. } = joined.welcome else {
                panic!("expected welcome");
            };
            assert_eq!(media.addr, addr);
        }
    }

    #[tokio::test]
    async fn open_servers_create_calls_without_a_key() {
        let signal = Signal::new(
            b"server-secret",
            Config {
                host_key: None,
                ..config()
            },
        );
        assert_eq!(
            create_call(State(signal), HeaderMap::new()).await.status(),
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn full_servers_drop_the_oldest_empty_call_but_never_an_occupied_one() {
        let signal = Signal::new(
            b"server-secret",
            Config {
                host_key: None,
                max_calls: 3,
                ..config()
            },
        );
        let occupied = signal.create_call().unwrap();
        let _first = join(&signal, &occupied.code, 1).unwrap();
        let oldest = signal.create_call().unwrap();
        signal
            .lock()
            .rooms
            .get_mut(&oldest.code)
            .unwrap()
            .expires_unix -= 1;
        let newer = signal.create_call().unwrap();
        let newest = signal.create_call().unwrap();
        {
            let inner = signal.lock();
            assert!(!inner.rooms.contains_key(&oldest.code));
            for kept in [&occupied, &newer, &newest] {
                assert!(inner.rooms.contains_key(&kept.code));
            }
        }

        let _second = join(&signal, &newer.code, 2).unwrap();
        let _third = join(&signal, &newest.code, 3).unwrap();
        assert_eq!(
            create_call(State(signal.clone()), HeaderMap::new())
                .await
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(signal.lock().rooms.len(), 3);
    }

    #[tokio::test]
    async fn expiry_denies_new_admissions_and_rosters_but_keeps_active_calls() {
        let signal = signal();
        let invite = signal.create_call().unwrap();
        assert_eq!(invite.code.len(), 19);
        assert!(signal::valid_room(&invite.code));
        assert!(invite.expires_unix > unix_now());
        let joined = join(&signal, &invite.code, 1).unwrap();
        let ServerEvent::Welcome { token, .. } = joined.welcome else {
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
        assert_eq!(
            join(&signal, &invite.code, 2).err(),
            Some(INVALID_CODE.into())
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
        join(&signal, &invite.code, 1).unwrap();
        leave(&signal, &invite.code, 1);
        assert!(signal.lock().rooms[&invite.code].members.is_empty());
        assert!(join(&signal, &invite.code, 2).is_ok());
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

    #[tokio::test]
    async fn media_access_ends_when_a_participant_leaves() {
        let signal = signal();
        let call = signal.create_call().unwrap();
        let mut first = join(&signal, &call.code, 1).unwrap();
        let mut second = join(&signal, &call.code, 2).unwrap();
        assert!(signal.departure(&call.code, 1).is_some());
        assert!(signal.departure(&call.code, 3).is_none());
        assert!(signal.departure("not-a-call", 1).is_none());

        let mut later = Box::pin(signal.departure(&call.code, 1).unwrap())
            as Pin<Box<dyn Future<Output = ()> + Send>>;
        leave(&signal, &call.code, 1);
        assert!(resolves(&mut first.departed).await);
        assert!(resolves(&mut later).await);
        assert!(signal.departure(&call.code, 1).is_none());
        assert!(!resolves(&mut second.departed).await);
    }

    #[test]
    fn calls_and_the_server_cap_their_participants() {
        let signal = Signal::new(
            b"server-secret",
            Config {
                max_participants: signal::MAX_CALL_PARTICIPANTS + 1,
                ..config()
            },
        );
        let call = signal.create_call().unwrap();
        let _in_call: Vec<Joined> = (0..signal::MAX_CALL_PARTICIPANTS as ParticipantId)
            .map(|id| join(&signal, &call.code, id).unwrap())
            .collect();
        assert_eq!(join(&signal, &call.code, 100).err(), Some(CALL_FULL.into()));

        let other = signal.create_call().unwrap();
        let _last = join(&signal, &other.code, 101).unwrap();
        assert_eq!(
            join(&signal, &other.code, 102).err(),
            Some(SERVER_FULL.into())
        );
        leave(&signal, &call.code, 0);
        assert!(join(&signal, &other.code, 102).is_ok());
    }

    #[tokio::test]
    async fn participants_that_stop_reading_are_dropped_once() {
        let signal = signal();
        let call = signal.create_call().unwrap();
        let mut idle = join(&signal, &call.code, 1).unwrap();
        let mut reader = join(&signal, &call.code, 2).unwrap();
        let mut seen = Vec::new();
        for _ in 0..=EVENT_QUEUE {
            handle(
                &signal,
                &call.code,
                2,
                ClientEvent::Chat { text: "hi".into() },
            );
            while let Ok(event) = reader.events.try_recv() {
                seen.push(event);
            }
        }
        assert!(resolves(&mut idle.departed).await);
        leave(&signal, &call.code, 1);
        while let Ok(event) = reader.events.try_recv() {
            seen.push(event);
        }
        let lefts = seen
            .iter()
            .filter(|e| matches!(e, ServerEvent::Left { id: 1 }))
            .count();
        assert_eq!(lefts, 1);
        assert!(!resolves(&mut reader.departed).await);
        assert!(idle.events.try_recv().is_ok());
    }

    #[test]
    fn throttled_state_changes_wait_and_the_latest_wins() {
        let start = Instant::now();
        let mut throttle = Throttle::new();
        let chat = || ClientEvent::Chat { text: "hi".into() };
        let state = |audio_muted| ClientEvent::SetState {
            audio_muted,
            video_off: false,
        };
        for _ in 0..CLIENT_EVENT_BURST as usize {
            assert!(throttle.take(chat(), start).is_some());
        }
        assert!(throttle.take(chat(), start).is_none());
        assert!(throttle.retry_in(start).is_none());

        assert!(throttle.take(state(true), start).is_none());
        assert!(throttle.take(state(false), start).is_none());
        let wait = throttle.retry_in(start).unwrap();
        assert!(wait > Duration::ZERO);
        assert!(throttle.retry(start).is_none());
        assert_eq!(throttle.retry(start + wait), Some(state(false)));
        assert!(throttle.retry_in(start + wait).is_none());

        let later = start + Duration::from_secs(60);
        assert!(throttle.take(state(true), start + wait).is_none());
        assert_eq!(throttle.take(state(false), later), Some(state(false)));
        assert!(throttle.retry_in(later).is_none());
    }

    #[test]
    fn chat_loses_control_characters_and_excess_length() {
        assert_eq!(clean_chat("  hi\x1b[2J there\n"), "hi [2J there");
        assert_eq!(clean_chat("\x07\t "), "");
        assert_eq!(
            clean_chat(&"x".repeat(signal::MAX_CHAT_LEN + 10)).len(),
            signal::MAX_CHAT_LEN
        );
    }
}
