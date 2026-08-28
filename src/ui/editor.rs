//! Per-photo editor (photoup `EditorPanel`): preview on the left, a 332px panel
//! on the right with filename, histogram, Exposure / White balance / Crop / Image
//! sections, nav (‹ Prev / Next ›) and Reject / Close.
//!
//! The controls emit `AppEvent::PhotoEdit { id, adjustments }` through the
//! `on_event` callback; the wiring re-renders the preview and pushes the result
//! back via `set_preview`/`set_histogram`/`set_photo`.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use gtk4::prelude::*;
use gtk4::{Box as GBox, Button, DrawingArea, Label, Orientation, Picture};

use crate::image::math::{clamp_crop, crop_to_pixels};
use crate::ui::slider::FineSlider;
use crate::image::process::export_dimensions;
use crate::image::srgb::{auto_wb, wb_from_pick};
use crate::image::types::{Adjustments, ExposureMode, NormalizedCrop};
use crate::state::{AppEvent, AppState};

/// Longest edge of an export render (must match the controller's EXPORT_EDGE).
const EXPORT_EDGE: u32 = 2560;

/// Read the photo's current adjustments straight from state (so slider/crop
/// edits merge onto the latest value instead of clobbering it).
fn current_adjustments(state: &AppState, id: u64) -> Adjustments {
    state
        .photos
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.adjustments)
        .unwrap_or_default()
}

/// Keyboard fine-tune stepping with grid snapping. EV/warmth step 0.05: an auto
/// value like +1.93 snaps to the nearest grid on the first press (+1.90 / +1.95),
/// then steps by 0.05. Tint always steps 0.01 (its grid is 0.01, so every value is
/// already on it). Integer hundredths avoid float drift.
fn fine_step(current: f64, dir: i8, step: f64) -> f64 {
    let unit = (step * 100.0).round() as i64; // 5 (0.05) or 1 (0.01)
    let cur = (current * 100.0).round() as i64; // current in hundredths
    // div_euclid/rem_euclid = floor division + non-negative remainder (Rust's `/`
    // truncates toward zero, which breaks snapping for negative values).
    let idx = cur.div_euclid(unit);
    let rem = cur.rem_euclid(unit);
    if rem == 0 {
        ((idx + dir as i64) * unit) as f64 / 100.0
    } else if dir < 0 {
        (idx * unit) as f64 / 100.0 // snap down to the grid
    } else {
        ((idx + 1) * unit) as f64 / 100.0 // snap up to the grid
    }
}

/// "output 2560 × 1709 px" from the full dimensions + active crop.
fn output_line(full: (u32, u32), crop: Option<NormalizedCrop>) -> String {
    let out = export_dimensions(full.0, full.1, crop.as_ref(), EXPORT_EDGE);
    format!("output {} × {} px", out.width, out.height)
}

/// The camera→sRGB matrix is carried as libraw's `rgb_cam[3][4]` (4 columns, 3
/// used); the WB math needs the 3×3 part.
fn cam_matrix3x3(m: Option<[[f32; 4]; 3]>) -> Option<[[f32; 3]; 3]> {
    m.map(|m| [
        [m[0][0], m[0][1], m[0][2]],
        [m[1][0], m[1][1], m[1][2]],
        [m[2][0], m[2][1], m[2][2]],
    ])
}

/// Grey-world reference for auto-WB (photoup `autoWhiteBalance`) over the LINEAR
/// pre-tone sample: restrict to the crop region, average only near-neutral bright
/// pixels (linear thresholds — max-min < 0.2, luminance > 0.1), and fall back to
/// the whole-region mean when too few qualify. Returns the channel means (0..1).
/// The sample is already downscaled to ≤96px, so no further downscale is needed.
fn auto_wb_mean_linear(rgb: &[f32], w: u32, h: u32, crop: Option<&NormalizedCrop>) -> Option<(f32, f32, f32)> {
    // Restrict to the crop region when one is set: WB auto must react to what's
    // actually in the frame after cropping, not the out-of-crop area.
    let (_, _, cropped) = match crop {
        Some(c) => {
            let rect = crop_to_pixels(c, w, h);
            let mut buf = Vec::with_capacity((rect.width * rect.height * 3) as usize);
            for y in 0..rect.height as usize {
                let src_off = ((rect.y as usize + y) * w as usize + rect.x as usize) * 3;
                buf.extend_from_slice(&rgb[src_off..src_off + rect.width as usize * 3]);
            }
            (rect.width, rect.height, buf)
        }
        None => (w, h, rgb.to_vec()),
    };
    let mut sr = 0.0f64;
    let mut sg = 0.0f64;
    let mut sb = 0.0f64;
    let mut n = 0usize;
    for px in cropped.chunks_exact(3) {
        let (r, g, b) = (px[0], px[1], px[2]);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        // Near-neutral, reasonably bright → neutral reference. Saturated scene
        // colours (grass, sky, walls) mustn't pull the WB into green/magenta.
        if max - min < 0.2 && 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.1 {
            sr += r as f64;
            sg += g as f64;
            sb += b as f64;
            n += 1;
        }
    }
    if n < 16 {
        // Too few neutral pixels: use the whole-region mean.
        sr = 0.0;
        sg = 0.0;
        sb = 0.0;
        n = 0;
        for px in cropped.chunks_exact(3) {
            sr += px[0] as f64;
            sg += px[1] as f64;
            sb += px[2] as f64;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    Some((
        (sr / n as f64) as f32,
        (sg / n as f64) as f32,
        (sb / n as f64) as f32,
    ))
}

/// WB Auto reference: crop-aware mean of the LINEAR sample, logging the region
/// stats. Returns the channel means that feed `wb_from_pick` (clinical Auto) and
/// `auto_wb` (warm Auto2).
fn auto_wb_feed(
    data: &[f32],
    pw: u32,
    ph: u32,
    crop: Option<&NormalizedCrop>,
) -> Option<(f32, f32, f32)> {
    let (rx0, ry0, rx1, ry1) = match crop {
        Some(c) => {
            let rect = crop_to_pixels(c, pw, ph);
            (
                rect.x as usize,
                rect.y as usize,
                (rect.x + rect.width) as usize,
                (rect.y + rect.height) as usize,
            )
        }
        None => (0, 0, pw as usize, ph as usize),
    };
    log_wb_sample_stats("auto", data, pw, ph, rx0, ry0, rx1, ry1);
    auto_wb_mean_linear(data, pw, ph, crop)
}

/// A square `win×win` sample window around (cx, cy), clamped to the sample bounds
/// — shared by `pick_sample_linear` and the `[wb]` debug log.
fn pick_window_sized(w: u32, h: u32, cx: f64, cy: f64, win: isize) -> (usize, usize, usize, usize) {
    let sx0 = ((cx.floor() as isize) - win / 2).max(0).min((w as isize - win).max(0));
    let sy0 = ((cy.floor() as isize) - win / 2).max(0).min((h as isize - win).max(0));
    (
        sx0 as usize,
        sy0 as usize,
        (sx0 + win) as usize,
        (sy0 + win) as usize,
    )
}

/// Mean of an RGB region plus the largest per-channel standard error of the mean
/// (std/√N) — the noise proxy the pick grows its window against.
fn region_mean_stderr(
    rgb: &[f32],
    w: u32,
    h: u32,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> Option<((f32, f32, f32), f32)> {
    let (mut s, mut s2) = ([0.0f64; 3], [0.0f64; 3]);
    let mut n = 0usize;
    for yy in y0..y1.min(h as usize) {
        let row = yy * w as usize;
        for xx in x0..x1.min(w as usize) {
            let o = (row + xx) * 3;
            for c in 0..3 {
                let v = rgb[o + c] as f64;
                s[c] += v;
                s2[c] += v * v;
            }
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    let nn = n as f64;
    let mean = [s[0] / nn, s[1] / nn, s[2] / nn];
    let sem = (0..3)
        .map(|c| ((s2[c] / nn - mean[c] * mean[c]).max(0.0)).sqrt() / nn.sqrt())
        .fold(0.0f64, f64::max) as f32;
    Some(((mean[0] as f32, mean[1] as f32, mean[2] as f32), sem))
}

/// Average a square area of the LINEAR sample around pixel (cx, cy) — the
/// GIMP-style grey-point picker (photoup `pickNeutral`). Starts at 7×7 and grows
/// while the region looks noisy (its relative standard error of the mean is too
/// large), so a high-ISO / dark pick stays stable: noise cancels as 1/√N and the
/// mean converges to the true surface cast instead of jittering. Returns the
/// channel means (0..1) and the window size used, or None if the region is empty.
fn pick_sample_linear(
    rgb: &[f32],
    w: u32,
    h: u32,
    cx: f64,
    cy: f64,
) -> Option<((f32, f32, f32), isize)> {
    const MIN_WIN: isize = 7;
    const MAX_WIN: isize = 31;
    const STEP: isize = 6;
    // Relative standard error of the mean low enough to trust the average.
    const NOISE_TOL: f32 = 0.01;
    let mut win = MIN_WIN;
    loop {
        let (x0, y0, x1, y1) = pick_window_sized(w, h, cx, cy, win);
        let Some(((r, g, b), sem)) = region_mean_stderr(rgb, w, h, x0, y0, x1, y1) else {
            return None;
        };
        let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        if sem / lum.max(1e-3) <= NOISE_TOL || win >= MAX_WIN {
            return Some(((r, g, b), win));
        }
        win += STEP;
    }
}

/// Per-channel min/max/mean of an interleaved LINEAR RGB sample region — the
/// `[wb]` debug aid, so the actual cast feeding the pick/auto math is visible.
fn log_wb_sample_stats(
    tag: &str,
    rgb: &[f32],
    w: u32,
    h: u32,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) {
    let (mut mn, mut mx, mut sm) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3], [0.0f64; 3]);
    let mut n = 0usize;
    for yy in y0..y1.min(h as usize) {
        let row = yy * w as usize;
        for xx in x0..x1.min(w as usize) {
            let o = (row + xx) * 3;
            for c in 0..3 {
                mn[c] = mn[c].min(rgb[o + c]);
                mx[c] = mx[c].max(rgb[o + c]);
                sm[c] += rgb[o + c] as f64;
            }
            n += 1;
        }
    }
    if n == 0 {
        return;
    }
    log::info!(
        "[wb] {tag} region {x0}..{x1},{y0}..{y1} (n={n}): mean r={:.4} g={:.4} b={:.4} | min r={:.4} g={:.4} b={:.4} | max r={:.4} g={:.4} b={:.4}",
        sm[0] / n as f64,
        sm[1] / n as f64,
        sm[2] / n as f64,
        mn[0],
        mn[1],
        mn[2],
        mx[0],
        mx[1],
        mx[2]
    );
}

/// A WB correction is clamped to the ±2 slider range; reflect it on the warmth +
/// hue sliders and the value label, then emit a `PhotoEdit` so the preview
/// re-renders with the new WB (photoup `updateAdjustments`).
#[allow(clippy::too_many_arguments)]
fn apply_wb(
    id: u64,
    offset: f32,
    hue: f32,
    state: &std::sync::Arc<std::sync::RwLock<AppState>>,
    on_event: &std::sync::Arc<dyn Fn(AppEvent) + Send + Sync + 'static>,
    suppress: &Rc<Cell<bool>>,
    temp: &FineSlider,
    hue_scale: &FineSlider,
    wb_lab: &Label,
) {
    let offset = offset.clamp(-4.0, 4.0); // warmth range is ±4
    let hue = hue.clamp(-1.0, 1.0); // tint range is ±1 (slider is −1..+1)
    suppress.set(true);
    temp.set_value(offset as f64);
    hue_scale.set_value(hue as f64);
    suppress.set(false);
    wb_lab.set_text(&format!("{:+.2} · {:+.2}", offset, hue));
    let mut adj = current_adjustments(&state.read().unwrap(), id);
    adj.wb_offset = offset;
    adj.hue = hue;
    on_event(AppEvent::PhotoEdit { id, adjustments: adj });
}

// ---- Crop overlay (photoup `.stage` + `.crop-box`) ----

/// Minimum crop size in normalized units (photoup `MIN_CROP`).
const MIN_CROP: f32 = 0.05;
/// Accent color, photoup `--accent` #ff7a45.
const ACCENT: (f64, f64, f64) = (0xFF as f64 / 255.0, 0x7A as f64 / 255.0, 0x45 as f64 / 255.0);
/// Handle fill, photoup `.h` background #f2eadf.
const HANDLE_FILL: (f64, f64, f64) = (0xF2 as f64 / 255.0, 0xEA as f64 / 255.0, 0xDF as f64 / 255.0);
/// Handle square size in px.
const HANDLE_SIZE: f64 = 14.0;
/// Half-extent hit radius (px) around a handle anchor for grabbing it.
const HANDLE_HIT: f64 = 26.0;
/// Border-grab tolerance (px): a press within this distance of a crop-box edge
/// grabs that edge for resizing. The crop box's 1.5px accent border is a thin
/// target on its own — this widens the grabbable band so clicking the border
/// resizes instead of silently doing nothing (the "handle not picked" bug).
const EDGE_HIT: f64 = 10.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Handle {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
}

#[derive(Clone, Copy)]
enum DragKind {
    Move,
    Resize { handle: Handle },
}

/// In-flight drag bookkeeping (mirrors photoup's `DragState`).
#[derive(Clone, Copy)]
struct DragState {
    kind: DragKind,
    start_crop: NormalizedCrop,
    disp_w: f64,
    disp_h: f64,
}

/// Letterboxed display rect of the full image inside the preview area, under
/// `object-fit: contain` (photoup `contentRect`).
struct Projection {
    disp_w: f64,
    disp_h: f64,
    ox: f64,
    oy: f64,
}

fn project(area_w: f64, area_h: f64, full: (u32, u32)) -> Option<Projection> {
    let fw = full.0 as f64;
    let fh = full.1 as f64;
    if fw <= 0.0 || fh <= 0.0 {
        return None;
    }
    let scale = (area_w / fw).min(area_h / fh);
    let disp_w = fw * scale;
    let disp_h = fh * scale;
    Some(Projection {
        disp_w,
        disp_h,
        ox: (area_w - disp_w) / 2.0,
        oy: (area_h - disp_h) / 2.0,
    })
}

/// The crop selection rect in widget px.
fn crop_rect(p: &Projection, c: &NormalizedCrop) -> (f64, f64, f64, f64) {
    (
        p.ox + c.x as f64 * p.disp_w,
        p.oy + c.y as f64 * p.disp_h,
        c.width as f64 * p.disp_w,
        c.height as f64 * p.disp_h,
    )
}

/// Map a press near the crop-box border to the resize handle it should grab.
/// Corners win over edges; returns `None` when the press is not near any edge
/// (the caller then falls through to Move / no-op). Used as a second pass after
/// the handle-anchor hit test so the whole border is grabbable, not just the 8
/// small handle squares.
fn edge_handle_at(x: f64, y: f64, rx: f64, ry: f64, rw: f64, rh: f64) -> Option<Handle> {
    let near_top = (y - ry).abs() <= EDGE_HIT;
    let near_bottom = (y - (ry + rh)).abs() <= EDGE_HIT;
    let near_left = (x - rx).abs() <= EDGE_HIT;
    let near_right = (x - (rx + rw)).abs() <= EDGE_HIT;
    match (near_top, near_bottom, near_left, near_right) {
        (true, _, true, _) => Some(Handle::Nw),
        (true, _, _, true) => Some(Handle::Ne),
        (_, true, true, _) => Some(Handle::Sw),
        (_, true, _, true) => Some(Handle::Se),
        (true, _, _, _) => Some(Handle::N),
        (_, true, _, _) => Some(Handle::S),
        (_, _, true, _) => Some(Handle::W),
        (_, _, _, true) => Some(Handle::E),
        _ => None,
    }
}

/// The 8 handle anchors (corners + edge midpoints) of the crop rect.
fn handle_anchors(rx: f64, ry: f64, rw: f64, rh: f64) -> [(Handle, f64, f64); 8] {
    [
        (Handle::Nw, rx, ry),
        (Handle::N, rx + rw / 2.0, ry),
        (Handle::Ne, rx + rw, ry),
        (Handle::E, rx + rw, ry + rh / 2.0),
        (Handle::Se, rx + rw, ry + rh),
        (Handle::S, rx + rw / 2.0, ry + rh),
        (Handle::Sw, rx, ry + rh),
        (Handle::W, rx, ry + rh / 2.0),
    ]
}

/// Drag inside the selection: shift the rect, clamped to [0,1].
fn move_crop(c: NormalizedCrop, ndx: f32, ndy: f32) -> NormalizedCrop {
    clamp_crop(
        NormalizedCrop {
            x: (c.x + ndx).clamp(0.0, 1.0 - c.width),
            y: (c.y + ndy).clamp(0.0, 1.0 - c.height),
            ..c
        },
        MIN_CROP,
    )
}

/// Resize one edge/corner; the opposite edge stays put (photoup default path).
fn resize_crop(c: NormalizedCrop, handle: Handle, ndx: f32, ndy: f32) -> NormalizedCrop {
    let mut x = c.x;
    let mut y = c.y;
    let mut width = c.width;
    let mut height = c.height;
    if matches!(handle, Handle::E | Handle::Ne | Handle::Se) {
        width = (c.width + ndx).clamp(MIN_CROP, (1.0 - c.x).max(MIN_CROP));
    }
    if matches!(handle, Handle::W | Handle::Nw | Handle::Sw) {
        x = (c.x + ndx).clamp(0.0, c.x + c.width - MIN_CROP);
        width = c.x + c.width - x;
    }
    if matches!(handle, Handle::S | Handle::Se | Handle::Sw) {
        height = (c.height + ndy).clamp(MIN_CROP, (1.0 - c.y).max(MIN_CROP));
    }
    if matches!(handle, Handle::N | Handle::Nw | Handle::Ne) {
        y = (c.y + ndy).clamp(0.0, c.y + c.height - MIN_CROP);
        height = c.y + c.height - y;
    }
    clamp_crop(NormalizedCrop { x, y, width, height }, MIN_CROP)
}

/// Shift pressed: keep the aspect ratio (photoup `resizeCrop` shift branch).
fn resize_crop_shift(c: NormalizedCrop, handle: Handle, ndx: f32, ndy: f32) -> NormalizedCrop {
    let is_side = matches!(handle, Handle::N | Handle::S | Handle::E | Handle::W);
    if is_side {
        if matches!(handle, Handle::E | Handle::W) {
            let (x, w) = if handle == Handle::E {
                (c.x, (c.width + ndx).clamp(MIN_CROP, (1.0 - c.x).max(MIN_CROP)))
            } else {
                let x = (c.x + ndx).clamp(0.0, c.x + c.width - MIN_CROP);
                (x, c.x + c.width - x)
            };
            let h = c.height * (w / c.width);
            return clamp_crop(
                NormalizedCrop { x, y: c.y + (c.height - h) / 2.0, width: w, height: h },
                MIN_CROP,
            );
        }
        let (y, h) = if handle == Handle::S {
            (c.y, (c.height + ndy).clamp(MIN_CROP, (1.0 - c.y).max(MIN_CROP)))
        } else {
            let y = (c.y + ndy).clamp(0.0, c.y + c.height - MIN_CROP);
            (y, c.y + c.height - y)
        };
        let w = c.width * (h / c.height);
        return clamp_crop(
            NormalizedCrop { x: c.x + (c.width - w) / 2.0, y, width: w, height: h },
            MIN_CROP,
        );
    }
    // Corner handles anchor the opposite corner.
    let sx = match handle {
        Handle::E | Handle::Ne | Handle::Se => ndx,
        Handle::W | Handle::Nw | Handle::Sw => -ndx,
        _ => 0.0,
    } / c.width;
    let sy = match handle {
        Handle::S | Handle::Se | Handle::Sw => ndy,
        Handle::N | Handle::Nw | Handle::Ne => -ndy,
        _ => 0.0,
    } / c.height;
    let s = 1.0 + sx.max(sy);
    let max_w = if matches!(handle, Handle::W | Handle::Nw | Handle::Sw) {
        c.x + c.width
    } else {
        1.0 - c.x
    }
    .max(MIN_CROP);
    let max_h = if matches!(handle, Handle::N | Handle::Nw | Handle::Ne) {
        c.y + c.height
    } else {
        1.0 - c.y
    }
    .max(MIN_CROP);
    let scale = ((c.width * s).clamp(MIN_CROP, max_w) / c.width)
        .min((c.height * s).clamp(MIN_CROP, max_h) / c.height);
    let w = c.width * scale;
    let h = c.height * scale;
    let x = if matches!(handle, Handle::W | Handle::Nw | Handle::Sw) {
        c.x + c.width - w
    } else {
        c.x
    };
    let y = if matches!(handle, Handle::N | Handle::Nw | Handle::Ne) {
        c.y + c.height - h
    } else {
        c.y
    };
    clamp_crop(NormalizedCrop { x, y, width: w, height: h }, MIN_CROP)
}

/// Alt pressed: resize around the crop's center — the center stays put and both
/// edges move symmetrically, instead of `resize_crop`'s opposite-edge anchor.
fn resize_crop_center(c: NormalizedCrop, handle: Handle, ndx: f32, ndy: f32) -> NormalizedCrop {
    let cx = c.x + c.width / 2.0;
    let cy = c.y + c.height / 2.0;
    let max_w = (2.0 * cx.min(1.0 - cx)).max(MIN_CROP);
    let max_h = (2.0 * cy.min(1.0 - cy)).max(MIN_CROP);
    let mut width = c.width;
    let mut height = c.height;
    if matches!(handle, Handle::E | Handle::Ne | Handle::Se) {
        width = (c.width + ndx).clamp(MIN_CROP, max_w);
    }
    if matches!(handle, Handle::W | Handle::Nw | Handle::Sw) {
        width = (c.width - ndx).clamp(MIN_CROP, max_w);
    }
    if matches!(handle, Handle::S | Handle::Se | Handle::Sw) {
        height = (c.height + ndy).clamp(MIN_CROP, max_h);
    }
    if matches!(handle, Handle::N | Handle::Nw | Handle::Ne) {
        height = (c.height - ndy).clamp(MIN_CROP, max_h);
    }
    clamp_crop(
        NormalizedCrop { x: cx - width / 2.0, y: cy - height / 2.0, width, height },
        MIN_CROP,
    )
}

/// Alt+Shift pressed: keep the aspect ratio AND keep the center fixed — the crop
/// scales around its center so the dragged handle follows the pointer. A side drag
/// moves that one edge (the opposite moves symmetrically, so the dimension changes
/// by 2× the pointer delta); a corner drag scales by its dominant axis. Inward
/// drags shrink, outward grow; the result is clamped to stay in-frame and ≥ MIN_CROP.
fn resize_crop_shift_center(c: NormalizedCrop, handle: Handle, ndx: f32, ndy: f32) -> NormalizedCrop {
    let cx = c.x + c.width / 2.0;
    let cy = c.y + c.height / 2.0;
    // Side handles scale off their single axis (allowing a negative `s` to shrink);
    // the old `sx.max(sy)` left the zero second axis in the max, so an inward side
    // drag clamped to `s = 1.0` — sides could only grow, never shrink.
    let s = match handle {
        Handle::E => 1.0 + 2.0 * ndx / c.width,
        Handle::W => 1.0 - 2.0 * ndx / c.width,
        Handle::S => 1.0 + 2.0 * ndy / c.height,
        Handle::N => 1.0 - 2.0 * ndy / c.height,
        _ => {
            let sx = match handle {
                Handle::Ne | Handle::Se => ndx,
                Handle::Nw | Handle::Sw => -ndx,
                _ => 0.0,
            } / c.width;
            let sy = match handle {
                Handle::Se | Handle::Sw => ndy,
                Handle::Ne | Handle::Nw => -ndy,
                _ => 0.0,
            } / c.height;
            1.0 + 2.0 * sx.max(sy)
        }
    };
    let max_w = (2.0 * cx.min(1.0 - cx)).max(MIN_CROP);
    let max_h = (2.0 * cy.min(1.0 - cy)).max(MIN_CROP);
    let scale = ((c.width * s).clamp(MIN_CROP, max_w) / c.width)
        .min((c.height * s).clamp(MIN_CROP, max_h) / c.height);
    let w = c.width * scale;
    let h = c.height * scale;
    clamp_crop(NormalizedCrop { x: cx - w / 2.0, y: cy - h / 2.0, width: w, height: h }, MIN_CROP)
}

fn rounded_rect(cr: &gtk4::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
    cr.close_path();
}

/// Draw the dim-outside + accent border + 8 handles for the current crop.
fn draw_crop_overlay(
    cr: &gtk4::cairo::Context,
    area_w: f64,
    area_h: f64,
    full: (u32, u32),
    c: NormalizedCrop,
) {
    let Some(p) = project(area_w, area_h, full) else {
        return;
    };
    let (rx, ry, rw, rh) = crop_rect(&p, &c);

    // Dim everything outside the selection (photoup `box-shadow: 0 0 0 9999px`).
    cr.rectangle(0.0, 0.0, area_w, area_h);
    cr.rectangle(rx, ry, rw, rh);
    cr.set_fill_rule(gtk4::cairo::FillRule::EvenOdd);
    cr.set_source_rgba(6.0 / 255.0, 5.0 / 255.0, 4.0 / 255.0, 0.45);
    let _ = cr.fill();

    // Accent border.
    let lw = 1.5;
    cr.set_line_width(lw);
    cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2);
    cr.rectangle(rx + lw / 2.0, ry + lw / 2.0, rw - lw, rh - lw);
    let _ = cr.stroke();

    // 8 drag handles at corners/edges.
    let hs = HANDLE_SIZE;
    for (_h, hx, hy) in handle_anchors(rx, ry, rw, rh) {
        rounded_rect(cr, hx - hs / 2.0, hy - hs / 2.0, hs, hs, 3.0);
        cr.set_source_rgb(HANDLE_FILL.0, HANDLE_FILL.1, HANDLE_FILL.2);
        let _ = cr.fill_preserve();
        cr.set_line_width(1.5);
        cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2);
        let _ = cr.stroke();
    }
}

/// A click handler that applies a crop-preset ratio (photoup `applyPreset`):
/// computes a centered crop rect matching the ratio against the source aspect.
/// The result updates the overlay selection (crop cell + redraw), the output
/// line, and the photo's adjustments.
fn crop_preset_handler(
    ratio: f32,
    active_id: Rc<Cell<Option<u64>>>,
    state: Arc<RwLock<AppState>>,
    on_event: Arc<dyn Fn(AppEvent) + Send + Sync + 'static>,
    full_size: Rc<Cell<Option<(u32, u32)>>>,
    crop_cell: Rc<RefCell<Option<NormalizedCrop>>>,
    crop_area: DrawingArea,
    info2: Label,
) -> impl Fn(&Button) + 'static {
    move |_| {
        let Some(id) = active_id.get() else { return };
        let Some((fw, fh)) = full_size.get() else { return };
        if fw == 0 || fh == 0 {
            return;
        }
        let target = ratio * (fh as f32 / fw as f32);
        let (w, h) = if target >= 1.0 {
            (1.0, 1.0 / target)
        } else {
            (target, 1.0)
        };
        let crop = clamp_crop(
            NormalizedCrop {
                x: (1.0 - w) / 2.0,
                y: (1.0 - h) / 2.0,
                width: w,
                height: h,
            },
            MIN_CROP,
        );
        *crop_cell.borrow_mut() = Some(crop);
        crop_area.queue_draw();
        let mut adj = current_adjustments(&state.read().unwrap(), id);
        adj.crop = Some(crop);
        info2.set_text(&output_line((fw, fh), Some(crop)));
        on_event(AppEvent::PhotoEdit { id, adjustments: adj });
    }
}

/// A rotate-button handler: adds `delta` quarter-turns CW (1 = 90° CW, 3 = 90°
/// CCW) to the photo's rotation. Rotation swaps the display dims for odd deltas,
/// so the Image section and the crop overlay must re-project against the new
/// dims immediately — the controller re-renders the preview, but the editor's
/// `full_size` cell is only refreshed here (set_photo runs once per photo).
fn rotate_handler(
    delta: u8,
    active_id: Rc<Cell<Option<u64>>>,
    state: Arc<RwLock<AppState>>,
    on_event: Arc<dyn Fn(AppEvent) + Send + Sync + 'static>,
    full_size: Rc<Cell<Option<(u32, u32)>>>,
    crop_area: DrawingArea,
    info1: Label,
    info2: Label,
    is_raw: Rc<Cell<bool>>,
) -> impl Fn(&Button) + 'static {
    move |_| {
        let Some(id) = active_id.get() else { return };
        let mut adj = current_adjustments(&state.read().unwrap(), id);
        adj.rotation = (adj.rotation + delta) % 4;
        if let Some((fw, fh)) = full_size.get() {
            // rotate_dims(display, delta): odd deltas swap W/H, even keep them.
            let (nfw, nfh) = if delta % 2 == 1 { (fh, fw) } else { (fw, fh) };
            full_size.set(Some((nfw, nfh)));
            let src = if is_raw.get() { "RAW" } else { "JPEG" };
            info1.set_text(&format!("{src} · {} × {}", nfw, nfh));
            info2.set_text(&output_line((nfw, nfh), adj.crop));
        }
        crop_area.queue_draw();
        on_event(AppEvent::PhotoEdit { id, adjustments: adj });
    }
}

pub struct EditorScreen {
    pub root: GBox,
    pub preview: Picture,
    /// Overlay that draws the crop-selection rectangle + handles + dim.
    crop_area: DrawingArea,
    pub nav_prev: Button,
    pub nav_next: Button,
    file_label: Label,
    histogram_area: DrawingArea,
    exposure_slider: FineSlider,
    temp_slider: FineSlider,
    hue_slider: FineSlider,
    ev_value: Label,
    wb_value: Label,
    info1: Label,
    info2: Label,
    auto_exposure_btn: Button,
    burn_exposure_btn: Button,
    rest_exposure_btn: Button,
    wb_auto_button: Button,
    wb_auto2_button: Button,
    reset_wb_btn: Button,
    crop_11: Button,
    crop_23: Button,
    crop_32: Button,
    crop_orig: Button,
    rotate_ccw: Button,
    rotate_cw: Button,
    reject_button: Button,
    close_button: Button,
    /// Current photo id (set by `set_photo`), read by the signal closures.
    active_id: Rc<Cell<Option<u64>>>,
    /// Full source dimensions, needed for the Image section + crop presets.
    full_size: Rc<Cell<Option<(u32, u32)>>>,
    /// The active crop selection, mirrored from the photo's adjustments and
    /// updated live by presets / drags; the overlay draws from this.
    crop: Rc<RefCell<Option<NormalizedCrop>>>,
    /// The active photo's LINEAR (0..1) pre-tone RGB sample (≤96px edge), pushed
    /// by the controller on decode; the WB Auto / neutral-picker sample from it.
    /// Sampling the processed preview would distort the R/B ratio (auto-exposure,
    /// rolloff, camera S-curve, sRGB), understating a strong cast — hence the
    /// linear pre-tone base sample.
    wb_sample: Rc<RefCell<Option<(Vec<f32>, u32, u32)>>>,
    /// The active photo's camera→sRGB color matrix (RAW only; `None` for JPEG /
    /// no matrix), pushed by the controller for WB Auto/Pick.
    cam_matrix: Rc<RefCell<Option<[[f32; 4]; 3]>>>,
    is_raw: Rc<Cell<bool>>,
    /// The active exposure mode — `set_ev` only moves the EV slider in auto
    /// modes (in Manual the slider IS the user's manual value and must not jump).
    current_mode: Rc<Cell<ExposureMode>>,
    /// Suppresses PhotoEdit emission while `set_photo`/buttons program the
    /// controls (their `set_value` calls fire signals synchronously).
    suppress: Rc<Cell<bool>>,
    state: Arc<RwLock<AppState>>,
    /// Shared with every control closure (a plain `Box` can't be split across
    /// multiple `'static` signal handlers).
    on_event: Arc<dyn Fn(AppEvent) + Send + Sync + 'static>,
}

impl EditorScreen {
    pub fn new(
        state: Arc<RwLock<AppState>>,
        on_event: Arc<dyn Fn(AppEvent) + Send + Sync + 'static>,
    ) -> Self {
        let root = GBox::new(Orientation::Horizontal, 8);
        root.set_margin_top(8);
        root.set_margin_bottom(8);
        root.set_margin_start(8);
        root.set_margin_end(8);
        root.add_css_class("dark-bg");

        // Left: live preview (contain, like photoup's object-fit: contain) with a
        // transparent crop-selection overlay on top (photoup's `.stage` + `.crop-box`).
        // The stage reads as a dark development bed; the photo is the brightest thing.
        let overlay = gtk4::Overlay::new();
        overlay.add_css_class("editor-stage");
        overlay.set_vexpand(true);
        overlay.set_hexpand(true);
        let preview = Picture::new();
        preview.set_vexpand(true);
        preview.set_hexpand(true);
        // Don't let the full-res texture's natural width force the layout — the
        // preview shrinks to whatever the left side has left over.
        preview.set_can_shrink(true);
        preview.set_content_fit(gtk4::ContentFit::Contain);
        overlay.set_child(Some(&preview));
        let crop_area = DrawingArea::new();
        // Keep the overlay's size request driven by the preview, not this
        // DrawingArea: request ~0 so it never squeezes the 332px panel. It still
        // fills the overlay via the Fill alignment below.
        crop_area.set_width_request(1);
        crop_area.set_height_request(1);
        crop_area.set_vexpand(true);
        crop_area.set_hexpand(true);
        crop_area.set_halign(gtk4::Align::Fill);
        crop_area.set_valign(gtk4::Align::Fill);
        overlay.add_overlay(&crop_area);
        root.append(&overlay);
        let crop: Rc<RefCell<Option<NormalizedCrop>>> = Rc::new(RefCell::new(None));

        // Right: control panel, scrollable if the window is short. The scrolled
        // window must keep its 332px minimum, or the preview picture (which
        // requests its full texture width) would squeeze the panel away.
        let panel_scroll = gtk4::ScrolledWindow::new();
        // Fixed-width side panel: non-expanding so it stays 332px and the left
        // preview takes all remaining space.
        panel_scroll.set_width_request(332);
        panel_scroll.set_hexpand(false);
        panel_scroll.set_vexpand(true);
        // Hide the vertical scrollbar (the panel can overflow on short windows)
        // but keep wheel scrolling: policy only controls scrollbar visibility.
        panel_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Never);
        let panel = GBox::new(Orientation::Vertical, 10);
        panel.set_width_request(332);
        panel.set_hexpand(false);

        let file_label = Label::new(Some(""));
        file_label.add_css_class("editor-file");
        file_label.set_halign(gtk4::Align::Start);
        file_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        file_label.set_tooltip_text(Some(""));
        panel.append(&file_label);

        // Histogram (neutral frame until the wiring calls `set_histogram`).
        let histogram_area = DrawingArea::new();
        histogram_area.set_height_request(120);
        histogram_area.set_draw_func(|_area, cr, _width, _height| {
            // Warm near-black bed (Darkroom bg) so the RGB channels glow.
            cr.set_source_rgb(0.086, 0.075, 0.059);
            let _ = cr.paint();
        });
        panel.append(&histogram_area);

        // ---- Exposure ----
        panel.append(&section_label("Exposure"));
        // Custom fine slider: drags snap to the 0.05 grid (no GTK Scale
        // precision-mode / 0-stickiness). The zero-correction position is drawn
        // as a subtle tick on the track — a reference, not a snap point.
        let exposure_slider = FineSlider::new(-3.0, 5.0, 0.05);
        panel.append(&exposure_slider.area());

        let ev_row = GBox::new(Orientation::Horizontal, 6);
        let auto_exposure_btn = Button::with_label("Auto");
        let burn_exposure_btn = Button::with_label("Burn");
        let rest_exposure_btn = Button::with_label("Rest");
        let ev_value = Label::new(Some("+0.00 EV"));
        ev_value.add_css_class("editor-value");
        ev_value.set_hexpand(true);
        ev_value.set_halign(gtk4::Align::End);
        // Fixed character width so the label never jitters as the value changes
        // ("+0.00 EV" ↔ "+?.?? EV" ↔ "-1.25 EV" are all 5 chars + unit).
        ev_value.set_width_chars(6);
        ev_row.append(&auto_exposure_btn);
        ev_row.append(&burn_exposure_btn);
        ev_row.append(&rest_exposure_btn);
        ev_row.append(&ev_value);
        panel.append(&ev_row);

        // ---- White balance ----
        panel.append(&section_label("White balance"));
        // Warmth range −4..+4: some images need a stronger cool shift than ±2
        // (−2 was still reddish). At −4 red is quartered / blue quadrupled.
        let temp_slider = FineSlider::new(-4.0, 4.0, 0.05);
        panel.append(&temp_slider.area());

        // Tint (hue) is fine-grained: −1..+1 at 0.01 steps (the old −2..+2 was too
        // wide — tinting is a subtle correction).
        let hue_slider = FineSlider::new(-1.0, 1.0, 0.01);
        panel.append(&hue_slider.area());

        let wb_row = GBox::new(Orientation::Horizontal, 6);
        // Auto = clinical neutralization of the frame's neutral reference; Auto2 =
        // the same but keeping the warm ambience (Nikon AUTO2 "keep warm lighting").
        // The neutral-pick needs no button — clicking the preview samples a neutral
        // point directly (except on crop handles).
        let wb_auto_button = Button::with_label("Auto");
        wb_auto_button.set_tooltip_text(Some("Neutralize the warm/cool cast (clinical)"));
        let wb_auto2_button = Button::with_label("Auto2");
        wb_auto2_button.set_tooltip_text(Some("Neutralize but keep the warm ambience (Nikon AUTO2)"));
        let reset_wb_btn = Button::with_label("Reset");
        let wb_value = Label::new(Some("+0.00 · +0.00"));
        wb_value.add_css_class("editor-value");
        wb_value.set_hexpand(true);
        wb_value.set_halign(gtk4::Align::End);
        wb_value.set_width_chars(13);
        wb_row.append(&wb_auto_button);
        wb_row.append(&wb_auto2_button);
        wb_row.append(&reset_wb_btn);
        wb_row.append(&wb_value);
        // Auto-WB + neutral-pick now have the preview pixels + camera matrix
        // wired (see wire_buttons) — enabled.
        panel.append(&wb_row);

        // ---- Crop ----
        panel.append(&section_label("Crop"));
        let presets = GBox::new(Orientation::Horizontal, 6);
        let crop_11 = Button::with_label("1:1");
        let crop_23 = Button::with_label("2:3");
        let crop_32 = Button::with_label("3:2");
        let crop_orig = Button::with_label("Original");
        for b in [&crop_11, &crop_23, &crop_32, &crop_orig] {
            b.set_hexpand(true);
            presets.append(b);
        }
        panel.append(&presets);

        // ---- Rotate ---- (user rotation on top of any EXIF/libraw flip)
        panel.append(&section_label("Rotate"));
        let rotate_row = GBox::new(Orientation::Horizontal, 6);
        let rotate_ccw = Button::with_label("↺ CCW");
        let rotate_cw = Button::with_label("↻ CW");
        rotate_ccw.set_tooltip_text(Some("Rotate 90° counter-clockwise"));
        rotate_cw.set_tooltip_text(Some("Rotate 90° clockwise"));
        rotate_ccw.set_hexpand(true);
        rotate_cw.set_hexpand(true);
        rotate_row.append(&rotate_ccw);
        rotate_row.append(&rotate_cw);
        panel.append(&rotate_row);

        // ---- Image ----
        panel.append(&section_label("Image"));
        let info = GBox::new(Orientation::Vertical, 2);
        let info1 = Label::new(Some(""));
        info1.add_css_class("editor-value");
        info1.set_halign(gtk4::Align::Start);
        let info2 = Label::new(Some(""));
        info2.add_css_class("editor-value");
        info2.set_halign(gtk4::Align::Start);
        info.append(&info1);
        info.append(&info2);
        panel.append(&info);

        panel_scroll.set_child(Some(&panel));

        // Right column: the scrollable control panel on top, and a FIXED bottom
        // block (hints + nav + reject/close) below it — pinned to the bottom of
        // the editor and always reachable even when the panel scrolls on short
        // windows. The framed background wraps the whole column so the bottom
        // block reads as part of the same panel.
        let right_col = GBox::new(Orientation::Vertical, 8);
        right_col.add_css_class("editor-panel");
        right_col.set_width_request(332);
        right_col.set_hexpand(false);
        right_col.set_vexpand(true);
        right_col.set_margin_start(4);
        right_col.append(&panel_scroll);

        // Hint (crop-interaction hint; crop drag is deferred in the port).
        let hint = Label::new(Some(
            "Drag handles to resize · drag inside to move · Shift keeps ratio",
        ));
        hint.add_css_class("dim-label");
        hint.set_halign(gtk4::Align::Start);
        hint.set_wrap(true);

        // Keyboard fine-tune hint.
        let kb_hint = Label::new(Some("Fine-tune: Q/W exposure · A/S warmth · Z/X tint"));
        kb_hint.add_css_class("dim-label");
        kb_hint.set_halign(gtk4::Align::Start);
        kb_hint.set_wrap(true);

        let bottom_block = GBox::new(Orientation::Vertical, 6);
        bottom_block.append(&hint);
        bottom_block.append(&kb_hint);

        // Nav: ‹ Prev | Next ›.
        let nav_row = GBox::new(Orientation::Horizontal, 6);
        let nav_prev = Button::with_label("‹ Prev");
        let nav_next = Button::with_label("Next ›");
        nav_prev.set_hexpand(true);
        nav_next.set_hexpand(true);
        nav_row.append(&nav_prev);
        nav_row.append(&nav_next);
        bottom_block.append(&nav_row);

        // Bottom: Reject | Close.
        let bottom = GBox::new(Orientation::Horizontal, 6);
        let reject_button = Button::with_label("Reject");
        reject_button.add_css_class("editor-reject");
        let close_button = Button::with_label("Close");
        reject_button.set_hexpand(true);
        close_button.set_hexpand(true);
        bottom.append(&reject_button);
        bottom.append(&close_button);
        bottom_block.append(&bottom);

        right_col.append(&bottom_block);
        root.append(&right_col);

        let screen = Self {
            root,
            preview,
            crop_area,
            nav_prev,
            nav_next,
            file_label,
            histogram_area,
            exposure_slider,
            temp_slider,
            hue_slider,
            ev_value,
            wb_value,
            info1,
            info2,
            auto_exposure_btn,
            burn_exposure_btn,
            rest_exposure_btn,
            wb_auto_button,
            wb_auto2_button,
            reset_wb_btn,
            crop_11,
            crop_23,
            crop_32,
            crop_orig,
            rotate_ccw,
            rotate_cw,
            reject_button,
            close_button,
            active_id: Rc::new(Cell::new(None)),
            full_size: Rc::new(Cell::new(None)),
            crop,
            wb_sample: Rc::new(RefCell::new(None)),
            cam_matrix: Rc::new(RefCell::new(None)),
            is_raw: Rc::new(Cell::new(false)),
            current_mode: Rc::new(Cell::new(ExposureMode::Auto)),
            suppress: Rc::new(Cell::new(false)),
            state,
            on_event,
        };

        screen.wire_crop_overlay();
        screen.wire_controls();
        screen.wire_buttons();
        screen
    }

    /// Push a photo into the editor: set active id, name, adjustments and the
    /// info/nav rows. `shown_ev` is the effective EV (autoEV in auto/burn,
    /// the manual EV otherwise) — what the slider should display.
    pub fn set_photo(
        &mut self,
        id: u64,
        name: &str,
        adjustments: &Adjustments,
        shown_ev: f32,
        is_raw: bool,
        full_size: Option<(u32, u32)>,
    ) {
        self.suppress.set(true);
        self.active_id.set(Some(id));
        self.is_raw.set(is_raw);
        self.current_mode.set(adjustments.exposure_mode);
        self.full_size.set(full_size);
        self.crop.replace(adjustments.crop);
        // New photo: drop the previous photo's per-photo data (linear WB sample,
        // camera matrix).
        self.wb_sample.borrow_mut().take();
        self.cam_matrix.borrow_mut().take();
        self.crop_area.queue_draw();
        self.file_label.set_text(name);
        self.file_label.set_tooltip_text(Some(name));
        self.exposure_slider.set_value(shown_ev as f64);
        self.temp_slider.set_value(adjustments.wb_offset as f64);
        self.hue_slider.set_value(adjustments.hue as f64);
        self.suppress.set(false);
        self.refresh_value_labels();
        self.refresh_image_info();
    }

    pub fn set_preview(&self, texture: Option<&gdk4::Texture>) {
        self.preview.set_paintable(texture);
    }

    /// Set the display dimensions of the active photo. `dims` must be the
    /// DISPLAY-oriented size (already rotated) — the crop projection and the
    /// handle hit-test scale normalized crop coordinates against it.
    ///
    /// Without this the crop box is never drawn and crop drags silently no-op
    /// (both `draw_crop_overlay` and `drag_begin` bail when `full_size` is None).
    /// `set_photo` normally fills it, but a photo opened before its thumbnail has
    /// finished decoding gets filled here once the preview render lands.
    pub fn set_full_size(&self, dims: (u32, u32)) {
        self.full_size.set(Some(dims));
        self.crop_area.queue_draw();
    }

    /// Fine-tune the exposure EV slider (keyboard Q/W). Moving the slider fires its
    /// value_changed → PhotoEdit (switches to Manual EV, like dragging it by hand).
    /// Steps snap to the 0.05 grid: an auto value like +1.93 → +1.90 (Q) / +1.95 (W).
    pub fn fine_tune_ev(&self, dir: i8) {
        let v = self.exposure_slider.value();
        self.exposure_slider.set_value(fine_step(v, dir, 0.05));
    }

    /// Fine-tune the warmth (temperature) slider (keyboard A/S), 0.05 grid.
    pub fn fine_tune_wb(&self, dir: i8) {
        let v = self.temp_slider.value();
        self.temp_slider.set_value(fine_step(v, dir, 0.05));
    }

    /// Fine-tune the tint (hue) slider (keyboard Z/X) — always fine 0.01 steps.
    pub fn fine_tune_tint(&self, dir: i8) {
        let v = self.hue_slider.value();
        self.hue_slider.set_value(fine_step(v, dir, 0.01));
    }

    /// Set the EV indicator ("+0.35 EV"); `{:+.2}` keeps the width fixed so the
    /// label doesn't jitter as auto/manual EVs change. Also moves the EV slider to
    /// the real EV in auto modes (photoup: the slider value = the effective EV,
    /// so "0 correction" sits where the correction is, not always at the center).
    pub fn set_ev(&self, ev: f32) {
        self.ev_value.set_text(&format!("{ev:+.2} EV"));
        if self.current_mode.get() != ExposureMode::Manual {
            self.suppress.set(true);
            self.exposure_slider.set_value(ev as f64);
            self.suppress.set(false);
        }
    }

    /// Cache the active photo's LINEAR (0..1) pre-tone RGB sample so WB Auto and
    /// the neutral-picker can measure the true sensor cast.
    pub fn set_wb_sample(&self, rgba: Vec<f32>, w: u32, h: u32) {
        *self.wb_sample.borrow_mut() = Some((rgba, w, h));
    }

    /// Push the active photo's camera→sRGB color matrix (RAW) for WB Auto/Pick.
    pub fn set_cam_matrix(&self, cam_matrix: Option<[[f32; 4]; 3]>) {
        self.cam_matrix.replace(cam_matrix);
    }

    /// Drop the per-photo data the editor holds (linear WB sample, camera matrix)
    /// — called when the editor closes so a later photo can't read a stale
    /// previous photo's sample/matrix.
    pub fn release_photo_data(&self) {
        self.wb_sample.borrow_mut().take();
        self.cam_matrix.borrow_mut().take();
        // Clear the preview so closing the editor never leaves the previous
        // photo's image visible while the next one renders.
        self.preview.set_paintable(None::<&gdk4::Texture>);
    }

    /// RGB histogram (768 bins: 256 R, 256 G, 256 B) drawn as three translucent
    /// channels overlaid — the default editor view. The luminance histogram
    /// (`compute_histogram`, 256 bins) is kept in the pipeline for a revert.
    pub fn set_histogram(&self, bins: &[u32]) {
        let bins: Vec<u32> = bins.to_vec();
        self.histogram_area.set_draw_func(move |_area, cr, width, height| {
            let h = height as f64;
            let w = width as f64;
            // Warm near-black bed (Darkroom bg) — the RGB channels glow against it.
            cr.set_source_rgb(0.086, 0.075, 0.059);
            let _ = cr.paint();
            if bins.len() < 256 * 3 {
                return; // luminance (256-bin) data or none — nothing to draw
            }
            let max = bins.iter().copied().max().unwrap_or(1).max(1) as f64;
            let n = 256.0;
            let channels = [
                ((0.95, 0.30, 0.30), 0usize),   // R
                ((0.30, 0.90, 0.40), 256),       // G
                ((0.35, 0.45, 0.95), 512),       // B
            ];
            for (rgb, off) in channels {
                cr.set_source_rgba(rgb.0, rgb.1, rgb.2, 0.62);
                for i in 0..256usize {
                    let v = bins[off + i];
                    let x0 = (i as f64 / n) * w;
                    let x1 = ((i + 1) as f64 / n) * w;
                    let bh = (v as f64 / max) * h;
                    cr.rectangle(x0, h - bh, (x1 - x0).max(1.0), bh);
                }
                let _ = cr.fill();
            }
        });
    }

    pub fn set_nav(&self, has_prev: bool, has_next: bool) {
        self.nav_prev.set_sensitive(has_prev);
        self.nav_next.set_sensitive(has_next);
    }

    fn refresh_value_labels(&self) {
        self.ev_value
            .set_text(&format!("{:+.2} EV", self.exposure_slider.value()));
        self.wb_value.set_text(&format!(
            "{:+.2} · {:+.2}",
            self.temp_slider.value(),
            self.hue_slider.value()
        ));
    }

    fn refresh_image_info(&self) {
        match (self.full_size.get(), self.active_id.get()) {
            (Some(full), Some(id)) => {
                let src = if self.is_raw.get() { "RAW" } else { "JPEG" };
                self.info1
                    .set_text(&format!("{src} · {} × {}", full.0, full.1));
                let adj = current_adjustments(&self.state.read().unwrap(), id);
                self.info2.set_text(&output_line(full, adj.crop));
            }
            _ => {
                self.info1.set_text("");
                self.info2.set_text("");
            }
        }
    }

    fn wire_controls(&self) {
        let on_event = Arc::clone(&self.on_event);
        let active_id = Rc::clone(&self.active_id);
        let state = Arc::clone(&self.state);
        let suppress = Rc::clone(&self.suppress);
        let ev = self.exposure_slider.clone();
        let temp = self.temp_slider.clone();
        let hue = self.hue_slider.clone();
        let ev_lab = self.ev_value.clone();
        let wb_lab = self.wb_value.clone();

        // EV slider: dragging sets Manual exposure with the slider value.
        let (a, o, s, st, lab) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Rc::clone(&suppress),
            Arc::clone(&state),
            ev_lab.clone(),
        );
        ev.connect_change(move |v| {
            if s.get() {
                return;
            }
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.exposure_mode = ExposureMode::Manual;
            adj.exposure_ev = v as f32;
            lab.set_text(&format!("{:+.2} EV", adj.exposure_ev));
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });

        // Temperature slider.
        let (a, o, s, st, hue2, lab) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Rc::clone(&suppress),
            Arc::clone(&state),
            hue.clone(),
            wb_lab.clone(),
        );
        temp.connect_change(move |v| {
            if s.get() {
                return;
            }
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.wb_offset = v as f32;
            lab.set_text(&format!("{:+.2} · {:+.2}", adj.wb_offset, hue2.value()));
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });

        // Hue slider.
        let (a, o, s, st, temp2, lab) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Rc::clone(&suppress),
            Arc::clone(&state),
            temp.clone(),
            wb_lab,
        );
        hue.connect_change(move |v| {
            if s.get() {
                return;
            }
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.hue = v as f32;
            lab.set_text(&format!("{:+.2} · {:+.2}", temp2.value(), adj.hue));
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });
    }

    /// Wire the crop-selection overlay: a draw func that renders the selection
    /// (dim-outside + accent border + 8 handles) and a drag gesture that moves /
    /// resizes it. Dragging updates the crop cell + redraw + output line live,
    /// and commits the crop to the photo's adjustments on release (photoup
    /// `commitCrop` — the preview itself is NOT re-rendered on crop).
    fn wire_crop_overlay(&self) {
        let crop_draw = Rc::clone(&self.crop);
        let full_draw = Rc::clone(&self.full_size);
        let area = self.crop_area.clone();
        area.set_draw_func(move |_a, cr, width, height| {
            // Crop box is visible from the start: with no crop set it spans the
            // whole image (photoup shows the full-frame selection immediately, so
            // you can drag/resize without clicking a preset first).
            let c = match *crop_draw.borrow() {
                Some(c) => c,
                None => NormalizedCrop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 },
            };
            let Some(full) = full_draw.get() else { return };
            draw_crop_overlay(cr, width as f64, height as f64, full, c);
        });

        let gesture = gtk4::GestureDrag::new();
        let drag: Rc<RefCell<Option<DragState>>> = Rc::new(RefCell::new(None));

        // Press: pick a handle (resize) or the rect interior (move).
        let crop_begin = Rc::clone(&self.crop);
        let full_begin = Rc::clone(&self.full_size);
        let area_begin = self.crop_area.clone();
        let drag_begin = Rc::clone(&drag);
        gesture.connect_drag_begin(move |_g, x, y| {
            // The box is interactive from the start: with no crop set it's the full
            // frame ({0,0,1,1}) — press/drag/resize works without pressing a preset.
            let c = match *crop_begin.borrow() {
                Some(c) => c,
                None => NormalizedCrop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 },
            };
            let Some(full) = full_begin.get() else { return };
            let (aw, ah) = (area_begin.width() as f64, area_begin.height() as f64);
            let Some(p) = project(aw, ah, full) else { return };
            let (rx, ry, rw, rh) = crop_rect(&p, &c);

            let hit2 = HANDLE_HIT * HANDLE_HIT;
            let mut best: Option<(Handle, f64)> = None;
            for (handle, hx, hy) in handle_anchors(rx, ry, rw, rh) {
                let dx = x - hx;
                let dy = y - hy;
                let d2 = dx * dx + dy * dy;
                if d2 <= hit2 && best.map_or(true, |(_, bd)| d2 < bd) {
                    best = Some((handle, d2));
                }
            }
            let kind = match best {
                Some((handle, _)) => DragKind::Resize { handle },
                None => match edge_handle_at(x, y, rx, ry, rw, rh) {
                    Some(handle) => DragKind::Resize { handle },
                    None if x >= rx && x <= rx + rw && y >= ry && y <= ry + rh => DragKind::Move,
                    _ => return,
                },
            };
            *drag_begin.borrow_mut() = Some(DragState {
                kind,
                start_crop: c,
                disp_w: p.disp_w,
                disp_h: p.disp_h,
            });
        });

        // Drag: apply move/resize (Shift keeps ratio), live redraw + output line.
        let crop_update = Rc::clone(&self.crop);
        let full_update = Rc::clone(&self.full_size);
        let area_update = self.crop_area.clone();
        let drag_update = Rc::clone(&drag);
        let info2_update = self.info2.clone();
        gesture.connect_drag_update(move |gesture, x, y| {
            let Some(d) = *drag_update.borrow() else { return };
            let ndx = (x / d.disp_w) as f32;
            let ndy = (y / d.disp_h) as f32;
            let state = gesture.current_event_state();
            let shift = state.contains(gdk4::ModifierType::SHIFT_MASK);
            let alt = state.contains(gdk4::ModifierType::ALT_MASK);
            let new = match d.kind {
                DragKind::Move => move_crop(d.start_crop, ndx, ndy),
                DragKind::Resize { handle } => match (alt, shift) {
                    (true, true) => resize_crop_shift_center(d.start_crop, handle, ndx, ndy),
                    (true, false) => resize_crop_center(d.start_crop, handle, ndx, ndy),
                    (false, true) => resize_crop_shift(d.start_crop, handle, ndx, ndy),
                    (false, false) => resize_crop(d.start_crop, handle, ndx, ndy),
                },
            };
            *crop_update.borrow_mut() = Some(new);
            area_update.queue_draw();
            if let Some(full) = full_update.get() {
                info2_update.set_text(&output_line(full, Some(new)));
            }
        });

        // Release: commit the crop onto the photo's adjustments (keeps exposure/WB).
        let crop_end = Rc::clone(&self.crop);
        let drag_end = Rc::clone(&drag);
        let id_end = Rc::clone(&self.active_id);
        let state_end = Arc::clone(&self.state);
        let on_end = Arc::clone(&self.on_event);
        gesture.connect_drag_end(move |_g, _x, _y| {
            if drag_end.borrow().is_none() {
                return;
            }
            let new_crop = *crop_end.borrow();
            *drag_end.borrow_mut() = None;
            let Some(c) = new_crop else { return };
            let Some(id) = id_end.get() else { return };
            let mut adj = current_adjustments(&state_end.read().unwrap(), id);
            adj.crop = Some(c);
            on_end(AppEvent::PhotoEdit { id, adjustments: adj });
        });

        self.crop_area.add_controller(gesture);

        // Neutral-pick on click (photoup `pickNeutral`): clicking the preview
        // samples a 7×7 area under the cursor and maps it to warmth + hue. This is
        // the DEFAULT click action — no Pick button/mode. Clicks on the crop box's
        // 8 handles stay crop grabs (the drag gesture resizes them), and any
        // press-drag (crop move/resize) suppresses the click: we listen on
        // `released`, which GTK cancels once the drag gesture claims the sequence.
        // The click lands on the crop overlay (it fills the preview area), whose
        // coordinates map to the letterboxed image via `project` — the same rect
        // the crop overlay draws with.
        let click = gtk4::GestureClick::new();
        let crop_click = Rc::clone(&self.crop);
        let wb_click = Rc::clone(&self.wb_sample);
        let cam_click = Rc::clone(&self.cam_matrix);
        let full_click = Rc::clone(&self.full_size);
        let id_click = Rc::clone(&self.active_id);
        let state_click = Arc::clone(&self.state);
        let on_click = Arc::clone(&self.on_event);
        let area_click = self.crop_area.clone();
        let temp_click = self.temp_slider.clone();
        let hue_click = self.hue_slider.clone();
        let suppress_click = Rc::clone(&self.suppress);
        let wb_lab_click = self.wb_value.clone();
        click.connect_released(move |_g, _n_press, x, y| {
            let Some(id) = id_click.get() else { return };
            let Some(full) = full_click.get() else { return };
            let Some((data, pw, ph)) = &*wb_click.borrow() else { return };
            let (aw, ah) = (area_click.width() as f64, area_click.height() as f64);
            let Some(p) = project(aw, ah, full) else { return };
            // A click on a crop handle is a resize grab, not a pick — the drag
            // gesture owns the handles (26px hit radius, same as drag-begin).
            let c = match *crop_click.borrow() {
                Some(c) => c,
                None => NormalizedCrop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 },
            };
            let (rx, ry, rw, rh) = crop_rect(&p, &c);
            let hit2 = HANDLE_HIT * HANDLE_HIT;
            let near_handle = handle_anchors(rx, ry, rw, rh)
                .iter()
                .any(|(_h, hx, hy)| (x - hx) * (x - hx) + (y - hy) * (y - hy) <= hit2);
            // A press on the crop border is a resize grab, not a pick — keep the
            // click and drag gestures in agreement about what is "the handle".
            if near_handle || edge_handle_at(x, y, rx, ry, rw, rh).is_some() {
                return;
            }
            // Normalized position over the displayed (letterboxed) image.
            let nx = (x - p.ox) / p.disp_w;
            let ny = (y - p.oy) / p.disp_h;
            if !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny) {
                return; // clicked in the letterbox
            }
            let cx = nx * *pw as f64;
            let cy = ny * *ph as f64;
            let Some(((r, g, b), win)) = pick_sample_linear(data, *pw, *ph, cx, cy) else { return };
            let gray = (r + g + b) / 3.0;
            if gray < 0.03 || gray > 0.95 {
                on_click(AppEvent::Toast(
                    "Pick a neutral area (not black or blown out)".to_string(),
                ));
                return;
            }
            let cam = cam_matrix3x3(*cam_click.borrow());
            let (x0, y0, x1, y1) = pick_window_sized(*pw, *ph, cx, cy, win);
            log_wb_sample_stats("pick", data, *pw, *ph, x0, y0, x1, y1);
            let (offset, hue) = wb_from_pick(r, g, b, cam);
            log::info!(
                "[wb] pick feed r={r:.4} g={g:.4} b={b:.4} → offset={offset:.3} hue={hue:.3} cam={}",
                if cam.is_some() { "matrix" } else { "grey-world" }
            );
            apply_wb(id, offset, hue, &state_click, &on_click, &suppress_click, &temp_click, &hue_click, &wb_lab_click);
        });
        self.crop_area.add_controller(click);
    }

    fn wire_buttons(&self) {
        let on_event = Arc::clone(&self.on_event);
        let active_id = Rc::clone(&self.active_id);
        let state = Arc::clone(&self.state);
        let suppress = Rc::clone(&self.suppress);
        let ev = self.exposure_slider.clone();
        let temp = self.temp_slider.clone();
        let hue = self.hue_slider.clone();
        let full_size = Rc::clone(&self.full_size);
        let ev_lab = self.ev_value.clone();
        let wb_lab = self.wb_value.clone();
        let info2 = self.info2.clone();

        // Exposure: Auto / Burn. Both switch to an auto mode whose EV is the
        // render's auto-EV — show a placeholder until the re-render lands with the
        // real value (the controller's `set_ev` then updates the label).
        let (a, o, st, lab) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
            ev_lab.clone(),
        );
        self.auto_exposure_btn.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.exposure_mode = ExposureMode::Auto;
            lab.set_text("+?.?? EV");
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });
        let (a, o, st, lab) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
            ev_lab.clone(),
        );
        self.burn_exposure_btn.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.exposure_mode = ExposureMode::Burn;
            lab.set_text("+?.?? EV");
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });

        // Rest: manual EV = 0, snap the slider.
        let (a, o, st, s, ev, lab) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
            Rc::clone(&suppress),
            ev.clone(),
            ev_lab.clone(),
        );
        self.rest_exposure_btn.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.exposure_mode = ExposureMode::Manual;
            adj.exposure_ev = 0.0;
            s.set(true);
            ev.set_value(0.0);
            s.set(false);
            lab.set_text("+0.00 EV");
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });

        // WB Reset: warmth + hue back to neutral, snap both sliders.
        let (a, o, st, s, temp, hue, lab) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
            Rc::clone(&suppress),
            temp.clone(),
            hue.clone(),
            wb_lab.clone(),
        );
        self.reset_wb_btn.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.wb_offset = 0.0;
            adj.hue = 0.0;
            s.set(true);
            temp.set_value(0.0);
            hue.set_value(0.0);
            s.set(false);
            lab.set_text("+0.00 · +0.00");
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });

        // WB Auto (photoup `autoWhiteBalance`): the reference is a grey-world mean
        // over near-neutral bright pixels of the LINEAR pre-tone sample, falling
        // back to the whole-region mean if too few neutral pixels. Two flavours:
        //   Auto  → clinical neutralization (the full `wb_from_pick` fit).
        //   Auto2 → warm: the same reference, but only ~60% corrected + a warm bias,
        //            keeping the ambience (Nikon AUTO2 "keep warm lighting colors").
        let (a, o, st, wbs, cm, s, temp, hue, lab, croprc) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
            Rc::clone(&self.wb_sample),
            Rc::clone(&self.cam_matrix),
            Rc::clone(&suppress),
            // WB Reset's `let` shadowed + moved `temp`/`hue`, so clone fresh here.
            self.temp_slider.clone(),
            self.hue_slider.clone(),
            wb_lab.clone(),
            Rc::clone(&self.crop),
        );
        let (a2, o2, st2, wbs2, cm2, s2, temp2, hue2, lab2, croprc2) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
            Rc::clone(&self.wb_sample),
            Rc::clone(&self.cam_matrix),
            Rc::clone(&suppress),
            self.temp_slider.clone(),
            self.hue_slider.clone(),
            wb_lab.clone(),
            Rc::clone(&self.crop),
        );
        self.wb_auto_button.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let Some((data, pw, ph)) = &*wbs.borrow() else { return };
            let crop = *croprc.borrow();
            let Some((r, g, b)) = auto_wb_feed(data, *pw, *ph, crop.as_ref()) else { return };
            let cam = cam_matrix3x3(*cm.borrow());
            let (wb_offset, wb_hue) = wb_from_pick(r, g, b, cam); // clinical
            log::info!(
                "[wb] auto (clinical) feed r={r:.4} g={g:.4} b={b:.4} → offset={wb_offset:.3} hue={wb_hue:.3} cam={}",
                if cam.is_some() { "matrix" } else { "grey-world" }
            );
            apply_wb(id, wb_offset, wb_hue, &st, &o, &s, &temp, &hue, &lab);
        });
        self.wb_auto2_button.connect_clicked(move |_| {
            let Some(id) = a2.get() else { return };
            let Some((data, pw, ph)) = &*wbs2.borrow() else { return };
            let crop = *croprc2.borrow();
            let Some((r, g, b)) = auto_wb_feed(data, *pw, *ph, crop.as_ref()) else { return };
            let cam = cam_matrix3x3(*cm2.borrow());
            let (wb_offset, wb_hue) = auto_wb(r, g, b, cam); // warm (Nikon AUTO2)
            log::info!(
                "[wb] auto2 (warm) feed r={r:.4} g={g:.4} b={b:.4} → offset={wb_offset:.3} hue={wb_hue:.3} cam={}",
                if cam.is_some() { "matrix" } else { "grey-world" }
            );
            apply_wb(id, wb_offset, wb_hue, &st2, &o2, &s2, &temp2, &hue2, &lab2);
        });

        // Crop presets (each also mirrors the selection into the overlay).
        self.crop_11.connect_clicked(crop_preset_handler(
            1.0,
            Rc::clone(&active_id),
            Arc::clone(&state),
            Arc::clone(&on_event),
            Rc::clone(&full_size),
            Rc::clone(&self.crop),
            self.crop_area.clone(),
            info2.clone(),
        ));
        self.crop_23.connect_clicked(crop_preset_handler(
            2.0 / 3.0,
            Rc::clone(&active_id),
            Arc::clone(&state),
            Arc::clone(&on_event),
            Rc::clone(&full_size),
            Rc::clone(&self.crop),
            self.crop_area.clone(),
            info2.clone(),
        ));
        self.crop_32.connect_clicked(crop_preset_handler(
            3.0 / 2.0,
            Rc::clone(&active_id),
            Arc::clone(&state),
            Arc::clone(&on_event),
            Rc::clone(&full_size),
            Rc::clone(&self.crop),
            self.crop_area.clone(),
            info2.clone(),
        ));

        // Original: no crop → no selection box drawn.
        let (a, o, st, full, crop_cell, area, i2) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
            Rc::clone(&full_size),
            Rc::clone(&self.crop),
            self.crop_area.clone(),
            info2,
        );
        self.crop_orig.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.crop = None;
            *crop_cell.borrow_mut() = None;
            area.queue_draw();
            if let Some(full) = full.get() {
                i2.set_text(&output_line(full, None));
            }
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });

        // Rotate ↺ / ↻ (user rotation on top of EXIF/libraw orientation).
        self.rotate_ccw.connect_clicked(rotate_handler(
            3,
            Rc::clone(&active_id),
            Arc::clone(&state),
            Arc::clone(&on_event),
            Rc::clone(&full_size),
            self.crop_area.clone(),
            self.info1.clone(),
            self.info2.clone(),
            Rc::clone(&self.is_raw),
        ));
        self.rotate_cw.connect_clicked(rotate_handler(
            1,
            Rc::clone(&active_id),
            Arc::clone(&state),
            Arc::clone(&on_event),
            Rc::clone(&full_size),
            self.crop_area.clone(),
            self.info1.clone(),
            self.info2.clone(),
            Rc::clone(&self.is_raw),
        ));

        // Nav.
        let (o1, o2) = (Arc::clone(&on_event), Arc::clone(&on_event));
        self.nav_prev
            .connect_clicked(move |_| o1(AppEvent::Nav { delta: -1 }));
        self.nav_next
            .connect_clicked(move |_| o2(AppEvent::Nav { delta: 1 }));

        // Reject / Close.
        let (o3, o4) = (Arc::clone(&on_event), Arc::clone(&on_event));
        self.reject_button
            .connect_clicked(move |_| o3(AppEvent::RejectActive));
        self.close_button
            .connect_clicked(move |_| o4(AppEvent::ActivePhoto { index: None }));
    }
}

fn section_label(text: &str) -> Label {
    let l = Label::new(Some(text));
    l.add_css_class("editor-section");
    l.set_halign(gtk4::Align::Start);
    l
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn linear_fill(w: u32, h: u32, rgb: [f32; 3]) -> Vec<f32> {
        let mut v = vec![0f32; (w * h * 3) as usize];
        for px in v.chunks_exact_mut(3) {
            px[0] = rgb[0];
            px[1] = rgb[1];
            px[2] = rgb[2];
        }
        v
    }

    /// A test editor wired to a recorder that captures emitted AppEvents.
    fn test_editor() -> (EditorScreen, Arc<Mutex<Vec<AppEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let on_event: Arc<dyn Fn(AppEvent) + Send + Sync + 'static> = {
            let events = Arc::clone(&events);
            Arc::new(move |ev| events.lock().unwrap().push(ev))
        };
        let state = Arc::new(RwLock::new(AppState::default()));
        let editor = EditorScreen::new(state, on_event);
        (editor, events)
    }

    #[test]
    fn wb_mean_helpers_sample() {
        // Neutral gray (linear): all pixels qualify as near-neutral → mean exactly gray.
        let gray = linear_fill(16, 16, [0.2159, 0.2159, 0.2159]);
        let (r, g, b) = auto_wb_mean_linear(&gray, 16, 16, None).expect("gray sample");
        assert!(
            (r - 0.2159).abs() < 1e-4 && (g - 0.2159).abs() < 1e-4 && (b - 0.2159).abs() < 1e-4
        );

        // Saturated warm: max-min = 0.498 > 0.2 → no neutral pixels → whole-region mean.
        let warm = linear_fill(16, 16, [0.578, 0.216, 0.080]);
        let (r, g, b) = auto_wb_mean_linear(&warm, 16, 16, None).expect("warm sample");
        assert!(
            (r - 0.578).abs() < 1e-4 && (g - 0.216).abs() < 1e-4 && (b - 0.080).abs() < 1e-4
        );

        // Pick: window around the center of a uniform warm block — no noise, so
        // the 7×7 window is kept and the mean is the fill colour exactly.
        let ((r, g, b), win) = pick_sample_linear(&warm, 16, 16, 8.0, 8.0).expect("pick sample");
        assert!(
            (r - 0.578).abs() < 1e-4 && (g - 0.216).abs() < 1e-4 && (b - 0.080).abs() < 1e-4
        );
        assert_eq!(win, 7, "uniform fill -> no noise growth");
    }

    #[test]
    fn pick_grows_window_on_noisy_region() {
        // A noisy (high-variance) neutral block: the pick must widen its window
        // past 7×7 so the mean converges (noise cancels as 1/sqrt(N)) and the WB
        // doesn't jitter between clicks. Base neutral 0.2159 linear, ±0.1 noise.
        let mut buf = vec![0f32; 16 * 16 * 3];
        let mut x = 123u32; // simple LCG for deterministic pseudo-noise
        for px in buf.chunks_exact_mut(3) {
            for c in 0..3 {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = ((x >> 16) % 20_001) as f32 / 100_000.0 - 0.1; // ±0.1
                px[c] = (0.2159 + n).clamp(0.0, 1.0);
            }
        }
        let ((r, g, b), win) = pick_sample_linear(&buf, 16, 16, 8.0, 8.0).expect("noisy pick");
        // Window grew well past 7×7…
        assert!(win >= 19, "noisy region should grow the window, got {win}");
        // …and the mean stays close to the true neutral (noise averaged out).
        assert!(
            (r - 0.2159).abs() < 0.02 && (g - 0.2159).abs() < 0.02 && (b - 0.2159).abs() < 0.02,
            "mean ({r:.4},{g:.4},{b:.4}) drifted from 0.2159"
        );
    }

    #[test]
    fn editor_widgets_work() {
        // One #[test] initializes GTK on a single thread — the parallel test
        // harness would panic gtk4::init() from a second thread.
        if gtk4::init().is_err() {
            eprintln!("skipping: no display");
            return;
        }

        // EV indicator: formatted + fixed-width label.
        let (mut editor, _events) = test_editor();
        editor.set_photo(1, "test.jpg", &Adjustments::default(), 0.0, false, Some((800, 600)));
        editor.set_ev(1.5);
        assert_eq!(editor.ev_value.text(), "+1.50 EV");
        editor.set_ev(-0.35);
        assert_eq!(editor.ev_value.text(), "-0.35 EV");
        editor.set_ev(4.0);
        assert_eq!(editor.ev_value.text(), "+4.00 EV");

        // AUTO (clinical) on a neutral gray image (linear 0.2159) → no change;
        // AUTO2 (warm) → the fixed +0.15 warm bias, no tint.
        let (mut editor, events) = test_editor();
        editor.set_photo(7, "gray.jpg", &Adjustments::default(), 0.0, false, Some((16, 16)));
        editor.set_wb_sample(linear_fill(16, 16, [0.2159, 0.2159, 0.2159]), 16, 16);
        editor.wb_auto_button.emit_clicked();
        let adj = find_photo_edit(&events, 7).expect("neutral PhotoEdit (Auto)");
        assert!((adj.wb_offset - 0.0).abs() < 1e-4, "offset {}", adj.wb_offset);
        assert!((adj.hue - 0.0).abs() < 1e-4, "hue {}", adj.hue);
        editor.wb_auto2_button.emit_clicked();
        let adj = find_photo_edit(&events, 7).expect("neutral PhotoEdit (Auto2)");
        assert!((adj.wb_offset - 0.15).abs() < 1e-4, "offset {}", adj.wb_offset);
        assert!((adj.hue - 0.0).abs() < 1e-4, "hue {}", adj.hue);

        // AUTO (clinical) on a warm image → full neutralization: the exact pick fit
        // on (0.578,0.216,0.080) is offset ≈ −2.853, hue ≈ +0.009.
        let (mut editor, events) = test_editor();
        editor.set_photo(8, "warm.jpg", &Adjustments::default(), 0.0, false, Some((16, 16)));
        editor.set_wb_sample(linear_fill(16, 16, [0.578, 0.216, 0.080]), 16, 16);
        editor.wb_auto_button.emit_clicked();
        let adj = find_photo_edit(&events, 8).expect("warm PhotoEdit (Auto)");
        assert!(adj.wb_offset < -0.3, "offset {}", adj.wb_offset);
        assert!((adj.wb_offset - -2.852998).abs() < 1e-3, "offset {}", adj.wb_offset);
        assert!((adj.hue - 0.008614).abs() < 1e-3, "hue {}", adj.hue);
        // AUTO2 (warm) → 60% of the correction + 0.15 warm bias (offset ≈ −1.562),
        // keeping the ambience like Nikon AUTO2 "keep warm lighting colors".
        editor.wb_auto2_button.emit_clicked();
        let adj = find_photo_edit(&events, 8).expect("warm PhotoEdit (Auto2)");
        assert!((adj.wb_offset - -1.561799).abs() < 1e-3, "offset {}", adj.wb_offset);
        assert!((adj.hue - 0.002584).abs() < 1e-3, "hue {}", adj.hue);

        // No wb sample → the handler must not emit anything or crash.
        let (mut editor, events) = test_editor();
        editor.set_photo(9, "nopreview.jpg", &Adjustments::default(), 0.0, false, Some((16, 16)));
        editor.wb_auto_button.emit_clicked();
        assert!(
            !events.lock().unwrap().iter().any(|e| matches!(e, AppEvent::PhotoEdit { .. })),
            "must not emit PhotoEdit without a wb sample"
        );
    }

    /// The most recent `PhotoEdit` for `id` recorded by a test editor.
    fn find_photo_edit(events: &Arc<Mutex<Vec<AppEvent>>>, id: u64) -> Option<Adjustments> {
        events.lock().unwrap().iter().rev().find_map(|e| match e {
            AppEvent::PhotoEdit { id: i, adjustments } if *i == id => Some(*adjustments),
            _ => None,
        })
    }

    #[test]
    fn fine_step_snaps_auto_ev_to_grid() {
        // Auto EV +1.93: first press snaps to the 0.05 grid, not relative −0.05.
        assert!((fine_step(1.93, -1, 0.05) - 1.90).abs() < 1e-9);
        assert!((fine_step(1.93, 1, 0.05) - 1.95).abs() < 1e-9);
        // Once on the grid, step by 0.05.
        assert!((fine_step(1.90, -1, 0.05) - 1.85).abs() < 1e-9);
        assert!((fine_step(1.90, 1, 0.05) - 1.95).abs() < 1e-9);
        // Negative values snap correctly (floor/ceil, not truncate-toward-zero).
        assert!((fine_step(-1.93, 1, 0.05) - (-1.90)).abs() < 1e-9);
        assert!((fine_step(-1.93, -1, 0.05) - (-1.95)).abs() < 1e-9);
    }

    #[test]
    fn fine_step_tint_always_fine() {
        // Tint grid is 0.01, so every value is on-grid → plain 0.01 steps.
        assert!((fine_step(0.50, -1, 0.01) - 0.49).abs() < 1e-9);
        assert!((fine_step(0.50, 1, 0.01) - 0.51).abs() < 1e-9);
        assert!((fine_step(-0.05, 1, 0.01) - (-0.04)).abs() < 1e-9);
        assert!((fine_step(-0.05, -1, 0.01) - (-0.06)).abs() < 1e-9);
    }

    #[test]
    fn resize_center_keeps_center() {
        // A centered crop {0.3,0.3,0.4,0.4} has center (0.5,0.5).
        let c = NormalizedCrop { x: 0.3, y: 0.3, width: 0.4, height: 0.4 };
        // SE grow: width+0.1, height+0.1; both edges move so the center stays.
        let r = resize_crop_center(c, Handle::Se, 0.1, 0.1);
        assert!((r.x + r.width / 2.0 - 0.5).abs() < 1e-6, "x-center {}", r.x + r.width / 2.0);
        assert!((r.y + r.height / 2.0 - 0.5).abs() < 1e-6, "y-center {}", r.y + r.height / 2.0);
        assert!((r.width - 0.5).abs() < 1e-6 && (r.height - 0.5).abs() < 1e-6);
        // W shrink: dragging the left edge right shrinks width around the center.
        let r2 = resize_crop_center(c, Handle::W, 0.1, 0.0);
        assert!((r2.x + r2.width / 2.0 - 0.5).abs() < 1e-6);
        assert!((r2.width - 0.3).abs() < 1e-6);
        // The center never escapes the frame: growing far past one edge clamps
        // the half-extent to that edge (2*cx = 0.4 here), leaving x >= 0.
        let near_left = NormalizedCrop { x: 0.1, y: 0.1, width: 0.2, height: 0.2 };
        let r3 = resize_crop_center(near_left, Handle::E, 0.5, 0.0);
        assert!(r3.x >= -1e-6, "x {:.4}", r3.x);
        assert!((r3.width - 0.4).abs() < 1e-6, "clamped to 2*cx=0.4, got {}", r3.width);
    }

    #[test]
    fn resize_shift_center_keeps_center_and_ratio() {
        let c = NormalizedCrop { x: 0.3, y: 0.2, width: 0.4, height: 0.3 }; // ratio 4:3
        // SE grow driven by the dominant axis (E: sx=0.25 > sy=0.1667).
        let r = resize_crop_shift_center(c, Handle::Se, 0.1, 0.05);
        assert!((r.x + r.width / 2.0 - 0.5).abs() < 1e-6, "cx {}", r.x + r.width / 2.0);
        assert!((r.y + r.height / 2.0 - 0.35).abs() < 1e-6, "cy {}", r.y + r.height / 2.0);
        assert!(
            (r.width / r.height - c.width / c.height).abs() < 1e-6,
            "ratio {}",
            r.width / r.height
        );
        assert!(r.width > c.width, "grew: {} vs {}", r.width, c.width);
        // Side drag keeps ratio and center too.
        let r2 = resize_crop_shift_center(c, Handle::E, 0.1, 0.0);
        assert!((r2.x + r2.width / 2.0 - 0.5).abs() < 1e-6);
        assert!((r2.width / r2.height - c.width / c.height).abs() < 1e-6);
        // Shrink (corner dragged inward) shrinks both axes, center still fixed.
        let r3 = resize_crop_shift_center(c, Handle::Se, -0.1, -0.05);
        assert!(r3.width < c.width && r3.height < c.height);
        assert!((r3.x + r3.width / 2.0 - 0.5).abs() < 1e-6);
    }

    /// Regression for the writer-side panic: a crop shunted flush against an edge
    /// (`x: 0.95, width: 0.05`) has center cx = 0.975000006, whose f32
    /// Border presses resolve to the right resize handle: any point within
    /// `EDGE_HIT` px of an edge grabs that edge, corners win over edges, and the
    /// interior / far-outside presses are not grabs. This is the second pass that
    /// makes the whole crop-box border grabbable (not just the 8 handle squares).
    #[test]
    fn edge_handle_at_resolves_border_grabs() {
        let (rx, ry, rw, rh) = (100.0, 100.0, 400.0, 300.0); // 100,100 → 500,400
        // On each edge midpoint — must grab that edge.
        assert_eq!(edge_handle_at(300.0, 100.0, rx, ry, rw, rh), Some(Handle::N));
        assert_eq!(edge_handle_at(300.0, 400.0, rx, ry, rw, rh), Some(Handle::S));
        assert_eq!(edge_handle_at(100.0, 250.0, rx, ry, rw, rh), Some(Handle::W));
        assert_eq!(edge_handle_at(500.0, 250.0, rx, ry, rw, rh), Some(Handle::E));
        // Corners — both bordering edges resolve to the corner handle.
        assert_eq!(edge_handle_at(95.0, 95.0, rx, ry, rw, rh), Some(Handle::Nw));
        assert_eq!(edge_handle_at(505.0, 95.0, rx, ry, rw, rh), Some(Handle::Ne));
        assert_eq!(edge_handle_at(95.0, 405.0, rx, ry, rw, rh), Some(Handle::Sw));
        assert_eq!(edge_handle_at(505.0, 405.0, rx, ry, rw, rh), Some(Handle::Se));
        // A few px inside an edge still grabs it (the tolerance band).
        assert_eq!(edge_handle_at(300.0, 107.0, rx, ry, rw, rh), Some(Handle::N));
        // Deep inside the box or far outside → not a border grab.
        assert_eq!(edge_handle_at(300.0, 250.0, rx, ry, rw, rh), None);
        assert_eq!(edge_handle_at(300.0, 140.0, rx, ry, rw, rh), None);
        assert_eq!(edge_handle_at(20.0, 250.0, rx, ry, rw, rh), None);
        assert_eq!(edge_handle_at(300.0, 600.0, rx, ry, rw, rh), None);
    }

    /// An edge grab that hugs the bottom edge must still clamp in-frame (the
    /// `clamp_crop` net from the panic fix applies to edge-grab drags too).
    #[test]
    fn edge_grab_at_bottom_edge_stays_in_frame() {
        let mut c = NormalizedCrop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };
        // Simulate grabbing the bottom edge and dragging it down (ndy > 0 pushes
        // the rect past the frame; the writer must clamp it back).
        c = resize_crop(c, Handle::S, 0.0, 0.25);
        assert!(c.y + c.height <= 1.0 + 1e-6 && c.height >= MIN_CROP, "{c:?}");
        // Grabbing the top edge and dragging it up.
        c = NormalizedCrop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };
        c = resize_crop(c, Handle::N, 0.0, 0.3);
        assert!(c.y >= -1e-6 && c.height >= MIN_CROP, "{c:?}");
    }

    /// Alt+Shift on a SIDE handle must resize in both directions, not just corners.
    /// Before the fix `s = 1.0 + sx.max(sy)` left the side's zero second axis in the
    /// max, so an inward side drag clamped to `s = 1.0` — sides could grow but never
    /// shrink (only corners, whose second axis is non-zero, could shrink).
    #[test]
    fn shift_center_side_handles_resize_both_directions() {
        let c = NormalizedCrop { x: 0.25, y: 0.25, width: 0.5, height: 0.5 };
        // Right edge outward → grows, center fixed, ratio kept.
        let grown = resize_crop_shift_center(c, Handle::E, 0.1, 0.0);
        assert!(grown.width > c.width, "right edge outward grows: {grown:?}");
        assert!((grown.height / grown.width - 1.0).abs() < 1e-4, "ratio kept: {grown:?}");
        assert!(
            (grown.x + grown.width / 2.0 - 0.5).abs() < 1e-4,
            "center x fixed: {grown:?}"
        );
        // Right edge inward → shrinks (was a no-op before the fix).
        let shrunk = resize_crop_shift_center(c, Handle::E, -0.1, 0.0);
        assert!(shrunk.width < c.width, "right edge inward shrinks: {shrunk:?}");
        // Top edge inward (drag down) → shrinks too.
        let n_shrunk = resize_crop_shift_center(c, Handle::N, 0.0, 0.1);
        assert!(n_shrunk.height < c.height, "top edge inward shrinks: {n_shrunk:?}");
    }

    /// `2·(1−cx)` bound dips a hair under MIN_CROP → `.clamp(MIN_CROP, bound)`
    /// used to panic. The bound is now floored at MIN_CROP.
    #[test]
    fn center_resize_at_edge_never_panics() {
        // The exact failing input from the fuzz: right-edge crop, Ne grow, Alt.
        let c = NormalizedCrop { x: 0.95, y: 0.0, width: 0.050000012, height: 0.82297975 };
        let r = resize_crop_center(c, Handle::Ne, 0.529804707, 0.043798685);
        assert!(r.x + r.width <= 1.0 + 1e-6 && r.width >= MIN_CROP, "in-frame: {r:?}");
        // Same shape via the shift-center and plain-shift paths.
        let r2 = resize_crop_shift_center(c, Handle::Ne, 0.5, 0.0);
        assert!(r2.x + r2.width <= 1.0 + 1e-6 && r2.width >= MIN_CROP, "in-frame: {r2:?}");
        let r3 = resize_crop_shift(c, Handle::Ne, 0.5, 0.0);
        assert!(r3.x + r3.width <= 1.0 + 1e-6 && r3.width >= MIN_CROP, "in-frame: {r3:?}");
    }

    /// Fuzz: a chain of random drags (like a real user edit session) must never
    /// make a writer panic nor leave a crop that overhangs the image. This is the
    /// regression net for the "one photo stuck" worker panic: before the
    /// writers clamped their output, an edge-hugging crop could produce a pixel
    /// rect past the image edge and the render panicked.
    #[test]
    fn fuzz_drag_chain_keeps_rect_in_bounds() {
        use crate::image::math::crop_to_pixels;
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed as f64 / u64::MAX as f64) as f32
        };
        let (w, h) = (3680u32, 2456u32);
        let mut c = NormalizedCrop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };
        for step in 0..200_000 {
            let r = crop_to_pixels(&c, w, h);
            assert!(
                r.x + r.width <= w && r.y + r.height <= h,
                "step {step}: crop {:?} → rect {r:?} overhangs {w}x{h}",
                c
            );
            // random drag
            let handle = match (rnd() * 8.0) as usize {
                0 => Handle::N, 1 => Handle::S, 2 => Handle::E, 3 => Handle::W,
                4 => Handle::Ne, 5 => Handle::Nw, 6 => Handle::Se, _ => Handle::Sw,
            };
            let alt = rnd() < 0.3;
            let shift = rnd() < 0.3;
            let ndx = (rnd() - 0.5) * 2.0; // -1..1
            let ndy = (rnd() - 0.5) * 2.0;
            let do_move = rnd() < 0.15;
            let res = std::panic::catch_unwind(|| {
                if do_move {
                    move_crop(c, ndx, ndy)
                } else {
                    match (alt, shift) {
                        (true, true) => resize_crop_shift_center(c, handle, ndx, ndy),
                        (true, false) => resize_crop_center(c, handle, ndx, ndy),
                        (false, true) => resize_crop_shift(c, handle, ndx, ndy),
                        (false, false) => resize_crop(c, handle, ndx, ndy),
                    }
                }
            });
            match res {
                Ok(c2) => c = c2,
                Err(_) => panic!(
                    "writer panicked at step {step}: c={c:?} handle={handle:?} alt={alt} \
                     shift={shift} ndx={ndx:.9} ndy={ndy:.9}"
                ),
            }
        }
    }
}
