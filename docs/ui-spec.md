# photoup2 UI — specification

This is the authority document for the UI: the screens, their layout, and the
exact button/section labels. Keep the layout, captions, and interaction as
described here.

Everything below is checked against the implementation
(`src/ui/*.rs`, `src/app.rs`) — see the "Docs and code stay in sync" rule in
`CLAUDE.md`. When this file and the code disagree, one of them is a bug.

## Screens

Three screens, driven by state: **Login** (not authenticated) → **Main** (photo
grid) ↔ **Editor** (one photo). "Main" and "Editor" are the two post-login
screens; the editor is a screen in the root stack, not a separate window.

## Login screen

- Centered card (340px) on a dark background: title "**photoup2**", muted
  subtitle "**Telegram login**", one step description, the visible input(s), and
  one action button.
- Phone login only (grammers 0.10 dropped the QR flow).
- A single primary button whose label follows the auth step:
  - phone step → "**Request code**" (description "Enter your phone number to
    start", placeholder "+1 555 0132");
  - code step → "**Submit code**" (description "Enter the login code sent to
    your phone", placeholder "Login code");
  - 2FA step → "**Submit 2FA password**" (description "This account requires a
    2FA password", placeholder "2FA password", entry masked).
  - failure → back to the phone step with the primary reading "Request code" and
    the description "Login failed: {message}".
- The inputs for the other steps are hidden, not disabled.

## Main screen

Layout top to bottom:

1. **Header**: wordmark "photoup", then actions: "**Send to**" + group selector,
   **Reset**, **Logout**.
   - Group selector: the account's dialogs, fetched up to **200** (grammers
     `iter_dialogs` is lazy and unbounded, so the worker stops at 200).
   - **Reset** → clears all loaded photos (disabled while none are loaded).
   - **Logout** → Telegram logout + clear photos.
2. **Upload zone**: a dashed box, "**Upload photos**" / "Drop JPEG / PNG / NEF /
   CR2 / DNG here, or press Ctrl+V to paste". Click opens a file picker;
   drag-drop and paste accepted.
3. **Usage indicator**: only while photos are being processed — an amber pulsing
   dot + "**Processing {name}**" and, if more than one is pending, "({n} queued)"
   counting the ones behind the current file. NOT a ready/sent counter, NOT a RAM
   gauge.
4. **Grid**: a `GtkGridView` fed by a `GListStore`, cells sized **170 × 170**,
   square thumbs, bound through a `SingleSelection` (the selection drives
   click-to-edit). Each cell:
   - Square image, cropped to fill. While decoding shows a centered
     "developing…" placeholder; on error, "error" in red.
   - **Checkbox, absolute top-left**: a custom dark pill that toggles selection
     without opening the editor.
   - **EV badge, absolute top-right**: the exposure correction, mono font,
     `+2.7` / `-0.5`, shown only when ready and `|EV| > 0.05`.
   - Bottom meta row: filename (truncated) + a **RAW / JPG** badge.
   - Clicking a cell opens the Editor.
5. **Footer (sticky bottom)**: a single wide **Send** button:
   - Idle: "**Send {n} selected**" (disabled when 0 selected / not ready, and
     while a send backoff is pending).
   - Preparing: "**Preparing {i}/{n} · {name}**" with the current file name.
   - Sending: a progress bar + "**Sending {p}%**".
   - **After a successful send, the sent photos are removed from the grid**;
     unsent/failed photos stay with their edits. Albums are chunked to ≤10
     photos (Telegram's `MULTI_MEDIA_TOO_LONG` limit).

## Editor screen

Left = preview, right = 332px panel.

**Left (preview):** the photo, contained, with an interactive crop overlay
**visible from the start** — with no crop set it spans the whole image (so you
can drag/resize without clicking a preset first). Drag handles
(nw/n/ne/e/se/s/sw/w) to resize, drag inside to move, **Shift** keeps the ratio.
Only while the WB **Picker** is armed does clicking the preview sample a neutral
gray point (a 7×7–31×31 area) instead of starting a crop drag; arming the Picker
turns the preview cursor into a **crosshair**.

**Right panel, top to bottom:**

1. Photo filename (mono, truncated, full name as a tooltip).
2. **Histogram**: the RGB histogram (3 × 256 = 768 bins, R/G/B overlaid as
   translucent channels) drawn on a dark frame, 120px tall. The 256-bin
   luminance histogram is still computed by the pipeline but is not displayed.
3. Section "**Exposure**":
   - EV slider, min −3, max +5, step **0.05**, zero-centered (drag updates live,
     release commits).
   - **Black point** slider, **−0.5..+0.5**, step **0.01**, zero-centered,
     in **display (0.0..1.0) units**. The caption doubles as the value readout
     (`Black point +0.16`). Applied **last** in the pipeline — see "Black point
     & saturation" below. Positive values crush the floor to black and keep
     white at white (darkening everything between); **negative values are
     allowed on the slider only** — they lift the floor to `|b|/(1+|b|)`, a
     matte look no exposure gain can produce.
   - Row: **Auto** (auto-exposure) | **Burn** (aggressive) | **Rest** (reset
     exposure to 0) | the current EV value (e.g. `+2.7 EV`).
4. Section "**White balance**":
   - Temperature (warmth) slider, **−4..+4**, step 0.05, zero-centered (the wider
     range covers images that need a strong cool shift).
   - Hue (tint) slider, **−1..+1**, step 0.01, zero-centered (fine-grained).
   - **Saturation** slider, **−1..+1**, step **0.01**, zero-centered, caption
     as the value readout (`Saturation +0.00`). `−1` is fully desaturated, `0`
     leaves the photo unchanged, `+1` is 2× chroma. Luma-preserving
     (Rec. 709 weights), so it changes colour without changing brightness.
   - Row: **Auto** (neutralize the warm/cool cast — clinical) | **Auto2**
     (neutralize but keep the warm ambience, Nikon AUTO2 style) | **Reset** |
     **Picker** | the current WB display (`+0.00 · +0.00`). **Picker** is a
     visible toggle (amber while active). Both Auto buttons carry their
     explanation as a tooltip.
5. Section "**Crop**":
   - Presets: **1:1** | **2:3** | **3:2** | **Original** | **Pix**. **Pix** uses
     the current crop's center and nearest 1:1, 3:2, 16:9, or 2:1 aspect while
     preserving orientation, then creates the exact full-resolution source crop
     `2560 × 2560`, `2560 × 1707`, `2560 × 1440`, or `2560 × 1280` (or a portrait
     counterpart). At an edge it translates that rectangle inward; it never
     shrinks it. (Tooltip: "Snap crop to the nearest pixel-friendly export
     aspect".)
6. Section "**Rotate**":
   - **↺ CCW** | **↻ CW** (90° steps; applied on top of the EXIF orientation;
     tooltips "Rotate 90° counter-clockwise" / "Rotate 90° clockwise").
   - Preview and crop-overlay dimensions must stay in the same orientation
     through resizing, Pix, and delayed preview upgrades. Derive them from the
     unrotated decoded base on every render (×2 for half-resolution RAW), then
     apply the current rotation once. Never rotate a previous render's already
     rotated dimensions again.
7. Section "**Image**":
   - EXIF lines: camera, lens, `shutter · aperture · ISO`, date. Each line is
     hidden when unknown, so a photo without maker notes shows fewer lines
     rather than placeholders.
   - `RAW · 7360 × 4912` (source type + full decode dimensions).
   - `output 2560 × 1709 px` (what the export will be).
8. Hint line: "Drag handles to resize · drag inside to move · Shift keeps ratio".
9. Fine-tune hint: "Fine-tune: Q/W exposure · A/S warmth · Z/X tint".
10. Nav: **‹ Prev** | **Next ›** (disabled at ends).
11. Bottom: **Reject** (remove this photo) | **Close** (back to grid).

### Black point & saturation (weird-histogram photos)

RAW histograms that start well above zero (haze, veiling flare, a flat
moth-on-a-wall frame) cannot be fixed with exposure alone: an exposure gain is
multiplicative, so it can never put the floor *on* black. These two controls
are the additive/curve answer, and both act **after** exposure, white balance
and the tone curve:

1. **Saturation** — on the tone-mapped pixel, luma-preserving.
2. **Black point** — levels, applied **last** (`out = clamp((x − b)/(1 − b))`),
   so its white point and the exposure cap do not fight.

**Auto** (armed by **Auto** and **Burn**, and re-armed by pressing either) owns
the black point only while it is auto: it is derived **live on every render**
from the cropped source, and the slider shows the derived value exactly like
the EV badge shows the effective EV. The rules:

- **Stretch only, never shrink**: the derived value is clamped to
  `0.0 ..= 0.5`. Auto may pull a lifted floor down to black; it never lifts the
  floor, and it never darkens an already-good photo. Pushing a floor *up* is
  the slider's job (negative values), i.e. manual-only.
- Derivation: a ~16k-sample strided **raw-pixel** luminance sample of the crop
  (not a box average — averaging destroys the tail this percentile needs),
  tone-mapped through the same LUT the render uses, then its **p0.1**
  percentile is the floor.
- **Flatness gate**: if `3 × floor > median` the frame has no dark tail worth
  stretching (a wall, a sky, a product shot) and the derived point is `0`.
- The sample comes from the **source plus the crop**, never from the rendered
  pixels, so the 512px live preview and the 2560px export derive the same
  value — the preview cannot lie about the export.
- Dragging the black-point slider is a manual edit: it disarms auto and pins the
  slider's value (the log line drops its `(auto)` marker). Auto re-arms.

## Keyboard

| Key | Action |
|---|---|
| Ctrl+V | Paste photos (system clipboard), on the main screen |
| Ctrl+A | Toggle select-all on the grid |
| Return / KP_Enter | Open the editor for the selected cell |
| → / ← | Next / previous photo in the editor |
| Esc | Close the editor |
| Q / W | Exposure −/+ 0.05 EV |
| A / S | Warmth −/+ 0.05 |
| Z / X | Tint −/+ 0.01 |

## Toasts

Transient messages on the toast overlay wrapping the root stack: "Set api_id /
api_hash in {config path}" on a launch with no credentials, "Log in to Telegram
first", "No ready photos selected", "Pick a target group", "No exportable photos
selected", "Sent {n} photos", "Send failed: {error}", and "Send failed — {n}
JPEGs kept in {dir}" when a failed send leaves the exported JPEGs on disk so
nothing is lost.

## Deferred (documented, not implemented)

These are known gaps, deliberately deferred — do not treat them as bugs:

- The grid's checkbox renders no checkmark glyph when checked, and the selected
  cell has no accent border/glow. Selection state is real (and drives Send), only
  the two visual affordances are missing.
- The editor is not a dimmed modal backdrop — it is a screen in the root stack
  that covers the window.

## Notes for the GTK implementation

- Keep the layout/captions/sections EXACTLY as above; do not rename buttons or
  sections. ("Exposure"/"White balance"/"Crop"/"Rotate"/"Image";
  Auto/Burn/Rest; Auto/Auto2/Reset/Picker; 1:1/2:3/3:2/Original/Pix; ↺ CCW/↻ CW;
  Black point; Saturation;
  ‹ Prev/Next ›; Reject/Close; Reset/Logout; Send {n} selected;
  Preparing/Sending.)
- The theme is a light-on-dark look with an amber accent and a display font;
  matching the exact theme is optional — matching the layout, labels, and
  interaction is required.
- The login card is wordmarked "photoup2" and the main-screen header "photoup"
  (the upstream naming); the window title is "photoup2".
