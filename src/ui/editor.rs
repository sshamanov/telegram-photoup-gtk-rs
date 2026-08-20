//! Per-photo editor: exposure/WB/crop controls, live preview, histogram.
//! Thread: UI (GTK main loop) only.
//!
//! The controls emit `AppEvent::PhotoEdit { id, adjustments }` through the
//! `on_event` callback; the wiring (Task 21) re-renders the preview and pushes
//! the result back via `set_preview`/`set_histogram`/`set_photo`.
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use gtk4::prelude::*;
use gtk4::{Box as GBox, Button, DrawingArea, Label, Orientation, Picture, Scale};

use crate::image::types::{Adjustments, ExposureMode};
use crate::state::{AppEvent, AppState};

/// Field handles used by Task 21 (histogram_area, buttons, state) are not read
/// until the wiring lands; `#[allow(dead_code)]` keeps the build warning-free.
#[allow(dead_code)]
pub struct EditorScreen {
    pub root: GBox,
    pub preview: Picture,
    histogram_area: DrawingArea,
    exposure_scale: Scale,
    mode_dropdown: gtk4::DropDown,
    temp_scale: Scale,
    hue_scale: Scale,
    reset_button: Button,
    auto_button: Button,
    auto_wb_button: Button,
    crop_button: Button,
    raw_controls: GBox,
    /// Current photo id (set by `set_photo`), read by the signal closures.
    /// `Rc<Cell<..>>` so each closure can own a clone instead of borrowing the screen.
    pub active_id: Rc<Cell<Option<u64>>>,
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

        // Left: live preview.
        let preview = Picture::new();
        preview.set_vexpand(true);
        preview.set_hexpand(true);
        root.append(&preview);

        // Right: control panel.
        let panel = GBox::new(Orientation::Vertical, 8);
        panel.set_width_request(300);

        let title = Label::new(Some("Edit photo"));
        title.add_css_class("title-2");
        panel.append(&title);

        // Mode dropdown: Auto / Slide (aggressive) / Manual.
        let mode_model = gtk4::StringList::new(&["Auto", "Slide", "Manual"]);
        let mode_dropdown = gtk4::DropDown::new(Some(mode_model), None::<gtk4::Expression>);
        mode_dropdown.set_selected(0);
        let mode_row = row_labeled("Exposure mode", &mode_dropdown);
        panel.append(&mode_row);

        // EV scale -4..+4 (only meaningful in Manual; but always shown, disabled in auto).
        let ev_adj = gtk4::Adjustment::new(0.0, -4.0, 4.0, 0.1, 0.5, 0.0);
        let exposure_scale = Scale::new(gtk4::Orientation::Horizontal, Some(&ev_adj));
        exposure_scale.set_value(0.0);
        exposure_scale.set_draw_value(true);
        exposure_scale.set_digits(1);
        let ev_row = row_labeled("Exposure (EV)", &exposure_scale);
        panel.append(&ev_row);

        // RAW-only controls (hidden for JPEG by default).
        let raw_controls = GBox::new(Orientation::Vertical, 8);
        let temp_adj = gtk4::Adjustment::new(0.0, -1.0, 1.0, 0.01, 0.1, 0.0);
        let temp_scale = Scale::new(gtk4::Orientation::Horizontal, Some(&temp_adj));
        temp_scale.set_value(0.0);
        temp_scale.set_draw_value(true);
        temp_scale.set_digits(2);
        raw_controls.append(&row_labeled("Temperature", &temp_scale));

        let hue_adj = gtk4::Adjustment::new(0.0, -1.0, 1.0, 0.01, 0.1, 0.0);
        let hue_scale = Scale::new(gtk4::Orientation::Horizontal, Some(&hue_adj));
        hue_scale.set_value(0.0);
        hue_scale.set_draw_value(true);
        hue_scale.set_digits(2);
        raw_controls.append(&row_labeled("Hue", &hue_scale));

        let auto_wb_button = Button::with_label("Auto WB");
        raw_controls.append(&auto_wb_button);
        raw_controls.set_visible(false);
        panel.append(&raw_controls);

        // Action row.
        let actions = GBox::new(Orientation::Horizontal, 8);
        let auto_button = Button::with_label("Auto");
        let reset_button = Button::with_label("Reset");
        let crop_button = Button::with_label("Crop");
        actions.append(&auto_button);
        actions.append(&reset_button);
        actions.append(&crop_button);
        panel.append(&actions);

        // Histogram.
        let histogram_area = DrawingArea::new();
        histogram_area.set_height_request(120);
        // Neutral frame until the wiring (Task 21) calls `set_histogram`.
        histogram_area.set_draw_func(|_area, cr, _width, _height| {
            cr.set_source_rgb(0.25, 0.25, 0.25);
            let _ = cr.paint();
        });
        panel.append(&histogram_area);

        root.append(&panel);

        let screen = Self {
            root,
            preview,
            histogram_area,
            exposure_scale,
            mode_dropdown,
            temp_scale,
            hue_scale,
            reset_button,
            auto_button,
            auto_wb_button,
            crop_button,
            raw_controls,
            active_id: Rc::new(Cell::new(None)),
            state,
            on_event,
        };

        // Wire controls → PhotoEdit.
        screen.wire_controls();
        screen
    }

    /// Push a photo into the editor: set active id, adjustments, and show/hide RAW controls.
    pub fn set_photo(&mut self, id: u64, adjustments: &Adjustments, is_raw: bool) {
        self.active_id.set(Some(id));
        self.raw_controls.set_visible(is_raw);
        self.mode_dropdown.set_selected(match adjustments.exposure_mode {
            ExposureMode::Auto => 0,
            ExposureMode::Aggressive => 1,
            ExposureMode::Manual => 2,
        });
        self.exposure_scale.set_value(adjustments.exposure_ev as f64);
        self.temp_scale.set_value(adjustments.wb_offset as f64);
        self.hue_scale.set_value(adjustments.hue as f64);
    }

    pub fn set_preview(&self, texture: Option<&gdk4::Texture>) {
        self.preview.set_paintable(texture);
    }

    pub fn set_histogram(&self, bins: &[u32]) {
        let bins: Vec<u32> = bins.to_vec();
        self.histogram_area.set_draw_func(move |_area, cr, width, height| {
            let h = height as f64;
            let w = width as f64;
            cr.set_source_rgb(0.1, 0.1, 0.1);
            let _ = cr.paint();
            let max = bins.iter().copied().max().unwrap_or(1).max(1) as f64;
            cr.set_source_rgb(0.9, 0.9, 0.9);
            let n = bins.len().max(1);
            for (i, &v) in bins.iter().enumerate() {
                let x0 = (i as f64 / n as f64) * w;
                let x1 = ((i + 1) as f64 / n as f64) * w;
                let bh = (v as f64 / max) * h;
                cr.rectangle(x0, h - bh, (x1 - x0).max(1.0), bh);
            }
            let _ = cr.fill();
        });
    }

    /// Each control closure owns clones of the field handles + a clone of the
    /// `active_id` slot and the `on_event` callback, so no reference to `self`
    /// leaks into the `'static` signal handlers.
    fn wire_controls(&self) {
        let on_event = Arc::clone(&self.on_event);
        let active_id = Rc::clone(&self.active_id);
        let mode = self.mode_dropdown.clone();
        let ev = self.exposure_scale.clone();
        let temp = self.temp_scale.clone();
        let hue = self.hue_scale.clone();

        let (a, o, m, e, t, h) =
            (Rc::clone(&active_id), Arc::clone(&on_event), mode.clone(), ev.clone(), temp.clone(), hue.clone());
        mode.connect_selected_notify(move |_| emit(&a, &m, &e, &t, &h, &*o));

        let (a, o, m, e, t, h) =
            (Rc::clone(&active_id), Arc::clone(&on_event), mode.clone(), ev.clone(), temp.clone(), hue.clone());
        ev.connect_value_changed(move |_| emit(&a, &m, &e, &t, &h, &*o));

        let (a, o, m, e, t, h) =
            (Rc::clone(&active_id), Arc::clone(&on_event), mode.clone(), ev.clone(), temp.clone(), hue.clone());
        temp.connect_value_changed(move |_| emit(&a, &m, &e, &t, &h, &*o));

        let (a, o, m, e, t, h) =
            (Rc::clone(&active_id), Arc::clone(&on_event), mode.clone(), ev.clone(), temp.clone(), hue.clone());
        hue.connect_value_changed(move |_| emit(&a, &m, &e, &t, &h, &*o));
    }
}

/// Recompute `Adjustments` from the current control values and emit PhotoEdit
/// for the active photo (no-op while no photo is loaded).
fn emit(
    active_id: &Cell<Option<u64>>,
    mode: &gtk4::DropDown,
    ev: &Scale,
    temp: &Scale,
    hue: &Scale,
    on_event: &(dyn Fn(AppEvent) + Send + Sync),
) {
    if let Some(id) = active_id.get() {
        let adjustments = Adjustments {
            exposure_mode: match mode.selected() {
                1 => ExposureMode::Aggressive,
                2 => ExposureMode::Manual,
                _ => ExposureMode::Auto,
            },
            exposure_ev: ev.value() as f32,
            wb_offset: temp.value() as f32,
            hue: hue.value() as f32,
            crop: None,
        };
        on_event(AppEvent::PhotoEdit { id, adjustments });
    }
}

fn row_labeled(text: &str, widget: &impl IsA<gtk4::Widget>) -> GBox {
    let row = GBox::new(Orientation::Horizontal, 8);
    let label = Label::new(Some(text));
    label.set_width_request(120);
    label.set_halign(gtk4::Align::Start);
    row.append(&label);
    row.append(widget);
    row
}
