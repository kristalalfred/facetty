//! Wire format for a [`Frame`]: a 5-byte header, then zstd over two planes of
//! one byte per cell, `glyph << 4 | blue` and `red << 4 | green`, with each
//! color channel cut to 4 bits.

use crate::{Cell, Frame, GLYPHS};

const VERSION: u8 = 1;
const HEADER: usize = 5;
const MAX_CELLS: usize = 512 * 256;
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("frame is shorter than its header")]
    Truncated,
    #[error("unsupported frame version {0}")]
    Version(u8),
    #[error("frame of {0}x{1} cells is too large")]
    TooLarge(u16, u16),
    #[error("corrupt frame body")]
    Corrupt,
}

pub fn encode(frame: &Frame) -> Vec<u8> {
    let n = frame.cells.len();
    let mut planes = vec![0u8; 2 * n];
    let (hi, lo) = planes.split_at_mut(n);
    for (i, cell) in frame.cells.iter().enumerate() {
        let [r, g, b] = cell.rgb.map(to_nibble);
        hi[i] = cell.glyph << 4 | b;
        lo[i] = r << 4 | g;
    }
    let body = zstd::bulk::compress(&planes, ZSTD_LEVEL).expect("zstd compression into Vec");
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.push(VERSION);
    out.extend(frame.cols.to_le_bytes());
    out.extend(frame.rows.to_le_bytes());
    out.extend(body);
    out
}

pub fn decode(bytes: &[u8]) -> Result<Frame, DecodeError> {
    if bytes.len() < HEADER {
        return Err(DecodeError::Truncated);
    }
    if bytes[0] != VERSION {
        return Err(DecodeError::Version(bytes[0]));
    }
    let cols = u16::from_le_bytes([bytes[1], bytes[2]]);
    let rows = u16::from_le_bytes([bytes[3], bytes[4]]);
    let n = cols as usize * rows as usize;
    if n > MAX_CELLS {
        return Err(DecodeError::TooLarge(cols, rows));
    }
    let planes =
        zstd::bulk::decompress(&bytes[HEADER..], 2 * n).map_err(|_| DecodeError::Corrupt)?;
    if planes.len() != 2 * n {
        return Err(DecodeError::Corrupt);
    }
    let (hi, lo) = planes.split_at(n);
    let cells = hi
        .iter()
        .zip(lo)
        .map(|(&hi, &lo)| Cell {
            glyph: (hi >> 4).min(GLYPHS.len() as u8 - 1),
            rgb: [lo >> 4, lo & 0xf, hi & 0xf].map(from_nibble),
        })
        .collect();
    Ok(Frame { cols, rows, cells })
}

fn to_nibble(v: u8) -> u8 {
    ((v as u32 * 15 + 127) / 255) as u8
}

fn from_nibble(v: u8) -> u8 {
    v * 17
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_glyphs_and_quantizes_color() {
        let frame = Frame {
            cols: 3,
            rows: 2,
            cells: (0..6)
                .map(|i| Cell {
                    glyph: i as u8 * 2,
                    rgb: [i as u8 * 40, 255, 0],
                })
                .collect(),
        };
        let back = decode(&encode(&frame)).unwrap();
        assert_eq!((back.cols, back.rows), (3, 2));
        for (a, b) in frame.cells.iter().zip(&back.cells) {
            assert_eq!(a.glyph, b.glyph);
            for c in 0..3 {
                assert!((a.rgb[c] as i32 - b.rgb[c] as i32).abs() <= 8);
            }
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(decode(&[]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[9, 1, 0, 1, 0]), Err(DecodeError::Version(9)));
        assert_eq!(
            decode(&[VERSION, 1, 0, 1, 0, 1, 2, 3]),
            Err(DecodeError::Corrupt)
        );
        assert_eq!(
            decode(&[VERSION, 0xff, 0xff, 0xff, 0xff]),
            Err(DecodeError::TooLarge(0xffff, 0xffff))
        );
    }
}
