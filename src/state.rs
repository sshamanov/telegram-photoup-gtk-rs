use std::path::PathBuf;
use crate::image::types::{Adjustments, PhotoMeta, SourceType};

/// State is only ever mutated through `reduce(state, event)` on the GTK main thread.
#[derive(Debug, Default)]
pub struct AppState {
    pub telegram: TelegramState,
    pub photos: Vec<PhotoState>,
    pub active_photo: Option<usize>,
    pub sending: bool,
    pub usage: UsageStats,
}

#[derive(Debug, Default)]
pub struct TelegramState {
    pub status: AuthStatus,
    pub target_peer: Option<TargetPeer>,
    pub dialogs_loaded: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum AuthStatus {
    #[default]
    Idle,
    RequestingCode,
    AwaitingCode,
    Awaiting2fa,
    Authenticated,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TargetPeer {
    pub id: i64,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct PhotoState {
    pub id: u64,
    pub path: PathBuf,
    pub source_type: SourceType,
    pub adjustments: Adjustments,
    pub auto_ev: f32,
    /// Source dimensions at decode time (JPEG: native file size; RAW: decoded,
    /// half-resolution size). Used by the editor's Image section + crop presets.
    pub full_size: Option<(u32, u32)>,
    /// EXIF capture metadata (camera/lens/exposure/date), filled by the thumb
    /// decode and shown in the editor's Image section.
    pub meta: PhotoMeta,
    pub thumb: Option<Vec<u8>>, // RGBA8 preview, ≤512 edge
    pub thumb_size: Option<(u32, u32)>,
    pub histogram: Option<Vec<u32>>,
    pub status: PhotoStatus,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhotoStatus {
    Queued,
    Processing,
    Ready,
    Exporting,
    Error(String),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UsageStats {
    pub queued: usize,
    pub processing: usize,
    pub ready: usize,
    pub sent: usize,
}

#[derive(Debug)]
pub enum AppEvent {
    Auth(AuthEvent),
    PhotosAdded(Vec<PhotoState>),
    /// `meta` is `Some` only on the decode path — render-only jobs pass `None` so
    /// a slider edit cannot wipe the EXIF lines the thumb decode already filled
    /// (same convention as the camera matrix on `PreviewReady`).
    PhotoThumbReady { id: u64, rgba: Vec<u8>, size: (u32, u32), full: (u32, u32), auto_ev: f32, histogram: Vec<u32>, meta: Option<PhotoMeta> },
    PhotoFailed { id: u64, msg: String },
    PhotoEdit { id: u64, adjustments: Adjustments },
    PhotoSelected { id: u64, selected: bool },
    ActivePhoto { index: Option<usize> },
    /// Navigate the active photo by `delta` (-1 prev, +1 next).
    Nav { delta: i8 },
    /// Remove the currently-active photo and close the editor (handled by the
    /// controller; the reducer treats it as a no-op).
    RejectActive,
    /// Drop all loaded photos (photoup `clearPhotos`). Keeps the sent counter.
    PhotosCleared,
    /// Remove the given photo ids (sent photos are dropped after a send).
    PhotosRemoved(Vec<u64>),
    /// Telegram logout: drop all photos and return to the login screen.
    Logout,
    /// Show a transient toast (e.g. a rejected neutral-picker sample). Handled by
    /// the controller; the reducer treats it as a no-op.
    Toast(String),
    TargetPeer(TargetPeer),
    SendStarted,
    SendFinished(Result<(), String>),
    Usage(UsageStats),
}

#[derive(Debug)]
pub enum AuthEvent {
    PhoneRequested { phone: String },
    CodeEntered { code: String },
    PasswordEntered { password: String },
    Success,
    Failure(String),
}

/// Pure reducer — the only place AppState is written on the main thread.
pub fn reduce(state: &mut AppState, event: AppEvent) {
    match event {
        AppEvent::PhotosAdded(photos) => {
            state.usage.queued += photos.len();
            state.photos.extend(photos);
        }
        AppEvent::PhotoThumbReady { id, rgba, size, full, auto_ev, histogram, meta } => {
            if let Some(p) = state.photos.iter_mut().find(|p| p.id == id) {
                p.thumb = Some(rgba);
                p.thumb_size = Some(size);
                p.full_size = Some(full);
                p.auto_ev = auto_ev;
                p.histogram = Some(histogram);
                if let Some(m) = meta {
                    p.meta = m;
                }
                p.status = PhotoStatus::Ready;
            }
        }
        AppEvent::PhotoFailed { id, msg } => {
            if let Some(p) = state.photos.iter_mut().find(|p| p.id == id) {
                p.status = PhotoStatus::Error(msg);
            }
        }
        AppEvent::PhotoEdit { id, adjustments } => {
            if let Some(p) = state.photos.iter_mut().find(|p| p.id == id) {
                p.adjustments = adjustments;
                p.status = PhotoStatus::Processing; // thumbnail will be re-rendered
            }
        }
        AppEvent::PhotoSelected { id, selected } => {
            if let Some(p) = state.photos.iter_mut().find(|p| p.id == id) {
                p.selected = selected;
            }
        }
        AppEvent::ActivePhoto { index } => state.active_photo = index,
        AppEvent::Nav { .. } => {
            // Handled entirely by the controller (it must also drive the editor).
            // The reducer treats it as a no-op so the match stays exhaustive.
        }
        AppEvent::RejectActive => {
            // Same: the controller removes the row + photo and closes the editor.
        }
        AppEvent::PhotosCleared => {
            state.photos.clear();
            state.active_photo = None;
            state.usage = UsageStats { sent: state.usage.sent, ..Default::default() };
        }
        AppEvent::PhotosRemoved(ids) => {
            let active_id = state
                .active_photo
                .and_then(|i| state.photos.get(i))
                .map(|p| p.id);
            state.photos.retain(|p| !ids.contains(&p.id));
            let removed_active = active_id.map_or(false, |aid| ids.contains(&aid));
            if removed_active || state.active_photo.map_or(false, |i| i >= state.photos.len()) {
                state.active_photo = None;
            }
            state.usage = UsageStats { sent: state.usage.sent, ..Default::default() };
        }
        AppEvent::Logout => {
            state.photos.clear();
            state.active_photo = None;
            state.usage = UsageStats { sent: state.usage.sent, ..Default::default() };
            state.telegram.status = AuthStatus::Idle;
        }
        AppEvent::TargetPeer(peer) => state.telegram.target_peer = Some(peer),
        AppEvent::Toast(_) => {
            // Shown by the controller; nothing to persist.
        }
        AppEvent::SendStarted => {
            state.sending = true;
            // Mark selected photos as exporting so the usage label shows the
            // in-flight send (counted as "processing"). Reset back to Ready when
            // SendFinished arrives.
            for p in state.photos.iter_mut() {
                if p.selected && !matches!(p.status, PhotoStatus::Error(_)) {
                    p.status = PhotoStatus::Exporting;
                }
            }
        }
        AppEvent::SendFinished(result) => {
            state.sending = false;
            // Recover photos that were marked Exporting (both success and failure —
            // on error the unsent photos must be re-selectable, not stuck exporting).
            for p in state.photos.iter_mut() {
                if matches!(p.status, PhotoStatus::Exporting) {
                    p.status = PhotoStatus::Ready;
                }
            }
            let _ = result;
        }
        AppEvent::Usage(u) => state.usage = u,
        AppEvent::Auth(ev) => match ev {
            AuthEvent::PhoneRequested { .. } => state.telegram.status = AuthStatus::AwaitingCode,
            AuthEvent::CodeEntered { .. } => state.telegram.status = AuthStatus::Awaiting2fa,
            AuthEvent::PasswordEntered { .. } => state.telegram.status = AuthStatus::Authenticated,
            AuthEvent::Success => state.telegram.status = AuthStatus::Authenticated,
            AuthEvent::Failure(msg) => state.telegram.status = AuthStatus::Failed(msg),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::types::ExposureMode;

    #[test]
    fn photos_added_and_thumb_ready() {
        let mut s = AppState::default();
        let p = PhotoState {
            id: 1,
            path: PathBuf::from("/x/a.jpg"),
            source_type: SourceType::Jpeg,
            adjustments: Adjustments::default(),
            auto_ev: 0.0,
            full_size: None,
            meta: PhotoMeta::default(),
            thumb: None,
            thumb_size: None,
            histogram: None,
            status: PhotoStatus::Queued,
            selected: true,
        };
        reduce(&mut s, AppEvent::PhotosAdded(vec![p]));
        assert_eq!(s.photos.len(), 1);
        reduce(&mut s, AppEvent::PhotoThumbReady { id: 1, rgba: vec![0u8; 4], size: (1, 1), full: (2048, 1024), auto_ev: 0.5, histogram: vec![0; 256], meta: None });
        assert_eq!(s.photos[0].status, PhotoStatus::Ready);
        assert_eq!(s.photos[0].auto_ev, 0.5);
        assert_eq!(s.photos[0].full_size, Some((2048, 1024)));
    }

    #[test]
    fn photo_failed_sets_error_status() {
        let mut s = AppState::default();
        s.photos.push(PhotoState {
            id: 3, path: PathBuf::from("/x"), source_type: SourceType::Jpeg,
            adjustments: Adjustments::default(), auto_ev: 0.0, full_size: None, meta: PhotoMeta::default(), thumb: None, thumb_size: None,
            histogram: None, status: PhotoStatus::Processing, selected: true,
        });
        reduce(&mut s, AppEvent::PhotoFailed { id: 3, msg: "decode boom".into() });
        assert_eq!(s.photos[0].status, PhotoStatus::Error("decode boom".into()));
    }

    #[test]
    fn send_finished_preserves_error_status() {
        let mut s = AppState::default();
        s.photos.push(PhotoState {
            id: 1, path: PathBuf::from("/x/ok.jpg"), source_type: SourceType::Jpeg,
            adjustments: Adjustments::default(), auto_ev: 0.0, full_size: None, meta: PhotoMeta::default(), thumb: None, thumb_size: None,
            histogram: None, status: PhotoStatus::Ready, selected: true,
        });
        s.photos.push(PhotoState {
            id: 2, path: PathBuf::from("/x/bad.jpg"), source_type: SourceType::Jpeg,
            adjustments: Adjustments::default(), auto_ev: 0.0, full_size: None, meta: PhotoMeta::default(), thumb: None, thumb_size: None,
            histogram: None, status: PhotoStatus::Error("boom".into()), selected: true,
        });
        reduce(&mut s, AppEvent::SendFinished(Ok(())));
        // The successfully-sent photo recovers to Ready...
        assert_eq!(s.photos[0].status, PhotoStatus::Ready);
        // ...but the one whose export failed stays visibly Error'd.
        assert_eq!(s.photos[1].status, PhotoStatus::Error("boom".into()));
    }

    #[test]
    fn editing_sets_processing() {
        let mut s = AppState::default();
        s.photos.push(PhotoState {
            id: 7, path: PathBuf::from("/x"), source_type: SourceType::Jpeg,
            adjustments: Adjustments::default(), auto_ev: 0.0, full_size: None, meta: PhotoMeta::default(), thumb: None, thumb_size: None,
            histogram: None, status: PhotoStatus::Ready, selected: true,
        });
        let mut adj = Adjustments::default();
        adj.exposure_mode = ExposureMode::Manual;
        reduce(&mut s, AppEvent::PhotoEdit { id: 7, adjustments: adj });
        assert_eq!(s.photos[0].status, PhotoStatus::Processing);
        assert_eq!(s.photos[0].adjustments.exposure_mode, ExposureMode::Manual);
    }

    #[test]
    fn send_started_marks_selected_exporting() {
        let mut s = AppState::default();
        for (id, selected) in [(1, true), (2, true), (3, false)] {
            s.photos.push(PhotoState {
                id, path: PathBuf::from("/x"), source_type: SourceType::Jpeg,
                adjustments: Adjustments::default(), auto_ev: 0.0, full_size: None, meta: PhotoMeta::default(), thumb: None, thumb_size: None,
                histogram: None, status: PhotoStatus::Ready, selected,
            });
        }
        reduce(&mut s, AppEvent::SendStarted);
        assert_eq!(s.photos[0].status, PhotoStatus::Exporting);
        assert_eq!(s.photos[1].status, PhotoStatus::Exporting);
        assert_eq!(s.photos[2].status, PhotoStatus::Ready); // unselected untouched
        assert!(s.sending);
    }
}
