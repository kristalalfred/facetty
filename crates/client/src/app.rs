use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bits_ascii::Frame;
use bits_proto::ParticipantId;
use bits_proto::ladder::{self, Rung};
use bits_proto::signal::{self, Participant};
use crossterm::event::{
    Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent,
};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use tokio::sync::mpsc;

use crate::chat::{self, Chat};
use crate::desktop;
use crate::layout;
use crate::publisher::Publisher;
use crate::session::{Command, Event};
use crate::ui::{self, Palette, Tile};

const SPEAKING_LEVEL: f32 = 0.35;
const CHAT_WIDTH: u16 = 38;
const MIN_CHAT_WIDTH: u16 = 20;
const FLASH: Duration = Duration::from_secs(4);
const REACTION_TIME: Duration = Duration::from_secs(4);
const MAX_REACTIONS: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Grid,
    Speaker,
}

struct Reaction {
    emoji: String,
    at: Instant,
    lane: u16,
}

pub struct App {
    room: String,
    me: Participant,
    others: BTreeMap<ParticipantId, Participant>,
    frames: HashMap<ParticipantId, (u32, Arc<Frame>)>,
    subscriptions: HashMap<ParticipantId, Option<Rung>>,
    last_spoke: HashMap<ParticipantId, Instant>,
    reactions: HashMap<ParticipantId, Vec<Reaction>>,
    next_lane: u16,
    picking: bool,
    chat: Chat,
    chat_open: bool,
    input: Option<String>,
    view: View,
    palette: Palette,
    truecolor: bool,
    flash: Option<(String, Instant)>,
    publisher: Publisher,
    audio: Option<Arc<bits_audio::Engine>>,
    commands: Option<mpsc::UnboundedSender<Command>>,
    mirror_self: bool,
    closed: Option<String>,
    quit: bool,
}

pub struct Setup {
    pub room: String,
    pub me: Participant,
    pub others: Vec<Participant>,
    pub publisher: Publisher,
    pub audio: Option<Arc<bits_audio::Engine>>,
    pub commands: Option<mpsc::UnboundedSender<Command>>,
    pub notice: Option<String>,
    pub mirror_self: bool,
}

impl App {
    pub fn new(setup: Setup) -> Self {
        let truecolor = std::env::var("COLORTERM")
            .map(|v| v.contains("truecolor") || v.contains("24bit"))
            .unwrap_or(false);
        let mut me = setup.me;
        me.audio_muted |= setup.audio.is_none();
        let mut app = Self {
            room: setup.room,
            others: setup.others.into_iter().map(|p| (p.id, p)).collect(),
            frames: HashMap::new(),
            subscriptions: HashMap::new(),
            last_spoke: HashMap::new(),
            reactions: HashMap::new(),
            next_lane: 0,
            picking: false,
            chat: Chat::default(),
            chat_open: false,
            input: None,
            view: View::Grid,
            palette: Palette::Vivid,
            truecolor,
            flash: None,
            publisher: setup.publisher,
            audio: setup.audio,
            commands: setup.commands,
            mirror_self: setup.mirror_self,
            closed: None,
            quit: false,
            me,
        };
        if let Some(notice) = setup.notice {
            app.notify(notice);
        }
        app.send_state();
        app
    }

    pub async fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        mut events: Option<mpsc::UnboundedReceiver<Event>>,
    ) -> Result<Option<String>> {
        let mut input = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !self.quit {
            tokio::select! {
                term = input.next() => match term {
                    Some(Ok(TermEvent::Key(key))) if key.kind != KeyEventKind::Release => self.on_key(key),
                    Some(Ok(TermEvent::Mouse(mouse))) => self.on_mouse(mouse),
                    Some(Ok(TermEvent::Paste(text))) => self.on_paste(&text),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                },
                event = recv(&mut events) => match event {
                    Some(event) => self.on_event(event),
                    None => events = None,
                },
                _ = tick.tick() => {
                    terminal.draw(|f| self.draw(f.area(), f.buffer_mut()))?;
                }
            }
        }
        Ok(self.closed)
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Joined(p) => {
                self.notify(format!("{} joined", p.name));
                self.others.insert(p.id, p);
            }
            Event::Left(id) => {
                if let Some(p) = self.others.remove(&id) {
                    self.notify(format!("{} left", p.name));
                }
                self.frames.remove(&id);
                self.subscriptions.remove(&id);
                self.last_spoke.remove(&id);
                self.reactions.remove(&id);
                if let Some(audio) = &self.audio {
                    audio.remove(id);
                }
            }
            Event::Updated(p) if p.id == self.me.id => {}
            Event::Updated(p) => {
                if p.video_off {
                    self.frames.remove(&p.id);
                }
                self.others.insert(p.id, p);
            }
            Event::Chat { name, text } => {
                if !self.chat_open {
                    self.flash(format!("{name}: {text}"));
                }
                self.chat.push(Some(&name), &text);
            }
            Event::Reaction { from, emoji } => {
                let floating = self.reactions.entry(from).or_default();
                floating.retain(|r| r.at.elapsed() < REACTION_TIME);
                if floating.len() < MAX_REACTIONS {
                    floating.push(Reaction {
                        emoji,
                        at: Instant::now(),
                        lane: self.next_lane,
                    });
                    self.next_lane = self.next_lane.wrapping_add(1);
                }
            }
            Event::Video {
                publisher,
                seq,
                frame,
            } => {
                let newer = self
                    .frames
                    .get(&publisher)
                    .is_none_or(|(last, _)| (seq.wrapping_sub(*last) as i32) > 0);
                if newer && self.others.get(&publisher).is_some_and(|p| !p.video_off) {
                    self.frames.insert(publisher, (seq, frame));
                }
            }
            Event::EncodeRungs(rungs) => self.publisher.set_rungs(rungs),
            Event::Closed(reason) => {
                self.closed = Some(reason);
                self.quit = true;
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let KeyCode::PageUp | KeyCode::PageDown = key.code {
            self.chat.scroll_page(key.code == KeyCode::PageUp);
            return;
        }
        if let Some(input) = &mut self.input {
            match key.code {
                KeyCode::Enter => {
                    let text = std::mem::take(input);
                    if text.trim().is_empty() {
                        self.input = None;
                    } else {
                        self.send(Command::Chat(text));
                    }
                }
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            }
            return;
        }
        let picking = std::mem::take(&mut self.picking);
        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(emoji) = reaction_for(c) {
                    self.send(Command::React(emoji.to_string()));
                }
            }
            _ if picking => {}
            KeyCode::Char('r') if self.commands.is_some() => self.picking = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('m') if self.audio.is_some() => {
                self.me.audio_muted = !self.me.audio_muted;
                if let Some(audio) = &self.audio {
                    audio.set_muted(self.me.audio_muted);
                }
                self.send_state();
            }
            KeyCode::Char('v') => {
                self.me.video_off = !self.me.video_off;
                self.publisher.set_enabled(!self.me.video_off);
                self.send_state();
            }
            KeyCode::Char('l') => {
                self.view = match self.view {
                    View::Grid => View::Speaker,
                    View::Speaker => View::Grid,
                }
            }
            KeyCode::Char('t') if self.chat_open => self.chat_open = false,
            KeyCode::Char('t') => {
                self.chat_open = true;
                self.start_typing();
            }
            KeyCode::Enter | KeyCode::Char('/') => self.start_typing(),
            KeyCode::Char('p') => {
                self.palette = self.palette.next();
                self.notify(format!("palette: {}", self.palette.name()));
            }
            KeyCode::Char('e') => {
                self.publisher.update_params(|p| p.edges = !p.edges);
                let on = self.publisher.params().edges;
                self.notify(format!("edges {}", if on { "on" } else { "off" }));
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                let step = if key.code == KeyCode::Char(']') {
                    0.1
                } else {
                    -0.1
                };
                self.publisher
                    .update_params(|p| p.exposure = (p.exposure + step).clamp(0.2, 4.0));
                self.notify(format!("exposure {:.1}", self.publisher.params().exposure));
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        match self.chat.on_mouse(mouse) {
            Some(chat::Action::Copy(text)) => match desktop::copy(&text) {
                Ok(()) => self.flash(format!("copied {} characters", text.chars().count())),
                Err(e) => self.flash(format!("copy failed: {e:#}")),
            },
            Some(chat::Action::Open(url)) => {
                if let Err(e) = desktop::open(&url) {
                    self.flash(format!("could not open link: {e:#}"));
                }
            }
            Some(chat::Action::Type) => self.start_typing(),
            None => {}
        }
    }

    fn on_paste(&mut self, text: &str) {
        self.start_typing();
        if let Some(input) = &mut self.input {
            input.extend(text.chars().map(|c| if c.is_control() { ' ' } else { c }));
        }
    }

    fn start_typing(&mut self) {
        if self.commands.is_some() {
            self.chat_open = true;
            self.input.get_or_insert_with(String::new);
        }
    }

    fn draw(&mut self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let now = Instant::now();
        if let Some(audio) = &self.audio {
            for id in self.others.keys() {
                if audio.level(*id) > SPEAKING_LEVEL {
                    self.last_spoke.insert(*id, now);
                }
            }
        }

        let status = Rect {
            y: area.bottom().saturating_sub(1),
            height: 1,
            ..area
        };
        let mut stage = Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        let chat_width = CHAT_WIDTH.min(stage.width / 2);
        if self.chat_open && chat_width >= MIN_CHAT_WIDTH {
            stage.width -= chat_width;
            let area = Rect {
                x: stage.right(),
                width: chat_width,
                ..stage
            };
            let input = match (&self.input, &self.commands) {
                (Some(text), _) => chat::Input::Typing(text),
                (None, Some(_)) => chat::Input::Idle,
                (None, None) => chat::Input::Off,
            };
            self.chat.render(area, buf, input);
        } else {
            self.chat.hide();
        }

        let order = self.tile_order();
        let rects = match self.view {
            View::Grid => layout::grid(stage, order.len()),
            View::Speaker => layout::speaker(stage, order.len()),
        };
        for (who, rect) in order.iter().zip(rects) {
            let inner = Rect {
                x: rect.x + 1,
                y: rect.y + 1,
                width: rect.width.saturating_sub(2),
                height: rect.height.saturating_sub(2),
            };
            match who {
                None => self.draw_self(rect, inner, buf),
                Some(id) => self.draw_remote(*id, rect, inner, buf),
            }
            self.draw_reactions(who.unwrap_or(self.me.id), inner, buf);
        }
        self.draw_status(status, buf);
    }

    /// `None` is the local participant.
    fn tile_order(&self) -> Vec<Option<ParticipantId>> {
        let mut remotes: Vec<ParticipantId> = self.others.keys().copied().collect();
        if self.view == View::Speaker
            && let Some(active) = self.active_speaker()
        {
            remotes.retain(|id| *id != active);
            let mut order = vec![Some(active), None];
            order.extend(remotes.into_iter().map(Some));
            return order;
        }
        let mut order = vec![None];
        order.extend(remotes.into_iter().map(Some));
        order
    }

    fn active_speaker(&self) -> Option<ParticipantId> {
        self.last_spoke
            .iter()
            .max_by_key(|(_, t)| **t)
            .map(|(id, _)| *id)
            .or_else(|| self.others.keys().next().copied())
    }

    fn draw_self(&mut self, rect: Rect, inner: Rect, buf: &mut ratatui::buffer::Buffer) {
        let (cols, rows) = layout::fit(inner.width, inner.height);
        self.publisher.set_self_view(Some((cols, rows)));
        let frame = self.publisher.self_frame();
        let speaking = !self.me.audio_muted
            && self
                .audio
                .as_ref()
                .is_some_and(|a| a.local_level() > SPEAKING_LEVEL);
        let placeholder = match self.publisher.error() {
            Some(e) => e,
            None if self.me.video_off => "camera off".to_string(),
            None => "starting camera...".to_string(),
        };
        let frame = frame.filter(|_| !self.me.video_off);
        Tile {
            name: &format!("{} (you)", self.me.name),
            frame: frame.as_deref(),
            mirror: self.mirror_self,
            muted: self.me.audio_muted,
            speaking,
            placeholder: &placeholder,
            palette: self.palette,
            truecolor: self.truecolor,
        }
        .render(rect, buf);
    }

    fn draw_remote(
        &mut self,
        id: ParticipantId,
        rect: Rect,
        inner: Rect,
        buf: &mut ratatui::buffer::Buffer,
    ) {
        let Some(p) = self.others.get(&id) else {
            return;
        };
        let want = if p.video_off {
            None
        } else {
            ladder::best_fit(inner.width, inner.height)
        };
        if self.subscriptions.get(&id) != Some(&want) {
            self.subscriptions.insert(id, want);
            self.send(Command::Subscribe {
                publisher: id,
                rung: want,
            });
        }
        let speaking = !p.audio_muted
            && self
                .last_spoke
                .get(&id)
                .is_some_and(|t| t.elapsed() < Duration::from_millis(400));
        let placeholder = if p.video_off {
            "camera off"
        } else {
            "waiting for video..."
        };
        Tile {
            name: &p.name,
            frame: self.frames.get(&id).map(|(_, f)| f.as_ref()),
            mirror: false,
            muted: p.audio_muted,
            speaking,
            placeholder,
            palette: self.palette,
            truecolor: self.truecolor,
        }
        .render(rect, buf);
    }

    fn draw_reactions(&mut self, id: ParticipantId, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let Some(floating) = self.reactions.get_mut(&id) else {
            return;
        };
        floating.retain(|r| r.at.elapsed() < REACTION_TIME);
        for r in floating.iter() {
            let progress = r.at.elapsed().as_secs_f32() / REACTION_TIME.as_secs_f32();
            ui::draw_reaction(buf, area, &r.emoji, progress, r.lane);
        }
    }

    fn draw_status(&mut self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        if let Some((_, at)) = &self.flash
            && at.elapsed() > FLASH
        {
            self.flash = None;
        }
        if self.picking {
            let hints: Vec<_> = signal::REACTIONS
                .iter()
                .enumerate()
                .map(|(i, emoji)| (&"123456789"[i..=i], emoji.to_string(), false))
                .collect();
            ui::draw_status(buf, area, &format!("{} | react", self.room), &hints);
            return;
        }
        let people = self.others.len() + 1;
        let left = match (&self.flash, &self.commands) {
            (Some((msg, _)), _) => format!("{} | {msg}", self.room),
            (None, Some(_)) => format!("{} | {people} in call", self.room),
            (None, None) => self.room.clone(),
        };
        let mut hints = Vec::new();
        if self.audio.is_some() {
            let label = if self.me.audio_muted {
                "muted"
            } else {
                "mic on"
            };
            hints.push(("m", label.to_string(), self.me.audio_muted));
        }
        let cam = if self.me.video_off {
            "cam off"
        } else {
            "cam on"
        };
        hints.push(("v", cam.to_string(), self.me.video_off));
        let view = match self.view {
            View::Grid => "grid",
            View::Speaker => "speaker",
        };
        hints.push(("l", view.to_string(), false));
        if self.commands.is_some() {
            hints.push(("t", "chat".to_string(), false));
            hints.push(("r", "react".to_string(), false));
        }
        hints.push(("p", self.palette.name().to_string(), false));
        hints.push(("e/[/]", "look".to_string(), false));
        hints.push(("q", "leave".to_string(), false));
        ui::draw_status(buf, area, &left, &hints);
    }

    fn notify(&mut self, msg: String) {
        self.chat.push(None, &msg);
        self.flash(msg);
    }

    fn flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    fn send_state(&self) {
        self.send(Command::SetState {
            audio_muted: self.me.audio_muted,
            video_off: self.me.video_off,
        });
    }

    fn send(&self, cmd: Command) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(cmd);
        }
    }
}

fn reaction_for(key: char) -> Option<&'static str> {
    let index = key.to_digit(10)?.checked_sub(1)?;
    signal::REACTIONS.get(index as usize).copied()
}

async fn recv(events: &mut Option<mpsc::UnboundedReceiver<Event>>) -> Option<Event> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}
