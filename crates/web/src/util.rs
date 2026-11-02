use anyhow::{Result, anyhow};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Document, HtmlElement, Window};

pub fn window() -> Window {
    web_sys::window().expect("running in a window")
}

pub fn document() -> Document {
    window().document().expect("window has a document")
}

pub fn element<T: JsCast>(id: &str) -> Result<T> {
    document()
        .get_element_by_id(id)
        .and_then(|e| e.dyn_into::<T>().ok())
        .ok_or_else(|| anyhow!("page has no #{id}"))
}

pub fn js(e: JsValue) -> anyhow::Error {
    let text = e
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"));
    anyhow!(text)
}

pub async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = JsFuture::from(promise).await;
}

pub fn show_message(text: &str) {
    if let Ok(message) = element::<HtmlElement>("message") {
        message.set_text_content(Some(text));
        message.set_hidden(false);
    }
}
