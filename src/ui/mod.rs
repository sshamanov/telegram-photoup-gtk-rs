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
pub mod slider;
pub mod toast;
pub mod util;

const APP_ID: &str = "dev.shamanov.photoup2";

/// Held for the process lifetime: the kernel releases the flock on exit, so a
/// crashed instance never leaves the app unstartable.
struct InstanceLock {
    _file: std::fs::File,
}

/// Per-user single-instance lock. `Ok` = this process is the primary instance
/// (hold it until `run()` returns); `Err(pid)` = another photoup2 is alive.
///
/// GApplication hands a second launch to the running instance and then exits
/// without printing anything, which reads as "the app does not start" whenever
/// that live instance has no window on screen. The lock lets the forwarded
/// launch say what happened instead.
fn instance_lock() -> Result<InstanceLock, Option<u32>> {
    use std::io::Write;
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join("photoup2.lock");
    // Deliberately no truncate: on the failure path the file still holds the
    // live instance's pid.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|_| None)?;
    // SAFETY: `file` keeps the fd open for the call; flock takes no pointers.
    if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // Record who holds it — the lock, not the contents, is the truth.
        let _ = file.set_len(0);
        let _ = (&file).write_all(std::process::id().to_string().as_bytes());
        return Ok(InstanceLock { _file: file });
    }
    let owner = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok());
    Err(owner)
}

pub fn run() -> glib::ExitCode {
    adw::init().expect("adw init");
    match instance_lock() {
        Ok(lock) => {
            // Keep the lock alive for the whole `app.run()`.
            let _lock = lock;
            run_app()
        }
        Err(owner) => {
            log::warn!(
                "another photoup2 is already running{} — this launch is handed to it; it \
                 exits as soon as that instance takes the activation. If no window appears, \
                 `pgrep -x photoup2 | xargs -r kill` and start again.",
                owner.map(|p| format!(" (pid {p})")).unwrap_or_default()
            );
            run_app()
        }
    }
}

fn run_app() -> glib::ExitCode {

    // Darkroom: force a dark base so every libadwaita widget renders dark
    // underneath our warm CSS palette (the CSS provider then adds the amber
    // accent + surfaces).
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        // Single-instance: re-activate (dock click / relaunch) just re-presents.
        if let Some(win) = app.active_window() {
            log::info!("single-instance: another instance already running — re-presenting window");
            win.present();
            return;
        }
        let state = Arc::new(RwLock::new(AppState::default()));
        log::info!("creating main window");
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
    window.add_css_class("dark-bg");

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
