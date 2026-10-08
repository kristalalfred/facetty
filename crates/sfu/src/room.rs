use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bits_proto::ParticipantId;
use bits_proto::ladder::Rung;
use bits_proto::media::ServerControl;
use bytes::Bytes;
use quinn::Connection;
use tokio::sync::{mpsc, watch};
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
    control: mpsc::UnboundedSender<ServerControl>,
    encode_rungs: Vec<Rung>,
    generation: u64,
}

struct Subscription {
    rung: Rung,
    latest: watch::Sender<Option<Bytes>>,
    writer: JoinHandle<()>,
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
                if let Some(sub) = room.subscriptions.get_mut(&key) {
                    sub.rung = rung;
                } else if let Some(me) = room.members.get(&self.participant) {
                    room.subscriptions
                        .insert(key, Subscription::start(me.conn.clone(), rung));
                }
            }
        }
        room.update_demand(publisher);
    }

    pub fn publish_video(&self, rung: Rung, frame: Bytes) {
        let rooms = self.rooms.lock();
        let Some(room) = rooms.get(&self.room) else {
            return;
        };
        for ((_, publisher), sub) in &room.subscriptions {
            if *publisher == self.participant && sub.rung == rung {
                sub.latest.send_replace(Some(frame.clone()));
            }
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
    /// Forwards only the newest frame: while a write is stuck behind
    /// congestion, newer frames overwrite older ones in the watch slot.
    fn start(conn: Connection, rung: Rung) -> Self {
        let (latest, mut rx) = watch::channel::<Option<Bytes>>(None);
        let writer = tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(frame) = rx.borrow_and_update().clone() else {
                    continue;
                };
                let result = async {
                    let mut stream = conn.open_uni().await?;
                    stream.write_all(&frame).await?;
                    stream.finish()?;
                    anyhow::Ok(())
                }
                .await;
                if let Err(e) = result {
                    debug!("video forward failed: {e}");
                    if conn.close_reason().is_some() {
                        break;
                    }
                }
            }
        });
        Self {
            rung,
            latest,
            writer,
        }
    }
}
