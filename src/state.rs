use std::path::PathBuf;
use crate::image::types::{Adjustments, SourceType};

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
    PhotoThumbReady { id: u64, rgba: Vec<u8>, size: (u32, u32), auto_ev: f32, histogram: Vec<u32> },
    PhotoFailed { id: u64, msg: String },
    PhotoEdit { id: u64, adjustments: Adjustments },
    PhotoSelected { id: u64, selected: bool },
    ActivePhoto { index: Option<usize> },
    TargetPeer(TargetPeer),
    SendStarted,
    SendFinished(Result<(), String>),
    Usage(UsageStats),
}

#[derive(Debug)]
pub enum AuthEvent {
    PhoneRequested,
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
        AppEvent::PhotoThumbReady { id, rgba, size, auto_ev, histogram } => {
            if let Some(p) = state.photos.iter_mut().find(|p| p.id == id) {
                p.thumb = Some(rgba);
                p.thumb_size = Some(size);
                p.auto_ev = auto_ev;
                p.histogram = Some(histogram);
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
        AppEvent::TargetPeer(peer) => state.telegram.target_peer = Some(peer),
        AppEvent::SendStarted => state.sending = true,
        AppEvent::SendFinished(result) => {
            state.sending = false;
            if result.is_ok() {
                for p in state.photos.iter_mut() {
                    if p.selected { p.status = PhotoStatus::Ready; }
                }
            }
        }
        AppEvent::Usage(u) => state.usage = u,
        AppEvent::Auth(ev) => match ev {
            AuthEvent::PhoneRequested => state.telegram.status = AuthStatus::AwaitingCode,
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
            thumb: None,
            thumb_size: None,
            histogram: None,
            status: PhotoStatus::Queued,
            selected: true,
        };
        reduce(&mut s, AppEvent::PhotosAdded(vec![p]));
        assert_eq!(s.photos.len(), 1);
        reduce(&mut s, AppEvent::PhotoThumbReady { id: 1, rgba: vec![0u8; 4], size: (1, 1), auto_ev: 0.5, histogram: vec![0; 256] });
        assert_eq!(s.photos[0].status, PhotoStatus::Ready);
        assert_eq!(s.photos[0].auto_ev, 0.5);
    }

    #[test]
    fn photo_failed_sets_error_status() {
        let mut s = AppState::default();
        s.photos.push(PhotoState {
            id: 3, path: PathBuf::from("/x"), source_type: SourceType::Jpeg,
            adjustments: Adjustments::default(), auto_ev: 0.0, thumb: None, thumb_size: None,
            histogram: None, status: PhotoStatus::Processing, selected: true,
        });
        reduce(&mut s, AppEvent::PhotoFailed { id: 3, msg: "decode boom".into() });
        assert_eq!(s.photos[0].status, PhotoStatus::Error("decode boom".into()));
    }

    #[test]
    fn editing_sets_processing() {
        let mut s = AppState::default();
        s.photos.push(PhotoState {
            id: 7, path: PathBuf::from("/x"), source_type: SourceType::Jpeg,
            adjustments: Adjustments::default(), auto_ev: 0.0, thumb: None, thumb_size: None,
            histogram: None, status: PhotoStatus::Ready, selected: true,
        });
        let mut adj = Adjustments::default();
        adj.exposure_mode = ExposureMode::Manual;
        reduce(&mut s, AppEvent::PhotoEdit { id: 7, adjustments: adj });
        assert_eq!(s.photos[0].status, PhotoStatus::Processing);
        assert_eq!(s.photos[0].adjustments.exposure_mode, ExposureMode::Manual);
    }
}
