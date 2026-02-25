//! Webview-based editor for Hardwave Bridge.
//!
//! Embeds a wry `WebView` that loads the Hardwave analyser page.
//! The Rust side pushes FFT data into the webview via `evaluate_script`.

use crossbeam_channel::Receiver;
use nih_plug::prelude::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use wry::raw_window_handle as rwh06;
#[cfg(target_os = "windows")]
use wry::WebViewBuilderExtWindows;

use crate::auth;
use crate::protocol::AudioPacket;

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
const EDITOR_WIDTH: u32 = 900;
const EDITOR_HEIGHT: u32 = 640;

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

impl Editor for HardwaveBridgeEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        _context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        let packet_rx = self.packet_rx.clone();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);
        let auth_token = Arc::clone(&self.auth_token);
        let url = self.build_url();

        // ---------------------------------------------------------------
        // Windows: create webview on the DAW's UI thread using build()
        // (NOT build_as_child) so that wry attaches the parent subclass
        // that handles WM_SIZE, WM_SETFOCUS, and WM_WINDOWPOSCHANGED
        // (NotifyParentWindowPositionChanged). Without this subclass,
        // WebView2's DirectComposition layer doesn't know its screen
        // position → ghosting artifacts.
        // ---------------------------------------------------------------
        #[cfg(target_os = "windows")]
        {
            ensure_webview2();

            let parent_wrapper = RwhWrapper(parent);
            let ipc_auth_token = Arc::clone(&auth_token);

            // build() creates the webview as a WS_CHILD of the parent HWND,
            // sizes it to fill the parent, AND subclasses the parent to
            // forward WM_SIZE/WM_WINDOWPOSCHANGED to the WebView2 controller.
            //
            // --disable-gpu forces software rendering, avoiding conflicts
            // between WebView2's DirectComposition and the DAW's own
            // rendering pipeline (FL Studio uses GDI/DirectX).
            let webview = wry::WebViewBuilder::new()
                .with_additional_browser_args(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-gpu"
                )
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
                .build(&parent_wrapper);

            match webview {
                Ok(wv) => {
                    let send_wv = Arc::new(Mutex::new(SendWebView(wv)));
                    let send_wv_clone = Arc::clone(&send_wv);

                    // Background thread only for injecting FFT packets.
                    // evaluate_script marshals to the UI thread internally.
                    let _injector = thread::spawn(move || {
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
                                let wv = send_wv_clone.lock();
                                let _ = wv.0.evaluate_script(&js);
                            }

                            thread::sleep(Duration::from_millis(16));
                        }
                    });

                    Box::new(EditorHandle {
                        _thread: None,
                        _webview: Some(send_wv),
                        running,
                    })
                }
                Err(e) => {
                    nih_log!("Failed to create webview: {}", e);
                    Box::new(EditorHandle {
                        _thread: None,
                        _webview: None,
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

/// Handle returned from `spawn()`. When dropped, the editor closes.
struct EditorHandle {
    _thread: Option<thread::JoinHandle<()>>,
    _webview: Option<Arc<Mutex<SendWebView>>>,
    running: Arc<AtomicBool>,
}

impl Drop for EditorHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}
