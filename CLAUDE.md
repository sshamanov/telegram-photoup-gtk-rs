# photoup2 — Claude Code project rules

## What this project is

A native GTK4/libadwaita desktop app that fixes dark or monochrome-cast photos
(NEF/CR2/JPEG/PNG) and uploads them to a Telegram group as **inline ≤2560px,
4:4:4 mozjpeg JPEGs at Q100**, quality lowered adaptively only to stay under
Telegram's ~10 MB photo limit (`MAX_PHOTO_BYTES = 10_000_000`).

It is a native Rust + GTK app with real parallelism: photos are decoded,
auto-exposed, white-balanced and tone-mapped across a worker pool, and the
edited results upload straight to a Telegram group.

Original inception design: `docs/superpowers/specs/2026-08-19-photoup2-design.md`
Implementation plan: `docs/superpowers/plans/2026-08-19-photoup2.md`

## System boundaries

- **THIS project CAN install system packages** — it is a native app compiling
  against system GTK4/libadwaita/libraw.
- Dev is Ubuntu 24.04, prod is Arch. The **same `Cargo.toml` builds on both**:
  `libadwaita` is pinned to feature `v1_5` (Ubuntu ships 1.5.0; Arch's newer
  libadwaita is backward-compatible). **Do NOT bump `adw` to `v1_7`+** — it
  would not build on the dev host.
- `libraw-rs-sys` statically vendors LibRaw, so RAW support needs no separate
  runtime dep from the OS package manager.

## Build / run / test

- `cargo build`, `cargo test`, `cargo run` (debug). Release is `lto = true`.
- System deps required: GTK4, libadwaita, and LibRaw (via `libraw-rs-sys`).
- The RAW test in `src/image/decode.rs` decodes a real NEF/CR2 **only if** a
  sample file exists in `./samples/` or `../photoup/samples/` (it skips
  otherwise). Samples are gitignored and never committed.

## Debug & timing

- `RUST_LOG=info cargo run` prints `[timing]` lines from the pipeline jobs in
  `src/app.rs` (`run_thumb_job` / `run_preview_job` / `run_export_job`) with
  read/decode/render/encode phase durations, plus a send-upload total on
  `TEvent::Sent`. Use this to find bottlenecks; do not guess at performance.
- `PHOTOUP2_DEV=1 cargo run` skips Telegram auth and auto-loads `./samples/*`,
  so the grid/editor can be exercised headless without logging in.
- **Always measure with `--release`.** The debug build runs the pure-Rust JPEG
  decoder ~10× slower (a 36MP JPG thumbnail ~7 s debug vs <1 s release); libraw
  is compiled C++ and fast in both. Timings from a debug build are misleading.
- UI flow: login → grid with default-checked thumbnails → editor → send-as-album
  — see the README "UI flow" section. Known gaps: no QR login (grammers 0.10
  dropped it); the interactive crop drag is deferred (the crop presets and
  overlay work).

## Architecture

Three Kingdoms threading (borrowed from mpd-client), all in one Rust crate
(edition 2024). No global async runtime.

- **GTK main thread** — display only. Owns `AppState`; all mutations go through
  `reduce(state, event)` (`src/state.rs`). A single **50ms `poll()`** on the
  main thread (`src/app.rs`) drains channels from the other two kingdoms and
  reflects state into the screens. No blocking I/O, no decode/encode, no
  Telegram I/O here.
- **Telegram IO thread** — one persistent grammers MTProto connection running on
  a **tokio current-thread runtime** (`src/telegram/worker.rs`). Handles auth
  (phone + code + 2FA), dialog fetch, and photo/album sends. Commands in via
  channel, events out via channel. Never blocked by image work.
- **Image worker pool** — `nproc − 2` workers (`src/image/pool.rs`). Each worker
  runs a complete photo job end-to-end (decode → process → encode) so
  single-threaded-by-design libraries (libraw, mozjpeg) scale by running
  independent jobs across workers — never by threading inside the libraries.

### Adapter patterns

- **`TelegramAdapter`** (`src/telegram/mod.rs`): `handle(cmd: TCommand) -> Vec<TEvent>`
  is the contract the UI and tests both speak. Real impl is `GrammersSession`
  (`src/telegram/grammers.rs`); **`MockAdapter`** (`src/telegram/mock.rs`) is the
  deterministic mock used by integration tests in `tests/telegram_mock.rs`.
- **Image pipeline** is photoup2's own linear-light implementation: per-pixel
  exposure + WB gains applied in linear space, a precomputed tone LUT (linear →
  sRGB with highlight rolloff; RAW also gets a camera-Standard S-curve), and
  4:4:4 mozjpeg Q100 encode. `AppController` (`src/app.rs`) owns the wiring:
  screens, pool, and telegram worker.

## Image pipeline

- **Decode once → cached base → render.** Decoded RAW/JPEG becomes a renderable
  `Base` (`JpegBase` / `RawBase`, `src/image/process.rs`); every edit re-renders
  from that original base — never cumulative, never re-decode.
- **Auto-exposure** (`auto_exposure_ev`, `src/image/math.rs`): solved in LINEAR
  space (the gain is applied as `linear × 2^EV`) so the median pixel (p50) lands
  exactly on its target in the tone-mapped output. Both modes anchor midtones on
  128 and never darken (EV floored at 0 — bright photos are left alone). The
  modes differ only in highlight handling: **Auto** caps the p99-brightest pixel
  at 252 (just under white, no mass clip), and the cap is clamped at ≥ 0 so an
  already-blown photo keeps its white point instead of a pointless drag-down;
  **Burn** drops the cap entirely — the median governs the lift up
  to `max_ev` 6.0 and highlights may clip to pure white. The difference between
  the two modes is therefore exactly clipping and white point.
- Export format is **final, NOT tunable**: 4:4:4 mozjpeg Q100, adaptive quality
  down to `MAX_PHOTO_BYTES`, longest edge ≤2560px (`EXPORT_EDGE`). Interactive
  preview renders at `PREVIEW_EDGE = 1024`; grid thumbnails at ≤512px.
- RAW decodes with these LibRaw params (`src/image/decode.rs`):
  `use_camera_wb` (or `user_mul`), `use_camera_matrix=1`, `output_color=1`
  (sRGB primaries + gamma), `output_bps=16`, `no_auto_bright=1`, `half_size`
  (interactive only; exports pass `full_size`), `user_qual=3`.

## Dev instance & delivery

This repo is developed on a **headless dev instance** — an Xvfb display, but no
physical screen/desktop for the user. The user's machine syncs this repo via
git (`pull` from `origin/main`) and runs the app there. So the delivery loop is
**commit → push → user pulls → user runs**; only commit once the visual check
below passes.

## Verification rule

**Claude verifies visually with VLM.** When a change could visibly differ, take
real screenshots or render before/after images and inspect them via the VLM
before calling the change done — do not assert visual correctness without
looking. (The `Read` tool may not display images on some model backends; the VLM
endpoint always works.)

- **VLM** (dev-time only): OpenAI-compatible vision API
  `https://<vlm-endpoint>/api/chat/completions`, `model: "qwen3-vl:30b-instruct"`,
  `Authorization: Bearer sk-REDACTED`. Send the image as
  `{"type":"image_url","image_url":{"url":"data:image/png;base64,<b64>"}}` in a
  message, ask for per-image severity/neutrality ratings.
- App screenshots: `PHOTOUP2_DEV=1 cargo run` on the Xvfb display, capture with
  `import -window root out/x.png`. Renders of a sample photo with different WB
  settings: `cargo test --lib wb_debug_render_before_after -- --nocapture` →
  `out/wb_before_after/`.

The user's real NEF/CR2/JPEG samples stay local in `./samples/` (gitignored)
and are the source of truth for RAW output quality.

## Commit discipline

One commit per logical block, conventional messages (`feat:`, `fix:`, `docs:`,
`refactor:`, `test:`). Docs-and-code-of-the-same-change are separate commits
(see history: `docs:` commits track the plan; `feat:`/`fix:` commits land the
code).

## Accepted limitations (do not "fix" without checking with the user)

- **Progressive, not baseline, JPEG.** mozjpeg's Rust binding has no baseline
  switch and defaults to progressive; scan order is pixel-neutral, so decoded
  pixels are unaffected by the progressive scan order.
- **RAW access-hash caveat for supergroups/channels.** Sending needs the peer's
  access hash; `DialogInfo.access_hash` is `None` for plain basic groups/users
  (fine) but can be missing for some channels/supergroups — sends there fail
  until the hash is available.
- **grammers 0.10 dropped QR login.** Login is phone number + code + 2FA only.
