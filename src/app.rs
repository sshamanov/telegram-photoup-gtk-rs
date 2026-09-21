//! AppController: owns the screens, the image pool, and the telegram worker, and
//! drives the whole UI from a single 50ms poll on the GTK main thread.
//!
//! Thread model: every `AppController` method runs on the GTK main thread. The
//! pool workers and the telegram worker never touch widgets — they only send
//! bytes/events back through channels, which `poll()` drains.
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};

use gtk4::gio;
use gtk4::prelude::*;

use crate::image::decode::{RawDecodeOpts, decode_jpeg, decode_raw};
use crate::image::encode::{MAX_PHOTO_BYTES, encode_jpeg_444_adaptive};
use crate::image::math::fit_within;
use crate::image::pool::ImagePool;
use crate::image::process::{
    Base, JpegBase, RawBase, RotatedBase, compute_histogram_rgb, crop_rect, export_dimensions,
    rotate_dims,
};
use crate::image::srgb::export_wb_mul;
use crate::image::types::{Adjustments, ExposureMode, PhotoMeta, Size, SourceType};
use crate::state::{
    AppEvent, AppState, AuthEvent, AuthStatus, PhotoState, PhotoStatus, UsageStats, reduce,
};
use crate::telegram::{AuthStep, DialogInfo, TCommand, TEvent, worker::TelegramWorkerConfig};
use crate::ui::editor::EditorScreen;
use crate::ui::grid::PhotoRow;
use crate::ui::login::LoginScreen;
use crate::ui::main_screen::MainScreen;
use crate::ui::toast::Toast;

// Editor preview: live slider edits render at LIVE_EDGE (512px — snappy), then
// upgrade to the sharp FINAL_EDGE (1024px) once the edit settles
// (PREVIEW_DEBOUNCE_MS of quiet).
const LIVE_EDGE: u32 = 512;
const FINAL_EDGE: u32 = 1024;
/// Longest edge of an export render (Telegram photo size cap).
const EXPORT_EDGE: u32 = 2560;
/// Longest edge of the LINEAR WB sample pushed to the editor (the WB pick/auto
/// measure the cast from a small linear pre-tone downscale — see `linear_sample`).
const WB_SAMPLE_EDGE: u32 = 96;
/// Debounce window for slider-drag preview re-renders.
const PREVIEW_DEBOUNCE_MS: u64 = 150;
/// Image extensions the upload zone accepts (drag-drop, Ctrl+V paste, and the
/// file picker filter all funnel through this). photoup's `UploadZone` accepts
/// `image/*` plus RAW; we pin the six the picker advertises. The RAW ones must
/// stay in sync with `image::decode::RAW_EXTS` — `raw_exts_are_accepted_photos`
/// enforces that.
const IMAGE_EXTS: [&str; 6] = ["jpg", "jpeg", "png", "nef", "cr2", "dng"];

/// Is `path` a photo we accept? Extension-only check, case-insensitive
/// (photoup UploadZone `isPhoto`).
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| IMAGE_EXTS.contains(&s.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Which decoder a path gets: RAW formats (NEF/CR2/DNG) go through LibRaw, and
/// everything else through the `image`-crate JPEG/PNG path. Drives both the
/// decode dispatch and the grid's RAW/JPG badge.
pub fn source_type_for(path: &Path) -> SourceType {
    match path.extension().and_then(|s| s.to_str()) {
        Some(e) if crate::image::decode::is_raw_ext(e) => SourceType::Raw,
        _ => SourceType::Jpeg,
    }
}

/// Keep only accepted photo paths. The drag-drop and Ctrl+V paste handlers funnel
/// through here so non-photo files dropped/pasted are silently ignored.
pub fn filter_photo_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| is_image_path(p)).collect()
}

/// Pool job results, carried back to the UI thread.
pub enum UiEvent {
    ThumbReady {
        id: u64,
        rgba: Vec<u8>,
        size: (u32, u32),
        full: (u32, u32),
        auto_ev: f32,
        /// Effective black point this render used (auto-derived or manual).
        black_point: f32,
        histogram: Vec<u32>,
        cam_mul: Option<[f32; 4]>,
        /// EXIF capture metadata read during this decode, forwarded to state.
        meta: PhotoMeta,
    },
    PreviewReady {
        id: u64,
        rgba: Vec<u8>,
        size: (u32, u32),
        full: (u32, u32),
        auto_ev: f32,
        /// Effective black point this render used (auto-derived or manual).
        black_point: f32,
        histogram: Vec<u32>,
        cam_mul: Option<[f32; 4]>,
        cam_matrix: Option<[[f32; 4]; 3]>,
        /// Present only on the decode path; the controller caches it as the active
        /// photo's base so subsequent slider edits render from memory. `None` on
        /// render-only jobs (the base was already cached).
        base: Option<Arc<dyn Base>>,
        preview_gen: u64,
    },
    /// A downscaled LINEAR (0..1) RGB sample of the active photo's decoded base —
    /// before exposure/WB/tone — for the editor's WB Auto/Pick. Interleaved RGB,
    /// `w*h*3` length.
    WbSample {
        id: u64,
        rgba: Vec<f32>,
        w: u32,
        h: u32,
    },
    ExportReady {
        id: u64,
        jpeg: Vec<u8>,
        width: u32,
        height: u32,
    },
    JobFailed {
        id: u64,
        msg: String,
    },
}

pub struct AppController {
    pub state: Arc<RwLock<AppState>>,
    pub pool: ImagePool,
    pub telegram_cmd: Sender<TCommand>,
    pub telegram_events: Receiver<TEvent>,
    pub ui_events: Receiver<UiEvent>,
    pub stack: gtk4::Stack,
    pub login: LoginScreen,
    pub main_screen: MainScreen,
    pub editor: EditorScreen,
    pub toast: Toast,
    pub row_map: HashMap<u64, PhotoRow>,
    pub next_photo_id: u64,
    /// Parent for modal dialogs.
    window: adw::ApplicationWindow,

    // Channel senders handed to the screens / pool jobs.
    ui_events_sender: Sender<UiEvent>,
    screen_events: Receiver<AppEvent>,

    // Persisted app config (api creds, target group, session path).
    config: crate::config::AppConfig,

    // Group dropdown + export bookkeeping.
    dialogs: Vec<DialogInfo>,
    cam_mul: HashMap<u64, Option<[f32; 4]>>,
    /// Camera→sRGB color matrix per photo id (RAW only; JPEG → absent) for the
    /// editor's WB Auto/Pick to neutralize through the matrix.
    cam_matrix: HashMap<u64, Option<[[f32; 4]; 3]>>,
    send_pending: Vec<u64>,
    send_failed: Vec<u64>,
    send_peer: Option<DialogInfo>,
    pending_exports: HashMap<u64, Vec<u8>>,

    // Temp export files handed to the telegram worker for the in-flight send;
    // removed best-effort once the send finishes or fails (Task 23).
    send_temp_paths: Vec<PathBuf>,
    send_started_at: Option<std::time::Instant>,
    /// Last logged adjustments per photo, so "action: edit" lines only fire when
    /// an edit actually changes the image (not on every slider tick).
    last_edit_log: std::collections::HashMap<u64, crate::image::types::Adjustments>,
    /// Pending settle-upgrade timer: after a 512px live render, this fires once
    /// the edit has gone quiet and re-renders the preview at 1024px.
    settle_source: Option<glib::SourceId>,
    // Send-footer bookkeeping: export order+names for "Preparing {i}/{n}", the
    // count of completed exports, album-chunk progress, and photos sent so far.
    send_jobs: Vec<(u64, String)>,
    send_jobs_done: usize,
    albums_remaining: usize,
    batch_total: usize,
    photos_sent: usize,
    // Pending 2s re-enable of the Send button after a failed send (Task 23);
    // cancelled on success so it can't fight the immediate re-enable.
    send_backoff: Option<glib::SourceId>,

    // Debounced preview re-render bookkeeping.
    render_debounce: Option<glib::SourceId>,
    render_gen: u64,

    /// Decoded base of the ACTIVE photo (id-keyed), cached so slider edits render
    /// from memory instead of re-decoding the source every time. Dropped when the
    /// active photo changes/removes to return the memory.
    active_base: Option<(u64, Arc<dyn Base>)>,
    /// `(id, rotation)` the active photo's WB sample was last taken at. The WB
    /// sample is DISPLAY-oriented; a rotate must re-sample it (the controller
    /// can't re-decode, so it re-runs the cheap box-filter downscale from the
    /// cached base) or the pick/auto would read a stale orientation.
    last_wb_rotation: Option<(u64, u8)>,

    /// Weak self-handle so async (debounce) callbacks can reach back in.
    ctl: Option<Weak<RefCell<AppController>>>,
}

impl AppController {
    pub fn new(state: Arc<RwLock<AppState>>, window: adw::ApplicationWindow) -> Self {
        // Telegram worker: commands out, events back. Credentials and the session
        // path come from the persisted TOML config.
        let (events_tx, events_rx) = channel::<TEvent>();
        let config = crate::config::AppConfig::load();
        let telegram_cmd = crate::telegram::worker::spawn(
            TelegramWorkerConfig {
                session_path: config.session_path.clone(),
                api_id: config.api_id,
                api_hash: config.api_hash.clone(),
            },
            events_tx,
        );

        // Image pool + result channel.
        let pool = ImagePool::new();
        let (ui_tx, ui_rx) = channel::<UiEvent>();

        // Screen-originated AppEvents (login buttons, editor sliders, grid toggles).
        let (screen_tx, screen_rx) = channel::<AppEvent>();

        let stack = gtk4::Stack::new();
        let toast = Toast::new();
        if config.api_id == 0 || config.api_hash.is_empty() {
            // No credentials configured: the worker will fail to connect. Point the
            // user at the config file rather than a cryptic connect error.
            toast.show(&format!(
                "Set api_id / api_hash in {}",
                crate::config::AppConfig::path().display()
            ));
        }

        let login_on = {
            let tx = screen_tx.clone();
            Box::new(move |ev: AppEvent| {
                let _ = tx.send(ev);
            }) as Box<dyn Fn(AppEvent) + Send + 'static>
        };
        let login = LoginScreen::new(Arc::clone(&state), login_on);

        let main_on = {
            let tx = screen_tx.clone();
            move |id: u64, selected: bool| {
                let _ = tx.send(AppEvent::PhotoSelected { id, selected });
            }
        };
        let main_screen = MainScreen::new(main_on);

        // Editor requires Send + Sync; wrap the sender in a Mutex.
        let editor_on = {
            let tx = Arc::new(Mutex::new(screen_tx));
            Arc::new(move |ev: AppEvent| {
                if let Ok(guard) = tx.lock() {
                    let _ = guard.send(ev);
                }
            }) as Arc<dyn Fn(AppEvent) + Send + Sync + 'static>
        };
        let editor = EditorScreen::new(Arc::clone(&state), editor_on);

        let ctl = Self {
            state,
            pool,
            telegram_cmd,
            telegram_events: events_rx,
            ui_events: ui_rx,
            stack,
            login,
            main_screen,
            editor,
            toast,
            row_map: HashMap::new(),
            next_photo_id: 0,
            window,
            ui_events_sender: ui_tx,
            screen_events: screen_rx,
            config,
            dialogs: Vec::new(),
            cam_mul: HashMap::new(),
            cam_matrix: HashMap::new(),
            send_pending: Vec::new(),
            send_failed: Vec::new(),
            send_peer: None,
            pending_exports: HashMap::new(),
            send_temp_paths: Vec::new(),
            send_started_at: None,
            last_edit_log: std::collections::HashMap::new(),
            settle_source: None,
            send_jobs: Vec::new(),
            send_jobs_done: 0,
            albums_remaining: 0,
            batch_total: 0,
            photos_sent: 0,
            send_backoff: None,
            render_debounce: None,
            render_gen: 0,
            active_base: None,
            last_wb_rotation: None,
            ctl: None,
        };

        ctl.stack.add_named(&ctl.login.root, Some("login"));
        ctl.stack.add_named(&ctl.main_screen.root, Some("main"));
        ctl.stack.add_named(&ctl.editor.root, Some("editor"));
        ctl.stack.set_visible_child_name("login");

        // Ask the worker whether the stored session is already authorized, so the
        // login screen reflects reality on boot.
        let _ = ctl.telegram_cmd.send(TCommand::CheckAuth);

        ctl
    }

    /// Store the Rc<RefCell<Self>> handle (weak, so no cycle) and wire widget
    /// signals that need to reach back into the controller.
    pub fn setup(&mut self, ctl: Rc<RefCell<Self>>) {
        self.ctl = Some(Rc::downgrade(&ctl));

        // Grid row click → open the editor for that photo.
        if let Some(sel) = self
            .main_screen
            .grid
            .model()
            .and_then(|m| m.downcast::<gtk4::SingleSelection>().ok())
        {
            let ctl = Rc::clone(&ctl);
            sel.connect_selected_notify(move |sel| {
                let idx = sel.selected();
                // Defer to idle: this signal fires SYNCHRONOUSLY when the model
                // changes (e.g. rows removed after a send fires g_list_store_remove
                // → selected_notify). Calling borrow_mut() here would re-borrow the
                // controller while poll() already holds it → "RefCell already
                // borrowed" panic. Run on the next main-loop iteration instead.
                let ctl = Rc::clone(&ctl);
                glib::idle_add_local_once(move || {
                    ctl.borrow_mut().on_grid_selected(idx);
                });
            });
        }

        // Upload zone (opens the file picker) / Reset / Logout / Send buttons.
        let upload_ctl = Rc::clone(&ctl);
        self.main_screen
            .upload_zone
            .connect_clicked(move |_| upload_ctl.borrow_mut().on_load());

        let reset_ctl = Rc::clone(&ctl);
        self.main_screen
            .reset_button
            .connect_clicked(move |_| reset_ctl.borrow_mut().on_reset());

        let logout_ctl = Rc::clone(&ctl);
        self.main_screen
            .logout_button
            .connect_clicked(move |_| logout_ctl.borrow_mut().on_logout());

        let send_ctl = Rc::clone(&ctl);
        self.main_screen
            .send_button
            .connect_clicked(move |_| send_ctl.borrow_mut().on_send());

        // Drag-and-drop of files onto the upload zone (photoup UploadZone
        // `onDrop`). `GdkFileList` deserializes the dropped `text/uri-list`
        // (file-manager drags) into paths; `on_paths` filters to image types.
        let zone = self.main_screen.upload_zone.clone();
        let drop_target = gtk4::DropTarget::new(
            gtk4::gdk::FileList::static_type(),
            gtk4::gdk::DragAction::COPY,
        );
        {
            let zone = zone.clone();
            drop_target.connect_enter(move |_, _, _| {
                zone.add_css_class("drag-over");
                gtk4::gdk::DragAction::COPY
            });
        }
        drop_target.connect_leave(move |_| {
            zone.remove_css_class("drag-over");
        });
        let drop_ctl = Rc::clone(&ctl);
        drop_target.connect_drop(move |_, value, _, _| {
            let Some(file_list) = value.get::<gtk4::gdk::FileList>().ok() else {
                return false;
            };
            let paths: Vec<PathBuf> = file_list.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }
            drop_ctl.borrow_mut().on_paths(paths);
            true
        });
        self.main_screen.upload_zone.add_controller(drop_target);

        // Ctrl+V paste anywhere in the window (photoup UploadZone `onPaste`):
        // files copied from a file manager, or an image copied in a browser.
        let paste_ctl = Rc::clone(&ctl);
        let key = gtk4::EventControllerKey::new();
        key.connect_key_pressed(move |_, keyval, _, state| {
            let is_v = keyval == gtk4::gdk::Key::v || keyval == gtk4::gdk::Key::V;
            if is_v && state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                paste_ctl.borrow_mut().on_paste();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(key);

        // Window-level keyboard navigation (photoup App/EditorPanel `onKeyDown`):
        // Ctrl+A/Cmd+A toggles select-all, Enter opens the editor for the selected
        // cell, ArrowRight/ArrowLeft navigate the active photo in the editor, and
        // Escape closes the editor. Added after the Ctrl+V controller, so keys the
        // paste handler claims (Stop) never reach it.
        self.wire_keyboard(Rc::clone(&ctl));

        // Dev mode (PHOTOUP2_DEV=1): skip Telegram auth and auto-load ./samples/*
        // so the grid/editor can be exercised visually without a real session.
        if std::env::var("PHOTOUP2_DEV").is_ok() {
            reduce(
                &mut *self.state.write().unwrap(),
                AppEvent::Auth(AuthEvent::Success),
            );
            let ctl = Rc::clone(&ctl);
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                let mut ctl = ctl.borrow_mut();
                if let Ok(rd) = std::fs::read_dir("samples") {
                    for e in rd.flatten() {
                        let p = e.path();
                        if is_image_path(&p) {
                            ctl.add_photo(p);
                        }
                    }
                }
            });
        }
    }

    /// Install the window-level keyboard navigation (photoup `App.onKeyDown` +
    /// `EditorPanel.onKeyDown`): Ctrl+A/Cmd+A toggles select-all on the grid,
    /// Enter opens the editor for the selected cell, ArrowRight/ArrowLeft
    /// navigate the active photo in the editor, and Escape closes the editor.
    fn wire_keyboard(&mut self, ctl: Rc<RefCell<Self>>) {
        let key_ctl = Rc::clone(&ctl);
        let key = gtk4::EventControllerKey::new();
        // Capture phase: run BEFORE the focused widget, so arrows navigate the
        // editor even when a Scale/slider has focus (photoup's EditorPanel arrows
        // navigate over a focused range input — it only guards text inputs). The
        // focus guard below returns Proceed for text entries, so their keys (text
        // select-all, caret movement) are never stolen.
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, keyval, _, state| {
            let mut ctl = key_ctl.borrow_mut();
            ctl.on_key(keyval, state)
        });
        self.window.add_controller(key);
    }

    /// Dispatch a window key press to the matching action. Returns Stop for the
    /// keys this controller handles, Proceed otherwise (let GTK / child widgets
    /// keep unclaimed keys).
    fn on_key(
        &mut self,
        keyval: gtk4::gdk::Key,
        state: gtk4::gdk::ModifierType,
    ) -> glib::Propagation {
        use gtk4::gdk::{Key, ModifierType};
        // Focus guard: a focused text entry / text view / other editable keeps
        // its keys (Ctrl+A = select-all-text, arrows = caret movement). The login
        // screen's phone/code/password entries are `Entry`s.
        if self.focus_in_text() {
            return glib::Propagation::Proceed;
        }
        // Ctrl on Linux/Windows; Ctrl or Cmd on macOS (GDK reports Cmd as META).
        // Gated to macOS: on X11 `META_MASK` can alias Alt (Mod1), which must not
        // trigger select-all. Mirrors photoup's `event.ctrlKey || event.metaKey`.
        let ctrl = state.contains(ModifierType::CONTROL_MASK)
            || (cfg!(target_os = "macos") && state.contains(ModifierType::META_MASK));
        match keyval {
            k if (k == Key::a || k == Key::A) && ctrl => {
                self.on_select_all_toggle();
                // Always claim Ctrl+A (photoup preventDefaults it even with no
                // photos loaded).
                glib::Propagation::Stop
            }
            k if k == Key::Return || k == Key::KP_Enter => {
                // Stop only if we actually opened the editor; otherwise let GTK /
                // the focused widget keep Return (e.g. activate the grid cell).
                if self.on_key_enter() {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
            k if k == Key::Right => {
                if self.on_key_arrow(1) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
            k if k == Key::Left => {
                if self.on_key_arrow(-1) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
            k if k == Key::Escape => {
                if self.on_key_escape() {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
            // Keyboard fine-tune while the editor is open: Q/W EV, A/S warmth,
            // Z/X tint (Ctrl+A select-all is caught above; plain A is warmth-down).
            k if matches!(
                k,
                Key::q
                    | Key::Q
                    | Key::w
                    | Key::W
                    | Key::a
                    | Key::A
                    | Key::s
                    | Key::S
                    | Key::z
                    | Key::Z
                    | Key::x
                    | Key::X
            ) =>
            {
                if self.on_fine_tune(keyval) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
            _ => glib::Propagation::Proceed,
        }
    }

    /// Keyboard fine-tune (editor open): each keypress nudges the corresponding
    /// slider by its step, which fires the slider's existing `PhotoEdit` — the EV
    /// keys switch exposure to Manual (like dragging the slider), warmth/tint just
    /// adjust their value. Returns true if a key was handled.
    fn on_fine_tune(&mut self, keyval: gtk4::gdk::Key) -> bool {
        use gtk4::gdk::Key;
        if self.state.read().unwrap().active_photo.is_none() {
            return false; // editor not open
        }
        let (dir, which) = match keyval {
            Key::q | Key::Q => (-1i8, 0),
            Key::w | Key::W => (1i8, 0),
            Key::a | Key::A => (-1i8, 1),
            Key::s | Key::S => (1i8, 1),
            Key::z | Key::Z => (-1i8, 2),
            Key::x | Key::X => (1i8, 2),
            _ => return false,
        };
        match which {
            0 => self.editor.fine_tune_ev(dir),
            1 => self.editor.fine_tune_wb(dir),
            _ => self.editor.fine_tune_tint(dir),
        }
        true
    }

    /// True when the focused widget is a text entry / text view / other editable,
    /// in which case keyboard navigation must not steal the keys. `Entry` and
    /// `TextView` are checked explicitly, plus the `Editable` interface (covers
    /// `SpinButton`, `PasswordEntry`, `SearchEntry`, …) for safety.
    fn focus_in_text(&self) -> bool {
        let Some(f) = gtk4::prelude::GtkWindowExt::focus(&self.window) else {
            return false;
        };
        f.is::<gtk4::Entry>() || f.is::<gtk4::TextView>() || f.is::<gtk4::Editable>()
    }

    /// Drain all channels and reflect state in the UI. Called every 50ms.
    pub fn poll(&mut self) {
        // Drain the pool's per-job completion signals (a `()` per finished job).
        // We don't need the payloads — the real results arrive on `ui_events` —
        // but leaving them unread leaks 8 bytes/job and leaves the API dead.
        while self.pool.try_wait_one() {}

        while let Ok(ev) = self.screen_events.try_recv() {
            self.handle_app_event(ev);
        }
        while let Ok(ev) = self.telegram_events.try_recv() {
            self.handle_telegram_event(ev);
        }
        while let Ok(ev) = self.ui_events.try_recv() {
            self.handle_ui_event(ev);
        }
        self.refresh_screens();
    }

    fn handle_app_event(&mut self, ev: AppEvent) {
        // A render must be scheduled only after the reducer stores an edit.
        // Otherwise a rotate/crop event submits a job with the PREVIOUS
        // adjustments; its late result can then overwrite the editor's new
        // display dimensions and make the crop overlay appear split-brained.
        let edited_id = match &ev {
            AppEvent::PhotoEdit { id, .. } => Some(*id),
            _ => None,
        };
        match &ev {
            AppEvent::Auth(AuthEvent::PhoneRequested { phone }) => {
                let _ = self.telegram_cmd.send(TCommand::RequestCode {
                    phone: phone.clone(),
                });
            }
            AppEvent::Auth(AuthEvent::CodeEntered { code }) => {
                let _ = self
                    .telegram_cmd
                    .send(TCommand::SubmitCode { code: code.clone() });
            }
            AppEvent::Auth(AuthEvent::PasswordEntered { password }) => {
                let _ = self.telegram_cmd.send(TCommand::SubmitPassword {
                    password: password.clone(),
                });
            }
            AppEvent::PhotoEdit { id, adjustments } => {
                // Keep the grid's EV badge in sync (auto → autoEV, else manual EV).
                if let Some(row) = self.row_map.get(id) {
                    let ev = match adjustments.exposure_mode {
                        ExposureMode::Manual => adjustments.exposure_ev,
                        _ => {
                            let st = self.state.read().unwrap();
                            st.photos
                                .iter()
                                .find(|p| p.id == *id)
                                .map(|p| p.auto_ev)
                                .unwrap_or(0.0)
                        }
                    };
                    row.set_ev(ev);
                }
            }
            AppEvent::Nav { delta } => return self.handle_nav(*delta),
            AppEvent::Toast(msg) => {
                self.toast.show(msg);
                return;
            }
            AppEvent::RejectActive => return self.handle_reject(),
            AppEvent::ActivePhoto { index: None } => {
                log::info!("action: close editor");
                // The editor closed: drop the active photo's decoded base (memory
                // back) and clear the editor's per-photo preview/matrix data.
                self.release_active_photo_data();
                // Deselect the grid row so clicking the same photo again re-opens
                // the editor (SingleSelection won't re-emit if already selected).
                // `deselect_grid` runs synchronously (safe inside this borrow: the
                // `selected_notify` handler only schedules an idle callback).
                self.deselect_grid();
            }
            _ => {}
        }
        reduce(&mut *self.state.write().unwrap(), ev);
        if let Some(id) = edited_id {
            self.schedule_preview(id);
        }
    }

    fn handle_telegram_event(&mut self, ev: TEvent) {
        match ev {
            TEvent::AuthStep(AuthStep::Ready) => {
                log::info!("telegram: authenticated — loading dialogs");
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::Auth(AuthEvent::Success),
                );
                let _ = self.telegram_cmd.send(TCommand::LoadDialogs);
            }
            TEvent::AuthStep(_) => {}
            TEvent::CodeRequested => {
                log::info!("telegram: login code requested");
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::Auth(AuthEvent::PhoneRequested {
                        phone: String::new(),
                    }),
                );
            }
            TEvent::PasswordRequired { hint } => {
                log::info!("telegram: 2FA password required");
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::Auth(AuthEvent::CodeEntered {
                        code: String::new(),
                    }),
                );
                if let Some(h) = hint {
                    self.toast.show(&format!("2FA password required: {h}"));
                }
            }
            TEvent::AuthFailed(msg) => {
                log::warn!("telegram: auth failed: {msg}");
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::Auth(AuthEvent::Failure(msg.clone())),
                );
                self.toast.show(&msg);
            }
            TEvent::Dialogs(dialogs) => {
                log::info!("telegram: loaded {} dialogs", dialogs.len());
                self.populate_group_picker(dialogs);
            }
            TEvent::Sent { ok, failed } => {
                self.photos_sent += ok;
                if self.albums_remaining > 0 {
                    self.albums_remaining -= 1;
                }
                if self.albums_remaining > 0 {
                    return; // more album chunks still in flight
                }
                self.finish_send(Ok(()), failed);
            }
            TEvent::Error(msg) => {
                // If the worker errored mid-send (connect lost, upload failed),
                // finalize the batch as a failure — sent photos are removed, the
                // rest stay with their edits. Outside a send, just surface it.
                let was_sending = {
                    let st = self.state.read().unwrap();
                    st.sending
                };
                if was_sending {
                    self.finish_send(Err(msg.clone()), Vec::new());
                } else {
                    self.cleanup_send_temp_files();
                    self.toast.show(&msg);
                }
            }
        }
    }

    fn handle_ui_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::ThumbReady {
                id,
                rgba,
                size,
                full,
                auto_ev,
                black_point,
                histogram,
                cam_mul,
                meta,
            } => {
                self.cam_mul.insert(id, cam_mul);
                let e = AppEvent::PhotoThumbReady {
                    id,
                    rgba: rgba.clone(),
                    size,
                    full,
                    auto_ev,
                    histogram: histogram.clone(),
                    meta: Some(meta.clone()),
                };
                reduce(&mut *self.state.write().unwrap(), e);
                if let Some(row) = self.row_map.get(&id) {
                    row.set_texture(&crate::ui::util::rgba_to_texture(
                        &rgba,
                        size.0 as i32,
                        size.1 as i32,
                    ));
                    // A successful render clears any prior error badge so a re-render
                    // can recover a previously-failed photo. The EV badge turns on
                    // when the thumb is ready and |EV| > 0.05 (photoup). Show the
                    // EFFECTIVE EV (manual slider in Manual mode, autoEV otherwise) —
                    // a raw autoEV would mislead when the user set a manual EV.
                    row.set_error(false);
                    row.set_ev(self.effective_ev_for(id, auto_ev));
                    row.set_ready(true);
                }
                // Keep the editor's EV indicator in sync if this is the active photo.
                let is_active = {
                    let st = self.state.read().unwrap();
                    st.active_photo == self.index_of(id)
                };
                if is_active {
                    self.editor.set_ev(self.effective_ev_for(id, auto_ev));
                    self.editor.set_black_point(black_point);
                    // The editor may have opened before this photo finished its
                    // first decode, so the Image section's EXIF lines land here.
                    self.editor.set_meta(&meta);
                }
            }
            UiEvent::PreviewReady {
                id,
                rgba,
                size,
                full,
                auto_ev,
                black_point,
                histogram,
                cam_mul,
                cam_matrix,
                base,
                preview_gen,
            } => {
                if preview_gen != self.render_gen {
                    return; // superseded by a newer edit — drop the stale render
                }
                // Only the decode path carries these; render-only jobs pass `None`
                // so they must not clobber the values cached from the first decode.
                if let Some(cm) = cam_mul {
                    self.cam_mul.insert(id, Some(cm));
                }
                if cam_matrix.is_some() {
                    self.cam_matrix.insert(id, cam_matrix);
                }
                let e = AppEvent::PhotoThumbReady {
                    id,
                    rgba: rgba.clone(),
                    size,
                    full,
                    auto_ev,
                    histogram: histogram.clone(),
                    meta: None, // render-only: keep the metadata from the decode
                };
                reduce(&mut *self.state.write().unwrap(), e);
                let idx = self.index_of(id);
                let is_active = {
                    let st = self.state.read().unwrap();
                    st.active_photo == idx
                };
                let decoded = base.is_some();
                // Cache the freshly-decoded base so slider edits render from memory,
                // and kick a LINEAR WB sample (the pick/auto measure the cast from
                // the pre-tone base, not the tone-processed preview). Re-submitted
                // only on a decode/re-cache — a re-cache happens on photo switch,
                // so this never spams during slider drags.
                if is_active && let Some(arc) = base {
                    // The WB sample must be DISPLAY-oriented (so the editor's
                    // click→sample mapping stays direct); wrap the raw base with the
                    // photo's current rotation when sampling.
                    let rotation = {
                        let st = self.state.read().unwrap();
                        idx.and_then(|i| st.photos.get(i))
                            .map_or(0, |p| p.adjustments.rotation)
                    };
                    let arc_job = Arc::clone(&arc);
                    let tx = self.ui_events_sender.clone();
                    self.pool.submit(move || {
                        let (rgba, w, h) = run_wb_sample_job(arc_job, rotation);
                        let _ = tx.send(UiEvent::WbSample { id, rgba, w, h });
                    });
                    self.active_base = Some((id, arc));
                    self.last_wb_rotation = Some((id, rotation));
                } else if is_active && let Some((cid, arc)) = self.active_base.take() {
                    if cid == id {
                        // Render-only job (no re-decode): re-kick the WB sample only
                        // if the photo's rotation changed since the last sample — the
                        // cached base doesn't move, but its DISPLAY orientation does.
                        let rotation = {
                            let st = self.state.read().unwrap();
                            idx.and_then(|i| st.photos.get(i))
                                .map_or(0, |p| p.adjustments.rotation)
                        };
                        if self.last_wb_rotation != Some((id, rotation)) {
                            self.active_base = Some((id, Arc::clone(&arc)));
                            let arc_job = Arc::clone(&arc);
                            let tx = self.ui_events_sender.clone();
                            self.pool.submit(move || {
                                let (rgba, w, h) = run_wb_sample_job(arc_job, rotation);
                                let _ = tx.send(UiEvent::WbSample { id, rgba, w, h });
                            });
                            self.last_wb_rotation = Some((id, rotation));
                        } else {
                            self.active_base = Some((cid, arc));
                        }
                    }
                    // cid != id: stale cache for a photo that's no longer active —
                    // drop it (active_base stays None, like the submit_preview path).
                }
                if is_active {
                    self.editor
                        .set_preview(Some(&crate::ui::util::rgba_to_texture(
                            &rgba,
                            size.0 as i32,
                            size.1 as i32,
                        )));
                    self.editor.set_full_size(full);
                    self.editor.set_histogram(&histogram);
                    self.editor.set_ev(self.effective_ev_for(id, auto_ev));
                    self.editor.set_black_point(black_point);
                    // The decode path carries the camera matrix the editor's WB
                    // Auto/Pick need; render-only jobs keep the previously-set one.
                    if decoded {
                        self.editor.set_cam_matrix(cam_matrix);
                    }
                }
                // Keep the grid thumbnail in sync with the edited preview; state
                // now holds the edited render, so the row must show it too.
                if let Some(row) = self.row_map.get(&id) {
                    row.set_texture(&crate::ui::util::rgba_to_texture(
                        &rgba,
                        size.0 as i32,
                        size.1 as i32,
                    ));
                    // Same as ThumbReady: a successful render clears the error badge
                    // and refreshes the EV badge with the effective EV (manual slider
                    // in Manual mode, autoEV otherwise) — never the raw autoEV, which
                    // would mask a manual correction made in the editor.
                    row.set_error(false);
                    row.set_ev(self.effective_ev_for(id, auto_ev));
                    row.set_ready(true);
                }
            }
            UiEvent::WbSample { id, rgba, w, h } => {
                // Only the active photo's sample is useful; a stale one from a
                // photo the user already navigated away from must not clobber it.
                let is_active = {
                    let st = self.state.read().unwrap();
                    st.active_photo == self.index_of(id)
                };
                if is_active {
                    self.editor.set_wb_sample(rgba, w, h);
                }
            }
            UiEvent::ExportReady { id, jpeg, .. } => {
                self.note_export_done(id);
                self.pending_exports.insert(id, jpeg);
                self.maybe_send();
            }
            UiEvent::JobFailed { id, msg } => {
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::PhotoFailed {
                        id,
                        msg: msg.clone(),
                    },
                );
                self.cam_mul.remove(&id);
                // Mark the grid cell with the error badge (red outline + disabled
                // checkbox). A later successful ThumbReady/PreviewReady clears it.
                if let Some(row) = self.row_map.get(&id) {
                    row.set_error(true);
                }
                let is_active = {
                    let st = self.state.read().unwrap();
                    st.active_photo == self.index_of(id)
                };
                if is_active {
                    self.editor.cancel_pending_rotation();
                }
                // If this photo was part of an in-flight send, account for the
                // failure so the send can proceed with the remaining photos.
                if self.send_pending.contains(&id) && !self.send_failed.contains(&id) {
                    self.send_failed.push(id);
                    self.note_export_done(id);
                    self.maybe_send();
                }
                self.toast.show(&format!("Photo {id} failed: {msg}"));
            }
        }
    }

    /// Set stack visibility + login step + usage indicator + footer + nav from state.
    fn refresh_screens(&mut self) {
        let st = self.state.read().unwrap();
        let target = match &st.telegram.status {
            AuthStatus::Authenticated => {
                if st.active_photo.is_some() {
                    "editor"
                } else {
                    "main"
                }
            }
            _ => "login",
        };
        // Log only on an actual screen change (this runs every 50 ms poll).
        if self.stack.visible_child_name().as_deref() != Some(target) {
            log::info!("screen: {target}");
        }
        match &st.telegram.status {
            AuthStatus::Authenticated => {
                if st.active_photo.is_some() {
                    self.stack.set_visible_child_name("editor");
                } else {
                    self.stack.set_visible_child_name("main");
                }
            }
            status => {
                self.login.render(status);
                self.stack.set_visible_child_name("login");
            }
        }
        let photos_len = st.photos.len();
        let selected_ready = st
            .photos
            .iter()
            .filter(|p| p.selected && matches!(p.status, PhotoStatus::Ready))
            .count();
        let sending = st.sending;
        drop(st);
        self.update_usage_indicator();
        self.main_screen.reset_button.set_sensitive(photos_len > 0);
        self.update_send_footer(selected_ready, sending);
        self.update_editor_nav();
    }

    /// photoup `UsageIndicator`: a "● Processing {name} ({n} queued)" line shown
    /// only while photos are being decoded/processed.
    fn update_usage_indicator(&mut self) {
        let pending: Vec<String> = {
            let st = self.state.read().unwrap();
            st.photos
                .iter()
                .filter(|p| matches!(p.status, PhotoStatus::Queued | PhotoStatus::Processing))
                .map(|p| crate::ui::util::file_name(&p.path))
                .collect()
        };
        if pending.is_empty() {
            self.main_screen.usage_label.set_text("");
            self.main_screen.usage_row.set_visible(false);
        } else {
            let extra = if pending.len() > 1 {
                format!(" ({} queued)", pending.len() - 1)
            } else {
                String::new()
            };
            // The amber pulsing "●" is a separate widget (`.usage-dot`) in the
            // row; this label only carries the muted mono text.
            self.main_screen
                .usage_label
                .set_text(&format!("Processing {}{extra}", pending[0]));
            self.main_screen.usage_row.set_visible(true);
        }
    }

    /// photoup footer: "Send {n} selected" → "Preparing {i}/{n} {name}" → a
    /// progress bar + "Sending {p}%". The bar only shows during the transfer phase.
    fn update_send_footer(&mut self, selected_ready: usize, sending: bool) {
        let send = &self.main_screen.send_button;
        let bar = &self.main_screen.send_progress;
        if sending {
            if self.send_jobs_done < self.send_jobs.len() {
                let done = self.send_jobs_done;
                let total = self.send_jobs.len();
                let name = self
                    .send_jobs
                    .get(done)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_default();
                send.set_label(&format!("Preparing {done}/{total} · {name}"));
                send.set_sensitive(false);
                bar.set_visible(false);
            } else {
                let frac = if self.batch_total > 0 {
                    (self.photos_sent as f64 / self.batch_total as f64).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                send.set_label(&format!("Sending {:.0}%", frac * 100.0));
                send.set_sensitive(false);
                bar.set_visible(true);
                bar.set_fraction(frac);
            }
        } else {
            let backoff = self.send_backoff.is_some();
            send.set_label(&format!("Send {selected_ready} selected"));
            send.set_sensitive(selected_ready > 0 && !backoff);
            bar.set_visible(false);
        }
    }

    fn update_editor_nav(&mut self) {
        let st = self.state.read().unwrap();
        let has_prev = st.active_photo.map_or(false, |i| i > 0);
        let has_next = st.active_photo.map_or(false, |i| i + 1 < st.photos.len());
        self.editor.set_nav(has_prev, has_next);
    }

    fn populate_group_picker(&mut self, dialogs: Vec<DialogInfo>) {
        self.dialogs = dialogs;
        let titles: Vec<String> = self.dialogs.iter().map(|d| d.title.clone()).collect();
        let refs: Vec<&str> = titles.iter().map(|s| s.as_str()).collect();
        let model = gtk4::StringList::new(&refs);
        let expr = gtk4::PropertyExpression::new(
            gtk4::StringObject::static_type(),
            Option::<gtk4::Expression>::None,
            "string",
        );
        self.main_screen.group_dropdown.set_expression(Some(&expr));
        self.main_screen.group_dropdown.set_model(Some(&model));
        if !self.dialogs.is_empty() {
            // Pre-select the persisted target group if it's still in the dialog list.
            let idx = self
                .dialogs
                .iter()
                .position(|d| Some(d.id) == self.config.target_peer_id);
            match idx {
                Some(i) => self.main_screen.group_dropdown.set_selected(i as u32),
                None => self.main_screen.group_dropdown.set_selected(0),
            }
        }
    }

    // ---- File loading -----------------------------------------------------

    fn on_load(&mut self) {
        let filter = gtk4::FileFilter::new();
        // `add_suffix` matches case-insensitively — real camera files are
        // uppercase (`DSC_4858.NEF`, `IMG_7833.CR2`), and pattern globs are
        // case-sensitive, which hid RAW files from the dialog before.
        for ext in IMAGE_EXTS {
            filter.add_suffix(&format!(".{ext}"));
        }
        filter.set_name(Some("Photos"));
        let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
        filters.append(&filter);

        let dialog = gtk4::FileDialog::builder().title("Load photos").build();
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&filter));

        let Some(w) = self.ctl.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        dialog.open_multiple(Some(&self.window), None::<&gio::Cancellable>, move |res| {
            let Ok(model) = res else { return }; // cancelled
            let mut ctl = w.borrow_mut();
            for i in 0..model.n_items() {
                if let Some(file) = model.item(i).and_then(|o| o.downcast::<gio::File>().ok()) {
                    if let Some(path) = file.path() {
                        ctl.add_photo(path);
                    }
                }
            }
        });
    }

    /// Add photos from arbitrary paths, silently dropping any that aren't an
    /// accepted image type. Entry point for drag-drop and Ctrl+V paste (photoup
    /// UploadZone `handleFiles` → `isPhoto` filter).
    pub fn on_paths(&mut self, paths: Vec<PathBuf>) {
        for path in filter_photo_paths(paths) {
            self.add_photo(path);
        }
    }

    /// Ctrl+V paste → add photos from the clipboard. Files copied from a file
    /// manager surface as `text/uri-list` and are added directly; an image
    /// copied in a browser (e.g. a screenshot) surfaces as a `GdkTexture` and is
    /// saved to a temp PNG before being loaded like any other file (photoup
    /// UploadZone `onPaste` prefers `files`, then falls back to image `items`).
    /// Runs on the GTK main thread (key events); the async clipboard reads return
    /// to the same thread, so `borrow_mut` below is always on the main loop.
    fn on_paste(&mut self) {
        let clipboard = gtk4::prelude::WidgetExt::display(&self.window).clipboard();
        let Some(w) = self.ctl.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        // File-manager copies are advertised as text/uri-list → read as a FileList.
        if clipboard.formats().contain_mime_type("text/uri-list") {
            let clipboard = clipboard.clone();
            clipboard.read_value_async(
                gtk4::gdk::FileList::static_type(),
                glib::Priority::DEFAULT,
                None::<&gio::Cancellable>,
                move |res| {
                    let Ok(value) = res else { return };
                    let paths: Vec<PathBuf> = value
                        .get::<gtk4::gdk::FileList>()
                        .map(|fl| fl.files().iter().filter_map(|f| f.path()).collect())
                        .unwrap_or_default();
                    if !paths.is_empty() {
                        w.borrow_mut().on_paths(paths);
                    }
                },
            );
        } else {
            // Browser image copies surface as a texture → temp PNG, then load.
            let clipboard = clipboard.clone();
            clipboard.read_texture_async(None::<&gio::Cancellable>, move |res| {
                let Ok(Some(texture)) = res else { return };
                let bytes = texture.save_to_png_bytes();
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let path = std::env::temp_dir().join(format!("photoup2-paste-{ts}.png"));
                if std::fs::write(&path, &bytes).is_ok() {
                    w.borrow_mut().add_photo(path);
                }
            });
        }
    }

    /// Load a photo file into the grid: create its `PhotoState` + row and submit
    /// a thumbnail decode. The file picker, drag-drop, and Ctrl+V paste all feed
    /// through this (photoup `addPhotos`).
    pub fn add_photo(&mut self, path: PathBuf) {
        let id = self.next_photo_id;
        self.next_photo_id += 1;
        let source_type = source_type_for(&path);
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        log::info!("photo added: id={id} {name} ({source_type:?})");
        let photo = PhotoState {
            id,
            path: path.clone(),
            source_type,
            adjustments: Adjustments::default(),
            auto_ev: 0.0,
            full_size: None,
            meta: PhotoMeta::default(),
            thumb: None,
            thumb_size: None,
            histogram: None,
            status: PhotoStatus::Queued,
            selected: true, // photoup defaults checkboxes to checked
        };
        reduce(
            &mut *self.state.write().unwrap(),
            AppEvent::PhotosAdded(vec![photo]),
        );
        let row = PhotoRow::new(id);
        // The grid's checkbox renders from the ROW's `selected` property (not the
        // state's), so mirror the state default (checked) or the check lies until
        // the first toggle.
        row.set_selected(true);
        row.set_name(crate::ui::util::file_name(&path));
        row.set_is_raw(source_type == SourceType::Raw);
        row.set_ev(0.0);
        row.set_ready(false);
        self.main_screen.grid_store.append(&row);
        self.row_map.insert(id, row);
        self.submit_decode_thumb(id, source_type, path);
    }

    // ---- Pool jobs --------------------------------------------------------

    fn submit_decode_thumb(&mut self, id: u64, source_type: SourceType, path: PathBuf) {
        let tx = self.ui_events_sender.clone();
        self.pool.submit(move || {
            // Panic-safe: a panicking decode must surface as JobFailed, or the
            // photo stays "queued" forever with no feedback.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_thumb_job(&path, source_type)
            }))
            .unwrap_or_else(|_| Err("thumbnail job panicked".to_string()));
            let _ = tx.send(match result {
                Ok((rgba, size, full, auto_ev, black_point, hist, cam, meta)) => UiEvent::ThumbReady {
                    id,
                    rgba,
                    size,
                    full,
                    auto_ev,
                    black_point,
                    histogram: hist,
                    cam_mul: cam,
                    meta,
                },
                Err(msg) => UiEvent::JobFailed { id, msg },
            });
        });
    }

    fn schedule_preview(&mut self, id: u64) {
        // Only the active photo is worth re-rendering (a stale edit could be a
        // stray event for a photo the user already navigated away from).
        if self.index_of(id) != self.state.read().unwrap().active_photo {
            return;
        }
        if let Some(src) = self.render_debounce.take() {
            src.remove();
        }
        // A new edit cancels any pending settle-upgrade (we're back in live mode).
        if let Some(src) = self.settle_source.take() {
            src.remove();
        }
        let preview_gen = self.render_gen + 1;
        self.render_gen = preview_gen;
        let Some(w) = self.ctl.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        let src = glib::timeout_add_local(
            std::time::Duration::from_millis(PREVIEW_DEBOUNCE_MS),
            move || {
                w.borrow_mut().submit_preview(preview_gen);
                glib::ControlFlow::Break
            },
        );
        self.render_debounce = Some(src);
    }

    fn submit_preview(&mut self, preview_gen: u64) {
        if preview_gen != self.render_gen {
            return; // a newer edit superseded this timer
        }
        self.render_debounce = None;
        let (id, source_type, adjustments, path) = {
            let st = self.state.read().unwrap();
            let Some(i) = st.active_photo else { return };
            let Some(p) = st.photos.get(i) else { return };
            (p.id, p.source_type, p.adjustments, p.path.clone())
        };
        // Effective-edit log: only when the settled adjustments actually changed
        // (this is the debounced render that hits the image — slider drags coalesce
        // to one commit here, not one log per tick).
        let name = crate::ui::util::file_name(&path);
        let changed = self
            .last_edit_log
            .get(&id)
            .map_or(true, |last| *last != adjustments);
        if changed {
            log::info!(
                "action: edit {name} → mode={:?} EV={:+.2} wb={:+.2} hue={:+.2} sat={:+.2} bp={:+.2}{}",
                adjustments.exposure_mode,
                adjustments.exposure_ev,
                adjustments.wb_offset,
                adjustments.hue,
                adjustments.saturation,
                adjustments.black_point,
                // The black point is only the slider's value while it is manual;
                // in auto modes the render derives it (and `set_black_point`
                // shows the derived value in the panel).
                if adjustments.black_point_auto { " (auto)" } else { "" }
            );
            self.last_edit_log.insert(id, adjustments);
        }
        let tx = self.ui_events_sender.clone();

        // Fast path: the active photo's decoded base is cached — render from memory
        // (no re-decode). This is what makes slider edits snappy.
        if let Some((cached_id, base)) = self.active_base.take() {
            if cached_id == id {
                self.active_base = Some((cached_id, Arc::clone(&base)));
                self.pool.submit(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_render_job(base, &adjustments, LIVE_EDGE, source_type)
                    }))
                    .unwrap_or_else(|_| Err("preview render panicked".to_string()));
                    let _ = tx.send(match result {
                        Ok((rgba, size, full, auto_ev, black_point, hist)) => UiEvent::PreviewReady {
                            id,
                            rgba,
                            size,
                            full,
                            auto_ev,
                            black_point,
                            histogram: hist,
                            cam_mul: None,
                            cam_matrix: None,
                            base: None,
                            preview_gen,
                        },
                        Err(msg) => UiEvent::JobFailed { id, msg },
                    });
                });
                // Live render was 512px — once the edit settles (~400ms of no new
                // input), upgrade to the sharp 1024px preview. The settle's OWN
                // PreviewReady does NOT schedule another settle, so this fires once
                // per live render — not in an endless loop.
                if let Some(src) = self.settle_source.take() {
                    src.remove();
                }
                let settle_gen = preview_gen;
                if let Some(w) = self.ctl.as_ref().and_then(|w| w.upgrade()) {
                    let src = glib::timeout_add_local_once(
                        std::time::Duration::from_millis(400),
                        move || {
                            let mut ctl = w.borrow_mut();
                            ctl.settle_source = None;
                            ctl.submit_settle(settle_gen);
                        },
                    );
                    self.settle_source = Some(src);
                }
                return;
            }
            // Stale cache (active photo changed) — drop it and decode+render below.
            self.active_base = None;
        }

        // Decode + render path (first render, or cache miss). Returns the base so
        // the controller can cache it for subsequent slider edits.
        self.pool.submit(move || {
            // Panic-safe, same rationale as the thumbnail/export jobs.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_preview_job(&path, source_type, &adjustments)
            }))
            .unwrap_or_else(|_| Err("preview job panicked".to_string()));
            let _ = tx.send(match result {
                Ok((rgba, size, full, auto_ev, black_point, hist, cam, cam_matrix, base)) => {
                    UiEvent::PreviewReady {
                        id,
                        rgba,
                        size,
                        full,
                        auto_ev,
                        black_point,
                        histogram: hist,
                        cam_mul: cam,
                        cam_matrix,
                        base: Some(base),
                        preview_gen,
                    }
                }
                Err(msg) => UiEvent::JobFailed { id, msg },
            });
        });
    }

    /// Settle-upgrade: once the edit has gone quiet, re-render the active photo's
    /// preview at the sharp FINAL_EDGE (1024) from the cached base — no re-decode.
    /// The live 512px render stays on screen until this lands.
    fn submit_settle(&mut self, preview_gen: u64) {
        if preview_gen != self.render_gen {
            return; // a newer edit superseded this settle
        }
        let (id, adjustments, source_type) = {
            let st = self.state.read().unwrap();
            let Some(i) = st.active_photo else { return };
            let Some(p) = st.photos.get(i) else { return };
            (p.id, p.adjustments, p.source_type)
        };
        let Some((cached_id, base)) = self.active_base.take() else {
            return;
        };
        if cached_id != id {
            self.active_base = None;
            return;
        }
        self.active_base = Some((cached_id, Arc::clone(&base)));
        let tx = self.ui_events_sender.clone();
        self.pool.submit(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_render_job(base, &adjustments, FINAL_EDGE, source_type)
            }))
            .unwrap_or_else(|_| Err("settle render panicked".to_string()));
            let _ = tx.send(match result {
                Ok((rgba, size, full, auto_ev, black_point, hist)) => UiEvent::PreviewReady {
                    id,
                    rgba,
                    size,
                    full,
                    auto_ev,
                    black_point,
                    histogram: hist,
                    cam_mul: None,
                    cam_matrix: None,
                    base: None,
                    preview_gen,
                },
                Err(msg) => UiEvent::JobFailed { id, msg },
            });
        });
    }

    fn submit_export(
        &mut self,
        id: u64,
        source_type: SourceType,
        adjustments: Adjustments,
        cam_mul: Option<[f32; 4]>,
        path: PathBuf,
    ) {
        let tx = self.ui_events_sender.clone();
        self.pool.submit(move || {
            // The pool's worker catch_unwinds panics silently; a panicking export
            // would never emit ExportReady/JobFailed and the send's all_accounted
            // wait would hang forever. Report the panic as a JobFailed instead.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_export_job(&path, source_type, &adjustments, cam_mul)
            }))
            .unwrap_or_else(|_| Err("export job panicked".to_string()));
            let _ = tx.send(match result {
                Ok((jpeg, width, height)) => UiEvent::ExportReady {
                    id,
                    jpeg,
                    width,
                    height,
                },
                Err(msg) => UiEvent::JobFailed { id, msg },
            });
        });
    }

    // ---- Send flow --------------------------------------------------------

    fn on_send(&mut self) {
        let (auth_ok, sending, selected) = {
            let st = self.state.read().unwrap();
            (
                st.telegram.status == AuthStatus::Authenticated,
                st.sending,
                st.photos
                    .iter()
                    .filter(|p| p.selected && matches!(p.status, PhotoStatus::Ready))
                    .map(|p| p.id)
                    .collect::<Vec<_>>(),
            )
        };
        if !auth_ok {
            self.toast.show("Log in to Telegram first");
            return;
        }
        if sending {
            return;
        }
        if selected.is_empty() {
            self.toast.show("No ready photos selected");
            return;
        }
        let idx = self.main_screen.group_dropdown.selected() as usize;
        let Some(peer) = self.dialogs.get(idx).cloned() else {
            self.toast.show("Pick a target group");
            return;
        };
        // Persist the chosen target group so it's pre-selected next launch.
        self.config.target_peer_id = Some(peer.id);
        let _ = self.config.save();

        let jobs: Vec<(u64, SourceType, Adjustments, Option<[f32; 4]>, PathBuf)> = {
            let st = self.state.read().unwrap();
            st.photos
                .iter()
                .filter(|p| p.selected && matches!(p.status, PhotoStatus::Ready))
                .map(|p| {
                    (
                        p.id,
                        p.source_type,
                        p.adjustments,
                        self.cam_mul.get(&p.id).copied().flatten(),
                        p.path.clone(),
                    )
                })
                .collect()
        };
        if jobs.is_empty() {
            self.toast.show("No exportable photos selected");
            return;
        }
        log::info!(
            "send: exporting {} selected photos → {}",
            jobs.len(),
            peer.title
        );

        // Reset the footer bookkeeping for the new batch.
        self.send_jobs = jobs
            .iter()
            .map(|(id, .., path)| (*id, crate::ui::util::file_name(path)))
            .collect();
        self.send_jobs_done = 0;
        self.batch_total = jobs.len();
        self.photos_sent = 0;
        self.albums_remaining = 0;

        reduce(&mut *self.state.write().unwrap(), AppEvent::SendStarted);
        self.send_pending = jobs.iter().map(|(id, ..)| *id).collect();
        self.send_failed.clear();
        self.send_peer = Some(peer);
        self.pending_exports.clear();
        for (id, source_type, adjustments, cam_mul, path) in jobs {
            self.submit_export(id, source_type, adjustments, cam_mul, path);
        }
    }

    fn maybe_send(&mut self) {
        if self.send_pending.is_empty() {
            return;
        }
        // Fire once every selected photo is accounted for (exported OR failed).
        let all_accounted = self
            .send_pending
            .iter()
            .all(|id| self.pending_exports.contains_key(id) || self.send_failed.contains(id));
        if !all_accounted {
            return;
        }
        let Some(peer) = self.send_peer.take() else {
            return;
        };
        let pending = std::mem::take(&mut self.send_pending);
        self.send_failed.clear();

        let mut paths = Vec::new();
        for id in &pending {
            if let Some(jpeg) = self.pending_exports.remove(id) {
                let tmp = std::env::temp_dir().join(format!("photoup2-{id}.jpg"));
                if std::fs::write(&tmp, &jpeg).is_ok() {
                    paths.push(tmp);
                }
            }
        }
        self.pending_exports.clear();
        if paths.is_empty() {
            self.finish_send(Err("no export written".into()), Vec::new());
            return;
        }
        // Remember the temp files so the worker's send can be followed up with a
        // best-effort cleanup once it finishes or fails (Task 23).
        self.send_temp_paths = paths.clone();
        self.send_started_at = Some(std::time::Instant::now());
        // Albums are chunked to ≤10 photos (Telegram's MULTI_MEDIA_TOO_LONG limit).
        // The worker processes the chunks in order, emitting one Sent per album.
        let chunks: Vec<Vec<PathBuf>> = paths.chunks(10).map(|c| c.to_vec()).collect();
        self.albums_remaining = chunks.len();
        for chunk in chunks {
            let _ = self.telegram_cmd.send(TCommand::SendAlbum {
                peer_id: peer.id,
                access_hash: peer.access_hash,
                paths: chunk,
                caption: None,
            });
        }
    }

    /// A send batch is fully done (success or failure): reset state, clean up,
    /// remove the actually-sent photos from the grid, and re-enable the footer.
    fn finish_send(&mut self, result: Result<(), String>, worker_failed: Vec<PathBuf>) {
        let ok = self.photos_sent;
        match &result {
            Ok(()) => log::info!("send: uploaded {ok} photos"),
            Err(e) => log::error!("send failed: {e} (uploaded {ok})"),
        }
        if let Some(t0) = self.send_started_at.take() {
            log::info!(
                "[timing] send batch ok={} upload_total {:.2}s",
                ok,
                t0.elapsed().as_secs_f64()
            );
        }
        reduce(
            &mut *self.state.write().unwrap(),
            AppEvent::SendFinished(result.clone()),
        );
        let sent = {
            let st = self.state.read().unwrap();
            st.usage.sent + self.photos_sent
        };
        reduce(
            &mut *self.state.write().unwrap(),
            AppEvent::Usage(UsageStats {
                sent,
                ..Default::default()
            }),
        );
        // On success the temp files were uploaded — delete them. On failure, keep
        // the exported JPEGs so nothing is lost (the user can re-send from the
        // files), and tell them where they are.
        if result.is_ok() {
            self.cleanup_send_temp_files();
        } else {
            let kept = std::mem::take(&mut self.send_temp_paths);
            if !kept.is_empty() {
                let dir = kept
                    .first()
                    .and_then(|p| p.parent())
                    .map(|d| d.display().to_string())
                    .unwrap_or_default();
                log::warn!("send failed — {} exported JPEGs kept in {dir}", kept.len());
                self.toast
                    .show(&format!("Send failed — {} JPEGs kept in {dir}", kept.len()));
            }
        }

        // Remove only the photos that were actually sent (photoup keeps the rest
        // with their edits); worker-level failures stay.
        let failed_ids: Vec<u64> = {
            let st = self.state.read().unwrap();
            st.photos
                .iter()
                .filter(|p| worker_failed.contains(&p.path))
                .map(|p| p.id)
                .collect()
        };
        let sent_ids: Vec<u64> = self
            .send_jobs
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| !failed_ids.contains(id))
            .collect();
        self.remove_photos_from_ui(sent_ids);

        // Reset the batch bookkeeping.
        self.send_jobs.clear();
        self.send_jobs_done = 0;
        self.albums_remaining = 0;
        self.batch_total = 0;
        self.photos_sent = 0;
        self.send_pending.clear();
        self.send_failed.clear();
        self.pending_exports.clear();

        if result.is_ok() {
            self.send_reenable();
            self.toast.show(&format!("Sent {ok} photos"));
        } else {
            self.send_failed_backoff();
            self.toast
                .show(&format!("Send failed: {}", result.unwrap_err()));
        }
        self.refresh_screens();
    }

    /// Count a finished export job toward the "Preparing {i}/{n}" footer label.
    fn note_export_done(&mut self, id: u64) {
        if self.send_jobs_done < self.send_jobs.len()
            && self.send_jobs.iter().any(|(jid, _)| *jid == id)
        {
            self.send_jobs_done += 1;
        }
    }

    /// Remove the temp export files for the in-flight send (best-effort, ignore
    /// errors). Called when the send finishes or fails.
    fn cleanup_send_temp_files(&mut self) {
        for path in std::mem::take(&mut self.send_temp_paths) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// A send failed: park the Send button for 2s so the user can't hammer a
    /// broken connection, then re-enable it (Task 23).
    fn send_failed_backoff(&mut self) {
        self.main_screen.send_button.set_sensitive(false);
        if let Some(src) = self.send_backoff.take() {
            src.remove();
        }
        let Some(w) = self.ctl.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        let src = glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
            let mut ctl = w.borrow_mut();
            ctl.send_backoff = None;
            ctl.main_screen.send_button.set_sensitive(true);
        });
        self.send_backoff = Some(src);
    }

    /// A send succeeded: re-enable the Send button right away and cancel any
    /// pending backoff timer so it can't re-enable at a bad time (Task 23).
    fn send_reenable(&mut self) {
        self.main_screen.send_button.set_sensitive(true);
        if let Some(src) = self.send_backoff.take() {
            src.remove();
        }
    }

    // ---- Helpers ----------------------------------------------------------

    fn index_of(&self, id: u64) -> Option<usize> {
        self.state
            .read()
            .unwrap()
            .photos
            .iter()
            .position(|p| p.id == id)
    }

    /// The EV the editor's indicator should show after a render of `id`: the
    /// render's auto EV in Auto/Burn mode, the manual EV otherwise.
    fn effective_ev_for(&self, id: u64, render_auto_ev: f32) -> f32 {
        let st = self.state.read().unwrap();
        let Some(adj) = st.photos.iter().find(|p| p.id == id).map(|p| p.adjustments) else {
            return render_auto_ev;
        };
        match adj.exposure_mode {
            ExposureMode::Manual => adj.exposure_ev,
            _ => render_auto_ev,
        }
    }

    fn on_grid_selected(&mut self, idx: u32) {
        if idx == gtk4::INVALID_LIST_POSITION {
            return;
        }
        let Some(obj) = self.main_screen.grid_store.item(idx) else {
            return;
        };
        let Some(row) = obj.downcast_ref::<PhotoRow>() else {
            return;
        };
        let id = row.id();
        let index = {
            let st = self.state.read().unwrap();
            st.photos.iter().position(|p| p.id == id)
        };
        let Some(index) = index else { return };
        reduce(
            &mut *self.state.write().unwrap(),
            AppEvent::ActivePhoto { index: Some(index) },
        );
        self.open_editor_for_active();
        self.refresh_screens();
    }

    /// Clear the grid's selection so clicking the same photo again re-opens the
    /// editor (SingleSelection won't re-emit `selected_notify` for a repeated
    /// index). Runs synchronously — the `selected_notify` handler (see `setup`)
    /// defers its controller borrow to an idle callback, so `set_selected` here
    /// (even inside `poll()`'s `RefMut`) only schedules that callback and cannot
    /// re-borrow. Deferring it instead let a fast re-click land while the row was
    /// still selected, swallowing the click.
    fn deselect_grid(&mut self) {
        if let Some(sel) = self
            .main_screen
            .grid
            .model()
            .and_then(|m| m.downcast::<gtk4::SingleSelection>().ok())
        {
            sel.set_selected(gtk4::INVALID_LIST_POSITION);
        }
    }

    /// Reset → drop all loaded photos (photoup `clearPhotos`).
    fn on_reset(&mut self) {
        let n = self.state.read().unwrap().photos.len();
        log::info!("action: reset — cleared {n} photos");
        self.main_screen.grid_store.remove_all();
        self.row_map.clear();
        self.cam_mul.clear();
        self.cam_matrix.clear();
        self.active_base = None;
        reduce(&mut *self.state.write().unwrap(), AppEvent::PhotosCleared);
        self.deselect_grid();
        self.refresh_screens();
    }

    /// Logout → drop all photos + dialogs and return to the login screen.
    /// The telegram worker has no logout command, so the session itself stays
    /// authorized (the next launch will skip login); see the report.
    fn on_logout(&mut self) {
        log::info!("action: logout");
        self.main_screen.grid_store.remove_all();
        self.row_map.clear();
        self.dialogs.clear();
        self.cam_mul.clear();
        self.cam_matrix.clear();
        self.active_base = None;
        self.main_screen
            .group_dropdown
            .set_model(Some(&gtk4::StringList::new(&[])));
        reduce(&mut *self.state.write().unwrap(), AppEvent::Logout);
        self.deselect_grid();
        self.refresh_screens();
    }

    /// Remove the given photo ids from both the grid store and AppState.
    fn remove_photos_from_ui(&mut self, ids: Vec<u64>) {
        if ids.is_empty() {
            return;
        }
        let mut idx = self.main_screen.grid_store.n_items();
        while idx > 0 {
            idx -= 1;
            if let Some(obj) = self.main_screen.grid_store.item(idx) {
                if let Some(row) = obj.downcast_ref::<PhotoRow>() {
                    if ids.contains(&row.id()) {
                        self.row_map.remove(&row.id());
                        self.main_screen.grid_store.remove(idx);
                    }
                }
            }
        }
        // Drop the removed photos' caches + camera matrices (memory back).
        for id in &ids {
            self.cam_mul.remove(id);
            self.cam_matrix.remove(id);
            if self
                .active_base
                .as_ref()
                .map_or(false, |(cid, _)| cid == id)
            {
                self.active_base = None;
            }
        }
        reduce(
            &mut *self.state.write().unwrap(),
            AppEvent::PhotosRemoved(ids),
        );
    }

    /// Ctrl+A / Cmd+A: toggle select-all on the grid (photoup `selectAll(!allSelected)`):
    /// if every photo is selected, deselect all; otherwise select all. Mirrors the
    /// choice into AppState (a `PhotoSelected` reduce per photo) AND each grid
    /// row's `selected` property — scroll-recycled cells repaint from the row
    /// property, and the visible checkbox follows via `selected-notify` (grid.rs).
    fn on_select_all_toggle(&mut self) {
        let (all_selected, ids): (bool, Vec<u64>) = {
            let st = self.state.read().unwrap();
            (
                !st.photos.is_empty() && st.photos.iter().all(|p| p.selected),
                st.photos.iter().map(|p| p.id).collect(),
            )
        };
        let target = !all_selected;
        let mut st = self.state.write().unwrap();
        for id in ids {
            reduce(
                &mut st,
                AppEvent::PhotoSelected {
                    id,
                    selected: target,
                },
            );
        }
        drop(st);
        for row in self.row_map.values() {
            row.set_selected(target);
        }
        // Refresh the footer "Send {n} selected" count (and usage indicator).
        self.refresh_screens();
    }

    /// Enter / KP_Enter on the main screen: open the editor for the grid's
    /// selected cell. photoup opens the editor on click; Enter is the keyboard
    /// analogue (a grid cell must be selected first). Returns true only when the
    /// editor was actually opened — so unhandled Returns keep propagating (e.g. to
    /// activate a focused grid cell / button).
    fn on_key_enter(&mut self) -> bool {
        if self.state.read().unwrap().active_photo.is_some() {
            return false; // the editor is already open
        }
        if self.stack.visible_child_name().as_deref() != Some("main") {
            return false; // not on the main (grid) screen
        }
        let Some(sel) = self
            .main_screen
            .grid
            .model()
            .and_then(|m| m.downcast::<gtk4::SingleSelection>().ok())
        else {
            return false;
        };
        let idx = sel.selected();
        if idx == gtk4::INVALID_LIST_POSITION {
            return false;
        }
        self.on_grid_selected(idx);
        true
    }

    /// ArrowRight / ArrowLeft in the editor: navigate the active photo (photoup
    /// EditorPanel `onKeyDown` → `onNext`/`onPrev`), reusing the same path the
    /// editor's Prev/Next buttons use (`AppEvent::Nav`). Returns true only when a
    /// navigation happened, so arrows outside the editor keep moving grid focus.
    fn on_key_arrow(&mut self, delta: i8) -> bool {
        if self.state.read().unwrap().active_photo.is_none() {
            return false;
        }
        self.handle_nav(delta);
        true
    }

    /// Escape: close the editor back to the grid. Natural extra — photoup closes
    /// via the Close button, which emits `ActivePhoto { index: None }`; route
    /// through the same handler (drops the cached base, deselects the grid row).
    /// Returns true when the editor was closed.
    fn on_key_escape(&mut self) -> bool {
        if self.state.read().unwrap().active_photo.is_none() {
            return false;
        }
        self.handle_app_event(AppEvent::ActivePhoto { index: None });
        self.refresh_screens();
        true
    }

    /// Editor Prev/Next: move the active photo by `delta`, then reload the editor.
    fn handle_nav(&mut self, delta: i8) {
        let target = {
            let st = self.state.read().unwrap();
            if st.photos.is_empty() {
                return;
            }
            let n = st.photos.len() as i64;
            let cur = st.active_photo.unwrap_or(0) as i64;
            (cur + delta as i64).clamp(0, n - 1) as usize
        };
        {
            let st = self.state.read().unwrap();
            if let Some(p) = st.photos.get(target) {
                log::info!(
                    "action: navigate {} → {}",
                    if delta < 0 { "prev" } else { "next" },
                    crate::ui::util::file_name(&p.path)
                );
            }
        }
        reduce(
            &mut *self.state.write().unwrap(),
            AppEvent::ActivePhoto {
                index: Some(target),
            },
        );
        self.open_editor_for_active();
        self.refresh_screens();
    }

    /// Editor Reject: remove the active photo and close the editor (photoup
    /// `rejectCurrent`).
    fn handle_reject(&mut self) {
        let active = {
            let st = self.state.read().unwrap();
            st.active_photo
                .and_then(|i| st.photos.get(i))
                .map(|p| (p.id, crate::ui::util::file_name(&p.path)))
        };
        if let Some((id, name)) = active {
            log::info!("action: reject {name}");
            self.remove_photos_from_ui(vec![id]);
            self.deselect_grid();
        }
        self.refresh_screens();
    }

    /// Drop the cached decoded base of the currently-active photo (returns its
    /// memory) and clear the editor's per-photo data. Called when the editor closes.
    fn release_active_photo_data(&mut self) {
        self.active_base = None;
        self.editor.release_photo_data();
    }

    /// Load the currently-active photo (state.active_photo) into the editor and
    /// schedule a preview render for it.
    fn open_editor_for_active(&mut self) {
        let active = {
            let st = self.state.read().unwrap();
            let Some(i) = st.active_photo else { return };
            let Some(p) = st.photos.get(i) else { return };
            (
                p.id,
                crate::ui::util::file_name(&p.path),
                p.adjustments,
                shown_ev(p),
                p.source_type == SourceType::Raw,
                p.full_size,
            )
        };
        log::info!("action: open for edit {}", active.1);
        // A different photo is now active: the previous one's decoded base is
        // useless — drop it (returns its memory) and let the next preview decode
        // re-cache under the new id.
        if self
            .active_base
            .as_ref()
            .map_or(true, |(cid, _)| *cid != active.0)
        {
            self.active_base = None;
        }
        self.editor
            .set_photo(active.0, &active.1, &active.2, active.3, active.4, active.5);
        self.editor
            .set_cam_matrix(self.cam_matrix.get(&active.0).copied().flatten());
        // Show the already-decoded thumbnail immediately so the editor is never
        // blank — and never shows the previous photo — while the ≤1024 preview
        // renders in the background and then upgrades this image.
        if let Some(row) = self.row_map.get(&active.0) {
            if let Some(tex) = row.texture() {
                self.editor.set_preview(Some(&tex));
            }
        }
        self.schedule_preview(active.0);
    }
}

/// Effective exposure correction for a photo (photoup `shownEV`): autoEV while in
/// auto/burn mode, the manual EV otherwise.
fn shown_ev(p: &PhotoState) -> f32 {
    match p.adjustments.exposure_mode {
        ExposureMode::Manual => p.adjustments.exposure_ev,
        _ => p.auto_ev,
    }
}

/// A decoded photo: the renderable base plus everything the editor needs
/// alongside the pixels.
struct DecodedBase {
    base: Box<dyn Base>,
    /// Full developed dimensions (RAW interactive decodes are half-size, so this
    /// is the doubled geometry, not the buffer's).
    developed: (u32, u32),
    /// Camera as-shot WB multipliers, so exports can bake `export_wb_mul` into
    /// libraw's `user_mul`. `None` for JPEG.
    cam_mul: Option<[f32; 4]>,
    /// Camera→sRGB matrix, so the editor's WB Auto/Pick can neutralize a picked
    /// pixel through the matrix. `None` for JPEG.
    cam_matrix: Option<[[f32; 4]; 3]>,
    meta: PhotoMeta,
}

/// Decode a file into a renderable base plus its camera metadata.
fn decode_base(
    data: &[u8],
    source_type: SourceType,
    full_size: bool,
    user_mul: Option<[f32; 4]>,
) -> Result<DecodedBase, String> {
    match source_type {
        SourceType::Jpeg => {
            let (size, rgba, meta) = decode_jpeg(data).map_err(|e| e.to_string())?;
            Ok(DecodedBase {
                base: Box::new(JpegBase::new(size.width, size.height, rgba)),
                developed: (size.width, size.height),
                cam_mul: None,
                cam_matrix: None,
                meta,
            })
        }
        SourceType::Raw => {
            let dr = decode_raw(
                data,
                &RawDecodeOpts {
                    full_size,
                    user_mul,
                },
            )
            .map_err(|e| e.to_string())?;
            let cam = dr.cam_mul;
            let cam_matrix = dr.cam_matrix;
            let developed = (dr.developed_size.width, dr.developed_size.height);
            // `dr` is consumed by RawBase; take the metadata first.
            let meta = dr.meta.clone();
            Ok(DecodedBase {
                base: Box::new(RawBase::new(dr)),
                developed,
                cam_mul: cam,
                cam_matrix,
                meta,
            })
        }
    }
}

/// Decode + render a ≤512 thumbnail. Runs on a pool worker. The returned `full`
/// is the source's full developed size (JPEG: native; RAW: the half-resolution
/// interactive decode's dimensions multiplied by two).
fn run_thumb_job(
    path: &Path,
    source_type: SourceType,
) -> Result<
    (
        Vec<u8>,
        (u32, u32),
        (u32, u32),
        f32,
        f32,
        Vec<u32>,
        Option<[f32; 4]>,
        PhotoMeta,
    ),
    String,
> {
    let t0 = std::time::Instant::now();
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let t_read = t0.elapsed();
    let decoded = decode_base(&data, source_type, false, None)?;
    let t_decode = t0.elapsed();
    let (base, full, cam) = (decoded.base, decoded.developed, decoded.cam_mul);
    let meta = decoded.meta;
    let (w, h) = fit_within(base.width(), base.height(), 512);
    let r = base.render(
        None,
        Size {
            width: w,
            height: h,
        },
        &Adjustments::default(),
    );
    let t_render = t0.elapsed();
    let hist = compute_histogram_rgb(&r.rgba);
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    log::info!(
        "[timing] thumb {name} {source_type:?} read {:.3}s decode {:.3}s render {:.3}s total {:.3}s",
        t_read.as_secs_f64(),
        t_decode.as_secs_f64(),
        t_render.as_secs_f64(),
        t_render.as_secs_f64()
    );
    Ok((r.rgba, (w, h), full, r.auto_ev, r.black_point, hist, cam, meta))
}

/// Decode + render a ≤1024 preview with the photo's current adjustments. Also
/// returns the camera as-shot WB multipliers, the camera→sRGB color matrix, and
/// the decoded base itself so the controller can cache it (slider edits then
/// re-render from memory instead of re-decoding — see `run_render_job`).
///
/// The preview ALWAYS shows the full original frame (crop applied with `None`):
/// the crop is drawn interactively on top in the editor as a selection overlay
/// (photoup `object-fit: contain` + `.crop-box`), and is applied only at export
/// (`run_export_job`). Exposure/WB still apply.
fn run_preview_job(
    path: &Path,
    source_type: SourceType,
    adjustments: &Adjustments,
) -> Result<
    (
        Vec<u8>,
        (u32, u32),
        (u32, u32),
        f32,
        f32,
        Vec<u32>,
        Option<[f32; 4]>,
        Option<[[f32; 4]; 3]>,
        Arc<dyn Base>,
    ),
    String,
> {
    let t0 = std::time::Instant::now();
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let decoded = decode_base(&data, source_type, false, None)?;
    let (base, developed_full, cam, cam_matrix) =
        (decoded.base, decoded.developed, decoded.cam_mul, decoded.cam_matrix);
    let t_decode = t0.elapsed();
    // The preview renders in DISPLAY space: `full` carries the rotated dims and
    // the target size is aspect-matched to them; the base itself stays unrotated
    // so it can be cached and re-wrapped as rotation changes.
    let arc_base: Arc<dyn Base> = Arc::from(base);
    let preview_full = rotate_dims(arc_base.width(), arc_base.height(), adjustments.rotation);
    let full = rotate_dims(developed_full.0, developed_full.1, adjustments.rotation);
    let (w, h) = fit_within(preview_full.0, preview_full.1, FINAL_EDGE);
    let wrapped = RotatedBase::new(Arc::clone(&arc_base), adjustments.rotation);
    let (ev_override, bp_override) = crop_aware_autos(&wrapped, adjustments);
    let r = wrapped.render_with_overrides(
        None,
        Size {
            width: w,
            height: h,
        },
        adjustments,
        ev_override,
        bp_override,
    );
    let t_render = t0.elapsed();
    let hist = compute_histogram_rgb(&r.rgba);
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    log::info!(
        "[timing] preview {name} {source_type:?} read+decode {:.3}s render {:.3}s total {:.3}s",
        t_decode.as_secs_f64(),
        t_render.as_secs_f64(),
        t_render.as_secs_f64()
    );
    Ok((
        r.rgba,
        (w, h),
        full,
        r.auto_ev,
        r.black_point,
        hist,
        cam,
        cam_matrix,
        arc_base,
    ))
}

/// The auto-exposure EV and auto black point computed over the CROP region, so
/// the preview reacts to what is actually in the frame after cropping — and ends
/// up identical to the export, which renders the crop itself. Each is `None`
/// when its own mode is manual, or when no crop is set (auto over the full frame
/// is then the same thing).
fn crop_aware_autos(base: &dyn Base, adjustments: &Adjustments) -> (Option<f32>, Option<f32>) {
    let Some(crop) = adjustments.crop else {
        return (None, None);
    };
    let rect = crop_rect(base.width(), base.height(), Some(&crop));
    let tiny = fit_within(rect.width, rect.height, 128);
    let r = base.render(
        Some(&crop),
        Size {
            width: tiny.0,
            height: tiny.1,
        },
        adjustments,
    );
    // Only the auto modes own these; in Manual (or once the slider took the black
    // point over) the render's own local values stand.
    let ev = (adjustments.exposure_mode != ExposureMode::Manual).then_some(r.auto_ev);
    let bp = adjustments.black_point_auto.then_some(r.black_point);
    (ev, bp)
}

/// Render a preview from an already-decoded base — no re-decode, so slider edits
/// on the active photo are snappy (RAW especially). Runs on a pool worker.
/// `edge` is LIVE_EDGE (512, during a drag) or FINAL_EDGE (1024, once settled).
/// Same output shape as `run_preview_job` minus the decode-only extras (cam_mul,
/// cam_matrix, base); the caller already has those cached.
fn run_render_job(
    base: Arc<dyn Base>,
    adjustments: &Adjustments,
    edge: u32,
    source_type: SourceType,
) -> Result<(Vec<u8>, (u32, u32), (u32, u32), f32, f32, Vec<u32>), String> {
    let t0 = std::time::Instant::now();
    let render_full = rotate_dims(base.width(), base.height(), adjustments.rotation);
    // The cached base never has user rotation applied. Interactive RAW bases
    // are half-size (decode_base uses full_size=false); JPEG bases are native.
    // Derive geometry from that immutable source, never PhotoState.full_size:
    // that field already contains DISPLAY dimensions from the previous render,
    // so rotating it again flips the overlay on every edit/settle at 90°/270°.
    let pixel_scale = if source_type == SourceType::Raw { 2 } else { 1 };
    let full = (render_full.0 * pixel_scale, render_full.1 * pixel_scale);
    let (w, h) = fit_within(render_full.0, render_full.1, edge);
    let wrapped = RotatedBase::new(base, adjustments.rotation);
    let (ev_override, bp_override) = crop_aware_autos(&wrapped, adjustments);
    let r = wrapped.render_with_overrides(
        None,
        Size {
            width: w,
            height: h,
        },
        adjustments,
        ev_override,
        bp_override,
    );
    let t_render = t0.elapsed();
    let hist = compute_histogram_rgb(&r.rgba);
    log::info!(
        "[timing] preview (cached base) render {:.3}s total {:.3}s",
        t_render.as_secs_f64(),
        t_render.as_secs_f64()
    );
    Ok((r.rgba, (w, h), full, r.auto_ev, r.black_point, hist))
}

/// Downscale the active photo's decoded base to a ≤96px LINEAR (0..1) RGB sample
/// for the WB pick/auto. Interleaved RGB, `w*h*3` length. Runs on a pool worker;
/// cheap (a box-filtered downscale), so it can fire alongside the first preview.
/// The sample is DISPLAY-oriented (the base is wrapped with the photo's rotation)
/// so the editor's click→sample mapping stays direct.
fn run_wb_sample_job(base: Arc<dyn Base>, rotation: u8) -> (Vec<f32>, u32, u32) {
    let (dw, dh) = rotate_dims(base.width(), base.height(), rotation);
    let (w, h) = fit_within(dw, dh, WB_SAMPLE_EDGE);
    let wrapped = RotatedBase::new(base, rotation);
    let rgba = wrapped.linear_sample(Size {
        width: w,
        height: h,
    });
    (rgba, w, h)
}

/// For RAW exports the effective WB is baked into libraw's `user_mul`
/// (pre-matrix), so the render must NOT apply `wb_gains` again — otherwise the
/// warmth/hue is squared vs the preview (photoup's photos.ts renders the baked
/// export with `wbOffset: 0, hue: 0`). JPEG — and RAW decoded with camera WB
/// when no `cam_mul` is available — keep render-time WB, exactly like the preview.
fn export_render_adjustments(
    source_type: SourceType,
    user_mul: Option<[f32; 4]>,
    adjustments: &Adjustments,
) -> Adjustments {
    if source_type == SourceType::Raw && user_mul.is_some() {
        Adjustments {
            wb_offset: 0.0,
            hue: 0.0,
            ..*adjustments
        }
    } else {
        *adjustments
    }
}

/// Full-size render ≤2560 + adaptive JPEG encode. RAW bakes the effective WB
/// (camera as-shot × user warmth/hue) into libraw's pre-matrix `user_mul`,
/// matching photoup's export path.
fn run_export_job(
    path: &Path,
    source_type: SourceType,
    adjustments: &Adjustments,
    cam_mul: Option<[f32; 4]>,
) -> Result<(Vec<u8>, u32, u32), String> {
    let t0 = std::time::Instant::now();
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let t_read = t0.elapsed();
    let user_mul = match source_type {
        SourceType::Raw => cam_mul.map(|m| export_wb_mul(m, adjustments)),
        SourceType::Jpeg => None,
    };
    let base = decode_base(&data, source_type, true, user_mul)?.base;
    let t_decode = t0.elapsed();
    // Crop state is unrotated source space. Size it there first, then rotate the
    // output dimensions for the display/export orientation.
    let source_size = export_dimensions(
        base.width(),
        base.height(),
        adjustments.crop.as_ref(),
        EXPORT_EDGE,
    );
    let (out_w, out_h) = rotate_dims(source_size.width, source_size.height, adjustments.rotation);
    let size = Size {
        width: out_w,
        height: out_h,
    };
    let render_adj = export_render_adjustments(source_type, user_mul, adjustments);
    let wrapped = RotatedBase::new(Arc::from(base), adjustments.rotation);
    let r = wrapped.render(render_adj.crop.as_ref(), size, &render_adj);
    let t_render = t0.elapsed();
    let mut rgb = Vec::with_capacity((size.width * size.height * 3) as usize);
    for px in r.rgba.chunks_exact(4) {
        rgb.push(px[0]);
        rgb.push(px[1]);
        rgb.push(px[2]);
    }
    let jpeg = encode_jpeg_444_adaptive(
        &rgb,
        size.width as usize,
        size.height as usize,
        MAX_PHOTO_BYTES,
    )
    .map_err(|e| e.to_string())?;
    let t_encode = t0.elapsed();
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    log::info!(
        "[timing] export {name} {source_type:?} full_size=({}x{}) read {:.3}s decode {:.3}s render {:.3}s encode {:.3}s total {:.3}s -> {} bytes",
        size.width,
        size.height,
        t_read.as_secs_f64(),
        t_decode.as_secs_f64(),
        t_render.as_secs_f64(),
        t_encode.as_secs_f64(),
        t_encode.as_secs_f64(),
        jpeg.len()
    );
    Ok((jpeg, size.width, size.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::types::ExposureMode;
    use image::ImageEncoder;

    #[test]
    fn is_image_path_is_case_insensitive() {
        // The dialog filter uses `add_suffix` (case-insensitive), and the
        // drag-drop/paste gate `is_image_path` lowercases too — both must accept
        // the uppercase extensions cameras actually produce.
        for p in [
            "/x/DSC_4858.NEF",
            "/x/IMG_7833.CR2",
            "/x/CRW_0463.DNG",
            "/x/a.JPG",
            "/x/b.PNG",
            "/x/c.nef",
            "/x/d.jpeg",
        ] {
            assert!(is_image_path(Path::new(p)), "{p} must be a photo");
        }
        assert!(!is_image_path(Path::new("/x/notes.txt")));
        assert!(!is_image_path(Path::new("/x/a.tif")));
    }

    fn make_jpeg_file(w: u32, h: u32) -> PathBuf {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let dir = std::env::temp_dir().join("photoup2-app-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("gradient-{w}x{h}.jpg"));
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut buf)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .expect("encode jpeg");
        std::fs::write(&path, &buf).unwrap();
        path
    }

    #[test]
    fn thumb_job_renders_small_preview() {
        let path = make_jpeg_file(2048, 1024);
        let (rgba, size, full, _ev, _bp, hist, cam, meta) =
            run_thumb_job(&path, SourceType::Jpeg).unwrap();
        assert!(size.0 <= 512 && size.1 <= 512, "size {size:?}");
        assert_eq!(rgba.len(), (size.0 * size.1 * 4) as usize);
        // RGB histogram: 768 bins, and every pixel lands in R, G, AND B → 3×.
        assert_eq!(hist.len(), 768);
        assert_eq!(hist.iter().sum::<u32>(), size.0 * size.1 * 3);
        assert_eq!(cam, None);
        assert_eq!(full, (2048, 1024), "JPEG reports native dims");
        // A synthetic JPEG carries no EXIF, so the metadata is empty (not an error).
        assert!(meta.is_empty());
    }

    #[test]
    fn preview_job_applies_adjustments() {
        let path = make_jpeg_file(800, 600);
        let mut adj = Adjustments::default();
        adj.exposure_mode = ExposureMode::Manual;
        adj.exposure_ev = -2.0;
        let (rgba, size, full, _ev, _bp, _hist, _cam, _cam_matrix, _base) =
            run_preview_job(&path, SourceType::Jpeg, &adj).unwrap();
        assert!(size.0 <= FINAL_EDGE && size.1 <= FINAL_EDGE);
        assert_eq!(rgba.len(), (size.0 * size.1 * 4) as usize);
        assert_eq!(full, (800, 600));
    }

    #[test]
    fn cached_rotated_preview_dimensions_stay_stable_after_resize() {
        let base: Arc<dyn Base> = Arc::new(JpegBase::new(120, 80, vec![128; 120 * 80 * 4]));
        for (source_type, scale) in [(SourceType::Jpeg, 1), (SourceType::Raw, 2)] {
            for rotation in 0..4 {
                let mut adj = Adjustments::default();
                adj.rotation = rotation;
                adj.exposure_mode = ExposureMode::Manual;
                let expected = rotate_dims(120 * scale, 80 * scale, rotation);
                // Fast preview, settle, crop edit, settle. RAW uses a synthetic
                // half-size buffer: its geometry must still report full pixels.
                for edge in [60, 120, 60, 120] {
                    let (_, size, full, _, _, _) =
                        run_render_job(Arc::clone(&base), &adj, edge, source_type).unwrap();
                    assert_eq!(full, expected, "rotation {rotation}, edge {edge}");
                    assert_eq!(size.0 * full.1, size.1 * full.0);
                    adj.crop = Some(crate::image::types::NormalizedCrop {
                        x: 0.1,
                        y: 0.2,
                        width: 0.7,
                        height: 0.6,
                    });
                }
            }
        }
    }

    #[test]
    fn export_job_encodes_valid_jpeg() {
        let path = make_jpeg_file(3000, 2000);
        let (jpeg, w, h) =
            run_export_job(&path, SourceType::Jpeg, &Adjustments::default(), None).unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8], "JPEG SOI marker");
        assert!(w <= EXPORT_EDGE && h <= EXPORT_EDGE, "dims {w}x{h}");
        assert!(jpeg.len() <= MAX_PHOTO_BYTES);
        assert!(!jpeg.is_empty());
    }

    #[test]
    fn missing_file_surfaces_error() {
        let bogus = std::env::temp_dir()
            .join("photoup2-app-test")
            .join("nope.jpg");
        let err = run_thumb_job(&bogus, SourceType::Jpeg).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn image_extension_filter_accepts_supported_types() {
        for name in [
            "a.jpg", "a.jpeg", "a.JPG", "a.PnG", "A.NEF", "b.cr2", "c.DNG", "d.dng",
        ] {
            assert!(is_image_path(Path::new(name)), "{name} should be accepted");
        }
    }

    /// Every RAW extension must also be an accepted photo, and must be classified
    /// as RAW (not JPEG) for the decode dispatch + the grid badge. Without this a
    /// new RAW format can be decodable yet unreachable, or silently fed to the
    /// JPEG decoder — exactly the state DNG was in.
    #[test]
    fn raw_exts_are_accepted_photos() {
        use crate::image::decode::{RAW_EXTS, is_raw_ext};
        for ext in RAW_EXTS {
            assert!(
                IMAGE_EXTS.contains(&ext),
                "RAW ext {ext} missing from IMAGE_EXTS"
            );
            assert!(is_raw_ext(ext), "{ext} must classify as RAW");
            assert!(
                is_raw_ext(&ext.to_ascii_uppercase()),
                "{ext} must classify as RAW in any case"
            );
            assert!(!is_raw_ext("jpg"), "jpg is not RAW");
            assert!(!is_raw_ext("png"), "png is not RAW");
        }
    }

    /// A DNG source must dispatch to the RAW path, not the JPEG one.
    #[test]
    fn dng_is_classified_as_raw() {
        assert_eq!(source_type_for(Path::new("/x/CRW_0463.DNG")), SourceType::Raw);
        assert_eq!(source_type_for(Path::new("/x/a.nef")), SourceType::Raw);
        assert_eq!(source_type_for(Path::new("/x/a.cr2")), SourceType::Raw);
        assert_eq!(source_type_for(Path::new("/x/a.jpg")), SourceType::Jpeg);
    }

    #[test]
    fn image_extension_filter_rejects_other_files() {
        for name in [
            "a.txt",
            "a.png.txt",
            "a.jpeg.bak",
            "notes",
            "",
            ".jpg",
            "a.svg",
            "a.gif",
        ] {
            assert!(
                !is_image_path(Path::new(name)),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn filter_photo_paths_keeps_only_images() {
        let mixed = vec![
            PathBuf::from("/tmp/a.jpg"),
            PathBuf::from("/tmp/b.txt"),
            PathBuf::from("/tmp/c.NEF"),
            PathBuf::from("/tmp/no_ext"),
            PathBuf::from("/tmp/d.png"),
            PathBuf::from("/tmp/e.DNG"),
        ];
        let kept = filter_photo_paths(mixed);
        let names: Vec<&str> = kept
            .iter()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
            .collect();
        assert_eq!(names, ["a.jpg", "c.NEF", "d.png", "e.DNG"]);
    }

    #[test]
    fn export_neutralizes_wb_only_when_baked() {
        let mut adj = Adjustments::default();
        adj.exposure_mode = ExposureMode::Manual;
        adj.exposure_ev = 1.5;
        adj.wb_offset = 0.5;
        adj.hue = -0.3;
        // RAW + baked user_mul → WB neutralized, exposure/crop kept (photoup parity).
        let baked = export_render_adjustments(SourceType::Raw, Some([1.0, 1.0, 1.0, 1.0]), &adj);
        assert_eq!(baked.wb_offset, 0.0);
        assert_eq!(baked.hue, 0.0);
        assert_eq!(baked.exposure_ev, 1.5);
        assert_eq!(baked.exposure_mode, ExposureMode::Manual);
        // JPEG (WB applied at render time) keeps the full adjustments.
        let jpeg = export_render_adjustments(SourceType::Jpeg, None, &adj);
        assert_eq!(jpeg.wb_offset, 0.5);
        assert_eq!(jpeg.hue, -0.3);
        // RAW decoded with camera WB (no cam_mul → no bake) also keeps WB.
        let unbaked = export_render_adjustments(SourceType::Raw, None, &adj);
        assert_eq!(unbaked.wb_offset, 0.5);
        assert_eq!(unbaked.hue, -0.3);
    }
}
