use std::cell::RefCell;
use std::rc::{Rc, Weak};

use anyhow::Result;
use facetty_proto::media::{
    self as packets, AudioPacket, ClientControl, ServerControl, VideoFrame,
};
use facetty_proto::signal::{ClientEvent, Participant, ServerEvent};
use futures::StreamExt;
use futures::channel::mpsc;
use js_sys::{Function, Promise, Reflect};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{HtmlCanvasElement, MediaStream};
use web_transport_wasm::{SendStream, Session};

use crate::audio::Audio;
use crate::media::{self, Camera, Devices};
use crate::publish::{self, Outbox, Publisher};
use crate::render::Renderer;
use crate::signal::{self, Signal};
use crate::stage::Stage;
use crate::transport;
use crate::ui::{self, Action, DeviceKind, Phase, Ui, View};
use crate::util::{element, js, load, save, sleep, window};

const FPS: i32 = 15;
const LISTEN_MS: i32 = 100;
const NAME_KEY: &str = "facetty.name";
const MIN_EXPOSURE: f32 = 0.2;
const MAX_EXPOSURE: f32 = 4.0;

enum Event {
    Ui(Action),
    Server(ServerEvent),
    Control(ServerControl),
    Camera(Result<MediaStream, String>),
    Mic(Result<MediaStream, String>),
    Devices(Devices),
    Copied(Result<String, String>),
    Closed(String),
}

type Events = mpsc::UnboundedSender<Event>;
type Shared<T> = Rc<RefCell<T>>;
type FrameCallback = Closure<dyn FnMut()>;

pub async fn run() {
    if let Err(e) = start().await {
        ui::fail(&format!("{e:#}"));
    }
}

async fn start() -> Result<()> {
    let location = window().location();
    let code = location
        .pathname()
        .map_err(js)?
        .trim_matches('/')
        .to_ascii_lowercase();
    let search =
        web_sys::UrlSearchParams::new_with_str(&location.search().map_err(js)?).map_err(js)?;

    let (events, mut inbox) = mpsc::unbounded::<Event>();
    let (actions, mut from_ui) = mpsc::unbounded::<Action>();
    let ui = Ui::new(actions)?;
    let forward = events.clone();
    spawn_local(async move {
        while let Some(action) = from_ui.next().await {
            let _ = forward.unbounded_send(Event::Ui(action));
        }
    });
    if !signal::exists(&code).await? {
        ui.end(signal::INVALID_CODE, false);
        return Ok(());
    }
    if let Some(name) = search.get("name").or_else(|| load(NAME_KEY)) {
        ui.name().set_value(&name);
    }

    let renderer = Renderer::new(element::<HtmlCanvasElement>("view")?)?;
    let (subscribe, subscriptions) = mpsc::unbounded::<ClientControl>();
    let me = Participant {
        id: 0,
        name: String::new(),
        audio_muted: false,
        video_off: false,
    };
    let stage = Rc::new(RefCell::new(Stage::new(renderer, me.clone(), subscribe)?));
    let camera = Rc::new(Camera::new()?);
    let (outbox, rings) = Outbox::new();
    let publisher = Rc::new(RefCell::new(Publisher::new(outbox.clone())));
    spawn_local(capture(
        Rc::downgrade(&camera),
        publisher.clone(),
        Rc::downgrade(&stage),
    ));
    animate(Rc::downgrade(&stage));

    let mut call = Call {
        code,
        ui,
        stage,
        publisher,
        camera,
        camera_on: true,
        camera_device: None,
        mic: None,
        mic_on: true,
        speaker: None,
        audio: None,
        me,
        link: None,
        pending: Some(Pending {
            outbox,
            rings,
            subscriptions,
        }),
        events,
    };
    call.fit();
    call.show_look();
    call.show_media();
    let _ = call.ui.name().focus();

    let (video, mic) = media::open().await;
    call.camera_on = video.is_some();
    call.camera_device = video.as_ref().and_then(media::device);
    call.camera.set_stream(video);
    call.mic_on = mic.is_some();
    call.mic = mic;
    call.show_media();

    while let Some(event) = inbox.next().await {
        if call.handle(event).await {
            break;
        }
    }
    Ok(())
}

/// Claude: what joining consumes, made before it so the lobby can show the
/// camera.
struct Pending {
    outbox: Outbox,
    rings: mpsc::UnboundedReceiver<()>,
    subscriptions: mpsc::UnboundedReceiver<ClientControl>,
}

struct Link {
    signal: Signal,
    session: Session,
}

/// Claude: `camera_on` and `mic_on` are what the user asked for; the
/// devices may still be opening or have failed to.
struct Call {
    code: String,
    ui: Ui,
    stage: Shared<Stage>,
    publisher: Shared<Publisher>,
    camera: Rc<Camera>,
    camera_on: bool,
    camera_device: Option<String>,
    mic: Option<MediaStream>,
    mic_on: bool,
    speaker: Option<String>,
    audio: Option<Rc<Audio>>,
    me: Participant,
    link: Option<Link>,
    pending: Option<Pending>,
    events: Events,
}

impl Call {
    /// Claude: returns true once the call is over.
    async fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Ui(action) => return self.on_action(action).await,
            Event::Server(event) => return self.on_server(event),
            Event::Control(msg) => match msg {
                ServerControl::EncodeRungs { rungs } => {
                    self.publisher.borrow_mut().set_rungs(rungs)
                }
                ServerControl::Refresh { rung } => self.publisher.borrow_mut().refresh(rung),
                ServerControl::Error { message } => return self.end(&message),
                ServerControl::Welcome { .. } => {}
            },
            Event::Camera(Ok(stream)) if self.camera_on => {
                self.camera_device = media::device(&stream);
                self.camera.set_stream(Some(stream));
                self.show_media();
            }
            Event::Camera(Ok(stream)) => media::stop(&stream),
            Event::Camera(Err(e)) => {
                self.ui.toast(Some("camera"), &format!("camera: {e}"));
                if !self.camera.is_on() {
                    self.camera_on = false;
                    self.show_media();
                }
            }
            Event::Mic(Ok(stream)) => {
                if let Some(old) = self.mic.replace(stream) {
                    media::stop(&old);
                }
                if let Some(audio) = &self.audio {
                    audio.set_microphone(self.mic.as_ref());
                }
                self.show_media();
            }
            Event::Mic(Err(e)) => {
                self.ui.toast(Some("mic"), &format!("microphone: {e}"));
                if self.mic.is_none() {
                    self.mic_on = false;
                    self.show_media();
                }
            }
            Event::Devices(devices) => {
                let current = [
                    self.camera.device().or_else(|| self.camera_device.clone()),
                    self.mic.as_ref().and_then(media::device),
                    self.speaker.clone(),
                ];
                let speakers = self.audio.as_ref().is_some_and(|a| a.can_choose_speaker());
                self.ui.show_devices(&devices, current, speakers);
            }
            Event::Copied(Ok(_)) => self.ui.toast(Some("invite"), "invite link copied"),
            Event::Copied(Err(link)) => self.ui.toast(Some("invite"), &link),
            Event::Closed(reason) => return self.end(&reason),
        }
        false
    }

    async fn on_action(&mut self, action: Action) -> bool {
        match action {
            Action::Mic => {
                if self.mic.is_none() && !self.mic_on {
                    self.request(DeviceKind::Mic, None);
                }
                self.mic_on = !self.mic_on;
                self.show_media();
            }
            Action::Camera => {
                self.camera_on = !self.camera_on;
                if self.camera_on {
                    self.request(DeviceKind::Camera, self.camera_device.clone());
                } else {
                    self.camera.set_stream(None);
                    self.stage.borrow_mut().show_self(None);
                }
                self.show_media();
            }
            Action::Join(name) => {
                if self.link.is_none() {
                    self.join(name.trim()).await;
                }
            }
            Action::Resize => self.fit(),
            _ if self.link.is_none() => {}
            Action::Chat => self.set_chat(!self.ui.chatting()),
            Action::Type => self.set_chat(true),
            Action::Say(text) => self.send(&ClientEvent::Chat { text }),
            Action::React(emoji) => self.send(&ClientEvent::React {
                emoji: emoji.to_string(),
            }),
            Action::Invite => self.invite(),
            Action::Devices => {
                let events = self.events.clone();
                spawn_local(async move {
                    match media::devices().await {
                        Ok(devices) => {
                            let _ = events.unbounded_send(Event::Devices(devices));
                        }
                        Err(e) => web_sys::console::warn_1(&format!("{e:#}").into()),
                    }
                });
            }
            Action::Use(DeviceKind::Camera, id) => {
                self.camera_device = Some(id.clone());
                if self.camera_on {
                    self.request(DeviceKind::Camera, Some(id));
                }
            }
            Action::Use(DeviceKind::Mic, id) => self.request(DeviceKind::Mic, Some(id)),
            Action::Use(DeviceKind::Speaker, id) => self.use_speaker(id),
            Action::View(view) => self.set_view(view, false),
            Action::NextView => {
                let view = self.stage.borrow().view().next();
                self.set_view(view, true);
            }
            Action::Palette(palette) => {
                self.stage.borrow_mut().set_palette(palette);
                self.show_look();
            }
            Action::NextPalette => {
                let palette = self.stage.borrow().palette().next();
                self.stage.borrow_mut().set_palette(palette);
                self.show_look();
                self.ui
                    .toast(Some("palette"), &format!("palette: {}", palette.name()));
            }
            Action::Edges(on) => {
                self.publisher.borrow_mut().update_params(|p| p.edges = on);
                self.show_look();
            }
            Action::ToggleEdges => {
                self.publisher
                    .borrow_mut()
                    .update_params(|p| p.edges = !p.edges);
                self.show_look();
                let on = self.publisher.borrow().params().edges;
                self.ui
                    .toast(Some("edges"), if on { "edges on" } else { "edges off" });
            }
            Action::Exposure(value) => {
                self.set_exposure(|_| value);
            }
            Action::NudgeExposure(step) => {
                let exposure = self.set_exposure(|e| e + step);
                self.ui
                    .toast(Some("exposure"), &format!("exposure {exposure:.1}"));
            }
            Action::Leave => return self.end(""),
        }
        false
    }

    fn on_server(&mut self, event: ServerEvent) -> bool {
        match event {
            ServerEvent::Joined { participant } => {
                self.ui.toast(None, &format!("{} joined", participant.name));
                self.stage.borrow_mut().add(participant);
            }
            ServerEvent::Left { id } => {
                let name = self.stage.borrow().name(id);
                if let Some(name) = name {
                    self.ui.toast(None, &format!("{name} left"));
                }
                self.stage.borrow_mut().remove(id);
                if let Some(audio) = &self.audio {
                    audio.remove(id);
                }
            }
            ServerEvent::Updated { participant } if participant.id == self.me.id => {}
            ServerEvent::Updated { participant } => self.stage.borrow_mut().update(participant),
            ServerEvent::Chat { from, name, text } => {
                let mine = from == self.me.id;
                self.ui.chat.push(from, &name, &text, mine);
                if !mine && !self.ui.chatting() {
                    self.ui.toast(None, &format!("{name}: {text}"));
                    self.ui.set_unread(true);
                }
            }
            ServerEvent::Reaction { from, emoji } => self.stage.borrow_mut().react(from, &emoji),
            ServerEvent::Error { message } => return self.end(&message),
            ServerEvent::Welcome { .. } => {}
        }
        false
    }

    async fn join(&mut self, name: &str) {
        if !name.is_empty() {
            save(NAME_KEY, name);
        }
        self.ui.set_joining(true);
        match self.connect(name).await {
            Ok(()) => {
                self.ui.set_phase(Phase::Call);
                self.fit();
            }
            Err(e) => {
                self.ui.set_joining(false);
                self.ui.toast(Some("join"), &format!("{e:#}"));
            }
        }
    }

    async fn connect(&mut self, name: &str) -> Result<()> {
        let late = Rc::new(RefCell::new(None::<Session>));
        if self.audio.is_none() {
            match Audio::start(!self.mic_on, audio_sender(late.clone())).await {
                Ok(audio) => {
                    let audio = Rc::new(audio);
                    audio.set_microphone(self.mic.as_ref());
                    spawn_local(listen(Rc::downgrade(&audio), Rc::downgrade(&self.stage)));
                    self.audio = Some(audio);
                }
                Err(e) => web_sys::console::warn_1(&format!("no call audio: {e:#}").into()),
            }
        }

        let (welcome, signal, server_events) = signal::connect(&self.code, name).await?;
        let session = transport::connect(&welcome.media).await?;
        let (control_send, control) = transport::hello(&session, welcome.token).await?;
        let pending = self
            .pending
            .take()
            .ok_or_else(|| anyhow::anyhow!("already joined"))?;
        *late.borrow_mut() = Some(session.clone());
        spawn_local(write_control(control_send, pending.subscriptions));
        spawn_local(publish::send(
            session.clone(),
            pending.outbox,
            pending.rings,
        ));
        spawn_local(forward_server(server_events, self.events.clone()));
        spawn_local(read_control(control, self.events.clone()));
        spawn_local(watch_session(session.clone(), self.events.clone()));
        spawn_local(receive_video(session.clone(), self.stage.clone()));
        if let Some(audio) = &self.audio {
            spawn_local(receive_audio(session.clone(), audio.clone()));
        }

        self.me = welcome.me;
        self.link = Some(Link { signal, session });
        self.show_media();
        let mut stage = self.stage.borrow_mut();
        for participant in welcome.participants {
            stage.add(participant);
        }
        Ok(())
    }

    fn request(&self, kind: DeviceKind, device: Option<String>) {
        let events = self.events.clone();
        spawn_local(async move {
            let event = match kind {
                DeviceKind::Camera => Event::Camera(
                    media::camera(device.as_deref())
                        .await
                        .map_err(|e| format!("{e:#}")),
                ),
                _ => Event::Mic(
                    media::microphone(device.as_deref())
                        .await
                        .map_err(|e| format!("{e:#}")),
                ),
            };
            let _ = events.unbounded_send(event);
        });
    }

    fn use_speaker(&mut self, id: String) {
        let Some(audio) = self.audio.clone() else {
            return;
        };
        self.speaker = Some(id.clone());
        let events = self.events.clone();
        spawn_local(async move {
            if let Err(e) = audio.set_speaker(&id).await {
                web_sys::console::warn_1(&format!("speaker: {e:#}").into());
                let _ = events.unbounded_send(Event::Ui(Action::Devices));
            }
        });
    }

    /// Claude: shows the microphone and camera as asked for, and tells
    /// everyone what they actually get.
    fn show_media(&mut self) {
        self.ui.set_mic(self.mic_on);
        self.ui.set_camera(self.camera_on);
        self.me.audio_muted = !(self.mic_on && self.mic.is_some());
        self.me.video_off = !(self.camera_on && self.camera.is_on());
        if let Some(audio) = &self.audio {
            audio.set_muted(self.me.audio_muted);
        }
        self.stage.borrow_mut().set_me(self.me.clone());
        self.send(&ClientEvent::SetState {
            audio_muted: self.me.audio_muted,
            video_off: self.me.video_off,
        });
    }

    fn show_look(&self) {
        let params = self.publisher.borrow().params();
        let stage = self.stage.borrow();
        self.ui
            .show_look(stage.view(), stage.palette(), params.edges, params.exposure);
    }

    fn set_exposure(&self, f: impl FnOnce(f32) -> f32) -> f32 {
        self.publisher
            .borrow_mut()
            .update_params(|p| p.exposure = f(p.exposure).clamp(MIN_EXPOSURE, MAX_EXPOSURE));
        self.show_look();
        self.publisher.borrow().params().exposure
    }

    fn set_view(&mut self, view: View, announce: bool) {
        self.stage.borrow_mut().set_view(view);
        self.show_look();
        if announce {
            self.ui
                .toast(Some("view"), &format!("{} view", view.name()));
        }
    }

    fn set_chat(&mut self, open: bool) {
        self.ui.set_chat(open);
        self.fit();
        if open {
            self.ui.chat.focus();
        }
    }

    fn fit(&self) {
        let area = self.ui.stage_area();
        self.stage.borrow_mut().set_area(area);
    }

    /// Claude: WebTransport needs a secure context, so the clipboard is
    /// there whenever a call is; it can still refuse.
    fn invite(&self) {
        let origin = window().location().origin().unwrap_or_default();
        let link = format!("{origin}/{}", self.code);
        let written = Reflect::get(&window().navigator(), &"clipboard".into())
            .ok()
            .filter(|c| !c.is_undefined())
            .and_then(|clipboard| {
                let write: Function = Reflect::get(&clipboard, &"writeText".into())
                    .ok()?
                    .dyn_into()
                    .ok()?;
                write.call1(&clipboard, &link.as_str().into()).ok()
            })
            .and_then(|p| p.dyn_into::<Promise>().ok());
        let events = self.events.clone();
        spawn_local(async move {
            let copied = match written {
                Some(promise) => JsFuture::from(promise).await.is_ok(),
                None => false,
            };
            let result = if copied { Ok(link) } else { Err(link) };
            let _ = events.unbounded_send(Event::Copied(result));
        });
    }

    fn send(&self, event: &ClientEvent) {
        if let Some(link) = &self.link {
            link.signal.send(event);
        }
    }

    fn end(&mut self, message: &str) -> bool {
        if let Some(link) = self.link.take() {
            drop(link.signal);
            link.session.close(0, "bye");
        }
        self.camera.set_stream(None);
        if let Some(mic) = self.mic.take() {
            media::stop(&mic);
        }
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        self.ui.end(message, true);
        true
    }
}

async fn forward_server(mut server: mpsc::UnboundedReceiver<ServerEvent>, events: Events) {
    while let Some(event) = server.next().await {
        let _ = events.unbounded_send(Event::Server(event));
    }
    let _ = events.unbounded_send(Event::Closed("disconnected".into()));
}

async fn read_control(mut control: transport::ControlReader, events: Events) {
    while let Ok(Some(msg)) = control.next().await {
        let _ = events.unbounded_send(Event::Control(msg));
    }
    let _ = events.unbounded_send(Event::Closed("disconnected".into()));
}

async fn watch_session(session: Session, events: Events) {
    session.closed().await;
    let _ = events.unbounded_send(Event::Closed("disconnected".into()));
}

async fn write_control(mut send: SendStream, mut messages: mpsc::UnboundedReceiver<ClientControl>) {
    while let Some(msg) = messages.next().await {
        if transport::write_control(&mut send, &msg).await.is_err() {
            break;
        }
    }
}

/// Claude: audio starts before the media session exists, inside the click
/// that joins, so the browser lets it play; packets before then are
/// dropped.
fn audio_sender(session: Rc<RefCell<Option<Session>>>) -> impl FnMut(u16, Vec<u8>) + 'static {
    move |seq, payload| {
        let Some(session) = session.borrow().clone() else {
            return;
        };
        let packet = AudioPacket {
            publisher: 0,
            seq,
            payload,
        };
        let _ = session.try_send_datagram(&packets::encode(&packet));
    }
}

fn animate(stage: Weak<RefCell<Stage>>) {
    let tick: Rc<RefCell<Option<FrameCallback>>> = Rc::default();
    let next = tick.clone();
    *tick.borrow_mut() = Some(Closure::new(move || {
        let Some(stage) = stage.upgrade() else {
            return;
        };
        stage.borrow_mut().draw();
        if let Some(tick) = next.borrow().as_ref() {
            let _ = window().request_animation_frame(tick.as_ref().unchecked_ref());
        }
    }));
    if let Some(tick) = tick.borrow().as_ref() {
        let _ = window().request_animation_frame(tick.as_ref().unchecked_ref());
    }
    std::mem::forget(tick);
}

async fn capture(camera: Weak<Camera>, publisher: Shared<Publisher>, stage: Weak<RefCell<Stage>>) {
    loop {
        sleep(1000 / FPS).await;
        let (Some(camera), Some(stage)) = (camera.upgrade(), stage.upgrade()) else {
            return;
        };
        let Some(image) = camera.grab() else {
            continue;
        };
        let self_size = stage.borrow().self_size();
        let picture = publisher.borrow_mut().publish(&image, self_size);
        stage.borrow_mut().show_self(picture);
    }
}

async fn listen(audio: Weak<Audio>, stage: Weak<RefCell<Stage>>) {
    loop {
        sleep(LISTEN_MS).await;
        let (Some(audio), Some(stage)) = (audio.upgrade(), stage.upgrade()) else {
            return;
        };
        stage.borrow_mut().hear(&audio.levels(), audio.level());
    }
}

async fn receive_video(session: Session, stage: Shared<Stage>) {
    while let Ok(mut stream) = session.accept_uni().await {
        let stage = stage.clone();
        spawn_local(async move {
            let Ok(bytes) = transport::read_to_end(&mut stream, packets::MAX_VIDEO_FRAME).await
            else {
                return;
            };
            if let Ok(msg) = packets::decode::<VideoFrame>(&bytes) {
                stage.borrow_mut().video(&msg);
            }
        });
    }
}

async fn receive_audio(session: Session, audio: Rc<Audio>) {
    while let Ok(datagram) = session.recv_datagram().await {
        if let Ok(packet) = packets::decode::<AudioPacket>(&datagram) {
            audio.receive(packet.publisher, packet.seq, &packet.payload);
        }
    }
}
