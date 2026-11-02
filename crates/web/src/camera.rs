use anyhow::Result;
use facetty_ascii::Image;
use js_sys::{Object, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlVideoElement, MediaStream};

use crate::util::{document, js, window};

pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 360;

/// Claude: camera and microphone together, or whichever one the browser
/// grants.
pub async fn open() -> Option<MediaStream> {
    for (video, audio) in [(true, true), (false, true), (true, false)] {
        if let Ok(stream) = request(video, audio).await {
            return Some(stream);
        }
    }
    None
}

async fn request(video: bool, audio: bool) -> Result<MediaStream> {
    let constraints = web_sys::MediaStreamConstraints::new();
    constraints.set_video(&if video {
        video_constraints()
    } else {
        false.into()
    });
    constraints.set_audio(&if audio {
        audio_constraints()
    } else {
        false.into()
    });
    let devices = window().navigator().media_devices().map_err(js)?;
    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(js)?;
    Ok(JsFuture::from(promise).await.map_err(js)?.unchecked_into())
}

fn ideal(value: f64) -> JsValue {
    let o = Object::new();
    let _ = Reflect::set(&o, &"ideal".into(), &value.into());
    o.into()
}

fn video_constraints() -> JsValue {
    let o = Object::new();
    let _ = Reflect::set(&o, &"width".into(), &ideal(1280.0));
    let _ = Reflect::set(&o, &"height".into(), &ideal(720.0));
    let _ = Reflect::set(&o, &"frameRate".into(), &ideal(30.0));
    o.into()
}

fn audio_constraints() -> JsValue {
    let o = Object::new();
    for key in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
        let _ = Reflect::set(&o, &key.into(), &true.into());
    }
    o.into()
}

pub fn has_video(stream: &MediaStream) -> bool {
    stream.get_video_tracks().length() > 0
}

pub fn has_audio(stream: &MediaStream) -> bool {
    stream.get_audio_tracks().length() > 0
}

pub struct Camera {
    video: HtmlVideoElement,
    context: CanvasRenderingContext2d,
}

impl Camera {
    pub fn new(stream: &MediaStream) -> Result<Self> {
        let video: HtmlVideoElement = document()
            .create_element("video")
            .map_err(js)?
            .unchecked_into();
        video.set_muted(true);
        video.set_attribute("playsinline", "").map_err(js)?;
        video.set_src_object(Some(stream));
        let _ = video.play();

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
        Ok(Self { video, context })
    }

    pub fn grab(&self) -> Option<Image> {
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
