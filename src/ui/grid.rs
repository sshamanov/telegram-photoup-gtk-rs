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

/// Build the GridView with a list store of rows. Returns (grid, store) so callers
/// can push rows and update their texture/selected properties.
pub fn build_grid() -> (gtk4::GridView, gtk4::gio::ListStore) {
    let store = gtk4::gio::ListStore::new::<PhotoRow>();
    let selection = gtk4::SingleSelection::new(Some(store.clone()));
    let factory = SignalListItemFactory::new();

    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<ListItem>() else { return };
        let cell = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        cell.set_margin_bottom(8);
        let image = gtk4::Picture::new();
        image.set_vexpand(true);
        image.set_hexpand(true);
        let check = gtk4::CheckButton::new();
        check.set_valign(gtk4::Align::End);
        cell.append(&image);
        cell.append(&check);
        // Keep the two widgets addressable from bind().
        image.set_widget_name("cell-image");
        check.set_widget_name("cell-check");
        item.set_child(Some(&cell));
    });

    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<ListItem>() else { return };
        // Bind the owned row object to a local so the &PhotoRow borrow is valid.
        let Some(obj) = item.item() else { return };
        let Some(row) = obj.downcast_ref::<PhotoRow>() else { return };
        let Some(cell) = item.child().and_then(|c| c.downcast::<gtk4::Box>().ok()) else { return };
        // Bind the row's texture → Picture, selected → checkbox.
        if let Some(image) = child_by_widget_name(&cell, "cell-image")
            .and_then(|w| w.downcast::<gtk4::Picture>().ok())
        {
            let tex = row.texture();
            image.set_paintable(tex.as_ref());
        }
        if let Some(check) = child_by_widget_name(&cell, "cell-check")
            .and_then(|w| w.downcast::<gtk4::CheckButton>().ok())
        {
            check.set_active(row.selected());
        }
    });

    let grid = gtk4::GridView::new(Some(selection.clone()), Some(factory.clone()));
    (grid, store)
}
