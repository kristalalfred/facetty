use std::collections::BTreeMap;

use anyhow::Result;

use crate::FRAME_SAMPLES;
use crate::opus::Decoder;

const TARGET_DEPTH: usize = 3;
const MAX_DEPTH: u64 = 10;
const MAX_CONCEALED: u32 = 5;
const RESET_JUMP: i64 = 1_000;

/// Reorders one publisher's packets and turns them into a steady stream of
/// 20 ms frames, concealing short losses.
pub struct JitterBuffer {
    decoder: Decoder,
    packets: BTreeMap<u64, Vec<u8>>,
    highest: Option<u64>,
    next: Option<u64>,
    played: Option<u64>,
    last_real: Option<u64>,
    concealed: u32,
}

impl JitterBuffer {
    pub fn new() -> Result<Self> {
        Ok(JitterBuffer {
            decoder: Decoder::new()?,
            packets: BTreeMap::new(),
            highest: None,
            next: None,
            played: None,
            last_real: None,
            concealed: 0,
        })
    }

    pub fn push(&mut self, seq: u16, payload: &[u8]) {
        let ext = match self.highest {
            // Starting high leaves room below for packets that arrive reordered.
            None => (1 << 32) + seq as u64,
            Some(h) => {
                let delta = seq.wrapping_sub(h as u16) as i16 as i64;
                if delta.abs() > RESET_JUMP {
                    self.reset();
                    (1 << 32) + seq as u64
                } else {
                    (h as i64 + delta) as u64
                }
            }
        };
        if self.played.is_some_and(|p| ext <= p) {
            return;
        }
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        self.packets.entry(ext).or_insert_with(|| payload.to_vec());

        if let (Some(next), Some(highest)) = (self.next, self.highest)
            && (highest + 1).saturating_sub(next) > MAX_DEPTH
        {
            let next = highest + 1 - TARGET_DEPTH as u64;
            self.packets = self.packets.split_off(&next);
            self.next = Some(next);
        }
    }

    /// Fills `out` with the next 20 ms. Returns false, with `out` silent, while
    /// buffering or after the publisher went quiet.
    pub fn pull(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> bool {
        let next = match self.next {
            Some(n) => n,
            None if self.packets.len() >= TARGET_DEPTH => *self.packets.keys().next().unwrap(),
            None => {
                out.fill(0.0);
                return false;
            }
        };

        let decoded = if let Some(packet) = self.packets.remove(&next) {
            self.last_real = Some(next);
            self.concealed = 0;
            self.decoder.decode(&packet, out)
        } else if let Some(following) = self.packets.get(&(next + 1)) {
            self.concealed = 0;
            self.decoder.decode_fec(following, out)
        } else {
            self.concealed += 1;
            if self.concealed > MAX_CONCEALED {
                self.next = None;
                self.played = self.last_real;
                self.concealed = 0;
                out.fill(0.0);
                return false;
            }
            self.decoder.conceal(out)
        };
        if let Err(e) = decoded {
            tracing::debug!("opus decode: {e}");
            out.fill(0.0);
        }
        self.played = Some(next);
        self.next = Some(next + 1);
        true
    }

    fn reset(&mut self) {
        self.packets.clear();
        self.highest = None;
        self.next = None;
        self.played = None;
        self.last_real = None;
        self.concealed = 0;
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.packets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opus::Encoder;

    fn packets(start: u16, count: u16) -> Vec<(u16, Vec<u8>)> {
        let mut enc = Encoder::new().unwrap();
        (0..count)
            .map(|i| {
                let seq = start.wrapping_add(i);
                let tone: [f32; FRAME_SAMPLES] = std::array::from_fn(|n| {
                    let f = 200.0 + 20.0 * i as f32;
                    0.3 * (2.0 * std::f32::consts::PI * f * n as f32 / 48_000.0).sin()
                });
                (seq, enc.encode(&tone).unwrap())
            })
            .collect()
    }

    fn pull_all(jb: &mut JitterBuffer, n: usize) -> Vec<bool> {
        let mut out = [0f32; FRAME_SAMPLES];
        (0..n).map(|_| jb.pull(&mut out)).collect()
    }

    #[test]
    fn buffers_before_playing() {
        let mut jb = JitterBuffer::new().unwrap();
        let p = packets(0, 3);
        jb.push(p[0].0, &p[0].1);
        jb.push(p[1].0, &p[1].1);
        assert_eq!(pull_all(&mut jb, 1), [false]);
        jb.push(p[2].0, &p[2].1);
        assert_eq!(pull_all(&mut jb, 3), [true, true, true]);
    }

    #[test]
    fn reorders_across_wraparound() {
        let mut jb = JitterBuffer::new().unwrap();
        let p = packets(65_534, 4);
        for i in [1, 0, 3, 2] {
            jb.push(p[i].0, &p[i].1);
        }
        assert_eq!(jb.depth(), 4);
        let base = jb.packets.keys().next().copied().unwrap();
        assert_eq!(
            jb.packets.keys().copied().collect::<Vec<_>>(),
            (base..base + 4).collect::<Vec<_>>()
        );
        assert_eq!(pull_all(&mut jb, 4), [true; 4]);
        assert_eq!(jb.depth(), 0);
    }

    #[test]
    fn conceals_a_lost_packet_then_goes_idle() {
        let mut jb = JitterBuffer::new().unwrap();
        let p = packets(10, 6);
        for (i, (seq, data)) in p.iter().enumerate() {
            if i != 3 {
                jb.push(*seq, data);
            }
        }
        assert_eq!(pull_all(&mut jb, 6), [true; 6]);
        let tail = pull_all(&mut jb, MAX_CONCEALED as usize + 1);
        assert!(tail[..MAX_CONCEALED as usize].iter().all(|&b| b));
        assert!(!tail[MAX_CONCEALED as usize]);
    }

    #[test]
    fn drops_late_and_duplicate_packets() {
        let mut jb = JitterBuffer::new().unwrap();
        let p = packets(0, 5);
        for (seq, data) in &p[..4] {
            jb.push(*seq, data);
        }
        pull_all(&mut jb, 2);
        jb.push(p[0].0, &p[0].1);
        jb.push(p[2].0, &p[2].1);
        assert_eq!(jb.depth(), 2);
    }

    #[test]
    fn resumes_after_sender_pause_without_dropping_onset() {
        let mut jb = JitterBuffer::new().unwrap();
        let p = packets(0, 8);
        for (seq, data) in &p[..3] {
            jb.push(*seq, data);
        }
        pull_all(&mut jb, 3 + MAX_CONCEALED as usize + 1);
        for (seq, data) in &p[3..6] {
            jb.push(*seq, data);
        }
        assert_eq!(jb.depth(), 3);
        assert_eq!(pull_all(&mut jb, 3), [true; 3]);
    }

    #[test]
    fn caps_latency_when_packets_pile_up() {
        let mut jb = JitterBuffer::new().unwrap();
        let p = packets(0, 30);
        for (seq, data) in &p[..3] {
            jb.push(*seq, data);
        }
        pull_all(&mut jb, 1);
        for (seq, data) in &p[3..] {
            jb.push(*seq, data);
        }
        assert!(jb.depth() <= MAX_DEPTH as usize, "depth {}", jb.depth());
    }
}
