//! Main screen (photoup `App.svelte`): header (title + "Send to" group selector +
//! Reset + Logout), upload zone, usage indicator, thumbnail grid, and a sticky
//! footer with the Send button + send-progress bar.
//! Thread: UI (GTK main loop) only.
use gtk4::prelude::*;
use gtk4::{Box as GBox, Button, Label, Orientation, ProgressBar};

pub struct MainScreen {
    pub root: GBox,
    pub group_dropdown: gtk4::DropDown,
    /// Dashed clickable upload area → opens the file picker.
    pub upload_zone: Button,
    pub reset_button: Button,
    pub logout_button: Button,
    /// UsageIndicator row: amber pulsing dot + muted "Processing {name} …" text.
    pub usage_row: gtk4::Box,
    pub usage_dot: Label,
    pub usage_label: Label,
    pub send_button: Button,
    /// Progress bar shown next to the Send button while an album is uploading.
    pub send_progress: ProgressBar,
    pub grid_store: gtk4::gio::ListStore,
    pub grid: gtk4::GridView,
}

impl MainScreen {
    pub fn new(on_toggle: impl Fn(u64, bool) + 'static) -> Self {
        let root = GBox::new(Orientation::Vertical, 8);
        root.set_margin_top(12);
        root.set_margin_bottom(0);
        root.set_margin_start(16);
        root.set_margin_end(16);
        root.add_css_class("dark-bg");

        // Header: title + actions (group selector / Reset / Logout) on a raised
        // darkroom control strip.
        let header = GBox::new(Orientation::Horizontal, 10);
        header.add_css_class("header");
        header.set_margin_bottom(10);
        let title = Label::new(Some("photoup"));
        title.add_css_class("title-app");
        header.append(&title);

        let spacer = GBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);

        let send_to = Label::new(Some("Send to"));
        send_to.add_css_class("dim-label");
        header.append(&send_to);
        let group_dropdown = gtk4::DropDown::default();
        group_dropdown.set_tooltip_text(Some("Target group…"));
        header.append(&group_dropdown);

        let reset_button = Button::with_label("Reset");
        reset_button.set_sensitive(false);
        header.append(&reset_button);

        let logout_button = Button::with_label("Logout");
        header.append(&logout_button);
        root.append(&header);

        // Upload zone (photoup `UploadZone`): dashed clickable area.
        let upload_zone = Button::new();
        upload_zone.add_css_class("upload-zone");
        upload_zone.set_hexpand(true);
        let zone_box = GBox::new(Orientation::Vertical, 6);
        zone_box.set_margin_top(16);
        zone_box.set_margin_bottom(16);
        let z1 = Label::new(Some("Upload photos"));
        z1.add_css_class("title-3");
        let z2 = Label::new(Some("Drop JPEG / PNG / NEF / CR2 here, or press Ctrl+V to paste"));
        z2.add_css_class("dim-label");
        zone_box.append(&z1);
        zone_box.append(&z2);
        upload_zone.set_child(Some(&zone_box));
        root.append(&upload_zone);

        // Usage indicator: amber pulsing dot + muted mono text on one row.
        // Hidden as a whole while nothing is being processed (the controller
        // toggles `usage_row` visibility).
        let usage_row = GBox::new(Orientation::Horizontal, 6);
        usage_row.set_halign(gtk4::Align::Start);
        let usage_dot = Label::new(Some("●"));
        usage_dot.add_css_class("usage-dot");
        let usage_label = Label::new(Some(""));
        usage_label.add_css_class("usage-text");
        usage_label.set_hexpand(true);
        usage_label.set_halign(gtk4::Align::Start);
        usage_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        usage_row.append(&usage_dot);
        usage_row.append(&usage_label);
        usage_row.set_visible(false);
        root.append(&usage_row);

        // Thumbnail grid (lazy-virtualized).
        let (grid, grid_store) = crate::ui::grid::build_grid(on_toggle);
        grid.set_max_columns(5);
        grid.set_min_columns(2);
        grid.set_halign(gtk4::Align::Fill);
        grid.set_valign(gtk4::Align::Fill);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_child(Some(&grid));
        scroller.set_vexpand(true);
        root.append(&scroller);

        // Footer (sticky bottom): the Send button + a send-progress bar.
        let footer = GBox::new(Orientation::Horizontal, 10);
        footer.set_margin_top(8);
        footer.set_margin_bottom(12);
        let send_button = Button::with_label("Send 0 selected");
        // The amber Send bar (darkroom footer CTA).
        send_button.add_css_class("btn-send");
        send_button.set_hexpand(true);
        send_button.set_sensitive(false);
        let send_progress = ProgressBar::new();
        send_progress.add_css_class("send-progress");
        send_progress.set_width_request(150);
        send_progress.set_show_text(false);
        send_progress.set_visible(false);
        footer.append(&send_button);
        footer.append(&send_progress);
        root.append(&footer);

        Self {
            root,
            group_dropdown,
            upload_zone,
            reset_button,
            logout_button,
            usage_row,
            usage_dot,
            usage_label,
            send_button,
            send_progress,
            grid_store,
            grid,
        }
    }
}
