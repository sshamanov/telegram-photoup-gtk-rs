//! Per-photo editor (photoup `EditorPanel`): preview on the left, a 332px panel
//! on the right with filename, histogram, Exposure / White balance / Crop / Image
//! sections, nav (‹ Prev / Next ›) and Reject / Close.
//!
//! The controls emit `AppEvent::PhotoEdit { id, adjustments }` through the
//! `on_event` callback; the wiring re-renders the preview and pushes the result
//! back via `set_preview`/`set_histogram`/`set_photo`.
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use gtk4::prelude::*;
use gtk4::{Box as GBox, Button, DrawingArea, Label, Orientation, Picture, Scale};

use crate::image::process::export_dimensions;
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

/// "output 2560 × 1709 px" from the full dimensions + active crop.
fn output_line(full: (u32, u32), crop: Option<NormalizedCrop>) -> String {
    let out = export_dimensions(full.0, full.1, crop.as_ref(), EXPORT_EDGE);
    format!("output {} × {} px", out.width, out.height)
}

/// A click handler that applies a crop-preset ratio (photoup `applyPreset`):
/// computes a centered crop rect matching the ratio against the source aspect.
fn crop_preset_handler(
    ratio: f32,
    active_id: Rc<Cell<Option<u64>>>,
    state: Arc<RwLock<AppState>>,
    on_event: Arc<dyn Fn(AppEvent) + Send + Sync + 'static>,
    full_size: Rc<Cell<Option<(u32, u32)>>>,
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
        let crop = NormalizedCrop {
            x: (1.0 - w) / 2.0,
            y: (1.0 - h) / 2.0,
            width: w,
            height: h,
        };
        let mut adj = current_adjustments(&state.read().unwrap(), id);
        adj.crop = Some(crop);
        info2.set_text(&output_line((fw, fh), Some(crop)));
        on_event(AppEvent::PhotoEdit { id, adjustments: adj });
    }
}

pub struct EditorScreen {
    pub root: GBox,
    pub preview: Picture,
    pub nav_prev: Button,
    pub nav_next: Button,
    file_label: Label,
    histogram_area: DrawingArea,
    exposure_scale: Scale,
    temp_scale: Scale,
    hue_scale: Scale,
    ev_value: Label,
    wb_value: Label,
    info1: Label,
    info2: Label,
    auto_exposure_btn: Button,
    slide_exposure_btn: Button,
    rest_exposure_btn: Button,
    reset_wb_btn: Button,
    crop_11: Button,
    crop_23: Button,
    crop_32: Button,
    crop_orig: Button,
    reject_button: Button,
    close_button: Button,
    /// Current photo id (set by `set_photo`), read by the signal closures.
    active_id: Rc<Cell<Option<u64>>>,
    /// Full source dimensions, needed for the Image section + crop presets.
    full_size: Rc<Cell<Option<(u32, u32)>>>,
    is_raw: Rc<Cell<bool>>,
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

        // Left: live preview (contain, like photoup's object-fit: contain).
        let preview = Picture::new();
        preview.set_vexpand(true);
        preview.set_hexpand(true);
        preview.set_content_fit(gtk4::ContentFit::Contain);
        root.append(&preview);

        // Right: control panel, scrollable if the window is short.
        let panel_scroll = gtk4::ScrolledWindow::new();
        let panel = GBox::new(Orientation::Vertical, 10);
        panel.set_width_request(332);
        panel.set_margin_start(4);

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
            cr.set_source_rgb(0.15, 0.15, 0.15);
            let _ = cr.paint();
        });
        panel.append(&histogram_area);

        // ---- Exposure ----
        panel.append(&section_label("Exposure"));
        let ev_adj = gtk4::Adjustment::new(0.0, -3.0, 5.0, 0.1, 0.5, 0.0);
        let exposure_scale = Scale::new(gtk4::Orientation::Horizontal, Some(&ev_adj));
        exposure_scale.set_value(0.0);
        exposure_scale.set_draw_value(false);
        panel.append(&exposure_scale);

        let ev_row = GBox::new(Orientation::Horizontal, 6);
        let auto_exposure_btn = Button::with_label("Auto");
        let slide_exposure_btn = Button::with_label("Slide");
        let rest_exposure_btn = Button::with_label("Rest");
        let ev_value = Label::new(Some("+0.00 EV"));
        ev_value.add_css_class("editor-value");
        ev_value.set_hexpand(true);
        ev_value.set_halign(gtk4::Align::End);
        ev_row.append(&auto_exposure_btn);
        ev_row.append(&slide_exposure_btn);
        ev_row.append(&rest_exposure_btn);
        ev_row.append(&ev_value);
        panel.append(&ev_row);

        // ---- White balance ----
        panel.append(&section_label("White balance"));
        let temp_adj = gtk4::Adjustment::new(0.0, -2.0, 2.0, 0.05, 0.5, 0.0);
        let temp_scale = Scale::new(gtk4::Orientation::Horizontal, Some(&temp_adj));
        temp_scale.set_value(0.0);
        temp_scale.set_draw_value(false);
        panel.append(&temp_scale);

        let hue_adj = gtk4::Adjustment::new(0.0, -2.0, 2.0, 0.05, 0.5, 0.0);
        let hue_scale = Scale::new(gtk4::Orientation::Horizontal, Some(&hue_adj));
        hue_scale.set_value(0.0);
        hue_scale.set_draw_value(false);
        panel.append(&hue_scale);

        let wb_row = GBox::new(Orientation::Horizontal, 6);
        let wb_auto_button = Button::with_label("Auto");
        let pick_button = Button::with_label("Pick");
        let reset_wb_btn = Button::with_label("Reset");
        let wb_value = Label::new(Some("+0.00 · +0.00"));
        wb_value.add_css_class("editor-value");
        wb_value.set_hexpand(true);
        wb_value.set_halign(gtk4::Align::End);
        wb_row.append(&wb_auto_button);
        wb_row.append(&pick_button);
        wb_row.append(&reset_wb_btn);
        wb_row.append(&wb_value);
        // Auto-WB + neutral-picker need the camera color matrix, which the port
        // doesn't carry yet — keep the buttons present but disabled (see report).
        wb_auto_button.set_sensitive(false);
        pick_button.set_sensitive(false);
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

        // Hint (crop-interaction hint; crop drag is deferred in the port).
        let hint = Label::new(Some(
            "Drag handles to resize · drag inside to move · Shift keeps ratio",
        ));
        hint.add_css_class("dim-label");
        hint.set_halign(gtk4::Align::Start);
        hint.set_wrap(true);
        panel.append(&hint);

        // Nav: ‹ Prev | Next ›.
        let nav_row = GBox::new(Orientation::Horizontal, 6);
        let nav_prev = Button::with_label("‹ Prev");
        let nav_next = Button::with_label("Next ›");
        nav_prev.set_hexpand(true);
        nav_next.set_hexpand(true);
        nav_row.append(&nav_prev);
        nav_row.append(&nav_next);
        panel.append(&nav_row);

        // Bottom: Reject | Close.
        let bottom = GBox::new(Orientation::Horizontal, 6);
        let reject_button = Button::with_label("Reject");
        reject_button.add_css_class("editor-reject");
        let close_button = Button::with_label("Close");
        reject_button.set_hexpand(true);
        close_button.set_hexpand(true);
        bottom.append(&reject_button);
        bottom.append(&close_button);
        panel.append(&bottom);

        panel_scroll.set_child(Some(&panel));
        root.append(&panel_scroll);

        let screen = Self {
            root,
            preview,
            nav_prev,
            nav_next,
            file_label,
            histogram_area,
            exposure_scale,
            temp_scale,
            hue_scale,
            ev_value,
            wb_value,
            info1,
            info2,
            auto_exposure_btn,
            slide_exposure_btn,
            rest_exposure_btn,
            reset_wb_btn,
            crop_11,
            crop_23,
            crop_32,
            crop_orig,
            reject_button,
            close_button,
            active_id: Rc::new(Cell::new(None)),
            full_size: Rc::new(Cell::new(None)),
            is_raw: Rc::new(Cell::new(false)),
            suppress: Rc::new(Cell::new(false)),
            state,
            on_event,
        };

        screen.wire_controls();
        screen.wire_buttons();
        screen
    }

    /// Push a photo into the editor: set active id, name, adjustments and the
    /// info/nav rows. `shown_ev` is the effective EV (autoEV in auto/aggressive,
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
        self.full_size.set(full_size);
        self.file_label.set_text(name);
        self.file_label.set_tooltip_text(Some(name));
        self.exposure_scale.set_value(shown_ev as f64);
        self.temp_scale.set_value(adjustments.wb_offset as f64);
        self.hue_scale.set_value(adjustments.hue as f64);
        self.suppress.set(false);
        self.refresh_value_labels();
        self.refresh_image_info();
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

    pub fn set_nav(&self, has_prev: bool, has_next: bool) {
        self.nav_prev.set_sensitive(has_prev);
        self.nav_next.set_sensitive(has_next);
    }

    fn refresh_value_labels(&self) {
        self.ev_value
            .set_text(&format!("{:+.2} EV", self.exposure_scale.value()));
        self.wb_value.set_text(&format!(
            "{:+.2} · {:+.2}",
            self.temp_scale.value(),
            self.hue_scale.value()
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
        let ev = self.exposure_scale.clone();
        let temp = self.temp_scale.clone();
        let hue = self.hue_scale.clone();
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
        ev.connect_value_changed(move |sc| {
            if s.get() {
                return;
            }
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.exposure_mode = ExposureMode::Manual;
            adj.exposure_ev = sc.value() as f32;
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
        temp.connect_value_changed(move |sc| {
            if s.get() {
                return;
            }
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.wb_offset = sc.value() as f32;
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
        hue.connect_value_changed(move |sc| {
            if s.get() {
                return;
            }
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.hue = sc.value() as f32;
            lab.set_text(&format!("{:+.2} · {:+.2}", temp2.value(), adj.hue));
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });
    }

    fn wire_buttons(&self) {
        let on_event = Arc::clone(&self.on_event);
        let active_id = Rc::clone(&self.active_id);
        let state = Arc::clone(&self.state);
        let suppress = Rc::clone(&self.suppress);
        let ev = self.exposure_scale.clone();
        let temp = self.temp_scale.clone();
        let hue = self.hue_scale.clone();
        let full_size = Rc::clone(&self.full_size);
        let ev_lab = self.ev_value.clone();
        let wb_lab = self.wb_value.clone();
        let info2 = self.info2.clone();

        // Exposure: Auto / Slide.
        let (a, o, st) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
        );
        self.auto_exposure_btn.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.exposure_mode = ExposureMode::Auto;
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });
        let (a, o, st) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
        );
        self.slide_exposure_btn.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.exposure_mode = ExposureMode::Aggressive;
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

        // Crop presets.
        self.crop_11.connect_clicked(crop_preset_handler(
            1.0,
            Rc::clone(&active_id),
            Arc::clone(&state),
            Arc::clone(&on_event),
            Rc::clone(&full_size),
            info2.clone(),
        ));
        self.crop_23.connect_clicked(crop_preset_handler(
            2.0 / 3.0,
            Rc::clone(&active_id),
            Arc::clone(&state),
            Arc::clone(&on_event),
            Rc::clone(&full_size),
            info2.clone(),
        ));
        self.crop_32.connect_clicked(crop_preset_handler(
            3.0 / 2.0,
            Rc::clone(&active_id),
            Arc::clone(&state),
            Arc::clone(&on_event),
            Rc::clone(&full_size),
            info2.clone(),
        ));

        // Original: no crop.
        let (a, o, st, full, i2) = (
            Rc::clone(&active_id),
            Arc::clone(&on_event),
            Arc::clone(&state),
            Rc::clone(&full_size),
            info2,
        );
        self.crop_orig.connect_clicked(move |_| {
            let Some(id) = a.get() else { return };
            let mut adj = current_adjustments(&st.read().unwrap(), id);
            adj.crop = None;
            if let Some(full) = full.get() {
                i2.set_text(&output_line(full, None));
            }
            o(AppEvent::PhotoEdit { id, adjustments: adj });
        });

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
