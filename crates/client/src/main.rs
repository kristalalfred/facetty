use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use bits_audio::{Engine, Input, Options, Output};
use bits_proto::signal::Participant;
use clap::{Args, Parser, Subcommand};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tracing::info;

use bits::app::{App, Setup};
use bits::camera;
use bits::capture::Source;
use bits::publisher::Publisher;
use bits::session::{self, Event};

/// Video calls in your terminal, drawn in ASCII.
#[derive(Parser)]
#[command(name = "bits", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a call and print its invitation code.
    Create {
        #[arg(long, env = "BITS_SERVER", default_value = "http://127.0.0.1:8740")]
        server: String,
        /// Server host key for creating calls.
        #[arg(long, env = "BITS_HOST_KEY", hide_env_values = true)]
        host_key: String,
    },
    /// Join a call using its invitation code.
    Join {
        code: String,
        #[command(flatten)]
        conn: ConnArgs,
        #[command(flatten)]
        video: VideoArgs,
        /// Join with the camera off.
        #[arg(long)]
        no_video: bool,
        /// Do not open any audio device.
        #[arg(long)]
        no_audio: bool,
        /// Microphone name (or part of it).
        #[arg(long)]
        mic: Option<String>,
        /// Speaker name (or part of it).
        #[arg(long)]
        speaker: Option<String>,
    },
    /// Join headless and publish a test pattern, a file, or a stream.
    Bot {
        code: String,
        #[command(flatten)]
        conn: ConnArgs,
        #[command(flatten)]
        video: VideoArgs,
        /// Audio to publish: "tone", or a file or URL that ffmpeg can open.
        #[arg(long)]
        audio: Option<String>,
    },
    /// Show yourself in ASCII without joining a call.
    Preview {
        #[command(flatten)]
        video: VideoArgs,
    },
    /// List cameras and audio devices.
    Devices,
}

#[derive(Args)]
struct ConnArgs {
    /// Signaling server URL.
    #[arg(long, env = "BITS_SERVER", default_value = "http://127.0.0.1:8740")]
    server: String,
    #[arg(long, env = "BITS_NAME")]
    name: Option<String>,
}

#[derive(Args)]
struct VideoArgs {
    /// "camera", "camera:<index or name>", "test", or a file or URL that ffmpeg can open (srt://, rtmp://, ...).
    #[arg(long, default_value = "camera")]
    video: String,
    #[arg(long, default_value_t = 15)]
    fps: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    match Cli::parse().command {
        Cmd::Create { server, host_key } => {
            let invite = session::create_call(&server, &host_key).await?;
            println!("{}", invite.code);
            Ok(())
        }
        Cmd::Join {
            code,
            conn,
            video,
            no_video,
            no_audio,
            mic,
            speaker,
        } => {
            log_to_file()?;
            let audio = (!no_audio).then(|| Options {
                input: Input::Device(mic),
                output: Output::Device(speaker),
            });
            join(code.to_ascii_lowercase(), conn, video, !no_video, audio).await
        }
        Cmd::Bot {
            code,
            conn,
            video,
            audio,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter())
                .with_writer(std::io::stderr)
                .init();
            bot(code.to_ascii_lowercase(), conn, video, audio).await
        }
        Cmd::Preview { video } => {
            log_to_file()?;
            preview(video).await
        }
        Cmd::Devices => devices(),
    }
}

async fn join(
    room: String,
    conn: ConnArgs,
    video: VideoArgs,
    video_on: bool,
    audio: Option<Options>,
) -> Result<()> {
    let name = conn.name.unwrap_or_else(default_name);
    eprintln!("joining {room} on {} as {name}...", conn.server);
    let (video_tx, video_rx) = mpsc::channel(4);
    let session = session::connect(&conn.server, &room, &name, video_rx).await?;
    let source = Source::parse(&video.video);
    let mirror_self = matches!(source, Source::Camera(_));
    let publisher = Publisher::start(source, video.fps, video_tx, video_on);

    let (engine, notice) = match audio {
        None => (None, None),
        Some(opts) => {
            match Engine::start(opts, session::audio_sender(session.connection.clone())) {
                Ok(engine) => {
                    let problem = engine.device_problem();
                    (Some(Arc::new(engine)), problem)
                }
                Err(e) => (None, Some(format!("audio off: {e:#}"))),
            }
        }
    };
    if let Some(engine) = &engine {
        tokio::spawn(session::receive_audio(
            session.connection.clone(),
            engine.clone(),
        ));
    }

    let mut me = session.me.clone();
    me.video_off = !video_on;
    let app = App::new(Setup {
        room,
        me,
        others: session.participants.clone(),
        publisher,
        audio: engine,
        commands: Some(session.commands.clone()),
        notice,
        mirror_self,
    });
    let mut terminal = init_terminal();
    let mut session = session;
    let events = std::mem::replace(&mut session.events, mpsc::unbounded_channel().1);
    let result = app.run(&mut terminal, Some(events)).await;
    restore_terminal();
    session.close().await;
    if let Some(reason) = result? {
        eprintln!("disconnected: {reason}");
    }
    Ok(())
}

fn init_terminal() -> DefaultTerminal {
    let terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release_mouse_and_paste();
        hook(info);
    }));
    terminal
}

fn restore_terminal() {
    release_mouse_and_paste();
    ratatui::restore();
}

fn release_mouse_and_paste() {
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
}

async fn preview(video: VideoArgs) -> Result<()> {
    let (video_tx, _video_rx) = mpsc::channel(1);
    let source = Source::parse(&video.video);
    let mirror_self = matches!(source, Source::Camera(_));
    let publisher = Publisher::start(source, video.fps, video_tx, true);
    let app = App::new(Setup {
        room: "preview".into(),
        me: Participant {
            id: 0,
            name: default_name(),
            audio_muted: true,
            video_off: false,
        },
        others: Vec::new(),
        publisher,
        audio: None,
        commands: None,
        notice: None,
        mirror_self,
    });
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal, None).await;
    ratatui::restore();
    result.map(|_| ())
}

async fn bot(room: String, conn: ConnArgs, video: VideoArgs, audio: Option<String>) -> Result<()> {
    let name = conn.name.unwrap_or_else(|| "bot".into());
    let (video_tx, video_rx) = mpsc::channel(4);
    let mut session = session::connect(&conn.server, &room, &name, video_rx).await?;
    info!(
        room,
        id = session.me.id,
        others = session.participants.len(),
        "joined"
    );
    let publisher = Publisher::start(Source::parse(&video.video), video.fps, video_tx, true);

    let mut pcm = None;
    let input = match audio.as_deref() {
        None => Input::None,
        Some("tone") => Input::Tone,
        Some(source) => {
            let child = spawn_pcm(source)?;
            let stdout = child.lock().unwrap().stdout.take();
            pcm = Some(child);
            Input::Pcm(Box::new(stdout.context("ffmpeg stdout")?))
        }
    };
    let _engine = Engine::start(
        Options {
            input,
            output: Output::None,
        },
        session::audio_sender(session.connection.clone()),
    )?;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            event = session.events.recv() => match event {
                Some(Event::EncodeRungs(rungs)) => {
                    info!(?rungs, "subscribers want");
                    publisher.set_rungs(rungs);
                }
                Some(Event::Joined(p)) => info!(name = p.name, id = p.id, "joined"),
                Some(Event::Left(id)) => info!(id, "left"),
                Some(Event::Chat { name, text }) => info!("{name}: {text}"),
                Some(Event::Reaction { from, emoji }) => info!(from, "{emoji}"),
                Some(Event::Closed(reason)) => {
                    info!(reason, "disconnected");
                    break;
                }
                Some(_) => {}
                None => break,
            },
        }
        if let Some(e) = publisher.error() {
            anyhow::bail!(e);
        }
    }
    if let Some(child) = pcm {
        let _ = child.lock().unwrap().kill();
    }
    session.close().await;
    Ok(())
}

fn spawn_pcm(spec: &str) -> Result<Arc<Mutex<Child>>> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    match spec {
        url if url.contains("://") => cmd.args(["-i", url]),
        path => cmd.args(["-stream_loop", "-1", "-i", path]),
    };
    cmd.args(["-vn", "-f", "s16le", "-ac", "1", "-ar", "48000", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let child = cmd.spawn().context("starting ffmpeg for audio")?;
    Ok(Arc::new(Mutex::new(child)))
}

fn devices() -> Result<()> {
    println!("cameras (use --video camera:<index>):");
    let cameras = camera::list();
    if cameras.is_empty() {
        println!("  none found");
    }
    for (i, name) in cameras.iter().enumerate() {
        println!("  [{i}] {name}");
    }
    let (inputs, outputs) = bits_audio::list_devices()?;
    println!("microphones (use --mic <name>):");
    for d in inputs {
        println!("  {d}");
    }
    println!("speakers (use --speaker <name>):");
    for d in outputs {
        println!("  {d}");
    }
    Ok(())
}

fn default_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "guest".into())
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())
}

fn log_to_file() -> Result<()> {
    let path = std::env::temp_dir().join("bits.log");
    let file = std::fs::File::create(&path)
        .with_context(|| format!("creating log file {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
    Ok(())
}
