//! Application configuration. Task 22 adds TOML persistence; for now the Telegram
//! worker gets its credentials from the environment (with placeholders) and a
//! default session path under the XDG data dir.
use std::path::PathBuf;

/// Telegram app credentials for the grammers worker. Overridable via env so a
/// no-network host can still boot the app and surface the connect error as a toast.
pub fn telegram_credentials() -> (i32, String) {
    let api_id = std::env::var("TG_API_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let api_hash = std::env::var("TG_API_HASH").unwrap_or_default();
    (api_id, api_hash)
}

/// Where the grammers sqlite session file lives.
pub fn session_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("photoup2")
        .join("telegram.session")
}
