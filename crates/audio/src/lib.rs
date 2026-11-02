//! Call audio: microphone → Opus packets, and Opus packets from any number of
//! publishers → jitter buffers → mix → speakers.

#[cfg(feature = "engine")]
mod device;
#[cfg(feature = "engine")]
mod engine;
pub mod jitter;
mod mixer;
pub mod opus;
pub mod resample;

#[cfg(feature = "engine")]
pub use engine::{Engine, Input, Options, Output, list_devices, pick_device};
pub use mixer::{Mix, loudness, smooth};

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;
