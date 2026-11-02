use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::FRAME_SAMPLES;
use crate::jitter::JitterBuffer;

struct Remote {
    jitter: JitterBuffer,
    level: f32,
}

/// Every publisher's jitter buffer, mixed into one 20 ms frame at a time by
/// whatever drives playback.
pub struct Mix {
    remotes: HashMap<u32, Remote>,
    scratch: [f32; FRAME_SAMPLES],
}

impl Default for Mix {
    fn default() -> Self {
        Mix {
            remotes: HashMap::new(),
            scratch: [0.0; FRAME_SAMPLES],
        }
    }
}

impl Mix {
    pub fn push(&mut self, publisher: u32, seq: u16, payload: &[u8]) {
        let remote = match self.remotes.entry(publisher) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => match JitterBuffer::new() {
                Ok(jitter) => e.insert(Remote { jitter, level: 0.0 }),
                Err(err) => {
                    tracing::warn!("audio decoder for {publisher}: {err}");
                    return;
                }
            },
        };
        remote.jitter.push(seq, payload);
    }

    pub fn remove(&mut self, publisher: u32) {
        self.remotes.remove(&publisher);
    }

    pub fn mix(&mut self, out: &mut [f32; FRAME_SAMPLES]) {
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
    }

    /// Each publisher's smoothed loudness, 0 to 1, as of the last mix.
    pub fn levels(&self) -> impl Iterator<Item = (u32, f32)> + '_ {
        self.remotes.iter().map(|(&id, r)| (id, r.level))
    }
}

pub fn loudness(frame: &[f32]) -> f32 {
    let mean_square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
    let db = 10.0 * mean_square.max(1e-10).log10();
    ((db + 55.0) / 40.0).clamp(0.0, 1.0)
}

pub fn smooth(previous: f32, current: f32) -> f32 {
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
