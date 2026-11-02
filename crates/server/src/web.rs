use axum::Router;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::IntoResponse;
use axum::routing::get;

const PAGE: &[u8] = include_bytes!("../../web/static/index.html");
const WORKLET: &[u8] = include_bytes!("../../web/static/worklet.js");
const SCRIPT: &[u8] = include_bytes!("../../web/pkg/facetty_web.js");
const WASM: &[u8] = include_bytes!("../../web/pkg/facetty_web_bg.wasm");

/// Claude: a call's page is at its code's path.
pub fn router() -> Router {
    Router::new()
        .route("/{code}", get(|| file("text/html; charset=utf-8", PAGE)))
        .route(
            "/web/facetty_web.js",
            get(|| file("text/javascript", SCRIPT)),
        )
        .route(
            "/web/facetty_web_bg.wasm",
            get(|| file("application/wasm", WASM)),
        )
        .route("/web/worklet.js", get(|| file("text/javascript", WORKLET)))
}

async fn file(content_type: &'static str, body: &'static [u8]) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, content_type), (CACHE_CONTROL, "no-cache")],
        body,
    )
}
