use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use facetty_audio::opus::Encoder;
use facetty_audio::resample::Resampler;
use facetty_audio::{FRAME_SAMPLES, Mix, SAMPLE_RATE};
use js_sys::{Array, Float32Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioWorkletNode, AudioWorkletNodeOptions, MediaStream, MessageEvent};

use crate::util::{js, window};

/// Claude: the microphone goes through an AudioWorklet into Opus packets,
/// and every publisher's packets are mixed and played through another
/// worklet that asks for 20 ms at a time.
pub struct Audio {
    context: AudioContext,
    mix: Rc<RefCell<Mix>>,
    _nodes: Vec<AudioWorkletNode>,
    _handlers: Vec<Closure<dyn FnMut(MessageEvent)>>,
    _unlock: Closure<dyn FnMut()>,
}

impl Audio {
    pub async fn start(
        microphone: Option<&MediaStream>,
        mut on_packet: impl FnMut(u16, Vec<u8>) + 'static,
    ) -> Result<Self> {
        let context = AudioContext::new().map_err(js)?;
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
        let mut nodes = Vec::new();
        let mut handlers = Vec::new();

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
        nodes.push(playback);
        handlers.push(to_speaker);

        if let Some(stream) = microphone {
            let source = context.create_media_stream_source(stream).map_err(js)?;
            let capture = node(&context, "facetty-capture", 1)?;
            source.connect_with_audio_node(&capture).map_err(js)?;
            capture
                .connect_with_audio_node(&context.destination())
                .map_err(js)?;
            let mut encoder = Encoder::new()?;
            let mut resampler = Resampler::new(rate, SAMPLE_RATE);
            let mut pending: Vec<f32> = Vec::new();
            let mut seq = 0u16;
            let from_mic = Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
                let samples: Float32Array = message.data().unchecked_into();
                resampler.process(&samples.to_vec(), &mut pending);
                let (frames, rest) = pending.as_chunks::<FRAME_SAMPLES>();
                for frame in frames {
                    if let Ok(packet) = encoder.encode(frame) {
                        on_packet(seq, packet);
                        seq = seq.wrapping_add(1);
                    }
                }
                let used = pending.len() - rest.len();
                pending.drain(..used);
            });
            capture
                .port()
                .map_err(js)?
                .set_onmessage(Some(from_mic.as_ref().unchecked_ref()));
            nodes.push(capture);
            handlers.push(from_mic);
        }

        let unlock = {
            let context = context.clone();
            Closure::<dyn FnMut()>::new(move || {
                let _ = context.resume();
            })
        };
        for event in ["pointerdown", "keydown"] {
            let _ =
                window().add_event_listener_with_callback(event, unlock.as_ref().unchecked_ref());
        }
        let _ = context.resume();

        Ok(Self {
            context,
            mix,
            _nodes: nodes,
            _handlers: handlers,
            _unlock: unlock,
        })
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
        let _ = self.context.close();
    }
}

fn node(context: &AudioContext, name: &str, inputs: u32) -> Result<AudioWorkletNode> {
    let options = AudioWorkletNodeOptions::new();
    options.set_number_of_inputs(inputs);
    options.set_number_of_outputs(1);
    options.set_output_channel_count(&Array::of1(&1.into()));
    AudioWorkletNode::new_with_options(context, name, &options).map_err(js)
}
