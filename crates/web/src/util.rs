use anyhow::{Result, anyhow};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Document, EventTarget, HtmlElement, Storage, Window};

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

pub fn create(tag: &str, class: &str) -> HtmlElement {
    let element: HtmlElement = document()
        .create_element(tag)
        .expect("valid tag name")
        .unchecked_into();
    if !class.is_empty() {
        element.set_class_name(class);
    }
    element
}

/// Claude: listeners live as long as the page, which a call never outlives.
pub fn on<E: JsCast + 'static>(
    target: &EventTarget,
    event: &str,
    mut handler: impl FnMut(E) + 'static,
) {
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        handler(e.unchecked_into())
    });
    let _ = target.add_event_listener_with_callback(event, closure.as_ref().unchecked_ref());
    closure.forget();
}

pub fn after(ms: i32, f: impl FnOnce() + 'static) {
    let callback = Closure::once_into_js(f);
    let _ = window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), ms);
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

pub fn now() -> f64 {
    js_sys::Date::now()
}

/// Claude: storage can be missing or throw in private windows; callers
/// treat that as nothing stored.
fn storage() -> Option<Storage> {
    window().local_storage().ok().flatten()
}

pub fn load(key: &str) -> Option<String> {
    storage()?.get_item(key).ok().flatten()
}

pub fn save(key: &str, value: &str) {
    if let Some(storage) = storage() {
        let _ = storage.set_item(key, value);
    }
}
