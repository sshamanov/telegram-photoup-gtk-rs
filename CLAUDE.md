# photoup2 — Claude Code project rules

## What this project is

A native GTK4/libadwaita desktop app that fixes dark or monochrome-cast photos
(NEF/CR2/DNG/JPEG/PNG) and uploads them to a Telegram group as **inline
≤2560px, 4:4:4 mozjpeg JPEGs at Q100**, quality lowered adaptively only to stay
under Telegram's ~10 MB photo limit (`MAX_PHOTO_BYTES = 10_000_000`).

It is a native Rust + GTK app with real parallelism: photos are decoded,
auto-exposed, white-balanced and tone-mapped across a worker pool, and the
edited results upload straight to a Telegram group.

Original inception design: `docs/superpowers/specs/2026-08-19-photoup2-design.md`
Implementation plan: `docs/superpowers/plans/2026-08-19-photoup2.md`

## System boundaries

- **THIS project CAN install system packages** — it is a native app compiling
  against system GTK4/libadwaita and a C/C++ toolchain.
- Two build targets, one `Cargo.toml`: **Ubuntu 24.04** and **Arch**. Both must
  keep building from the same manifest: `libadwaita` is pinned to feature
  `v1_5` (Ubuntu 24.04 ships 1.5.0; Arch's newer libadwaita is backward
  compatible). **Do NOT bump `adw` to `v1_7`+** — it would not build on Ubuntu.
- The **file dialog filter** is `app::photo_filter()`, built from
  `photo_filter_rules()` — keep it that way, because the dialog is not GTK's:
  on Wayland/GNOME `GtkFileDialog` goes through **xdg-desktop-portal**, which is
  handed the filter as *serialized globs*. Two traps, both of which show up as
  "Photos" in the type dropdown and then an empty list: `add_suffix` takes a
  **bare** suffix (GTK prepends `*.` itself, so `".jpg"` asks for `*..jpg`), and
  a suffix rule arrives at the portal as a bracket-class glob
  (`*.[jJ][pP][gG]`) that the backend's matcher cannot read. So the filter
  carries bare suffixes *plus* literal `*.ext` / `*.EXT` globs (patterns are
  case-sensitive; camera files are uppercase) and `image/jpeg`/`image/png` mime
  types. `photo_filter_has_glob_rules_for_every_extension` asserts the rules
  headless; the serialized form is asserted from the one GTK-initialising test
  (`assert_photo_filter_serializes_matchable_globs`).
- `libraw-rs-sys` statically vendors LibRaw (compiled from source by `cc`), so
  RAW support needs **no** `libraw-dev` package and no runtime RAW dependency —
  but it does need a working C++ compiler.
- The **dev instance itself is Arch + Wayland (Hyprland), headless**, not
  Ubuntu — the Ubuntu target is the user's own machine. Do not assume the dev
  host's package names or compositor in build instructions.

## Build / run / test

- `cargo build`, `cargo test`, `cargo run` (debug). Release is `lto = true`.
- **Telegram `api_id`/`api_hash` are baked in at build time** by `build.rs` from
  `TG_API_ID` / `TG_API_HASH` (build env, else the gitignored `.env`; template
  `.env.example`) and read via `config::telegram_credentials()`. They are not in
  `config.toml`. **Never commit `.env` or any key** — the repo is public.
- System deps: GTK4 + libadwaita (dev headers) and gcc/clang. Arch:
  `gtk4 libadwaita base-devel`. Ubuntu:
  `libgtk-4-dev libadwaita-1-dev build-essential`. No `libraw-dev`.
- RAW support is driven by one list: `image::decode::RAW_EXTS` (`nef`, `cr2`,
  `dng`) with `is_raw_ext()`. `app::IMAGE_EXTS` (what the picker/drop/paste
  accept) must contain every `RAW_EXTS` entry — `raw_exts_are_accepted_photos`
  enforces it, so a new RAW format is added in both places or not at all.
- RAW/PNG/JPEG tests in `src/image/decode.rs` decode a real sample **only if**
  one exists in `./samples/` or `../photoup/samples/` (they skip otherwise).
  Samples are gitignored and never committed.

## Debug & timing

- `RUST_LOG=info cargo run` prints `[timing]` lines from the pipeline jobs in
  `src/app.rs` with their real phases: thumb `read / decode / render / total`,
  preview `read+decode / render / total` (a slider edit re-renders from the
  cached base and logs `preview (cached base) render / total` instead), export
  `read / decode / render / encode / total -> bytes`, and a send-batch
  `upload_total`. Use these; do not guess at performance.
- `PHOTOUP2_DEV=1 cargo run` skips Telegram auth and auto-loads `./samples/*`,
  so the grid/editor can be exercised headless without logging in.
- **Always measure with `--release`.** The debug build runs the pure-Rust JPEG
  decoder ~10× slower (a 36MP JPG thumbnail ~7 s debug vs <1 s release); libraw
  is compiled C++ and fast in both. Timings from a debug build are misleading.
- UI flow: login → grid with default-checked thumbnails → editor → send-as-album
  — see the README "UI flow" section. Known gaps: no QR login (grammers 0.10
  dropped it); the grid's checkbox checkmark/selected-cell accent and the
  editor's dimmed backdrop are deferred (marked in `docs/ui-spec.md`).

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

- **`TelegramAdapter`** (`src/telegram/mod.rs`): `handle(cmd: TCommand) -> Vec<TEvent>`.
  Only **`MockAdapter`** (`src/telegram/mock.rs`) implements it — it is the
  deterministic contract the integration tests in `tests/telegram_mock.rs`
  speak. The **real** path is not an adapter impl: `worker.rs` drives
  `GrammersSession` (`src/telegram/grammers.rs`) through an inline
  `match cmd` on the IO thread. Wire new Telegram commands into both, or the
  tests will exercise a path the app does not run.
- **Image pipeline** is photoup2's own linear-light implementation: per-pixel
  exposure + WB gains applied in linear space, a precomputed tone LUT (linear →
  sRGB with highlight rolloff; RAW also gets a camera-Standard S-curve),
  luma-preserving saturation, a black-point levels LUT applied last, and 4:4:4
  mozjpeg Q100 encode. `AppController` (`src/app.rs`) owns the wiring: screens,
  pool, and telegram worker.

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
- **Render order** is fixed: exposure + WB in linear → tone LUT → **saturation**
  (display space, luma-preserving, Rec. 709 weights: `−1` gray … `0` identity …
  `+1` 2× chroma) → **black point** (levels, **applied last**:
  `out = clamp((x − b)/(1 − b))`) → clamp. The levels curve is what an exposure
  gain cannot do: `b > 0` puts a lifted floor on black while keeping white at
  white, `b < 0` lifts the floor to `|b|/(1+|b|)` (a matte look).
- **The black point is MANUAL ONLY** (`adjustments.black_point`, applied in
  `src/image/process.rs`). Exposure **Auto** and **Burn** set the EV and nothing
  else, exactly as they did before this control existed — no auto derivation, no
  `black_point_auto` field, and the `action: edit` log line has no `(auto)`
  marker. This is deliberate: a levels stretch that keeps white at white
  (`out = (x − b)/(1 − b)`, fixed point at white) necessarily darkens what lies
  between, so a derived value would quietly undo the auto exposure's
  median-on-128 promise. Pushing the stretch is the slider's job. (`crop_aware_autos`
  in `src/app.rs` now feeds the EV override only; `RenderResult` carries
  `rgba` + `auto_ev`.)
- Export format is **final, NOT tunable**: 4:4:4 mozjpeg Q100, adaptive quality
  down to `MAX_PHOTO_BYTES`, longest edge ≤`EXPORT_EDGE` (2560). Grid thumbnails
  are ≤512px. The editor preview is **two-stage**: live slider/drag edits render
  at `LIVE_EDGE` (512, snappy), then the same edit is re-rendered at `FINAL_EDGE`
  (1024) from the cached base once input goes quiet for `PREVIEW_DEBOUNCE_MS`
  (150ms). There is no `PREVIEW_EDGE`.
- RAW decodes with these LibRaw params (`src/image/decode.rs`):
  `use_camera_wb` (or `user_mul`), `use_camera_matrix=1`, `output_color=1`
  (sRGB primaries + gamma), `output_bps=16`, `no_auto_bright=1`, `half_size`
  (interactive only; exports pass `full_size`), `user_qual=3`.
- **Only the export bakes WB into libraw — the previews never do.** `run_export_job`
  (`src/app.rs`) passes `user_mul = export_wb_mul(cam_mul, adjustments)` with
  `use_camera_wb=0` and neutralizes `wb_offset`/`hue` for the render; thumbnails
  and previews pass `user_mul: None` and decode with the camera WB. A defect in
  `export_wb_mul` (`src/image/srgb.rs`) is therefore invisible in the grid and
  the editor and shows up **only** in the file that reaches Telegram.
  It must never return 0 for a channel: libraw leaves `cam_mul[3]` at 0 on a
  three-colour sensor (the Canon PowerShot DNGs in `./samples/`), and passing
  that through zeroes half the greens, which the camera matrix then drags R and
  B down with — a black export with `auto_ev` pinned at +6.00. A missing channel
  falls back to the green reference. `app::tests::raw_export_with_edits_is_not_black`
  covers the whole path on a real DNG (skipped when no sample is present).
- **Capture metadata (EXIF) for the editor's Image section** comes from two
  readers with one shared type (`PhotoMeta`, `src/image/types.rs`): RAW through
  LibRaw (`Raw::meta()`, `src/image/rawffi.rs`) and JPEG/PNG through
  `kamadak-exif` (`exif_meta()`, `src/image/decode.rs`). It travels with the
  decode (`UiEvent::ThumbReady.meta`, `DecodedRaw.meta`), not with renders —
  `AppEvent::PhotoThumbReady { meta: Option<PhotoMeta> }` is `None` on
  render-only events so a slider edit cannot wipe the lines the decode filled.
  Both readers must agree on the same display rules (`PhotoMeta::from_parts`:
  drop empty/zero values, never repeat the maker). LibRaw's `other.timestamp`
  is a **local-time** epoch, so it is rendered with `localtime_r`
  (`format_epoch_local`) — rendering it as UTC shifts every displayed time by
  the host's offset; the EXIF crate path reformats the stored string directly.

## Dev instance & delivery

The dev instance is **headless** — no screen the user is looking at. The user's
own machine syncs this repo via git (`pull` from `origin/main`) and runs the app
there. So the delivery loop is **commit → push → user pulls → user runs**; only
commit once the visual check below passes.

## Verification rule

**Claude verifies visually — Claude *is* the VLM.** When a change could visibly
differ, render it and *look at the image yourself* before calling the change
done; never assert visual correctness from the code alone. There is no external
vision service; read the PNG with the `Read` tool.

- **Never screenshot the user's desktop.** This dev instance runs the user's real
  Wayland session (Hyprland), and a full-screen grab exposes their terminal and
  private files. Always capture the app window alone, and delete captures that
  leak anything else.
- App screenshots: the working recipe on this host is a **dedicated Xvfb display**
  — `Xvfb :99 -screen 0 1600x1200x24`, then
  `DISPLAY=:99 GDK_BACKEND=x11 PHOTOUP2_DEV=1 ./target/release/photoup2`, then
  `DISPLAY=:99 import -window <id> out/x.png` (`xwininfo -root -tree` for the id).
  On the Wayland session itself `import -window root` is black/rootless and
  `grim -g` grabs whatever is stacked there — do not use them to inspect the app.
- Driving the UI without a physical pointer: `python-xlib` is available, so
  XTEST (`Xlib.ext.xtest.fake_input`) can click/type into the window on `:99`
  (e.g. double-click a thumbnail to open the editor, drag inside the crop rect).
  A cursor's own pixels never appear in `import -window` captures — assert
  cursor behaviour in a test instead, and read the theme's cursor file
  (`/usr/share/icons/<theme>/cursors/<name>`) when the shape itself matters.
- Renders of a sample photo with different WB settings:
  `cargo test --lib wb_debug_render_before_after -- --nocapture` →
  `out/wb_before_after/`.
- Black point / saturation across every sample (prints `ev / bp / p0.1 / p1 /
  p50 / p99` per photo and writes the `-bp0` / `-bp20` / `-bpm20` / `-sat-1` /
  `-sat+1` variants):
  `PHOTOUP2_DEBUG_LEVELS=1 cargo test --release --lib levels_debug_render -- --nocapture`
  → `out/levels/`.

The user's real NEF/CR2/DNG/JPEG samples stay local in `./samples/` (gitignored)
and are the source of truth for RAW output quality.

## Docs and code stay in sync

`docs/ui-spec.md` is the authority for UI behaviour; `CLAUDE.md`/`README.md` are
the authority for architecture and workflow. They are living documents, not
history:

- **Docs follow code.** When a change makes any statement in these files false —
  a constant, a phase name, a deferred feature that now works, a dependency, a
  path — fix the doc in the same delivery. Stale "known gap" claims are the worst
  kind: they cost a future reader real time.
- **Code follows docs.** When the spec says the UI does something the code does
  not, that is a defect in one of them: either implement it, or strike/defer it
  in the spec explicitly (marked **deferred**, with the reason). Never leave the
  spec describing behaviour that does not exist.
- Before claiming a doc statement is wrong, **check the code** (or the running
  app) — do not repeat a stale summary. And when a doc and the code disagree
  about a *number*, the constant in the code wins.

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
