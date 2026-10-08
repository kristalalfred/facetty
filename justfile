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

# Run the server; Ctrl-C stops it.
server *args="":
    cargo run -q -p bits-server -- {{args}}

# Host a LAN call and print join commands.
share port="8740":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q --release -p bits -p bits-server
    ip=$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || hostname -I | awk '{print $1}')
    export BITS_HOST_KEY=$(openssl rand -hex 32)
    export BITS_LISTEN=0.0.0.0:{{port}}
    export BITS_MEDIA_LISTEN=0.0.0.0:$(({{port}} + 1))
    trap 'kill 0' EXIT
    ./target/release/bits-server &
    for attempt in {1..50}; do
        if curl -fsS "http://127.0.0.1:{{port}}/healthz" >/dev/null 2>&1; then
            break
        fi
        sleep 0.1
    done
    code=$(./target/release/bits create --server http://127.0.0.1:{{port}})
    echo
    echo "  others run:  bits join $code --server http://$ip:{{port}}"
    echo "  you run:     ./target/release/bits join $code --server http://127.0.0.1:{{port}}"
    echo "  more calls:  ./target/release/bits create --host-key $BITS_HOST_KEY --server http://127.0.0.1:{{port}}"
    echo
    wait

create *args="":
    cargo run -q -p bits -- create {{args}}

join code *args="":
    cargo run -q -p bits -- join {{code}} {{args}}

# Join headless with an audio tone.
bot code *args="--audio tone":
    cargo run -q -p bits -- bot {{code}} {{args}}

preview *args="":
    cargo run -q -p bits -- preview {{args}}

# Render an image as ASCII.
ascii image cols="96" *args="":
    cargo run -q --release -p bits-ascii --example render -- {{image}} {{cols}} {{args}}
