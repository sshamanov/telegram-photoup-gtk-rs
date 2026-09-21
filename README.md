# photoup2

A native GTK4/libadwaita desktop app that fixes photos that came out too dark or
with a monochrome camera picture style (NEF, CR2, DNG, JPEG, PNG) and uploads
them to a Telegram group as **inline ≤2560px, 4:4:4 mozjpeg JPEGs at Q100**
(quality lowered adaptively only to stay under Telegram's ~10 MB photo limit).

A native Rust + GTK re-implementation of the photo-fixing flow with real
parallelism: the desktop app processes many photos across cores on a bounded
worker pool.

## Why

- **Fix in one step.** Load photos → the pipeline auto-exposes them (with mild
  highlight rolloff) and, for RAW, applies the camera "Standard" tone curve and
  camera as-shot white balance. No per-photo work needed.
- **Verifiable output.** The image math is unit-tested and its results are
  checked by eye against real sample renders before every delivery.
- **Real formats.** Decodes real sensor data from NEF/CR2/DNG through LibRaw,
  plus JPEG/PNG.
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

Loose coupling via adapters: the `TelegramAdapter` trait is the contract the
integration tests speak, implemented by `MockAdapter` (`tests/telegram_mock.rs`);
the real path drives `GrammersSession` from the Telegram worker. The image
pipeline is photoup2's own linear-light implementation.

**Pipeline:** decode once → cached linear base → re-render from the base on
every edit (never cumulative) → ≤512px grid thumbnails / 512px live + 1024px
settled editor preview / 2560px export at send. Each render applies, in a fixed
order: exposure + white balance in linear → tone curve → luma-preserving
saturation → **black point last** (levels, for the RAW frames whose histogram
starts above zero; **manual only** — the Auto/Burn exposure modes set the EV and
never touch it). Export is fixed: 4:4:4 mozjpeg Q100, adaptive to fit ~10 MB —
**not user-tunable by design**.

## Build / run

System deps (this is a native app, so it installs system packages):

```bash
# Arch
sudo pacman -S gtk4 libadwaita base-devel
# Debian/Ubuntu
sudo apt install libgtk-4-dev libadwaita-1-dev build-essential
```

`libraw-rs-sys` compiles a vendored LibRaw from source (needs the C++ compiler
from `base-devel`/`build-essential`), so there is **no** `libraw-dev` package
and no runtime RAW dependency.

```bash
cargo build        # debug
cargo run          # launch the app
cargo test         # unit + integration tests (telegram uses the mock adapter)
```

`libadwaita` is pinned to feature `v1_5` so the same `Cargo.toml` builds on both
Ubuntu 24.04 and Arch.

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

The full UI spec is the authority document:

**`docs/ui-spec.md`**

In short: Login (phone/code/2FA) → Main screen (header with group selector +
**Reset** + **Logout**; upload zone; "Processing {name} (n queued)" usage
indicator; a square-thumbnail grid with checkbox top-left, EV badge top-right,
filename + RAW/JPG badge) → Editor (preview with crop overlay + right panel:
RGB histogram, **Exposure · Black point** [EV slider + black-point slider, each
with its value at the end of its track, then Auto/Burn/Rest], **White balance ·
Tint · Saturation** [temp + tint + saturation sliders, then
Auto/Auto2/Reset/Picker], Crop [1:1/2:3/3:2/Original/Pix], Rotate
[↺ CCW/↻ CW], Image [camera / lens / shutter · aperture · ISO / date, then
`RAW|JPEG · W × H` and `output W × H px`], Prev/Next, Reject/Close) → sticky
**Send {n} selected** footer (Preparing/Sending progress; sent photos are
removed after a successful send).

Keyboard: Ctrl+V pastes photos, Ctrl+A toggles select-all, Enter opens the
editor for the selected cell, ←/→ navigate photos in the editor, Esc closes it,
and Q/W (exposure), A/S (warmth), Z/X (tint) fine-tune the active photo.

Known gaps (tracked in `docs/ui-spec.md`): QR login is unavailable (grammers
0.10); the grid's checkbox checkmark and selected-cell accent, and the editor's
dimmed backdrop, are deferred.

## Development

- `PHOTOUP2_DEV=1 cargo run` — skips Telegram auth and auto-loads `./samples/*`
  so the grid/editor can be exercised without logging in.
- `RUST_LOG=info cargo run` — prints `[timing]` lines for the thumb/preview/
  export decode–render–encode phases and the send upload, so bottlenecks are
  measurable.
- Use `cargo run --release` for realistic performance. The debug build runs the
  pure-Rust JPEG decoder ~10× slower (a 36MP JPG thumbnail takes ~7 s in debug,
  <1 s in release); libraw is compiled C++ and is fast in both.
- Screenshots for visual checks: run the release binary on a dedicated
  `Xvfb :99 -screen 0 1600x1200x24` with `GDK_BACKEND=x11`, then
  `DISPLAY=:99 import -window <id> out/x.png`. Capturing the developer's real
  Wayland desktop is not used — it would leak unrelated windows.

## Accepted limitations

- **Progressive, not baseline, JPEG.** mozjpeg's Rust binding has no baseline
  switch and defaults to progressive. Scan order is pixel-neutral, so decoded
  pixels are unaffected by the progressive scan order.
- **RAW access-hash caveat for supergroups/channels.** Sending to a
  channel/supergroup requires the peer's access hash. It is present for most
  dialogs, but a plain basic group or a minimal user may carry `None`; a send
  that lacks the hash will fail until it is available.
- **grammers 0.10 dropped QR login.** Login is phone + code + 2FA only (no QR
  flow).

## Verification

Image output is verified by looking at real renders — the developer reads the
rendered PNGs directly; there is no external vision service in the loop. The
user's real NEF/CR2/DNG/JPEG samples live in `./samples/` (gitignored, local
only) and are used to confirm RAW decode quality. The RAW decode tests in
`src/image/decode.rs` use a sample file automatically when one is present.
