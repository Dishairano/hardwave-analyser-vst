//! Webview-based editor for Hardwave Analyser.
//!
//! Embeds a wry `WebView` that loads the Hardwave analyser page.
//! On Windows, FFT data is delivered via a local HTTP server (TcpListener
//! on a random port) that JS polls at ~60fps. This avoids both the STA
//! threading restriction on ICoreWebView2::ExecuteScript and the wry
//! custom-protocol interception issues in wry 0.46.

use nih_plug::prelude::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use wry::raw_window_handle as rwh06;

use crate::auth;
use crate::params::HardwaveAnalyserParams;
use crate::protocol::AudioPacket;

/// Write a debug line to %TEMP%\hardwave-debug.log (Windows) or /tmp/hardwave-debug.log.
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

/// Stopwatch that records named checkpoints with millisecond timestamps.
/// Call `mark("label")` at each step, then `dump()` to write the full
/// timeline to the debug log so we can see where time is spent.
struct Stopwatch {
    start: std::time::Instant,
    marks: Vec<(String, u128)>,
}

impl Stopwatch {
    fn new(label: &str) -> Self {
        let start = std::time::Instant::now();
        Self { start, marks: vec![(label.to_string(), 0)] }
    }

    fn mark(&mut self, label: &str) {
        let elapsed = self.start.elapsed().as_millis();
        self.marks.push((label.to_string(), elapsed));
    }

    fn dump(&mut self, label: &str) {
        self.mark(label);
        let mut lines = vec![format!("=== SPAWN TIMING ===")];
        let mut prev = 0u128;
        for (name, ms) in &self.marks {
            lines.push(format!("  {:>6} ms  (+{} ms)  {}", ms, ms - prev, name));
            prev = *ms;
        }
        lines.push(format!("===================="));
        debug_log(&lines.join("\n"));
    }
}


#[cfg(target_os = "windows")] const PLUGIN_OS: &str = "windows";
#[cfg(target_os = "macos")]   const PLUGIN_OS: &str = "macos";
#[cfg(target_os = "linux")]   const PLUGIN_OS: &str = "linux";
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
const PLUGIN_OS: &str = "unknown";

// ---------------------------------------------------------------------------
// WebView2 auto-install (Windows only)
// ---------------------------------------------------------------------------

/// Cached result of the WebView2 presence check. Spawning reg.exe is slow
/// (~2-3 s each with AV scanning). We only need to check once per DAW session.
#[cfg(target_os = "windows")]
static WEBVIEW2_ENSURED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "windows")]
pub(crate) fn ensure_webview2() {
    // Already confirmed present this session — skip the slow reg.exe checks.
    if WEBVIEW2_ENSURED.load(Ordering::Relaxed) { return; }

    use std::process::Command;

    let installed = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
            "/v", "pv",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if installed {
        WEBVIEW2_ENSURED.store(true, Ordering::Relaxed);
        return;
    }

    let installed_user = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
            "/v", "pv",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if installed_user {
        WEBVIEW2_ENSURED.store(true, Ordering::Relaxed);
        return;
    }

    nih_log!("WebView2 Runtime not found — downloading bootstrapper...");

    let temp_dir = std::env::temp_dir();
    let bootstrapper_path = temp_dir.join("MicrosoftEdgeWebview2Setup.exe");

    let download = Command::new("powershell")
        .env("HW_OUTFILE", &bootstrapper_path)
        .args([
            "-NoProfile",
            "-Command",
            "Invoke-WebRequest -Uri 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $env:HW_OUTFILE",
        ])
        .output();

    match download {
        Ok(output) if output.status.success() => {
            nih_log!("Installing WebView2 Runtime silently...");
            let _ = Command::new(&bootstrapper_path)
                .args(["/silent", "/install"])
                .output();
            let _ = std::fs::remove_file(&bootstrapper_path);
            WEBVIEW2_ENSURED.store(true, Ordering::Relaxed);
        }
        _ => {
            nih_log!("Failed to download WebView2 bootstrapper");
        }
    }
}

/// Default editor size.
const EDITOR_WIDTH: u32 = 1100;
const EDITOR_HEIGHT: u32 = 700;
const MIN_WIDTH: u32 = 800;
const MIN_HEIGHT: u32 = 500;
const MAX_WIDTH: u32 = 2560;
const MAX_HEIGHT: u32 = 1600;

/// Base URL for the analyser page.
/// Points directly at the analyser subdomain to skip the 307 redirect from
/// the main domain — faster load and guaranteed query-param preservation.
const ANALYSER_URL: &str = "https://analyser.hardwavestudios.com/vst/analyser";

// ---------------------------------------------------------------------------
// raw-window-handle 0.5 (nih-plug) → 0.6 (wry) bridge
// ---------------------------------------------------------------------------

struct RwhWrapper(ParentWindowHandle);

impl rwh06::HasWindowHandle for RwhWrapper {
    fn window_handle(&self) -> Result<rwh06::WindowHandle<'_>, rwh06::HandleError> {
        let raw = match self.0 {
            ParentWindowHandle::X11Window(window) => {
                let handle = rwh06::XcbWindowHandle::new(
                    std::num::NonZeroU32::new(window)
                        .ok_or(rwh06::HandleError::Unavailable)?,
                );
                rwh06::RawWindowHandle::Xcb(handle)
            }
            ParentWindowHandle::AppKitNsView(ns_view) => {
                let handle = rwh06::AppKitWindowHandle::new(
                    std::ptr::NonNull::new(ns_view)
                        .ok_or(rwh06::HandleError::Unavailable)?,
                );
                rwh06::RawWindowHandle::AppKit(handle)
            }
            ParentWindowHandle::Win32Hwnd(hwnd) => {
                let handle = rwh06::Win32WindowHandle::new(
                    std::num::NonZeroIsize::new(hwnd as isize)
                        .ok_or(rwh06::HandleError::Unavailable)?,
                );
                rwh06::RawWindowHandle::Win32(handle)
            }
        };
        Ok(unsafe { rwh06::WindowHandle::borrow_raw(raw) })
    }
}

impl rwh06::HasDisplayHandle for RwhWrapper {
    fn display_handle(&self) -> Result<rwh06::DisplayHandle<'_>, rwh06::HandleError> {
        #[cfg(target_os = "linux")]
        {
            Ok(unsafe {
                rwh06::DisplayHandle::borrow_raw(rwh06::RawDisplayHandle::Xcb(
                    rwh06::XcbDisplayHandle::new(None, 0),
                ))
            })
        }
        #[cfg(target_os = "macos")]
        {
            Ok(unsafe {
                rwh06::DisplayHandle::borrow_raw(rwh06::RawDisplayHandle::AppKit(
                    rwh06::AppKitDisplayHandle::new(),
                ))
            })
        }
        #[cfg(target_os = "windows")]
        {
            Ok(unsafe {
                rwh06::DisplayHandle::borrow_raw(rwh06::RawDisplayHandle::Windows(
                    rwh06::WindowsDisplayHandle::new(),
                ))
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ParentData {
    X11(u32),
    AppKit(usize),
    Win32(usize),
}

unsafe impl Send for ParentData {}

/// Wrapper to make wry::WebView sendable across threads.
/// SAFETY: On Windows, we create the webview on the DAW's UI thread and only
/// access it from a background thread for evaluate_script calls, which WebView2
/// marshals to the UI thread internally.
struct SendWebView(wry::WebView);
unsafe impl Send for SendWebView {}

pub struct HardwaveAnalyserEditor {
    packet_slot: Arc<Mutex<Option<AudioPacket>>>,
    auth_token: Arc<Mutex<Option<String>>>,
    /// Current display scale factor (stored as f32 bits for atomic access).
    scale: Arc<AtomicU32>,
    /// Milliseconds between FFT deliveries — kept in sync with the refresh_rate param.
    refresh_interval_ms: Arc<AtomicU32>,
    /// Current editor size (updated by host or JS resize IPC).
    editor_size: Arc<Mutex<(u32, u32)>>,
    /// Channel to tell the webview thread to resize.
    resize_tx: Arc<Mutex<Option<crossbeam_channel::Sender<(u32, u32)>>>>,
    /// Process-unique identifier for this plug-in instance. Used to give
    /// every instance its own WebView2 user-data folder.
    instance_id: String,
    /// Plugin parameters, including the persisted preset_state field.
    /// Shared with the Plugin struct via Arc so nih-plug's DAW save/load
    /// lifecycle keeps preset_state in sync automatically.
    params: Arc<HardwaveAnalyserParams>,
}

/// Generate an identifier guaranteed unique within this process.
fn unique_instance_id() -> String {
    use std::sync::atomic::AtomicUsize;
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{}-{}-{}", pid, nanos, n)
}

impl HardwaveAnalyserEditor {
    pub fn new(
        packet_slot: Arc<Mutex<Option<AudioPacket>>>,
        refresh_interval_ms: Arc<AtomicU32>,
        params: Arc<HardwaveAnalyserParams>,
    ) -> Self {
        let token = auth::load_token();

        // Pre-warm WebView2 check and clean up legacy session dirs in the background
        // so neither blocks the DAW's UI thread when the user opens the plugin window.
        #[cfg(target_os = "windows")]
        {
            std::thread::spawn(|| {
                // Run the WebView2 reg check once so WEBVIEW2_ENSURED is already
                // set by the time spawn() is called — avoids ~5 s reg.exe delay.
                ensure_webview2();
            });
        }

        Self {
            packet_slot,
            auth_token: Arc::new(Mutex::new(token)),
            scale: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            refresh_interval_ms,
            editor_size: Arc::new(Mutex::new((EDITOR_WIDTH, EDITOR_HEIGHT))),
            resize_tx: Arc::new(Mutex::new(None)),
            instance_id: unique_instance_id(),
            params,
        }
    }

    fn build_url(&self, packet_port: Option<u16>) -> String {
        let token = self.auth_token.lock();
        let mut url = match token.as_deref() {
            Some(t) => format!("{}?token={}", ANALYSER_URL, t),
            None => ANALYSER_URL.to_string(),
        };
        if let Some(port) = packet_port {
            let sep = if url.contains('?') { '&' } else { '?' };
            url.push_str(&format!("{}packetPort={}", sep, port));
        }
        url
    }

    /// Returns JS that injects auth globals before the page loads.
    fn globals_init_script(&self) -> String {
        let token = self.auth_token.lock();
        let token_js = match token.as_deref() {
            Some(t) => {
                let escaped = t.replace('\\', "\\\\").replace('`', "\\`");
                format!("window.__hardwave_token = `{}`;", escaped)
            }
            None => "window.__hardwave_token = null;".to_string(),
        };
        let sub_valid = auth::load_sub_cache();
        let preset_state_str = self.params.preset_state.read().clone();
        let preset_state_str = match preset_state_str {
            Some(ref v) if !v.is_empty() => v.clone(),
            _ => match crate::auth::load_preset_state() {
                Some(v) => {
                    *self.params.preset_state.write() = Some(v.clone());
                    v
                }
                None => String::new(),
            },
        };
        let preset_js = if preset_state_str.is_empty() {
            "window.__HARDWAVE_PRESET_STATE = null;".to_string()
        } else {
            let escaped = preset_state_str.replace('\\', "\\\\").replace('\'', "\\'");
            format!("window.__HARDWAVE_PRESET_STATE = '{}';", escaped)
        };
        format!(
            r#"
            window.__HARDWAVE_SUB_VALID = {sub_valid};
            {token_js}
            {preset_js}
            "#,
            sub_valid = sub_valid,
            token_js = token_js,
            preset_js = preset_js,
        )
    }
}

// ---------------------------------------------------------------------------
// Local HTTP packet server (Windows only)
// ---------------------------------------------------------------------------

/// Spawn a tiny HTTP server on a random loopback port that serves the latest
/// FFT packet as JSON. JS fetches `http://127.0.0.1:{port}/` at ~60 fps.
///
/// The server runs until `running` is set to false (EditorHandle dropped).
#[cfg(target_os = "windows")]
fn start_packet_server(
    packet_slot: Arc<Mutex<Option<crate::protocol::AudioPacket>>>,
    running: Arc<AtomicBool>,
) -> u16 {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            debug_log(&format!("start_packet_server: bind failed: {}", e));
            return 0;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);

    thread::spawn(move || {
        // HTTP accept loop (non-blocking so we can check `running`).
        listener.set_nonblocking(true).ok();
        let mut conn_count: u64 = 0;
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    conn_count += 1;
                    if conn_count <= 3 || conn_count % 300 == 0 {
                        debug_log(&format!("packet server: conn #{} from JS", conn_count));
                    }
                    // Read the HTTP request to detect method (GET vs OPTIONS preflight).
                    stream.set_read_timeout(Some(Duration::from_millis(10))).ok();
                    let mut buf = [0u8; 1024];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let is_options = n >= 7 && &buf[..7] == b"OPTIONS";

                    let resp = if is_options {
                        // CORS preflight response — no body needed.
                        "HTTP/1.1 204 No Content\r\n\
                         Access-Control-Allow-Origin: *\r\n\
                         Access-Control-Allow-Methods: GET, OPTIONS\r\n\
                         Access-Control-Allow-Headers: *\r\n\
                         Access-Control-Max-Age: 86400\r\n\
                         Content-Length: 0\r\n\
                         Connection: close\r\n\
                         \r\n".to_string()
                    } else {
                        let body = {
                            // Take the latest packet from the shared slot.
                            match packet_slot.lock().take() {
                                Some(p) => serde_json::to_string(&p)
                                    .unwrap_or_else(|_| "null".to_string()),
                                None => "null".to_string(),
                            }
                        };
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Cache-Control: no-store\r\n\
                             Connection: close\r\n\
                             Content-Length: {}\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        )
                    };
                    stream.set_write_timeout(Some(Duration::from_millis(100))).ok();
                    let _ = stream.write_all(resp.as_bytes());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        debug_log("Packet server stopped");
    });

    port
}

// ---------------------------------------------------------------------------

impl Editor for HardwaveAnalyserEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        let packet_slot = Arc::clone(&self.packet_slot);
        let running = Arc::new(AtomicBool::new(true));
        let auth_token = Arc::clone(&self.auth_token);
        let globals_script = self.globals_init_script();

        // Resize channel: IPC/host → webview thread
        let (resize_tx_val, resize_rx) = crossbeam_channel::unbounded::<(u32, u32)>();
        *self.resize_tx.lock() = Some(resize_tx_val);
        let editor_size = Arc::clone(&self.editor_size);
        let resize_tx = Arc::clone(&self.resize_tx);

        // ---------------------------------------------------------------
        // Windows: create webview on the DAW's UI thread using build()
        // (NOT build_as_child) so that wry attaches the parent subclass
        // that handles WM_SIZE, WM_SETFOCUS, and WM_WINDOWPOSCHANGED
        // (NotifyParentWindowPositionChanged). Without this subclass,
        // WebView2's DirectComposition layer doesn't know its screen
        // position → ghosting artifacts.
        //
        // FFT data is delivered via a local TCP server (start_packet_server).
        // JS fetches http://127.0.0.1:{port}/ at ~60fps. Chrome permits
        // HTTPS pages fetching from 127.0.0.1 (localhost is "potentially
        // trustworthy" per the W3C spec), so no --disable-web-security needed.
        // ---------------------------------------------------------------
        #[cfg(target_os = "windows")]
        {
            let mut sw = Stopwatch::new("spawn() start");

            // Drain any backlogged packets from the previous session so the
            let parent_hwnd = match parent {
                ParentWindowHandle::Win32Hwnd(h) => h as usize,
                _ => 0,
            };
            debug_log(&format!("spawn() called, parent HWND = 0x{:X}", parent_hwnd));

            ensure_webview2();
            sw.mark("ensure_webview2()");

            // Per-instance data directory. The earlier slot_a/slot_b approach
            // failed because slots could overlap; the bare "analyser-webview2"
            // approach failed because two plug-in instances on different
            // tracks shared one UserDataFolder and raced inside the WebView2
            // browser process. Per-instance under analyser-webview2/<id>
            // ends both failure modes — every instance gets its own browser
            // child process keyed on a unique folder, and the folder is
            // stable across editor close/reopen on the SAME instance so we
            // keep cookies and cache.
            let data_dir = dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("hardwave")
                .join("analyser-webview2")
                .join(&self.instance_id);
            let _ = std::fs::create_dir_all(&data_dir);
            debug_log(&format!("Using WebView2 data dir: {:?}", data_dir));
            sw.mark("data dir ready");

            let mut web_context = wry::WebContext::new(Some(data_dir));
            sw.mark("WebContext::new()");

            let parent_wrapper = RwhWrapper(parent);
            let ipc_auth_token = Arc::clone(&auth_token);
            let ipc_params = Arc::clone(&self.params);

            // Start the local HTTP server that serves FFT packets as JSON.
            // JS polls http://127.0.0.1:{port}/ at ~60fps.
            let server_port = start_packet_server(Arc::clone(&packet_slot), Arc::clone(&running));
            sw.mark(&format!("packet server bound (port {})", server_port));

            let url = self.build_url(Some(server_port));

            let init_script = format!(
                r#"
                // Apply dark background immediately to prevent white flash while page loads.
                document.documentElement.style.cssText += ';background:#0a0a0b!important;';
                document.addEventListener('DOMContentLoaded', function() {{
                    document.documentElement.style.cssText += ';background:#0a0a0b!important;';
                    if (document.body) document.body.style.cssText += ';background:#0a0a0b!important;';
                }});

                window.__HARDWAVE_VST = true;
                {globals_script}
                window.__hardwave = {{
                    version: "{version}",
                    os: "{os}",
                    saveToken: function(token) {{
                        window.ipc.postMessage('saveToken:' + token);
                    }},
                    saveSubCache: function(signedToken) {{
                        window.ipc.postMessage('saveSubCache:' + (signedToken || ''));
                    }},
                    clearToken: function() {{
                        window.ipc.postMessage('clearToken');
                    }},
                    resize: function(w, h) {{
                        window.ipc.postMessage('resize:' + w + ',' + h);
                    }},
                    saveState: function(json) {{
                        window.__HARDWAVE_PRESET_STATE = json;
                        // saveState is no longer sent via IPC (broken in wry).
                        // Rust polls localStorage directly via evaluate_script instead.
                    }},
                    loadState: function() {{
                        return window.__HARDWAVE_PRESET_STATE;
                    }}
                }};
                window.__HARDWAVE_DEBUG_LOG = "";

                // Block right-click, save, print, view-source, devtools
                document.addEventListener('contextmenu', function(e) {{ e.preventDefault(); }});
                document.addEventListener('keydown', function(e) {{
                    if ((e.ctrlKey || e.metaKey) && 'spu'.indexOf(e.key.toLowerCase()) !== -1) e.preventDefault();
                    if (e.key === 'F12') e.preventDefault();
                }});
                "#,
                globals_script = globals_script,
                version = env!("CARGO_PKG_VERSION"),
                os = PLUGIN_OS,
            );

            sw.mark("init script built");

            #[allow(unused_imports)]
            use wry::WebViewBuilderExtWindows as _;

            let webview = wry::WebViewBuilder::with_web_context(&mut web_context)
                .with_additional_browser_args(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
                     --allow-insecure-localhost"
                )
                .with_devtools(false)
                .with_transparent(false)
                .with_background_color((10, 10, 11, 255))
                .with_visible(true)
                .with_focused(true)
                .with_url(&url)
                .with_navigation_handler(|url: String| {
                    url.starts_with("https://hardwavestudios.com/") ||
                    url.starts_with("https://analyser.hardwavestudios.com/") ||
                    url.starts_with("http://127.0.0.1:")
                })
                .with_ipc_handler({
                    let ipc_editor_size = Arc::clone(&editor_size);
                    let ipc_resize_tx = Arc::clone(&resize_tx);
                    let ipc_context = Arc::clone(&context);
                    move |req: wry::http::Request<String>| {
                        let msg = req.body().as_str();
                        if let Some(token) = msg.strip_prefix("saveToken:") {
                            let token = token.trim().to_string();
                            auth::save_token(&token);
                            *ipc_auth_token.lock() = Some(token);
                        } else if let Some(signed) = msg.strip_prefix("saveSubCache:") {
                            auth::save_sub_cache(signed.trim());
                        } else if msg == "clearToken" {
                            auth::clear_token();
                            *ipc_auth_token.lock() = None;
                        } else if let Some(json) = msg.strip_prefix("resize:") {
                            // JS sends "resize:{w},{h}"
                            let parts: Vec<&str> = json.split(',').collect();
                            if parts.len() == 2 {
                                if let (Ok(w), Ok(h)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                                    let w = w.clamp(MIN_WIDTH, MAX_WIDTH);
                                    let h = h.clamp(MIN_HEIGHT, MAX_HEIGHT);
                                    *ipc_editor_size.lock() = (w, h);
                                    if ipc_context.request_resize() {
                                        if let Some(tx) = ipc_resize_tx.lock().as_ref() {
                                            let _ = tx.send((w, h));
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
                .with_initialization_script(&init_script)
                .build(&parent_wrapper);

            match webview {
                Ok(wv) => {
                    sw.dump("WebViewBuilder::build() OK — webview visible");

                    let wv_arc = Arc::new(Mutex::new(SendWebView(wv)));
                    let poll_wv = Arc::clone(&wv_arc);
                    let poll_running = Arc::clone(&running);
                    let running_handle = Arc::clone(&running);
                    let poll_params = Arc::clone(&self.params);
                    let poll_last = Arc::new(Mutex::new(String::new()));
                    thread::spawn(move || {
                        thread::sleep(Duration::from_secs(3));
                        while poll_running.load(Ordering::Relaxed) {
                            if let Ok(wv) = poll_wv.lock() {
                                let last = Arc::clone(&poll_last);
                                let params = Arc::clone(&poll_params);
                                let _ = wv.0.evaluate_script_with_callback(
                                    r#"(function(){
                                        try {
                                            var c=localStorage.getItem('hw-analyser-config');
                                            var p=localStorage.getItem('hw-analyser-presets');
                                            var d=localStorage.getItem('hw-analyser-default-preset');
                                            if (!c && !p && !d) return null;
                                            return JSON.stringify({
                                                config: c ? JSON.parse(c) : {},
                                                presets: p ? JSON.parse(p) : [],
                                                defaultPresetId: d || null
                                            });
                                        } catch(e) { return null; }
                                    })()"#,
                                    move |result: String| {
                                        if result == "null" || result.is_empty() { return; }
                                        let inner: String = serde_json::from_str(&result).unwrap_or_default();
                                        if inner.is_empty() || inner == "null" { return; }
                                        let mut last = last.lock().unwrap();
                                        if inner == *last { return; }
                                        *last = inner.clone();
                                        *params.preset_state.write() = Some(inner.clone());
                                        crate::auth::save_preset_state(&inner);
                                    },
                                );
                            }
                            thread::sleep(Duration::from_secs(3));
                        }
                    });

                    Box::new(EditorHandle {
                        _thread: None,
                        _webview: Some(wv_arc),
                        _web_context: Some(SendWebContext(web_context)),
                        running: running_handle,
                    })
                }
                Err(e) => {
                    sw.dump(&format!("WebViewBuilder::build() FAILED: {}", e));
                    Box::new(EditorHandle {
                        _thread: None,
                        _webview: None,
                        _web_context: None,
                        running,
                    })
                }
            }
        }

        // ---------------------------------------------------------------
        // Linux / macOS: spawn thread with GTK/platform init
        // ---------------------------------------------------------------
        #[cfg(not(target_os = "windows"))]
        {
            let running_clone = Arc::clone(&running);
            let scale_clone = Arc::clone(&self.scale);
            let refresh_interval_ms_clone = Arc::clone(&self.refresh_interval_ms);
            let params_clone = Arc::clone(&self.params);
            let parent_data = match parent {
                ParentWindowHandle::X11Window(w) => ParentData::X11(w),
                ParentWindowHandle::AppKitNsView(v) => ParentData::AppKit(v as usize),
                ParentWindowHandle::Win32Hwnd(h) => ParentData::Win32(h as usize),
            };

            let url = self.build_url(None);
            let initial_scale = f32::from_bits(self.scale.load(Ordering::Relaxed));

            let handle = thread::spawn(move || {
                #[cfg(all(target_os = "linux", feature = "gtk"))]
                {
                    let _ = gtk::init();
                }

                let reconstructed = match parent_data {
                    ParentData::X11(w) => ParentWindowHandle::X11Window(w),
                    ParentData::AppKit(v) => {
                        ParentWindowHandle::AppKitNsView(v as *mut std::ffi::c_void)
                    }
                    ParentData::Win32(h) => {
                        ParentWindowHandle::Win32Hwnd(h as *mut std::ffi::c_void)
                    }
                };
                let parent_wrapper = RwhWrapper(reconstructed);

                let ipc_auth_token = Arc::clone(&auth_token);
                let ipc_params = Arc::clone(&params_clone);
                let ipc_editor_size = Arc::clone(&editor_size);
                let ipc_resize_tx = Arc::clone(&resize_tx);
                let ipc_context = Arc::clone(&context);
                let webview = wry::WebViewBuilder::new()
                    .with_bounds(wry::Rect {
                        position: wry::dpi::LogicalPosition::new(0, 0).into(),
                        size: wry::dpi::LogicalSize::new(
                            (EDITOR_WIDTH as f32 * initial_scale) as u32,
                            (EDITOR_HEIGHT as f32 * initial_scale) as u32,
                        ).into(),
                    })
                    .with_transparent(false)
                    .with_background_color((10, 10, 11, 255))
                    .with_visible(true)
                    .with_focused(true)
                    .with_url(&url)
                    .with_devtools(false)
                    .with_navigation_handler(|url: String| {
                        url.starts_with("https://hardwavestudios.com/") ||
                        url.starts_with("https://analyser.hardwavestudios.com/")
                    })
                    .with_ipc_handler(move |req: wry::http::Request<String>| {
                        let msg = req.body().as_str();
                        if let Some(token) = msg.strip_prefix("saveToken:") {
                            let token = token.trim().to_string();
                            auth::save_token(&token);
                            *ipc_auth_token.lock() = Some(token);
                        } else if let Some(signed) = msg.strip_prefix("saveSubCache:") {
                            auth::save_sub_cache(signed.trim());
                        } else if msg == "clearToken" {
                            auth::clear_token();
                            *ipc_auth_token.lock() = None;
                        } else if let Some(json) = msg.strip_prefix("resize:") {
                            let parts: Vec<&str> = json.split(',').collect();
                            if parts.len() == 2 {
                                if let (Ok(w), Ok(h)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                                    let w = w.clamp(MIN_WIDTH, MAX_WIDTH);
                                    let h = h.clamp(MIN_HEIGHT, MAX_HEIGHT);
                                    *ipc_editor_size.lock() = (w, h);
                                    if ipc_context.request_resize() {
                                        if let Some(tx) = ipc_resize_tx.lock().as_ref() {
                                            let _ = tx.send((w, h));
                                        }
                                    }
                                }
                            }
                        }
                    })
                    .with_initialization_script(&format!(
                        r#"
                        document.documentElement.style.cssText += ';background:#0a0a0b!important;';
                        document.addEventListener('DOMContentLoaded', function() {{
                            document.documentElement.style.cssText += ';background:#0a0a0b!important;';
                            if (document.body) document.body.style.cssText += ';background:#0a0a0b!important;';
                        }});

                        window.__HARDWAVE_VST = true;
                        {globals_script}
                        window.__hardwave = {{
                            version: "{version}",
                            os: "{os}",
                            saveToken: function(token) {{
                                window.ipc.postMessage('saveToken:' + token);
                            }},
                            saveSubCache: function(signedToken) {{
                                window.ipc.postMessage('saveSubCache:' + (signedToken || ''));
                            }},
                            clearToken: function() {{
                                window.ipc.postMessage('clearToken');
                            }},
                            resize: function(w, h) {{
                                window.ipc.postMessage('resize:' + w + ',' + h);
                            }},
                            saveState: function(json) {{
                                window.__HARDWAVE_PRESET_STATE = json;
                            }},
                            loadState: function() {{
                                return window.__HARDWAVE_PRESET_STATE;
                            }}
                        }};
                        window.__HARDWAVE_DEBUG_LOG = "";

                        // Block right-click, save, print, view-source, devtools
                        document.addEventListener('contextmenu', function(e) {{ e.preventDefault(); }});
                        document.addEventListener('keydown', function(e) {{
                            if ((e.ctrlKey || e.metaKey) && 'spu'.indexOf(e.key.toLowerCase()) !== -1) e.preventDefault();
                            if (e.key === 'F12') e.preventDefault();
                        }});
                        "#,
                        globals_script = globals_script,
                        version = env!("CARGO_PKG_VERSION"),
                        os = PLUGIN_OS,
                    ))
                    .build_as_child(&parent_wrapper);

                match webview {
                    Ok(webview) => {
                        let mut last_scale_bits = initial_scale.to_bits();
                        while running_clone.load(Ordering::Relaxed) {
                            // Apply pending resize from JS IPC or host.
                            while let Ok((w, h)) = resize_rx.try_recv() {
                                let _ = webview.set_bounds(wry::Rect {
                                    position: wry::dpi::LogicalPosition::new(0, 0).into(),
                                    size: wry::dpi::LogicalSize::new(w, h).into(),
                                });
                            }

                            // Resize webview if the DAW reported a new scale factor.
                            let current_scale_bits = scale_clone.load(Ordering::Relaxed);
                            if current_scale_bits != last_scale_bits {
                                let scale = f32::from_bits(current_scale_bits);
                                let (ew, eh) = *editor_size.lock();
                                let _ = webview.set_bounds(wry::Rect {
                                    position: wry::dpi::LogicalPosition::new(0, 0).into(),
                                    size: wry::dpi::LogicalSize::new(
                                        (ew as f32 * scale) as u32,
                                        (eh as f32 * scale) as u32,
                                    ).into(),
                                });
                                last_scale_bits = current_scale_bits;
                            }

                            if let Some(packet) = packet_slot.lock().take() {
                                let json = serde_json::to_string(&packet).unwrap_or_default();
                                let js = format!(
                                    "window.__onAudioPacket && window.__onAudioPacket({})",
                                    json
                                );
                                let _ = webview.evaluate_script(&js);
                            }

                            #[cfg(all(target_os = "linux", feature = "gtk"))]
                            {
                                while gtk::events_pending() {
                                    gtk::main_iteration_do(false);
                                }
                            }

                            let interval = refresh_interval_ms_clone.load(Ordering::Relaxed);
                            thread::sleep(Duration::from_millis(interval as u64));
                        }
                    }
                    Err(e) => {
                        nih_log!("Failed to create webview: {}", e);
                    }
                }
            });

            Box::new(EditorHandle {
                _thread: Some(handle),
                _webview: None,
                _web_context: None,
                running,
            })
        }
    }

    fn size(&self) -> (u32, u32) {
        let (w, h) = *self.editor_size.lock();
        let scale = f32::from_bits(self.scale.load(Ordering::Relaxed));
        ((w as f32 * scale) as u32, (h as f32 * scale) as u32)
    }

    fn set_scale_factor(&self, factor: f32) -> bool {
        self.scale.store(factor.to_bits(), Ordering::Relaxed);
        true
    }

    fn set_size(&self, width: u32, height: u32) {
        let w = width.clamp(MIN_WIDTH, MAX_WIDTH);
        let h = height.clamp(MIN_HEIGHT, MAX_HEIGHT);
        *self.editor_size.lock() = (w, h);
        if let Some(tx) = self.resize_tx.lock().as_ref() {
            let _ = tx.send((w, h));
        }
    }

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}
    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}
    fn param_values_changed(&self) {}
}

/// Wrapper to make wry::WebContext sendable across threads.
struct SendWebContext(wry::WebContext);
unsafe impl Send for SendWebContext {}

/// Handle returned from `spawn()`. When dropped, the editor closes.
struct EditorHandle {
    _thread: Option<thread::JoinHandle<()>>,
    _webview: Option<Arc<Mutex<SendWebView>>>,
    /// Must outlive the webview.
    _web_context: Option<SendWebContext>,
    running: Arc<AtomicBool>,
}

impl Drop for EditorHandle {
    fn drop(&mut self) {
        debug_log("EditorHandle dropped, closing editor");
        // Signal shutdown then explicitly join the worker thread.
        // JoinHandle::drop detaches — leaves the thread alive past Drop,
        // racing with WebView2 teardown and holding the user-data dir lock
        // for the next instance. Joining bounds Drop deterministically.
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self._thread.take() {
            let _ = h.join();
        }
    }
}
