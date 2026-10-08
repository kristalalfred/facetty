//! Cameras through the operating system: AVFoundation on macOS, V4L2 on
//! Linux, Media Foundation on Windows.

use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow};
use facetty_ascii::Image;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub fn list() -> Vec<String> {
        Vec::new()
    }

    pub fn run(_: &str, _: &AtomicBool, _: impl FnMut(Image)) -> Result<()> {
        anyhow::bail!("cameras are not supported on this platform")
    }
}

/// Camera names, in the order `camera:<index>` counts them.
pub fn list() -> Vec<String> {
    platform::list()
}

/// Captures from `device`, an index or part of a camera's name, until `stop`
/// is set. Frames arrive already fitted to the capture size.
pub fn run(device: &str, stop: &AtomicBool, on_frame: impl FnMut(Image)) -> Result<()> {
    platform::run(device, stop, on_frame)
}

pub fn pick(names: &[String], device: &str) -> Result<usize> {
    let found = match device.parse::<usize>() {
        Ok(index) => (index < names.len()).then_some(index),
        Err(_) => {
            let wanted = device.to_lowercase();
            names
                .iter()
                .position(|n| n.to_lowercase().contains(&wanted))
        }
    };
    found.ok_or_else(|| match names.len() {
        0 => anyhow!("no camera found"),
        _ => anyhow!("no camera matches \"{device}\" (`facetty devices` lists them)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_by_index_or_name() {
        let names = ["FaceTime HD Camera".to_string(), "Desk View".to_string()];
        assert_eq!(pick(&names, "1").unwrap(), 1);
        assert_eq!(pick(&names, "facetime").unwrap(), 0);
        assert!(pick(&names, "2").is_err());
        assert!(pick(&names, "webcam").is_err());
    }
}
