//! Connection to a call: the signaling WebSocket plus the QUIC media
//! connection to the SFU it points at.

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use facetty_ascii::{Decoder, Frame};
use facetty_proto::ladder::Rung;
use facetty_proto::media::{
    self, AudioPacket, ClientControl, ServerControl, VideoFrame, VideoSender,
};
use facetty_proto::signal::{CallInvite, ClientEvent, MediaServer, Participant, ServerEvent};
use facetty_proto::{ALPN, ParticipantId};
use futures_util::{SinkExt, StreamExt};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, ring};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::publisher::Outbox;

pub enum Event {
    Joined(Participant),
    Left(ParticipantId),
    Updated(Participant),
    Chat {
        name: String,
        text: String,
    },
    Reaction {
        from: ParticipantId,
        emoji: String,
    },
    /// The publisher's picture after applying frame `seq`. Frames are
    /// applied out of order, so the newest picture is the last event, not the
    /// one with the highest `seq`.
    Video {
        publisher: ParticipantId,
        seq: u32,
        frame: Arc<Frame>,
    },
    EncodeRungs(Vec<Rung>),
    /// Send this rung's next frame in full.
    Refresh(Rung),
    Closed(String),
}

pub enum Command {
    SetState {
        audio_muted: bool,
        video_off: bool,
    },
    Chat(String),
    React(String),
    Subscribe {
        publisher: ParticipantId,
        rung: Option<Rung>,
    },
}

pub struct Session {
    pub me: Participant,
    pub participants: Vec<Participant>,
    pub events: mpsc::UnboundedReceiver<Event>,
    pub commands: mpsc::UnboundedSender<Command>,
    pub connection: quinn::Connection,
    endpoint: quinn::Endpoint,
}

impl Session {
    /// Sends the QUIC close and waits briefly so the SFU sees it, instead of
    /// learning about it from its idle timeout.
    pub async fn close(&self) {
        self.connection.close(0u32.into(), b"bye");
        let _ = tokio::time::timeout(Duration::from_secs(1), self.endpoint.wait_idle()).await;
    }
}

pub fn ws_url(server: &str, room: &str, name: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(server).with_context(|| format!("bad server URL {server}"))?;
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => bail!("unsupported scheme {other}"),
    };
    url.set_scheme(scheme).expect("ws schemes are valid");
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("server URL cannot have a path"))?
        .pop_if_empty()
        .extend(["rooms", room, "ws"]);
    url.query_pairs_mut().append_pair("name", name);
    Ok(url)
}

/// `host_key` can be `None` on servers that let anyone create calls.
pub async fn create_call(server: &str, host_key: Option<&str>) -> Result<CallInvite> {
    let mut url = url::Url::parse(server).with_context(|| format!("bad server URL {server}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("creating a call requires an http:// or https:// server URL");
    }
    if host_key.is_some_and(str::is_empty) {
        bail!("host key cannot be empty");
    }
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("server URL cannot have a path"))?
        .pop_if_empty()
        .push("rooms");
    let mut request = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?
        .post(url);
    if let Some(key) = host_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.context("creating a call")?;
    match response.status() {
        reqwest::StatusCode::UNAUTHORIZED if host_key.is_none() => {
            bail!("this server needs a host key; pass --host-key or set FACETTY_HOST_KEY")
        }
        reqwest::StatusCode::UNAUTHORIZED => {
            bail!("host key rejected; check FACETTY_HOST_KEY or --host-key")
        }
        reqwest::StatusCode::SERVICE_UNAVAILABLE => {
            bail!("every call on this server is in use; try again later")
        }
        _ => {}
    }
    Ok(response.error_for_status()?.json().await?)
}

pub async fn connect(server: &str, room: &str, name: &str, video_out: Outbox) -> Result<Session> {
    let url = ws_url(server, &room.to_ascii_lowercase(), name)?;
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .map_err(|e| match &e {
            tokio_tungstenite::tungstenite::Error::Http(response)
                if response.status().as_u16() == 404 =>
            {
                anyhow::anyhow!("invalid or expired call code")
            }
            _ => anyhow::Error::new(e).context("connecting to signaling server"),
        })?;

    let welcome = loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => break serde_json::from_str::<ServerEvent>(&text)?,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => bail!("signaling server closed the connection"),
        }
    };
    let (me, token, media_server, participants) = match welcome {
        ServerEvent::Welcome {
            you,
            token,
            media,
            participants,
        } => (you, token, media, participants),
        ServerEvent::Error { message } => bail!("signaling server: {message}"),
        other => bail!("unexpected first message {other:?}"),
    };

    let (endpoint, connection) = connect_media(&media_server).await?;
    let (mut control_send, mut control_recv) = connection.open_bi().await?;
    media::write_control(&mut control_send, &ClientControl::Hello { token }).await?;
    match media::read_control::<_, ServerControl>(&mut control_recv).await? {
        Some(ServerControl::Welcome { .. }) => {}
        Some(ServerControl::Error { message }) => bail!("media server: {message}"),
        other => bail!("unexpected media server reply {other:?}"),
    }

    let (events_tx, events) = mpsc::unbounded_channel();
    let (commands, mut commands_rx) = mpsc::unbounded_channel::<Command>();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ClientControl>();
    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<ClientEvent>();

    tokio::spawn(async move {
        while let Some(cmd) = commands_rx.recv().await {
            match cmd {
                Command::SetState {
                    audio_muted,
                    video_off,
                } => {
                    let _ = ws_tx.send(ClientEvent::SetState {
                        audio_muted,
                        video_off,
                    });
                }
                Command::Chat(text) => {
                    let _ = ws_tx.send(ClientEvent::Chat { text });
                }
                Command::React(emoji) => {
                    let _ = ws_tx.send(ClientEvent::React { emoji });
                }
                Command::Subscribe { publisher, rung } => {
                    let _ = control_tx.send(ClientControl::Subscribe { publisher, rung });
                }
            }
        }
    });

    let events_ws = events_tx.clone();
    tokio::spawn(async move {
        let reason = loop {
            tokio::select! {
                out = ws_rx.recv() => {
                    let Some(event) = out else { break "client closed".to_string() };
                    let json = serde_json::to_string(&event).expect("client events serialize");
                    if let Err(e) = ws.send(Message::Text(json.into())).await {
                        break format!("signaling: {e}");
                    }
                }
                incoming = ws.next() => {
                    let text = match incoming {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(_))) | None => break "signaling server closed the connection".to_string(),
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => break format!("signaling: {e}"),
                    };
                    let event = match serde_json::from_str::<ServerEvent>(&text) {
                        Ok(e) => e,
                        Err(e) => { warn!("bad server event: {e}"); continue; }
                    };
                    let mapped = match event {
                        ServerEvent::Joined { participant } => Event::Joined(participant),
                        ServerEvent::Left { id } => Event::Left(id),
                        ServerEvent::Updated { participant } => Event::Updated(participant),
                        ServerEvent::Chat { name, text, .. } => Event::Chat { name, text },
                        ServerEvent::Reaction { from, emoji } => Event::Reaction { from, emoji },
                        ServerEvent::Error { message } => break message,
                        ServerEvent::Welcome { .. } => continue,
                    };
                    let _ = events_ws.send(mapped);
                }
            }
        };
        let _ = events_ws.send(Event::Closed(reason));
    });

    let events_control = events_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                out = control_rx.recv() => {
                    let Some(msg) = out else { break };
                    if media::write_control(&mut control_send, &msg).await.is_err() {
                        break;
                    }
                }
                incoming = media::read_control::<_, ServerControl>(&mut control_recv) => {
                    match incoming {
                        Ok(Some(ServerControl::EncodeRungs { rungs })) => {
                            let _ = events_control.send(Event::EncodeRungs(rungs));
                        }
                        Ok(Some(ServerControl::Refresh { rung })) => {
                            let _ = events_control.send(Event::Refresh(rung));
                        }
                        Ok(Some(ServerControl::Error { message })) => {
                            let _ = events_control.send(Event::Closed(format!("media server: {message}")));
                        }
                        Ok(Some(ServerControl::Welcome { .. })) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
            }
        }
    });

    tokio::spawn(receive_video(connection.clone(), events_tx.clone()));
    tokio::spawn(send_video(connection.clone(), video_out));

    let events_closed = events_tx;
    let watched = connection.clone();
    tokio::spawn(async move {
        let reason = watched.closed().await;
        let _ = events_closed.send(Event::Closed(format!("media connection: {reason}")));
    });

    Ok(Session {
        me,
        participants,
        events,
        commands,
        connection,
        endpoint,
    })
}

/// Feeds incoming audio datagrams to the engine until the connection ends.
pub async fn receive_audio(connection: quinn::Connection, audio: Arc<facetty_audio::Engine>) {
    while let Ok(datagram) = connection.read_datagram().await {
        if let Ok(packet) = media::decode::<AudioPacket>(&datagram) {
            audio.receive(packet.publisher, packet.seq, &packet.payload);
        }
    }
}

pub fn audio_sender(connection: quinn::Connection) -> impl FnMut(u16, Vec<u8>) + Send + 'static {
    late_audio_sender(Arc::new(OnceLock::from(connection)))
}

/// For audio that starts before the call is joined: drops packets until
/// `link` holds the connection.
pub fn late_audio_sender(
    link: Arc<OnceLock<quinn::Connection>>,
) -> impl FnMut(u16, Vec<u8>) + Send + 'static {
    move |seq, payload| {
        let Some(connection) = link.get() else {
            return;
        };
        let packet = AudioPacket {
            publisher: 0,
            seq,
            payload,
        };
        let _ = connection.send_datagram(media::encode(&packet).into());
    }
}

/// Frames arrive on concurrent streams, so one can overtake another; the
/// decoder sorts that out cell by cell. Events go out under the lock so they
/// leave in the order the decoder applied them.
async fn receive_video(connection: quinn::Connection, events: mpsc::UnboundedSender<Event>) {
    let decoders: Arc<Mutex<HashMap<ParticipantId, Decoder>>> = Arc::default();
    while let Ok(mut stream) = connection.accept_uni().await {
        let events = events.clone();
        let decoders = decoders.clone();
        tokio::spawn(async move {
            let Ok(bytes) = stream.read_to_end(media::MAX_VIDEO_FRAME).await else {
                return;
            };
            let Ok(msg) = media::decode::<VideoFrame>(&bytes) else {
                return;
            };
            let mut decoders = decoders.lock().unwrap();
            let decoder = decoders.entry(msg.publisher).or_default();
            match decoder.decode(msg.seq, &msg.payload) {
                Ok(frame) => {
                    let _ = events.send(Event::Video {
                        publisher: msg.publisher,
                        seq: msg.seq,
                        frame: Arc::new(frame),
                    });
                }
                Err(e) => debug!(publisher = msg.publisher, "bad video frame: {e}"),
            }
        });
    }
}

/// Takes a frame only once the connection has room, so the one sent is the
/// newest and the rest were folded into it.
async fn send_video(connection: quinn::Connection, outbox: Outbox) {
    let video = VideoSender::new(connection.clone());
    let send = async {
        loop {
            outbox.filled().await;
            video.ready().await;
            let Some(f) = outbox.take() else { continue };
            let frame = VideoFrame {
                publisher: 0,
                rung: f.rung,
                seq: f.seq,
                payload: f.payload,
            };
            if let Err(e) = video.send(&frame).await {
                debug!("video send failed: {e}");
            }
        }
    };
    tokio::select! {
        _ = connection.closed() => {}
        () = send => {}
    }
}

async fn connect_media(server: &MediaServer) -> Result<(quinn::Endpoint, quinn::Connection)> {
    let addr: SocketAddr = server
        .addr
        .to_socket_addrs()
        .with_context(|| format!("resolving media server {}", server.addr))?
        .min_by_key(|a| a.is_ipv6())
        .with_context(|| format!("no address for media server {}", server.addr))?;
    let pinned = hex::decode(&server.cert_sha256).context("bad certificate hash")?;

    let provider = Arc::new(ring::default_provider());
    let verifier = Arc::new(PinnedCert {
        sha256: pinned,
        algorithms: provider.signature_verification_algorithms,
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut transport = quinn::TransportConfig::default();
    transport
        .max_concurrent_uni_streams(1024u32.into())
        .max_idle_timeout(Some(Duration::from_secs(15).try_into()?))
        .keep_alive_interval(Some(Duration::from_secs(5)));
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(Arc::new(transport));

    let bind: SocketAddr = if addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(config);
    let connection = endpoint
        .connect(addr, "facetty-sfu")?
        .await
        .with_context(|| format!("connecting to media server {addr}"))?;
    Ok((endpoint, connection))
}

/// Accepts exactly the certificate whose SHA-256 the signaling server gave us.
#[derive(Debug)]
struct PinnedCert {
    sha256: Vec<u8>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Sha256::digest(end_entity.as_ref()).as_slice() == self.sha256.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "media server certificate does not match the pinned hash".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_ws_urls() {
        assert_eq!(
            ws_url("http://localhost:7000", "standup", "Ada L")
                .unwrap()
                .as_str(),
            "ws://localhost:7000/rooms/standup/ws?name=Ada+L"
        );
        assert_eq!(
            ws_url("https://example.com/", "x", "y").unwrap().as_str(),
            "wss://example.com/rooms/x/ws?name=y"
        );
    }
}
