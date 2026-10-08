use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use bits::capture::Source;
use bits::publisher::Publisher;
use bits::session::{self, Command, Event, Session};
use bits_audio::{Engine, Input, Options, Output};
use bits_proto::ladder;
use bits_proto::signal::{MediaServer, SfuHeartbeat};
use tokio::sync::mpsc;
use tokio::time::timeout;

const SECRET: &[u8] = b"test-secret";
const WAIT: Duration = Duration::from_secs(10);

async fn start_servers() -> String {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let signal = bits_signal::Signal::new(SECRET, None);
    let sfu = Arc::new(bits_sfu::Sfu::bind("127.0.0.1:0".parse().unwrap(), SECRET).unwrap());
    signal.register_sfu(SfuHeartbeat {
        id: "test".into(),
        media: MediaServer {
            addr: format!(":{}", sfu.local_addr().unwrap().port()),
            cert_sha256: sfu.cert_sha256().to_string(),
        },
        connections: 0,
    });
    tokio::spawn(async move { sfu.run().await });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, bits_signal::router(signal))
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
        for chunk in buf.chunks_exact_mut(2) {
            let t = self.0 as f32 / 48_000.0;
            let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            chunk.copy_from_slice(&v.to_le_bytes());
            self.0 += 1;
        }
        Ok(buf.len() / 2 * 2)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_people_see_hear_chat_and_react() {
    let server = start_servers().await;

    let (alice_video, alice_video_rx) = mpsc::channel(4);
    let mut alice = session::connect(&server, "room", "alice", alice_video_rx)
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

    let (_bob_video, bob_video_rx) = mpsc::channel(4);
    let mut bob = session::connect(&server, "room", "bob", bob_video_rx)
        .await
        .unwrap();
    assert_eq!(bob.participants.len(), 1);
    assert_eq!(bob.participants[0].name, "alice");
    let alice_id = alice.me.id;

    let bob_name = next_matching(&mut alice, |e| match e {
        Event::Joined(p) => Some(p.name),
        _ => None,
    })
    .await;
    assert_eq!(bob_name, "bob");

    let rung = ladder::best_fit(80, 24).unwrap();
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
}
