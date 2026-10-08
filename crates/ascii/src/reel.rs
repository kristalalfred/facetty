//! Pre-rendered animations stored at several grid sizes, so a player can pick
//! the largest that fits its terminal.
//!
//! The file is `MAGIC`, a count, one header per reel (`cols`, `rows`, frame
//! count, `loop_start`, `exit_start` as u16, body length as u32), then the
//! bodies. A body is zstd over every frame's two cell planes, packed as in
//! the call codec and XORed with the previous frame's.

use crate::codec::{Packed, pack, unpack};
use crate::{DecodeError, Frame};

const MAGIC: &[u8; 6] = b"FTREEL";
const VERSION: u8 = 1;
const HEADER: usize = 14;
const ZSTD_LEVEL: i32 = 19;
const MAX_BYTES: usize = 64 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reel {
    pub frames: Vec<Frame>,
    /// First frame of the idle loop, which runs up to `exit_start`.
    pub loop_start: usize,
    /// First frame of the exit, which runs to the end.
    pub exit_start: usize,
}

impl Reel {
    pub fn size(&self) -> (u16, u16) {
        self.frames.first().map_or((0, 0), |f| (f.cols, f.rows))
    }
}

pub fn encode(reels: &[Reel]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend([VERSION, reels.len() as u8]);
    let bodies: Vec<Vec<u8>> = reels.iter().map(body).collect();
    for (reel, body) in reels.iter().zip(&bodies) {
        let (cols, rows) = reel.size();
        for v in [
            cols,
            rows,
            reel.frames.len() as u16,
            reel.loop_start as u16,
            reel.exit_start as u16,
        ] {
            out.extend(v.to_le_bytes());
        }
        out.extend((body.len() as u32).to_le_bytes());
    }
    for body in bodies {
        out.extend(body);
    }
    out
}

fn body(reel: &Reel) -> Vec<u8> {
    let (cols, rows) = reel.size();
    let cells = cols as usize * rows as usize;
    let mut prev = vec![[0u8; 2]; cells];
    let mut raw = Vec::with_capacity(cells * 2 * reel.frames.len());
    for frame in &reel.frames {
        assert_eq!(
            (frame.cols, frame.rows),
            (cols, rows),
            "reel frames differ in size"
        );
        let packed: Vec<Packed> = frame.cells.iter().map(pack).collect();
        for plane in 0..2 {
            raw.extend(packed.iter().zip(&prev).map(|(p, q)| p[plane] ^ q[plane]));
        }
        prev = packed;
    }
    zstd::bulk::compress(&raw, ZSTD_LEVEL).expect("zstd compression into Vec")
}

/// Decodes the reel in `data` with the most cells that fits within
/// `cols` x `rows`, if any does.
pub fn decode_fitting(data: &[u8], cols: u16, rows: u16) -> Result<Option<Reel>, DecodeError> {
    if data.len() < MAGIC.len() + 2 {
        return Err(DecodeError::Truncated);
    }
    if &data[..MAGIC.len()] != MAGIC {
        return Err(DecodeError::Corrupt);
    }
    if data[MAGIC.len()] != VERSION {
        return Err(DecodeError::Version(data[MAGIC.len()]));
    }
    let count = data[MAGIC.len() + 1] as usize;
    let headers = &data[MAGIC.len() + 2..];
    if headers.len() < count * HEADER {
        return Err(DecodeError::Truncated);
    }
    let mut offset = MAGIC.len() + 2 + count * HEADER;
    let mut best: Option<(usize, [u16; 5], usize, usize)> = None;
    for h in headers[..count * HEADER].chunks_exact(HEADER) {
        let v: [u16; 5] = std::array::from_fn(|i| u16::from_le_bytes([h[2 * i], h[2 * i + 1]]));
        let len = u32::from_le_bytes([h[10], h[11], h[12], h[13]]) as usize;
        let cells = v[0] as usize * v[1] as usize;
        if v[0] <= cols && v[1] <= rows && best.is_none_or(|b| cells > b.0) {
            best = Some((cells, v, offset, len));
        }
        offset += len;
    }
    let Some((cells, [c, r, frames, loop_start, exit_start], start, len)) = best else {
        return Ok(None);
    };
    let compressed = data.get(start..start + len).ok_or(DecodeError::Truncated)?;
    let size = cells * 2 * frames as usize;
    if size > MAX_BYTES {
        return Err(DecodeError::TooLarge(c, r));
    }
    let raw = zstd::bulk::decompress(compressed, size).map_err(|_| DecodeError::Corrupt)?;
    if raw.len() != size {
        return Err(DecodeError::Corrupt);
    }
    let mut prev = vec![[0u8; 2]; cells];
    let frames = raw
        .chunks_exact(cells * 2)
        .map(|planes| {
            let (hi, lo) = planes.split_at(cells);
            for (i, p) in prev.iter_mut().enumerate() {
                *p = [p[0] ^ hi[i], p[1] ^ lo[i]];
            }
            Frame {
                cols: c,
                rows: r,
                cells: prev.iter().map(|&p| unpack(p)).collect(),
            }
        })
        .collect();
    Ok(Some(Reel {
        frames,
        loop_start: loop_start as usize,
        exit_start: exit_start as usize,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cell;

    fn reel(cols: u16, rows: u16, frames: usize) -> Reel {
        let frames = (0..frames)
            .map(|t| Frame {
                cols,
                rows,
                cells: (0..cols as usize * rows as usize)
                    .map(|i| Cell {
                        glyph: ((i + t) % 14) as u8,
                        rgb: [(i % 16 * 17) as u8, (t % 16 * 17) as u8, 0xff],
                    })
                    .collect(),
            })
            .collect();
        Reel {
            frames,
            loop_start: 1,
            exit_start: 2,
        }
    }

    #[test]
    fn round_trips_the_largest_reel_that_fits() {
        let small = reel(8, 3, 4);
        let large = reel(16, 6, 4);
        let data = encode(&[small.clone(), large.clone()]);
        assert_eq!(decode_fitting(&data, 20, 10).unwrap(), Some(large));
        assert_eq!(decode_fitting(&data, 15, 10).unwrap(), Some(small));
        assert_eq!(decode_fitting(&data, 7, 10).unwrap(), None);
    }

    #[test]
    fn rejects_other_data() {
        assert_eq!(decode_fitting(b"FTREEL", 1, 1), Err(DecodeError::Truncated));
        assert_eq!(decode_fitting(b"nonsense", 1, 1), Err(DecodeError::Corrupt));
    }
}
