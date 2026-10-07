//! Video sources. Everything is center-cropped to 16:9 and scaled to
//! `WIDTH` x `HEIGHT` RGB before the ASCII analysis sees it.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bits_ascii::Image;

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 360;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Camera(String),
    Test,
    /// A file or URL that ffmpeg can open, e.g. an SRT feed from a Strom flow.
    Input(String),
}

impl Source {
    pub fn parse(spec: &str) -> Self {
        match spec {
            "test" => Source::Test,
            "camera" => Source::Camera("0".into()),
            s => match s.strip_prefix("camera:") {
                Some(device) => Source::Camera(device.into()),
                None => Source::Input(s.into()),
            },
        }
    }
}

#[derive(Default)]
struct Shared {
    latest: Mutex<Option<(u64, Arc<Image>)>>,
    error: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
    stop: AtomicBool,
}

pub struct Capture {
    shared: Arc<Shared>,
}

impl Capture {
    pub fn start(source: Source) -> Self {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        thread::Builder::new()
            .name("capture".into())
            .spawn(move || match source {
                Source::Test => test_pattern(&worker),
                source => run_ffmpeg(&worker, &source),
            })
            .expect("spawn capture thread");
        Self { shared }
    }

    /// The newest frame and its sequence number.
    pub fn latest(&self) -> Option<(u64, Arc<Image>)> {
        self.shared.latest.lock().unwrap().clone()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(mut child) = self.shared.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn ffmpeg_input_args(source: &Source) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    match source {
        Source::Camera(device) => {
            if cfg!(target_os = "macos") {
                args.extend(
                    [
                        "-f",
                        "avfoundation",
                        "-framerate",
                        "30",
                        "-video_size",
                        "1280x720",
                    ]
                    .map(String::from),
                );
                args.extend(["-i".into(), format!("{device}:none")]);
            } else if cfg!(target_os = "windows") {
                args.extend(["-f", "dshow", "-i"].map(String::from));
                args.push(format!("video={device}"));
            } else {
                let path = if device.starts_with('/') {
                    device.clone()
                } else {
                    format!("/dev/video{device}")
                };
                args.extend(["-f", "v4l2", "-framerate", "30", "-i"].map(String::from));
                args.push(path);
            }
        }
        Source::Input(input) if input.contains("://") => {
            args.extend(["-i".into(), input.clone()]);
        }
        Source::Input(path) => {
            args.extend(["-re", "-stream_loop", "-1", "-i"].map(String::from));
            args.push(path.clone());
        }
        Source::Test => unreachable!("test pattern does not use ffmpeg"),
    }
    args
}

fn run_ffmpeg(shared: &Shared, source: &Source) {
    let filter = format!("crop='min(iw,ih*16/9)':'min(ih,iw*9/16)',scale={WIDTH}:{HEIGHT}");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(ffmpeg_input_args(source))
        .args([
            "-an", "-vf", &filter, "-pix_fmt", "rgb24", "-f", "rawvideo", "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            *shared.error.lock().unwrap() = Some(format!("could not start ffmpeg: {e}"));
            return;
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    *shared.child.lock().unwrap() = Some(child);

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let tail_writer = tail.clone();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut tail = tail_writer.lock().unwrap();
            if tail.len() == 4 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    });

    let mut seq = 0u64;
    loop {
        let mut rgb = vec![0u8; WIDTH * HEIGHT * 3];
        if stdout.read_exact(&mut rgb).is_err() {
            break;
        }
        seq += 1;
        *shared.latest.lock().unwrap() = Some((seq, Arc::new(Image::new(WIDTH, HEIGHT, rgb))));
    }
    if !shared.stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
        let detail: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
        *shared.error.lock().unwrap() = Some(if detail.is_empty() {
            "ffmpeg exited".to_string()
        } else {
            format!("ffmpeg: {}", detail.join(" / "))
        });
    }
}

fn test_pattern(shared: &Shared) {
    let start = Instant::now();
    let mut seq = 0u64;
    while !shared.stop.load(Ordering::Relaxed) {
        let t = start.elapsed().as_secs_f32();
        seq += 1;
        *shared.latest.lock().unwrap() = Some((seq, Arc::new(draw_test_pattern(t))));
        thread::sleep(Duration::from_millis(33));
    }
}

/// A slow color gradient with a bouncing ball and a spinning bar, so both
/// fill and every edge direction show up.
pub fn draw_test_pattern(t: f32) -> Image {
    let (w, h) = (WIDTH as f32, HEIGHT as f32);
    let ball = (
        w * (0.5 + 0.35 * (t * 0.9).sin()),
        h * (0.5 + 0.3 * (t * 1.3).cos()),
    );
    let radius = h * 0.18;
    let bar_center = (w * 0.25, h * 0.5);
    let (sin, cos) = (t * 0.8).sin_cos();
    let mut rgb = Vec::with_capacity(WIDTH * HEIGHT * 3);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let (fx, fy) = (x as f32, y as f32);
            let base = 0.15 + 0.35 * fx / w;
            let mut c = [
                base * (0.6 + 0.4 * (t * 0.3).sin()),
                base * 0.8,
                base * (0.6 + 0.4 * (t * 0.2).cos()),
            ];
            let (dx, dy) = (fx - ball.0, fy - ball.1);
            let d = (dx * dx + dy * dy).sqrt();
            if d < radius {
                let shade = 1.0 - 0.6 * d / radius;
                c = [1.0 * shade, 0.75 * shade, 0.2 * shade];
            }
            let (bx, by) = (fx - bar_center.0, fy - bar_center.1);
            let along = bx * cos + by * sin;
            let across = -bx * sin + by * cos;
            if along.abs() < h * 0.3 && across.abs() < h * 0.04 {
                c = [0.3, 0.9, 1.0];
            }
            rgb.extend(c.map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8));
        }
    }
    Image::new(WIDTH, HEIGHT, rgb)
}

/// Camera names as ffmpeg reports them, best effort.
pub fn list_cameras() -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let Ok(out) = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-f",
            "avfoundation",
            "-list_devices",
            "true",
            "-i",
            "",
        ])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines()
        .skip_while(|l| !l.contains("video devices"))
        .skip(1)
        .take_while(|l| !l.contains("audio devices"))
        .filter_map(|l| l.split_once("] [").map(|(_, rest)| format!("[{rest}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources() {
        assert_eq!(Source::parse("test"), Source::Test);
        assert_eq!(Source::parse("camera"), Source::Camera("0".into()));
        assert_eq!(Source::parse("camera:2"), Source::Camera("2".into()));
        assert_eq!(
            Source::parse("srt://example.com:9000"),
            Source::Input("srt://example.com:9000".into())
        );
    }

    #[test]
    fn test_source_produces_frames() {
        let capture = Capture::start(Source::Test);
        let deadline = Instant::now() + Duration::from_secs(2);
        while capture.latest().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let (_, img) = capture.latest().expect("a frame");
        assert_eq!((img.width, img.height), (WIDTH, HEIGHT));
    }
}
