//! Lightweight toast overlay (mpd-client pattern).
//! Thread: UI (GTK main loop) only.
use gtk4::prelude::*;
use gtk4::{Label, Overlay};

pub struct Toast {
    pub overlay: Overlay,
    label: Label,
}

impl Toast {
    pub fn new() -> Self {
        let overlay = Overlay::new();
        let label = Label::new(Some(""));
        label.add_css_class("toast");
        label.set_halign(gtk4::Align::Center);
        label.set_valign(gtk4::Align::End);
        label.set_margin_bottom(24);
        label.set_visible(false);
        overlay.add_overlay(&label);

        // The `.toast` pill (dark + amber accent + slide-in/fade) lives in the
        // shared Darkroom theme in grid::install_css() — install it here so the
        // style is registered even if the toast is shown before the grid builds.
        crate::ui::grid::install_css();

        Self { overlay, label }
    }

    /// Show a message for 3s, then hide.
    pub fn show(&self, msg: &str) {
        self.label.set_text(msg);
        self.label.set_visible(true);
        let label = self.label.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(3000), move || {
            label.set_visible(false);
        });
    }
}
