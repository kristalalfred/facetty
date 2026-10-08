//! Turns captured frames into ASCII at the sizes subscribers asked for, plus
//! the local self-view at whatever size its tile has.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use facetty_ascii::{Analyzer, Encoder, Frame, Params};
use facetty_proto::ladder::{self, Rung};
use tokio::sync::{mpsc, watch};

use crate::capture::{Capture, Source};

pub struct EncodedFrame {
    pub rung: Rung,
    pub seq: u32,
    pub payload: Vec<u8>,
}

#[derive(Default)]
struct Control {
    rungs: Mutex<Vec<Rung>>,
    refresh: Mutex<Vec<Rung>>,
    self_view: Mutex<Option<(u16, u16)>>,
    params: Mutex<Params>,
    enabled: AtomicBool,
    stop: AtomicBool,
}

pub struct Publisher {
    control: Arc<Control>,
    self_frames: watch::Receiver<Option<Arc<Frame>>>,
    error: Arc<Mutex<Option<String>>>,
}

impl Publisher {
    pub fn start(source: Source, fps: u32, out: mpsc::Sender<EncodedFrame>, enabled: bool) -> Self {
        let control = Arc::new(Control {
            enabled: AtomicBool::new(enabled),
            ..Default::default()
        });
        let (self_tx, self_frames) = watch::channel(None);
        let error = Arc::new(Mutex::new(None));
        let worker = (control.clone(), error.clone());
        thread::Builder::new()
            .name("publisher".into())
            .spawn(move || run(source, fps.max(1), worker.0, worker.1, self_tx, out))
            .expect("spawn publisher thread");
        Self {
            control,
            self_frames,
            error,
        }
    }

    pub fn set_rungs(&self, rungs: Vec<Rung>) {
        *self.control.rungs.lock().unwrap() = rungs;
    }

    /// Sends the next frame at `rung` in full.
    pub fn refresh(&self, rung: Rung) {
        self.control.refresh.lock().unwrap().push(rung);
    }

    pub fn set_self_view(&self, size: Option<(u16, u16)>) {
        *self.control.self_view.lock().unwrap() = size;
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.control.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn update_params(&self, f: impl FnOnce(&mut Params)) {
        f(&mut self.control.params.lock().unwrap());
    }

    pub fn params(&self) -> Params {
        *self.control.params.lock().unwrap()
    }

    pub fn self_frame(&self) -> Option<Arc<Frame>> {
        self.self_frames.borrow().clone()
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
    }
}

fn run(
    source: Source,
    fps: u32,
    control: Arc<Control>,
    error: Arc<Mutex<Option<String>>>,
    self_tx: watch::Sender<Option<Arc<Frame>>>,
    out: mpsc::Sender<EncodedFrame>,
) {
    let interval = Duration::from_secs(1) / fps;
    let mut capture: Option<Capture> = None;
    let mut analyzer = Analyzer::new(Params::default());
    let mut encoders: HashMap<Rung, Encoder> = HashMap::new();
    let mut last_seen = 0u64;
    let mut seq = 0u32;
    let mut next = Instant::now();

    while !control.stop.load(Ordering::Relaxed) {
        next += interval;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        } else {
            next = Instant::now();
        }

        if !control.enabled.load(Ordering::Relaxed) {
            if capture.take().is_some() {
                self_tx.send_replace(None);
            }
            continue;
        }
        let cap = capture.get_or_insert_with(|| Capture::start(source.clone()));
        *error.lock().unwrap() = cap.error();
        let Some((frame_seq, image)) = cap.latest() else {
            continue;
        };
        if frame_seq == last_seen {
            continue;
        }
        last_seen = frame_seq;

        analyzer.set_params(*control.params.lock().unwrap());
        analyzer.analyze(&image);

        let rungs = control.rungs.lock().unwrap().clone();
        encoders.retain(|rung, _| rungs.contains(rung));
        for rung in std::mem::take(&mut *control.refresh.lock().unwrap()) {
            if let Some(encoder) = encoders.get_mut(&rung) {
                encoder.request_full();
            }
        }
        for rung in rungs {
            let Some(size) = ladder::size(rung) else {
                continue;
            };
            seq = seq.wrapping_add(1);
            let encoder = encoders.entry(rung).or_default();
            let payload = encoder.encode(&analyzer.render(size.cols, size.rows));
            if out.try_send(EncodedFrame { rung, seq, payload }).is_err() {
                encoder.undo();
            }
        }

        let self_view = *control.self_view.lock().unwrap();
        if let Some((cols, rows)) = self_view {
            self_tx.send_replace(Some(Arc::new(analyzer.render(cols, rows))));
        }
    }
}
