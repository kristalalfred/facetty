use std::collections::{BTreeMap, HashMap};

use facetty_ascii::{Decoder, Frame};
use facetty_proto::ParticipantId;
use facetty_proto::ladder::{self, Rung};
use facetty_proto::media::{ClientControl, VideoFrame};
use facetty_proto::signal::Participant;
use futures::channel::mpsc;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;

use crate::layout::{self, Rect};
use crate::render::{Renderer, Tile};
use crate::util::{document, window};

/// Claude: the local participant's tile. Server ids start at 1.
pub const SELF: ParticipantId = 0;

struct Remote {
    participant: Participant,
    decoder: Decoder,
    frame: Option<Frame>,
    version: u64,
    rung: Option<Rung>,
}

/// Claude: everyone's tiles: where they sit, what video they show, and the
/// labels over the canvas.
pub struct Stage {
    renderer: Renderer,
    me: Participant,
    remotes: BTreeMap<ParticipantId, Remote>,
    camera: bool,
    self_frame: Option<Frame>,
    self_version: u64,
    self_size: Option<(u16, u16)>,
    tiles: Vec<(ParticipantId, Rect)>,
    labels: HashMap<ParticipantId, HtmlElement>,
    subscribe: mpsc::UnboundedSender<ClientControl>,
    dirty: bool,
}

impl Stage {
    pub fn new(
        renderer: Renderer,
        me: Participant,
        camera: bool,
        subscribe: mpsc::UnboundedSender<ClientControl>,
    ) -> Self {
        Self {
            renderer,
            me,
            remotes: BTreeMap::new(),
            camera,
            self_frame: None,
            self_version: 0,
            self_size: None,
            tiles: Vec::new(),
            labels: HashMap::new(),
            subscribe,
            dirty: true,
        }
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
        if let Some(label) = self.labels.remove(&id) {
            label.remove();
        }
        self.relayout();
    }

    pub fn update(&mut self, participant: Participant) {
        if let Some(remote) = self.remotes.get_mut(&participant.id) {
            remote.participant = participant;
        }
        self.relayout();
    }

    /// Claude: frames arrive on concurrent streams, so one can overtake
    /// another; the decoder sorts that out cell by cell.
    pub fn video(&mut self, msg: &VideoFrame) {
        let Some(remote) = self.remotes.get_mut(&msg.publisher) else {
            return;
        };
        if let Ok(frame) = remote.decoder.decode(msg.seq, &msg.payload) {
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

    pub fn relayout(&mut self) {
        let (width, height) = view_size();
        let mut ids: Vec<ParticipantId> = self.remotes.keys().copied().collect();
        if self.camera {
            ids.push(SELF);
        }
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: width,
            h: height,
        };
        let rects = layout::grid(area, ids.len());
        self.tiles = ids.into_iter().zip(rects).collect();
        for (id, rect) in &self.tiles {
            let rung = layout::rung_for(rect);
            if *id == SELF {
                self.self_size = rung
                    .and_then(ladder::size)
                    .map(|size| (size.cols, size.rows));
            } else if let Some(remote) = self.remotes.get_mut(id)
                && remote.rung != rung
            {
                remote.rung = rung;
                let _ = self.subscribe.unbounded_send(ClientControl::Subscribe {
                    publisher: *id,
                    rung,
                });
            }
        }
        let ids: Vec<ParticipantId> = self.tiles.iter().map(|(id, _)| *id).collect();
        self.labels.retain(|id, label| {
            let keep = ids.contains(id);
            if !keep {
                label.remove();
            }
            keep
        });
        self.dirty = true;
    }

    /// Claude: called on every animation frame; draws only when something
    /// changed.
    pub fn draw(&mut self) {
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
        let mut tiles = Vec::new();
        let mut unlit = Vec::new();
        for &(id, rect) in &self.tiles {
            let shown = if id == SELF {
                self.self_frame.as_ref().map(|f| (f, self.self_version))
            } else {
                self.remotes
                    .get(&id)
                    .filter(|r| !r.participant.video_off)
                    .and_then(|r| r.frame.as_ref().map(|f| (f, r.version)))
            };
            match shown {
                Some((frame, version)) => tiles.push(Tile {
                    id,
                    rect,
                    frame,
                    version,
                    mirror: id == SELF,
                }),
                None => unlit.push((id, rect)),
            }
        }
        if let Err(e) = self.renderer.draw(&tiles, scale) {
            web_sys::console::error_1(&format!("{e:#}").into());
        }
        let lit: Vec<ParticipantId> = tiles.iter().map(|t| t.id).collect();
        for (id, label) in &self.labels {
            label.set_hidden(lit.contains(id));
        }
        for (id, rect) in unlit {
            let name = if id == SELF {
                self.me.name.clone()
            } else {
                self.remotes
                    .get(&id)
                    .map(|r| r.participant.name.clone())
                    .unwrap_or_default()
            };
            let Some(label) = label(&mut self.labels, id) else {
                continue;
            };
            label.set_text_content(Some(&name));
            label.set_hidden(false);
            let style = label.style();
            let _ = style.set_property("left", &format!("{}px", rect.x));
            let _ = style.set_property("top", &format!("{}px", rect.y));
            let _ = style.set_property("width", &format!("{}px", rect.w));
            let _ = style.set_property("height", &format!("{}px", rect.h));
        }
    }
}

fn label(
    labels: &mut HashMap<ParticipantId, HtmlElement>,
    id: ParticipantId,
) -> Option<HtmlElement> {
    if let Some(label) = labels.get(&id) {
        return Some(label.clone());
    }
    let label: HtmlElement = document().create_element("div").ok()?.unchecked_into();
    label.set_class_name("label");
    document().body()?.append_child(&label).ok()?;
    labels.insert(id, label.clone());
    Some(label)
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
