use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use facetty_ascii::reel::{self, Reel};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use crate::ui;

/// Rendered by `splash/phosphor.py` and baked by `just splash`.
const REEL: &[u8] = include_bytes!("../assets/splash.reel");
const FRAME_TIME: Duration = Duration::from_nanos(1_000_000_000 / 24);
const HINT: &str = "enter join · q quit";

#[derive(PartialEq, Eq)]
enum Key {
    Enter,
    Quit,
    Other,
}

/// Loops the splash until Enter, then zooms into the screen while `connect`
/// runs, showing `waiting` if it outlasts the zoom. Returns `None` if the
/// user quits instead.
pub async fn play<T>(
    terminal: &mut DefaultTerminal,
    connect: impl Future<Output = Result<T>>,
    waiting: &str,
) -> Result<Option<T>> {
    let mut player = Player::new();
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(FRAME_TIME);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            key = next_key(&mut input) => match key? {
                Key::Enter => break,
                Key::Quit => return Ok(None),
                Key::Other => {}
            },
            _ = tick.tick() => {
                terminal.draw(|f| player.draw(f.area(), f.buffer_mut()))?;
                player.advance();
            }
        }
    }

    player.exit();
    let mut connect = std::pin::pin!(connect);
    let mut connected: Option<Result<T>> = None;
    loop {
        if player.done()
            && let Some(result) = connected
        {
            return result.map(Some);
        }
        tokio::select! {
            result = &mut connect, if connected.is_none() => connected = Some(result),
            key = next_key(&mut input) => if key? == Key::Quit {
                return Ok(None);
            },
            _ = tick.tick() => {
                terminal.draw(|f| {
                    let area = f.area();
                    player.draw(area, f.buffer_mut());
                    if player.done() {
                        centered(f.buffer_mut(), area, area.y + area.height / 2, waiting);
                    }
                })?;
                player.advance();
            }
        }
    }
}

async fn next_key(input: &mut EventStream) -> Result<Key> {
    loop {
        match input.next().await {
            Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                return Ok(classify(key));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(Key::Quit),
        }
    }
}

fn classify(key: KeyEvent) -> Key {
    match key.code {
        KeyCode::Enter => Key::Enter,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::Quit,
        KeyCode::Char('q') | KeyCode::Esc => Key::Quit,
        _ => Key::Other,
    }
}

struct Player {
    reel: Option<Reel>,
    fitted: Option<(u16, u16)>,
    frame: usize,
    exiting: bool,
    truecolor: bool,
}

impl Player {
    fn new() -> Self {
        Self {
            reel: None,
            fitted: None,
            frame: 0,
            exiting: false,
            truecolor: ui::truecolor(),
        }
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer) {
        let stage = Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        if self.fitted != Some((stage.width, stage.height)) {
            self.fitted = Some((stage.width, stage.height));
            self.reel = reel::decode_fitting(REEL, stage.width, stage.height)
                .ok()
                .flatten();
        }
        if let Some(reel) = &self.reel
            && let Some(frame) = reel.frames.get(self.frame.min(reel.frames.len() - 1))
        {
            let x0 = stage.x + (stage.width - frame.cols) / 2;
            let y0 = stage.y + (stage.height - frame.rows) / 2;
            for row in 0..frame.rows {
                for col in 0..frame.cols {
                    let cell = frame.get(col, row);
                    if cell.glyph == 0 {
                        continue;
                    }
                    buf[(x0 + col, y0 + row)]
                        .set_char(cell.char())
                        .set_fg(ui::color(cell.rgb, self.truecolor));
                }
            }
        }
        if !self.exiting {
            centered(buf, area, area.bottom().saturating_sub(1), HINT);
        }
    }

    fn advance(&mut self) {
        let Some(reel) = &self.reel else { return };
        self.frame += 1;
        if !self.exiting && self.frame >= reel.exit_start {
            self.frame = reel.loop_start;
        }
    }

    fn exit(&mut self) {
        if let Some(reel) = &self.reel {
            self.frame = reel.exit_start;
        }
        self.exiting = true;
    }

    fn done(&self) -> bool {
        self.exiting
            && self
                .reel
                .as_ref()
                .is_none_or(|r| self.frame >= r.frames.len())
    }
}

fn centered(buf: &mut Buffer, area: Rect, y: u16, text: &str) {
    let width = text.chars().count() as u16;
    if width > area.width || y >= area.bottom() {
        return;
    }
    let x = area.x + (area.width - width) / 2;
    buf.set_string(x, y, text, Style::new().fg(Color::DarkGray));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baked_splash_fits_common_terminals() {
        for (cols, rows) in [(80, 23), (120, 35), (200, 50)] {
            let reel = reel::decode_fitting(REEL, cols, rows)
                .unwrap()
                .expect("a size fits");
            let (c, r) = reel.size();
            assert!(c <= cols && r <= rows);
            assert!(reel.loop_start < reel.exit_start && reel.exit_start < reel.frames.len());
        }
    }

    #[test]
    fn the_exit_ends_on_a_blank_screen() {
        let reel = reel::decode_fitting(REEL, u16::MAX, u16::MAX)
            .unwrap()
            .unwrap();
        let last = reel.frames.last().unwrap();
        assert!(last.cells.iter().all(|c| c.glyph == 0));
    }
}
