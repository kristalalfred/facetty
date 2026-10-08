use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use facetty_proto::ParticipantId;
use facetty_proto::ladder::Rung;
use facetty_proto::media::{ServerControl, VideoFrame, VideoSender};
use quinn::Connection;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;

#[derive(Default)]
pub struct Rooms {
    rooms: Mutex<HashMap<String, Room>>,
    generation: AtomicU64,
}

#[derive(Default)]
struct Room {
    members: HashMap<ParticipantId, Member>,
    /// Keyed by (subscriber, publisher). A subscription may name a publisher
    /// that has not connected yet; it takes effect when the publisher joins.
    subscriptions: HashMap<(ParticipantId, ParticipantId), Subscription>,
}

struct Member {
    conn: Connection,
    video: Arc<VideoSender>,
    control: mpsc::UnboundedSender<ServerControl>,
    encode_rungs: Vec<Rung>,
    generation: u64,
}

struct Subscription {
    rung: Rung,
    waiting: Arc<Waiting>,
    writer: JoinHandle<()>,
}

/// The frame a subscriber gets next. One that arrives while another still
/// waits is merged into it, so a slow link skips frames but no changes.
#[derive(Default)]
struct Waiting {
    frame: Mutex<Option<VideoFrame>>,
    filled: Notify,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.writer.abort();
    }
}

/// Removes the member from its room when dropped.
pub struct Membership {
    rooms: Arc<Rooms>,
    room: String,
    participant: ParticipantId,
    generation: u64,
}

impl Rooms {
    pub fn join(
        self: &Arc<Self>,
        room_name: &str,
        participant: ParticipantId,
        conn: Connection,
        control: mpsc::UnboundedSender<ServerControl>,
    ) -> Membership {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let mut rooms = self.lock();
        let room = rooms.entry(room_name.to_string()).or_default();
        if let Some(old) = room.members.remove(&participant) {
            old.conn
                .close(0u32.into(), b"replaced by a newer connection");
            room.subscriptions.retain(|(sub, _), _| *sub != participant);
        }
        room.members.insert(
            participant,
            Member {
                video: VideoSender::new(conn.clone()),
                conn,
                control,
                encode_rungs: Vec::new(),
                generation,
            },
        );
        room.update_demand(participant);
        Membership {
            rooms: self.clone(),
            room: room_name.to_string(),
            participant,
            generation,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Room>> {
        self.rooms.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Membership {
    pub fn participant(&self) -> ParticipantId {
        self.participant
    }

    pub fn subscribe(&self, publisher: ParticipantId, rung: Option<Rung>) {
        let mut rooms = self.rooms.lock();
        let Some(room) = rooms.get_mut(&self.room) else {
            return;
        };
        let key = (self.participant, publisher);
        match rung {
            None => {
                room.subscriptions.remove(&key);
            }
            Some(rung) => {
                let changed = if let Some(sub) = room.subscriptions.get_mut(&key) {
                    let changed = std::mem::replace(&mut sub.rung, rung) != rung;
                    if changed {
                        sub.waiting.take();
                    }
                    changed
                } else if let Some(me) = room.members.get(&self.participant) {
                    room.subscriptions
                        .insert(key, Subscription::start(me.video.clone(), rung));
                    true
                } else {
                    false
                };
                if changed
                    && let Some(member) = room.members.get(&publisher)
                    && member.encode_rungs.contains(&rung)
                {
                    let _ = member.control.send(ServerControl::Refresh { rung });
                }
            }
        }
        room.update_demand(publisher);
    }

    pub fn publish_video(&self, frame: VideoFrame) {
        let subscribers: Vec<Arc<Waiting>> = {
            let rooms = self.rooms.lock();
            let Some(room) = rooms.get(&self.room) else {
                return;
            };
            room.subscriptions
                .iter()
                .filter(|((_, publisher), sub)| {
                    *publisher == self.participant && sub.rung == frame.rung
                })
                .map(|(_, sub)| sub.waiting.clone())
                .collect()
        };
        for waiting in subscribers {
            waiting.offer(frame.clone());
        }
    }

    pub fn publish_audio(&self, packet: Bytes) {
        let rooms = self.rooms.lock();
        let Some(room) = rooms.get(&self.room) else {
            return;
        };
        for (id, member) in &room.members {
            if *id != self.participant {
                let _ = member.conn.send_datagram(packet.clone());
            }
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        let mut rooms = self.rooms.lock();
        let Some(room) = rooms.get_mut(&self.room) else {
            return;
        };
        let is_current = room
            .members
            .get(&self.participant)
            .is_some_and(|m| m.generation == self.generation);
        if !is_current {
            return;
        }
        room.members.remove(&self.participant);
        let mut affected = Vec::new();
        room.subscriptions.retain(|&(sub, publisher), _| {
            if sub == self.participant {
                affected.push(publisher);
            }
            sub != self.participant
        });
        for publisher in affected {
            room.update_demand(publisher);
        }
        if room.members.is_empty() {
            rooms.remove(&self.room);
        }
    }
}

impl Room {
    fn update_demand(&mut self, publisher: ParticipantId) {
        let mut rungs: Vec<Rung> = self
            .subscriptions
            .iter()
            .filter(|((_, p), _)| *p == publisher)
            .map(|(_, s)| s.rung)
            .collect();
        rungs.sort_unstable();
        rungs.dedup();
        let Some(member) = self.members.get_mut(&publisher) else {
            return;
        };
        if member.encode_rungs != rungs {
            member.encode_rungs = rungs.clone();
            let _ = member.control.send(ServerControl::EncodeRungs { rungs });
        }
    }
}

impl Subscription {
    fn start(video: Arc<VideoSender>, rung: Rung) -> Self {
        let waiting = Arc::new(Waiting::default());
        let next = waiting.clone();
        let writer = tokio::spawn(async move {
            loop {
                next.filled().await;
                video.ready().await;
                let Some(frame) = next.take() else {
                    continue;
                };
                if let Err(e) = video.send(&frame).await {
                    debug!("video forward failed: {e}");
                    if matches!(e, quinn::WriteError::ConnectionLost(_)) {
                        break;
                    }
                }
            }
        });
        Self {
            rung,
            waiting,
            writer,
        }
    }
}

impl Waiting {
    fn offer(&self, frame: VideoFrame) {
        let mut waiting = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        *waiting = Some(match waiting.take() {
            Some(earlier) => fold(earlier, frame),
            None => frame,
        });
        drop(waiting);
        self.filled.notify_one();
    }

    fn take(&self) -> Option<VideoFrame> {
        self.frame.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    async fn filled(&self) {
        while self
            .frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            self.filled.notified().await;
        }
    }
}

/// One frame with the changes of both. Frames can arrive out of order, and
/// the one numbered later wins where both change a cell.
fn fold(a: VideoFrame, b: VideoFrame) -> VideoFrame {
    let (older, newer) = if (b.seq.wrapping_sub(a.seq) as i32) > 0 {
        (a, b)
    } else {
        (b, a)
    };
    match facetty_ascii::merge(&older.payload, &newer.payload) {
        Ok(payload) => VideoFrame { payload, ..newer },
        Err(e) => {
            debug!("could not merge video frames: {e}");
            newer
        }
    }
}
