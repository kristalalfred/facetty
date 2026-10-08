//! Call audio: microphone → Opus packets, and Opus packets from any number of
//! publishers → jitter buffers → mix → speakers.

mod device;
mod jitter;
mod mixer;
mod opus;
mod resample;

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::ErrorKind;
use cpal::traits::HostTrait;

use device::Capture;
use mixer::{Command, Mixer};

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;

const FRAME_DURATION: Duration = Duration::from_millis(20);
const MAX_CLOCK_LAG: Duration = Duration::from_millis(200);
/// Bounds memory if playback stalls (e.g. the output device disappeared).
const MAX_INBOX: usize = 2_000;
/// Lets a device settle after a rate change or reconnect before its stream
/// is rebuilt.
const REOPEN_DELAY: Duration = Duration::from_millis(250);
const RETRY_DELAY: Duration = Duration::from_secs(2);

pub enum Input {
    None,
    Device(Option<String>),
    /// 48 kHz mono s16le, consumed in real time.
    Pcm(Box<dyn Read + Send>),
    /// 440 Hz with a short 1760 Hz beep at the start of every second.
    Tone,
}

pub enum Output {
    None,
    Device(Option<String>),
}

pub struct Options {
    pub input: Input,
    pub output: Output,
}

type PacketSink = Box<dyn FnMut(u16, Vec<u8>) + Send>;

pub(crate) struct Shared {
    running: AtomicBool,
    muted: AtomicBool,
    local_level: AtomicU32,
    inbox: Mutex<Vec<Command>>,
    levels: Mutex<HashMap<u32, f32>>,
    device_problem: Mutex<Option<String>>,
}

pub struct Engine {
    shared: Arc<Shared>,
    device_thread: Option<DeviceThread>,
}

struct DeviceThread {
    signals: mpsc::Sender<DeviceSignal>,
    handle: JoinHandle<()>,
}

impl Engine {
    /// `on_packet(seq, opus)` runs on an engine thread once per 20 ms frame,
    /// except while muted.
    pub fn start(
        opts: Options,
        on_packet: impl FnMut(u16, Vec<u8>) + Send + 'static,
    ) -> Result<Engine> {
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            muted: AtomicBool::new(false),
            local_level: AtomicU32::new(0f32.to_bits()),
            inbox: Mutex::new(Vec::new()),
            levels: Mutex::new(HashMap::new()),
            device_problem: Mutex::new(None),
        });
        let sender = PacketSender::new(shared.clone(), Box::new(on_packet))?;
        let mut engine = Engine {
            shared: shared.clone(),
            device_thread: None,
        };

        let mut slots = Vec::new();
        match opts.input {
            Input::None => {}
            Input::Device(name) => {
                let capture = Arc::new(Capture {
                    samples: Mutex::new(VecDeque::new()),
                    ready: Condvar::new(),
                });
                slots.push(Slot::new(Role::Mic(capture.clone()), name));
                let shared = shared.clone();
                thread::Builder::new()
                    .name("bits-audio-encode".into())
                    .spawn(move || encode_capture(shared, capture, sender))?;
            }
            Input::Pcm(reader) => {
                let shared = shared.clone();
                thread::Builder::new()
                    .name("bits-audio-pcm".into())
                    .spawn(move || encode_pcm(shared, reader, sender))?;
            }
            Input::Tone => {
                let shared = shared.clone();
                thread::Builder::new()
                    .name("bits-audio-tone".into())
                    .spawn(move || encode_tone(shared, sender))?;
            }
        }
        match opts.output {
            Output::Device(name) => slots.push(Slot::new(Role::Speaker, name)),
            Output::None => {
                let mixer = Mixer::new(shared.clone());
                let shared = shared.clone();
                thread::Builder::new()
                    .name("bits-audio-mix".into())
                    .spawn(move || mix_headless(shared, mixer))?;
            }
        }
        if !slots.is_empty() {
            engine.device_thread = Some(DeviceThread::spawn(shared, slots)?);
        }
        Ok(engine)
    }

    /// Why the mic or speaker is not working right now, if it isn't. Failed
    /// devices are retried in the background.
    pub fn device_problem(&self) -> Option<String> {
        self.shared.device_problem.lock().unwrap().clone()
    }

    /// Never blocks on decoding; packets are handed to the playback thread.
    pub fn receive(&self, publisher: u32, seq: u16, payload: &[u8]) {
        let mut inbox = self.shared.inbox.lock().unwrap();
        if inbox.len() >= MAX_INBOX {
            inbox.drain(..MAX_INBOX / 2);
        }
        inbox.push(Command::Packet {
            publisher,
            seq,
            payload: payload.to_vec(),
        });
    }

    pub fn remove(&self, publisher: u32) {
        self.shared
            .inbox
            .lock()
            .unwrap()
            .push(Command::Remove(publisher));
        self.shared.levels.lock().unwrap().remove(&publisher);
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    pub fn level(&self, publisher: u32) -> f32 {
        self.shared
            .levels
            .lock()
            .unwrap()
            .get(&publisher)
            .copied()
            .unwrap_or(0.0)
    }

    /// Microphone level. Keeps metering while muted, so a UI can warn about
    /// talking into a muted mic.
    pub fn local_level(&self) -> f32 {
        f32::from_bits(self.shared.local_level.load(Ordering::Relaxed))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.device_thread.take() {
            let _ = thread.signals.send(DeviceSignal::Stop);
            let _ = thread.handle.join();
        }
    }
}

enum DeviceSignal {
    Stop,
    Failed {
        slot: usize,
        generation: u64,
        error: String,
    },
}

enum Role {
    Mic(Arc<Capture>),
    Speaker,
}

struct Slot {
    role: Role,
    name: Option<String>,
    stream: Option<cpal::Stream>,
    generation: u64,
    retry_at: Instant,
    problem: Option<String>,
}

impl Slot {
    fn new(role: Role, name: Option<String>) -> Self {
        Slot {
            role,
            name,
            stream: None,
            generation: 0,
            retry_at: Instant::now(),
            problem: None,
        }
    }

    fn label(&self) -> &'static str {
        match self.role {
            Role::Mic(_) => "mic",
            Role::Speaker => "speaker",
        }
    }

    fn open(&mut self, index: usize, shared: &Arc<Shared>, signals: &mpsc::Sender<DeviceSignal>) {
        self.generation += 1;
        let label = self.label();
        let generation = self.generation;
        let signals = signals.clone();
        let on_error = move |e: cpal::Error| {
            if matches!(
                e.kind(),
                ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
            ) {
                tracing::debug!("{label}: {e}");
            } else {
                let _ = signals.send(DeviceSignal::Failed {
                    slot: index,
                    generation,
                    error: e.to_string(),
                });
            }
        };
        let opened = device::find(self.name.as_deref(), matches!(self.role, Role::Mic(_)))
            .and_then(|device| {
                match &self.role {
                    Role::Mic(capture) => device::start_input(&device, capture.clone(), on_error),
                    Role::Speaker => {
                        device::start_output(&device, Mixer::new(shared.clone()), on_error)
                    }
                }
                .with_context(|| device::device_name(&device))
            });
        match opened {
            Ok(stream) => {
                if self.problem.take().is_some() {
                    tracing::info!("{label}: reopened");
                }
                self.stream = Some(stream);
            }
            Err(e) => {
                let problem = format!("{label}: {e:#}");
                if self.problem.as_ref() != Some(&problem) {
                    tracing::warn!("{problem}");
                }
                self.problem = Some(problem);
                self.retry_at = Instant::now() + RETRY_DELAY;
            }
        }
    }

    fn fail(&mut self, error: String) {
        if self.stream.take().is_some() {
            let problem = format!("{}: {error}", self.label());
            tracing::warn!("{problem}; reopening");
            self.problem = Some(problem);
            self.retry_at = Instant::now() + REOPEN_DELAY;
        }
    }
}

impl DeviceThread {
    /// cpal streams are not `Send` on every platform, so one thread builds
    /// them and keeps them alive until the engine drops. Returns once every
    /// device has been tried once.
    fn spawn(shared: Arc<Shared>, slots: Vec<Slot>) -> Result<DeviceThread> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (signals, signals_rx) = mpsc::channel();
        let signals_for_streams = signals.clone();
        let handle = thread::Builder::new()
            .name("bits-audio-device".into())
            .spawn(move || run_devices(shared, slots, signals_for_streams, signals_rx, ready_tx))?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("audio device thread exited during setup"))?;
        Ok(DeviceThread { signals, handle })
    }
}

fn run_devices(
    shared: Arc<Shared>,
    mut slots: Vec<Slot>,
    signals: mpsc::Sender<DeviceSignal>,
    signals_rx: mpsc::Receiver<DeviceSignal>,
    ready: mpsc::SyncSender<()>,
) {
    let mut ready = Some(ready);
    loop {
        let now = Instant::now();
        for (index, slot) in slots.iter_mut().enumerate() {
            if slot.stream.is_none() && slot.retry_at <= now {
                slot.open(index, &shared, &signals);
            }
        }
        let problems: Vec<&str> = slots.iter().filter_map(|s| s.problem.as_deref()).collect();
        *shared.device_problem.lock().unwrap() =
            (!problems.is_empty()).then(|| problems.join("; "));
        if let Some(ready) = ready.take() {
            let _ = ready.send(());
        }

        let next_retry = slots
            .iter()
            .filter(|s| s.stream.is_none())
            .map(|s| s.retry_at)
            .min();
        let signal = match next_retry {
            Some(at) => {
                match signals_rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(signal) => signal,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match signals_rx.recv() {
                Ok(signal) => signal,
                Err(_) => break,
            },
        };
        match signal {
            DeviceSignal::Stop => break,
            DeviceSignal::Failed {
                slot,
                generation,
                error,
            } => {
                let slot = &mut slots[slot];
                if slot.generation == generation {
                    slot.fail(error);
                }
            }
        }
    }
}

struct PacketSender {
    shared: Arc<Shared>,
    encoder: opus::Encoder,
    on_packet: PacketSink,
    seq: u16,
    level: f32,
}

impl PacketSender {
    fn new(shared: Arc<Shared>, on_packet: PacketSink) -> Result<Self> {
        Ok(PacketSender {
            shared,
            encoder: opus::Encoder::new()?,
            on_packet,
            seq: 0,
            level: 0.0,
        })
    }

    fn send(&mut self, frame: &[f32; FRAME_SAMPLES]) {
        self.level = mixer::smooth(self.level, mixer::loudness(frame));
        self.shared
            .local_level
            .store(self.level.to_bits(), Ordering::Relaxed);
        if self.shared.muted.load(Ordering::Relaxed) {
            return;
        }
        match self.encoder.encode(frame) {
            Ok(packet) => {
                (self.on_packet)(self.seq, packet);
                self.seq = self.seq.wrapping_add(1);
            }
            Err(e) => tracing::warn!("opus encode: {e}"),
        }
    }
}

fn encode_capture(shared: Arc<Shared>, capture: Arc<Capture>, mut sender: PacketSender) {
    let mut pending = Vec::new();
    while shared.running.load(Ordering::Relaxed) {
        {
            let mut samples = capture.samples.lock().unwrap();
            if samples.is_empty() {
                samples = capture
                    .ready
                    .wait_timeout(samples, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
            pending.extend(samples.drain(..));
        }
        let mut frames = pending.chunks_exact(FRAME_SAMPLES);
        for frame in &mut frames {
            sender.send(frame.try_into().unwrap());
        }
        let used = pending.len() - frames.remainder().len();
        pending.drain(..used);
    }
}

fn encode_pcm(shared: Arc<Shared>, mut reader: Box<dyn Read + Send>, mut sender: PacketSender) {
    let mut bytes = [0u8; FRAME_SAMPLES * 2];
    let mut frame = [0f32; FRAME_SAMPLES];
    let mut clock = Pacer::new();
    while shared.running.load(Ordering::Relaxed) {
        if reader.read_exact(&mut bytes).is_err() {
            break;
        }
        for (s, b) in frame.iter_mut().zip(bytes.chunks_exact(2)) {
            *s = i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0;
        }
        sender.send(&frame);
        clock.wait();
    }
}

fn encode_tone(shared: Arc<Shared>, mut sender: PacketSender) {
    let rate = SAMPLE_RATE as u64;
    let mut frame = [0f32; FRAME_SAMPLES];
    let mut clock = Pacer::new();
    let mut n = 0u64;
    while shared.running.load(Ordering::Relaxed) {
        for s in &mut frame {
            let hz = if n % rate < rate / 25 { 1760.0 } else { 440.0 };
            let t = n as f64 / rate as f64;
            *s = (0.125 * (std::f64::consts::TAU * hz * t).sin()) as f32;
            n += 1;
        }
        sender.send(&frame);
        clock.wait();
    }
}

fn mix_headless(shared: Arc<Shared>, mut mixer: Mixer) {
    let mut frame = [0f32; FRAME_SAMPLES];
    let mut clock = Pacer::new();
    while shared.running.load(Ordering::Relaxed) {
        mixer.mix(&mut frame);
        clock.wait();
    }
}

struct Pacer {
    next: Instant,
}

impl Pacer {
    fn new() -> Self {
        Pacer {
            next: Instant::now(),
        }
    }

    fn wait(&mut self) {
        self.next += FRAME_DURATION;
        let now = Instant::now();
        if self.next > now {
            thread::sleep(self.next - now);
        } else if now - self.next > MAX_CLOCK_LAG {
            self.next = now;
        }
    }
}

/// Names of the available (input, output) devices.
pub fn list_devices() -> Result<(Vec<String>, Vec<String>)> {
    let host = cpal::default_host();
    let inputs = host
        .input_devices()?
        .map(|d| device::device_name(&d))
        .collect();
    let outputs = host
        .output_devices()?
        .map(|d| device::device_name(&d))
        .collect();
    Ok((inputs, outputs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_pcm(seconds: f32) -> Vec<u8> {
        let n = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..n)
            .flat_map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let s = (0.3 * (2.0 * std::f32::consts::PI * 330.0 * t).sin() * 32_767.0) as i16;
                s.to_le_bytes()
            })
            .collect()
    }

    #[test]
    fn engine_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Engine>();
    }

    #[test]
    fn pcm_input_is_paced_in_real_time() {
        let packets = Arc::new(Mutex::new(Vec::new()));
        let sink = packets.clone();
        let reader = std::io::Cursor::new(sine_pcm(10.0));
        let engine = Engine::start(
            Options {
                input: Input::Pcm(Box::new(reader)),
                output: Output::None,
            },
            move |seq, packet| sink.lock().unwrap().push((seq, packet)),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(1_000));
        let got = packets.lock().unwrap().len();
        assert!((45..=56).contains(&got), "{got} packets in 1 s");
        assert!(engine.local_level() > 0.3);
        let seqs: Vec<u16> = packets.lock().unwrap().iter().map(|p| p.0).collect();
        assert_eq!(seqs, (0..seqs.len() as u16).collect::<Vec<_>>());
    }

    #[test]
    fn tone_input_is_audible() {
        let count = Arc::new(AtomicU32::new(0));
        let sink = count.clone();
        let engine = Engine::start(
            Options {
                input: Input::Tone,
                output: Output::None,
            },
            move |_, _| {
                sink.fetch_add(1, Ordering::Relaxed);
            },
        )
        .unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(count.load(Ordering::Relaxed) >= 10);
        assert!(engine.local_level() > 0.0);
    }

    #[test]
    fn muted_engine_sends_nothing() {
        let count = Arc::new(AtomicU32::new(0));
        let sink = count.clone();
        let engine = Engine::start(
            Options {
                input: Input::Pcm(Box::new(std::io::Cursor::new(sine_pcm(10.0)))),
                output: Output::None,
            },
            move |_, _| {
                sink.fetch_add(1, Ordering::Relaxed);
            },
        )
        .unwrap();
        engine.set_muted(true);
        thread::sleep(Duration::from_millis(300));
        assert!(count.load(Ordering::Relaxed) <= 1);
    }

    #[test]
    fn received_packets_reach_the_mixer() {
        let (tx, rx) = mpsc::channel();
        let publisher = Engine::start(
            Options {
                input: Input::Pcm(Box::new(std::io::Cursor::new(sine_pcm(10.0)))),
                output: Output::None,
            },
            move |seq, packet| {
                let _ = tx.send((seq, packet));
            },
        )
        .unwrap();
        let listener = Engine::start(
            Options {
                input: Input::None,
                output: Output::None,
            },
            |_, _| {},
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            if let Ok((seq, packet)) = rx.recv_timeout(Duration::from_millis(50)) {
                listener.receive(7, seq, &packet);
            }
        }
        assert!(listener.level(7) > 0.3, "level {}", listener.level(7));
        assert_eq!(listener.level(8), 0.0);
        listener.remove(7);
        assert_eq!(listener.level(7), 0.0);
        drop(publisher);
    }

    #[test]
    fn lists_devices_without_opening_them() {
        assert!(list_devices().is_ok());
    }
}
