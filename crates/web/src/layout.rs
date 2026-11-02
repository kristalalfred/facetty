use facetty_proto::ladder::{self, Rung};

/// Claude: CSS pixels across one cell. Cells are twice as tall, as in a
/// terminal.
pub const CELL_WIDTH: f64 = 8.0;
const MARGIN: f64 = 24.0;
const GAP: f64 = 16.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

pub fn grid(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let area_w = (area.w - 2.0 * MARGIN).max(0.0);
    let area_h = (area.h - 2.0 * MARGIN).max(0.0);
    let (cols, tile_w) = (1..=n)
        .map(|cols| {
            let rows = n.div_ceil(cols);
            let by_width = (area_w - GAP * (cols - 1) as f64) / cols as f64;
            let by_height = (area_h - GAP * (rows - 1) as f64) / rows as f64 * 16.0 / 9.0;
            (cols, by_width.min(by_height).max(0.0))
        })
        .fold((1, -1.0), |best, c| if c.1 > best.1 { c } else { best });
    let tile_h = tile_w * 9.0 / 16.0;
    let rows = n.div_ceil(cols);
    let top = area.y + (area.h - (rows as f64 * tile_h + GAP * (rows - 1) as f64)) / 2.0;
    (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = (n - row * cols).min(cols);
            let row_w = in_row as f64 * tile_w + GAP * (in_row - 1) as f64;
            Rect {
                x: area.x + (area.w - row_w) / 2.0 + col as f64 * (tile_w + GAP),
                y: top + row as f64 * (tile_h + GAP),
                w: tile_w,
                h: tile_h,
            }
        })
        .collect()
}

pub fn rung_for(tile: &Rect) -> Option<Rung> {
    let cols = (tile.w / CELL_WIDTH).floor().clamp(0.0, u16::MAX as f64) as u16;
    ladder::best_fit(cols, ladder::rows_for(cols)).or(Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: f64, h: f64) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }
    }

    fn overlaps(a: &Rect, b: &Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn tiles_fit_without_overlapping() {
        for (w, h) in [(1920.0, 1080.0), (390.0, 844.0), (1280.0, 400.0)] {
            for n in 1..=9 {
                let tiles = grid(area(w, h), n);
                assert_eq!(tiles.len(), n);
                for (i, a) in tiles.iter().enumerate() {
                    assert!(a.x >= 0.0 && a.y >= 0.0 && a.x + a.w <= w && a.y + a.h <= h);
                    assert!((a.w / a.h - 16.0 / 9.0).abs() < 1e-9);
                    for b in &tiles[i + 1..] {
                        assert!(!overlaps(a, b), "{n} tiles in {w}x{h}: {a:?} and {b:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn two_people_sit_side_by_side_in_a_wide_view() {
        let tiles = grid(area(1920.0, 1080.0), 2);
        assert_eq!(tiles[0].y, tiles[1].y);
        assert!(tiles[0].x < tiles[1].x);
    }

    #[test]
    fn a_phone_stacks_people() {
        let tiles = grid(area(390.0, 844.0), 2);
        assert_eq!(tiles[0].x, tiles[1].x);
    }

    #[test]
    fn rungs_follow_tile_width() {
        let wide = rung_for(&Rect {
            x: 0.0,
            y: 0.0,
            w: 1600.0,
            h: 900.0,
        })
        .unwrap();
        assert_eq!(ladder::size(wide).unwrap().cols, 200);
        assert_eq!(
            rung_for(&Rect {
                x: 0.0,
                y: 0.0,
                w: 40.0,
                h: 22.5
            }),
            Some(0)
        );
    }
}
