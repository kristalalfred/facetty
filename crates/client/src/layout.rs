use facetty_proto::ladder;
use ratatui::layout::Rect;

/// Largest 16:9 grid of cells (32:9 cols:rows) that fits.
pub fn fit(width: u16, height: u16) -> (u16, u16) {
    let mut cols = width.min((height as u32 * 32 / 9) as u16);
    while cols > 0 && ladder::rows_for(cols) > height {
        cols -= 1;
    }
    (cols, ladder::rows_for(cols))
}

/// Tiles with a one-cell border around 16:9 content, arranged to make each
/// as large as possible and centered in `area`.
pub fn grid(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let n16 = n as u16;
    let (cols, content) = (1..=n16)
        .map(|cols| {
            let rows = n16.div_ceil(cols);
            let inner_w = (area.width / cols).saturating_sub(2);
            let inner_h = (area.height / rows).saturating_sub(2);
            (cols, fit(inner_w, inner_h))
        })
        .max_by_key(|&(cols, (w, _))| (w, std::cmp::Reverse(cols)))
        .expect("at least one candidate");
    let rows = n16.div_ceil(cols);
    let tile = (content.0 + 2, content.1 + 2);
    place(area, n, cols, rows, tile)
}

/// One big tile on top, the rest in a strip along the bottom.
pub fn speaker(area: Rect, n: usize) -> Vec<Rect> {
    if n <= 1 {
        return grid(area, n);
    }
    let strip_h = (area.height / 4).max(6).min(area.height / 2);
    let main = Rect {
        height: area.height - strip_h,
        ..area
    };
    let strip = Rect {
        y: area.y + main.height,
        height: strip_h,
        ..area
    };
    let others = (n - 1) as u16;
    let inner_w = (strip.width / others).saturating_sub(2);
    let content = fit(inner_w, strip_h.saturating_sub(2));
    let mut tiles = grid(main, 1);
    tiles.extend(place(
        strip,
        n - 1,
        others,
        1,
        (content.0 + 2, content.1 + 2),
    ));
    tiles
}

fn place(area: Rect, n: usize, cols: u16, rows: u16, tile: (u16, u16)) -> Vec<Rect> {
    let top = area.y + area.height.saturating_sub(rows * tile.1) / 2;
    (0..n as u16)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = (n as u16 - row * cols).min(cols);
            let left = area.x + area.width.saturating_sub(in_row * tile.0) / 2;
            Rect::new(left + col * tile.0, top + row * tile.1, tile.0, tile.1)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect() {
        assert_eq!(fit(200, 45), (160, 45));
        assert_eq!(fit(64, 100), (64, 18));
        assert_eq!(fit(0, 10), (0, 0));
    }

    #[test]
    fn grid_tiles_stay_inside_and_do_not_overlap() {
        let area = Rect::new(0, 0, 180, 50);
        for n in 1..=9 {
            let tiles = grid(area, n);
            assert_eq!(tiles.len(), n);
            for (i, a) in tiles.iter().enumerate() {
                assert!(
                    area.contains(a.as_position())
                        && a.right() <= area.right()
                        && a.bottom() <= area.bottom()
                );
                for b in &tiles[i + 1..] {
                    assert!(!a.intersects(*b), "{n} tiles: {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn two_people_sit_side_by_side_on_a_wide_terminal() {
        let tiles = grid(Rect::new(0, 0, 200, 40), 2);
        assert_eq!(tiles[0].y, tiles[1].y);
    }

    #[test]
    fn speaker_view_puts_the_rest_below() {
        let tiles = speaker(Rect::new(0, 0, 160, 48), 4);
        assert_eq!(tiles.len(), 4);
        assert!(tiles[1..].iter().all(|t| t.y >= tiles[0].bottom()));
    }
}
