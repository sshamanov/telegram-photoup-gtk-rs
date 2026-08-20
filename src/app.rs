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
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use gtk4::prelude::*;
use gtk4::gio;

use crate::image::decode::{decode_jpeg, decode_raw, RawDecodeOpts};
use crate::image::encode::{encode_jpeg_444_adaptive, MAX_PHOTO_BYTES};
use crate::image::math::fit_within;
use crate::image::pool::ImagePool;
use crate::image::process::{compute_histogram, export_dimensions, Base, JpegBase, RawBase};
use crate::image::srgb::export_wb_mul;
use crate::image::types::{Adjustments, Size, SourceType};
use crate::state::{
    reduce, AppEvent, AppState, AuthEvent, AuthStatus, PhotoState, PhotoStatus, UsageStats,
};
use crate::telegram::{worker::TelegramWorkerConfig, AuthStep, DialogInfo, TCommand, TEvent};
use crate::ui::editor::EditorScreen;
use crate::ui::grid::PhotoRow;
use crate::ui::login::LoginScreen;
use crate::ui::main_screen::MainScreen;
use crate::ui::toast::Toast;

/// Longest edge of an interactive preview render.
const PREVIEW_EDGE: u32 = 1024;
/// Longest edge of an export render (Telegram photo size cap).
const EXPORT_EDGE: u32 = 2560;
/// Debounce window for slider-drag preview re-renders.
const PREVIEW_DEBOUNCE_MS: u64 = 150;

/// Pool job results, carried back to the UI thread.
pub enum UiEvent {
    ThumbReady {
        id: u64,
        rgba: Vec<u8>,
        size: (u32, u32),
        auto_ev: f32,
        histogram: Vec<u32>,
        cam_mul: Option<[f32; 4]>,
    },
    PreviewReady {
        id: u64,
        rgba: Vec<u8>,
        size: (u32, u32),
        auto_ev: f32,
        histogram: Vec<u32>,
        cam_mul: Option<[f32; 4]>,
        preview_gen: u64,
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
    send_pending: Vec<u64>,
    send_failed: Vec<u64>,
    send_peer: Option<DialogInfo>,
    pending_exports: HashMap<u64, Vec<u8>>,

    // Temp export files handed to the telegram worker for the in-flight send;
    // removed best-effort once the send finishes or fails (Task 23).
    send_temp_paths: Vec<PathBuf>,
    // Pending 2s re-enable of the Send button after a failed send (Task 23);
    // cancelled on success so it can't fight the immediate re-enable.
    send_backoff: Option<glib::SourceId>,

    // Debounced preview re-render bookkeeping.
    render_debounce: Option<glib::SourceId>,
    render_gen: u64,

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
            send_pending: Vec::new(),
            send_failed: Vec::new(),
            send_peer: None,
            pending_exports: HashMap::new(),
            send_temp_paths: Vec::new(),
            send_backoff: None,
            render_debounce: None,
            render_gen: 0,
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
                ctl.borrow_mut().on_grid_selected(sel.selected());
            });
        }

        // Load / Send buttons.
        let load_ctl = Rc::clone(&ctl);
        self.main_screen
            .load_button
            .connect_clicked(move |_| load_ctl.borrow_mut().on_load());

        let send_ctl = Rc::clone(&ctl);
        self.main_screen
            .send_button
            .connect_clicked(move |_| send_ctl.borrow_mut().on_send());

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
                let exts = ["jpg", "jpeg", "png", "nef", "cr2"];
                if let Ok(rd) = std::fs::read_dir("samples") {
                    for e in rd.flatten() {
                        let p = e.path();
                        let is_photo = p
                            .extension()
                            .and_then(|s| s.to_str())
                            .map(|s| exts.contains(&s.to_ascii_lowercase().as_str()))
                            .unwrap_or(false);
                        if is_photo {
                            ctl.add_photo(p);
                        }
                    }
                }
            });
        }
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
        match &ev {
            AppEvent::Auth(AuthEvent::PhoneRequested { phone }) => {
                let _ = self.telegram_cmd.send(TCommand::RequestCode { phone: phone.clone() });
            }
            AppEvent::Auth(AuthEvent::CodeEntered { code }) => {
                let _ = self.telegram_cmd.send(TCommand::SubmitCode { code: code.clone() });
            }
            AppEvent::Auth(AuthEvent::PasswordEntered { password }) => {
                let _ = self.telegram_cmd.send(TCommand::SubmitPassword { password: password.clone() });
            }
            AppEvent::PhotoEdit { id, .. } => self.schedule_preview(*id),
            AppEvent::ActivePhoto { index: None } => {
                // Deselect the grid row so clicking the same photo again re-opens
                // the editor (SingleSelection won't re-emit if already selected).
                // Deferred to an idle callback: we're inside `poll()`'s `RefMut`,
                // and `set_selected` fires `selected_notify` synchronously, which
                // would try to borrow the controller a second time (re-entrancy
                // panic). The idle callback runs after the borrow is released.
                if let Some(sel) = self
                    .main_screen
                    .grid
                    .model()
                    .and_then(|m| m.downcast::<gtk4::SingleSelection>().ok())
                {
                    glib::idle_add_local_once(move || {
                        sel.set_selected(gtk4::INVALID_LIST_POSITION);
                    });
                }
            }
            _ => {}
        }
        reduce(&mut *self.state.write().unwrap(), ev);
    }

    fn handle_telegram_event(&mut self, ev: TEvent) {
        match ev {
            TEvent::AuthStep(AuthStep::Ready) => {
                reduce(&mut *self.state.write().unwrap(), AppEvent::Auth(AuthEvent::Success));
                let _ = self.telegram_cmd.send(TCommand::LoadDialogs);
            }
            TEvent::AuthStep(_) => {}
            TEvent::CodeRequested => {
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::Auth(AuthEvent::PhoneRequested { phone: String::new() }),
                );
            }
            TEvent::PasswordRequired { hint } => {
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::Auth(AuthEvent::CodeEntered { code: String::new() }),
                );
                if let Some(h) = hint {
                    self.toast.show(&format!("2FA password required: {h}"));
                }
            }
            TEvent::AuthFailed(msg) => {
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::Auth(AuthEvent::Failure(msg.clone())),
                );
                self.toast.show(&msg);
            }
            TEvent::Dialogs(dialogs) => self.populate_group_picker(dialogs),
            TEvent::Sent { ok, failed } => {
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::SendFinished(Ok(())),
                );
                let sent = {
                    let st = self.state.read().unwrap();
                    st.usage.sent + ok
                };
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::Usage(UsageStats { sent, ..Default::default() }),
                );
                // The send is over: the temp files are no longer needed, and the
                // Send button comes back immediately (cancelling any backoff timer).
                self.cleanup_send_temp_files();
                self.send_reenable();
                if failed.is_empty() {
                    self.toast.show(&format!("Sent {ok} photos"));
                } else {
                    self.toast.show(&format!("Sent {ok} photos, {} failed", failed.len()));
                }
            }
            TEvent::Error(msg) => {
                // If the worker errored mid-send (connect lost, upload failed), reset
                // `sending` so the Send button is usable again, park it briefly (2s
                // backoff), and drop the temp files. Harmless otherwise.
                let was_sending = {
                    let st = self.state.read().unwrap();
                    st.sending
                };
                if was_sending {
                    reduce(
                        &mut *self.state.write().unwrap(),
                        AppEvent::SendFinished(Err(msg.clone())),
                    );
                    self.send_failed_backoff();
                }
                self.cleanup_send_temp_files();
                self.toast.show(&msg);
            }
        }
    }

    fn handle_ui_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::ThumbReady { id, rgba, size, auto_ev, histogram, cam_mul } => {
                self.cam_mul.insert(id, cam_mul);
                let e = AppEvent::PhotoThumbReady {
                    id,
                    rgba: rgba.clone(),
                    size,
                    auto_ev,
                    histogram: histogram.clone(),
                };
                reduce(&mut *self.state.write().unwrap(), e);
                if let Some(row) = self.row_map.get(&id) {
                    row.set_texture(&crate::ui::util::rgba_to_texture(
                        &rgba,
                        size.0 as i32,
                        size.1 as i32,
                    ));
                    // A successful render clears any prior error badge so a re-render
                    // can recover a previously-failed photo.
                    row.set_error(false);
                }
            }
            UiEvent::PreviewReady { id, rgba, size, auto_ev, histogram, cam_mul, preview_gen } => {
                if preview_gen != self.render_gen {
                    return; // superseded by a newer edit — drop the stale render
                }
                self.cam_mul.insert(id, cam_mul);
                let e = AppEvent::PhotoThumbReady {
                    id,
                    rgba: rgba.clone(),
                    size,
                    auto_ev,
                    histogram: histogram.clone(),
                };
                reduce(&mut *self.state.write().unwrap(), e);
                let idx = self.index_of(id);
                let is_active = {
                    let st = self.state.read().unwrap();
                    st.active_photo == idx
                };
                if is_active {
                    self.editor.set_preview(Some(&crate::ui::util::rgba_to_texture(
                        &rgba,
                        size.0 as i32,
                        size.1 as i32,
                    )));
                    self.editor.set_histogram(&histogram);
                }
                // Keep the grid thumbnail in sync with the edited preview; state
                // now holds the edited render, so the row must show it too.
                if let Some(row) = self.row_map.get(&id) {
                    row.set_texture(&crate::ui::util::rgba_to_texture(
                        &rgba,
                        size.0 as i32,
                        size.1 as i32,
                    ));
                    // Same as ThumbReady: a successful render clears the error badge.
                    row.set_error(false);
                }
            }
            UiEvent::ExportReady { id, jpeg, .. } => {
                self.pending_exports.insert(id, jpeg);
                self.maybe_send();
            }
            UiEvent::JobFailed { id, msg } => {
                reduce(
                    &mut *self.state.write().unwrap(),
                    AppEvent::PhotoFailed { id, msg: msg.clone() },
                );
                self.cam_mul.remove(&id);
                // Mark the grid cell with the error badge (red outline + disabled
                // checkbox). A later successful ThumbReady/PreviewReady clears it.
                if let Some(row) = self.row_map.get(&id) {
                    row.set_error(true);
                }
                // If this photo was part of an in-flight send, account for the
                // failure so the send can proceed with the remaining photos.
                if self.send_pending.contains(&id) && !self.send_failed.contains(&id) {
                    self.send_failed.push(id);
                    self.maybe_send();
                }
                self.toast.show(&format!("Photo {id} failed: {msg}"));
            }
        }
    }

    /// Set stack visibility + login step + usage label from state.
    fn refresh_screens(&mut self) {
        let st = self.state.read().unwrap();
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
        drop(st);
        self.update_usage_label();
    }

    fn update_usage_label(&mut self) {
        let st = self.state.read().unwrap();
        let (mut queued, mut processing, mut ready) = (0usize, 0usize, 0usize);
        for p in &st.photos {
            match p.status {
                PhotoStatus::Queued => queued += 1,
                PhotoStatus::Processing | PhotoStatus::Exporting => processing += 1,
                PhotoStatus::Ready => ready += 1,
                PhotoStatus::Error(_) => {}
            }
        }
        let sent = st.usage.sent;
        drop(st);
        self.main_screen.usage_label.set_text(&format!(
            "queued {queued} · processing {processing} · ready {ready} · sent {sent}"
        ));
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
            let idx = self.dialogs.iter().position(|d| Some(d.id) == self.config.target_peer_id);
            match idx {
                Some(i) => self.main_screen.group_dropdown.set_selected(i as u32),
                None => self.main_screen.group_dropdown.set_selected(0),
            }
        }
    }

    // ---- File loading -----------------------------------------------------

    fn on_load(&mut self) {
        let filter = gtk4::FileFilter::new();
        filter.add_pattern("*.jpg");
        filter.add_pattern("*.jpeg");
        filter.add_pattern("*.png");
        filter.add_pattern("*.nef");
        filter.add_pattern("*.cr2");
        filter.set_name(Some("Photos"));
        let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
        filters.append(&filter);

        let dialog = gtk4::FileDialog::builder().title("Load photos").build();
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&filter));

        let Some(w) = self.ctl.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        dialog.open_multiple(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |res| {
                let Ok(model) = res else { return }; // cancelled
                let mut ctl = w.borrow_mut();
                for i in 0..model.n_items() {
                    if let Some(file) = model.item(i).and_then(|o| o.downcast::<gio::File>().ok()) {
                        if let Some(path) = file.path() {
                            ctl.add_photo(path);
                        }
                    }
                }
            },
        );
    }

    fn add_photo(&mut self, path: PathBuf) {
        let id = self.next_photo_id;
        self.next_photo_id += 1;
        let source_type = match path.extension().and_then(|s| s.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("nef") || e.eq_ignore_ascii_case("cr2") => {
                SourceType::Raw
            }
            _ => SourceType::Jpeg,
        };
        let photo = PhotoState {
            id,
            path: path.clone(),
            source_type,
            adjustments: Adjustments::default(),
            auto_ev: 0.0,
            thumb: None,
            thumb_size: None,
            histogram: None,
            status: PhotoStatus::Queued,
            selected: true, // photoup defaults checkboxes to checked
        };
        reduce(&mut *self.state.write().unwrap(), AppEvent::PhotosAdded(vec![photo]));
        let row = PhotoRow::new(id);
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
                Ok((rgba, size, auto_ev, hist, cam)) => UiEvent::ThumbReady {
                    id,
                    rgba,
                    size,
                    auto_ev,
                    histogram: hist,
                    cam_mul: cam,
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
        let tx = self.ui_events_sender.clone();
        self.pool.submit(move || {
            // Panic-safe, same rationale as the thumbnail/export jobs.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_preview_job(&path, source_type, &adjustments)
            }))
            .unwrap_or_else(|_| Err("preview job panicked".to_string()));
            let _ = tx.send(match result {
                Ok((rgba, size, auto_ev, hist, cam)) => UiEvent::PreviewReady {
                    id,
                    rgba,
                    size,
                    auto_ev,
                    histogram: hist,
                    cam_mul: cam,
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
                Ok((jpeg, width, height)) => UiEvent::ExportReady { id, jpeg, width, height },
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
                st.photos.iter().filter(|p| p.selected).map(|p| p.id).collect::<Vec<_>>(),
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
            self.toast.show("Select at least one photo to send");
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
                .filter(|p| p.selected && !matches!(p.status, PhotoStatus::Error(_)))
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
        let all_accounted = self.send_pending.iter().all(|id| {
            self.pending_exports.contains_key(id) || self.send_failed.contains(id)
        });
        if !all_accounted {
            return;
        }
        let Some(peer) = self.send_peer.take() else { return };
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
            reduce(
                &mut *self.state.write().unwrap(),
                AppEvent::SendFinished(Err("no export written".into())),
            );
            self.send_failed_backoff();
            self.toast.show("Send failed: no photo was exported");
            return;
        }
        // Remember the temp files so the worker's send can be followed up with a
        // best-effort cleanup once it finishes or fails (Task 23).
        self.send_temp_paths = paths.clone();
        let _ = self.telegram_cmd.send(TCommand::SendAlbum {
            peer_id: peer.id,
            access_hash: peer.access_hash,
            paths,
            caption: None,
        });
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
        let src = glib::timeout_add_local_once(
            std::time::Duration::from_secs(2),
            move || {
                w.borrow_mut().main_screen.send_button.set_sensitive(true);
            },
        );
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
        self.state.read().unwrap().photos.iter().position(|p| p.id == id)
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
        let (index, is_raw, adjustments) = {
            let st = self.state.read().unwrap();
            let Some(i) = st.photos.iter().position(|p| p.id == id) else { return };
            let p = &st.photos[i];
            (i, p.source_type == SourceType::Raw, p.adjustments)
        };
        reduce(
            &mut *self.state.write().unwrap(),
            AppEvent::ActivePhoto { index: Some(index) },
        );
        self.editor.set_photo(id, &adjustments, is_raw);
        self.schedule_preview(id);
        self.refresh_screens();
    }
}

/// Decode a file into a renderable base. For RAW, returns the camera as-shot WB
/// multipliers so exports can bake `export_wb_mul` into libraw's user_mul.
fn decode_base(
    data: &[u8],
    source_type: SourceType,
    full_size: bool,
    user_mul: Option<[f32; 4]>,
) -> Result<(Box<dyn Base>, Option<[f32; 4]>), String> {
    match source_type {
        SourceType::Jpeg => {
            let (size, rgba) = decode_jpeg(data).map_err(|e| e.to_string())?;
            Ok((Box::new(JpegBase::new(size.width, size.height, rgba)), None))
        }
        SourceType::Raw => {
            let dr = decode_raw(data, &RawDecodeOpts { full_size, user_mul })
                .map_err(|e| e.to_string())?;
            let cam = dr.cam_mul;
            Ok((Box::new(RawBase::new(dr)), cam))
        }
    }
}

/// Decode + render a ≤512 thumbnail. Runs on a pool worker.
fn run_thumb_job(
    path: &Path,
    source_type: SourceType,
) -> Result<(Vec<u8>, (u32, u32), f32, Vec<u32>, Option<[f32; 4]>), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let (base, cam) = decode_base(&data, source_type, false, None)?;
    let (w, h) = fit_within(base.width(), base.height(), 512);
    let r = base.render(None, Size { width: w, height: h }, &Adjustments::default());
    let hist = compute_histogram(&r.rgba);
    Ok((r.rgba, (w, h), r.auto_ev, hist, cam))
}

/// Decode + render a ≤1024 preview with the photo's current adjustments.
fn run_preview_job(
    path: &Path,
    source_type: SourceType,
    adjustments: &Adjustments,
) -> Result<(Vec<u8>, (u32, u32), f32, Vec<u32>, Option<[f32; 4]>), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let (base, cam) = decode_base(&data, source_type, false, None)?;
    let (w, h) = fit_within(base.width(), base.height(), PREVIEW_EDGE);
    let r = base.render(adjustments.crop.as_ref(), Size { width: w, height: h }, adjustments);
    let hist = compute_histogram(&r.rgba);
    Ok((r.rgba, (w, h), r.auto_ev, hist, cam))
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
        Adjustments { wb_offset: 0.0, hue: 0.0, ..*adjustments }
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
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let user_mul = match source_type {
        SourceType::Raw => cam_mul.map(|m| export_wb_mul(m, adjustments)),
        SourceType::Jpeg => None,
    };
    let (base, _) = decode_base(&data, source_type, true, user_mul)?;
    let size = export_dimensions(base.width(), base.height(), adjustments.crop.as_ref(), EXPORT_EDGE);
    let render_adj = export_render_adjustments(source_type, user_mul, adjustments);
    let r = base.render(render_adj.crop.as_ref(), size, &render_adj);
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
    Ok((jpeg, size.width, size.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::types::ExposureMode;
    use image::ImageEncoder;

    fn make_jpeg_file(w: u32, h: u32) -> PathBuf {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let dir = std::env::temp_dir().join("photoup2-app-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("gradient-{w}x{h}.jpg"));
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut buf)
            .write_image(
                img.as_raw(),
                w,
                h,
                image::ExtendedColorType::Rgb8,
            )
            .expect("encode jpeg");
        std::fs::write(&path, &buf).unwrap();
        path
    }

    #[test]
    fn thumb_job_renders_small_preview() {
        let path = make_jpeg_file(2048, 1024);
        let (rgba, size, _ev, hist, cam) = run_thumb_job(&path, SourceType::Jpeg).unwrap();
        assert!(size.0 <= 512 && size.1 <= 512, "size {size:?}");
        assert_eq!(rgba.len(), (size.0 * size.1 * 4) as usize);
        assert_eq!(hist.iter().sum::<u32>(), size.0 * size.1);
        assert_eq!(cam, None);
    }

    #[test]
    fn preview_job_applies_adjustments() {
        let path = make_jpeg_file(800, 600);
        let mut adj = Adjustments::default();
        adj.exposure_mode = ExposureMode::Manual;
        adj.exposure_ev = -2.0;
        let (rgba, size, _ev, _hist, _cam) = run_preview_job(&path, SourceType::Jpeg, &adj).unwrap();
        assert!(size.0 <= PREVIEW_EDGE && size.1 <= PREVIEW_EDGE);
        assert_eq!(rgba.len(), (size.0 * size.1 * 4) as usize);
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
        let bogus = std::env::temp_dir().join("photoup2-app-test").join("nope.jpg");
        let err = run_thumb_job(&bogus, SourceType::Jpeg).unwrap_err();
        assert!(!err.is_empty());
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

