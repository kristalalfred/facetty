default:
    @just --list

build:
    cargo build --workspace

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all --check

fmt:
    cargo fmt --all

# Signaling and media server on localhost; Ctrl-C stops both.
servers:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q -p bits-signal -p bits-sfu
    trap 'kill 0' EXIT
    ./target/debug/bits-signal &
    ./target/debug/bits-sfu &
    wait

join room="lobby" *args="":
    cargo run -q -p bits -- join {{room}} {{args}}

# A headless participant that publishes a test pattern and a beeping tone.
bot room="lobby" *args="--audio tone":
    cargo run -q -p bits -- bot {{room}} {{args}}

preview *args="":
    cargo run -q -p bits -- preview {{args}}

# Print an image as ASCII, for tuning the encoder: just ascii face.png 96 edge=0.8
ascii image cols="96" *args="":
    cargo run -q --release -p bits-ascii --example render -- {{image}} {{cols}} {{args}}
