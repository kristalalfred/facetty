use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use facetty_audio::opus::Encoder;
use facetty_audio::resample::Resampler;
use facetty_audio::{FRAME_SAMPLES, Mix, SAMPLE_RATE, loudness, smooth};
use js_sys::{Array, Float32Array, Function, Reflect};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioWorkletNode, AudioWorkletNodeOptions, MediaStream,
    MediaStreamAudioSourceNode, MessageEvent,
};

use crate::util::{js, window};

const UNLOCK_EVENTS: [&str; 2] = ["pointerdown", "keydown"];

/// Claude: the microphone goes through an AudioWorklet into Opus packets,
/// and every publisher's packets are mixed and played through another
/// worklet that asks for 20 ms at a time. While muted the microphone is
/// still metered but sends nothing, as in the native engine.
pub struct Audio {
    context: AudioContext,
    mix: Rc<RefCell<Mix>>,
    capture: AudioWorkletNode,
    source: RefCell<Option<MediaStreamAudioSourceNode>>,
    muted: Rc<Cell<bool>>,
    level: Rc<Cell<f32>>,
    playback: AudioWorkletNode,
    _handlers: Vec<Closure<dyn FnMut(MessageEvent)>>,
    unlock: Closure<dyn FnMut()>,
}

impl Audio {
    pub async fn start(
        muted: bool,
        mut on_packet: impl FnMut(u16, Vec<u8>) + 'static,
    ) -> Result<Self> {
        let context = AudioContext::new().map_err(js)?;
        let unlock = {
            let context = context.clone();
            Closure::<dyn FnMut()>::new(move || {
                let _ = context.resume();
            })
        };
        for event in UNLOCK_EVENTS {
            let _ =
                window().add_event_listener_with_callback(event, unlock.as_ref().unchecked_ref());
        }
        let _ = context.resume();
        let rate = context.sample_rate() as u32;
        JsFuture::from(
            context
                .audio_worklet()
                .map_err(js)?
                .add_module("/web/worklet.js")
                .map_err(js)?,
        )
        .await
        .map_err(js)?;

        let mix = Rc::new(RefCell::new(Mix::default()));
        let playback = node(&context, "facetty-playback", 0)?;
        playback
            .connect_with_audio_node(&context.destination())
            .map_err(js)?;
        let to_speaker = {
            let mix = mix.clone();
            let port = playback.port().map_err(js)?;
            let mut resampler = Resampler::new(SAMPLE_RATE, rate);
            let mut frame = [0f32; FRAME_SAMPLES];
            let mut out = Vec::new();
            Closure::<dyn FnMut(MessageEvent)>::new(move |_: MessageEvent| {
                mix.borrow_mut().mix(&mut frame);
                out.clear();
                resampler.process(&frame, &mut out);
                let chunk = Float32Array::from(&out[..]);
                let _ = port.post_message_with_transferable(&chunk, &Array::of1(&chunk.buffer()));
            })
        };
        playback
            .port()
            .map_err(js)?
            .set_onmessage(Some(to_speaker.as_ref().unchecked_ref()));

        let capture = node(&context, "facetty-capture", 1)?;
        capture
            .connect_with_audio_node(&context.destination())
            .map_err(js)?;
        let muted = Rc::new(Cell::new(muted));
        let level = Rc::new(Cell::new(0.0));
        let from_mic = {
            let (muted, level) = (muted.clone(), level.clone());
            let mut encoder = Encoder::new()?;
            let mut resampler = Resampler::new(rate, SAMPLE_RATE);
            let mut pending: Vec<f32> = Vec::new();
            let mut seq = 0u16;
            Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
                let samples: Float32Array = message.data().unchecked_into();
                resampler.process(&samples.to_vec(), &mut pending);
                let (frames, rest) = pending.as_chunks::<FRAME_SAMPLES>();
                for frame in frames {
                    level.set(smooth(level.get(), loudness(frame)));
                    if muted.get() {
                        continue;
                    }
                    if let Ok(packet) = encoder.encode(frame) {
                        on_packet(seq, packet);
                        seq = seq.wrapping_add(1);
                    }
                }
                let used = pending.len() - rest.len();
                pending.drain(..used);
            })
        };
        capture
            .port()
            .map_err(js)?
            .set_onmessage(Some(from_mic.as_ref().unchecked_ref()));

        Ok(Self {
            context,
            mix,
            capture,
            source: RefCell::new(None),
            muted,
            level,
            playback,
            _handlers: vec![to_speaker, from_mic],
            unlock,
        })
    }

    pub fn set_microphone(&self, stream: Option<&MediaStream>) {
        if let Some(old) = self.source.take() {
            let _ = old.disconnect();
        }
        self.level.set(0.0);
        let Some(stream) = stream else {
            return;
        };
        match self.context.create_media_stream_source(stream) {
            Ok(source) => {
                let _ = source.connect_with_audio_node(&self.capture);
                *self.source.borrow_mut() = Some(source);
            }
            Err(e) => web_sys::console::warn_1(&e),
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.set(muted);
    }

    /// Claude: the microphone's smoothed loudness, 0 to 1, muted or not.
    pub fn level(&self) -> f32 {
        self.level.get()
    }

    pub fn levels(&self) -> Vec<(u32, f32)> {
        self.mix.borrow().levels().collect()
    }

    pub fn can_choose_speaker(&self) -> bool {
        Reflect::has(&self.context, &"setSinkId".into()).unwrap_or(false)
    }

    pub async fn set_speaker(&self, id: &str) -> Result<()> {
        let set: Function = Reflect::get(&self.context, &"setSinkId".into())
            .map_err(js)?
            .dyn_into()
            .map_err(|_| anyhow!("this browser cannot choose a speaker"))?;
        let promise: js_sys::Promise = set
            .call1(&self.context, &id.into())
            .map_err(js)?
            .dyn_into()
            .map_err(js)?;
        JsFuture::from(promise).await.map_err(js)?;
        Ok(())
    }

    pub fn stop(&self) {
        self.set_microphone(None);
        let _ = self.context.close();
    }

    /// Claude: the worklets and the window keep calling these handlers until
    /// told otherwise, so they are unhooked before they are freed.
    fn detach(&self) {
        for node in [&self.capture, &self.playback] {
            if let Ok(port) = node.port() {
                port.set_onmessage(None);
            }
        }
        for event in UNLOCK_EVENTS {
            let _ = window()
                .remove_event_listener_with_callback(event, self.unlock.as_ref().unchecked_ref());
        }
    }

    pub fn receive(&self, publisher: u32, seq: u16, payload: &[u8]) {
        self.mix.borrow_mut().push(publisher, seq, payload);
    }

    pub fn remove(&self, publisher: u32) {
        self.mix.borrow_mut().remove(publisher);
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.detach();
    }
}

fn node(context: &AudioContext, name: &str, inputs: u32) -> Result<AudioWorkletNode> {
    let options = AudioWorkletNodeOptions::new();
    options.set_number_of_inputs(inputs);
    options.set_number_of_outputs(1);
    options.set_output_channel_count(&Array::of1(&1.into()));
    AudioWorkletNode::new_with_options(context, name, &options).map_err(js)
}
