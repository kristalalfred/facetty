//! Turns captured frames into ASCII at the sizes subscribers asked for, plus
//! the local self-view at whatever size its tile has.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use facetty_ascii::{Analyzer, Encoder, Frame, Params};
use facetty_proto::ladder::{self, Rung};
use tokio::sync::{Notify, watch};
use tracing::trace;

use crate::capture::{Capture, Source};

pub struct EncodedFrame {
    pub rung: Rung,
    pub seq: u32,
    pub payload: Vec<u8>,
}

/// The newest encoded frame for each rung, waiting for the connection to have
/// room. The publisher takes back a frame that is still waiting when it
/// encodes the next one for that rung, and folds it into that one.
#[derive(Clone, Default)]
pub struct Outbox(Arc<Waiting>);

#[derive(Default)]
struct Waiting {
    frames: Mutex<BTreeMap<Rung, EncodedFrame>>,
    filled: Notify,
}

impl Outbox {
    /// Waits until a frame is waiting. Only one task may wait at a time.
    pub async fn filled(&self) {
        while self.0.frames.lock().unwrap().is_empty() {
            self.0.filled.notified().await;
        }
    }

    /// The waiting frame that was encoded first.
    pub fn take(&self) -> Option<EncodedFrame> {
        let mut frames = self.0.frames.lock().unwrap();
        let rung = frames.values().min_by_key(|f| f.seq)?.rung;
        frames.remove(&rung)
    }

    /// Encodes `picture` as the next frame for `rung`, folding in the frame
    /// for `rung` that is still waiting, if any.
    fn encode(&self, encoder: &mut Encoder, rung: Rung, seq: u32, picture: &Frame, full: bool) {
        // Undo before requesting a full frame: undo restores the state from
        // before the withdrawn frame, which would drop the request.
        if self.0.frames.lock().unwrap().remove(&rung).is_some() {
            encoder.undo();
        }
        if full {
            encoder.request_full();
        }
        let payload = encoder.encode(picture);
        trace!(rung, seq, bytes = payload.len(), "encoded video frame");
        let frame = EncodedFrame { rung, seq, payload };
        self.0.frames.lock().unwrap().insert(rung, frame);
        self.0.filled.notify_one();
    }
}

#[derive(Default)]
struct Control {
    rungs: Mutex<Vec<Rung>>,
    refresh: Mutex<Vec<Rung>>,
    self_view: Mutex<Option<(u16, u16)>>,
    next_source: Mutex<Option<Source>>,
    params: Mutex<Params>,
    enabled: AtomicBool,
    stop: AtomicBool,
}

pub struct Publisher {
    source: Source,
    control: Arc<Control>,
    self_frames: watch::Receiver<Option<Arc<Frame>>>,
    error: Arc<Mutex<Option<String>>>,
}

impl Publisher {
    pub fn start(source: Source, fps: u32, out: Outbox, enabled: bool) -> Self {
        let control = Arc::new(Control {
            enabled: AtomicBool::new(enabled),
            ..Default::default()
        });
        let (self_tx, self_frames) = watch::channel(None);
        let error = Arc::new(Mutex::new(None));
        let worker = (source.clone(), control.clone(), error.clone());
        thread::Builder::new()
            .name("publisher".into())
            .spawn(move || run(worker.0, fps.max(1), worker.1, worker.2, self_tx, out))
            .expect("spawn publisher thread");
        Self {
            source,
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

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn set_source(&mut self, source: Source) {
        *self.control.next_source.lock().unwrap() = Some(source.clone());
        self.source = source;
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

    pub fn self_frames(&self) -> watch::Receiver<Option<Arc<Frame>>> {
        self.self_frames.clone()
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
    mut source: Source,
    fps: u32,
    control: Arc<Control>,
    error: Arc<Mutex<Option<String>>>,
    self_tx: watch::Sender<Option<Arc<Frame>>>,
    out: Outbox,
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

        if let Some(next_source) = control.next_source.lock().unwrap().take() {
            source = next_source;
            if capture.take().is_some() {
                self_tx.send_replace(None);
            }
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
        let refresh = std::mem::take(&mut *control.refresh.lock().unwrap());
        for rung in rungs {
            let Some(size) = ladder::size(rung) else {
                continue;
            };
            seq = seq.wrapping_add(1);
            let encoder = encoders.entry(rung).or_default();
            let picture = analyzer.render(size.cols, size.rows);
            out.encode(encoder, rung, seq, &picture, refresh.contains(&rung));
        }

        let self_view = *control.self_view.lock().unwrap();
        if let Some((cols, rows)) = self_view {
            self_tx.send_replace(Some(Arc::new(analyzer.render(cols, rows))));
        }
    }
}

#[cfg(test)]
mod tests {
    use facetty_ascii::{Cell, Decoder};

    use super::*;

    fn picture(glyph: u8) -> Frame {
        let mut frame = Frame::blank(8, 2);
        frame.cells.fill(Cell { glyph, rgb: [0; 3] });
        frame
    }

    fn half(glyph: u8) -> Frame {
        let mut frame = picture(1);
        frame.cells[..8].fill(Cell { glyph, rgb: [0; 3] });
        frame
    }

    #[test]
    fn a_frame_still_waiting_is_folded_into_the_next() {
        let out = Outbox::default();
        let mut encoder = Encoder::default();
        let mut viewer = Decoder::default();
        out.encode(&mut encoder, 3, 1, &picture(1), false);
        let sent = out.take().unwrap();
        viewer.decode(sent.seq, &sent.payload).unwrap();

        out.encode(&mut encoder, 3, 2, &picture(2), false);
        out.encode(&mut encoder, 3, 3, &picture(2), false);
        let next = out.take().unwrap();
        assert_eq!(next.seq, 3);
        assert!(out.take().is_none());
        let shown = viewer.decode(next.seq, &next.payload).unwrap();
        assert!(shown.cells.iter().all(|c| c.glyph == 2));
    }

    #[test]
    fn a_refresh_survives_folding_in_a_waiting_frame() {
        let out = Outbox::default();
        let mut encoder = Encoder::default();
        out.encode(&mut encoder, 3, 1, &picture(1), false);
        out.take().unwrap();
        out.encode(&mut encoder, 3, 2, &half(2), false);
        out.encode(&mut encoder, 3, 3, &half(2), true);
        let next = out.take().unwrap();
        let shown = Decoder::default().decode(next.seq, &next.payload).unwrap();
        assert_eq!(shown, half(2), "a new viewer gets every cell");
    }
}
