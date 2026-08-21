//! Fine-grained custom-drawn slider (`FineSlider`) — a `GtkScale` replacement.
//!
//! The stock `GtkScale` has two behaviors that break fine adjustments:
//! 1. drags are continuous, so the pointer never lands on the 0.05 (EV/warmth)
//!    or 0.01 (tint) step grid, and
//! 2. press-and-hold + drag enters a ~2× slower "precision mode" and the value
//!    sticks at 0 (GTK's built-in range-drag behavior can't be disabled).
//!
//! This widget draws its own thin amber track + round warm knob and maps the
//! pointer straight to a step-snapped value, so drags always land on the grid
//! and the value follows the pointer directly — no precision mode, no
//! 0-stickiness.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

/// Darkroom palette (mirrors `src/ui/grid.rs` `install_css()`).
const TRACK: (f64, f64, f64) = (0x3A as f64 / 255.0, 0x32 as f64 / 255.0, 0x2A as f64 / 255.0);
const FILL: (f64, f64, f64) = (0xFF as f64 / 255.0, 0x7A as f64 / 255.0, 0x45 as f64 / 255.0);
const KNOB: (f64, f64, f64) = (0xEC as f64 / 255.0, 0xE4 as f64 / 255.0, 0xD8 as f64 / 255.0);
const KNOB_ACTIVE: (f64, f64, f64) =
    (0xF5 as f64 / 255.0, 0xEF as f64 / 255.0, 0xE4 as f64 / 255.0);
const ZERO: (f64, f64, f64) = (0x8A as f64 / 255.0, 0x7F as f64 / 255.0, 0x72 as f64 / 255.0);

/// Track inset from the widget edges (px).
const PAD: f64 = 10.0;
/// Track thickness (px).
const TRACK_H: f64 = 4.0;
/// Knob radius (px).
const KNOB_R: f64 = 7.0;

/// The change callback (receives the new value).
type ChangeFn = Box<dyn Fn(f64)>;

/// Snap a raw value to the nearest multiple of `step` (float-safe: round to
/// 1e-4 so e.g. 39·0.05 = 1.9500000000000002 renders as 1.95), then clamp to
/// [min, max]. Pure — unit-tested without a GTK display.
pub(crate) fn snap_to_grid(raw: f64, step: f64, min: f64, max: f64) -> f64 {
    let snapped = ((raw / step).round() * step * 10000.0).round() / 10000.0;
    snapped.clamp(min, max)
}

/// A custom-drawn fine slider: thin dark track, amber fill, round warm knob,
/// subtle zero tick, and a step-snapped drag. Dragging follows the pointer
/// directly — there is no GTK Scale precision-mode or 0-stickiness.
#[derive(Clone)]
pub struct FineSlider {
    area: gtk4::DrawingArea,
    value: Rc<Cell<f64>>,
    min: f64,
    max: f64,
    step: f64,
    on_change: Rc<RefCell<Option<ChangeFn>>>,
    dragging: Rc<Cell<bool>>,
}

impl FineSlider {
    pub fn new(min: f64, max: f64, step: f64) -> Self {
        let area = gtk4::DrawingArea::new();
        area.set_height_request(24);
        area.set_hexpand(true);

        let value = Rc::new(Cell::new(min));
        let on_change: Rc<RefCell<Option<ChangeFn>>> = Rc::new(RefCell::new(None));
        let dragging = Rc::new(Cell::new(false));

        // Draw: track + amber fill + round knob + subtle zero tick. The value is
        // read from the shared cell so every `set_value` redraws the slider.
        let draw_value = Rc::clone(&value);
        let draw_dragging = Rc::clone(&dragging);
        let (mn, mx) = (min, max);
        area.set_draw_func(move |_area, cr, width, height| {
            let width = width as f64;
            let height = height as f64;
            let track_len = (width - 2.0 * PAD).max(0.0);
            if track_len <= 0.0 {
                return;
            }
            let ratio = |v: f64| ((v - mn) / (mx - mn)).clamp(0.0, 1.0);
            let x = PAD + ratio(draw_value.get()) * track_len;
            let cy = height / 2.0;

            // Track (dark warm border).
            rounded_rect(cr, PAD, cy - TRACK_H / 2.0, track_len, TRACK_H, TRACK_H / 2.0);
            cr.set_source_rgb(TRACK.0, TRACK.1, TRACK.2);
            let _ = cr.fill();

            // Amber fill from the left edge to the current value.
            let fill_w = (x - PAD).max(0.0);
            if fill_w > 0.0 {
                rounded_rect(cr, PAD, cy - TRACK_H / 2.0, fill_w, TRACK_H, TRACK_H / 2.0);
                cr.set_source_rgb(FILL.0, FILL.1, FILL.2);
                let _ = cr.fill();
            }

            // Subtle zero-correction tick on the track — a reference mark, NOT a
            // snap point (the value passes through it freely).
            let zr = ratio(0.0);
            if (0.0..=1.0).contains(&zr) {
                let zx = PAD + zr * track_len;
                cr.set_source_rgba(ZERO.0, ZERO.1, ZERO.2, 0.85);
                cr.rectangle(zx - 0.5, cy - TRACK_H / 2.0 - 4.0, 1.0, TRACK_H + 8.0);
                let _ = cr.fill();
            }

            // Round knob at the value position (brightens while dragging).
            let (k0, k1, k2) = if draw_dragging.get() { KNOB_ACTIVE } else { KNOB };
            cr.set_source_rgb(k0, k1, k2);
            cr.arc(x, cy, KNOB_R, 0.0, 2.0 * std::f64::consts::PI);
            let _ = cr.fill();
        });

        let slider = Self {
            area,
            value,
            min,
            max,
            step,
            on_change,
            dragging,
        };
        slider.wire_drag();
        slider
    }

    /// The `DrawingArea` to append to a panel.
    pub fn area(&self) -> gtk4::DrawingArea {
        self.area.clone()
    }

    /// Clamp `v` to [min, max], store it, redraw, and (if the value actually
    /// changed) fire the change callback. Programmatic sets are NOT snapped — an
    /// auto EV like +1.93 shows as-is; only user drags snap to the grid.
    pub fn set_value(&self, v: f64) {
        let v = v.clamp(self.min, self.max);
        let old = self.value.get();
        if (v - old).abs() > 1e-9 {
            self.value.set(v);
            self.area.queue_draw();
            if let Some(f) = self.on_change.borrow().as_ref() {
                f(v);
            }
        }
    }

    pub fn value(&self) -> f64 {
        self.value.get()
    }

    /// Register the change callback (fired on every effective `set_value`,
    /// including drag ticks).
    pub fn connect_change(&self, f: impl Fn(f64) + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(f));
    }

    /// Wire the drag gesture: pointer x → raw value in [min, max], snap to the
    /// step, clamp, then `set_value` (which fires `on_change`). No precision-mode
    /// or 0-stickiness: the value follows the pointer directly, always snapped.
    /// `drag_begin`/`drag_end` only track state (used to brighten the knob).
    fn wire_drag(&self) {
        let gesture = gtk4::GestureDrag::new();
        let value = Rc::clone(&self.value);
        let on_change = Rc::clone(&self.on_change);
        let dragging = Rc::clone(&self.dragging);
        let (mn, mx, step) = (self.min, self.max, self.step);

        let dragging_begin = Rc::clone(&dragging);
        let area_begin = self.area.clone();
        gesture.connect_drag_begin(move |_g, _x, _y| {
            dragging_begin.set(true);
            area_begin.queue_draw();
        });

        let area_update = self.area.clone();
        gesture.connect_drag_update(move |g, dx, _dy| {
            let (start_x, _) = g.start_point().unwrap_or((0.0, 0.0));
            let w = area_update.width() as f64;
            let track_len = (w - 2.0 * PAD).max(0.0);
            if track_len <= 0.0 {
                return;
            }
            let raw = mn + ((start_x + dx - PAD) / track_len).clamp(0.0, 1.0) * (mx - mn);
            let snapped = snap_to_grid(raw, step, mn, mx);
            // Fire only on an actual step change (a pointer move within one 0.05
            // bucket is the same value), and redraw live so the knob tracks the
            // pointer during the drag — not just on release.
            if (snapped - value.get()).abs() > 1e-9 {
                value.set(snapped);
                area_update.queue_draw();
                if let Some(f) = on_change.borrow().as_ref() {
                    f(snapped);
                }
            }
        });

        let dragging_end = Rc::clone(&dragging);
        let area_end = self.area.clone();
        gesture.connect_drag_end(move |_g, _dx, _dy| {
            dragging_end.set(false);
            area_end.queue_draw();
        });

        self.area.add_controller(gesture);
    }
}

/// Cairo helper: a rounded rectangle path (radius clamped to the smaller side).
fn rounded_rect(cr: &gtk4::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::snap_to_grid;

    #[test]
    fn snap_to_grid_lands_on_step() {
        // EV/warmth grid 0.05.
        assert!((snap_to_grid(1.93, 0.05, -3.0, 5.0) - 1.95).abs() < 1e-9);
        assert!((snap_to_grid(1.90, 0.05, -3.0, 5.0) - 1.90).abs() < 1e-9);
        assert!((snap_to_grid(-1.93, 0.05, -4.0, 4.0) - (-1.95)).abs() < 1e-9);
        // Float-safe: 39·0.05 does not leak 1.9500000000000002.
        assert_eq!(snap_to_grid(1.95, 0.05, -3.0, 5.0), 1.95);
        // Tint grid 0.01.
        assert!((snap_to_grid(0.005, 0.01, -1.0, 1.0) - 0.01).abs() < 1e-9);
        assert!((snap_to_grid(-0.005, 0.01, -1.0, 1.0) - (-0.01)).abs() < 1e-9);
        // Clamped to [min, max].
        assert_eq!(snap_to_grid(99.0, 0.05, -3.0, 5.0), 5.0);
        assert_eq!(snap_to_grid(-99.0, 0.05, -3.0, 5.0), -3.0);
    }
}
