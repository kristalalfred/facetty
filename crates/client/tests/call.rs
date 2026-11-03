use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use facetty::capture::Source;
use facetty::publisher::{Outbox, Publisher};
use facetty::session::{self, Command, Event, Session};
use facetty_audio::{Engine, Input, Options, Output};
use facetty_proto::ladder;
use tokio::time::timeout;

const SECRET: &[u8] = b"test-secret";
const HOST_KEY: &str = "test-host-key";
const WAIT: Duration = Duration::from_secs(10);

async fn start_servers() -> String {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sfu = Arc::new(facetty_sfu::Sfu::bind("127.0.0.1:0".parse().unwrap(), SECRET).unwrap());
    let signal = facetty_signal::Signal::new(
        SECRET,
        facetty_signal::Config {
            host_key: Some(HOST_KEY.as_bytes().to_vec()),
            max_calls: 100,
            max_participants: 100,
            media_addr: format!(":{}", sfu.local_addr().unwrap().port()),
            media_cert: sfu.certificate(),
        },
    );
    let roster = signal.clone();
    tokio::spawn(async move { sfu.run(move |room, id| roster.departure(room, id)).await });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, facetty_signal::router(signal))
            .await
            .unwrap();
    });
    url
}

async fn next_matching<T>(session: &mut Session, mut f: impl FnMut(Event) -> Option<T>) -> T {
    timeout(WAIT, async {
        loop {
            let event = session.events.recv().await.expect("session ended");
            if let Some(found) = f(event) {
                return found;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

/// 440 Hz sine as 48 kHz mono s16le, forever.
struct Tone(u64);

impl Read for Tone {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        for chunk in buf.as_chunks_mut::<2>().0 {
            let t = self.0 as f32 / 48_000.0;
            let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            *chunk = v.to_le_bytes();
            self.0 += 1;
        }
        Ok(buf.len() / 2 * 2)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invited_callers_see_hear_chat_and_react_with_other_calls_isolated() {
    let server = start_servers().await;
    let code = session::create_call(&server, Some(HOST_KEY))
        .await
        .unwrap()
        .code;

    let alice_video = Outbox::default();
    let mut alice = session::connect(&server, &code, "alice", alice_video.clone())
        .await
        .unwrap();
    let alice_publisher = Publisher::start(Source::Test, 30, alice_video, true);
    let _alice_audio = Engine::start(
        Options {
            input: Input::Pcm(Box::new(Tone(0))),
            output: Output::None,
        },
        session::audio_sender(alice.connection.clone()),
    )
    .unwrap();

    let mut bob = session::connect(&server, &code, "bob", Outbox::default())
        .await
        .unwrap();
    assert_eq!(bob.participants.len(), 1);
    assert_eq!(bob.participants[0].name, "alice");
    let alice_id = alice.me.id;

    let other_code = session::create_call(&server, Some(HOST_KEY))
        .await
        .unwrap()
        .code;
    assert_ne!(other_code, code);
    let mut carol = session::connect(&server, &other_code, "carol", Outbox::default())
        .await
        .unwrap();
    assert!(carol.participants.is_empty());
    let roster: Vec<facetty_proto::signal::Participant> = reqwest::get(format!(
        "{server}/rooms/{}",
        other_code.to_ascii_uppercase()
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(roster, vec![carol.me.clone()]);

    let bob_name = next_matching(&mut alice, |e| match e {
        Event::Joined(p) => Some(p.name),
        _ => None,
    })
    .await;
    assert_eq!(bob_name, "bob");

    let rung = ladder::best_fit(80, 24).unwrap();
    carol
        .commands
        .send(Command::Subscribe {
            publisher: alice_id,
            rung: Some(rung),
        })
        .unwrap();
    bob.commands
        .send(Command::Subscribe {
            publisher: alice_id,
            rung: Some(rung),
        })
        .unwrap();
    let wanted = next_matching(&mut alice, |e| match e {
        Event::EncodeRungs(r) => Some(r),
        _ => None,
    })
    .await;
    assert_eq!(wanted, vec![rung]);
    alice_publisher.set_rungs(wanted);

    let (publisher, frame) = next_matching(&mut bob, |e| match e {
        Event::Video {
            publisher, frame, ..
        } => Some((publisher, frame)),
        _ => None,
    })
    .await;
    let size = ladder::size(rung).unwrap();
    assert_eq!(publisher, alice_id);
    assert_eq!((frame.cols, frame.rows), (size.cols, size.rows));
    assert!(frame.cells.iter().any(|c| c.glyph != 0), "frame is blank");

    let bob_audio = Arc::new(
        Engine::start(
            Options {
                input: Input::None,
                output: Output::None,
            },
            |_, _| {},
        )
        .unwrap(),
    );
    tokio::spawn(session::receive_audio(
        bob.connection.clone(),
        bob_audio.clone(),
    ));
    timeout(WAIT, async {
        while bob_audio.level(alice_id) < 0.01 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bob never heard alice");

    alice.commands.send(Command::Chat("hi bob".into())).unwrap();
    let (name, text) = next_matching(&mut bob, |e| match e {
        Event::Chat { name, text } => Some((name, text)),
        _ => None,
    })
    .await;
    assert_eq!((name.as_str(), text.as_str()), ("alice", "hi bob"));

    alice.commands.send(Command::React("hi".into())).unwrap();
    alice.commands.send(Command::React("🎉".into())).unwrap();
    let reaction = next_matching(&mut bob, |e| match e {
        Event::Reaction { from, emoji } => Some((from, emoji)),
        _ => None,
    })
    .await;
    assert_eq!(reaction, (alice_id, "🎉".to_string()));

    assert!(
        timeout(Duration::from_millis(200), carol.events.recv())
            .await
            .is_err(),
        "another call received signaling or video"
    );
    assert!(
        timeout(Duration::from_millis(200), carol.connection.read_datagram())
            .await
            .is_err(),
        "another call received audio"
    );

    bob.commands
        .send(Command::Subscribe {
            publisher: alice_id,
            rung: None,
        })
        .unwrap();
    let wanted = next_matching(&mut alice, |e| match e {
        Event::EncodeRungs(r) => Some(r),
        _ => None,
    })
    .await;
    assert!(wanted.is_empty());

    alice.close().await;
    drop(alice);
    let left = next_matching(&mut bob, |e| match e {
        Event::Left(id) => Some(id),
        _ => None,
    })
    .await;
    assert_eq!(left, alice_id);
    bob.close().await;
    carol.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn creation_needs_a_host_key_and_joining_needs_an_existing_code() {
    let server = start_servers().await;
    let response = reqwest::Client::new()
        .post(format!("{server}/rooms"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let error = session::create_call(&server, Some("wrong"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("host key rejected"));
    let error = session::create_call(&server, None).await.unwrap_err();
    assert!(error.to_string().contains("needs a host key"));
    let response = reqwest::get(format!("{server}/rooms/lobby")).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    let error = match session::connect(&server, "lobby", "guest", Outbox::default()).await {
        Ok(_) => panic!("room names must not create calls"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid or expired call code"));

    let code = session::create_call(&server, Some(HOST_KEY))
        .await
        .unwrap()
        .code;
    let session = session::connect(
        &server,
        &code.to_ascii_uppercase(),
        "guest",
        Outbox::default(),
    )
    .await
    .unwrap();
    assert!(session.participants.is_empty());
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_subscriber_gets_the_publisher_to_send_a_full_frame() {
    let server = start_servers().await;
    let code = session::create_call(&server, Some(HOST_KEY))
        .await
        .unwrap()
        .code;
    let alice_video = Outbox::default();
    let mut alice = session::connect(&server, &code, "alice", alice_video.clone())
        .await
        .unwrap();
    let alice_publisher = Publisher::start(Source::Test, 30, alice_video, true);
    let rung = ladder::best_fit(80, 24).unwrap();

    let mut viewers = Vec::new();
    for name in ["bob", "dave"] {
        let mut viewer = session::connect(&server, &code, name, Outbox::default())
            .await
            .unwrap();
        viewer
            .commands
            .send(Command::Subscribe {
                publisher: alice.me.id,
                rung: Some(rung),
            })
            .unwrap();
        let first_viewer = viewers.is_empty();
        let request = next_matching(&mut alice, |e| match e {
            Event::EncodeRungs(r) if first_viewer => Some(Event::EncodeRungs(r)),
            Event::Refresh(r) if !first_viewer => Some(Event::Refresh(r)),
            _ => None,
        })
        .await;
        match request {
            Event::EncodeRungs(r) => alice_publisher.set_rungs(r),
            Event::Refresh(r) => {
                assert_eq!(r, rung);
                alice_publisher.refresh(r);
            }
            _ => unreachable!(),
        }
        let frame = next_matching(&mut viewer, |e| match e {
            Event::Video { frame, .. } => Some(frame),
            _ => None,
        })
        .await;
        assert!(frame.cells.iter().any(|c| c.glyph != 0), "frame is blank");
        viewers.push(viewer);
    }
    for viewer in viewers {
        viewer.close().await;
    }
    alice.close().await;
}

/// Claude: joins the way the browser client does: signaling over the
/// WebSocket, then media in a WebTransport session that pins the
/// certificate hash.
async fn join_over_webtransport(
    server: &str,
    code: &str,
) -> (
    facetty_proto::signal::Participant,
    web_transport_quinn::Session,
    web_transport_quinn::SendStream,
    web_transport_quinn::RecvStream,
) {
    use facetty_proto::media::{ClientControl, ServerControl};
    use facetty_proto::quic;
    use facetty_proto::signal::ServerEvent;
    use futures_util::StreamExt;

    let url = session::ws_url(server, code, "web").unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .unwrap();
    let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(welcome))) = ws.next().await else {
        panic!("no welcome");
    };
    let ServerEvent::Welcome {
        you, token, media, ..
    } = serde_json::from_str(&welcome).unwrap()
    else {
        panic!("unexpected first event");
    };
    tokio::spawn(async move { while ws.next().await.is_some() {} });

    let client = web_transport_quinn::ClientBuilder::new()
        .with_server_certificate_hashes(vec![hex::decode(&media.cert_sha256).unwrap()])
        .unwrap();
    let url = url::Url::parse(&format!("https://{}/", media.addr)).unwrap();
    let session = client.connect(url).await.unwrap();
    let (mut control, mut replies) = session.open_bi().await.unwrap();
    quic::write_control(&mut control, &ClientControl::Hello { token })
        .await
        .unwrap();
    let reply = quic::read_control::<_, ServerControl>(&mut replies)
        .await
        .unwrap();
    assert_eq!(
        reply,
        Some(ServerControl::Welcome {
            participant: you.id
        })
    );
    (you, session, control, replies)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_webtransport_guest_and_a_native_caller_see_and_hear_each_other() {
    use facetty_ascii::{Cell, Decoder, Encoder, Frame};
    use facetty_proto::media::{self, AudioPacket, ClientControl, ServerControl, VideoFrame};
    use facetty_proto::quic;

    let server = start_servers().await;
    let code = session::create_call(&server, Some(HOST_KEY))
        .await
        .unwrap()
        .code;
    let alice_video = Outbox::default();
    let mut alice = session::connect(&server, &code, "alice", alice_video.clone())
        .await
        .unwrap();
    let alice_publisher = Publisher::start(Source::Test, 30, alice_video, true);
    let _alice_mic = Engine::start(
        Options {
            input: Input::Pcm(Box::new(Tone(0))),
            output: Output::None,
        },
        session::audio_sender(alice.connection.clone()),
    )
    .unwrap();
    let alice_ears = Arc::new(
        Engine::start(
            Options {
                input: Input::None,
                output: Output::None,
            },
            |_, _| {},
        )
        .unwrap(),
    );
    tokio::spawn(session::receive_audio(
        alice.connection.clone(),
        alice_ears.clone(),
    ));

    let (guest, web, mut control, mut replies) = join_over_webtransport(&server, &code).await;
    let rung = ladder::best_fit(80, 24).unwrap();
    quic::write_control(
        &mut control,
        &ClientControl::Subscribe {
            publisher: alice.me.id,
            rung: Some(rung),
        },
    )
    .await
    .unwrap();
    let wanted = next_matching(&mut alice, |e| match e {
        Event::EncodeRungs(r) => Some(r),
        _ => None,
    })
    .await;
    alice_publisher.set_rungs(wanted);

    let mut stream = timeout(WAIT, web.accept_uni()).await.unwrap().unwrap();
    let bytes = stream.read_to_end(media::MAX_VIDEO_FRAME).await.unwrap();
    let frame: VideoFrame = media::decode(&bytes).unwrap();
    assert_eq!(frame.publisher, alice.me.id);
    let picture = Decoder::default()
        .decode(frame.seq, &frame.payload)
        .unwrap();
    assert!(picture.cells.iter().any(|c| c.glyph != 0), "frame is blank");

    let heard = timeout(WAIT, web.read_datagram()).await.unwrap().unwrap();
    let packet: AudioPacket = media::decode(&heard).unwrap();
    assert_eq!(packet.publisher, alice.me.id);

    alice
        .commands
        .send(Command::Subscribe {
            publisher: guest.id,
            rung: Some(rung),
        })
        .unwrap();
    let request = timeout(WAIT, quic::read_control::<_, ServerControl>(&mut replies))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        request,
        Some(ServerControl::EncodeRungs { rungs: vec![rung] })
    );
    let size = ladder::size(rung).unwrap();
    let mut picture = Frame::blank(size.cols, size.rows);
    picture.cells.fill(Cell {
        glyph: 8,
        rgb: [200, 120, 40],
    });
    let payload = Encoder::default().encode(&picture);
    let sent = media::encode(&VideoFrame {
        publisher: 0,
        rung,
        seq: 1,
        payload,
    });
    let mut out = web.open_uni().await.unwrap();
    out.write_all(&sent).await.unwrap();
    out.finish().unwrap();
    let shown = next_matching(&mut alice, |e| match e {
        Event::Video {
            publisher, frame, ..
        } if publisher == guest.id => Some(frame),
        _ => None,
    })
    .await;
    assert_eq!(shown.cells[0].glyph, 8);

    let mut seq = 0u16;
    timeout(WAIT, async {
        while alice_ears.level(guest.id) < 0.01 {
            let mut pcm = [0f32; facetty_audio::FRAME_SAMPLES];
            for (i, s) in pcm.iter_mut().enumerate() {
                *s = 0.3 * (i as f32 * 0.07).sin();
            }
            let opus = facetty_audio::opus::Encoder::new()
                .unwrap()
                .encode(&pcm)
                .unwrap();
            let packet = AudioPacket {
                publisher: 0,
                seq,
                payload: opus,
            };
            web.send_datagram(media::encode(&packet).into()).unwrap();
            seq = seq.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("alice never heard the guest");

    web.close(0, b"bye");
    alice.close().await;
}
