pub mod config;
pub mod errors;
pub mod image;
pub mod state;
pub mod telegram;
pub mod ui;

// Re-export the version at the crate root so `--version` works from main.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
