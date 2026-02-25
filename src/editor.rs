//! Webview-based editor for Hardwave Bridge.
//!
//! Embeds a wry `WebView` that loads the Hardwave analyser page.
//! On Windows, FFT data is delivered via a local HTTP server (TcpListener
//! on a random port) that JS polls at ~60fps. This avoids both the STA
//! threading restriction on ICoreWebView2::ExecuteScript and the wry
//! custom-protocol interception issues in wry 0.46.

use crossbeam_channel::Receiver;
use nih_plug::prelude::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use wry::raw_window_handle as rwh06;

use crate::auth;
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

// ---------------------------------------------------------------------------
// WebView2 auto-install (Windows only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn ensure_webview2() {
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

    if installed { return; }

    let installed_user = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
            "/v", "pv",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if installed_user { return; }

    nih_log!("WebView2 Runtime not found — downloading bootstrapper...");

    let temp_dir = std::env::temp_dir();
    let bootstrapper_path = temp_dir.join("MicrosoftEdgeWebview2Setup.exe");

    let download = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Invoke-WebRequest -Uri 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile '{}'",
                bootstrapper_path.display()
            ),
        ])
        .output();

    match download {
        Ok(output) if output.status.success() => {
            nih_log!("Installing WebView2 Runtime silently...");
            let _ = Command::new(&bootstrapper_path)
                .args(["/silent", "/install"])
                .output();
            let _ = std::fs::remove_file(&bootstrapper_path);
        }
        _ => {
            nih_log!("Failed to download WebView2 bootstrapper");
        }
    }
}

/// Default editor size.
const EDITOR_WIDTH: u32 = 1100;
const EDITOR_HEIGHT: u32 = 700;

/// Base URL for the analyser page.
const ANALYSER_URL: &str = "https://hardwavestudios.com/vst/analyser";

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
                        .expect("X11 window handle must be non-zero"),
                );
                rwh06::RawWindowHandle::Xcb(handle)
            }
            ParentWindowHandle::AppKitNsView(ns_view) => {
                let handle = rwh06::AppKitWindowHandle::new(
                    std::ptr::NonNull::new(ns_view).expect("NSView must be non-null"),
                );
                rwh06::RawWindowHandle::AppKit(handle)
            }
            ParentWindowHandle::Win32Hwnd(hwnd) => {
                let handle = rwh06::Win32WindowHandle::new(
                    std::num::NonZeroIsize::new(hwnd as isize)
                        .expect("HWND must be non-zero"),
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

pub struct HardwaveBridgeEditor {
    packet_rx: Receiver<AudioPacket>,
    auth_token: Arc<Mutex<Option<String>>>,
    size: (u32, u32),
}

impl HardwaveBridgeEditor {
    pub fn new(packet_rx: Receiver<AudioPacket>) -> Self {
        let token = auth::load_token();
        Self {
            packet_rx,
            auth_token: Arc::new(Mutex::new(token)),
            size: (EDITOR_WIDTH, EDITOR_HEIGHT),
        }
    }

    fn build_url(&self) -> String {
        let token = self.auth_token.lock();
        match token.as_deref() {
            Some(t) => format!("{}?token={}", ANALYSER_URL, t),
            None => ANALYSER_URL.to_string(),
        }
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
    packet_rx: Receiver<crate::protocol::AudioPacket>,
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
        // Shared storage for the latest packet.
        let latest: Arc<Mutex<Option<crate::protocol::AudioPacket>>> =
            Arc::new(Mutex::new(None));

        // Drainer thread: keeps `latest` current from the crossbeam channel.
        {
            let latest_w = Arc::clone(&latest);
            let running_d = Arc::clone(&running);
            thread::spawn(move || {
                while running_d.load(Ordering::Relaxed) {
                    while let Ok(p) = packet_rx.try_recv() {
                        *latest_w.lock() = Some(p);
                    }
                    thread::sleep(Duration::from_millis(4));
                }
            });
        }

        // HTTP accept loop (non-blocking so we can check `running`).
        listener.set_nonblocking(true).ok();
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let body = {
                        let guard = latest.lock();
                        match guard.as_ref() {
                            Some(p) => serde_json::to_string(p)
                                .unwrap_or_else(|_| "null".to_string()),
                            None => "null".to_string(),
                        }
                    };
                    // Drain the incoming HTTP request bytes (ignore them).
                    stream.set_read_timeout(Some(Duration::from_millis(10))).ok();
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    // Write minimal HTTP response.
                    let resp = format!(
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
                    );
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

impl Editor for HardwaveBridgeEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        _context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        let packet_rx = self.packet_rx.clone();
        let running = Arc::new(AtomicBool::new(true));
        let auth_token = Arc::clone(&self.auth_token);
        let url = self.build_url();

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
            let parent_hwnd = match parent {
                ParentWindowHandle::Win32Hwnd(h) => h as usize,
                _ => 0,
            };
            debug_log(&format!("spawn() called, parent HWND = 0x{:X}", parent_hwnd));

            ensure_webview2();

            // Use a writable data directory for WebView2. The default is the
            // executable's folder (FL Studio's Program Files) which is not
            // writable → E_ACCESSDENIED.
            let data_dir = dirs::data_local_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("Hardwave")
                .join("WebView2");
            debug_log(&format!("WebView2 data dir = {:?}", data_dir));
            let _ = std::fs::create_dir_all(&data_dir);
            let mut web_context = wry::WebContext::new(Some(data_dir));

            let parent_wrapper = RwhWrapper(parent);
            let ipc_auth_token = Arc::clone(&auth_token);

            debug_log(&format!("URL = {}", url));

            // Start the local HTTP server that serves FFT packets as JSON.
            // JS polls http://127.0.0.1:{port}/ at ~60fps.
            let server_port = start_packet_server(packet_rx.clone(), Arc::clone(&running));
            debug_log(&format!("Packet server listening on port {}", server_port));

            let init_script = format!(
                r#"
                window.__HARDWAVE_VST = true;
                window.__hardwave = {{
                    saveToken: function(token) {{
                        window.ipc.postMessage('saveToken:' + token);
                    }}
                }};

                // Poll for FFT data from the local TCP packet server.
                // On Windows, evaluate_script from a Rust background thread
                // fails silently (ICoreWebView2 is STA-bound). Instead, JS
                // fetches http://127.0.0.1:{port}/ at ~60fps from the real
                // TCP server. Chrome permits HTTPS→http://127.0.0.1 because
                // loopback is considered potentially trustworthy.
                (function() {{
                    var _polling = false;
                    var _fetchOk = 0;
                    var _fetchNull = 0;
                    var _fetchErr = 0;
                    var _packetsSent = 0;

                    function dbg(msg) {{
                        try {{ window.ipc.postMessage('debug:' + msg); }} catch(e) {{}}
                    }}

                    function startPolling() {{
                        if (_polling) return;
                        _polling = true;
                        dbg('polling started on ' + window.location.href + ' port={port}');

                        (function poll() {{
                            fetch('http://127.0.0.1:{port}/')
                                .then(function(r) {{
                                    _fetchOk++;
                                    return r.json();
                                }})
                                .then(function(data) {{
                                    if (data !== null) {{
                                        if (typeof window.__onAudioPacket === 'function') {{
                                            window.__onAudioPacket(data);
                                            _packetsSent++;
                                            if (_packetsSent <= 3) {{
                                                dbg('packet delivered #' + _packetsSent +
                                                    ' peak=' + data.left_peak);
                                            }}
                                        }} else {{
                                            _fetchNull++;
                                        }}
                                    }} else {{
                                        _fetchNull++;
                                    }}
                                    // Report stats every ~5 seconds (300 polls @ 16ms)
                                    if ((_fetchOk + _fetchErr) % 300 === 0) {{
                                        dbg('poll stats: ok=' + _fetchOk +
                                            ' null=' + _fetchNull +
                                            ' err=' + _fetchErr +
                                            ' sent=' + _packetsSent);
                                    }}
                                }})
                                .catch(function(e) {{
                                    _fetchErr++;
                                    if (_fetchErr <= 3) {{
                                        dbg('fetch error #' + _fetchErr + ': ' + e);
                                    }}
                                }})
                                .finally(function() {{ setTimeout(poll, 16); }});
                        }})();
                    }}

                    if (document.readyState === 'loading') {{
                        document.addEventListener('DOMContentLoaded', startPolling);
                    }} else {{
                        startPolling();
                    }}
                }})();
                "#,
                port = server_port
            );

            #[allow(unused_imports)]
            use wry::WebViewBuilderExtWindows as _;

            let webview = wry::WebViewBuilder::with_web_context(&mut web_context)
                .with_additional_browser_args(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
                     --allow-insecure-localhost"
                )
                .with_devtools(true)
                .with_transparent(false)
                .with_background_color((10, 10, 11, 255))
                .with_visible(true)
                .with_focused(true)
                .with_url(&url)
                .with_ipc_handler(move |req: wry::http::Request<String>| {
                    let msg = req.body().as_str();
                    if let Some(token) = msg.strip_prefix("saveToken:") {
                        let token = token.trim().to_string();
                        auth::save_token(&token);
                        *ipc_auth_token.lock() = Some(token);
                    } else if let Some(info) = msg.strip_prefix("debug:") {
                        debug_log(&format!("[js] {}", info));
                    }
                })
                .with_initialization_script(&init_script)
                .build(&parent_wrapper);

            match webview {
                Ok(wv) => {
                    debug_log("WebView created successfully (TCP packet server active)!");
                    Box::new(EditorHandle {
                        _thread: None,
                        _webview: Some(Arc::new(Mutex::new(SendWebView(wv)))),
                        _web_context: Some(SendWebContext(web_context)),
                        running,
                    })
                }
                Err(e) => {
                    debug_log(&format!("FAILED to create webview: {}", e));
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
            let parent_data = match parent {
                ParentWindowHandle::X11Window(w) => ParentData::X11(w),
                ParentWindowHandle::AppKitNsView(v) => ParentData::AppKit(v as usize),
                ParentWindowHandle::Win32Hwnd(h) => ParentData::Win32(h as usize),
            };

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
                let webview = wry::WebViewBuilder::new()
                    .with_bounds(wry::Rect {
                        position: wry::dpi::LogicalPosition::new(0, 0).into(),
                        size: wry::dpi::LogicalSize::new(EDITOR_WIDTH, EDITOR_HEIGHT).into(),
                    })
                    .with_transparent(false)
                    .with_background_color((10, 10, 11, 255))
                    .with_visible(true)
                    .with_focused(true)
                    .with_url(&url)
                    .with_ipc_handler(move |req: wry::http::Request<String>| {
                        let msg = req.body().as_str();
                        if let Some(token) = msg.strip_prefix("saveToken:") {
                            let token = token.trim().to_string();
                            auth::save_token(&token);
                            *ipc_auth_token.lock() = Some(token);
                        }
                    })
                    .with_initialization_script(
                        r#"
                        window.__HARDWAVE_VST = true;
                        window.__hardwave = {
                            saveToken: function(token) {
                                window.ipc.postMessage('saveToken:' + token);
                            }
                        };
                        "#,
                    )
                    .build_as_child(&parent_wrapper);

                match webview {
                    Ok(webview) => {
                        while running_clone.load(Ordering::Relaxed) {
                            let mut latest: Option<AudioPacket> = None;
                            while let Ok(packet) = packet_rx.try_recv() {
                                latest = Some(packet);
                            }

                            if let Some(packet) = latest {
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

                            thread::sleep(Duration::from_millis(16));
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
        self.size
    }

    fn set_scale_factor(&self, _factor: f32) -> bool {
        true
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
        self.running.store(false, Ordering::Relaxed);
    }
}
