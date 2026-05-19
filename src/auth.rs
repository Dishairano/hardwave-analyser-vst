//! Token and subscription-cache persistence for the VST webview editor.
//!
//! - JWT auth token: `~/.hardwave/vst-token`
//! - Subscription cache: `~/.hardwave/sub-cache`
//!
//! The sub-cache stores a server-issued Ed25519-signed token (not a plain
//! timestamp). The private key never leaves the server. The public key is
//! embedded here. Forging the cache requires the server's private key —
//! editing the file on disk is not enough.

use std::fs;
use std::path::PathBuf;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Ed25519 public key — generated once, private key stored in server .env only.
/// To rotate: generate a new keypair, update this constant, redeploy server with new private key.
const PUBLIC_KEY: [u8; 32] = [
    0x22, 0x8b, 0xb9, 0x2b, 0x1b, 0x54, 0x81, 0x12,
    0x3b, 0x57, 0x78, 0x3f, 0x2b, 0xc4, 0x9a, 0x94,
    0xd5, 0xb6, 0x0b, 0xac, 0xcb, 0x0c, 0x05, 0xa6,
    0x20, 0x58, 0xef, 0x5b, 0xc8, 0x23, 0x32, 0xef,
];

fn hardwave_dir() -> Option<PathBuf> {
    // Primary: ~/.hardwave (works on all platforms)
    if let Some(home) = dirs::home_dir() {
        return Some(home.join(".hardwave"));
    }
    // Windows fallback: %APPDATA%\.hardwave if home_dir() fails
    #[cfg(target_os = "windows")]
    if let Ok(appdata) = std::env::var("APPDATA") {
        return Some(PathBuf::from(appdata).join(".hardwave"));
    }
    debug_log("[auth] hardwave_dir: could not determine home directory");
    None
}

/// Write a line to the debug log (same file as editor.rs).
#[allow(unused)]
fn debug_log(msg: &str) {
    use std::io::Write;
    let path = {
        let mut p = std::env::temp_dir();
        p.push("hardwave-debug.log");
        p
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(f, "[{}] {}", now, msg);
    }
}

fn token_path() -> Option<PathBuf> {
    hardwave_dir().map(|d| d.join("vst-token"))
}

/// Token path written by the Hardwave Suite (Tauri app) on login/logout.
/// Used as a fallback so users logged into the Suite don't need a separate
/// Analyser login.
fn suite_token_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("hardwave").join("auth_token"))
}

fn sub_cache_path() -> Option<PathBuf> {
    hardwave_dir().map(|d| d.join("sub-cache"))
}

/// Load a previously-saved JWT token from disk.
///
/// Checks two locations and returns the **most recently modified** token so
/// that a fresh Suite login always wins over a stale VST-saved token:
///   - `~/.hardwave/vst-token` — written by the Analyser itself after login.
///   - `data_dir/hardwave/auth_token` — written by the Hardwave Suite on login.
pub fn load_token() -> Option<String> {
    let candidates: &[Option<PathBuf>] = &[token_path(), suite_token_path()];

    let mut best: Option<(String, std::time::SystemTime, PathBuf)> = None;

    for path_opt in candidates {
        let Some(path) = path_opt else { continue };
        let content = match fs::read_to_string(path) {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                debug_log(&format!("[auth] load_token: read failed {:?}: {}", path, e));
                continue;
            }
        };
        if content.is_empty() {
            debug_log(&format!("[auth] load_token: file empty at {:?}", path));
            continue;
        }
        let mtime = fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);

        match &best {
            Some((_, prev_mtime, _)) if mtime <= *prev_mtime => {
                debug_log(&format!("[auth] load_token: skipping older {:?}", path));
            }
            _ => {
                debug_log(&format!("[auth] load_token: candidate {} bytes from {:?}", content.len(), path));
                best = Some((content, mtime, path.clone()));
            }
        }
    }

    match best {
        Some((token, _, path)) => {
            debug_log(&format!("[auth] load_token: using {:?}", path));
            Some(token)
        }
        None => None,
    }
}

/// Save a JWT token to disk.
pub fn save_token(token: &str) {
    if let Some(path) = token_path() {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                debug_log(&format!("[auth] save_token: mkdir failed {:?}: {}", parent, e));
                return;
            }
        }
        match fs::write(&path, token) {
            Ok(()) => {
                debug_log(&format!("[auth] save_token: wrote {} bytes to {:?}", token.len(), path));
            }
            Err(e) => {
                debug_log(&format!("[auth] save_token: write failed {:?}: {}", path, e));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    } else {
        debug_log("[auth] save_token: no token path available");
    }
}

/// Delete the VST-saved token from disk (called when server rejects it).
pub fn clear_token() {
    if let Some(path) = token_path() {
        if path.exists() {
            match fs::remove_file(&path) {
                Ok(()) => debug_log(&format!("[auth] clear_token: deleted {:?}", path)),
                Err(e) => debug_log(&format!("[auth] clear_token: failed {:?}: {}", path, e)),
            }
        }
    }
}

/// Verify and load the subscription cache.
///
/// The cache file contains a server-signed offline token in the format:
///   `<base64_signature>.<base64_payload>`
/// where payload is the UTF-8 string `<userId>:<exp_unix_secs>`.
///
/// Returns `true` only if:
///   1. The file exists and parses correctly
///   2. The Ed25519 signature is valid against the embedded public key
///   3. The expiry timestamp has not passed
///
/// Called at plugin startup — result is injected as `window.__HARDWAVE_SUB_VALID`
/// before the WebView loads. JS reads it but cannot set it from DevTools.
pub fn load_sub_cache() -> bool {
    let raw = match sub_cache_path().and_then(|p| fs::read_to_string(p).ok()) {
        Some(s) => s.trim().to_string(),
        None => return false,
    };

    let (sig_b64, payload_b64) = match raw.split_once('.') {
        Some(parts) => parts,
        None => return false,
    };

    let sig_bytes = match base64_decode(sig_b64) {
        Some(b) => b,
        None => return false,
    };
    let payload_bytes = match base64_decode(payload_b64) {
        Some(b) => b,
        None => return false,
    };

    // Verify Ed25519 signature
    let vk = match VerifyingKey::from_bytes(&PUBLIC_KEY) {
        Ok(k) => k,
        Err(_) => return false,
    };
    let sig_arr: [u8; 64] = match sig_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let sig = Signature::from_bytes(&sig_arr);
    if vk.verify(&payload_bytes, &sig).is_err() {
        return false;
    }

    // Parse payload: "<userId>:<exp>"
    let payload_str = match std::str::from_utf8(&payload_bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let exp: u64 = match payload_str.split_once(':').and_then(|(_, e)| e.parse().ok()) {
        Some(t) => t,
        None => return false,
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    now < exp
}

/// Save a server-signed offline token to disk, or delete the cache if empty.
/// The token format is `<base64_sig>.<base64_payload>` — produced by the server.
pub fn save_sub_cache(signed_token: &str) {
    let path = match sub_cache_path() {
        Some(p) => p,
        None => return,
    };
    if signed_token.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, signed_token);
}

// ---------------------------------------------------------------------------
// Preset state persistence (filesystem fallback for DAWs like FL Studio
// that don't reliably save/restore VST3 custom state via IBStream).
// ---------------------------------------------------------------------------

fn preset_state_path() -> Option<PathBuf> {
    hardwave_dir().map(|d| d.join("preset-state"))
}

/// Read the preset state JSON from disk. Returns None if the file doesn't
/// exist or can't be read.
pub fn load_preset_state() -> Option<String> {
    let path = preset_state_path()?;
    match fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        }
        Err(e) => {
            debug_log(&format!("[auth] load_preset_state: read failed {:?}: {}", path, e));
            None
        }
    }
}

/// Write the preset state JSON to disk. The state is shared across all
/// instances (not per-project) since FL Studio drops VST3 IBStream state.
pub fn save_preset_state(json: &str) {
    let path = match preset_state_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            debug_log(&format!("[auth] save_preset_state: mkdir failed {:?}: {}", parent, e));
            return;
        }
    }
    match fs::write(&path, json) {
        Ok(()) => debug_log(&format!("[auth] save_preset_state: wrote {} bytes to {:?}", json.len(), path)),
        Err(e) => debug_log(&format!("[auth] save_preset_state: write failed {:?}: {}", path, e)),
    }
}

/// Minimal base64 decode (standard alphabet, handles padding).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}
