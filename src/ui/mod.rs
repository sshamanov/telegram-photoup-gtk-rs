use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use adw::prelude::AdwApplicationWindowExt;
use gtk4::prelude::*;

use crate::app::AppController;
use crate::state::AppState;

pub mod editor;
pub mod grid;
pub mod login;
pub mod main_screen;
pub mod toast;
pub mod util;

const APP_ID: &str = "dev.shamanov.photoup2";

pub fn run() -> glib::ExitCode {
    adw::init().expect("adw init");
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        // Single-instance: re-activate (dock click / relaunch) just re-presents.
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        let state = Arc::new(RwLock::new(AppState::default()));
        build_window(app, state);
    });
    app.run()
}

/// Build the window + controller, then drive everything from one 50ms poll.
///
/// The controller owns the screens and channels; this function is just the
/// assembly: window → toast overlay → root stack, plus the poller.
fn build_window(app: &adw::Application, state: Arc<RwLock<AppState>>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("photoup2")
        .default_width(1100)
        .default_height(760)
        .build();

    let ctl = Rc::new(RefCell::new(AppController::new(state, window.clone())));
    ctl.borrow_mut().setup(Rc::clone(&ctl));

    // The toast overlay is the window's content; the root stack lives inside it.
    let overlay = ctl.borrow().toast.overlay.clone();
    let stack = ctl.borrow().stack.clone();
    overlay.set_child(Some(&stack));
    window.set_content(Some(&overlay));

    // One poll loop drives the whole app: drains telegram events, pool results,
    // and screen events, then reflects state in the visible screens.
    let poll_ctl = Rc::clone(&ctl);
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        poll_ctl.borrow_mut().poll();
        glib::ControlFlow::Continue
    });

    window.present();
}
