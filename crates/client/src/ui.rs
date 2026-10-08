use facetty_ascii::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph, Widget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    Natural,
    Vivid,
    Mono,
    Matrix,
}

impl Palette {
    pub fn next(self) -> Self {
        match self {
            Palette::Natural => Palette::Vivid,
            Palette::Vivid => Palette::Mono,
            Palette::Mono => Palette::Matrix,
            Palette::Matrix => Palette::Natural,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Palette::Vivid => "vivid",
            Palette::Natural => "natural",
            Palette::Mono => "mono",
            Palette::Matrix => "matrix",
        }
    }

    fn rgb(self, [r, g, b]: [u8; 3]) -> [u8; 3] {
        match self {
            Palette::Natural => [r, g, b],
            Palette::Mono => [230, 230, 230],
            Palette::Matrix => [40, 255, 90],
            Palette::Vivid => {
                let max = r.max(g).max(b);
                if max < 12 {
                    return [90, 90, 90];
                }
                let scale = |v: u8| (v as u32 * 255 / max as u32) as u8;
                [scale(r), scale(g), scale(b)]
            }
        }
    }
}

pub fn color(rgb: [u8; 3], truecolor: bool) -> Color {
    let [r, g, b] = rgb;
    if truecolor {
        return Color::Rgb(r, g, b);
    }
    let level = |v: u8| ((v as u16 * 5 + 127) / 255) as u8;
    Color::Indexed(16 + 36 * level(r) + 6 * level(g) + level(b))
}

pub struct Tile<'a> {
    pub name: &'a str,
    pub frame: Option<&'a Frame>,
    pub mirror: bool,
    pub muted: bool,
    pub speaking: bool,
    pub placeholder: &'a str,
    pub palette: Palette,
    pub truecolor: bool,
}

impl Widget for Tile<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border = if self.speaking {
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border)
            .title(Span::styled(
                format!(" {} ", self.name),
                Style::new().fg(Color::White),
            ));
        if self.muted {
            block = block.title_bottom(
                Line::from(Span::styled(" muted ", Style::new().fg(Color::Red))).right_aligned(),
            );
        }
        let inner = block.inner(area);
        block.render(area, buf);
        match self.frame {
            Some(frame) => draw_frame(buf, inner, frame, self.mirror, self.palette, self.truecolor),
            None => {
                let y = inner.y + inner.height / 2;
                Paragraph::new(self.placeholder)
                    .style(Style::new().fg(Color::DarkGray))
                    .alignment(Alignment::Center)
                    .render(
                        Rect {
                            y,
                            height: 1,
                            ..inner
                        },
                        buf,
                    );
            }
        }
    }
}

fn draw_frame(
    buf: &mut Buffer,
    area: Rect,
    frame: &Frame,
    mirror: bool,
    palette: Palette,
    truecolor: bool,
) {
    let cols = frame.cols.min(area.width);
    let rows = frame.rows.min(area.height);
    let (x0, y0) = (
        area.x + (area.width - cols) / 2,
        area.y + (area.height - rows) / 2,
    );
    let (skip_c, skip_r) = ((frame.cols - cols) / 2, (frame.rows - rows) / 2);
    for r in 0..rows {
        for c in 0..cols {
            let src = if mirror {
                frame.cols - 1 - (skip_c + c)
            } else {
                skip_c + c
            };
            let cell = frame.get(src, skip_r + r);
            let ch = match (cell.char(), mirror) {
                (' ', _) => continue,
                ('/', true) => '\\',
                ('\\', true) => '/',
                (ch, _) => ch,
            };
            buf[(x0 + c, y0 + r)]
                .set_char(ch)
                .set_fg(color(palette.rgb(cell.rgb), truecolor));
        }
    }
}

/// `progress` runs from 0 at the bottom of `area` to 1 at the top. `lane`
/// picks the column; consecutive lanes land far apart.
pub fn draw_reaction(buf: &mut Buffer, area: Rect, emoji: &str, progress: f32, lane: u16) {
    if area.width < 2 || area.height == 0 {
        return;
    }
    let rise = (progress.clamp(0.0, 1.0) * (area.height - 1) as f32).round() as u16;
    let columns = (area.width - 1) as f32;
    let x = area.x + ((lane as f32 * 0.618_034).fract() * columns) as u16;
    buf.set_stringn(x, area.bottom() - 1 - rise, emoji, 2, Style::new());
}

pub fn draw_toasts(buf: &mut Buffer, area: Rect, toasts: &[&str]) {
    if toasts.is_empty() || area.width < 8 || area.height < 4 {
        return;
    }
    let lines: Vec<Line> = toasts.iter().map(|t| Line::raw(*t)).collect();
    let widest = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (widest + 4).min(area.width - 2);
    let height = (lines.len() as u16 + 2).min(area.height - 1);
    let rect = Rect {
        x: area.right() - width - 1,
        y: area.y + 1,
        width,
        height,
    };
    Clear.render(rect, buf);
    Paragraph::new(lines)
        .style(Style::new().fg(Color::White))
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(Color::Cyan))
                .padding(Padding::horizontal(1)),
        )
        .render(rect, buf);
}

pub fn draw_status(buf: &mut Buffer, area: Rect, left: &str, hints: &[(&str, String, bool)]) {
    let mut spans = vec![Span::styled(
        format!(" {left} "),
        Style::new().fg(Color::Black).bg(Color::Cyan),
    )];
    for (key, label, alert) in hints {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(*key, Style::new().fg(Color::Yellow)));
        let style = if *alert {
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {label}"), style));
    }
    Paragraph::new(Line::from(spans)).render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(buf: &Buffer, emoji: &str) -> Option<(u16, u16)> {
        buf.area
            .positions()
            .find(|p| buf[(p.x, p.y)].symbol() == emoji)
            .map(|p| (p.x, p.y))
    }

    #[test]
    fn reactions_rise_from_the_bottom_and_stay_inside() {
        let area = Rect::new(3, 2, 10, 5);
        for (progress, row) in [(0.0, 6), (0.5, 4), (1.0, 2), (1.5, 2)] {
            for lane in 0..20 {
                let mut buf = Buffer::empty(Rect::new(0, 0, 16, 9));
                draw_reaction(&mut buf, area, "🎉", progress, lane);
                let (x, y) = find(&buf, "🎉").expect("drawn");
                assert_eq!(y, row);
                assert!(x >= area.x && x + 2 <= area.right(), "lane {lane} at {x}");
            }
        }
    }

    #[test]
    fn toasts_sit_in_the_top_right_corner() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        draw_toasts(
            &mut buf,
            Rect::new(0, 0, 30, 10),
            &["exposure 1.2", "ann joined"],
        );
        let row = |y: u16| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert_eq!(row(0).trim(), "");
        assert_eq!(
            row(2),
            format!("{}│ exposure 1.2 │{}", " ".repeat(13), " ".repeat(11))
        );
        assert_eq!(row(3).trim(), "│ ann joined   │");
        assert_eq!(row(5).trim(), "");
    }

    #[test]
    fn reactions_skip_tiles_too_small_for_them() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 4));
        draw_reaction(&mut buf, Rect::new(0, 0, 1, 4), "🎉", 0.0, 0);
        assert_eq!(find(&buf, "🎉"), None);
    }
}
