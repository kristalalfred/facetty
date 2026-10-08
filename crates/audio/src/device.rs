use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

use crate::mixer::Mixer;
use crate::resample::Resampler;
use crate::{FRAME_SAMPLES, SAMPLE_RATE};

const MAX_CAPTURE_BACKLOG: usize = SAMPLE_RATE as usize;

pub(crate) struct Capture {
    pub samples: Mutex<VecDeque<f32>>,
    pub ready: Condvar,
}

pub(crate) fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| device.to_string())
}

pub(crate) fn find(name: Option<&str>, input: bool) -> Result<Device> {
    let host = cpal::default_host();
    let Some(name) = name else {
        let device = if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        };
        return device.ok_or_else(|| anyhow!("no default {} device", direction(input)));
    };
    let devices: Vec<Device> = if input {
        host.input_devices()?.collect()
    } else {
        host.output_devices()?.collect()
    };
    let names: Vec<String> = devices.iter().map(device_name).collect();
    let found = crate::pick_device(&names, name)
        .ok_or_else(|| anyhow!("no {} device matching {name:?}", direction(input)))?;
    Ok(devices.into_iter().nth(found).unwrap())
}

fn direction(input: bool) -> &'static str {
    if input { "input" } else { "output" }
}

/// Starts capturing into `capture`, resampled to `SAMPLE_RATE`.
pub(crate) fn start_input<E>(device: &Device, capture: Arc<Capture>, on_error: E) -> Result<Stream>
where
    E: FnMut(cpal::Error) + Send + 'static,
{
    let supported = device
        .default_input_config()
        .context("input device has no usable config")?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => input_stream::<f32, E>(device, config, capture, on_error),
        SampleFormat::F64 => input_stream::<f64, E>(device, config, capture, on_error),
        SampleFormat::I8 => input_stream::<i8, E>(device, config, capture, on_error),
        SampleFormat::I16 => input_stream::<i16, E>(device, config, capture, on_error),
        SampleFormat::I32 => input_stream::<i32, E>(device, config, capture, on_error),
        SampleFormat::U8 => input_stream::<u8, E>(device, config, capture, on_error),
        SampleFormat::U16 => input_stream::<u16, E>(device, config, capture, on_error),
        SampleFormat::U32 => input_stream::<u32, E>(device, config, capture, on_error),
        other => bail!("unsupported input sample format {other:?}"),
    }?;
    stream.play()?;
    Ok(stream)
}

fn input_stream<T, E>(
    device: &Device,
    config: StreamConfig,
    capture: Arc<Capture>,
    on_error: E,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
    E: FnMut(cpal::Error) + Send + 'static,
{
    let channels = config.channels.max(1) as usize;
    let mut resampler = Resampler::new(config.sample_rate, SAMPLE_RATE);
    let mut mono = Vec::new();
    let mut resampled = Vec::new();
    let stream = device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            mono.clear();
            mono.extend(data.chunks(channels).map(|frame| {
                frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32
            }));
            resampled.clear();
            resampler.process(&mono, &mut resampled);
            let mut samples = capture.samples.lock().unwrap();
            samples.extend(&resampled);
            let excess = samples.len().saturating_sub(MAX_CAPTURE_BACKLOG);
            samples.drain(..excess);
            drop(samples);
            capture.ready.notify_one();
        },
        on_error,
        None,
    )?;
    Ok(stream)
}

pub(crate) fn start_output<E>(device: &Device, mixer: Mixer, on_error: E) -> Result<Stream>
where
    E: FnMut(cpal::Error) + Send + 'static,
{
    let supported = device
        .default_output_config()
        .context("output device has no usable config")?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => output_stream::<f32, E>(device, config, mixer, on_error),
        SampleFormat::F64 => output_stream::<f64, E>(device, config, mixer, on_error),
        SampleFormat::I8 => output_stream::<i8, E>(device, config, mixer, on_error),
        SampleFormat::I16 => output_stream::<i16, E>(device, config, mixer, on_error),
        SampleFormat::I32 => output_stream::<i32, E>(device, config, mixer, on_error),
        SampleFormat::U8 => output_stream::<u8, E>(device, config, mixer, on_error),
        SampleFormat::U16 => output_stream::<u16, E>(device, config, mixer, on_error),
        SampleFormat::U32 => output_stream::<u32, E>(device, config, mixer, on_error),
        other => bail!("unsupported output sample format {other:?}"),
    }?;
    stream.play()?;
    Ok(stream)
}

fn output_stream<T, E>(
    device: &Device,
    config: StreamConfig,
    mut mixer: Mixer,
    on_error: E,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
    E: FnMut(cpal::Error) + Send + 'static,
{
    let channels = config.channels.max(1) as usize;
    let mut resampler = Resampler::new(SAMPLE_RATE, config.sample_rate);
    let mut pending: VecDeque<f32> = VecDeque::new();
    let mut frame = [0f32; FRAME_SAMPLES];
    let mut resampled = Vec::new();
    let stream = device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            let frames = data.len() / channels;
            while pending.len() < frames {
                mixer.mix(&mut frame);
                resampled.clear();
                resampler.process(&frame, &mut resampled);
                pending.extend(&resampled);
            }
            for out in data.chunks_mut(channels) {
                out.fill(T::from_sample(pending.pop_front().unwrap_or(0.0)));
            }
        },
        on_error,
        None,
    )?;
    Ok(stream)
}
