//! Wire format for a [`Frame`]: a 6-byte header, then zstd over the body.
//!
//! A full frame's body is two planes of one byte per cell, `glyph << 4 | blue`
//! and `red << 4 | green`, with each color channel cut to 4 bits. An update's
//! body is a bitmask of the cells it changes, then the same two planes for
//! just those cells. Updates carry new values rather than differences, so a
//! receiver that missed one shows a few old cells instead of garbage, and each
//! update also resends one rotating band of rows to correct those.

use crate::{Cell, Frame, GLYPHS};

const VERSION: u8 = 2;
const HEADER: usize = 6;
const FULL: u8 = 0;
const UPDATE: u8 = 1;
const MAX_CELLS: usize = 512 * 256;
const ZSTD_LEVEL: i32 = 9;
/// A shown color channel only changes once the source drifts more than one
/// 4-bit step away from it, so sensor noise does not resend the cell.
const COLOR_HOLD: i32 = 17;
/// Each update resends every row whose index matches the frame count modulo
/// this, so every cell is resent at least this often.
const REFRESH_PERIOD: usize = 30;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("frame is shorter than its header")]
    Truncated,
    #[error("unsupported frame version {0}")]
    Version(u8),
    #[error("unknown frame kind {0}")]
    Kind(u8),
    #[error("frame of {0}x{1} cells is too large")]
    TooLarge(u16, u16),
    #[error("corrupt frame body")]
    Corrupt,
}

/// Cell as sent: `[glyph << 4 | blue, red << 4 | green]`.
pub(crate) type Packed = [u8; 2];

pub(crate) fn pack(cell: &Cell) -> Packed {
    let [r, g, b] = cell.rgb.map(to_nibble);
    [cell.glyph << 4 | b, r << 4 | g]
}

pub(crate) fn unpack([hi, lo]: Packed) -> Cell {
    Cell {
        glyph: (hi >> 4).min(GLYPHS.len() as u8 - 1),
        rgb: [lo >> 4, lo & 0xf, hi & 0xf].map(from_nibble),
    }
}

/// Whether `cell` has moved far enough from what was sent to send it again.
fn drifted(cell: &Cell, sent: Packed) -> bool {
    let shown = unpack(sent);
    cell.glyph != shown.glyph
        || (0..3).any(|c| (cell.rgb[c] as i32 - shown.rgb[c] as i32).abs() > COLOR_HOLD)
}

#[derive(Clone, Default)]
struct EncoderState {
    cols: u16,
    rows: u16,
    sent: Vec<Packed>,
    count: usize,
    full_requested: bool,
}

/// Encodes one stream of frames, sending only what changed since the last
/// frame it encoded.
#[derive(Default)]
pub struct Encoder {
    state: EncoderState,
    before: Option<EncoderState>,
}

impl Encoder {
    /// The next frame is sent in full.
    pub fn request_full(&mut self) {
        self.state.full_requested = true;
    }

    /// Forgets the last [`encode`](Self::encode), for when that frame was
    /// never sent.
    pub fn undo(&mut self) {
        if let Some(before) = self.before.take() {
            self.state = before;
        }
    }

    pub fn encode(&mut self, frame: &Frame) -> Vec<u8> {
        self.before = Some(self.state.clone());
        let state = &mut self.state;
        let full = state.full_requested
            || state.cols != frame.cols
            || state.rows != frame.rows
            || state.sent.len() != frame.cells.len();
        state.count = state.count.wrapping_add(1);
        let (kind, body) = if full {
            state.cols = frame.cols;
            state.rows = frame.rows;
            state.sent = frame.cells.iter().map(pack).collect();
            state.full_requested = false;
            (FULL, planes(&state.sent))
        } else {
            let cols = frame.cols.max(1) as usize;
            let mut changes = Vec::new();
            for (i, (cell, sent)) in frame.cells.iter().zip(&mut state.sent).enumerate() {
                let refresh = (i / cols) % REFRESH_PERIOD == state.count % REFRESH_PERIOD;
                if drifted(cell, *sent) {
                    *sent = pack(cell);
                } else if !refresh {
                    continue;
                }
                changes.push((i, *sent));
            }
            (UPDATE, update_body(frame.cells.len(), &changes))
        };
        seal(kind, frame.cols, frame.rows, &body)
    }
}

/// Decodes one stream of frames, numbered by the sender. Frames may arrive
/// out of order: each cell keeps the value from the newest frame that set it,
/// and a frame older than the last change of size is ignored. An update for a
/// size the decoder has no frame of applies to a blank frame.
#[derive(Default)]
pub struct Decoder {
    cols: u16,
    rows: u16,
    cells: Vec<Packed>,
    set_by: Vec<Option<u32>>,
    newest: Option<u32>,
}

impl Decoder {
    /// Applies frame number `seq` and returns the picture so far.
    pub fn decode(&mut self, seq: u32, bytes: &[u8]) -> Result<Frame, DecodeError> {
        let Parsed { cols, rows, body } = parse(bytes)?;
        if (cols, rows) != (self.cols, self.rows) {
            if self.newest.is_some_and(|newest| is_newer(newest, seq)) {
                return Ok(self.frame());
            }
            let n = cols as usize * rows as usize;
            self.cols = cols;
            self.rows = rows;
            self.cells = vec![[0, 0]; n];
            self.set_by = vec![None; n];
        }
        if self.newest.is_none_or(|newest| is_newer(seq, newest)) {
            self.newest = Some(seq);
        }
        let mut apply = |(i, value): (usize, Packed)| {
            if self.set_by[i].is_none_or(|by| is_newer(seq, by)) {
                self.cells[i] = value;
                self.set_by[i] = Some(seq);
            }
        };
        match body {
            Body::Full(cells) => cells.into_iter().enumerate().for_each(&mut apply),
            Body::Update(changes) => changes.into_iter().for_each(&mut apply),
        }
        Ok(self.frame())
    }

    fn frame(&self) -> Frame {
        Frame {
            cols: self.cols,
            rows: self.rows,
            cells: self.cells.iter().copied().map(unpack).collect(),
        }
    }
}

/// Folds `newer` into `older`, two frames of one stream in that order, giving
/// one frame that leaves a receiver as if it had decoded both. A sender whose
/// link is still busy with earlier frames replaces a waiting frame with this
/// instead of dropping it.
pub fn merge(older: &[u8], newer: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let new = parse(newer)?;
    let Body::Update(changes) = new.body else {
        return Ok(newer.to_vec());
    };
    let old = parse(older)?;
    if (old.cols, old.rows) != (new.cols, new.rows) {
        return Ok(newer.to_vec());
    }
    let n = new.cols as usize * new.rows as usize;
    Ok(match old.body {
        Body::Full(mut cells) => {
            for (i, value) in changes {
                cells[i] = value;
            }
            seal(FULL, new.cols, new.rows, &planes(&cells))
        }
        Body::Update(earlier) => {
            let mut cells = vec![None; n];
            for (i, value) in earlier.into_iter().chain(changes) {
                cells[i] = Some(value);
            }
            let changes: Vec<(usize, Packed)> = cells
                .into_iter()
                .enumerate()
                .filter_map(|(i, value)| Some((i, value?)))
                .collect();
            seal(UPDATE, new.cols, new.rows, &update_body(n, &changes))
        }
    })
}

enum Body {
    Full(Vec<Packed>),
    Update(Vec<(usize, Packed)>),
}

struct Parsed {
    cols: u16,
    rows: u16,
    body: Body,
}

fn parse(bytes: &[u8]) -> Result<Parsed, DecodeError> {
    if bytes.len() < HEADER {
        return Err(DecodeError::Truncated);
    }
    if bytes[0] != VERSION {
        return Err(DecodeError::Version(bytes[0]));
    }
    let kind = bytes[1];
    let cols = u16::from_le_bytes([bytes[2], bytes[3]]);
    let rows = u16::from_le_bytes([bytes[4], bytes[5]]);
    let n = cols as usize * rows as usize;
    if n > MAX_CELLS {
        return Err(DecodeError::TooLarge(cols, rows));
    }
    let mask_len = n.div_ceil(8);
    let capacity = match kind {
        FULL => 2 * n,
        UPDATE => mask_len + 2 * n,
        other => return Err(DecodeError::Kind(other)),
    };
    let body =
        zstd::bulk::decompress(&bytes[HEADER..], capacity).map_err(|_| DecodeError::Corrupt)?;
    let body = if kind == FULL {
        if body.len() != 2 * n {
            return Err(DecodeError::Corrupt);
        }
        Body::Full(unplanes(&body))
    } else {
        if body.len() < mask_len {
            return Err(DecodeError::Corrupt);
        }
        let (mask, values) = body.split_at(mask_len);
        let changed: Vec<usize> = (0..n)
            .filter(|i| mask[i / 8] & (1 << (i % 8)) != 0)
            .collect();
        if values.len() != 2 * changed.len() {
            return Err(DecodeError::Corrupt);
        }
        Body::Update(changed.into_iter().zip(unplanes(values)).collect())
    };
    Ok(Parsed { cols, rows, body })
}

fn seal(kind: u8, cols: u16, rows: u16, body: &[u8]) -> Vec<u8> {
    let body = zstd::bulk::compress(body, ZSTD_LEVEL).expect("zstd compression into Vec");
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend([VERSION, kind]);
    out.extend(cols.to_le_bytes());
    out.extend(rows.to_le_bytes());
    out.extend(body);
    out
}

/// A bitmask of the changed cells out of `n`, then their two planes.
/// `changes` must be in index order, the order the planes are read back in.
fn update_body(n: usize, changes: &[(usize, Packed)]) -> Vec<u8> {
    let mut body = vec![0u8; n.div_ceil(8)];
    for &(i, _) in changes {
        body[i / 8] |= 1 << (i % 8);
    }
    let values: Vec<Packed> = changes.iter().map(|&(_, value)| value).collect();
    body.extend(planes(&values));
    body
}

/// Whether frame number `a` comes after `b`, allowing for wraparound.
fn is_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

fn planes(cells: &[Packed]) -> Vec<u8> {
    let n = cells.len();
    let mut out = vec![0u8; 2 * n];
    let (hi, lo) = out.split_at_mut(n);
    for (i, [h, l]) in cells.iter().enumerate() {
        hi[i] = *h;
        lo[i] = *l;
    }
    out
}

fn unplanes(planes: &[u8]) -> Vec<Packed> {
    let (hi, lo) = planes.split_at(planes.len() / 2);
    hi.iter().zip(lo).map(|(&h, &l)| [h, l]).collect()
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

    fn frame(cols: u16, rows: u16, f: impl Fn(usize) -> Cell) -> Frame {
        Frame {
            cols,
            rows,
            cells: (0..cols as usize * rows as usize).map(f).collect(),
        }
    }

    fn cell(glyph: u8, v: u8) -> Cell {
        Cell {
            glyph,
            rgb: [v, 255, 0],
        }
    }

    #[test]
    fn roundtrip_keeps_glyphs_and_quantizes_color() {
        let original = frame(3, 2, |i| cell(i as u8 * 2, i as u8 * 40));
        let back = Decoder::default()
            .decode(1, &Encoder::default().encode(&original))
            .unwrap();
        assert_eq!((back.cols, back.rows), (3, 2));
        for (a, b) in original.cells.iter().zip(&back.cells) {
            assert_eq!(a.glyph, b.glyph);
            for c in 0..3 {
                assert!((a.rgb[c] as i32 - b.rgb[c] as i32).abs() <= 8);
            }
        }
    }

    #[test]
    fn updates_carry_changed_cells_and_hold_small_color_drift() {
        let mut encoder = Encoder::default();
        let mut decoder = Decoder::default();
        let first = frame(64, 64, |_| cell(1, 100));
        let full = encoder.encode(&first);
        decoder.decode(1, &full).unwrap();

        let next = frame(64, 64, |i| match i {
            0 => cell(2, 100),
            1 => cell(1, 160),
            _ => cell(1, 110),
        });
        let update = encoder.encode(&next);
        assert_eq!(update[1], UPDATE);
        let shown = decoder.decode(2, &update).unwrap();
        assert_eq!(shown.cells[0].glyph, 2);
        assert_eq!(shown.cells[1].rgb[0], 153);
        assert_eq!(
            shown.cells[2].rgb[0], 102,
            "drift within the hold is not sent"
        );
    }

    #[test]
    fn rotating_rows_repair_a_receiver_that_missed_an_update() {
        let mut encoder = Encoder::default();
        let mut decoder = Decoder::default();
        decoder
            .decode(1, &encoder.encode(&frame(4, 40, |_| cell(1, 0))))
            .unwrap();
        let changed = frame(4, 40, |_| cell(3, 255));
        let _missed = encoder.encode(&changed);
        let mut shown = None;
        for seq in 3..3 + REFRESH_PERIOD as u32 {
            shown = Some(decoder.decode(seq, &encoder.encode(&changed)).unwrap());
        }
        assert_eq!(
            shown.unwrap(),
            Decoder::default()
                .decode(1, &Encoder::default().encode(&changed))
                .unwrap()
        );
    }

    #[test]
    fn undo_and_requests_resend_what_a_receiver_lacks() {
        let mut encoder = Encoder::default();
        let mut decoder = Decoder::default();
        decoder
            .decode(1, &encoder.encode(&frame(8, 2, |_| cell(1, 0))))
            .unwrap();
        let changed = frame(8, 2, |_| cell(4, 0));
        encoder.encode(&changed);
        encoder.undo();
        assert_eq!(
            decoder.decode(2, &encoder.encode(&changed)).unwrap().cells[5].glyph,
            4
        );

        encoder.request_full();
        assert_eq!(encoder.encode(&changed)[1], FULL);
        assert_eq!(encoder.encode(&changed)[1], UPDATE);
        assert_eq!(encoder.encode(&frame(4, 2, |_| cell(1, 0)))[1], FULL);
    }

    #[test]
    fn updates_without_a_matching_frame_start_from_blank() {
        let mut encoder = Encoder::default();
        encoder.encode(&frame(4, 1, |_| cell(1, 0)));
        let update = encoder.encode(&frame(4, 1, |i| cell(if i == 0 { 5 } else { 1 }, 0)));
        let shown = Decoder::default().decode(2, &update).unwrap();
        assert_eq!(shown.cells[0].glyph, 5);
        assert_eq!(shown.cells.len(), 4);
    }

    #[test]
    fn late_frames_fill_in_only_what_newer_frames_left() {
        let mut encoder = Encoder::default();
        let full = encoder.encode(&frame(8, 1, |_| cell(1, 0)));
        let both = encoder.encode(&frame(8, 1, |i| cell(if i < 2 { 2 } else { 1 }, 0)));
        let one = encoder.encode(&frame(8, 1, |i| {
            cell(if i < 2 { 2 + i as u8 } else { 1 }, 0)
        }));

        let mut decoder = Decoder::default();
        decoder.decode(1, &full).unwrap();
        decoder.decode(3, &one).unwrap();
        let shown = decoder.decode(2, &both).unwrap();
        assert_eq!(shown.cells[0].glyph, 2);
        assert_eq!(
            shown.cells[1].glyph, 3,
            "the older frame does not undo the newer"
        );
    }

    #[test]
    fn frames_from_before_a_resize_are_ignored() {
        let mut decoder = Decoder::default();
        let small = Encoder::default().encode(&frame(4, 1, |_| cell(1, 0)));
        let large = Encoder::default().encode(&frame(8, 1, |_| cell(2, 0)));
        decoder.decode(5, &large).unwrap();
        let shown = decoder.decode(4, &small).unwrap();
        assert_eq!((shown.cols, shown.rows), (8, 1));
        assert!(shown.cells.iter().all(|c| c.glyph == 2));
    }

    #[test]
    fn merged_frames_decode_like_the_frames_they_replace() {
        let pictures = [
            frame(16, 4, |_| cell(1, 0)),
            frame(16, 4, |i| cell(if i % 3 == 0 { 4 } else { 1 }, 0)),
            frame(16, 4, |i| cell(if i % 5 == 0 { 6 } else { 1 }, 200)),
        ];
        let mut encoder = Encoder::default();
        let [full, first, second] = pictures.each_ref().map(|p| encoder.encode(p));
        let mut expected = Decoder::default();
        for (seq, bytes) in [&full, &first, &second].into_iter().enumerate() {
            expected.decode(seq as u32, bytes).unwrap();
        }
        let expected = expected.frame();

        let updates = merge(&first, &second).unwrap();
        assert_eq!(updates[1], UPDATE);
        let mut decoder = Decoder::default();
        decoder.decode(0, &full).unwrap();
        assert_eq!(decoder.decode(2, &updates).unwrap(), expected);

        let refreshed = merge(&full, &merge(&first, &second).unwrap()).unwrap();
        assert_eq!(refreshed[1], FULL);
        assert_eq!(Decoder::default().decode(2, &refreshed).unwrap(), expected);

        assert_eq!(merge(&first, &full).unwrap(), full);
    }

    #[test]
    fn merged_and_reordered_frames_end_on_the_same_picture() {
        let mut encoder = Encoder::default();
        let frames: Vec<(u32, Vec<u8>)> = (0..40u32)
            .map(|t| {
                if t == 17 {
                    encoder.request_full();
                }
                let picture = frame(16, 8, |i| {
                    cell(
                        ((i as u32 * 7 + t * t) % 14) as u8,
                        (i as u32 * t % 256) as u8,
                    )
                });
                (t, encoder.encode(&picture))
            })
            .collect();
        let mut in_order = Decoder::default();
        for (seq, bytes) in &frames {
            in_order.decode(*seq, bytes).unwrap();
        }

        let mut sent = Vec::new();
        let mut waiting: Option<(u32, Vec<u8>)> = None;
        for (seq, bytes) in frames {
            waiting = Some(match waiting.take() {
                Some((_, earlier)) if seq % 4 != 0 => (seq, merge(&earlier, &bytes).unwrap()),
                earlier => {
                    sent.extend(earlier);
                    (seq, bytes)
                }
            });
        }
        sent.extend(waiting);
        for pair in sent.chunks_mut(2) {
            pair.reverse();
        }
        let mut decoder = Decoder::default();
        for (seq, bytes) in &sent {
            decoder.decode(*seq, bytes).unwrap();
        }
        assert_eq!(decoder.frame(), in_order.frame());
    }

    #[test]
    fn rejects_garbage() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.decode(1, &[]), Err(DecodeError::Truncated));
        assert_eq!(
            decoder.decode(1, &[9, 0, 1, 0, 1, 0]),
            Err(DecodeError::Version(9))
        );
        assert_eq!(
            decoder.decode(1, &[VERSION, 7, 1, 0, 1, 0]),
            Err(DecodeError::Kind(7))
        );
        assert_eq!(
            decoder.decode(1, &[VERSION, FULL, 1, 0, 1, 0, 1, 2, 3]),
            Err(DecodeError::Corrupt)
        );
        assert_eq!(
            decoder.decode(1, &[VERSION, FULL, 0xff, 0xff, 0xff, 0xff]),
            Err(DecodeError::TooLarge(0xffff, 0xffff))
        );
        let mut bogus = vec![VERSION, UPDATE, 8, 0, 1, 0];
        bogus.extend(zstd::bulk::compress(&[0b11, 1, 2], 3).unwrap());
        assert_eq!(decoder.decode(1, &bogus), Err(DecodeError::Corrupt));
    }
}
