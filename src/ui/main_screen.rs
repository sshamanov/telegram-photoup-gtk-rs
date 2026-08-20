//! Main screen: target-group picker, load button, send button, usage label, thumbnail grid.
//! Thread: UI (GTK main loop) only.
use gtk4::prelude::*;
use gtk4::{Box as GBox, Button, Label, Orientation};

pub struct MainScreen {
    pub root: GBox,
    pub group_dropdown: gtk4::DropDown,
    pub load_button: Button,
    pub send_button: Button,
    pub usage_label: Label,
    pub grid_store: gtk4::gio::ListStore,
}

impl MainScreen {
    pub fn new() -> Self {
        let root = GBox::new(Orientation::Vertical, 8);
        root.set_margin_top(8);
        root.set_margin_bottom(8);
        root.set_margin_start(8);
        root.set_margin_end(8);

        // Header row.
        let header = GBox::new(Orientation::Horizontal, 8);
        let group_dropdown = gtk4::DropDown::default();
        group_dropdown.set_tooltip_text(Some("Target group…"));
        header.append(&group_dropdown);

        let load_button = Button::with_label("Load photos…");
        header.append(&load_button);

        let send_button = Button::with_label("Send");
        send_button.add_css_class("suggested-action");
        header.append(&send_button);

        let usage_label = Label::new(Some(""));
        usage_label.set_hexpand(true);
        usage_label.set_halign(gtk4::Align::End);
        header.append(&usage_label);
        root.append(&header);

        // Thumbnail grid (lazy-virtualized).
        let (grid, grid_store) = crate::ui::grid::build_grid();
        grid.set_max_columns(5);
        grid.set_min_columns(2);
        grid.set_halign(gtk4::Align::Fill);
        grid.set_valign(gtk4::Align::Fill);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_child(Some(&grid));
        scroller.set_vexpand(true);
        root.append(&scroller);

        Self { root, group_dropdown, load_button, send_button, usage_label, grid_store }
    }
}
