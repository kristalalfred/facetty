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
    cargo run -q -p facetty-server -- {{args}}

# Host a LAN call and print join commands.
share port="8740":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q --release -p facetty -p facetty-server
    ip=$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || hostname -I | awk '{print $1}')
    export FACETTY_HOST_KEY=$(openssl rand -hex 32)
    export FACETTY_LISTEN=0.0.0.0:{{port}}
    export FACETTY_MEDIA_LISTEN=0.0.0.0:$(({{port}} + 1))
    trap 'kill 0' EXIT
    ./target/release/facetty-server &
    for attempt in {1..50}; do
        if curl -fsS "http://127.0.0.1:{{port}}/healthz" >/dev/null 2>&1; then
            break
        fi
        sleep 0.1
    done
    code=$(./target/release/facetty create --server http://127.0.0.1:{{port}})
    echo
    echo "  others run:  facetty join $code --server http://$ip:{{port}}"
    echo "  you run:     ./target/release/facetty join $code --server http://127.0.0.1:{{port}}"
    echo "  more calls:  ./target/release/facetty create --host-key $FACETTY_HOST_KEY --server http://127.0.0.1:{{port}}"
    echo
    wait

# Join a local call on the intro, with a guest showing test, camera:<n>, or a video file.
demo guest="test" *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q --release -p facetty -p facetty-server
    export FACETTY_SERVER=http://127.0.0.1:8750
    if curl -fsS "$FACETTY_SERVER/healthz" >/dev/null 2>&1; then
        echo "port 8750 is in use" >&2
        exit 1
    fi
    trap 'kill $(jobs -p) 2>/dev/null' EXIT
    FACETTY_OPEN=true FACETTY_LISTEN=127.0.0.1:8750 FACETTY_MEDIA_LISTEN=127.0.0.1:8751 \
        ./target/release/facetty-server >/dev/null 2>&1 &
    server=$!
    until curl -fsS "$FACETTY_SERVER/healthz" >/dev/null 2>&1; do
        kill -0 $server 2>/dev/null || { echo "server failed to start" >&2; exit 1; }
        sleep 0.1
    done
    code=$(./target/release/facetty create)
    audio=""
    if [[ -f "{{guest}}" ]]; then audio="{{guest}}"; fi
    FACETTY_NAME=guest ./target/release/facetty bot "$code" --video "{{guest}}" ${audio:+--audio "$audio"} >/dev/null 2>&1 &
    ./target/release/facetty join "$code" --intro {{args}}

create *args="":
    cargo run -q -p facetty -- create {{args}}

join code *args="":
    cargo run -q -p facetty -- join {{code}} {{args}}

# Join headless with an audio tone.
bot code *args="--audio tone":
    cargo run -q -p facetty -- bot {{code}} {{args}}

preview *args="":
    cargo run -q -p facetty -- preview {{args}}

# Render an image as ASCII.
ascii image cols="96" *args="":
    cargo run -q --release -p facetty-ascii --example render -- {{image}} {{cols}} {{args}}

blender := env("BLENDER", "/Applications/Blender.app/Contents/MacOS/Blender")

# Render the intro in Blender and bake it into the client.
splash:
    rm -rf target/splash
    "{{blender}}" -b --factory-startup --python-exit-code 1 -P splash/phosphor.py -- target/splash
    cargo run -q --release -p facetty-ascii --example bake_splash -- target/splash splash/phosphor.txt crates/client/assets/splash.reel
