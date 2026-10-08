use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

const HINT: &str = " enter pick  esc close ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Camera(usize),
    Mic(Option<String>),
    Speaker(Option<String>),
}

impl Choice {
    pub fn kind(&self) -> &'static str {
        match self {
            Choice::Camera(_) => "camera",
            Choice::Mic(_) => "mic",
            Choice::Speaker(_) => "speaker",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Close,
    Pick(Choice, String),
}

enum Row {
    Heading(&'static str),
    Empty,
    Choice {
        choice: Choice,
        label: String,
        current: bool,
    },
}

impl Row {
    fn is_choice(&self) -> bool {
        matches!(self, Row::Choice { .. })
    }

    fn line(&self) -> Line<'_> {
        match self {
            Row::Heading(heading) => Line::styled(*heading, Style::new().fg(Color::Cyan)),
            Row::Empty => Line::styled("  none found", Style::new().fg(Color::DarkGray)),
            Row::Choice { label, current, .. } => Line::from(vec![
                Span::styled(
                    if *current { "● " } else { "  " },
                    Style::new().fg(Color::Green),
                ),
                Span::raw(label.as_str()),
            ]),
        }
    }
}

#[derive(Default)]
pub struct Picker {
    rows: Vec<Row>,
    cursor: usize,
    top: usize,
}

impl Picker {
    /// `current` indexes `choices`. The cursor starts on the first section's
    /// current choice.
    pub fn add(
        &mut self,
        heading: &'static str,
        choices: Vec<(Choice, String)>,
        current: Option<usize>,
    ) {
        self.rows.push(Row::Heading(heading));
        if choices.is_empty() {
            self.rows.push(Row::Empty);
        }
        let first = self.rows.len();
        let start = current.unwrap_or(0).min(choices.len().saturating_sub(1));
        let placed = self.rows.get(self.cursor).is_some_and(Row::is_choice);
        self.rows.extend(
            choices
                .into_iter()
                .enumerate()
                .map(|(i, (choice, label))| Row::Choice {
                    choice,
                    label,
                    current: current == Some(i),
                }),
        );
        if !placed && self.rows.get(first + start).is_some_and(Row::is_choice) {
            self.cursor = first + start;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Enter => {
                return match self.rows.get(self.cursor) {
                    Some(Row::Choice { choice, label, .. }) => {
                        Outcome::Pick(choice.clone(), label.clone())
                    }
                    _ => Outcome::Close,
                };
            }
            KeyCode::Esc | KeyCode::Char('d') | KeyCode::Char('q') => return Outcome::Close,
            _ => {}
        }
        Outcome::Stay
    }

    fn step(&mut self, down: bool) {
        let next = if down {
            (self.cursor + 1..self.rows.len()).find(|&i| self.rows[i].is_choice())
        } else {
            (0..self.cursor).rev().find(|&i| self.rows[i].is_choice())
        };
        if let Some(i) = next {
            self.cursor = i;
        }
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let widest = self.rows.iter().map(|r| r.line().width()).max();
        let width = (widest.unwrap_or(0).max(HINT.len()) as u16 + 4).min(area.width);
        let height = (self.rows.len() as u16 + 2).min(area.height);
        if width < 3 || height < 3 {
            return;
        }
        let rect = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::Cyan))
            .title(" devices ")
            .title_bottom(Line::styled(HINT, Style::new().fg(Color::DarkGray)).right_aligned());
        let inner = block.inner(rect);
        Clear.render(rect, buf);
        block.render(rect, buf);

        let visible = inner.height as usize;
        self.top = self
            .top
            .min(self.cursor.saturating_sub(1))
            .max((self.cursor + 1).saturating_sub(visible));
        for (y, (i, row)) in
            (inner.top()..inner.bottom()).zip(self.rows.iter().enumerate().skip(self.top))
        {
            let line_area = Rect {
                x: inner.x + 1,
                y,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            row.line().render(line_area, buf);
            if i == self.cursor {
                buf.set_style(
                    Rect {
                        y,
                        height: 1,
                        ..inner
                    },
                    Style::new().add_modifier(Modifier::REVERSED),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn press(picker: &mut Picker, code: KeyCode) -> Outcome {
        picker.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn picker() -> Picker {
        let mut picker = Picker::default();
        picker.add(
            "camera",
            vec![
                (Choice::Camera(0), "FaceTime".into()),
                (Choice::Camera(1), "Desk View".into()),
            ],
            Some(1),
        );
        picker.add(
            "microphone",
            vec![
                (Choice::Mic(None), "system default".into()),
                (Choice::Mic(Some("USB".into())), "USB".into()),
            ],
            Some(0),
        );
        picker
    }

    #[test]
    fn starts_on_the_current_camera_and_steps_over_headings() {
        let mut picker = picker();
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Outcome::Pick(Choice::Camera(1), "Desk View".into())
        );
        press(&mut picker, KeyCode::Down);
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Outcome::Pick(Choice::Mic(None), "system default".into())
        );
        for _ in 0..5 {
            press(&mut picker, KeyCode::Down);
        }
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Outcome::Pick(Choice::Mic(Some("USB".into())), "USB".into())
        );
        for _ in 0..5 {
            press(&mut picker, KeyCode::Up);
        }
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Outcome::Pick(Choice::Camera(0), "FaceTime".into())
        );
        assert_eq!(press(&mut picker, KeyCode::Esc), Outcome::Close);
    }

    #[test]
    fn skips_sections_with_nothing_to_pick() {
        let mut picker = Picker::default();
        picker.add("camera", Vec::new(), None);
        assert_eq!(press(&mut picker, KeyCode::Enter), Outcome::Close);
        picker.add(
            "speaker",
            vec![(Choice::Speaker(None), "default".into())],
            Some(0),
        );
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Outcome::Pick(Choice::Speaker(None), "default".into())
        );
    }

    #[test]
    fn keeps_the_cursor_in_view_when_the_list_is_taller_than_the_box() {
        let mut picker = Picker::default();
        let cameras = (0..20)
            .map(|i| (Choice::Camera(i), format!("cam {i}")))
            .collect();
        picker.add("camera", cameras, Some(19));
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 8));
        picker.render(buf.area, &mut buf);
        let shown: String = (0..8)
            .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        assert!(shown.contains("cam 19"));
        assert!(!shown.contains("cam 10"));
    }
}
