# facetty

Video calls with colored ASCII video and audio, in a terminal or a browser.

## Install

With Homebrew on macOS or Linux:

```sh
brew install kristalalfred/tap/facetty
```

Without Homebrew on macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/KristalAlfred/facetty/releases/latest/download/facetty-installer.sh | sh
```

Windows, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/KristalAlfred/facetty/releases/latest/download/facetty-installer.ps1 | iex"
```

Archives for each platform are on the [releases page][releases]. On Linux the
client needs ALSA (`libasound2`).

`facetty preview` shows your camera in ASCII without a server or a call.

## Join a call

```sh
facetty join <code> --server <server-url>
```

The host gives you the code and the server URL.

To join from a browser, open `<server-url>/<code>`. Browsers allow the camera,
microphone, and media connection only over `https://` or on the server's own
machine (`localhost` or `127.0.0.1`).

## Host calls

The server is `facetty-server`. Guests must reach it on TCP 8740 and UDP 8741.

```sh
brew install kristalalfred/tap/facetty-server
FACETTY_HOST_KEY=<secret> facetty-server
facetty create --host-key <secret> --server http://<host>:8740
```

Without Homebrew on macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/KristalAlfred/facetty/releases/latest/download/facetty-server-installer.sh | sh
```

With Docker:

```sh
docker run -d --name facetty --restart unless-stopped \
  -p 8740:8740 -p 8741:8741/udp -e FACETTY_HOST_KEY=<secret> \
  ghcr.io/kristalalfred/facetty-server
```

`<secret>` is any long random string, for example from `openssl rand -hex 32`.
`create` prints a call code; give guests the code and `http://<host>:8740`.
Outside a trusted LAN, [add TLS](#tls).

## Client

Upgrade with `brew upgrade kristalalfred/tap/facetty`. Homebrew refuses the
short name `facetty` from a tap you have not trusted; `brew trust
kristalalfred/tap` allows it.

Set `FACETTY_SERVER` to leave out `--server`. On macOS, allow camera,
microphone, and local-network access when prompted. Use headphones; there is no
echo cancellation. For demos, `join --intro` (or `FACETTY_INTRO=1`) opens on an
intro animation; press Enter to join.

Press `d` in a call or preview to switch camera, microphone, or speaker.
`facetty devices` lists devices and their selection flags; `--video
camera:<index>` also takes part of a camera's name. `--video` also accepts
`test`, and with `ffmpeg` installed, a file (looped) or a URL.

## Server

`compose.yaml` in this repository runs the server container, limited to 512 MB
of memory and one CPU, and passes `FACETTY_HOST_KEY`, `FACETTY_OPEN`,
`FACETTY_MAX_CALLS`, and `FACETTY_MAX_PARTICIPANTS` through:
`FACETTY_HOST_KEY=<secret> docker compose up -d`.

| Setting | Environment variable | Default |
|---|---|---|
| HTTP listener | `FACETTY_LISTEN` | `0.0.0.0:8740` (TCP) |
| Media listener | `FACETTY_MEDIA_LISTEN` | `0.0.0.0:8741` (UDP) |
| Media address for clients | `FACETTY_MEDIA_ADDR` | Server host with the media listener's port |
| Key for creating calls | `FACETTY_HOST_KEY` | Generated and printed at startup |
| Let anyone create calls | `FACETTY_OPEN` | `false` |
| Most calls held at once | `FACETTY_MAX_CALLS` | `1000` |
| Most people in calls at once | `FACETTY_MAX_PARTICIPANTS` | `64` |

`FACETTY_MEDIA_ADDR` takes `host:port`, or `:port` to keep the host clients
used. Set it when clients reach UDP on a different port than the server listens
on, for example through a port mapping.

### TLS

Put TCP 8740 behind a TLS proxy so host keys and call codes are encrypted, and
give guests the `https://` URL. Browser guests need it to use their camera and
microphone.

The `tls` profile in `compose.yaml` adds Caddy, which gets a certificate for
`FACETTY_DOMAIN` and needs TCP 80 and 443 open:

```sh
FACETTY_DOMAIN=calls.example.com FACETTY_HOST_KEY=<secret> docker compose --profile tls up -d
```

Guests then use `https://calls.example.com`. With Caddy installed separately:

```
calls.example.com {
    reverse_proxy 127.0.0.1:8740
}
```

The proxy must pass the `Host` header through, as Caddy does by default:
clients find the media port on the host they used to reach the server. Media
uses QUIC over UDP and does not go through the proxy.

### Call permissions

With `FACETTY_OPEN=true`, anyone can create calls without a host key. When a
server holds `FACETTY_MAX_CALLS` calls, creating one drops the oldest call
nobody is in, and fails if every call has someone in it.

A call holds up to 16 people. Create more calls with the same host key and
server URL. Each code admits guests only to its own call; room names cannot
create or join calls. Chat, participant lists, and media stay within that call.

Codes accept new participants for 24 hours, including after everyone leaves.
Already connected participants can continue after expiry. Restarting the
server clears all codes. Anyone with a code can join that call; keep the host
key private.

## Development

Requires Rust and `just`. On Linux, building also needs `libasound2-dev`,
`libclang-dev`, and `pkg-config` (Debian and Ubuntu package names). `just build`
and `just web` also need `wasm-bindgen-cli` at the version of `wasm-bindgen` in
`Cargo.lock`.

```sh
just server        # terminal 1
just create --host-key <key-printed-by-server>  # terminal 2
just join <code>
```

To add a test participant, run `just bot <code> --video test --audio tone` in
another terminal. `just share` builds release binaries, starts the server with a
random host key, creates a call, and prints join commands for your network;
`just share 8750` uses ports 8750/8751.

`just demo [guest]` starts a local server on ports 8750/8751, adds a guest, and
joins you through the intro. The guest shows `test` (the default),
`camera:<index or name>`, or a video file with its audio. Later arguments go to
`facetty join`, for example `just demo test --no-audio`.

`just build`, `just test`, and `just lint` build and check the workspace.
`just web` builds the browser client into `crates/web/pkg`, which the server
embeds; `just lint` fails when that build is out of date.

`facetty-signal` manages rooms and chat over WebSocket; `facetty-sfu` forwards
video and Opus audio over QUIC, directly from the terminal client and as
WebTransport from browsers; `facetty-server` runs both in one process and
serves `facetty-web`, the browser client. The
ASCII encoder is a CPU port of [Acerola's ASCII shader][AcerolaFX_ASCII.fx]
(MIT, see `LICENSE-AcerolaFX`).

The intro is a Blender scene built by `splash/phosphor.py`. Its frames go
through the same encoder, with the shader's depth and normal edges added from
Blender's render passes, and are baked into `crates/client/assets/splash.reel`.
`just splash` renders and bakes it again; it needs Blender (set `BLENDER` if it
is not in `/Applications`) and the Arial Black font that macOS ships.

## Releasing

Bump `version` in `Cargo.toml`, commit, and push a tag such as `v0.2.0`. The
Release workflow, generated by [dist], builds the client and server archives
and installers, publishes a GitHub release, and pushes the `facetty` and
`facetty-server` Homebrew formulas to `KristalAlfred/homebrew-tap`. The Docker workflow pushes the server image to
`ghcr.io/kristalalfred/facetty-server`.

`release.yml` differs from what `dist generate` writes in one line: the tap
checkout uses the `HOMEBREW_TAP_DEPLOY_KEY` secret, a deploy key with write
access to the tap only, instead of a token. `allow-dirty = ["ci"]` in
`dist-workspace.toml` keeps dist from overwriting it, so after changing dist
settings or upgrading dist, remove that line, run `dist generate`, and put the
`ssh-key:` line back.

## License

MIT, see `LICENSE`.

## Limits

- Camera capture has only been run on macOS. The Linux (V4L2, YUYV only) and
  Windows (Media Foundation) cameras build but have not been tried.
- Microphone capture is untested.
- A server restart ends calls and clears call codes.
- The browser client has only been run in Chrome.

[releases]: https://github.com/KristalAlfred/facetty/releases
[dist]: https://github.com/axodotdev/cargo-dist
[AcerolaFX_ASCII.fx]: https://github.com/GarrettGunnell/AcerolaFX/blob/main/Shaders/AcerolaFX_ASCII.fx
