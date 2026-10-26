//! Token and subscription-cache persistence for the VST webview editor.
//!
//! - JWT auth token: `data_dir/hardwave/auth_token`, shared with the Suite and every plug-in
//!   (`~/.hardwave/vst-token` from older builds is still read)
//! - Subscription cache: `~/.hardwave/sub-cache`
//!
//! The sub-cache stores a server-issued Ed25519-signed token (not a plain
//! timestamp). The private key never leaves the server. The public key is
//! embedded here. Forging the cache requires the server's private key —
//! editing the file on disk is not enough.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Ed25519 public key — generated once, private key stored in server .env only.
/// To rotate: generate a new keypair, update this constant, redeploy server with new private key.
const PUBLIC_KEY: [u8; 32] = [
    0x22, 0x8b, 0xb9, 0x2b, 0x1b, 0x54, 0x81, 0x12, 0x3b, 0x57, 0x78, 0x3f, 0x2b, 0xc4, 0x9a, 0x94,
    0xd5, 0xb6, 0x0b, 0xac, 0xcb, 0x0c, 0x05, 0xa6, 0x20, 0x58, 0xef, 0x5b, 0xc8, 0x23, 0x32, 0xef,
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
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(f, "[{}] {}", now, msg);
    }
}

/// The token file older Analyser builds wrote after a sign-in in the plug-in window.
///
/// Nothing writes it any more, but it is still read: a person who signed in with an older build
/// has their only token there. KickForge reads it before the shared file, so a sign-in or sign-out
/// here removes it; a token left in it would hide the newer one in the shared file.
fn legacy_token_path() -> Option<PathBuf> {
    hardwave_dir().map(|d| d.join("vst-token"))
}

/// The sign-in shared by the Hardwave Suite and every plug-in: whoever signs in writes it,
/// everyone reads it.
fn shared_token_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("hardwave").join("auth_token"))
}

fn sub_cache_path() -> Option<PathBuf> {
    hardwave_dir().map(|d| d.join("sub-cache"))
}

/// The longest token accepted from the page or from a file. A Hardwave JWT is a few hundred bytes.
const MAX_TOKEN_LEN: usize = 8 * 1024;

/// Does `token` look like a JWT: three base64url parts, nothing else, of a sane length?
///
/// Tokens come from files other programs write and from the page, so neither is trusted to be
/// well formed. Anything that is not shaped like a JWT is never written and never handed on.
pub fn is_well_formed(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_LEN
        && token.split('.').count() == 3
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'='))
}

/// The `exp` claim of a JWT, in Unix seconds.
///
/// `None` when the token is not a JWT or its payload cannot be read; `Some(None)` when the payload
/// is readable and has no `exp`. The signature is not checked: the server does that. This only
/// decides which of the tokens on disk is worth handing to the page.
fn jwt_exp(token: &str) -> Option<Option<u64>> {
    use base64::Engine as _;
    if !is_well_formed(token) {
        return None;
    }
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if !claims.is_object() {
        return None;
    }
    Some(claims.get("exp").and_then(serde_json::Value::as_u64))
}

/// Of the tokens found on disk, the one to use: the newest that has not expired.
///
/// `candidates` are each file's contents and when it was last written. A token that is not a
/// readable JWT, or whose `exp` is at or before `now`, is skipped however new it is. A token
/// without an `exp` is kept: only the server can say whether it is still good.
fn choose_token(candidates: Vec<(String, SystemTime)>, now: u64) -> Option<String> {
    candidates
        .into_iter()
        .filter(|(token, _)| match jwt_exp(token) {
            None => false,
            Some(Some(exp)) => exp > now,
            Some(None) => true,
        })
        .max_by_key(|(_, written)| *written)
        .map(|(token, _)| token)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The contents of a token file and when it was written, or `None` when there is nothing usable.
fn read_token_file(path: &Path) -> Option<(String, SystemTime)> {
    let meta = fs::metadata(path).ok()?;
    // Read nothing bigger than a token can be: the file is not ours alone.
    if meta.len() > MAX_TOKEN_LEN as u64 + 16 {
        debug_log(&format!("[auth] token file too large, ignored: {:?}", path));
        return None;
    }
    let content = fs::read_to_string(path).ok()?;
    let token = content.trim();
    if token.is_empty() {
        return None;
    }
    let written = meta.modified().unwrap_or(UNIX_EPOCH);
    Some((token.to_string(), written))
}

/// The sign-in to hand to the page: the newest token on disk that has not expired.
///
/// Read from the shared file the Suite and every plug-in write, and from the file older Analyser
/// builds wrote. Called each time a window opens, so a sign-in made in the Suite or in another
/// plug-in window since this instance was created is picked up.
pub fn load_token() -> Option<String> {
    let candidates: Vec<(String, SystemTime)> = [shared_token_path(), legacy_token_path()]
        .iter()
        .flatten()
        .filter_map(|path| read_token_file(path))
        .collect();
    let found = candidates.len();
    let token = choose_token(candidates, now_secs());
    debug_log(&format!(
        "[auth] load_token: {} token file(s), {}",
        found,
        if token.is_some() {
            "one is usable"
        } else {
            "none usable"
        }
    ));
    token
}

/// Write `token` to `path` so only this user can read it (0600 on macOS and Linux; the user's
/// AppData folder already is on Windows).
///
/// Written to a file beside it and renamed over it, so the Suite or another plug-in reading at
/// the same moment sees the old token or the new one, never half of one.
fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut f| {
        f.write_all(token.as_bytes())?;
        f.sync_all()
    });
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    {
        // The mode above only applies to a file the call created; one left over from a crash
        // keeps its own.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Remove the token file at `path` if it holds `held`, or whatever it holds when `held` is `None`.
///
/// A window signs out the token it was given. If the Suite or another window has written a newer
/// one since, that sign-in is someone else's and stays. A file that is already gone is fine.
fn remove_token_file(path: &Path, held: Option<&str>) -> std::io::Result<()> {
    if let Some(held) = held {
        match fs::read_to_string(path) {
            Ok(on_disk) if on_disk.trim() != held => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Save a sign-in made in the plug-in window to the shared file, so the Suite and every other
/// plug-in window open signed in. Returns whether it was written.
///
/// A token that is not shaped like a JWT is refused: it comes from the page.
pub fn save_token(token: &str) -> bool {
    if !is_well_formed(token) {
        debug_log("[auth] save_token: refused, not a well-formed token");
        return false;
    }
    let Some(path) = shared_token_path() else {
        debug_log("[auth] save_token: no data folder on this system");
        return false;
    };
    if let Err(e) = write_token_file(&path, token) {
        debug_log(&format!(
            "[auth] save_token: write failed {:?}: {}",
            path, e
        ));
        return false;
    }
    debug_log(&format!(
        "[auth] save_token: wrote {} bytes to {:?}",
        token.len(),
        path
    ));
    // The older file would otherwise hide this sign-in from a reader that prefers it.
    if let Some(legacy) = legacy_token_path() {
        let _ = remove_token_file(&legacy, None);
    }
    true
}

/// Sign out: remove `held`, the token this window had, from the shared file and the older one.
///
/// Called when the person signs out in the window or the server rejects the token.
pub fn clear_token(held: Option<&str>) {
    for path in [shared_token_path(), legacy_token_path()].iter().flatten() {
        if let Err(e) = remove_token_file(path, held) {
            debug_log(&format!("[auth] clear_token: failed {:?}: {}", path, e));
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
    let exp: u64 = match payload_str
        .split_once(':')
        .and_then(|(_, e)| e.parse().ok())
    {
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
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(e) => {
            debug_log(&format!(
                "[auth] load_preset_state: read failed {:?}: {}",
                path, e
            ));
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
            debug_log(&format!(
                "[auth] save_preset_state: mkdir failed {:?}: {}",
                parent, e
            ));
            return;
        }
    }
    match fs::write(&path, json) {
        Ok(()) => debug_log(&format!(
            "[auth] save_preset_state: wrote {} bytes to {:?}",
            json.len(),
            path
        )),
        Err(e) => debug_log(&format!(
            "[auth] save_preset_state: write failed {:?}: {}",
            path, e
        )),
    }
}

/// Minimal base64 decode (standard alphabet, handles padding).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// What is hosting this plug-in, when we can tell.
///
/// Our own plug-ins are free inside the Hardwave DAW and paid in
/// every other host, so the window has to be able to say which it is
/// in. The DAW marks its own process before anything loads, and a
/// plug-in loaded into it inherits that.
///
/// Nothing is claimed when the variable is absent: a plug-in in FL or
/// Ableton simply says nothing, and the page falls back to the
/// licence the user has.
pub fn host_kind() -> Option<String> {
    if let Ok(host) = std::env::var("HARDWAVE_HOST") {
        let host = host.trim().to_lowercase();
        if !host.is_empty() {
            return Some(host);
        }
    }
    // The same answer by another road, for a host that strips the
    // environment: the program we are loaded into is named after
    // itself.
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_stem()?.to_string_lossy().to_lowercase();
    name.contains("hardwave-daw")
        .then(|| "hardwave-daw".to_string())
}

/// `&host=...` for the window's URL, or nothing when we are a guest
/// in someone else's host.
pub fn host_query(separator: char) -> String {
    match host_kind() {
        Some(host) => format!("{separator}host={host}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use std::time::Duration;

    const NOW: u64 = 1_800_000_000;

    /// A JWT whose payload is `claims`; header and signature are filler the plug-in never reads.
    fn jwt(claims: &str) -> String {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
        format!("eyJhbGciOiJIUzI1NiJ9.{payload}.c2lnbmF0dXJl")
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A folder of its own under the system temp folder, empty, for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hardwave-analyser-auth-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a scratch folder");
        dir
    }

    #[test]
    fn the_newest_valid_token_wins() {
        let old = jwt(&format!(r#"{{"sub":"1","exp":{}}}"#, NOW + 100));
        let new = jwt(&format!(r#"{{"sub":"2","exp":{}}}"#, NOW + 50));
        let chosen = choose_token(vec![(old, at(10)), (new.clone(), at(20))], NOW);
        assert_eq!(chosen, Some(new), "the one written last is the one to use");
    }

    #[test]
    fn an_expired_token_is_skipped_however_new() {
        let valid = jwt(&format!(r#"{{"exp":{}}}"#, NOW + 3600));
        let expired = jwt(&format!(r#"{{"exp":{}}}"#, NOW - 1));
        let on_the_dot = jwt(&format!(r#"{{"exp":{NOW}}}"#));
        let chosen = choose_token(
            vec![
                (valid.clone(), at(10)),
                (expired, at(30)),
                (on_the_dot, at(20)),
            ],
            NOW,
        );
        assert_eq!(chosen, Some(valid));
    }

    #[test]
    fn nothing_usable_gives_no_token() {
        let expired = jwt(&format!(r#"{{"exp":{}}}"#, NOW - 10));
        assert_eq!(choose_token(vec![(expired, at(10))], NOW), None);
        assert_eq!(choose_token(Vec::new(), NOW), None);
    }

    #[test]
    fn a_token_that_is_not_a_jwt_is_skipped() {
        let valid = jwt(&format!(r#"{{"exp":{}}}"#, NOW + 60));
        let chosen = choose_token(
            vec![
                (valid.clone(), at(1)),
                ("not a token".to_string(), at(5)),
                ("a.b".to_string(), at(6)),
                ("a.!!!.c".to_string(), at(7)),
                (jwt("[1,2,3]"), at(8)),
                (jwt("not json"), at(9)),
            ],
            NOW,
        );
        assert_eq!(chosen, Some(valid));
    }

    #[test]
    fn a_token_without_an_expiry_is_left_to_the_server() {
        let no_exp = jwt(r#"{"sub":"1"}"#);
        assert_eq!(
            choose_token(vec![(no_exp.clone(), at(1))], NOW),
            Some(no_exp)
        );
    }

    #[test]
    fn only_jwt_shaped_tokens_are_well_formed() {
        assert!(is_well_formed(&jwt(r#"{"exp":1}"#)));
        assert!(!is_well_formed(""));
        assert!(!is_well_formed("abc"));
        assert!(!is_well_formed("a.b.c\n"));
        assert!(!is_well_formed("a.b.c\"<script>"));
        assert!(!is_well_formed(&format!(
            "a.{}.c",
            "b".repeat(MAX_TOKEN_LEN)
        )));
    }

    #[test]
    fn a_written_token_reads_back_and_replaces_the_old_one() {
        let dir = scratch("write");
        let path = dir.join("hardwave").join("auth_token");
        write_token_file(&path, "first.token.x").expect("the first write");
        write_token_file(&path, "second.token.y").expect("the second write");
        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            "second.token.y"
        );
        let left: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 1, "no temporary file left behind: {left:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_written_token_is_readable_by_this_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let path = dir.join("auth_token");
        // A file left readable by everyone is tightened, not kept as it was.
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_token_file(&path, "a.b.c").expect("the write");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode {mode:o}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn signing_out_removes_the_token_the_window_held() {
        let dir = scratch("remove");
        let path = dir.join("auth_token");
        write_token_file(&path, "held.token.x").unwrap();
        remove_token_file(&path, Some("held.token.x")).expect("the removal");
        assert!(!path.exists());
        // Already gone is not an error.
        remove_token_file(&path, Some("held.token.x")).expect("a second removal");
        remove_token_file(&path, None).expect("a removal of nothing");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn signing_out_leaves_a_newer_sign_in_alone() {
        let dir = scratch("keep");
        let path = dir.join("auth_token");
        write_token_file(&path, "newer.suite.token").unwrap();
        remove_token_file(&path, Some("older.window.token")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "newer.suite.token");
        remove_token_file(&path, None).unwrap();
        assert!(!path.exists(), "with no token held, the file goes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_token_file_is_read_trimmed_and_an_empty_one_is_nothing() {
        let dir = scratch("read");
        let path = dir.join("auth_token");
        fs::write(&path, "  a.b.c\n").unwrap();
        assert_eq!(read_token_file(&path).map(|(t, _)| t), Some("a.b.c".into()));
        fs::write(&path, "\n").unwrap();
        assert_eq!(read_token_file(&path), None);
        fs::write(&path, "x".repeat(MAX_TOKEN_LEN * 2)).unwrap();
        assert_eq!(read_token_file(&path), None, "an oversized file is ignored");
        assert_eq!(read_token_file(&dir.join("missing")), None);
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;

    #[test]
    fn the_host_is_named_only_when_it_is_known() {
        if std::env::var("HARDWAVE_HOST").is_err() {
            assert!(
                host_query('&').is_empty(),
                "a plug-in in someone else's host claims nothing"
            );
        }
    }
}
