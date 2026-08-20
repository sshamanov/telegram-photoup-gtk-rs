use std::sync::{Arc, RwLock};
use gtk4::{prelude::*, Application, ApplicationWindow};

use crate::state::{AppState, AppEvent, reduce};

pub mod editor;
pub mod login;
pub mod main_screen;
pub mod toast;
pub mod util;

const APP_ID: &str = "dev.shamanov.photoup2";

pub fn run() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        let state = Arc::new(RwLock::new(AppState::default()));
        build_window(app, state);
    });
    app.run()
}

fn build_window(app: &Application, state: Arc<RwLock<AppState>>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("photoup2")
        .default_width(1100)
        .default_height(760)
        .build();

    // Root stack: Login / Main / Editor. Screen switches are driven by state.
    let stack = gtk4::Stack::new();
    window.set_child(Some(&stack));

    // Poll thread events on the main loop (channels → glib::idle_add).
    // Task 21 wires the actual channels; for now a placeholder reduce wiring.
    let st = Arc::clone(&state);
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        let mut s = st.write().unwrap();
        reduce(&mut s, AppEvent::Usage(crate::state::UsageStats::default()));
        glib::ControlFlow::Continue
    });

    window.present();
}
