# photoup2

A native GTK4/libadwaita desktop app that fixes photos that came out too dark or
with a monochrome camera picture style (NEF, CR2, JPEG, PNG) and uploads them to
a Telegram group as **inline ≤2560px, 4:4:4 mozjpeg JPEGs at Q100** (quality
lowered adaptively only to stay under Telegram's ~10 MB photo limit).

It is a port of the browser app [photoup](../photoup) — same app, same flow,
same quality thesis — to native Rust + GTK with real parallelism. The browser
version is sequential and RAM-limited; the desktop version processes many photos
across cores with a bounded memory budget.

## Why

- **Fix in one step.** Load photos → the pipeline auto-exposes them (with mild
  highlight rolloff) and, for RAW, applies the camera "Standard" tone curve and
  camera as-shot white balance. No per-photo work needed.
- **Same output as photoup, verified by eye.** The image math is ported
  verbatim from photoup's pipeline, so the desktop export matches the browser
  app pixel-for-pixel.
- **Real formats.** Decodes real sensor data from NEF/CR2 through LibRaw (same
  engine photoup used), plus JPEG/PNG.
- **Telegram-native.** Uploads as inline `photo` media (no file attachments),
  multi-photo batches as albums.

## Architecture

One Rust crate (edition 2024, no global async runtime). Three Kingdoms
threading (borrowed from mpd-client):

| Kingdom | What it does |
|---|---|
| **GTK main thread** | Display only. Owns `AppState`, mutated solely via `reduce(state, event)`. A single 50 ms poll drains channels and drives the screens. |
| **Telegram IO thread** | One persistent grammers MTProto connection on a tokio current-thread runtime. Auth, dialog fetch, photo/album sends. |
| **Image worker pool** | `nproc − 2` workers, each running a complete photo job (decode → process → encode). Single-threaded libs (libraw, mozjpeg) scale by independent jobs across workers. |

Loose coupling via adapters: the `TelegramAdapter` trait is implemented by
`GrammersSession` (real) and `MockAdapter` (tests). The image pipeline is a
faithful port of photoup's `src/lib/image/*`.

**Pipeline:** decode once → cached linear base → re-render from the base on
every edit (never cumulative) → ≤512px grid thumbnails / 1024px editor preview /
2560px export at send. Export is fixed: 4:4:4 mozjpeg Q100, adaptive to fit
~10 MB — **not user-tunable by design**.

## Build / run

System deps (native app — unlike photoup's Docker constraint, this project may
install system packages):

```bash
# Debian/Ubuntu
sudo apt install libgtk-4-dev libadwaita-1-dev libraw-dev
```

`libraw-rs-sys` statically vendors LibRaw, so no additional runtime package is
needed from the OS.

```bash
cargo build        # debug
cargo run          # launch the app
cargo test         # unit + integration tests (telegram uses the mock adapter)
```

`libadwaita` is pinned to feature `v1_5` so the same `Cargo.toml` builds on both
Ubuntu 24.04 (dev) and Arch (prod).

## Configuration

The app reads `~/.config/photoup2/config.toml` (auto-created with defaults on
first run):

```toml
api_id = 0            # your app api_id from https://my.telegram.org
api_hash = ""         # your app api_hash from https://my.telegram.org
target_peer_id = null # last chosen target group (remembered after first send)
session_path = "/home/you/.config/photoup2/telegram.session"
```

Get `api_id`/`api_hash` from [my.telegram.org](https://my.telegram.org) → API
development tools (create an app). On first launch, log in with phone number →
SMS code → 2FA password (if set); the session is persisted to the session file,
so later launches skip login.

## UI flow

The UI is a port of photoup's — same screens, captions, buttons, and layout.
The exact spec (extracted from photoup's components) is the authority document:

**`docs/ui-spec.md`**

In short: Login (phone/code/2FA) → Main screen (header with group selector +
**Reset** + **Logout**; upload zone; "Processing {name} (n queued)" usage
indicator; a square-thumbnail grid with checkbox top-left, EV badge top-right,
filename + RAW/JPG badge) → Editor (preview + right panel: histogram, Exposure
[Auto/Slide/Rest + EV], White balance [Auto/Pick/Reset + temp/hue], Crop
[1:1/2:3/3:2/Original], Image [EXIF + output size], Prev/Next, Reject/Close) →
sticky **Send {n} selected** footer (Preparing/Sending progress; sent photos
are removed after a successful send).

Known gaps vs photoup (tracked in `docs/ui-spec.md`): QR login is unavailable
(grammers 0.10); the neutral-picker (Pick) and interactive crop may be
implemented incrementally.

## Development

- `PHOTOUP2_DEV=1 cargo run` — skips Telegram auth and auto-loads `./samples/*`
  so the grid/editor can be exercised without logging in.
- `RUST_LOG=info cargo run` — prints `[timing]` lines for the thumb/preview/
  export decode–render–encode phases and the send upload, so bottlenecks are
  measurable.
- Use `cargo run --release` for realistic performance. The debug build runs the
  pure-Rust JPEG decoder ~10× slower (a 36MP JPG thumbnail takes ~7 s in debug,
  <1 s in release); libraw is compiled C++ and is fast in both.

## Accepted limitations

- **Progressive, not baseline, JPEG.** mozjpeg's Rust binding has no baseline
  switch and defaults to progressive. Scan order is pixel-neutral, so decoded
  pixels are identical to photoup's baseline output — not a quality divergence.
- **RAW access-hash caveat for supergroups/channels.** Sending to a
  channel/supergroup requires the peer's access hash. It is present for most
  dialogs, but a plain basic group or a minimal user may carry `None`; a send
  that lacks the hash will fail until it is available.
- **grammers 0.10 dropped QR login.** Login is phone + code + 2FA only (no QR
  flow).

## Verification

Image output is verified by looking at real renders — no VLM in the loop. The
user's real NEF/CR2/JPEG samples live in `./samples/` (gitignored, local only)
and are used to confirm RAW parity with photoup. The RAW decode test in
`src/image/decode.rs` uses a sample file automatically when one is present.
