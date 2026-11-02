use std::collections::{BTreeMap, HashMap};

use facetty_ascii::{Decoder, Frame};
use facetty_proto::ParticipantId;
use facetty_proto::ladder::{self, Rung};
use facetty_proto::media::{ClientControl, VideoFrame};
use facetty_proto::signal::Participant;
use futures::channel::mpsc;
use web_sys::HtmlElement;

use crate::layout::{self, Rect};
use crate::render::{Palette, Renderer, Tile};
use crate::ui::View;
use crate::util::{after, create, element, now, window};

/// Claude: the local participant's tile. Server ids start at 1.
pub const SELF: ParticipantId = 0;
const SPEAKING_LEVEL: f32 = 0.35;
const SPEAKING_HOLD: f64 = 400.0;
const REACTION_TIME: i32 = 4000;
const MAX_REACTIONS: u32 = 12;
const MUTED_ICON: &str = r#"<svg viewBox="0 0 24 24"><rect x="9" y="3" width="6" height="11" rx="3"/><path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21M4 4l16 16"/></svg>"#;

struct Remote {
    participant: Participant,
    decoder: Decoder,
    frame: Option<Frame>,
    version: u64,
    rung: Option<Rung>,
}

/// Claude: the name, mute and speaking marks, and reactions over a tile.
struct Overlay {
    tile: HtmlElement,
    name: HtmlElement,
    marks: Option<Marks>,
}

#[derive(Clone, Copy, PartialEq)]
struct Marks {
    lit: bool,
    muted: bool,
    speaking: bool,
}

impl Overlay {
    fn new(parent: &HtmlElement) -> Self {
        let tile = create("div", "tile");
        let tag = create("div", "tag");
        let name = create("span", "");
        let _ = tag.append_child(&name);
        let _ = tag.insert_adjacent_html("beforeend", MUTED_ICON);
        let _ = tile.append_child(&tag);
        let _ = parent.append_child(&tile);
        Self {
            tile,
            name,
            marks: None,
        }
    }

    fn place(&self, rect: Rect) {
        let style = self.tile.style();
        let _ = style.set_property("left", &format!("{}px", rect.x));
        let _ = style.set_property("top", &format!("{}px", rect.y));
        let _ = style.set_property("width", &format!("{}px", rect.w));
        let _ = style.set_property("height", &format!("{}px", rect.h));
        let _ = style.set_property("--height", &format!("{}px", rect.h));
    }

    fn mark(&mut self, name: &str, marks: Marks) {
        if self.name.text_content().as_deref() != Some(name) {
            self.name.set_text_content(Some(name));
        }
        if self.marks == Some(marks) {
            return;
        }
        self.marks = Some(marks);
        let classes = self.tile.class_list();
        let _ = classes.toggle_with_force("unlit", !marks.lit);
        let _ = classes.toggle_with_force("muted", marks.muted);
        let _ = classes.toggle_with_force("speaking", marks.speaking);
    }
}

/// Claude: everyone's tiles: where they sit, what video they show, and the
/// overlays on top of the canvas.
pub struct Stage {
    renderer: Renderer,
    layer: HtmlElement,
    me: Participant,
    remotes: BTreeMap<ParticipantId, Remote>,
    self_frame: Option<Frame>,
    self_version: u64,
    self_size: Option<(u16, u16)>,
    area: Rect,
    view: View,
    palette: Palette,
    tiles: Vec<(ParticipantId, Rect)>,
    overlays: HashMap<ParticipantId, Overlay>,
    last_spoke: HashMap<ParticipantId, f64>,
    self_level: f32,
    active: Option<ParticipantId>,
    next_lane: u32,
    subscribe: mpsc::UnboundedSender<ClientControl>,
    dirty: bool,
}

impl Stage {
    pub fn new(
        renderer: Renderer,
        me: Participant,
        subscribe: mpsc::UnboundedSender<ClientControl>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            renderer,
            layer: element("tiles")?,
            me,
            remotes: BTreeMap::new(),
            self_frame: None,
            self_version: 0,
            self_size: None,
            area: Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            view: View::Grid,
            palette: Palette::Natural,
            tiles: Vec::new(),
            overlays: HashMap::new(),
            last_spoke: HashMap::new(),
            self_level: 0.0,
            active: None,
            next_lane: 0,
            subscribe,
            dirty: true,
        })
    }

    /// Claude: the local participant as the server knows them; their tile
    /// stays `SELF`.
    pub fn set_me(&mut self, me: Participant) {
        self.me = me;
    }

    pub fn add(&mut self, participant: Participant) {
        if participant.id == self.me.id {
            return;
        }
        self.remotes.insert(
            participant.id,
            Remote {
                participant,
                decoder: Decoder::default(),
                frame: None,
                version: 0,
                rung: None,
            },
        );
        self.relayout();
    }

    pub fn remove(&mut self, id: ParticipantId) {
        self.remotes.remove(&id);
        self.last_spoke.remove(&id);
        if let Some(overlay) = self.overlays.remove(&id) {
            overlay.tile.remove();
        }
        self.relayout();
    }

    pub fn update(&mut self, participant: Participant) {
        if let Some(remote) = self.remotes.get_mut(&participant.id) {
            if participant.video_off {
                remote.frame = None;
            }
            remote.participant = participant;
        }
        self.relayout();
    }

    pub fn name(&self, id: ParticipantId) -> Option<String> {
        self.remotes.get(&id).map(|r| r.participant.name.clone())
    }

    /// Claude: frames arrive on concurrent streams, so one can overtake
    /// another; the decoder sorts that out cell by cell.
    pub fn video(&mut self, msg: &VideoFrame) {
        let Some(remote) = self.remotes.get_mut(&msg.publisher) else {
            return;
        };
        if let Ok(frame) = remote.decoder.decode(msg.seq, &msg.payload)
            && !remote.participant.video_off
        {
            remote.frame = Some(frame);
            remote.version += 1;
            self.dirty = true;
        }
    }

    pub fn self_size(&self) -> Option<(u16, u16)> {
        self.self_size
    }

    pub fn show_self(&mut self, frame: Option<Frame>) {
        self.self_frame = frame;
        self.self_version += 1;
        self.dirty = true;
    }

    pub fn set_area(&mut self, area: Rect) {
        if area != self.area {
            self.area = area;
            self.relayout();
        }
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
        self.relayout();
    }

    pub fn palette(&self) -> Palette {
        self.palette
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        self.dirty = true;
    }

    /// Claude: `levels` are each publisher's loudness, `local` the
    /// microphone's. A participant counts as speaking for a moment after
    /// they were last loud.
    pub fn hear(&mut self, levels: &[(ParticipantId, f32)], local: f32) {
        let at = now();
        for &(id, level) in levels {
            if level > SPEAKING_LEVEL && self.remotes.contains_key(&id) {
                self.last_spoke.insert(id, at);
            }
        }
        self.self_level = local;
        if self.view == View::Speaker && self.active_speaker() != self.active {
            self.relayout();
        }
    }

    pub fn react(&mut self, from: ParticipantId, emoji: &str) {
        let id = if from == self.me.id { SELF } else { from };
        let Some(overlay) = self.overlays.get(&id) else {
            return;
        };
        if overlay
            .tile
            .query_selector_all(".reaction")
            .map_or(0, |l| l.length())
            >= MAX_REACTIONS
        {
            return;
        }
        let reaction = create("span", "reaction");
        reaction.set_text_content(Some(emoji));
        let lane = (self.next_lane as f64 * 0.618_034).fract();
        self.next_lane = self.next_lane.wrapping_add(1);
        let _ = reaction.style().set_property(
            "left",
            &format!("calc({:.1}% - {:.1}px)", lane * 100.0, lane * 34.0),
        );
        let _ = overlay.tile.append_child(&reaction);
        after(REACTION_TIME, move || reaction.remove());
    }

    fn active_speaker(&self) -> Option<ParticipantId> {
        self.last_spoke
            .iter()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(id, _)| *id)
            .or_else(|| self.remotes.keys().next().copied())
    }

    fn order(&self) -> Vec<ParticipantId> {
        let mut remotes: Vec<ParticipantId> = self.remotes.keys().copied().collect();
        if self.view == View::Speaker
            && let Some(active) = self.active_speaker()
        {
            remotes.retain(|id| *id != active);
            let mut order = vec![active, SELF];
            order.extend(remotes);
            return order;
        }
        let mut order = vec![SELF];
        order.extend(remotes);
        order
    }

    pub fn relayout(&mut self) {
        let ids = self.order();
        self.active = (self.view == View::Speaker)
            .then(|| self.active_speaker())
            .flatten();
        let rects = match self.view {
            View::Grid => layout::grid(self.area, ids.len()),
            View::Speaker => layout::speaker(self.area, ids.len()),
        };
        self.tiles = ids.into_iter().zip(rects).collect();
        for (id, rect) in &self.tiles {
            if *id == SELF {
                self.self_size = layout::rung_for(rect)
                    .and_then(ladder::size)
                    .map(|size| (size.cols, size.rows));
            } else if let Some(remote) = self.remotes.get_mut(id) {
                let rung = (!remote.participant.video_off)
                    .then(|| layout::rung_for(rect))
                    .flatten();
                if remote.rung != rung {
                    remote.rung = rung;
                    let _ = self.subscribe.unbounded_send(ClientControl::Subscribe {
                        publisher: *id,
                        rung,
                    });
                }
            }
            self.overlays
                .entry(*id)
                .or_insert_with(|| Overlay::new(&self.layer))
                .place(*rect);
        }
        self.dirty = true;
    }

    /// Claude: called on every animation frame. Overlays follow speaking
    /// right away; the canvas redraws only when video or layout changed.
    pub fn draw(&mut self) {
        self.mark();
        let scale = window().device_pixel_ratio();
        let (width, height) = view_size();
        let size = (
            (width * scale).round() as u32,
            (height * scale).round() as u32,
        );
        if self.renderer.size() != size {
            self.renderer.resize(size.0, size.1);
            self.dirty = true;
        }
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        let tiles: Vec<Tile> = self
            .tiles
            .iter()
            .filter_map(|&(id, rect)| {
                let (frame, version) =
                    shown(&self.remotes, &self.self_frame, self.self_version, id)?;
                Some(Tile {
                    id,
                    rect,
                    frame,
                    version,
                    mirror: id == SELF,
                })
            })
            .collect();
        if let Err(e) = self.renderer.draw(&tiles, scale, self.palette) {
            web_sys::console::error_1(&format!("{e:#}").into());
        }
    }

    fn mark(&mut self) {
        let at = now();
        for &(id, _) in &self.tiles {
            let lit = shown(&self.remotes, &self.self_frame, self.self_version, id).is_some();
            let (name, muted, speaking) = if id == SELF {
                let muted = self.me.audio_muted;
                let speaking = !muted && self.self_level > SPEAKING_LEVEL;
                (self.me.name.as_str(), muted, speaking)
            } else {
                let Some(p) = self.remotes.get(&id).map(|r| &r.participant) else {
                    continue;
                };
                let recent = self
                    .last_spoke
                    .get(&id)
                    .is_some_and(|t| at - t < SPEAKING_HOLD);
                (p.name.as_str(), p.audio_muted, !p.audio_muted && recent)
            };
            if let Some(overlay) = self.overlays.get_mut(&id) {
                overlay.mark(
                    name,
                    Marks {
                        lit,
                        muted,
                        speaking,
                    },
                );
            }
        }
    }
}

fn shown<'a>(
    remotes: &'a BTreeMap<ParticipantId, Remote>,
    self_frame: &'a Option<Frame>,
    self_version: u64,
    id: ParticipantId,
) -> Option<(&'a Frame, u64)> {
    if id == SELF {
        return self_frame.as_ref().map(|f| (f, self_version));
    }
    let remote = remotes.get(&id)?;
    remote.frame.as_ref().map(|f| (f, remote.version))
}

fn view_size() -> (f64, f64) {
    let w = window();
    let width = w.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
    let height = w
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    (width, height)
}
