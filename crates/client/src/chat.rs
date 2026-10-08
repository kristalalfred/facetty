//! The chat pane. It wraps lines itself instead of using `Paragraph`, so a
//! mouse position can be mapped back to the text under it for selection and
//! links.

use std::ops::Range;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
use unicode_segmentation::UnicodeSegmentation;

const WHEEL_ROWS: isize = 3;

pub enum Input<'a> {
    Off,
    Idle,
    Typing(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Copy(String),
    Open(String),
    Type,
}

#[derive(Default)]
pub struct Chat {
    lines: Vec<Shaped>,
    links: Vec<String>,
    rows: Vec<Row>,
    width: u16,
    pinned_top: Option<usize>,
    shown_top: usize,
    area: Rect,
    log: Rect,
    input: Rect,
    selection: Option<(Point, Point)>,
    press: Option<Press>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    line: usize,
    glyph: usize,
}

struct Press {
    at: Point,
    dragged: bool,
}

struct Hit {
    point: Point,
    exact: bool,
}

#[derive(Default)]
struct Shaped {
    text: String,
    glyphs: Vec<Glyph>,
}

struct Glyph {
    bytes: Range<usize>,
    width: u16,
    style: Style,
    link: Option<usize>,
}

struct Row {
    line: usize,
    glyphs: Range<usize>,
}

impl Chat {
    pub fn push(&mut self, name: Option<&str>, text: &str) {
        let mut shaped = Shaped::default();
        match name {
            Some(name) => {
                shaped.push(&format!("{name}: "), Style::new().fg(Color::Cyan), None);
                shaped.push_text(text, Style::new(), &mut self.links);
            }
            None => shaped.push_text(text, Style::new().fg(Color::DarkGray), &mut self.links),
        }
        let line = self.lines.len();
        self.rows.extend(
            wrap(&shaped, self.width)
                .into_iter()
                .map(|glyphs| Row { line, glyphs }),
        );
        self.lines.push(shaped);
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, input: Input) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(" chat ");
        let inner = block.inner(area);
        let input_rows = if matches!(input, Input::Off) { 0 } else { 1 };
        self.area = area;
        self.log = Rect {
            height: inner.height.saturating_sub(input_rows),
            ..inner
        };
        self.input = Rect {
            y: self.log.bottom(),
            height: inner.height - self.log.height,
            ..inner
        };
        if self.width != self.log.width {
            self.rewrap(self.log.width);
        }
        let max_top = self.max_top();
        if self.pinned_top.is_some_and(|top| top >= max_top) {
            self.pinned_top = None;
        }
        self.shown_top = self.pinned_top.unwrap_or(max_top);

        let block = if self.pinned_top.is_some() {
            block.title_bottom(Line::from(" ↓ more ").right_aligned())
        } else {
            block
        };
        block.render(area, buf);
        for (y, row) in (self.log.top()..self.log.bottom()).zip(&self.rows[self.shown_top..]) {
            self.draw_row(row, y, buf);
        }

        let prompt = Span::styled("> ", Style::new().fg(Color::Yellow));
        let line = match input {
            Input::Off => return,
            Input::Idle => Line::from(vec![
                prompt.style(Style::new().fg(Color::DarkGray)),
                Span::styled("Enter to type", Style::new().fg(Color::DarkGray)),
            ]),
            Input::Typing(text) => {
                let width = self.input.width.saturating_sub(3) as usize;
                let skip = text.chars().count().saturating_sub(width);
                Line::from(vec![
                    prompt,
                    Span::raw(text.chars().skip(skip).collect::<String>()),
                    Span::styled("_", Style::new().add_modifier(Modifier::SLOW_BLINK)),
                ])
            }
        };
        Paragraph::new(line).render(self.input, buf);
    }

    pub fn hide(&mut self) {
        self.area = Rect::default();
        self.log = Rect::default();
        self.input = Rect::default();
        self.press = None;
    }

    pub fn scroll_page(&mut self, up: bool) {
        let page = self.log.height.saturating_sub(1).max(1) as isize;
        self.scroll(if up { -page } else { page });
    }

    pub fn on_mouse(&mut self, event: MouseEvent) -> Option<Action> {
        let at = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = None;
                if self.input.contains(at) {
                    return Some(Action::Type);
                }
                self.press = self
                    .log
                    .contains(at)
                    .then(|| self.hit(at))
                    .flatten()
                    .map(|hit| Press {
                        at: hit.point,
                        dragged: false,
                    });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let head = self.hit(at).map(|hit| hit.point);
                if let (Some(press), Some(head)) = (&mut self.press, head) {
                    press.dragged = true;
                    self.selection = Some((press.at, head));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let press = self.press.take()?;
                if press.dragged {
                    return self.selected_text().map(Action::Copy);
                }
                return self.link_at(at).map(|url| Action::Open(url.to_string()));
            }
            MouseEventKind::ScrollUp if self.area.contains(at) => self.scroll(-WHEEL_ROWS),
            MouseEventKind::ScrollDown if self.area.contains(at) => self.scroll(WHEEL_ROWS),
            _ => {}
        }
        None
    }

    fn max_top(&self) -> usize {
        self.rows.len().saturating_sub(self.log.height as usize)
    }

    fn scroll(&mut self, rows: isize) {
        let max_top = self.max_top();
        let top = self
            .pinned_top
            .unwrap_or(max_top)
            .saturating_add_signed(rows);
        self.pinned_top = (top < max_top).then_some(top);
    }

    fn rewrap(&mut self, width: u16) {
        self.width = width;
        self.pinned_top = None;
        self.rows = self
            .lines
            .iter()
            .enumerate()
            .flat_map(|(line, shaped)| {
                wrap(shaped, width)
                    .into_iter()
                    .map(move |glyphs| Row { line, glyphs })
            })
            .collect();
    }

    fn draw_row(&self, row: &Row, y: u16, buf: &mut Buffer) {
        let shaped = &self.lines[row.line];
        let mut x = self.log.x;
        for i in row.glyphs.clone() {
            let glyph = &shaped.glyphs[i];
            if x + glyph.width > self.log.right() {
                break;
            }
            let mut style = glyph.style;
            if self.is_selected(Point {
                line: row.line,
                glyph: i,
            }) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            buf.set_stringn(
                x,
                y,
                &shaped.text[glyph.bytes.clone()],
                glyph.width as usize,
                style,
            );
            x += glyph.width;
        }
    }

    fn hit(&self, at: Position) -> Option<Hit> {
        let visible = self
            .rows
            .len()
            .saturating_sub(self.shown_top)
            .min(self.log.height as usize);
        if visible == 0 {
            return None;
        }
        let (index, x, on_row) = if at.y < self.log.top() {
            (0, 0, false)
        } else if at.y >= self.log.top() + visible as u16 {
            (visible - 1, u16::MAX, false)
        } else {
            ((at.y - self.log.top()) as usize, at.x, true)
        };
        let row = &self.rows[self.shown_top + index];
        let glyphs = &self.lines[row.line].glyphs;
        let mut left = self.log.x;
        let mut glyph = row.glyphs.start;
        let mut exact = false;
        for i in row.glyphs.clone() {
            glyph = i;
            let right = left + glyphs[i].width;
            if x < right {
                exact = on_row && x >= left;
                break;
            }
            left = right;
        }
        Some(Hit {
            point: Point {
                line: row.line,
                glyph,
            },
            exact,
        })
    }

    fn link_at(&self, at: Position) -> Option<&str> {
        let hit = self.hit(at).filter(|hit| hit.exact)?;
        let link = self.lines[hit.point.line]
            .glyphs
            .get(hit.point.glyph)?
            .link?;
        Some(&self.links[link])
    }

    fn is_selected(&self, point: Point) -> bool {
        self.selection
            .is_some_and(|(a, b)| a.min(b) <= point && point <= a.max(b))
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (from, to) = (a.min(b), a.max(b));
        let mut text = String::new();
        for line in from.line..=to.line {
            if line > from.line {
                text.push('\n');
            }
            let shaped = &self.lines[line];
            let first = if line == from.line { from.glyph } else { 0 };
            let last = if line == to.line {
                to.glyph
            } else {
                usize::MAX
            };
            let last = last.min(shaped.glyphs.len().saturating_sub(1));
            if let (Some(first), Some(last)) = (shaped.glyphs.get(first), shaped.glyphs.get(last)) {
                text.push_str(&shaped.text[first.bytes.start..last.bytes.end]);
            }
        }
        Some(text)
    }
}

impl Shaped {
    fn push(&mut self, text: &str, style: Style, link: Option<usize>) {
        for grapheme in text.graphemes(true) {
            let start = self.text.len();
            if grapheme.contains(char::is_control) {
                self.text.push(' ');
            } else {
                self.text.push_str(grapheme);
            }
            let bytes = start..self.text.len();
            self.glyphs.push(Glyph {
                width: self.text[bytes.clone()].cell_width(),
                bytes,
                style,
                link,
            });
        }
    }

    fn push_text(&mut self, text: &str, style: Style, links: &mut Vec<String>) {
        let link_style = style
            .fg(Color::LightBlue)
            .add_modifier(Modifier::UNDERLINED);
        let mut done = 0;
        for url in find_links(text) {
            self.push(&text[done..url.start], style, None);
            links.push(text[url.clone()].to_string());
            self.push(&text[url.clone()], link_style, Some(links.len() - 1));
            done = url.end;
        }
        self.push(&text[done..], style, None);
    }

    fn is_space(&self, glyph: usize) -> bool {
        &self.text[self.glyphs[glyph].bytes.clone()] == " "
    }
}

/// Spaces at a break stay at the end of the row before it, past the edge.
fn wrap(shaped: &Shaped, width: u16) -> Vec<Range<usize>> {
    let glyphs = &shaped.glyphs;
    let mut rows = Vec::new();
    let mut start = 0;
    while start < glyphs.len() {
        let mut end = start;
        let mut used = 0;
        while end < glyphs.len() && used + glyphs[end].width <= width {
            used += glyphs[end].width;
            end += 1;
        }
        let next = if end == glyphs.len() {
            end
        } else if shaped.is_space(end) {
            (end..glyphs.len())
                .find(|&i| !shaped.is_space(i))
                .unwrap_or(glyphs.len())
        } else {
            match (start + 1..end).rev().find(|&i| shaped.is_space(i)) {
                Some(space) => space + 1,
                None => end.max(start + 1),
            }
        };
        rows.push(start..next);
        start = next;
    }
    rows
}

fn find_links(text: &str) -> Vec<Range<usize>> {
    let mut links = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find("http") {
        let start = from + found;
        let word = text[start..]
            .split(char::is_whitespace)
            .next()
            .unwrap_or_default();
        from = start + word.len();
        let url = trim_url(word);
        let joined = text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        let has_host = ["https://", "http://"].iter().any(|scheme| {
            url.strip_prefix(scheme)
                .is_some_and(|rest| !rest.is_empty())
        });
        if has_host && !joined {
            links.push(start..start + url.len());
        }
    }
    links
}

fn trim_url(mut url: &str) -> &str {
    while let Some(last) = url.chars().next_back() {
        let unbalanced = |open: char| url.matches(last).count() > url.matches(open).count();
        let trailing = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '>' => true,
            ')' => unbalanced('('),
            ']' => unbalanced('['),
            _ => false,
        };
        if !trailing {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn mouse(chat: &mut Chat, kind: MouseEventKind, column: u16, row: u16) -> Option<Action> {
        chat.on_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn drag(chat: &mut Chat, from: (u16, u16), to: (u16, u16)) -> Option<Action> {
        let left = MouseButton::Left;
        mouse(chat, MouseEventKind::Down(left), from.0, from.1);
        mouse(chat, MouseEventKind::Drag(left), to.0, to.1);
        mouse(chat, MouseEventKind::Up(left), to.0, to.1)
    }

    fn click(chat: &mut Chat, x: u16, y: u16) -> Option<Action> {
        let down = mouse(chat, MouseEventKind::Down(MouseButton::Left), x, y);
        down.or(mouse(chat, MouseEventKind::Up(MouseButton::Left), x, y))
    }

    fn draw(chat: &mut Chat, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        chat.render(area, &mut buf, Input::Idle);
        (1..height - 2)
            .map(|y| {
                (1..width - 1)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn finds_links_and_leaves_out_trailing_punctuation() {
        let text = "see https://a.io/x_(y), (http://b.org/z). xhttp://c.io https:// end";
        let links: Vec<&str> = find_links(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(links, ["https://a.io/x_(y)", "http://b.org/z"]);
    }

    #[test]
    fn wraps_after_spaces() {
        let mut chat = Chat::default();
        chat.push(Some("ann"), "one two three four");
        assert_eq!(draw(&mut chat, 12, 6), ["ann: one", "two three", "four"]);
    }

    #[test]
    fn dragging_copies_across_wrapped_rows_and_lines() {
        let mut chat = Chat::default();
        chat.push(Some("ann"), "one two three four");
        chat.push(None, "bob joined");
        draw(&mut chat, 12, 8);
        let copied = drag(&mut chat, (6, 1), (9, 2));
        assert_eq!(copied, Some(Action::Copy("one two three".into())));
        let copied = drag(&mut chat, (3, 3), (3, 4));
        assert_eq!(copied, Some(Action::Copy("ur\nbob".into())));
    }

    #[test]
    fn dragging_backwards_and_past_the_end_selects_whole_rows() {
        let mut chat = Chat::default();
        chat.push(Some("ann"), "hi");
        draw(&mut chat, 20, 8);
        let copied = drag(&mut chat, (15, 4), (1, 1));
        assert_eq!(copied, Some(Action::Copy("ann: hi".into())));
    }

    #[test]
    fn clicking_a_link_opens_it() {
        let mut chat = Chat::default();
        chat.push(Some("bo"), "go https://a.io now");
        draw(&mut chat, 40, 5);
        assert_eq!(
            click(&mut chat, 9, 1),
            Some(Action::Open("https://a.io".into()))
        );
        assert_eq!(click(&mut chat, 22, 1), None);
        assert_eq!(click(&mut chat, 30, 1), None);
    }

    #[test]
    fn clicking_the_input_row_starts_typing() {
        let mut chat = Chat::default();
        draw(&mut chat, 30, 6);
        assert_eq!(click(&mut chat, 5, 4), Some(Action::Type));
    }

    #[test]
    fn scrolled_up_view_stays_put_when_lines_arrive() {
        let mut chat = Chat::default();
        for i in 0..6 {
            chat.push(None, &format!("line {i}"));
        }
        assert_eq!(draw(&mut chat, 20, 5), ["line 4", "line 5"]);
        mouse(&mut chat, MouseEventKind::ScrollUp, 3, 2);
        assert_eq!(draw(&mut chat, 20, 5), ["line 1", "line 2"]);
        chat.push(None, "line 6");
        assert_eq!(draw(&mut chat, 20, 5), ["line 1", "line 2"]);
        for _ in 0..4 {
            chat.scroll_page(false);
        }
        assert_eq!(draw(&mut chat, 20, 5), ["line 5", "line 6"]);
    }

    #[test]
    fn control_characters_become_spaces() {
        let mut chat = Chat::default();
        chat.push(Some("eve"), "a\x1b[2Jb");
        assert_eq!(draw(&mut chat, 20, 5), ["eve: a [2Jb", ""]);
    }
}
