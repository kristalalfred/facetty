use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use facetty_proto::media::{self, AudioPacket, ClientControl, ServerControl, VideoFrame};
use facetty_proto::signal::{ClientEvent, ServerEvent};
use futures::StreamExt;
use futures::channel::mpsc;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlCanvasElement;
use web_transport_wasm::Session;

use crate::audio::Audio;
use crate::camera::{self, Camera};
use crate::publish::{self, Outbox, Publisher};
use crate::render::Renderer;
use crate::stage::Stage;
use crate::util::{element, js, show_message, sleep, window};
use crate::{signal, transport};

const FPS: i32 = 15;

enum Event {
    Server(ServerEvent),
    Control(ServerControl),
    Closed(String),
}

type Events = mpsc::UnboundedSender<Event>;
type Shared<T> = Rc<RefCell<T>>;
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
    let (welcome, signal, server_events) = signal::connect(&code, name.as_deref()).await?;
    if !has_video || !has_audio {
        signal.send(&ClientEvent::SetState {
            audio_muted: !has_audio,
            video_off: !has_video,
        });
    }

    let session = transport::connect(&welcome.media).await?;
    let (control_send, control) = transport::hello(&session, welcome.token).await?;
    let (subscribe, subscriptions) = mpsc::unbounded::<ClientControl>();
    spawn_local(write_control(control_send, subscriptions));

    let stage = Rc::new(RefCell::new(Stage::new(
        renderer, welcome.me, has_video, subscribe,
    )));
    for participant in welcome.participants {
        stage.borrow_mut().add(participant);
    }
    stage.borrow_mut().relayout();

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
            stage.clone(),
        ));
    }
    spawn_local(receive_video(session.clone(), stage.clone()));
    if let Some(audio) = &audio {
        spawn_local(receive_audio(session.clone(), audio.clone()));
    }
    watch_resize(stage.clone());
    animate(stage.clone());

    let (events_tx, mut events) = mpsc::unbounded::<Event>();
    spawn_local(forward_server(server_events, events_tx.clone()));
    spawn_local(read_control(control, events_tx.clone()));
    spawn_local(watch_session(session.clone(), events_tx));

    let reason = loop {
        let Some(event) = events.next().await else {
            break "disconnected".to_string();
        };
        match event {
            Event::Server(event) => match event {
                ServerEvent::Joined { participant } => stage.borrow_mut().add(participant),
                ServerEvent::Left { id } => {
                    stage.borrow_mut().remove(id);
                    if let Some(audio) = &audio {
                        audio.remove(id);
                    }
                }
                ServerEvent::Updated { participant } => stage.borrow_mut().update(participant),
                ServerEvent::Error { message } => break message,
                _ => {}
            },
            Event::Control(msg) => match msg {
                ServerControl::EncodeRungs { rungs } => publisher.borrow_mut().set_rungs(rungs),
                ServerControl::Refresh { rung } => publisher.borrow_mut().refresh(rung),
                ServerControl::Error { message } => break message,
                ServerControl::Welcome { .. } => {}
            },
            Event::Closed(reason) => break reason,
        }
    };
    drop(signal);
    session.close(0, "bye");
    show_message(&reason);
    Ok(())
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

async fn write_control(
    mut send: web_transport_wasm::SendStream,
    mut messages: mpsc::UnboundedReceiver<ClientControl>,
) {
    while let Some(msg) = messages.next().await {
        if transport::write_control(&mut send, &msg).await.is_err() {
            break;
        }
    }
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

fn watch_resize(stage: Shared<Stage>) {
    let on_resize = Closure::<dyn FnMut()>::new(move || stage.borrow_mut().relayout());
    let _ = window().add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
    on_resize.forget();
}

fn animate(stage: Shared<Stage>) {
    let tick: Rc<RefCell<Option<FrameCallback>>> = Rc::default();
    let next = tick.clone();
    *tick.borrow_mut() = Some(Closure::new(move || {
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

async fn capture(camera: Camera, publisher: Shared<Publisher>, stage: Shared<Stage>) {
    loop {
        sleep(1000 / FPS).await;
        let Some(image) = camera.grab() else {
            continue;
        };
        let self_size = stage.borrow().self_size();
        let picture = publisher.borrow_mut().publish(&image, self_size);
        stage.borrow_mut().show_self(picture);
    }
}

async fn receive_video(session: Session, stage: Shared<Stage>) {
    while let Ok(mut stream) = session.accept_uni().await {
        let stage = stage.clone();
        spawn_local(async move {
            let Ok(bytes) = transport::read_to_end(&mut stream, media::MAX_VIDEO_FRAME).await
            else {
                return;
            };
            if let Ok(msg) = media::decode::<VideoFrame>(&bytes) {
                stage.borrow_mut().video(&msg);
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
