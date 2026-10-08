use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use bits_ascii::Image;
use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::video::capture::Parameters;
use v4l::{Device, FourCC};

use crate::capture::{HEIGHT, WIDTH, fit};

pub fn list() -> Vec<String> {
    nodes().into_iter().map(|(_, name)| name).collect()
}

pub fn run(device: &str, stop: &AtomicBool, mut on_frame: impl FnMut(Image)) -> Result<()> {
    let (path, name) = if device.starts_with('/') {
        (PathBuf::from(device), device.to_string())
    } else {
        let mut nodes = nodes();
        let names: Vec<String> = nodes.iter().map(|(_, name)| name.clone()).collect();
        nodes.swap_remove(super::pick(&names, device)?)
    };
    let dev = Device::with_path(&path).with_context(|| format!("opening {}", path.display()))?;
    let mut format = dev
        .format()
        .with_context(|| format!("reading the format of {name}"))?;
    format.width = WIDTH as u32;
    format.height = HEIGHT as u32;
    format.fourcc = FourCC::new(b"YUYV");
    let format = dev
        .set_format(&format)
        .with_context(|| format!("setting the format of {name}"))?;
    if format.fourcc != FourCC::new(b"YUYV") {
        bail!("{name} does not offer YUYV video, only {}", format.fourcc);
    }
    let _ = dev.set_params(&Parameters::with_fps(30));

    let (width, height) = (format.width as usize, format.height as usize);
    let stride = format.stride as usize;
    let mut stream = Stream::with_buffers(&dev, Type::VideoCapture, 4)
        .with_context(|| format!("starting {name}"))?;
    while !stop.load(Ordering::Relaxed) {
        let (data, meta) = stream.next().with_context(|| format!("reading {name}"))?;
        if (meta.bytesused as usize) < stride * height {
            continue;
        }
        on_frame(fit(width, height, |x, y| {
            let i = y * stride + (x & !1) * 2;
            yuv_to_rgb(data[i + (x & 1) * 2], data[i + 1], data[i + 3])
        }));
    }
    Ok(())
}

/// `/dev/video*` nodes that capture video, in number order, with their names.
/// UVC cameras also create a metadata node, which this skips.
fn nodes() -> Vec<(PathBuf, String)> {
    let mut found: Vec<(u32, PathBuf, String)> = fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let number = entry
                .file_name()
                .to_str()?
                .strip_prefix("video")?
                .parse()
                .ok()?;
            let caps = Device::with_path(entry.path()).ok()?.query_caps().ok()?;
            caps.capabilities
                .contains(Flags::VIDEO_CAPTURE)
                .then(|| (number, entry.path(), caps.card))
        })
        .collect();
    found.sort_by_key(|(number, ..)| *number);
    found
        .into_iter()
        .map(|(_, path, name)| (path, name))
        .collect()
}

/// BT.601 studio range, the usual encoding for webcam YUYV.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = 298 * (y as i32 - 16);
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    [c + 409 * e, c - 100 * d - 208 * e, c + 516 * d].map(|x| ((x + 128) >> 8).clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_studio_range_yuv() {
        assert_eq!(yuv_to_rgb(16, 128, 128), [0, 0, 0]);
        assert_eq!(yuv_to_rgb(235, 128, 128), [255, 255, 255]);
        let [r, g, b] = yuv_to_rgb(81, 90, 240);
        assert!(r > 230 && g < 20 && b < 20, "{r} {g} {b}");
    }
}
