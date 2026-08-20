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

        // Give the toast a dark rounded pill so it reads over the grid.
        let css = gtk4::CssProvider::new();
        css.load_from_string(
            ".toast { background-color: rgba(20,20,20,0.85); color: white; \
             border-radius: 14px; padding: 8px 18px; font-weight: 600; }",
        );
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

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
