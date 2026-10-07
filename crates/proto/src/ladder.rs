//! Simulcast ladder. Every published video is 16:9; in terminal cells, which
//! are roughly twice as tall as they are wide, that is 32:9 cols:rows.

use serde::{Deserialize, Serialize};

pub type Rung = u8;

const STEP: u16 = 8;
const RUNGS: u16 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

pub fn rungs() -> impl Iterator<Item = Rung> {
    0..RUNGS as Rung
}

/// Rung `n` is `8 * (n + 2)` columns wide: 16 up to 264.
pub fn size(rung: Rung) -> Option<Size> {
    if rung as u16 >= RUNGS {
        return None;
    }
    let cols = STEP * (rung as u16 + 2);
    Some(Size {
        cols,
        rows: rows_for(cols),
    })
}

pub fn rows_for(cols: u16) -> u16 {
    ((cols as u32 * 9 + 16) / 32) as u16
}

/// Largest rung that fits inside `cols` x `rows` cells.
pub fn best_fit(cols: u16, rows: u16) -> Option<Rung> {
    rungs()
        .take_while(|&r| {
            let s = size(r).expect("rung in range");
            s.cols <= cols && s.rows <= rows
        })
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_follow_aspect() {
        assert_eq!(
            size(18),
            Some(Size {
                cols: 160,
                rows: 45
            })
        );
        assert_eq!(size(2), Some(Size { cols: 32, rows: 9 }));
        assert_eq!(size(RUNGS as Rung), None);
    }

    #[test]
    fn best_fit_respects_both_dimensions() {
        assert_eq!(
            best_fit(200, 45).and_then(size),
            Some(Size {
                cols: 160,
                rows: 45
            })
        );
        assert_eq!(
            best_fit(200, 20).and_then(size),
            Some(Size { cols: 72, rows: 20 })
        );
        assert_eq!(best_fit(10, 10), None);
    }
}
