use std::cell::RefCell;

use anyhow::Result;
use facetty_ascii::Image;
use js_sys::{Array, Object, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, HtmlVideoElement, MediaDeviceInfo,
    MediaDeviceKind, MediaStream, MediaStreamTrack,
};

use crate::util::{document, js, window};

pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 360;

pub struct Device {
    pub id: String,
    pub label: String,
}

#[derive(Default)]
pub struct Devices {
    pub cameras: Vec<Device>,
    pub mics: Vec<Device>,
    pub speakers: Vec<Device>,
}

/// Claude: camera and microphone in one request, so the browser asks once,
/// or whichever one it grants. Returns them as separate streams.
pub async fn open() -> (Option<MediaStream>, Option<MediaStream>) {
    for (video, audio) in [(true, true), (false, true), (true, false)] {
        if let Ok(stream) = request(video, audio, None).await {
            return (
                only(&stream.get_video_tracks()),
                only(&stream.get_audio_tracks()),
            );
        }
    }
    (None, None)
}

fn only(tracks: &Array) -> Option<MediaStream> {
    if tracks.length() == 0 {
        return None;
    }
    MediaStream::new_with_tracks(tracks).ok()
}

pub async fn camera(device: Option<&str>) -> Result<MediaStream> {
    request(true, false, device).await
}

pub async fn microphone(device: Option<&str>) -> Result<MediaStream> {
    request(false, true, device).await
}

async fn request(video: bool, audio: bool, device: Option<&str>) -> Result<MediaStream> {
    let constraints = web_sys::MediaStreamConstraints::new();
    constraints.set_video(&if video {
        video_constraints(device)
    } else {
        false.into()
    });
    constraints.set_audio(&if audio {
        audio_constraints(device)
    } else {
        false.into()
    });
    let devices = window().navigator().media_devices().map_err(js)?;
    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(js)?;
    Ok(JsFuture::from(promise).await.map_err(js)?.unchecked_into())
}

fn constraint(key: &str, value: &JsValue) -> JsValue {
    let o = Object::new();
    let _ = Reflect::set(&o, &key.into(), value);
    o.into()
}

fn with_device(o: &Object, device: Option<&str>) {
    if let Some(id) = device {
        let _ = Reflect::set(o, &"deviceId".into(), &constraint("exact", &id.into()));
    }
}

fn video_constraints(device: Option<&str>) -> JsValue {
    let o = Object::new();
    let _ = Reflect::set(&o, &"width".into(), &constraint("ideal", &1280.into()));
    let _ = Reflect::set(&o, &"height".into(), &constraint("ideal", &720.into()));
    let _ = Reflect::set(&o, &"frameRate".into(), &constraint("ideal", &30.into()));
    with_device(&o, device);
    o.into()
}

fn audio_constraints(device: Option<&str>) -> JsValue {
    let o = Object::new();
    for key in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
        let _ = Reflect::set(&o, &key.into(), &true.into());
    }
    with_device(&o, device);
    o.into()
}

pub fn stop(stream: &MediaStream) {
    for track in stream.get_tracks() {
        track.unchecked_into::<MediaStreamTrack>().stop();
    }
}

/// Claude: the id of the device behind the stream's first track.
pub fn device(stream: &MediaStream) -> Option<String> {
    let track: MediaStreamTrack = stream.get_tracks().get(0).dyn_into().ok()?;
    Reflect::get(&track.get_settings(), &"deviceId".into())
        .ok()?
        .as_string()
}

/// Claude: browsers leave out labels, and sometimes ids, until the page may
/// use a device of that kind.
pub async fn devices() -> Result<Devices> {
    let media = window().navigator().media_devices().map_err(js)?;
    let list: Array = JsFuture::from(media.enumerate_devices().map_err(js)?)
        .await
        .map_err(js)?
        .unchecked_into();
    let mut devices = Devices::default();
    for info in list.iter() {
        let info: MediaDeviceInfo = info.unchecked_into();
        let device = Device {
            id: info.device_id(),
            label: info.label(),
        };
        if device.id.is_empty() {
            continue;
        }
        match info.kind() {
            MediaDeviceKind::Videoinput => devices.cameras.push(device),
            MediaDeviceKind::Audioinput => devices.mics.push(device),
            MediaDeviceKind::Audiooutput => devices.speakers.push(device),
            _ => {}
        }
    }
    Ok(devices)
}

/// Claude: plays the camera stream into a hidden video element and crops
/// each grabbed frame to 16:9.
pub struct Camera {
    video: HtmlVideoElement,
    context: CanvasRenderingContext2d,
    stream: RefCell<Option<MediaStream>>,
}

impl Camera {
    pub fn new() -> Result<Self> {
        let video: HtmlVideoElement = document()
            .create_element("video")
            .map_err(js)?
            .unchecked_into();
        video.set_muted(true);
        video.set_attribute("playsinline", "").map_err(js)?;

        let canvas: HtmlCanvasElement = document()
            .create_element("canvas")
            .map_err(js)?
            .unchecked_into();
        canvas.set_width(WIDTH);
        canvas.set_height(HEIGHT);
        let options = Object::new();
        let _ = Reflect::set(&options, &"willReadFrequently".into(), &true.into());
        let context = canvas
            .get_context_with_context_options("2d", &options)
            .map_err(js)?
            .ok_or_else(|| anyhow::anyhow!("no 2D canvas"))?
            .unchecked_into();
        Ok(Self {
            video,
            context,
            stream: RefCell::new(None),
        })
    }

    /// Claude: stops the stream it replaces, which turns that camera off.
    pub fn set_stream(&self, stream: Option<MediaStream>) {
        self.video.set_src_object(stream.as_ref());
        if stream.is_some() {
            let _ = self.video.play();
        }
        if let Some(old) = self.stream.replace(stream) {
            stop(&old);
        }
    }

    pub fn is_on(&self) -> bool {
        self.stream.borrow().is_some()
    }

    pub fn device(&self) -> Option<String> {
        self.stream.borrow().as_ref().and_then(device)
    }

    pub fn grab(&self) -> Option<Image> {
        self.stream.borrow().as_ref()?;
        let (w, h) = (
            self.video.video_width() as f64,
            self.video.video_height() as f64,
        );
        if w == 0.0 || h == 0.0 || self.video.ready_state() < 2 {
            return None;
        }
        let crop_w = w.min(h * 16.0 / 9.0);
        let crop_h = h.min(w * 9.0 / 16.0);
        self.context
            .draw_image_with_html_video_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                &self.video,
                (w - crop_w) / 2.0,
                (h - crop_h) / 2.0,
                crop_w,
                crop_h,
                0.0,
                0.0,
                WIDTH as f64,
                HEIGHT as f64,
            )
            .ok()?;
        let rgba = self
            .context
            .get_image_data(0, 0, WIDTH as i32, HEIGHT as i32)
            .ok()?
            .data();
        let rgb = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        Some(Image::new(WIDTH as usize, HEIGHT as usize, rgb))
    }
}
