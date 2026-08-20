use std::sync::{Arc, RwLock};
use adw::prelude::AdwApplicationWindowExt;
use gtk4::prelude::*;

use crate::state::{AppState, AppEvent, reduce};

pub mod editor;
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

fn build_window(app: &adw::Application, state: Arc<RwLock<AppState>>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("photoup2")
        .default_width(1100)
        .default_height(760)
        .build();

    // Root stack: Login / Main / Editor. Screen switches are driven by state.
    let stack = gtk4::Stack::new();
    window.set_content(Some(&stack));

    // One-shot wiring proof (do NOT make this a repeating timer — it would clobber
    // AppState.usage every tick once Task 20 drives the usage label).
    let st = Arc::clone(&state);
    glib::idle_add_local(move || {
        let mut s = st.write().unwrap();
        reduce(&mut s, AppEvent::Usage(crate::state::UsageStats::default()));
        glib::ControlFlow::Break
    });

    window.present();
}
