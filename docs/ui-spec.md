# photoup2 UI — specification

This is the authority document for the UI: the screens, their layout, and the
exact button/section labels. Keep the layout, captions, and interaction as
described here.

## Screens

Three screens, driven by state: **Login** (not authenticated) → **Main** (photo
grid) ↔ **Editor** (one photo). "Main" and "Editor" are the two post-login
screens.

## Login screen

- Centered column, title "photoup2".
- Phone login only (grammers 0.10 dropped the QR flow).
- Flow: phone input → **Send code** → code input → **Sign in** → if 2FA,
  password input → **Confirm**.
- Captions: placeholders "+1234567890", "Code", "2FA password".

## Main screen

Layout top to bottom:

1. **Header**: title, then actions: group selector, **Reset**, **Logout**.
   - Group selector: a label "Send to" + a select of the account's groups
     (fetches up to 1000 dialogs).
   - **Reset** → clears all loaded photos.
   - **Logout** → Telegram logout + clear photos.
2. **Upload zone**: a dashed box, "**Upload photos**" / "Drop JPEG / PNG / NEF /
   CR2 here, or press Ctrl+V to paste". Click opens a file picker; drag-drop and
   paste accepted.
3. **Usage indicator**: only while photos are being processed — a pulsing dot +
   "**Processing {name}**" and, if more than one, "({n} queued)". NOT a
   ready/sent counter, NOT a RAM gauge.
4. **Grid**: square thumbnails, `repeat(auto-fill, minmax(180px, 1fr))`, ~14px
   gap. Each cell:
   - Square image, `object-fit: cover` (crop to fill). While decoding shows a
     centered "developing…" placeholder; on error, "error" in red.
   - **Checkbox, absolute top-left** (custom dark pill, checkmark when checked).
   - **EV badge, absolute top-right**: the exposure correction, mono font,
     `+2.7` / `-0.5`, shown only when ready and `|EV| > 0.05`.
   - Bottom meta row: filename (truncated) + a **RAW / JPG** badge.
   - Clicking a cell opens the Editor. The checkbox stops propagation.
   - Selected cell = accent border/glow.
5. **Footer (sticky bottom)**: a single wide **Send** button:
   - Idle: "**Send {n} selected**" (disabled when 0 selected / not ready).
   - Preparing: "**Preparing {i}/{n}**" with the current file name.
   - Sending: a progress bar + "**Sending {p}%**".
   - **After a successful send, the sent photos are removed from the grid**;
     unsent/failed photos stay with their edits. Albums are chunked to ≤10
     photos (Telegram's `MULTI_MEDIA_TOO_LONG` limit).

## Editor screen

Full-window overlay (dimmed backdrop). Left = preview, right = 332px panel.

**Left (preview):** the photo, `object-fit: contain`, with an interactive crop
overlay **visible from the start** — with no crop set it spans the whole image
(so you can drag/resize without clicking a preset first). Drag handles
(nw/n/ne/e/se/s/sw/w) to resize, drag inside to move, Shift keeps ratio. A
"Pick" neutral-WB mode turns the preview into a crosshair for selecting the gray
point.

**Right panel, top to bottom:**

1. Photo filename (mono, truncated).
2. **Histogram** (256-bin luminance, dark background, light bars).
3. Section "**Exposure**":
   - EV slider, min −3, max +5, step 0.1, zero-centered (drag updates live,
     release commits).
   - Row: **Auto** (auto-exposure) | **Slide** (aggressive) | **Rest** (reset
     exposure to 0) | the current EV value (e.g. `+2.7`).
4. Section "**White balance**":
   - Temperature (warmth) slider, **−4..+4**, step 0.05, zero-centered (the wider
     range covers images that need a strong cool shift).
   - Hue (tint) slider, **−1..+1**, step 0.01, zero-centered (fine-grained).
   - Row: **Auto** (clinical neutralization) | **Auto2** (neutralize but keep the
     warm ambience, Nikon AUTO2 style) | **Reset** | the current WB display.
     The neutral **Pick** needs no button — clicking the preview samples the
     gray point directly.
5. Section "**Crop**":
   - Presets: **1:1** | **2:3** | **3:2** | **Original**.
6. Section "**Rotate**":
   - **↺ CCW** | **↻ CW** (90° steps; applied on top of the EXIF orientation).
7. Section "**Image**":
   - EXIF lines: camera, lens, `shutter · aperture · ISO`, date.
   - `RAW · 7360 × 4912` (source type + full dimensions).
   - `output 2560 × 1709 px`.
7. Hint line: "Drag handles to resize · drag inside to move · Shift keeps ratio".
8. Nav: **‹ Prev** | **Next ›** (disabled at ends).
9. Bottom: **Reject** (remove this photo) | **Close** (back to grid).

## Notes for the GTK implementation

- Keep the layout/captions/sections EXACTLY as above; do not rename buttons or
  sections. ("Exposure"/"White balance"/"Crop"/"Rotate"/"Image";
  Auto/Slide/Rest; Auto/Auto2/Reset; 1:1/2:3/3:2/Original; ↺ CCW/↻ CW;
  ‹ Prev/Next ›; Reject/Close; Reset/Logout; Send {n} selected;
  Preparing/Sending.)
- The theme is a light-on-dark look with an amber accent and a display font;
  matching the exact theme is optional — matching the layout, labels, and
  interaction is required.
- The neutral-pick (click the preview) and crop presets are implemented; the
  interactive crop drag is deferred — the crop section and overlay must stay
  present.
