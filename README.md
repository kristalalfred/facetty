# bits

Terminal video calls with colored ASCII video and audio.

## Try it

Requires Rust, `just`, and `ffmpeg`.

```sh
just servers       # terminal 1
just join standup  # terminal 2
```

`just preview` checks your camera without joining a call. To add a test
participant, run `just bot standup --video test --audio tone` in another terminal.

On macOS, allow camera, microphone, and local-network access when prompted.
Use headphones; there is no echo cancellation.

## Share a call on your network

```sh
just share standup
```

Builds release binaries, starts both servers with a random secret, and prints
join commands. Guests download the client to `~/.bits/bin`; they need the same
OS and architecture as the host, plus `ffmpeg` for camera capture.
`just share standup 8750` uses ports 8750/8751.

`bits devices` lists devices and their selection flags. `--video` also accepts
`test`, a file (looped), or a URL supported by ffmpeg.

## Hosting

Run `bits-signal` and `bits-sfu` with the same `BITS_SECRET`.

| Setting | Environment variable | Default |
|---|---|---|
| Signaling listener | `BITS_SIGNAL_LISTEN` | `0.0.0.0:8740` (TCP) |
| Media listener | `BITS_SFU_LISTEN` | `0.0.0.0:8741` (UDP) |
| Media address for clients | `BITS_SFU_PUBLIC_ADDR` | Signaling host with the media listener's port |
| SFU registration URL | `BITS_SIGNAL_URL` | `http://127.0.0.1:8740` |
| Shared server secret | `BITS_SECRET` | Insecure development value |
| Client's signaling URL | `BITS_SERVER` | `http://127.0.0.1:8740` |
| Downloadable client | `BITS_CLIENT_BINARY` | `bits` beside `bits-signal` |

For HTTPS, put signaling behind a TLS proxy and use an `https://` client URL.
Media uses QUIC over UDP.

## Development

`just build`, `just test`, and `just lint` build and check the workspace.

`bits-signal` manages rooms and chat over WebSocket; `bits-sfu` forwards video
and Opus audio over QUIC. The ASCII encoder is a CPU port of
[Acerola's ASCII shader][AcerolaFX_ASCII.fx].

## Limits

- Camera and microphone capture are untested. Linux and Windows camera
  arguments are unverified.
- An SFU restart disconnects callers; rejoin manually.
- Anyone who knows a room name can join.

[AcerolaFX_ASCII.fx]: https://github.com/GarrettGunnell/AcerolaFX/blob/main/Shaders/AcerolaFX_ASCII.fx
