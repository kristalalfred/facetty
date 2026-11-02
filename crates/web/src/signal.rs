use anyhow::{Result, anyhow, bail};
use facetty_proto::signal::{ClientEvent, MediaServer, Participant, ServerEvent};
use futures::StreamExt;
use futures::channel::mpsc;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{MessageEvent, Response, WebSocket};

use crate::util::{js, window};

pub struct Welcome {
    pub me: Participant,
    pub token: String,
    pub media: MediaServer,
    pub participants: Vec<Participant>,
}

/// Claude: events after the welcome arrive on the channel returned with the
/// socket, which ends when the socket closes.
pub struct Signal {
    socket: WebSocket,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut()>,
}

impl Drop for Signal {
    fn drop(&mut self) {
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

impl Signal {
    pub fn send(&self, event: &ClientEvent) {
        let json = serde_json::to_string(event).expect("client events serialize");
        let _ = self.socket.send_with_str(&json);
    }
}

pub const INVALID_CODE: &str = "invalid or expired call code";

pub async fn exists(code: &str) -> Result<bool> {
    let response: Response = JsFuture::from(window().fetch_with_str(&format!("/rooms/{code}")))
        .await
        .map_err(js)?
        .unchecked_into();
    Ok(response.status() != 404)
}

pub async fn connect(
    code: &str,
    name: &str,
) -> Result<(Welcome, Signal, mpsc::UnboundedReceiver<ServerEvent>)> {
    if !exists(code).await? {
        bail!(INVALID_CODE);
    }
    let origin = window().location().origin().map_err(js)?;

    let mut url = url::Url::parse(&origin)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| anyhow!("cannot reach the server over a WebSocket"))?;
    url.set_path(&format!("/rooms/{code}/ws"));
    url.query_pairs_mut().append_pair("name", name);
    let socket = WebSocket::new(url.as_str()).map_err(js)?;

    let (tx, mut events) = mpsc::unbounded();
    let on_message = {
        let tx = tx.clone();
        Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
            let Some(text) = message.data().as_string() else {
                return;
            };
            if let Ok(event) = serde_json::from_str::<ServerEvent>(&text) {
                let _ = tx.unbounded_send(event);
            }
        })
    };
    let on_close = Closure::<dyn FnMut()>::new(move || tx.close_channel());
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

    let welcome = match events.next().await {
        Some(ServerEvent::Welcome {
            you,
            token,
            media,
            participants,
        }) => Welcome {
            me: you,
            token,
            media,
            participants,
        },
        Some(ServerEvent::Error { message }) => bail!(message),
        Some(_) => bail!("unexpected first message from the server"),
        None => bail!("cannot reach the server"),
    };
    let signal = Signal {
        socket,
        _on_message: on_message,
        _on_close: on_close,
    };
    Ok((welcome, signal, events))
}
