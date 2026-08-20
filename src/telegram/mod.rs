//! Telegram behind an adapter (mpd-client's adapter pattern; photoup used the same).
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct DialogInfo {
    pub id: i64,
    pub title: String,
    pub is_group: bool,
}

#[derive(Debug, Clone)]
pub struct SentResult {
    pub message_ids: Vec<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStep {
    NotStarted,
    AwaitingCode,
    AwaitingPassword,
    Ready,
}

/// What the Telegram worker can be asked to do. This is the contract the UI and the
/// mock both speak.
pub enum TCommand {
    /// Start login: request the SMS code for `phone`. Emits `TEvent::CodeRequested`.
    RequestCode { phone: String },
    /// Submit the login code. May emit `TEvent::PasswordRequired` or `TEvent::Ready`.
    SubmitCode { code: String },
    /// Submit the 2FA password.
    SubmitPassword { password: String },
    /// Load dialogs → `TEvent::Dialogs(Vec<DialogInfo>)`.
    LoadDialogs,
    /// Upload one photo file and send it (single). Returns `TEvent::Sent`.
    SendPhoto { peer_id: i64, path: PathBuf, caption: Option<String> },
    /// Upload several photos and send them as an album.
    SendAlbum { peer_id: i64, paths: Vec<PathBuf>, caption: Option<String> },
    /// Check whether the session is already authorized.
    CheckAuth,
}

pub enum TEvent {
    AuthStep(AuthStep),
    CodeRequested,
    PasswordRequired { hint: Option<String> },
    AuthFailed(String),
    Dialogs(Vec<DialogInfo>),
    Sent { ok: usize, failed: Vec<PathBuf> },
    Error(String),
}

pub trait TelegramAdapter: Send + 'static {
    fn handle(&mut self, cmd: TCommand) -> Vec<TEvent>;
}
