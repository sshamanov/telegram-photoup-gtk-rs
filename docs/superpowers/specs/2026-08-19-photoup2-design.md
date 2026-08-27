# photoup2 — Desktop Telegram Photo Uploader, Design

Date: 2026-08-19
Status: approved
Author: discussion between schaman and Claude Code

> **Historical note (2026-08-27):** this is the original inception design. The
> app has since diverged from the "faithful photoup port" premise below — it is
> now photoup2's own implementation (own exposure/WB math, editor rotation, WB
> Auto2, diverged UI). Treat the parity claims in this document as context on
> how the project started, NOT as current requirements. The living docs are
> `CLAUDE.md`, `README.md`, and `docs/ui-spec.md`.

## Background

photoup is a working browser app that fixes photos which came out too dark or
with a monochrome camera picture style, then uploads them to a Telegram group.
Its quality thesis: decode the real sensor data, process, and ship a **≤2560px,
4:4:4 mozjpeg JPEG at Q100, lowered only to stay under Telegram's ~10 MB photo
limit**. This pipeline is proven and now **final** — not a knob to expose.

The browser implementation is slow and RAM-limited by design (sequential,
single-threaded processing to preserve memory). The desktop port keeps the exact
same app and flow, but:

- GTK (native) instead of JS/Svelte for the UI.
- Rust instead of WASM for image manipulation.
- Real parallelism: multiple photos processed across cores.
- More RAM available, with a still-bounded memory budget.

## Scope

**Full photoup parity, as a native GTK4 desktop app on Linux.** Same source
formats (NEF, CR2, JPEG, PNG), same flow (load → auto-process → grid → editor →
batch send to a chosen Telegram group), same output (inline ≤2560px, 4:4:4
mozjpeg, adaptive quality).

## Goals

- Same app as photoup, but fast and responsive on the desktop.
- Parallel image pipeline (`nproc − 2` workers) so decode/process/encode scale
  across cores, while single-threaded-by-design libraries (libraw, mozjpeg)
  are scaled by running independent photo jobs across workers — never by
  threading inside the libraries.
- Bounded memory: decode-on-demand, release buffers on close/send/clear, live
  usage indicator.
- Visual parity with photoup: **auto vs manual adjustment split preserved**
  (see Adjustments).

## Non-goals

- No encode-parameter experimentation UI — the 4:4:4 Q100→adaptive 2560px
  settings are final.
- No platform other than Linux (GTK4, X11/Wayland).
- No telemetry, no network except Telegram itself.

## Tech stack

Single Rust crate (edition 2024, MSRV ~1.85 — same as mpd-client). No async
runtime; `std::thread` over tokio (small binary, no tokio dependency).

| Concern                    | Crate / tool          | Notes |
|----------------------------|-----------------------|-------|
| UI                         | `gtk4` 0.11 (`v4_14`), `libadwaita` 0.9 (`v1_7`) | same versions as mpd-client |
| Telegram (user account)    | `grammers`            | pure-Rust MTProto; phone/QR/2FA; session to file |
| RAW decode (NEF/CR2)       | `libraw-rs` (LibRaw)  | same engine as photoup (via WASM) → color/WB parity; needs system `libraw-dev` |
| JPEG/PNG decode            | `image`               | already used by mpd-client |
| Resize → ≤2560             | `fast_image_resize`   | SIMD downscale |
| JPEG encode (4:4:4)        | `mozjpeg`             | same encoder photoup used (@jsquash/mozjpeg) |
| Config                     | `toml`, `dirs`        | `~/.config/photoup2/config.toml` |
| Logging                    | `log` + `env_logger`  | mpd-client stack |

**Fallback note:** if `libraw-rs` proves problematic, `rawloader` (pure Rust,
dcraw-derived) is the fallback for RAW decode — but it may shift color/WB vs
photoup and must be visually checked against photoup output before adopting.

## Architecture: thread model

mpd-client's Three Kingdoms model, re-targeted:

- **GTK main thread** — display only. Owns `AppState`; all mutations via
  `reduce(AppState, Event)`. Renders thumbnails/preview as `GdkTexture`. No
  blocking I/O, no decode, no encode, no Telegram I/O.
- **Telegram IO thread** — one persistent grammers MTProto connection (≈
  mpd-client's MPD IO thread). Handles auth state, dialog fetch, and photo
  sends. Receives commands via channel, emits events via channel. Never blocked
  by image work.
- **Image pipeline pool** — `nproc − 2` worker threads. Each worker runs a
  complete photo job end-to-end: decode (libraw / image) → process → thumbnail
  render; and on the send path: process → ≤2560 resize → mozjpeg 4:4:4 encode.
  Single-threaded libraries are scaled by independent jobs across workers.
  In-flight jobs are bounded by a channel of capacity = worker count → the
  memory budget. Per-photo order preserved for the batch.

State and communication:

- `Arc<RwLock<AppState>>` — read by GTK widgets, written only by `reduce()` on
  the main thread.
- `std::sync::mpsc` channels carry pre-processed data (RGBA textures, final
  JPEG bytes, events) — no serialization in the hot path.
- No shared mutable state across threads beyond `SharedState` and the shutdown
  flag; prefer message passing over locks.
- Deadlock prevention: no nested lock acquisitions; locks held briefly.

## Image pipeline

photoup's discipline, now parallel:

1. **Decode once** → cached linear base. RAW: LibRaw camera-WB 16-bit linear
   RGB. JPEG/PNG: `image` decode → linear.
2. **Adjust at render time** (see Adjustments) — never re-decode, never
   process cumulatively from an edited preview. Always re-render from the
   original base.
3. **Render** — ≤512px thumbnail updates the grid/editor on every edit;
   ≤2560px export rendered once at send.
4. **Encode export — final settings:** 4:4:4 mozjpeg Q100, adaptive quality
   (max that fits ~10 MB), ≤2560px, sent as inline `photo`; multi-photo batches
   sent as an album.
5. Reprocessing always from the original decoded source. Decode-on-demand,
   release decoded buffers on close/send/clear, live usage indicator.

## Adjustments — auto vs manual

**Automatic on upload, zero user action:**
- **Auto exposure** (standard) — immediately on upload, RAW and JPEG.
- **Mild highlight rolloff** — bundled with exposure, RAW and JPEG.
- **RAW only: camera-"Standard" S-curve** — mid-tone contrast + slight shadow
  lift, applied in sRGB so RAW isn't flat (JPEG already carries the camera's
  own tone).
- **RAW only: WB = camera-as-shot** as the default state (no correction).

**Manual, per-photo, user-driven:**
- Exposure: "Slide" aggressive auto (film-slide, hard clip, ~230 target) or a
  manual exposure slider.
- **WB (RAW only):** temperature slider, hue offset, auto-WB button
  (grey-world), neutral picker (drag to pick gray).
- **Crop** (RAW and JPEG). JPEG gets exposure + crop only — color never touched.
- **Reset / Auto** buttons in the editor.
- Selection for send (grid checkboxes, default selected).

**Display-only (automatic):** processed thumbnail previews in the grid, live
256-bin luminance histogram over the final sRGB preview, memory/usage indicator.

**Porting caveat:** the exact recipe "standard auto" computes (the EV math, and
how it differs from "Slide" minus the hard clip) lives in photoup's
`src/lib/image/math.ts`. Port it from that file so the desktop matches photoup
pixel-for-pixel; the auto/manual *boundary* above is authoritative per photoup's
docs.

## Telegram integration

grammers user client:
- Auth: QR code rendered in the UI, or phone number + code + 2FA.
- Session persisted to a file on disk (survives restarts).
- Fetch dialogs → user picks a target group; last pick remembered in config.
- Send selected photos as inline `photo` media; multi-photo batches as albums.

## UI flow (same app, native)

1. **Login** — QR or phone (with 2FA if set).
2. **Main** — target group selector; load photos (file picker and drag-and-drop);
   thumbnail grid showing processed previews, checkboxes default-selected.
3. **Editor** — per-photo: Reset / Auto / Exposure; RAW adds temperature,
   neutral picker, hue; crop (RAW + JPEG); live luminance histogram.
4. **Send** — selected photos → album to the chosen group; toast/status and
   memory/usage indicator throughout.

Same look-and-feel as photoup, GTK widgets instead of Svelte.

## Persistence

| What                     | Where |
|--------------------------|-------|
| Settings, last target group | `~/.config/photoup2/config.toml` |
| Telegram session         | session file on disk |
| Loaded photos/adjustments | in-memory only |

## Error handling

- Per-photo failures isolated: a bad file is marked in the grid; the batch
  continues.
- Telegram disconnect → reconnect with backoff; failed uploads marked/retried.
- Decode errors surface per-photo.
- No `unwrap`/`expect` in library code; callers log errors and continue.

## Testing & verification

- **Mock Telegram adapter** — grammers-shaped mock (like mpd-client's mock MPD
  server) for integration tests: auth, dialog fetch, send.
- **Image pipeline unit tests** — synthetic images (committable) for
  deterministic exposure/WB/resize/encode behavior.
- **Visual verification** — per the project rule, image output is judged by the
  user looking at real renders. When a change could visibly differ, produce
  actual before/after images (or an app screenshot) for the user to inspect.
  No VLM in the loop.
- The user's real NEF/CR2 samples stay local (gitignored), used to confirm
  RAW parity with photoup.

## Reference documents

- photoup: `CLAUDE.md` and `README.md` (authority docs for flow, adjustments,
  pipeline).
- photoup implementation plan: `PHOTOUP_IMPLEMENTATION_PLAN.md` (historical).
- mpd-client: `CLAUDE.md` and `_bmad-output/planning-artifacts/architecture.md`
  (chassis: thread model, config, testing, doc discipline).
