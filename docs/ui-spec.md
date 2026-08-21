# photoup2 UI — photoup parity spec

The app must replicate photoup's UI flow and layout as closely as GTK4 allows.
This is the authority document for the UI; extracted from photoup's actual
Svelte components (`../photoup/src/App.svelte`,
`components/gallery/PhotoThumb.svelte`, `components/editor/EditorPanel.svelte`,
`components/ui/UsageIndicator.svelte`, `components/gallery/GroupSelector.svelte`,
`components/upload/UploadZone.svelte`, `components/auth/AuthScreen.svelte`).

## Screens

Three screens, driven by state: **Login** (not authenticated) → **Main** (photo
grid) ↔ **Editor** (one photo). "Main" and "Editor" are the two post-login
screens.

## Login screen

- Centered column, title "photoup".
- Tabs **Phone | QR** (photoup has both; our grammers 0.10 drops QR — phone only).
- Phone flow: phone input → **Send code** → code input → **Sign in** → if 2FA,
  password input → **Confirm**.
- Captions: placeholders "+1234567890", "Code", "2FA password".

## Main screen

Layout top to bottom:

1. **Header**: title, then actions: group selector, **Reset**, **Logout**.
   - Group selector: a label "Send to" + a select of the account's groups
     (photoup fetches up to 1000 dialogs).
   - **Reset** → clears all loaded photos (photoup's `clearPhotos`).
   - **Logout** → Telegram logout + clear photos.
2. **Upload zone**: a dashed box, "**Upload photos**" / "Drop JPEG / PNG / NEF /
   CR2 here, or press Ctrl+V to paste". Click opens a file picker; drag-drop and
   paste accepted.
3. **Usage indicator** (photoup `UsageIndicator`): only while photos are being
   processed — a pulsing dot + "**Processing {name}**" and, if more than one, "
   ({n} queued)". NOT a ready/sent counter, NOT a RAM gauge.
4. **Grid**: square thumbnails, `repeat(auto-fill, minmax(180px, 1fr))`, ~14px
   gap. Each cell (photoup `PhotoThumb`):
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
   - Row: **Auto** (active when mode=auto) | **Slide** (aggressive) | **Rest**
     (photoup's label — reset exposure) | the current EV value (e.g. `+2.7`).
4. Section "**White balance**":
   - Temperature (warmth) slider, **−4..+4**, step 0.05, zero-centered (wider than
     photoup's ±2 — some images need a stronger cool shift).
   - Hue (tint) slider, **−1..+1**, step 0.01, zero-centered (fine-grained).
   - Row: **Auto** (auto-WB) | **Pick** (neutral picker mode) | **Reset** | the
     current WB display.
5. Section "**Crop**":
   - Presets: **1:1** | **2:3** | **3:2** | **Original**.
6. Section "**Image**":
   - EXIF lines: camera, lens, `shutter · aperture · ISO`, date.
   - `RAW · 7360 × 4912` (source type + full dimensions).
   - `output 2560 × 1709 px`.
7. Hint line: "Drag handles to resize · drag inside to move · Shift keeps ratio".
8. Nav: **‹ Prev** | **Next ›** (disabled at ends).
9. Bottom: **Reject** (remove this photo) | **Close** (back to grid).

## Notes for the GTK port

- Keep the layout/captions/sections EXACTLY as above; do not rename buttons or
  sections. ("Exposure"/"White balance"/"Crop"/"Image"; Auto/Slide/Rest;
  Auto/Pick/Reset; 1:1/2:3/3:2/Original; ‹ Prev/Next ›; Reject/Close;
  Reset/Logout; Send {n} selected; Preparing/Sending.)
- photoup is a light-on-dark theme with an orange accent and a display font;
  matching the exact theme is optional — matching the layout, labels, and
  interaction is required.
- The neutral-picker (Pick) and crop drag are interactive features that exist in
  photoup; the desktop port should implement them (or mark them clearly as
  deferred, but the buttons/sections must be present).
