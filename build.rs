//! Bakes the Telegram API credentials into the binary. `TG_API_ID` /
//! `TG_API_HASH` come from the build environment, else from the gitignored
//! `.env` next to `Cargo.toml`. Missing values bake in as empty; the app then
//! tells the user it was built without keys.

use std::path::Path;

fn main() {
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    println!("cargo:rerun-if-changed={}", env_path.display());
    println!("cargo:rerun-if-env-changed=TG_API_ID");
    println!("cargo:rerun-if-env-changed=TG_API_HASH");

    let dotenv = std::fs::read_to_string(&env_path).unwrap_or_default();
    for key in ["TG_API_ID", "TG_API_HASH"] {
        let value = std::env::var(key)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| dotenv_value(&dotenv, key))
            .unwrap_or_default();
        if key == "TG_API_ID" && !value.is_empty() && value.parse::<i32>().is_err() {
            panic!("TG_API_ID must be an integer, got {value:?}");
        }
        println!("cargo:rustc-env=PHOTOUP2_{key}={value}");
    }
}

/// `KEY=value` lookup: skips blanks and `#` comments, strips an optional
/// `export ` prefix and matching surrounding quotes.
fn dotenv_value(text: &str, key: &str) -> Option<String> {
    text.lines().map(str::trim).filter(|l| !l.starts_with('#')).find_map(|l| {
        let (k, v) = l.strip_prefix("export ").unwrap_or(l).split_once('=')?;
        if k.trim() != key {
            return None;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(v);
        Some(v.to_string()).filter(|v| !v.is_empty())
    })
}
