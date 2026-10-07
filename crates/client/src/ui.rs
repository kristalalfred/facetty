use bits_ascii::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget, Wrap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    Vivid,
    Natural,
    Mono,
    Matrix,
}

impl Palette {
    pub fn next(self) -> Self {
        match self {
            Palette::Vivid => Palette::Natural,
            Palette::Natural => Palette::Mono,
            Palette::Mono => Palette::Matrix,
            Palette::Matrix => Palette::Vivid,
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

pub struct ChatLine {
    pub name: Option<String>,
    pub text: String,
}

pub fn draw_chat(buf: &mut Buffer, area: Rect, lines: &[ChatLine], input: Option<&str>) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(" chat ");
    let inner = block.inner(area);
    block.render(area, buf);

    let mut text: Vec<Line> = lines
        .iter()
        .map(|l| match &l.name {
            Some(name) => Line::from(vec![
                Span::styled(format!("{name}: "), Style::new().fg(Color::Cyan)),
                Span::raw(l.text.clone()),
            ]),
            None => Line::styled(l.text.clone(), Style::new().fg(Color::DarkGray)),
        })
        .collect();
    let input_rows = if input.is_some() { 1 } else { 0 };
    let log_area = Rect {
        height: inner.height.saturating_sub(input_rows),
        ..inner
    };
    let paragraph = Paragraph::new(std::mem::take(&mut text)).wrap(Wrap { trim: false });
    let total = paragraph.line_count(log_area.width) as u16;
    paragraph
        .scroll((total.saturating_sub(log_area.height), 0))
        .render(log_area, buf);

    if let Some(input) = input {
        let width = inner.width.saturating_sub(3) as usize;
        let visible: String = input
            .chars()
            .rev()
            .take(width)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::new().fg(Color::Yellow)),
            Span::raw(visible),
            Span::styled("_", Style::new().add_modifier(Modifier::SLOW_BLINK)),
        ]))
        .render(
            Rect {
                y: inner.bottom().saturating_sub(1),
                height: 1,
                ..inner
            },
            buf,
        );
    }
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
