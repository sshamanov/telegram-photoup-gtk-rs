use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub target_peer_id: Option<i64>,
    pub session_path: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("photoup2");
        Self {
            api_id: 0,
            api_hash: String::new(),
            target_peer_id: None,
            session_path: base.join("telegram.session"),
        }
    }
}

impl AppConfig {
    pub fn path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("photoup2").join("config.toml")
    }

    pub fn load() -> Self {
        match Self::load_from(&Self::path()) {
            Ok(cfg) => cfg,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                let _ = cfg.save();
                cfg
            }
            Err(_) => {
                // Corrupt/unreadable config: keep the file, surface a clear default
                // (the user is told which file to fix).
                log::error!("config at {} is unreadable — using defaults", Self::path().display());
                Self::default()
            }
        }
    }

    /// Load from an explicit path (test-friendly; never touches the real config).
    pub fn load_from(path: &PathBuf) -> std::result::Result<Self, std::io::Error> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &PathBuf) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.session_path = dir.path().join("s");
        cfg.api_id = 123;
        cfg.target_peer_id = Some(42);
        cfg.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.api_id, 123);
        assert_eq!(loaded.target_peer_id, Some(42));
    }
}
