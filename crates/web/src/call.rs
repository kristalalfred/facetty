use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use anyhow::Result;
use facetty_ascii::{Decoder, Frame};
use facetty_proto::ParticipantId;
use facetty_proto::ladder::{self, Rung};
use facetty_proto::media::{self, AudioPacket, ClientControl, ServerControl, VideoFrame};
use facetty_proto::signal::{ClientEvent, Participant, ServerEvent};
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlCanvasElement, HtmlElement};
use web_transport_wasm::Session;

use crate::audio::Audio;
use crate::camera::{self, Camera};
use crate::layout::{self, Rect};
use crate::publish::{self, Outbox, Publisher};
use crate::render::{Renderer, Tile};
use crate::util::{document, element, js, show_message, sleep, window};
use crate::{signal, transport};

const FPS: i32 = 15;
const SELF: ParticipantId = 0;

struct Remote {
    participant: Participant,
    decoder: Decoder,
    frame: Option<Frame>,
    version: u64,
    rung: Option<Rung>,
}

struct State {
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

type Shared = Rc<RefCell<State>>;
type FrameCallback = Closure<dyn FnMut()>;

pub async fn run() -> Result<()> {
    let location = window().location();
    let code = location
        .pathname()
        .map_err(js)?
        .trim_matches('/')
        .to_ascii_lowercase();
    let search =
        web_sys::UrlSearchParams::new_with_str(&location.search().map_err(js)?).map_err(js)?;
    let name = search.get("name");

    let renderer = Renderer::new(element::<HtmlCanvasElement>("view")?)?;
    let stream = camera::open().await;
    let has_video = stream.as_ref().is_some_and(camera::has_video);
    let has_audio = stream.as_ref().is_some_and(camera::has_audio);
    let (welcome, signal, mut events) = signal::connect(&code, name.as_deref()).await?;
    if !has_video || !has_audio {
        signal.send(&ClientEvent::SetState {
            audio_muted: !has_audio,
            video_off: !has_video,
        });
    }

    let session = transport::connect(&welcome.media).await?;
    let (mut control_send, mut control) = transport::hello(&session, welcome.token).await?;
    let (subscribe, mut subscriptions) = mpsc::unbounded::<ClientControl>();
    spawn_local(async move {
        while let Some(msg) = subscriptions.next().await {
            if transport::write_control(&mut control_send, &msg)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let state: Shared = Rc::new(RefCell::new(State {
        me: welcome.me,
        remotes: BTreeMap::new(),
        camera: has_video,
        self_frame: None,
        self_version: 0,
        self_size: None,
        tiles: Vec::new(),
        labels: HashMap::new(),
        subscribe,
        dirty: true,
    }));
    for participant in welcome.participants {
        add(&state, participant);
    }
    relayout(&state);

    let microphone = stream.as_ref().filter(|s| camera::has_audio(s));
    let audio = match Audio::start(microphone, audio_sender(session.clone())).await {
        Ok(audio) => Some(Rc::new(audio)),
        Err(e) => {
            web_sys::console::warn_1(&format!("no call audio: {e:#}").into());
            None
        }
    };

    let (outbox, rings) = Outbox::new();
    let publisher = Rc::new(RefCell::new(Publisher::new(outbox.clone())));
    spawn_local(publish::send(session.clone(), outbox, rings));
    if let Some(stream) = stream.as_ref().filter(|s| camera::has_video(s)) {
        spawn_local(capture(
            Camera::new(stream)?,
            publisher.clone(),
            state.clone(),
        ));
    }
    spawn_local(receive_video(session.clone(), state.clone()));
    if let Some(audio) = &audio {
        spawn_local(receive_audio(session.clone(), audio.clone()));
    }
    watch_resize(state.clone());
    animate(state.clone(), renderer);

    let control_loop = async {
        while let Ok(Some(msg)) = control.next().await {
            match msg {
                ServerControl::EncodeRungs { rungs } => publisher.borrow_mut().set_rungs(rungs),
                ServerControl::Refresh { rung } => publisher.borrow_mut().refresh(rung),
                ServerControl::Error { message } => return message,
                ServerControl::Welcome { .. } => {}
            }
        }
        "disconnected".to_string()
    };
    let signal_loop = async {
        while let Some(event) = events.next().await {
            match event {
                ServerEvent::Joined { participant } => add(&state, participant),
                ServerEvent::Left { id } => {
                    remove(&state, id);
                    if let Some(audio) = &audio {
                        audio.remove(id);
                    }
                }
                ServerEvent::Updated { participant } => {
                    if let Some(remote) = state.borrow_mut().remotes.get_mut(&participant.id) {
                        remote.participant = participant;
                    }
                }
                ServerEvent::Error { message } => return message,
                _ => continue,
            }
            relayout(&state);
        }
        "disconnected".to_string()
    };
    let reason = futures::select! {
        r = control_loop.fuse() => r,
        r = signal_loop.fuse() => r,
        _ = session.closed().fuse() => "disconnected".to_string(),
    };
    drop(signal);
    session.close(0, "bye");
    show_message(&reason);
    Ok(())
}

fn audio_sender(session: Session) -> impl FnMut(u16, Vec<u8>) + 'static {
    move |seq, payload| {
        let packet = AudioPacket {
            publisher: 0,
            seq,
            payload,
        };
        let _ = session.try_send_datagram(&media::encode(&packet));
    }
}

fn add(state: &Shared, participant: Participant) {
    let mut s = state.borrow_mut();
    if participant.id == s.me.id {
        return;
    }
    s.remotes.insert(
        participant.id,
        Remote {
            participant,
            decoder: Decoder::default(),
            frame: None,
            version: 0,
            rung: None,
        },
    );
}

fn remove(state: &Shared, id: ParticipantId) {
    let mut s = state.borrow_mut();
    s.remotes.remove(&id);
    if let Some(label) = s.labels.remove(&id) {
        label.remove();
    }
}

fn relayout(state: &Shared) {
    let mut guard = state.borrow_mut();
    let s = &mut *guard;
    let (width, height) = view_size();
    let mut ids: Vec<ParticipantId> = s.remotes.keys().copied().collect();
    if s.camera {
        ids.push(SELF);
    }
    let rects = layout::grid(width, height, ids.len());
    s.tiles = ids.into_iter().zip(rects).collect();
    for (id, rect) in &s.tiles {
        let rung = layout::rung_for(rect);
        if *id == SELF {
            s.self_size = rung
                .and_then(ladder::size)
                .map(|size| (size.cols, size.rows));
        } else if let Some(remote) = s.remotes.get_mut(id)
            && remote.rung != rung
        {
            remote.rung = rung;
            let _ = s.subscribe.unbounded_send(ClientControl::Subscribe {
                publisher: *id,
                rung,
            });
        }
    }
    let ids: Vec<ParticipantId> = s.tiles.iter().map(|(id, _)| *id).collect();
    s.labels.retain(|id, label| {
        let keep = ids.contains(id);
        if !keep {
            label.remove();
        }
        keep
    });
    s.dirty = true;
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

fn watch_resize(state: Shared) {
    let on_resize = Closure::<dyn FnMut()>::new(move || relayout(&state));
    let _ = window().add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
    on_resize.forget();
}

fn animate(state: Shared, mut renderer: Renderer) {
    let tick: Rc<RefCell<Option<FrameCallback>>> = Rc::default();
    let next = tick.clone();
    *tick.borrow_mut() = Some(Closure::new(move || {
        let scale = window().device_pixel_ratio();
        let (width, height) = view_size();
        let (w, h) = (
            (width * scale).round() as u32,
            (height * scale).round() as u32,
        );
        let mut s = state.borrow_mut();
        if renderer.size() != (w, h) {
            renderer.resize(w, h);
            s.dirty = true;
        }
        if s.dirty {
            s.dirty = false;
            draw(&mut s, &mut renderer, scale);
        }
        drop(s);
        if let Some(tick) = next.borrow().as_ref() {
            let _ = window().request_animation_frame(tick.as_ref().unchecked_ref());
        }
    }));
    if let Some(tick) = tick.borrow().as_ref() {
        let _ = window().request_animation_frame(tick.as_ref().unchecked_ref());
    }
    std::mem::forget(tick);
}

fn draw(s: &mut State, renderer: &mut Renderer, scale: f64) {
    let mut tiles = Vec::new();
    let mut unlit = Vec::new();
    for &(id, rect) in &s.tiles {
        let shown = if id == SELF {
            s.self_frame.as_ref().map(|f| (f, s.self_version))
        } else {
            s.remotes
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
    if let Err(e) = renderer.draw(&tiles, scale) {
        web_sys::console::error_1(&format!("{e:#}").into());
    }
    let lit: Vec<ParticipantId> = tiles.iter().map(|t| t.id).collect();
    for (id, label) in &s.labels {
        label.set_hidden(lit.contains(id));
    }
    for (id, rect) in unlit {
        let name = if id == SELF {
            s.me.name.clone()
        } else {
            s.remotes
                .get(&id)
                .map(|r| r.participant.name.clone())
                .unwrap_or_default()
        };
        let Some(label) = label(&mut s.labels, id) else {
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

async fn capture(camera: Camera, publisher: Rc<RefCell<Publisher>>, state: Shared) {
    loop {
        sleep(1000 / FPS).await;
        let Some(image) = camera.grab() else {
            continue;
        };
        let self_size = state.borrow().self_size;
        let picture = publisher.borrow_mut().publish(&image, self_size);
        let mut s = state.borrow_mut();
        s.self_frame = picture;
        s.self_version += 1;
        s.dirty = true;
    }
}

/// Claude: frames arrive on concurrent streams, so one can overtake another;
/// the decoder sorts that out cell by cell.
async fn receive_video(session: Session, state: Shared) {
    while let Ok(mut stream) = session.accept_uni().await {
        let state = state.clone();
        spawn_local(async move {
            let Ok(bytes) = transport::read_to_end(&mut stream, media::MAX_VIDEO_FRAME).await
            else {
                return;
            };
            let Ok(msg) = media::decode::<VideoFrame>(&bytes) else {
                return;
            };
            let mut s = state.borrow_mut();
            let Some(remote) = s.remotes.get_mut(&msg.publisher) else {
                return;
            };
            if let Ok(frame) = remote.decoder.decode(msg.seq, &msg.payload) {
                remote.frame = Some(frame);
                remote.version += 1;
                s.dirty = true;
            }
        });
    }
}

async fn receive_audio(session: Session, audio: Rc<Audio>) {
    while let Ok(datagram) = session.recv_datagram().await {
        if let Ok(packet) = media::decode::<AudioPacket>(&datagram) {
            audio.receive(packet.publisher, packet.seq, &packet.payload);
        }
    }
}
