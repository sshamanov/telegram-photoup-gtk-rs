//! Thumbnail grid: a GObject row type + a GtkGridView fed by a GListStore.
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

/// Find a direct child of `parent` whose widget name equals `name`.
///
/// gtk4 0.11 has no `Widget::child_by_widget_name`, so we walk the
/// first_child → next_sibling chain (the cell is exactly 2 widgets deep).
fn child_by_widget_name<W: IsA<gtk4::Widget>>(parent: &W, name: &str) -> Option<gtk4::Widget> {
    let mut child = parent.first_child();
    while let Some(c) = child {
        if c.widget_name() == name {
            return Some(c);
        }
        child = c.next_sibling();
    }
    None
}

/// One-time install of the `.cell-error` style (red outline marking a photo whose
/// decode/export failed). Idempotent across grid rebuilds; matches toast.rs's
/// provider registration.
fn install_css() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let css = gtk4::CssProvider::new();
        css.load_from_string(".cell-error { border: 2px solid #e01b24; }");
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
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
        cell.set_margin_bottom(8);
        let image = gtk4::Picture::new();
        image.set_vexpand(true);
        image.set_hexpand(true);
        // GridView measures cells from natural size; a Picture with no paintable is
        // 0×0, so cells collapsed to checkbox height and thumbnails never got display
        // space. A minimum height guarantees the photo area renders even before the
        // async thumbnail arrives (it scales to fit via content-fit=contain).
        image.set_size_request(0, 150);
        let check = gtk4::CheckButton::new();
        check.set_valign(gtk4::Align::End);
        cell.append(&image);
        cell.append(&check);
        // Keep the two widgets addressable from bind().
        image.set_widget_name("cell-image");
        check.set_widget_name("cell-check");
        item.set_child(Some(&cell));
    });

    let bind_on_toggle = std::rc::Rc::clone(&on_toggle);
    factory.connect_bind(move |_, item| {
        let Some(list_item) = item.downcast_ref::<ListItem>() else { return };
        // Bind the owned row object to a local so the &PhotoRow borrow is valid.
        let Some(obj) = list_item.item() else { return };
        let Some(row) = obj.downcast_ref::<PhotoRow>() else { return };
        let Some(cell) = list_item.child().and_then(|c| c.downcast::<gtk4::Box>().ok()) else { return };
        // Bind the row's texture → Picture, selected → checkbox.
        if let Some(image) = child_by_widget_name(&cell, "cell-image")
            .and_then(|w| w.downcast::<gtk4::Picture>().ok())
        {
            image.set_paintable(row.texture().as_ref());
            if row.error() {
                image.add_css_class("cell-error");
            } else {
                // GridView recycles cells: a cell that previously showed a failed
                // photo must drop the red border when rebound to a healthy row.
                image.remove_css_class("cell-error");
            }
            // GridView only re-binds on items-changed / scroll recycle — NOT when a
            // row's properties change. So subscribe to the row's texture-notify so
            // async thumbnail arrival (Task 21 `set_texture`) repaints this Picture
            // in place. The handler id lives in the ListItem's qdata; unbind
            // disconnects it so a scrolled-away row can't touch a recycled cell.
            // Safety: "tex-conn" is only ever stored/read here as SignalHandlerId.
            let image2 = image.clone();
            let conn = row.connect_texture_notify(move |r| {
                image2.set_paintable(r.texture().as_ref());
            });
            unsafe { list_item.set_data("tex-conn", conn); }
        }
        if let Some(check) = child_by_widget_name(&cell, "cell-check")
            .and_then(|w| w.downcast::<gtk4::CheckButton>().ok())
        {
            check.set_active(row.selected());
            check.set_sensitive(!row.error());
            // Clicking the checkbox marks the photo for the album. Mirror into the
            // row (so scroll-recycling keeps the visual state) and notify the
            // controller. Mirrors the texture-notify conn/disconnect lifecycle.
            let row2 = row.clone();
            let on_toggle = std::rc::Rc::clone(&bind_on_toggle);
            let conn = check.connect_toggled(move |c| {
                let active = c.is_active();
                row2.set_selected(active);
                on_toggle(row2.id(), active);
            });
            unsafe { list_item.set_data("check-conn", conn); }
        }
        // Error state: red outline on the Picture + disabled checkbox. Applied
        // above at bind time; this notify keeps the cell in sync when the row's
        // `error` property changes after binding (Task 23), same lifecycle as the
        // texture/check connections.
        let err_image = child_by_widget_name(&cell, "cell-image")
            .and_then(|w| w.downcast::<gtk4::Picture>().ok());
        let err_check = child_by_widget_name(&cell, "cell-check")
            .and_then(|w| w.downcast::<gtk4::CheckButton>().ok());
        if let (Some(image), Some(check)) = (err_image, err_check) {
            let conn = row.connect_error_notify(move |r| {
                if r.error() {
                    image.add_css_class("cell-error");
                    check.set_sensitive(false);
                } else {
                    image.remove_css_class("cell-error");
                    check.set_sensitive(true);
                }
            });
            unsafe { list_item.set_data("err-conn", conn); }
        }
    });

    factory.connect_unbind(|_, item| {
        let Some(list_item) = item.downcast_ref::<ListItem>() else { return };
        let Some(obj) = list_item.item() else { return };
        let Some(row) = obj.downcast_ref::<PhotoRow>() else { return };
        // Steal (move out) the connection id stored in bind and disconnect it.
        // Safety: see bind — "tex-conn"/"check-conn" are SignalHandlerId if present,
        // and steal removes them from the ListItem's qdata so there's no stale entry.
        if let Some(conn) = unsafe { list_item.steal_data::<glib::SignalHandlerId>("tex-conn") } {
            row.disconnect(conn);
        }
        if let Some(conn) = unsafe { list_item.steal_data::<glib::SignalHandlerId>("check-conn") } {
            row.disconnect(conn);
        }
        if let Some(conn) = unsafe { list_item.steal_data::<glib::SignalHandlerId>("err-conn") } {
            row.disconnect(conn);
        }
    });

    let grid = gtk4::GridView::new(Some(selection.clone()), Some(factory.clone()));
    (grid, store)
}
