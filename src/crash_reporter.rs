//! Drop-in crash reporter for Hardwave VST plug-ins.
//!
//! Posts panic payloads to `https://hardwavestudios.com/api/telemetry/crash`
//! so the admin dashboard's "Top crashes" panel populates without anyone
//! reading local log files.
//!
//! Identical contents across every plug-in repo — only the `plugin_slug`
//! passed to `install()` differs. Long-term we should publish this as its
//! own crate; for now it's vendored to keep release cadence per plug-in
//! independent.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

const ENDPOINT: &str = "https://hardwavestudios.com/api/telemetry/crash";
static INIT: Once = Once::new();

/// Install a process-wide panic hook that forwards crash payloads to the
/// telemetry endpoint. Safe to call from multiple `Plugin::initialize`
/// paths — only the first invocation per process actually installs.
pub fn install(plugin_slug: &'static str) {
    INIT.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Always let the previous hook run first — the existing
            // forensic log-file writer is the one that produces detailed
            // stack traces on disk.
            prev(info);

            let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "unknown".to_string());
            let stack = std::backtrace::Backtrace::force_capture().to_string();

            // Best-effort send — never re-panic out of a panic hook.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                send_crash(plugin_slug, &payload, &location, &stack);
            }));
        }));
    });
}

fn send_crash(plugin_slug: &str, message: &str, top_frame: &str, stack: &str) {
    let machine_id = load_or_create_machine_id();
    let stack_hash = compute_stack_hash(plugin_slug, top_frame);

    let body = serde_json::json!({
        "machine_id":  machine_id,
        "plugin_slug": plugin_slug,
        "version":     env!("CARGO_PKG_VERSION"),
        "os":          os_label(),
        "top_frame":   top_frame,
        "message":     message,
        "stack_hash":  stack_hash,
        "stack":       truncate(stack, 8 * 1024),
    });

    // 3-second timeout — the process may be tearing down behind us. ureq is
    // blocking, which is fine in a panic hook.
    let _ = ureq::post(ENDPOINT)
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(3))
        .send_string(&body.to_string());
}

/// Resolve a stable per-machine identifier matching the SHA-256 hex shape
/// expected by `/api/telemetry/crash`. We generate 32 random bytes once and
/// cache them in the Hardwave data directory.
fn load_or_create_machine_id() -> String {
    let path = machine_id_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_lowercase();
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return trimmed;
        }
    }

    let mut bytes = [0u8; 32];
    if let Ok(()) = fill_random(&mut bytes) {
        let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, &hex);
        return hex;
    }

    "0".repeat(64)
}

fn machine_id_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("hardwave")
        .join("crash_machine_id")
}

/// Combine plugin + top frame into a stable 16-hex-char bucket key so the
/// admin dashboard's `GROUP BY stack_hash` rolls identical crashes up.
fn compute_stack_hash(plugin_slug: &str, top_frame: &str) -> String {
    let mut h = DefaultHasher::new();
    plugin_slug.hash(&mut h);
    env!("CARGO_PKG_VERSION").hash(&mut h);
    top_frame.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn os_label() -> &'static str {
    if cfg!(target_os = "windows") {
        if cfg!(target_arch = "aarch64") { "win-arm64" } else { "win-x64" }
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") { "mac-arm64" } else { "mac-intel" }
    } else if cfg!(target_os = "linux") {
        if cfg!(target_arch = "aarch64") { "linux-arm64" } else { "linux-x64" }
    } else {
        "unknown"
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 { end -= 1; }
    s[..end].to_string()
}

#[cfg(unix)]
fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

#[cfg(windows)]
fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    // BCryptGenRandom without an external crate: link directly to the
    // bcrypt.dll function exposed by Windows.
    extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut std::ffi::c_void,
            buffer: *mut u8,
            buffer_size: u32,
            flags: u32,
        ) -> i32;
    }
    const STATUS_SUCCESS: i32 = 0;
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x00000002;
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == STATUS_SUCCESS {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "BCryptGenRandom failed"))
    }
}

#[cfg(not(any(unix, windows)))]
fn fill_random(_buf: &mut [u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no rng available on this platform"))
}
