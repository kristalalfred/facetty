# facetty

Terminal video calls with colored ASCII video and audio.

## Try it

Requires Rust and `just`. On Linux, building also needs `libasound2-dev`,
`libclang-dev`, and `pkg-config` (Debian and Ubuntu package names).

```sh
just server        # terminal 1
just create --host-key <key-printed-by-server>  # terminal 2
just join <code>
```

`just preview` checks your camera without joining a call. To add a test
participant, run `just bot <code> --video test --audio tone` in another terminal.

On macOS, allow camera, microphone, and local-network access when prompted.
Use headphones; there is no echo cancellation.

## Share a call on your network

```sh
just share
```

Builds release binaries, starts the server with a random host key, creates a
call, and prints join commands. Guests need the `facetty` client.
`just share 8750` uses ports 8750/8751.

`facetty devices` lists devices and their selection flags; `--video camera:<index>`
also takes part of a camera's name. `--video` also accepts `test`, and with
`ffmpeg` installed, a file (looped) or a URL.

## Call permissions

`facetty create --host-key <host-key> --server <url>` prints a random call code.
Share it with guests, who run `facetty join <code> --server <url>`. The host key
can also come from `FACETTY_HOST_KEY`.

Create more calls with the same host key and server URL. Each code admits guests
only to its own call; room names cannot create or join calls. Chat, participant
lists, and media stay within that call.

Codes accept new participants for 24 hours, including after everyone leaves.
Already connected participants can continue after expiry. Restarting the
server clears all codes. Anyone with a code can join that call; keep the host
key private.

## Hosting

`facetty-server` serves call codes, rosters and chat over HTTP and WebSocket, and
forwards video and audio over QUIC. Set `FACETTY_HOST_KEY`, for example from
`openssl rand -hex 32`, to keep the host key across restarts; otherwise the
server generates and prints a new key at startup.

| Setting | Environment variable | Default |
|---|---|---|
| HTTP listener | `FACETTY_LISTEN` | `0.0.0.0:8740` (TCP) |
| Media listener | `FACETTY_MEDIA_LISTEN` | `0.0.0.0:8741` (UDP) |
| Media address for clients | `FACETTY_MEDIA_ADDR` | Server host with the media listener's port |
| Key for creating calls | `FACETTY_HOST_KEY` | Generated at startup |
| Client's server URL | `FACETTY_SERVER` | `http://127.0.0.1:8740` |

Outside a trusted LAN, put the HTTP listener behind a TLS proxy and use an
`https://` client URL to protect host keys and call codes. Media uses QUIC over
UDP.

## Development

`just build`, `just test`, and `just lint` build and check the workspace.

`facetty-signal` manages rooms and chat over WebSocket; `facetty-sfu` forwards video
and Opus audio over QUIC; `facetty-server` runs both in one process. The ASCII encoder is a CPU port of
[Acerola's ASCII shader][AcerolaFX_ASCII.fx].

## Limits

- Camera capture has only been run on macOS. The Linux (V4L2, YUYV only) and
  Windows (Media Foundation) cameras build but have not been tried.
- Microphone capture is untested.
- A server restart ends calls and clears call codes.

[AcerolaFX_ASCII.fx]: https://github.com/GarrettGunnell/AcerolaFX/blob/main/Shaders/AcerolaFX_ASCII.fx
