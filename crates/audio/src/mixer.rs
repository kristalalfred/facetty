use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use crate::jitter::JitterBuffer;
use crate::{FRAME_SAMPLES, Shared};

pub(crate) enum Command {
    Packet {
        publisher: u32,
        seq: u16,
        payload: Vec<u8>,
    },
    Remove(u32),
}

struct Remote {
    jitter: JitterBuffer,
    level: f32,
}

/// Owned by whichever thread drives playback (the device callback or the
/// headless clock). Packets reach it through `Shared::inbox`, so the network
/// side never waits on decoding.
pub(crate) struct Mixer {
    shared: Arc<Shared>,
    remotes: HashMap<u32, Remote>,
    commands: Vec<Command>,
    scratch: [f32; FRAME_SAMPLES],
}

impl Mixer {
    pub fn new(shared: Arc<Shared>) -> Self {
        Mixer {
            shared,
            remotes: HashMap::new(),
            commands: Vec::new(),
            scratch: [0.0; FRAME_SAMPLES],
        }
    }

    pub fn mix(&mut self, out: &mut [f32; FRAME_SAMPLES]) {
        std::mem::swap(&mut self.commands, &mut self.shared.inbox.lock().unwrap());
        for command in self.commands.drain(..) {
            match command {
                Command::Packet {
                    publisher,
                    seq,
                    payload,
                } => {
                    let remote = match self.remotes.entry(publisher) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => match JitterBuffer::new() {
                            Ok(jitter) => e.insert(Remote { jitter, level: 0.0 }),
                            Err(err) => {
                                tracing::warn!("audio decoder for {publisher}: {err}");
                                continue;
                            }
                        },
                    };
                    remote.jitter.push(seq, &payload);
                }
                Command::Remove(publisher) => {
                    self.remotes.remove(&publisher);
                }
            }
        }

        out.fill(0.0);
        for remote in self.remotes.values_mut() {
            let loudness = if remote.jitter.pull(&mut self.scratch) {
                for (o, s) in out.iter_mut().zip(&self.scratch) {
                    *o += s;
                }
                loudness(&self.scratch)
            } else {
                0.0
            };
            remote.level = smooth(remote.level, loudness);
        }
        for s in out.iter_mut() {
            *s = soft_clip(*s);
        }

        let mut levels = self.shared.levels.lock().unwrap();
        levels.clear();
        levels.extend(self.remotes.iter().map(|(&id, r)| (id, r.level)));
    }
}

pub(crate) fn loudness(frame: &[f32]) -> f32 {
    let mean_square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
    let db = 10.0 * mean_square.max(1e-10).log10();
    ((db + 55.0) / 40.0).clamp(0.0, 1.0)
}

pub(crate) fn smooth(previous: f32, current: f32) -> f32 {
    if current > previous {
        current
    } else {
        previous * 0.85 + current * 0.15
    }
}

fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_is_transparent_below_knee_and_bounded_above() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.8), -0.8);
        assert!(soft_clip(3.0) <= 1.0 && soft_clip(3.0) > 0.95);
        assert!(soft_clip(-3.0) >= -1.0);
    }

    #[test]
    fn loudness_maps_silence_to_zero_and_full_scale_to_one() {
        assert_eq!(loudness(&[0.0; 960]), 0.0);
        assert_eq!(loudness(&[1.0; 960]), 1.0);
        let speech = loudness(&[0.05; 960]);
        assert!(speech > 0.3 && speech < 0.9, "{speech}");
    }
}
