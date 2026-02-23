//! Token persistence for the VST webview editor.
//!
//! Stores the user's JWT at `~/.hardwave/vst-token` so they don't have to
//! log in every time the plugin window is opened.

use std::fs;
use std::path::PathBuf;

/// Get the path to the token file.
fn token_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".hardwave").join("vst-token"))
}

/// Load a previously-saved JWT token from disk.
pub fn load_token() -> Option<String> {
    let path = token_path()?;
    fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Save a JWT token to disk.
pub fn save_token(token: &str) {
    if let Some(path) = token_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, token);
    }
}
