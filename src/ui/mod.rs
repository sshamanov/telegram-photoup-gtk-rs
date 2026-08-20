use std::sync::{Arc, RwLock};
use adw::prelude::AdwApplicationWindowExt;
use gtk4::prelude::*;

use crate::state::{AppState, AppEvent, reduce};

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

    // Login screen. `on_event` is a placeholder until Task 21 dispatches AppEvent
    // through reduce() to the real Telegram channel.
    let st = Arc::clone(&state);
    let on_event = Box::new(move |_ev: crate::state::AppEvent| {
        let mut s = st.write().unwrap();
        // TODO(Task 21): also dispatch to the Telegram channel.
        reduce(&mut s, _ev);
    }) as Box<dyn Fn(crate::state::AppEvent) + Send + 'static>;

    let login = crate::ui::login::LoginScreen::new(Arc::clone(&state), on_event);
    stack.add_named(&login.root, Some("login"));
    // The `login` struct handle drops here; the widget tree (stack → root → children)
    // keeps the visible UI alive. Task 21 must hold the screen handles to switch
    // steps from AuthStatus (set_data needs Send + 'static, so Rc<RefCell> won't fit).

    // Main screen: group picker + load/send + thumbnail grid. Mounted but hidden —
    // the login screen is the visible child until Task 21 switches to it on auth.
    let main = crate::ui::main_screen::MainScreen::new();
    stack.add_named(&main.root, Some("main"));
    // Same lifetime story as `login` above: `main` drops here but the widget tree
    // (stack → root → header → scroller → grid → store) keeps everything alive.
    // Task 21 restructures screen holding.

    window.present();
}
