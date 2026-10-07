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
use cpal::traits::HostTrait;

use device::Capture;
use mixer::{Command, Mixer};
use resample::Resampler;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;

const FRAME_DURATION: Duration = Duration::from_millis(20);
const MAX_CLOCK_LAG: Duration = Duration::from_millis(200);
/// Bounds memory if playback stalls (e.g. the output device disappeared).
const MAX_INBOX: usize = 2_000;

pub enum Input {
    None,
    Device(Option<String>),
    /// 48 kHz mono s16le, consumed in real time.
    Pcm(Box<dyn Read + Send>),
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
}

pub struct Engine {
    shared: Arc<Shared>,
    device_thread: Option<DeviceThread>,
}

struct DeviceThread {
    stop: mpsc::Sender<()>,
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
        });
        let mut sender = Some(PacketSender::new(shared.clone(), Box::new(on_packet))?);
        let mixer = Mixer::new(shared.clone());

        let (input_device, pcm) = match opts.input {
            Input::None => (None, None),
            Input::Device(name) => (Some(name), None),
            Input::Pcm(reader) => (None, Some(reader)),
        };
        let (output_device, headless_mixer) = match opts.output {
            Output::Device(name) => (Some((name, mixer)), None),
            Output::None => (None, Some(mixer)),
        };

        let mut engine = Engine {
            shared: shared.clone(),
            device_thread: None,
        };

        let capture = input_device.as_ref().map(|_| {
            Arc::new(Capture {
                samples: Mutex::new(VecDeque::new()),
                ready: Condvar::new(),
            })
        });
        if input_device.is_some() || output_device.is_some() {
            let (thread, input_rate) =
                DeviceThread::spawn(input_device, capture.clone(), output_device)?;
            engine.device_thread = Some(thread);
            if let (Some(capture), Some(rate), Some(sender)) = (capture, input_rate, sender.take())
            {
                let shared = shared.clone();
                thread::Builder::new()
                    .name("bits-audio-encode".into())
                    .spawn(move || encode_capture(shared, capture, rate, sender))?;
            }
        }
        if let (Some(reader), Some(sender)) = (pcm, sender.take()) {
            let shared = shared.clone();
            thread::Builder::new()
                .name("bits-audio-pcm".into())
                .spawn(move || encode_pcm(shared, reader, sender))?;
        }

        if let Some(mixer) = headless_mixer {
            let shared = shared.clone();
            thread::Builder::new()
                .name("bits-audio-mix".into())
                .spawn(move || mix_headless(shared, mixer))?;
        }
        Ok(engine)
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
            drop(thread.stop);
            let _ = thread.handle.join();
        }
    }
}

impl DeviceThread {
    /// cpal streams are not `Send` on every platform, so one thread builds
    /// them and keeps them alive until the engine drops.
    fn spawn(
        input: Option<Option<String>>,
        capture: Option<Arc<Capture>>,
        output: Option<(Option<String>, Mixer)>,
    ) -> Result<(DeviceThread, Option<u32>)> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop, stop_rx) = mpsc::channel::<()>();
        let handle = thread::Builder::new()
            .name("bits-audio-device".into())
            .spawn(move || {
                let started = (|| -> Result<_> {
                    let mut streams = Vec::new();
                    let mut input_rate = None;
                    if let (Some(name), Some(capture)) = (input, capture) {
                        let device = device::find(name.as_deref(), true)?;
                        let (stream, rate) = device::start_input(&device, capture)
                            .with_context(|| format!("input {}", device::device_name(&device)))?;
                        streams.push(stream);
                        input_rate = Some(rate);
                    }
                    if let Some((name, mixer)) = output {
                        let device = device::find(name.as_deref(), false)?;
                        let stream = device::start_output(&device, mixer)
                            .with_context(|| format!("output {}", device::device_name(&device)))?;
                        streams.push(stream);
                    }
                    Ok((streams, input_rate))
                })();
                match started {
                    Ok((streams, input_rate)) => {
                        let _ = ready_tx.send(Ok(input_rate));
                        let _ = stop_rx.recv();
                        drop(streams);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;
        let input_rate = ready_rx
            .recv()
            .map_err(|_| anyhow!("audio device thread exited during setup"))??;
        Ok((DeviceThread { stop, handle }, input_rate))
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

fn encode_capture(shared: Arc<Shared>, capture: Arc<Capture>, rate: u32, mut sender: PacketSender) {
    let mut resampler = Resampler::new(rate, SAMPLE_RATE);
    let mut chunk = Vec::new();
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
            chunk.clear();
            chunk.extend(samples.drain(..));
        }
        resampler.process(&chunk, &mut pending);
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
