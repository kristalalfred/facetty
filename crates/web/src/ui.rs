use anyhow::Result;
use facetty_proto::signal::REACTIONS;
use futures::channel::mpsc;
use js_sys::{Function, Reflect};
use wasm_bindgen::JsCast;
use web_sys::{
    Element, Event, HtmlButtonElement, HtmlElement, HtmlFormElement, HtmlInputElement,
    HtmlOptionElement, HtmlSelectElement, KeyboardEvent,
};

use crate::chat::Chat;
use crate::layout::Rect;
use crate::media::{Device, Devices};
use crate::render::Palette;
use crate::util::{after, create, document, element, on, window};

const TOAST_TIME: i32 = 4000;
const MAX_TOASTS: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Grid,
    Speaker,
}

impl View {
    const ALL: [View; 2] = [View::Grid, View::Speaker];

    pub fn name(self) -> &'static str {
        match self {
            View::Grid => "grid",
            View::Speaker => "speaker",
        }
    }

    pub fn next(self) -> Self {
        match self {
            View::Grid => View::Speaker,
            View::Speaker => View::Grid,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Camera,
    Mic,
    Speaker,
}

pub enum Action {
    Mic,
    Camera,
    Join(String),
    Chat,
    Type,
    Say(String),
    React(&'static str),
    Invite,
    Devices,
    Use(DeviceKind, String),
    View(View),
    NextView,
    Palette(Palette),
    NextPalette,
    Edges(bool),
    ToggleEdges,
    Exposure(f32),
    NudgeExposure(f32),
    Leave,
    Resize,
}

pub enum Phase {
    Call,
    Ended,
}

pub struct Ui {
    body: HtmlElement,
    bar: HtmlElement,
    mic: HtmlElement,
    camera: HtmlElement,
    chat_button: HtmlElement,
    badge: HtmlElement,
    name: HtmlInputElement,
    join: HtmlElement,
    join_button: HtmlButtonElement,
    toasts: HtmlElement,
    reactions: HtmlElement,
    settings: HtmlElement,
    camera_device: HtmlSelectElement,
    mic_device: HtmlSelectElement,
    speaker_device: HtmlSelectElement,
    speaker_row: HtmlElement,
    views: HtmlElement,
    palettes: HtmlElement,
    edges: HtmlInputElement,
    exposure: HtmlInputElement,
    message: HtmlElement,
    rejoin: HtmlElement,
    pub chat: Chat,
}

impl Ui {
    pub fn new(actions: mpsc::UnboundedSender<Action>) -> Result<Self> {
        let ui = Self {
            body: document().body().expect("page has a body"),
            bar: element("bar")?,
            mic: element("mic")?,
            camera: element("camera")?,
            chat_button: element("chat-button")?,
            badge: element::<HtmlElement>("chat-button")?
                .query_selector(".badge")
                .ok()
                .flatten()
                .map(Element::unchecked_into)
                .ok_or_else(|| anyhow::anyhow!("chat button has no badge"))?,
            name: element("name")?,
            join: element("join")?,
            join_button: element::<HtmlElement>("join")?
                .query_selector("button")
                .ok()
                .flatten()
                .map(Element::unchecked_into)
                .ok_or_else(|| anyhow::anyhow!("join form has no button"))?,
            toasts: element("toasts")?,
            reactions: element("reactions")?,
            settings: element("settings")?,
            camera_device: element("camera-device")?,
            mic_device: element("mic-device")?,
            speaker_device: element("speaker-device")?,
            speaker_row: element("speaker-row")?,
            views: element("views")?,
            palettes: element("palettes")?,
            edges: element("edges")?,
            exposure: element("exposure")?,
            message: element("message")?,
            rejoin: element("rejoin")?,
            chat: Chat::new()?,
        };
        for (i, emoji) in REACTIONS.iter().enumerate() {
            let button = create("button", "");
            button.set_text_content(Some(emoji));
            button.set_title(&(i + 1).to_string());
            let _ = button.set_attribute("aria-label", emoji);
            let _ = ui.reactions.append_child(&button);
            let send = actions.clone();
            on(&button, "click", move |_: Event| {
                let _ = send.unbounded_send(Action::React(emoji));
            });
        }
        ui.bind(actions)?;
        Ok(ui)
    }

    fn bind(&self, actions: mpsc::UnboundedSender<Action>) -> Result<()> {
        let send = move |action| {
            let _ = actions.unbounded_send(action);
        };
        let click = |target: &HtmlElement, action: fn() -> Action| {
            let send = send.clone();
            on(target, "click", move |_: Event| send(action()));
        };
        click(&self.mic, || Action::Mic);
        click(&self.camera, || Action::Camera);
        click(&self.chat_button, || Action::Chat);
        click(&element("invite")?, || Action::Invite);
        click(&element("leave")?, || Action::Leave);
        on(&self.rejoin, "click", |_: Event| {
            let _ = window().location().reload();
        });

        let name = self.name.clone();
        let send_join = send.clone();
        on(&self.join, "submit", move |e: Event| {
            e.prevent_default();
            send_join(Action::Join(name.value()));
        });
        let say = self.chat.input().clone();
        let send_say = send.clone();
        on(
            &element::<HtmlFormElement>("compose")?.into(),
            "submit",
            move |e: Event| {
                e.prevent_default();
                let text = say.value();
                if !text.trim().is_empty() {
                    say.set_value("");
                    send_say(Action::Say(text));
                }
            },
        );

        let send_open = send.clone();
        on(&self.settings, "toggle", move |e: Event| {
            let open = Reflect::get(&e, &"newState".into())
                .ok()
                .and_then(|s| s.as_string())
                .is_some_and(|s| s == "open");
            if open {
                send_open(Action::Devices);
            }
        });
        for (select, kind) in [
            (&self.camera_device, DeviceKind::Camera),
            (&self.mic_device, DeviceKind::Mic),
            (&self.speaker_device, DeviceKind::Speaker),
        ] {
            let send = send.clone();
            let source = select.clone();
            on(select, "change", move |_: Event| {
                send(Action::Use(kind, source.value()))
            });
        }
        let send_view = send.clone();
        on(&self.views, "click", move |e: Event| {
            if let Some(view) =
                choice(&e).and_then(|c| View::ALL.into_iter().find(|v| v.name() == c))
            {
                send_view(Action::View(view));
            }
        });
        let send_palette = send.clone();
        on(&self.palettes, "click", move |e: Event| {
            if let Some(palette) =
                choice(&e).and_then(|c| Palette::ALL.into_iter().find(|p| p.name() == c))
            {
                send_palette(Action::Palette(palette));
            }
        });
        let edges = self.edges.clone();
        let send_edges = send.clone();
        on(&self.edges, "change", move |_: Event| {
            send_edges(Action::Edges(edges.checked()))
        });
        let exposure = self.exposure.clone();
        let send_exposure = send.clone();
        on(&self.exposure, "input", move |_: Event| {
            if let Ok(value) = exposure.value().parse() {
                send_exposure(Action::Exposure(value));
            }
        });

        let send_resize = send.clone();
        on(&window(), "resize", move |_: Event| {
            send_resize(Action::Resize)
        });

        let body = self.body.clone();
        let popovers = [self.reactions.clone(), self.settings.clone()];
        let (reactions, settings) = (self.reactions.clone(), self.settings.clone());
        on(&window(), "keydown", move |e: KeyboardEvent| {
            if e.ctrl_key() || e.meta_key() || e.alt_key() || e.is_composing() || e.repeat() {
                return;
            }
            let target = e.target().and_then(|t| t.dyn_into::<Element>().ok());
            let tag = target.as_ref().map(Element::tag_name).unwrap_or_default();
            if matches!(tag.as_str(), "INPUT" | "SELECT" | "TEXTAREA") {
                if e.key() == "Escape"
                    && let Some(target) = target.and_then(|t| t.dyn_into::<HtmlElement>().ok())
                {
                    let _ = target.blur();
                }
                return;
            }
            let phase = body.dataset().get("phase");
            let in_call = match phase.as_deref() {
                Some("call") => true,
                Some("lobby") => false,
                _ => return,
            };
            let key = e.key();
            let action = match key.as_str() {
                "m" => Some(Action::Mic),
                "v" => Some(Action::Camera),
                _ if !in_call => None,
                "t" => Some(Action::Chat),
                "Enter" | "/" if tag == "BODY" => Some(Action::Type),
                "c" => Some(Action::Invite),
                "l" => Some(Action::NextView),
                "p" => Some(Action::NextPalette),
                "e" => Some(Action::ToggleEdges),
                "[" => Some(Action::NudgeExposure(-0.1)),
                "]" => Some(Action::NudgeExposure(0.1)),
                "q" => Some(Action::Leave),
                "r" => {
                    call(&reactions, "togglePopover");
                    None
                }
                "d" => {
                    call(&settings, "togglePopover");
                    None
                }
                "Escape" if !popovers.iter().any(is_open) && is_chatting(&body) => {
                    Some(Action::Chat)
                }
                digit => digit
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| REACTIONS.get(n.checked_sub(1)?))
                    .map(|emoji| Action::React(emoji)),
            };
            if let Some(action) = action {
                e.prevent_default();
                send(action);
            }
        });
        Ok(())
    }

    pub fn set_phase(&self, phase: Phase) {
        let name = match phase {
            Phase::Call => "call",
            Phase::Ended => "ended",
        };
        let _ = self.body.dataset().set("phase", name);
        if !matches!(phase, Phase::Call) {
            for popover in [&self.reactions, &self.settings] {
                call(popover, "hidePopover");
            }
        }
    }

    pub fn name(&self) -> &HtmlInputElement {
        &self.name
    }

    pub fn set_joining(&self, joining: bool) {
        self.name.set_disabled(joining);
        self.join_button.set_disabled(joining);
    }

    pub fn set_mic(&self, on: bool) {
        set_class(&self.mic, "off", !on);
        let _ = self
            .mic
            .set_attribute("aria-pressed", if on { "true" } else { "false" });
    }

    pub fn set_camera(&self, on: bool) {
        set_class(&self.camera, "off", !on);
        let _ = self
            .camera
            .set_attribute("aria-pressed", if on { "true" } else { "false" });
    }

    pub fn set_chat(&mut self, open: bool) {
        self.chat.set_open(open);
        set_class(&self.body, "chatting", open);
        set_class(&self.chat_button, "active", open);
        if open {
            self.set_unread(false);
        }
    }

    pub fn chatting(&self) -> bool {
        is_chatting(&self.body)
    }

    pub fn set_unread(&self, unread: bool) {
        self.badge.set_hidden(!unread);
    }

    /// Claude: a toast of the same `kind` replaces the last one, so repeated
    /// changes show only their latest value.
    pub fn toast(&self, kind: Option<&str>, text: &str) {
        if let Some(kind) = kind
            && let Ok(Some(old)) = self
                .toasts
                .query_selector(&format!("[data-kind=\"{kind}\"]"))
        {
            old.remove();
        }
        let toast = create("div", "toast");
        toast.set_text_content(Some(text));
        if let Some(kind) = kind {
            let _ = toast.dataset().set("kind", kind);
        }
        let _ = self.toasts.append_child(&toast);
        while self.toasts.child_element_count() > MAX_TOASTS {
            if let Some(first) = self.toasts.first_element_child() {
                first.remove();
            }
        }
        after(TOAST_TIME, move || {
            set_class(&toast, "leaving", true);
            after(300, move || toast.remove());
        });
    }

    pub fn show_devices(&self, devices: &Devices, current: [Option<String>; 3], speakers: bool) {
        let [camera, mic, speaker] = current;
        fill(&self.camera_device, &devices.cameras, camera.as_deref());
        fill(&self.mic_device, &devices.mics, mic.as_deref());
        self.speaker_row
            .set_hidden(!speakers || devices.speakers.is_empty());
        fill(&self.speaker_device, &devices.speakers, speaker.as_deref());
    }

    pub fn show_look(&self, view: View, palette: Palette, edges: bool, exposure: f32) {
        press(&self.views, view.name());
        press(&self.palettes, palette.name());
        self.edges.set_checked(edges);
        self.exposure.set_value(&format!("{exposure:.1}"));
    }

    /// Claude: the part of the window tiles may use: above the bar, and
    /// beside the chat unless the chat covers the window.
    pub fn stage_area(&self) -> Rect {
        let w = window();
        let px =
            |v: Result<wasm_bindgen::JsValue, _>| v.ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        let (width, height) = (px(w.inner_width()), px(w.inner_height()));
        let chat = if self.chatting() {
            Some(self.chat.width())
                .filter(|&c| c < width)
                .unwrap_or(0.0)
        } else {
            0.0
        };
        Rect {
            x: 0.0,
            y: 0.0,
            w: width - chat,
            h: self.bar.get_bounding_client_rect().top().min(height),
        }
    }

    pub fn end(&self, message: &str, rejoin: bool) {
        self.set_phase(Phase::Ended);
        self.message.set_text_content(Some(message));
        self.rejoin.set_hidden(!rejoin);
    }
}

/// Claude: for failures before the controls exist.
pub fn fail(message: &str) {
    if let Some(body) = document().body() {
        let _ = body.dataset().set("phase", "ended");
    }
    if let Ok(text) = element::<HtmlElement>("message") {
        text.set_text_content(Some(message));
    }
}

fn choice(e: &Event) -> Option<String> {
    let target: Element = e.target()?.dyn_into().ok()?;
    let button = target.closest("[data-choice]").ok()??;
    button.get_attribute("data-choice")
}

fn press(group: &HtmlElement, chosen: &str) {
    let buttons = group.query_selector_all("[data-choice]");
    let Ok(buttons) = buttons else {
        return;
    };
    for i in 0..buttons.length() {
        if let Some(button) = buttons.get(i).and_then(|b| b.dyn_into::<Element>().ok()) {
            let pressed = button.get_attribute("data-choice").as_deref() == Some(chosen);
            let _ = button.set_attribute("aria-pressed", if pressed { "true" } else { "false" });
        }
    }
}

fn fill(select: &HtmlSelectElement, devices: &[Device], current: Option<&str>) {
    select.set_inner_html("");
    for (i, device) in devices.iter().enumerate() {
        let label = if device.label.is_empty() {
            format!(
                "{} {}",
                select.get_attribute("aria-label").unwrap_or_default(),
                i + 1
            )
        } else {
            device.label.clone()
        };
        if let Ok(option) = HtmlOptionElement::new_with_text_and_value(&label, &device.id) {
            option.set_selected(current == Some(device.id.as_str()));
            let _ = select.append_child(&option);
        }
    }
    select.set_disabled(devices.is_empty());
}

fn set_class(element: &HtmlElement, class: &str, on: bool) {
    let _ = element.class_list().toggle_with_force(class, on);
}

fn is_chatting(body: &HtmlElement) -> bool {
    body.class_list().contains("chatting")
}

fn is_open(popover: &HtmlElement) -> bool {
    popover.matches(":popover-open").unwrap_or(false)
}

/// Claude: the popover methods through `Reflect`, since older browsers lack
/// them and should only lose the shortcut.
fn call(popover: &HtmlElement, method: &str) {
    if let Ok(f) = Reflect::get(popover, &method.into())
        && let Some(f) = f.dyn_ref::<Function>()
    {
        let _ = f.call0(popover);
    }
}
