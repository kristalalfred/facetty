# bits

Video calls in the terminal. Each client turns its camera into colored ASCII,
a small SFU forwards it, and every terminal in the call draws the result.

```
camera ──ffmpeg──▶ bits (TUI) ════QUIC════▶ bits-sfu ════QUIC════▶ bits (TUI)
                       │                        │ heartbeat
                       └──── WebSocket ────▶ bits-signal
                                             rooms, chat, tokens
```

## Try it

Needs Rust and `ffmpeg` (used for camera capture).

```sh
just servers          # terminal 1: signaling + media server on localhost
just join standup     # terminal 2
just bot standup      # terminal 3: a test-pattern participant that beeps
```

`just preview` shows your own camera in ASCII without a call. The first run
makes macOS ask for camera and microphone access for your terminal app.

Use headphones. There is no echo cancellation, so speakers feed back into the mic.

| key | |
|---|---|
| `m` | mute / unmute |
| `v` | camera on / off |
| `l` | grid / speaker view |
| `t` | open the chat and type, or hide it; `Enter` or `/` to type, `Esc` to stop |
| `p` | palette: vivid, natural, mono, matrix |
| `e`, `[`, `]` | edges on / off, exposure down / up (changes what others see) |
| `q` | leave |

In the chat, drag to select text; it is copied when you let go. Click a link
to open it. The mouse wheel and `PgUp`/`PgDn` scroll. bits takes the mouse
while it runs, so the terminal's own selection needs a key held while dragging
(Option in iTerm2, Shift in most other terminals).

## Calling someone on your network

```sh
just share standup
```

This builds release binaries, starts both servers with a random secret, and
prints a line like this for the other person:

```sh
curl -fsSL http://192.0.2.10:8740/install | sh -s -- standup
```

The script downloads the client into `~/.bits/bin` and joins the room. The
signaling server hands out the `bits` binary that sits next to it, so the
other machine has to be the same OS and architecture as yours. They need
ffmpeg for their camera. On recent macOS the first connection may ask to let
the terminal app access the local network. `just share standup 8750` uses
ports 8750/8751 instead.

`bits devices` lists cameras and audio devices; pick them with
`--video camera:<index>`, `--mic <name>`, `--speaker <name>`. `--video` also
takes `test` or any input ffmpeg can open (a file loops).

## Crates

| crate | |
|---|---|
| `bits-ascii` | The encoder (a CPU port of Acerola's ASCII shader) and the frame wire format |
| `bits-audio` | Mic and speaker via cpal, Opus via a pure-Rust libopus, jitter buffer, mixer |
| `bits-proto` | Wire messages, the simulcast ladder, join tokens |
| `bits-signal` | Binary. Rooms, roster, chat; assigns a media server and signs tokens |
| `bits-sfu` | Binary. Forwards video frames and audio packets over QUIC |
| `bits` | Binary. The TUI (`join`, `preview`), headless `bot`, `devices` |

## How it works

**Encoding.** Frames are center-cropped to 16:9 and scaled to 640x360. The
analysis follows the passes in [AcerolaFX_ASCII.fx]: luminance, a thresholded
difference of Gaussians, a Sobel filter for edge direction, then per cell the
most common direction becomes `| - / \` if enough pixels agree, and otherwise
a fill glyph from ` .;coPO?@█` by luminance. Two things differ from the
shader. Depth and normal edges are gone, because a webcam has no depth buffer.
The direction bins follow the slant of `/` in a cell that is twice as tall as
it is wide. The per-pixel work runs once per frame (about 4 ms at 640x360 in
release on an M-series Mac); each grid size is then read from summed-area
tables in tens of microseconds.

**Wire format.** One byte of glyph and blue, one of red and green (4 bits per
channel), zstd over the two planes. A 96x27 frame is about 2.5 KB.

**Simulcast on demand.** The ladder is every 8 columns from 16 to 264, always
16:9. Each subscriber asks the SFU for the largest size that fits the tile it
has for that publisher. The SFU tells each publisher which sizes anyone wants,
and the publisher encodes only those.

**Transport.** QUIC via quinn. A bidirectional stream carries control
messages. Each video frame gets its own unidirectional stream, and the SFU
keeps only the newest frame per subscriber when a link is slow. Audio is 20 ms
Opus packets in datagrams; the SFU forwards every stream and each client
mixes.

**Trust.** The SFU makes a self-signed certificate at startup and reports its
SHA-256 in its heartbeat to the signaling server, which hands the hash to
joining clients; clients accept only that certificate. The signaling server
signs an HMAC token (room, participant id) that the SFU checks with the same
`BITS_SECRET`.

## Running the servers elsewhere

| | env | default |
|---|---|---|
| signal listen | `BITS_SIGNAL_LISTEN` | `0.0.0.0:8740` (TCP) |
| SFU listen | `BITS_SFU_LISTEN` | `0.0.0.0:8741` (UDP) |
| SFU address given to clients | `BITS_SFU_PUBLIC_ADDR` | the host the client used to reach the signaling server, with the SFU's port |
| signaling URL the SFU registers with | `BITS_SIGNAL_URL` | `http://127.0.0.1:8740` |
| shared secret (both) | `BITS_SECRET` | an insecure dev value, with a warning |
| server for the client | `BITS_SERVER` | `http://127.0.0.1:8740` |
| client binary served at `/install` | `BITS_CLIENT_BINARY` | `bits` next to `bits-signal` |

Several SFUs can register with one signaling server; a new room goes to the
one with the fewest connections. The signaling server speaks plain HTTP; put
it behind a TLS proxy and point clients at `https://` to get `wss://`.

## Strom

[Strom] is a GStreamer flow engine that runs as a server with a web editor and
a REST API. Its shader slots take GLSL only from its own built-in library, and
its outputs are encoded video streams, so it does not fit as the per-client
encoder here. It can join a call from the outside: `bits bot <room> --video
<url>` publishes anything ffmpeg can open, so a Strom flow with an SRT output
should work as `--video srt://host:port`. That needs an ffmpeg built with
libsrt, and it has not been tried.

## Known gaps

- No echo cancellation.
- Real cameras and microphones have not been run yet. Calls have been tested
  with the test pattern, video files, and generated audio. The camera
  arguments for Linux (v4l2) and Windows (dshow) are guesses.
- If the SFU restarts, clients in a call are dropped and have to rejoin.
- Rooms are open: anyone who knows the room name can join.

[AcerolaFX_ASCII.fx]: https://github.com/GarrettGunnell/AcerolaFX/blob/main/Shaders/AcerolaFX_ASCII.fx
[Strom]: https://github.com/eyevinn/strom
