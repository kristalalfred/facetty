use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use facetty_ascii::{Analyzer, Encoder, Frame, Image, Params};
use facetty_proto::ladder::{self, Rung};
use facetty_proto::media::{self, VideoFrame};
use futures::StreamExt;
use futures::channel::mpsc;
use web_transport_wasm::Session;

pub struct EncodedFrame {
    rung: Rung,
    seq: u32,
    payload: Vec<u8>,
}

pub struct Publisher {
    analyzer: Analyzer,
    encoders: HashMap<Rung, Encoder>,
    rungs: Vec<Rung>,
    refresh: Vec<Rung>,
    seq: u32,
    outbox: Outbox,
}

impl Publisher {
    pub fn new(outbox: Outbox) -> Self {
        Self {
            analyzer: Analyzer::new(Params::default()),
            encoders: HashMap::new(),
            rungs: Vec::new(),
            refresh: Vec::new(),
            seq: 0,
            outbox,
        }
    }

    pub fn set_rungs(&mut self, rungs: Vec<Rung>) {
        self.encoders.retain(|rung, _| rungs.contains(rung));
        self.rungs = rungs;
    }

    pub fn refresh(&mut self, rung: Rung) {
        self.refresh.push(rung);
    }

    pub fn params(&self) -> Params {
        *self.analyzer.params()
    }

    pub fn update_params(&mut self, f: impl FnOnce(&mut Params)) {
        let mut params = self.params();
        f(&mut params);
        self.analyzer.set_params(params);
    }

    pub fn publish(&mut self, image: &Image, self_view: Option<(u16, u16)>) -> Option<Frame> {
        self.analyzer.analyze(image);
        let refresh = std::mem::take(&mut self.refresh);
        for &rung in &self.rungs {
            let Some(size) = ladder::size(rung) else {
                continue;
            };
            self.seq = self.seq.wrapping_add(1);
            let encoder = self.encoders.entry(rung).or_default();
            let picture = self.analyzer.render(size.cols, size.rows);
            self.outbox
                .encode(encoder, rung, self.seq, &picture, refresh.contains(&rung));
        }
        self_view.map(|(cols, rows)| self.analyzer.render(cols, rows))
    }
}

/// Claude: the newest encoded frame for each rung, waiting for the previous
/// frame to leave. A frame still waiting when the next one for its rung is
/// encoded is folded into that one, as the native client does.
#[derive(Clone)]
pub struct Outbox(std::rc::Rc<Waiting>);

struct Waiting {
    frames: RefCell<BTreeMap<Rung, EncodedFrame>>,
    doorbell: mpsc::UnboundedSender<()>,
}

impl Outbox {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
        let (doorbell, rings) = mpsc::unbounded();
        let waiting = Waiting {
            frames: RefCell::default(),
            doorbell,
        };
        (Self(std::rc::Rc::new(waiting)), rings)
    }

    fn take(&self) -> Option<EncodedFrame> {
        let mut frames = self.0.frames.borrow_mut();
        let rung = frames.values().min_by_key(|f| f.seq)?.rung;
        frames.remove(&rung)
    }

    fn encode(&self, encoder: &mut Encoder, rung: Rung, seq: u32, picture: &Frame, full: bool) {
        if self.0.frames.borrow_mut().remove(&rung).is_some() {
            encoder.undo();
        }
        if full {
            encoder.request_full();
        }
        let payload = encoder.encode(picture);
        self.0
            .frames
            .borrow_mut()
            .insert(rung, EncodedFrame { rung, seq, payload });
        let _ = self.0.doorbell.unbounded_send(());
    }
}

/// Claude: one frame at a time, each on its own stream, taking the next only
/// once the browser has closed the stream of the last.
pub async fn send(session: Session, outbox: Outbox, mut rings: mpsc::UnboundedReceiver<()>) {
    loop {
        let Some(frame) = outbox.take() else {
            if rings.next().await.is_none() {
                return;
            }
            continue;
        };
        let bytes = media::encode(&VideoFrame {
            publisher: 0,
            rung: frame.rung,
            seq: frame.seq,
            payload: frame.payload,
        });
        let Ok(mut stream) = session.open_uni().await else {
            return;
        };
        if stream.write(&bytes).await.is_err() {
            continue;
        }
        let _ = stream.finish();
        let _ = stream.closed().await;
    }
}
