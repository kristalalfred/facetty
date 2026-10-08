//! `curl http://<server>/install | sh -s -- <code>` hands out the client binary
//! that sits next to this server, built for the same OS and architecture.

use std::sync::Arc;

use axum::extract::State;
use axum::http::header::{CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::Signal;

const SCRIPT: &str = r#"#!/bin/sh
# Installs the bits client from @SERVER@. Pass a call code to join it right away.
set -eu
server="@SERVER@"
dir="${BITS_HOME:-$HOME/.bits}/bin"

case "$(uname -s)/$(uname -m)" in
  @PLATFORM@) ;;
  *) echo "bits: $server only has a client for @LABEL@, not $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$dir"
echo "downloading the bits client from $server ..."
curl -fsSL "$server/download/bits" -o "$dir/bits.part"
chmod +x "$dir/bits.part"
mv "$dir/bits.part" "$dir/bits"

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "note: ffmpeg is missing, so your camera will not work (brew install ffmpeg)" >&2
fi

if [ "$#" -gt 0 ]; then
  code="$1"
  shift
  exec "$dir/bits" join "$code" --server "$server" "$@" </dev/tty
fi
echo "installed $dir/bits"
echo "join a call with: $dir/bits join <code> --server $server"
"#;

pub(crate) async fn script(State(signal): State<Arc<Signal>>, headers: HeaderMap) -> Response {
    if signal.client_binary.is_none() {
        return (
            StatusCode::NOT_FOUND,
            "this server has no client to hand out\n",
        )
            .into_response();
    }
    let Some(host) = headers.get(HOST).and_then(|h| h.to_str().ok()) else {
        return (StatusCode::BAD_REQUEST, "missing Host header\n").into_response();
    };
    let (os, arches) = platform();
    let pattern = arches
        .iter()
        .map(|a| format!("{os}/{a}"))
        .collect::<Vec<_>>()
        .join("|");
    let body = SCRIPT
        .replace("@SERVER@", &format!("http://{host}"))
        .replace("@PLATFORM@", &pattern)
        .replace("@LABEL@", &format!("{os} {}", arches[0]));
    ([(CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], body).into_response()
}

pub(crate) async fn download(State(signal): State<Arc<Signal>>) -> Response {
    let Some(path) = &signal.client_binary else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => ([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(e) => {
            tracing::warn!(path = %path.display(), "cannot read client binary: {e}");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// `uname -s` and the `uname -m` spellings for the platform this was built for.
fn platform() -> (&'static str, &'static [&'static str]) {
    let os = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        other => other,
    };
    let arches: &[&str] = match std::env::consts::ARCH {
        "aarch64" => &["arm64", "aarch64"],
        "x86_64" => &["x86_64", "amd64"],
        _ => &[std::env::consts::ARCH],
    };
    (os, arches)
}
