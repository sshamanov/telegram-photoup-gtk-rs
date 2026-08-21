//! Thumbnail grid: a GObject row type + a GtkGridView fed by a GListStore.
//! Cell layout mirrors photoup's `PhotoThumb`: a square-ish image area with a
//! checkbox (top-left), an EV badge (top-right), and a bottom meta row with the
//! truncated filename + a RAW/JPG badge.
//! Thread: UI (GTK main loop) only.
use gtk4::prelude::*;
use gtk4::{glib, ListItem, SignalListItemFactory};
use glib::subclass::prelude::*;

#[derive(Default, glib::Properties)]
#[properties(wrapper_type = PhotoRow)]
pub struct PhotoRowInner {
    #[property(get, set)]
    pub id: std::cell::RefCell<u64>,
    #[property(get, set)]
    pub texture: std::cell::RefCell<Option<gdk4::Texture>>,
    #[property(get, set)]
    pub selected: std::cell::RefCell<bool>,
    #[property(get, set)]
    pub error: std::cell::RefCell<bool>,
    /// Effective exposure correction for the EV badge (auto → computed autoEV).
    #[property(get, set)]
    pub ev: std::cell::RefCell<f32>,
    /// Display name (file name) shown under the thumbnail.
    #[property(get, set)]
    pub name: std::cell::RefCell<String>,
    /// True when the source is a RAW file → "RAW" badge, else "JPG".
    #[property(get, set)]
    pub is_raw: std::cell::RefCell<bool>,
    /// True once a thumbnail render succeeded (photoup `status === 'ready'`).
    /// The EV badge only shows when ready AND |ev| > 0.05.
    #[property(get, set)]
    pub ready: std::cell::RefCell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for PhotoRowInner {
    const NAME: &'static str = "PhotoupPhotoRow";
    type Type = PhotoRow;
    type ParentType = glib::Object;
}

// The `glib::Properties` derive fills in `DerivedObjectProperties`; wire those
// into the object machinery here (the gtk-rs standard pattern).
impl ObjectImpl for PhotoRowInner {
    fn properties() -> &'static [glib::ParamSpec] {
        Self::derived_properties()
    }

    fn set_property(&self, id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        self.derived_set_property(id, value, pspec)
    }

    fn property(&self, id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        self.derived_property(id, pspec)
    }
}

glib::wrapper! {
    pub struct PhotoRow(ObjectSubclass<PhotoRowInner>);
}

impl PhotoRow {
    pub fn new(id: u64) -> Self {
        glib::Object::builder().property("id", id).build()
    }
}

/// Depth-first search for a descendant widget whose widget name equals `name`.
///
/// gtk4 0.11 has no `Widget::child_by_widget_name`, so we walk the
/// first_child → next_sibling tree ourselves. The cell is now several levels
/// deep (overlay → picture / placeholder / checkbox / ev label).
fn find_by_name<W: IsA<gtk4::Widget>>(w: &W, name: &str) -> Option<gtk4::Widget> {
    if w.widget_name() == name {
        return Some(w.clone().upcast());
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        if let Some(found) = find_by_name(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// One-time install of the shared Darkroom theme (all rules in one provider).
/// Idempotent across grid rebuilds; also used by the editor/login/toast screens.
///
/// "The Darkroom": warm near-black surfaces, amber safelight accent (#ff7a45),
/// monospace technical readouts, and the photos themselves as the brightest thing
/// on screen. Palette mirrors photoup's `--accent` so the crop box, EV badges,
/// Send button and focus states all read as one amber family.
pub(crate) fn install_css() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let css = gtk4::CssProvider::new();
        css.load_from_string(
            r#"
/* =====================================================================
   Darkroom — warm near-black surfaces under an amber safelight.
   bg #16130f · surface #1f1a15 · raised #262019
   border #3a322a · strong #4a3f35 · text #ece4d8 · muted #8a7f72
   accent #ff7a45 (the single strong color) · glow rgba(255,122,69,0.35)
   ===================================================================== */

/* ---- Root / screens ---- */
.dark-bg { background-color: #16130f; }

/* Raised panel / card */
.panel,
.login-card {
    background-color: #1f1a15;
    border: 1px solid #3a322a;
    border-radius: 10px;
}
.login-card { padding: 26px; }

/* ---- Typography ---- */
.title-app {
    font-family: monospace;
    font-weight: 700;
    font-size: 21px;
    letter-spacing: 0.05em;
    color: #ece4d8;
}
.dim-label { color: #8a7f72; }
.mono { font-family: monospace; }

/* ---- Buttons ---- */
.btn-accent {
    background-color: #ff7a45;
    color: #1c120b;
    border-radius: 8px;
    font-weight: 700;
    padding: 6px 14px;
}
.btn-accent:hover {
    background-color: #ff8a5e;
    box-shadow: 0 0 16px rgba(255, 122, 69, 0.45);
}
.btn-accent:active { background-color: #e5672f; }
.btn-accent:disabled { background-color: #3a322a; color: #8a7f72; box-shadow: none; }

/* The big Send bar (sticky footer). */
.btn-send {
    background-color: #ff7a45;
    color: #1c120b;
    border-radius: 8px;
    font-weight: 700;
    padding: 8px 16px;
}
.btn-send:hover {
    background-color: #ff8a5e;
    box-shadow: 0 0 18px rgba(255, 122, 69, 0.5);
}
.btn-send:active { background-color: #e5672f; }
.btn-send:disabled { background-color: #3a322a; color: #8a7f72; box-shadow: none; }

/* libadwaita suggested-action lights up amber too (editor Pick mode). */
.suggested-action {
    background-color: #ff7a45;
    color: #1c120b;
}
.suggested-action:hover { background-color: #ff8a5e; }

/* ---- Entries / inputs ---- */
.dark-bg entry {
    background-color: #262019;
    color: #ece4d8;
    border: 1px solid #3a322a;
    border-radius: 6px;
}
.dark-bg entry:focus { border-color: #ff7a45; }

/* ---- Header (main screen) ---- */
.header {
    background-color: #1f1a15;
    border: 1px solid #3a322a;
    border-radius: 10px;
    padding: 6px 12px;
}

/* ---- Upload zone ---- */
.upload-zone {
    border: 1.5px dashed #3a322a;
    border-radius: 12px;
    background-color: rgba(31, 26, 21, 0.45);
    color: #ece4d8;
}
.upload-zone:hover {
    border-color: #ff7a45;
    box-shadow: 0 0 18px rgba(255, 122, 69, 0.18);
}
/* Drag-over highlight while a file is hovering the upload zone (photoup `.zone` hover). */
.upload-zone.drag-over {
    border-color: #ff7a45;
    background-color: rgba(255, 122, 69, 0.08);
}

/* ---- Usage indicator: amber pulsing dot + muted mono text ---- */
.usage-dot {
    color: #ff7a45;
    font-size: 13px;
    animation: 1.6s ease-in-out infinite usage-pulse;
}
.usage-text {
    color: #8a7f72;
    font-family: monospace;
    font-size: 12px;
}

/* ---- Send footer progress ---- */
progressbar.send-progress trough {
    background-color: #262019;
    border-radius: 5px;
    min-height: 8px;
}
progressbar.send-progress progress {
    background-color: #ff7a45;
    border-radius: 5px;
}

/* ---- Grid cells ---- */
.cell {
    border: 1px solid transparent;
    border-radius: 8px;
    transition: 150ms border-color, 150ms box-shadow;
}
.cell:hover {
    border-color: #ff7a45;
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.55), 0 0 0 1px rgba(255, 122, 69, 0.35);
}
.cell-name { font-family: monospace; font-size: 11px; color: #8a7f72; }

.cell-error { border: 2px solid #e01b24; }
.cell-placeholder {
    font-size: 11px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: #8a7f72;
    animation: 1.3s ease-in-out infinite developing-pulse;
}
.cell-placeholder-error { color: #e01b24; animation: none; }
.cell-ev {
    /* Amber accent pill: EV reads as the safelight color, high-contrast mono. */
    background: rgba(13, 11, 9, 0.85);
    border: 1px solid rgba(255, 122, 69, 0.55);
    border-radius: 5px;
    padding: 1px 7px;
    font-family: monospace;
    font-size: 12px;
    font-weight: 700;
    color: #ff7a45;
    box-shadow: 0 1px 6px rgba(0, 0, 0, 0.45);
}
/* High-contrast checkbox pill (photoup `.tick`): dark box, strong border,
   clearly visible against any photo. */
.cell-check {
    background: rgba(13, 11, 9, 0.82);
    border: 2px solid rgba(255, 255, 255, 0.85);
    border-radius: 5px;
    box-shadow: 0 1px 4px rgba(0, 0, 0, 0.5);
}
.cell-check check {
    min-width: 16px;
    min-height: 16px;
    background: transparent;
    box-shadow: none;
}
.cell-check check:checked {
    background: #ff7a45;
    -gtk-icon-source: none;
}
.cell-check check:checked image {
    color: #000;
}
.cell-badge {
    border: 1px solid #3a322a;
    border-radius: 5px;
    padding: 1px 5px;
    font-family: monospace;
    font-size: 10px;
    letter-spacing: 0.1em;
    color: #8a7f72;
    background: rgba(31, 26, 21, 0.8);
}

/* ---- Editor ---- */
.editor-stage {
    background-color: #16130f;
    border: 1px solid #3a322a;
    border-radius: 8px;
}
.editor-panel {
    background-color: #1f1a15;
    border: 1px solid #3a322a;
    border-radius: 10px;
}
.editor-section {
    font-size: 10px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: #8a7f72;
    font-family: monospace;
}
.editor-reject { color: #ff7a45; border-color: #ff7a45; }
.editor-mono { font-family: monospace; font-size: 13px; color: #ece4d8; }
.editor-value { font-family: monospace; font-size: 12px; color: #ffc9a8; }
.editor-file { font-family: monospace; font-size: 13px; font-weight: 500; color: #ece4d8; }

/* Slider fill / marks get the amber accent. Thin, clean track. */
scale trough {
    min-height: 4px;
    min-width: 4px;
}
scale highlight {
    background-color: #ff7a45;
    border-radius: 2px;
}
scale slider {
    min-width: 14px;
    min-height: 14px;
    border-radius: 50%;
}
scale mark label { color: #8a7f72; }

/* ---- Toast: dark pill, amber accent, gentle slide-in + fade ---- */
.toast {
    background-color: rgba(31, 26, 21, 0.94);
    border: 1px solid rgba(255, 122, 69, 0.6);
    color: #ece4d8;
    border-radius: 14px;
    padding: 8px 18px;
    font-weight: 600;
    animation: 180ms ease-out toast-in;
}

/* ---- Keyframes ---- */
@keyframes developing-pulse {
    0% { opacity: 0.3; }
    50% { opacity: 0.85; }
    100% { opacity: 0.3; }
}
@keyframes usage-pulse {
    0% { opacity: 1; }
    50% { opacity: 0.25; }
    100% { opacity: 1; }
}
@keyframes toast-in {
    from { opacity: 0; margin-top: 10px; }
    to { opacity: 1; margin-top: 0; }
}
"#,
        );
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

/// Show/hide the EV badge: only when the thumb is ready and the effective EV is
/// non-trivial (photoup `showEv = status === 'ready' && |ev| > 0.05`).
fn update_ev(row: &PhotoRow, ev: &gtk4::Label) {
    let v = row.ev();
    if row.ready() && v.abs() > 0.05 {
        ev.set_text(&format!("{v:+.1}"));
        ev.set_visible(true);
    } else {
        ev.set_text("");
        ev.set_visible(false);
    }
}

/// Show the "developing…" / "error" placeholder while the async thumb hasn't
/// arrived (or the decode failed), hiding it once a texture is present.
fn update_placeholder(row: &PhotoRow, ph: &gtk4::Label) {
    if row.error() {
        ph.set_text("error");
        ph.add_css_class("cell-placeholder-error");
        ph.set_visible(true);
    } else if row.texture().is_none() {
        ph.set_text("developing…");
        ph.remove_css_class("cell-placeholder-error");
        ph.set_visible(true);
    } else {
        ph.set_visible(false);
    }
}

/// Build the GridView with a list store of rows. Returns (grid, store) so callers
/// can push rows and update their texture/selected properties.
///
/// `on_toggle` is invoked with (photo id, checked) whenever a cell's checkbox is
/// toggled by the user, so the controller can mirror selection into AppState.
pub fn build_grid(
    on_toggle: impl Fn(u64, bool) + 'static,
) -> (gtk4::GridView, gtk4::gio::ListStore) {
    install_css();
    // Shared so the per-cell bind closures can each hold a clone without moving
    // the `impl Fn` out of the outer `Fn` bind closure.
    let on_toggle = std::rc::Rc::new(on_toggle);
    let store = gtk4::gio::ListStore::new::<PhotoRow>();
    // SingleSelection (not NoSelection) is load-bearing: Task 19's click-to-edit
    // reads the grid's selection to find the active photo. Task 19/21 wires
    // `grid.connect_selected(...)`.
    let selection = gtk4::SingleSelection::new(Some(store.clone()));
    // autoselect would auto-open the editor the moment the first photo is added.
    selection.set_autoselect(false);
    let factory = SignalListItemFactory::new();

    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<ListItem>() else { return };
        let cell = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        cell.add_css_class("cell");
        cell.set_margin_bottom(8);

        // Square-ish image area: the Picture is the Overlay's main child, so its
        // size drives the overlay. A fixed size request guarantees display space
        // before the async thumbnail arrives (it scales to fill via cover).
        let image = gtk4::Picture::new();
        image.set_hexpand(true);
        image.set_content_fit(gtk4::ContentFit::Cover);
        image.set_size_request(170, 170);

        let placeholder = gtk4::Label::new(Some("developing…"));
        placeholder.add_css_class("cell-placeholder");
        placeholder.set_halign(gtk4::Align::Center);
        placeholder.set_valign(gtk4::Align::Center);

        let check = gtk4::CheckButton::new();
        check.add_css_class("cell-check");
        check.set_size_request(22, 22);
        check.set_halign(gtk4::Align::Start);
        check.set_valign(gtk4::Align::Start);
        check.set_margin_top(6);
        check.set_margin_start(6);

        let ev = gtk4::Label::new(Some(""));
        ev.add_css_class("cell-ev");
        ev.set_halign(gtk4::Align::End);
        ev.set_valign(gtk4::Align::Start);
        ev.set_margin_top(6);
        ev.set_margin_end(6);

        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&image));
        overlay.add_overlay(&placeholder);
        overlay.add_overlay(&check);
        overlay.add_overlay(&ev);
        cell.append(&overlay);

        // Bottom meta row: truncated filename + RAW/JPG badge.
        let meta = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        meta.set_margin_start(10);
        meta.set_margin_end(10);
        meta.set_margin_top(2);
        let name = gtk4::Label::new(Some(""));
        name.add_css_class("cell-name");
        name.set_hexpand(true);
        name.set_halign(gtk4::Align::Start);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let badge = gtk4::Label::new(Some("JPG"));
        badge.add_css_class("cell-badge");
        badge.set_halign(gtk4::Align::End);
        meta.append(&name);
        meta.append(&badge);
        cell.append(&meta);

        // Keep the widgets addressable from bind().
        image.set_widget_name("cell-image");
        placeholder.set_widget_name("cell-placeholder");
        check.set_widget_name("cell-check");
        ev.set_widget_name("cell-ev");
        name.set_widget_name("cell-name");
        badge.set_widget_name("cell-badge");
        item.set_child(Some(&cell));
    });

    let bind_on_toggle = std::rc::Rc::clone(&on_toggle);
    factory.connect_bind(move |_, item| {
        let Some(list_item) = item.downcast_ref::<ListItem>() else { return };
        // Bind the owned row object to a local so the &PhotoRow borrow is valid.
        let Some(obj) = list_item.item() else { return };
        let Some(row) = obj.downcast_ref::<PhotoRow>() else { return };
        let Some(cell) = list_item.child().and_then(|c| c.downcast::<gtk4::Box>().ok()) else { return };

        let image = find_by_name(&cell, "cell-image")
            .and_then(|w| w.downcast::<gtk4::Picture>().ok());
        let placeholder = find_by_name(&cell, "cell-placeholder")
            .and_then(|w| w.downcast::<gtk4::Label>().ok());
        let check = find_by_name(&cell, "cell-check")
            .and_then(|w| w.downcast::<gtk4::CheckButton>().ok());
        let ev = find_by_name(&cell, "cell-ev").and_then(|w| w.downcast::<gtk4::Label>().ok());
        let name = find_by_name(&cell, "cell-name").and_then(|w| w.downcast::<gtk4::Label>().ok());
        let badge = find_by_name(&cell, "cell-badge").and_then(|w| w.downcast::<gtk4::Label>().ok());

        if let Some(image) = &image {
            image.set_paintable(row.texture().as_ref());
            if row.error() {
                image.add_css_class("cell-error");
            } else {
                image.remove_css_class("cell-error");
            }
            // GridView only re-binds on items-changed / scroll recycle — NOT when a
            // row's properties change. So subscribe to the row's texture-notify so
            // async thumbnail arrival repaints this Picture in place. The handler
            // id lives in the ListItem's qdata; unbind disconnects it.
            let image2 = image.clone();
            let ph2 = placeholder.clone();
            let conn = row.connect_texture_notify(move |r| {
                image2.set_paintable(r.texture().as_ref());
                if let Some(ph) = &ph2 {
                    update_placeholder(r, ph);
                }
            });
            unsafe { list_item.set_data("tex-conn", conn); }
        }
        if let Some(check) = &check {
            check.set_active(row.selected());
            check.set_sensitive(!row.error());
            // Clicking the checkbox marks the photo for the album. Mirror into the
            // row (so scroll-recycling keeps the visual state) and notify the
            // controller.
            let row2 = row.clone();
            let on_toggle = std::rc::Rc::clone(&bind_on_toggle);
            let conn = check.connect_toggled(move |c| {
                let active = c.is_active();
                row2.set_selected(active);
                on_toggle(row2.id(), active);
            });
            unsafe { list_item.set_data("check-conn", conn); }
            // Mirror programmatic selection changes (Ctrl+A select-all toggles the
            // rows' `selected` property) into the visible checkbox live — a cell
            // only rebinds on items-changed/scroll recycle, so without this the
            // check would lag the row property. `set_active` with an unchanged
            // value doesn't re-emit `toggled`, so there's no feedback loop.
            let check2 = check.clone();
            let conn = row.connect_selected_notify(move |r| {
                check2.set_active(r.selected());
            });
            unsafe { list_item.set_data("sel-conn", conn); }
        }
        if let Some(ev) = &ev {
            update_ev(&row, ev);
            let ev2 = ev.clone();
            let conn = row.connect_ev_notify(move |r| update_ev(r, &ev2));
            unsafe { list_item.set_data("ev-conn", conn); }
            let ev3 = ev.clone();
            let conn = row.connect_ready_notify(move |r| update_ev(r, &ev3));
            unsafe { list_item.set_data("ready-conn", conn); }
        }
        if let Some(name) = &name {
            name.set_text(row.name().as_str());
        }
        if let Some(badge) = &badge {
            badge.set_text(if row.is_raw() { "RAW" } else { "JPG" });
        }
        if let Some(placeholder) = &placeholder {
            update_placeholder(&row, placeholder);
        }

        // Error state: red outline on the Picture + disabled checkbox + "error"
        // placeholder. Kept in sync when the row's `error` property changes after
        // binding, same lifecycle as the texture/check connections.
        if let (Some(image), Some(check), Some(placeholder)) = (&image, &check, &placeholder) {
            let (image, check, placeholder) = (image.clone(), check.clone(), placeholder.clone());
            let conn = row.connect_error_notify(move |r| {
                if r.error() {
                    image.add_css_class("cell-error");
                    check.set_sensitive(false);
                } else {
                    image.remove_css_class("cell-error");
                    check.set_sensitive(true);
                }
                update_placeholder(r, &placeholder);
            });
            unsafe { list_item.set_data("err-conn", conn); }
        }
    });

    factory.connect_unbind(|_, item| {
        let Some(list_item) = item.downcast_ref::<ListItem>() else { return };
        let Some(obj) = list_item.item() else { return };
        let Some(row) = obj.downcast_ref::<PhotoRow>() else { return };
        // Steal (move out) the connection id stored in bind and disconnect it.
        // Safety: see bind — the keys below are SignalHandlerId if present, and
        // steal removes them from the ListItem's qdata so there's no stale entry.
        for key in ["tex-conn", "check-conn", "sel-conn", "err-conn", "ev-conn", "ready-conn"] {
            if let Some(conn) = unsafe { list_item.steal_data::<glib::SignalHandlerId>(key) } {
                row.disconnect(conn);
            }
        }
    });

    let grid = gtk4::GridView::new(Some(selection.clone()), Some(factory.clone()));
    (grid, store)
}
