//! Webview-based editor for Hardwave Bridge.
//!
//! Embeds a wry `WebView` that loads `https://hardwave.studio/vst/analyser`.
//! The Rust side pushes FFT data into the webview via `evaluate_script`.
//!
//! Key design constraints:
//! - wry's `WebView` is **not** `Send` on Linux (GTK). All webview access
//!   must happen on the thread that created it.
//! - nih-plug uses `raw-window-handle` 0.5 while wry re-exports 0.6. We
//!   bridge this with a thin wrapper.

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

/// Default editor size.
const EDITOR_WIDTH: u32 = 900;
const EDITOR_HEIGHT: u32 = 640;

/// Base URL for the analyser page.
const ANALYSER_URL: &str = "https://hardwave.studio/vst/analyser";

// ---------------------------------------------------------------------------
// raw-window-handle 0.5 (nih-plug) → 0.6 (wry) bridge
// ---------------------------------------------------------------------------

/// Wrapper around nih-plug's `ParentWindowHandle` that implements the rwh 0.6
/// traits (`HasWindowHandle` + `HasDisplayHandle`) so wry can consume it.
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

/// Send-safe representation of the parent window handle. We extract the raw
/// values from nih-plug's `ParentWindowHandle` (which contains `*mut c_void`)
/// and reconstruct them on the editor thread.
#[derive(Clone, Copy)]
enum ParentData {
    X11(u32),
    AppKit(usize),
    Win32(usize),
}

// SAFETY: The underlying window handles are valid pointers owned by the DAW
// host and guaranteed to outlive the editor by the plugin API contract.
unsafe impl Send for ParentData {}

/// Editor that embeds a wry webview loading the Hardwave website analyser.
pub struct HardwaveBridgeEditor {
    /// Receives audio packets from the plugin's process thread.
    packet_rx: Receiver<AudioPacket>,

    /// Cached auth token loaded from disk at plugin init.
    auth_token: Arc<Mutex<Option<String>>>,

    /// Current editor size.
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
}

impl Editor for HardwaveBridgeEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        _context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        let packet_rx = self.packet_rx.clone();
        let auth_token = Arc::clone(&self.auth_token);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        // Build the URL with token query param if available.
        let url = {
            let token = auth_token.lock();
            match token.as_deref() {
                Some(t) => format!("{}?token={}", ANALYSER_URL, t),
                None => ANALYSER_URL.to_string(),
            }
        };

        // Extract raw handle values before spawning — ParentWindowHandle
        // contains *mut c_void which isn't Send, but the underlying values
        // (window ID / pointer) are safe to send across threads as long as
        // the parent outlives the editor (guaranteed by nih-plug).
        let parent_data = match parent {
            ParentWindowHandle::X11Window(w) => ParentData::X11(w),
            ParentWindowHandle::AppKitNsView(v) => ParentData::AppKit(v as usize),
            ParentWindowHandle::Win32Hwnd(h) => ParentData::Win32(h as usize),
        };

        // Spawn a dedicated thread that creates the webview and runs the
        // packet injection loop. The webview must only be accessed from
        // the thread that created it (GTK requirement on Linux).
        let handle = thread::spawn(move || {
            #[cfg(target_os = "linux")]
            {
                let _ = gtk::init();
            }

            let reconstructed = match parent_data {
                ParentData::X11(w) => ParentWindowHandle::X11Window(w),
                ParentData::AppKit(v) => ParentWindowHandle::AppKitNsView(v as *mut std::ffi::c_void),
                ParentData::Win32(h) => ParentWindowHandle::Win32Hwnd(h as *mut std::ffi::c_void),
            };
            let parent_wrapper = RwhWrapper(reconstructed);

            let ipc_auth_token = Arc::clone(&auth_token);
            let webview = wry::WebViewBuilder::new()
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
                        // Drain all available packets, keep only the latest.
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

                        #[cfg(target_os = "linux")]
                        {
                            while gtk::events_pending() {
                                gtk::main_iteration_do(false);
                            }
                        }

                        thread::sleep(Duration::from_millis(16)); // ~60 Hz
                    }
                }
                Err(e) => {
                    nih_log!("Failed to create webview: {}", e);
                }
            }
        });

        Box::new(EditorHandle {
            _thread: Some(handle),
            running,
        })
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

/// Handle returned from `spawn()`. When dropped, the editor thread stops.
struct EditorHandle {
    _thread: Option<thread::JoinHandle<()>>,
    running: Arc<AtomicBool>,
}

impl Drop for EditorHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}
