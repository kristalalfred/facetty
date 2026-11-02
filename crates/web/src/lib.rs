pub mod layout;

#[cfg(target_arch = "wasm32")]
mod audio;
#[cfg(target_arch = "wasm32")]
mod call;
#[cfg(target_arch = "wasm32")]
mod chat;
#[cfg(target_arch = "wasm32")]
mod media;
#[cfg(target_arch = "wasm32")]
mod publish;
#[cfg(target_arch = "wasm32")]
mod render;
#[cfg(target_arch = "wasm32")]
mod signal;
#[cfg(target_arch = "wasm32")]
mod stage;
#[cfg(target_arch = "wasm32")]
mod transport;
#[cfg(target_arch = "wasm32")]
mod ui;
#[cfg(target_arch = "wasm32")]
mod util;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    wasm_bindgen_futures::spawn_local(call::run());
}
