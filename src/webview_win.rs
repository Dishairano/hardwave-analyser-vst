//! The editor's WebView2 on Windows, built without holding up the host.
//!
//! wry builds a WebView2 in one call: it starts the browser process and waits for it, then waits
//! for the page's controller, then for every start-up script. The host's UI thread sits inside our
//! `Editor::spawn` the whole time, so the host stops answering. On the test machine (19 Oct 2026)
//! every WideBoi window took one to three seconds to build and the Hardwave DAW froze for 3.4 s
//! opening one; FL Studio, or any other host, waits the same way, because the wait is ours.
//!
//! Here each step is started and the host gets its thread back at once. WebView2 calls back
//! through the host's own message loop when a step is done, the way JUCE builds its WebView2, and
//! the window shows the editor's background colour until the page is there. The reachability
//! probe, a network request of up to three seconds, runs on a thread of its own for the same
//! reason.
//!
//! Everything else follows wry 0.46, which built this window until now: the same browser
//! arguments and settings, a container window filling the host's window, and the host's window
//! subclassed so the page follows its size, its focus and its position.

/// When the page may be opened: once the WebView exists and its start-up scripts are in, so that
/// they run on the page, and once the probe has said whether the interface can be reached.
#[derive(Default)]
struct Gate {
    /// Start-up scripts the WebView has not confirmed yet; `None` until the WebView exists.
    scripts_left: Option<usize>,
    reachable: Option<bool>,
    opened: bool,
    closed: bool,
}

// Only the Windows build drives the gate; the tests run it everywhere.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl Gate {
    fn webview_ready(&mut self, scripts: usize) {
        self.scripts_left = Some(scripts);
    }

    /// The WebView could not be set up and another is being built: its scripts no longer count.
    fn webview_lost(&mut self) {
        self.scripts_left = None;
    }

    fn script_added(&mut self) {
        if let Some(left) = self.scripts_left.as_mut() {
            *left = left.saturating_sub(1);
        }
    }

    fn probed(&mut self, reachable: bool) {
        self.reachable = Some(reachable);
    }

    fn close(&mut self) {
        self.closed = true;
    }

    /// What to open now, if anything: `Some(true)` the interface, `Some(false)` the offline page.
    /// Answers once.
    fn take(&mut self) -> Option<bool> {
        if self.closed || self.opened || self.scripts_left != Some(0) {
            return None;
        }
        let reachable = self.reachable?;
        self.opened = true;
        Some(reachable)
    }
}

/// Browsers of closed pages that may still be shutting down, by data folder.
///
/// WebView2 starts a new browser for a folder whose old browser is still exiting, and the new one
/// may then wait about 30 s for the old one: WettBoi opened 3 s after it closed showed its page
/// after 33 s in FL Studio (20 Oct 2026), against 3 s the first time. A page opened while the
/// closed page's browser is still running starts in the next folder instead, which has a browser
/// of its own; the next open after that finds the first folder free again.
struct Closing {
    browsers: Vec<(std::path::PathBuf, u32)>,
}

// Only the Windows build keeps browsers; the tests run it everywhere.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl Closing {
    const fn new() -> Self {
        Closing {
            browsers: Vec::new(),
        }
    }

    /// A page on `dir` closed, and no other page used its browser `pid`.
    fn closed(&mut self, dir: std::path::PathBuf, pid: u32) {
        self.browsers.retain(|(d, _)| *d != dir);
        self.browsers.push((dir, pid));
    }

    /// The folders in the order to try them: when a closed page's browser on the first is still
    /// running and no open page uses it, the first and the second change places. Also returns
    /// that browser. Browsers that are gone are forgotten.
    fn order(
        &mut self,
        mut dirs: Vec<std::path::PathBuf>,
        running: impl Fn(u32) -> bool,
        in_use: impl Fn(u32) -> bool,
    ) -> (Vec<std::path::PathBuf>, Option<u32>) {
        self.browsers.retain(|&(_, pid)| running(pid));
        let Some(first) = dirs.first() else {
            return (dirs, None);
        };
        let closing = self
            .browsers
            .iter()
            .find(|(d, pid)| d == first && !in_use(*pid))
            .map(|&(_, pid)| pid);
        if closing.is_some() && dirs.len() > 1 {
            dirs.swap(0, 1);
            (dirs, closing)
        } else {
            (dirs, None)
        }
    }

    /// The closed page's browser still running on `dir` that no open page uses, if any.
    fn running_on(
        &mut self,
        dir: &std::path::Path,
        running: impl Fn(u32) -> bool,
        in_use: impl Fn(u32) -> bool,
    ) -> Option<u32> {
        self.browsers.retain(|&(_, pid)| running(pid));
        self.browsers
            .iter()
            .find(|(d, pid)| d == dir && !in_use(*pid))
            .map(|&(_, pid)| pid)
    }
}

/// Whether a page that waits to start its browser starts now: once it has waited `at_least`,
/// and once the closed page's browser on its folder has gone or it has waited `cap` for it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn wait_over(
    waited: std::time::Duration,
    at_least: std::time::Duration,
    browser_running: bool,
    cap: std::time::Duration,
) -> bool {
    waited >= at_least && (!browser_running || waited >= cap)
}

/// Whether a folder WebView2 called in use is tried again: only when no other folder is left,
/// and a few times. LoudLab's one folder per instance, still held by the closed editor's browser,
/// failed this way in FL Studio (20 Oct 2026) and the window stayed black.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn try_again_when_in_use(in_use: bool, folders_left: usize, tries: u32, max: u32) -> bool {
    in_use && folders_left == 0 && tries < max
}

#[cfg(target_os = "windows")]
pub use windows_page::{open, Page, Spec};
// For the keeper; a plug-in without one has no use for it.
#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use windows_page::Watch;

#[cfg(target_os = "windows")]
mod windows_page {
    use super::{try_again_when_in_use, wait_over, Closing, Gate};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::rc::{Rc, Weak};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use webview2_com::{
        take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
        ContentLoadingEventHandler, CoreWebView2EnvironmentOptions,
        CreateCoreWebView2ControllerCompletedHandler,
        CreateCoreWebView2EnvironmentCompletedHandler, NavigationCompletedEventHandler,
        NavigationStartingEventHandler, WebMessageReceivedEventHandler,
    };
    use windows::core::{Interface, HRESULT, HSTRING, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, BOOL, COLORREF, E_POINTER, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT,
        WAIT_TIMEOUT, WPARAM,
    };
    use windows::Win32::Globalization::{
        GetUserDefaultUILanguage, LCIDToLocaleName, LOCALE_ALLOW_NEUTRAL_NAMES, MAX_LOCALE_NAME,
    };
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::LibraryLoader::{
        GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    use windows::Win32::System::WinRT::EventRegistrationToken;
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetAncestor, GetClientRect,
        GetForegroundWindow, IsWindow, KillTimer, LoadCursorW, PostMessageW, RegisterClassExW,
        RegisterWindowMessageW, SetTimer, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW,
        GA_ROOT, HMENU, HWND_TOP, IDC_ARROW, SIZE_MINIMIZED, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SW_SHOW, WINDOW_EX_STYLE,
        WM_ENTERSIZEMOVE, WM_NCDESTROY, WM_SETFOCUS, WM_SIZE, WM_TIMER, WM_WINDOWPOSCHANGED,
        WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_VISIBLE,
    };

    /// Called with each message the page sends with `window.ipc.postMessage`.
    type OnMessage = Box<dyn Fn(&str)>;
    /// Whether the page may go to an address.
    type AllowNavigation = Box<dyn Fn(&str) -> bool>;

    /// The plug-in's name in its log lines and in its window class and message names. The one
    /// line that differs between the plug-ins that share this file.
    const NAME: &str = "HardwaveAnalyser";

    /// What the page needs, all of it handed over when the window opens.
    pub struct Spec {
        /// The interface.
        pub url: String,
        /// Shown instead of the interface when the probe says it cannot be reached.
        pub offline_html: String,
        /// Runs before the page's own scripts, on every page.
        pub init_script: String,
        /// User data folders to try in turn; see `spawn_windows` for why there are several.
        pub data_dirs: Vec<PathBuf>,
        /// The window's colour until the page has drawn, and the page's own background.
        pub background: (u8, u8, u8),
        /// Whether the interface can be reached. Runs on a thread of its own.
        pub reachable: Box<dyn FnOnce() -> bool + Send>,
        pub on_message: OnMessage,
        /// Browser arguments in place of wry's ([`BROWSER_ARGUMENTS`]). Keep them the same for
        /// every window of a plug-in: WebView2 refuses other arguments on a folder in use.
        pub browser_args: Option<String>,
        /// Which addresses the page may go to; `None` lets it go anywhere.
        pub allow_navigation: Option<AllowNavigation>,
    }

    /// The page in the host's window. Dropping it closes the page, whatever stage it reached.
    pub struct Page(Rc<Shared>);

    /// A look at a page that does not keep it open, for the keeper.
    #[allow(dead_code)]
    pub struct Watch(Weak<Shared>);

    /// The same bridge wry put on every page: the interface calls `window.ipc.postMessage`.
    const IPC_SCRIPT: &str = r#"Object.defineProperty(window, 'ipc', { value: Object.freeze({ postMessage: s=> window.chrome.webview.postMessage(s) }) });"#;
    /// wry's browser arguments: no mini menu, no PDF or SmartScreen pop-ups, and sound may start
    /// without a click first.
    const BROWSER_ARGUMENTS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required";
    /// Our id among the subclasses on the host's window.
    const SUBCLASS_ID: usize = 0x4857_5750;

    /// The container's timer while the page waits to start its browser.
    const WAIT_TIMER: usize = 0x4857_5751;
    const WAIT_TICK_MS: u32 = 100;
    /// How long a page waits for a closed page's browser on its only folder.
    const CLOSING_WAIT: Duration = Duration::from_secs(10);
    /// A folder WebView2 calls in use is tried again this long after, this many times.
    const IN_USE_PAUSE: Duration = Duration::from_secs(1);
    const IN_USE_TRIES: u32 = 5;
    /// HRESULT_FROM_WIN32(ERROR_BUSY), "The requested resource is in use".
    const IN_USE: HRESULT = HRESULT(0x8007_00AA_u32 as i32);

    /// For the whole process: a browser serves every page on its folder, whichever thread.
    static CLOSING: Mutex<Closing> = Mutex::new(Closing::new());

    struct Shared {
        parent: HWND,
        container: Cell<HWND>,
        url: String,
        offline_html: String,
        scripts: [String; 2],
        background: (u8, u8, u8),
        on_message: RefCell<Option<OnMessage>>,
        browser_args: String,
        allow_navigation: RefCell<Option<AllowNavigation>>,
        dirs: RefCell<VecDeque<PathBuf>>,
        gate: RefCell<Gate>,
        hooked: Cell<bool>,
        loaded: Cell<bool>,
        /// The next navigation is the one this module started, which the navigation filter
        /// lets through: the offline page is a `data:` page the filter knows nothing about.
        own_navigation: Cell<bool>,
        opened_at: Instant,
        env: RefCell<Option<ICoreWebView2Environment>>,
        controller: RefCell<Option<ICoreWebView2Controller>>,
        webview: RefCell<Option<ICoreWebView2>>,
        /// The folder of the WebView in use.
        dir: RefCell<Option<PathBuf>>,
        /// The thread the page opened on, which WebView2 wants for every call.
        thread: std::thread::ThreadId,
        /// The folder the page waits to start its browser in, since when, and for at least how
        /// long.
        waiting: RefCell<Option<(PathBuf, Instant, Duration)>>,
        /// How often a folder was found in use and tried again.
        in_use_tries: Cell<u32>,
    }

    thread_local! {
        // The pages on this thread, found again by window from the window procedures. Every page
        // lives on the thread that owns its host window, so the `Rc`s never cross threads.
        static PAGES: RefCell<Vec<Weak<Shared>>> = const { RefCell::new(Vec::new()) };
    }

    /// Start the page in `parent`, the host's window, and return at once.
    pub fn open(parent: isize, spec: Spec) -> Page {
        // As wry did: WebView2 wants a single-threaded apartment, which the host's UI thread
        // normally is already.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let shared = Rc::new(Shared {
            parent: HWND(parent as _),
            container: Cell::new(HWND::default()),
            url: spec.url,
            offline_html: spec.offline_html,
            scripts: [IPC_SCRIPT.to_string(), spec.init_script],
            background: spec.background,
            on_message: RefCell::new(Some(spec.on_message)),
            browser_args: spec
                .browser_args
                .unwrap_or_else(|| BROWSER_ARGUMENTS.to_string()),
            allow_navigation: RefCell::new(spec.allow_navigation),
            dirs: RefCell::new(ordered_dirs(spec.data_dirs).into()),
            gate: RefCell::new(Gate::default()),
            hooked: Cell::new(false),
            loaded: Cell::new(false),
            own_navigation: Cell::new(false),
            opened_at: Instant::now(),
            env: RefCell::new(None),
            controller: RefCell::new(None),
            webview: RefCell::new(None),
            dir: RefCell::new(None),
            thread: std::thread::current().id(),
            waiting: RefCell::new(None),
            in_use_tries: Cell::new(0),
        });
        let _ = PAGES.try_with(|pages| {
            if let Ok(mut pages) = pages.try_borrow_mut() {
                pages.retain(|p| p.strong_count() > 0);
                pages.push(Rc::downgrade(&shared));
            }
        });
        if !shared.make_container() {
            elog!("[{NAME}] page: could not make the page's window; the editor will stay empty");
            return Page(shared);
        }
        shared.hook_parent();
        shared.start_probe(spec.reachable);
        shared.next_environment();
        Page(shared)
    }

    impl Page {
        #[allow(dead_code)]
        pub fn watch(&self) -> Watch {
            Watch(Rc::downgrade(&self.0))
        }
    }

    impl Drop for Page {
        fn drop(&mut self) {
            let s = &self.0;
            if let Ok(mut gate) = s.gate.try_borrow_mut() {
                gate.close();
            }
            if std::thread::current().id() != s.thread {
                elog!("[{NAME}] page: closed on another thread than it opened on");
            }
            s.unhook_parent();
            s.remember_browser();
            let controller = s
                .controller
                .try_borrow_mut()
                .ok()
                .and_then(|mut c| c.take());
            if let Some(controller) = controller {
                // SAFETY: a controller we hold, closed once.
                if let Err(e) = unsafe { controller.Close() } {
                    elog!("[{NAME}] page: closing the WebView failed: {e}");
                }
            }
            if let Ok(mut w) = s.webview.try_borrow_mut() {
                w.take();
            }
            if let Ok(mut e) = s.env.try_borrow_mut() {
                e.take();
            }
            // What the page's messages reached (the plug-in's context) goes with the window.
            if let Ok(mut m) = s.on_message.try_borrow_mut() {
                m.take();
            }
            if let Ok(mut a) = s.allow_navigation.try_borrow_mut() {
                a.take();
            }
            let container = s.container.replace(HWND::default());
            // SAFETY: our own window on this thread; a window the host already destroyed with its
            // own fails IsWindow.
            if !container.is_invalid() && unsafe { IsWindow(container) }.as_bool() {
                let _ = unsafe { DestroyWindow(container) };
            }
        }
    }

    #[allow(dead_code)]
    impl Watch {
        /// Open, and not yet closed. A page still being built is kept in memory by WebView2's
        /// pending answers for a moment after it closes.
        pub fn alive(&self) -> bool {
            self.0.upgrade().is_some_and(|s| !s.closed())
        }

        /// The page's size, or the window's while the page is still being built.
        pub fn size(&self) -> Option<(i32, i32)> {
            let s = self.0.upgrade()?;
            let controller = s.controller.borrow().clone();
            let mut rect = RECT::default();
            // SAFETY: valid RECTs to write to; a controller we hold, our own window.
            let ok = match controller {
                Some(c) => unsafe { c.Bounds(&mut rect) }.is_ok(),
                None => unsafe { GetClientRect(s.container.get(), &mut rect) }.is_ok(),
            };
            ok.then_some((rect.right - rect.left, rect.bottom - rect.top))
        }

        pub fn fit(&self, width: i32, height: i32) -> Result<(), String> {
            match self.0.upgrade() {
                Some(s) => s.fit(width, height),
                None => Ok(()),
            }
        }

        /// Hide the page and show it again, which WebView2 needs when its surface came up hidden.
        pub fn reveal(&self) -> Result<(), String> {
            let Some(s) = self.0.upgrade() else {
                return Ok(());
            };
            // SAFETY: our own window.
            let _ = unsafe { ShowWindow(s.container.get(), SW_SHOW) };
            let Some(controller) = s.controller.borrow().clone() else {
                return Ok(());
            };
            // SAFETY: a controller we hold.
            unsafe {
                controller.SetIsVisible(false).map_err(|e| e.to_string())?;
                controller.SetIsVisible(true).map_err(|e| e.to_string())
            }
        }
    }

    impl Shared {
        fn ms(&self) -> u128 {
            self.opened_at.elapsed().as_millis()
        }

        fn closed(&self) -> bool {
            self.gate.try_borrow().map(|g| g.closed).unwrap_or(true)
        }

        /// The host destroyed its window, and ours with it, before dropping the editor. A
        /// WebView2 that failed then failed for that, not for its data folder.
        fn window_gone(&self) -> bool {
            // SAFETY: IsWindow takes any value, including a handle that is gone.
            unsafe { !IsWindow(self.container.get()).as_bool() || !IsWindow(self.parent).as_bool() }
        }

        /// The window the page goes in, filling the host's window and painted in the background
        /// colour until the page draws.
        fn make_container(&self) -> bool {
            let Some(class) = container_class(self.background) else {
                return false;
            };
            let mut rect = RECT::default();
            // SAFETY: a valid RECT to write to; a bad parent only makes this fail.
            let _ = unsafe { GetClientRect(self.parent, &mut rect) };
            // SAFETY: a class registered with this module, the host's window as parent.
            let made = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    PCWSTR::null(),
                    WS_CHILD | WS_CLIPCHILDREN | WS_VISIBLE,
                    0,
                    0,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    self.parent,
                    HMENU::default(),
                    module_instance(),
                    None,
                )
            };
            let Ok(hwnd) = made else {
                return false;
            };
            // SAFETY: our own window, just made.
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    0,
                    0,
                    0,
                    0,
                    SWP_ASYNCWINDOWPOS
                        | SWP_NOACTIVATE
                        | SWP_NOMOVE
                        | SWP_NOOWNERZORDER
                        | SWP_NOSIZE,
                )
            };
            self.container.set(hwnd);
            true
        }

        fn hook_parent(&self) {
            // SAFETY: the host's window, on its own thread; the procedure outlives it (the module
            // is pinned) and finds the page through PAGES, so no data pointer is handed over.
            let hooked =
                unsafe { SetWindowSubclass(self.parent, Some(parent_proc), SUBCLASS_ID, 0) }
                    .as_bool();
            self.hooked.set(hooked);
        }

        fn unhook_parent(&self) {
            if !self.hooked.replace(false) {
                return;
            }
            // Another page in the same window (an editor opened again before the old one went)
            // still needs the subclass, which is one per window.
            let shared_window = PAGES
                .try_with(|pages| {
                    pages.try_borrow().is_ok_and(|pages| {
                        pages.iter().filter_map(Weak::upgrade).any(|p| {
                            !std::ptr::eq(Rc::as_ptr(&p), self)
                                && p.parent == self.parent
                                && p.hooked.get()
                        })
                    })
                })
                .unwrap_or(false);
            if !shared_window {
                // SAFETY: removing our own subclass, on the window's thread.
                let _ =
                    unsafe { RemoveWindowSubclass(self.parent, Some(parent_proc), SUBCLASS_ID) };
            }
        }

        fn start_probe(&self, reachable: Box<dyn FnOnce() -> bool + Send>) {
            let container = self.container.get().0 as isize;
            let message = probe_message();
            let spawned = std::thread::Builder::new()
                .name("wideboi-probe".into())
                .spawn(move || {
                    let reachable = reachable();
                    // SAFETY: posting to a window that may be gone only fails. The message is
                    // registered under our own name, so a window that took its handle since ignores it.
                    let _ = unsafe {
                        PostMessageW(
                            HWND(container as _),
                            message,
                            WPARAM(usize::from(reachable)),
                            LPARAM(0),
                        )
                    };
                });
            if spawned.is_err() {
                // Without the probe, load the interface and let the WebView try, as a probe that
                // is unsure does.
                self.gate.borrow_mut().probed(true);
            }
        }

        fn probed(&self, reachable: bool) {
            self.gate.borrow_mut().probed(reachable);
            self.open_when_ready();
        }

        /// Start the browser in the next user data folder.
        fn next_environment(self: &Rc<Self>) {
            if self.closed() {
                return;
            }
            if self.window_gone() {
                elog!("[{NAME}] page: the window is gone; no WebView2 is started for it");
                return;
            }
            let Some(dir) = self.dirs.borrow_mut().pop_front() else {
                elog!(
                    "[{NAME}] no WebView2 environment could be created; the window will be empty"
                );
                return;
            };
            elog!("[{NAME}] WebView2 data dir: {:?}", dir);
            // The only folder left, and a closed page's browser still runs there: wait for it to
            // go, rather than start a browser that waits on it or finds the folder in use.
            if self.dirs.borrow().is_empty() {
                if let Some(pid) = closing_browser(&dir) {
                    elog!(
                        "[{NAME}] page: waiting for the closed editor's browser (pid {pid}) to exit"
                    );
                    self.wait_then_start(dir, Duration::ZERO);
                    return;
                }
            }
            self.start_environment(dir);
        }

        fn wait_then_start(self: &Rc<Self>, dir: PathBuf, at_least: Duration) {
            *self.waiting.borrow_mut() = Some((dir, Instant::now(), at_least));
            // SAFETY: our own window; the timer goes with it.
            if unsafe { SetTimer(self.container.get(), WAIT_TIMER, WAIT_TICK_MS, None) } == 0 {
                let waiting = self.waiting.borrow_mut().take();
                if let Some((dir, _, _)) = waiting {
                    self.start_environment(dir);
                }
            }
        }

        /// The container's timer while the page waits to start its browser.
        fn wait_tick(self: &Rc<Self>) {
            let Some((dir, since, at_least)) = self.waiting.borrow().clone() else {
                return;
            };
            let waited = since.elapsed();
            let running = closing_browser(&dir).is_some();
            if !wait_over(waited, at_least, running, CLOSING_WAIT) {
                return;
            }
            // SAFETY: our own window and timer.
            let _ = unsafe { KillTimer(self.container.get(), WAIT_TIMER) };
            self.waiting.borrow_mut().take();
            if running {
                elog!(
                    "[{NAME}] page: the closed editor's browser still runs after {} ms; starting anyway",
                    waited.as_millis()
                );
            } else if at_least.is_zero() {
                elog!(
                    "[{NAME}] page: the closed editor's browser exited after {} ms",
                    waited.as_millis()
                );
            }
            self.start_environment(dir);
        }

        /// A folder that failed: tried again when WebView2 called it in use and no other folder
        /// is left, else the next folder.
        fn after_failure(self: &Rc<Self>, dir: &Path, e: &windows::core::Error) {
            let tries = self.in_use_tries.get();
            if try_again_when_in_use(
                e.code() == IN_USE,
                self.dirs.borrow().len(),
                tries,
                IN_USE_TRIES,
            ) {
                self.in_use_tries.set(tries + 1);
                elog!(
                    "[{NAME}] page: {:?} is in use; trying it again in {} ms ({} of {})",
                    dir,
                    IN_USE_PAUSE.as_millis(),
                    tries + 1,
                    IN_USE_TRIES
                );
                self.wait_then_start(dir.to_path_buf(), IN_USE_PAUSE);
                return;
            }
            self.next_environment();
        }

        fn start_environment(self: &Rc<Self>, dir: PathBuf) {
            if self.closed() || self.window_gone() {
                return;
            }
            let _ = std::fs::create_dir_all(&dir);
            // WebView2 may call back even when the call itself failed; only the first answer counts.
            let settled = Rc::new(Cell::new(false));
            let handler = {
                let this = Rc::clone(self);
                let settled = Rc::clone(&settled);
                let dir = dir.clone();
                CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
                    move |result, env| {
                        if !settled.replace(true) {
                            this.environment_ready(&dir, result.and_then(|()| present(env)));
                        }
                        Ok(())
                    },
                ))
            };
            let options = environment_options(&self.browser_args);
            // SAFETY: valid arguments; the handler is called on this thread's message loop.
            let started = unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    &HSTRING::from(dir.as_path()),
                    &options,
                    &handler,
                )
            };
            if let Err(e) = started {
                if !settled.replace(true) {
                    self.environment_ready(&dir, Err(e));
                }
            }
        }

        fn environment_ready(
            self: &Rc<Self>,
            dir: &Path,
            env: windows::core::Result<ICoreWebView2Environment>,
        ) {
            if self.closed() {
                return;
            }
            let env = match env {
                Ok(env) => env,
                Err(e) => {
                    elog!("[{NAME}] WebView creation FAILED in {:?}: {}", dir, e);
                    self.after_failure(dir, &e);
                    return;
                }
            };
            elog!("[{NAME}] page: browser started after {} ms", self.ms());
            *self.env.borrow_mut() = Some(env.clone());
            let settled = Rc::new(Cell::new(false));
            let handler = {
                let this = Rc::clone(self);
                let settled = Rc::clone(&settled);
                let dir = dir.to_path_buf();
                CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                    move |result, controller| {
                        if !settled.replace(true) {
                            this.controller_ready(&dir, result.and_then(|()| present(controller)));
                        }
                        Ok(())
                    },
                ))
            };
            // SAFETY: our own window as the page's parent; the handler is called on this thread.
            let started =
                unsafe { env.CreateCoreWebView2Controller(self.container.get(), &handler) };
            if let Err(e) = started {
                if !settled.replace(true) {
                    self.controller_ready(dir, Err(e));
                }
            }
        }

        fn controller_ready(
            self: &Rc<Self>,
            dir: &Path,
            controller: windows::core::Result<ICoreWebView2Controller>,
        ) {
            let controller = match controller {
                Ok(controller) => controller,
                Err(e) => {
                    if !self.closed() {
                        elog!("[{NAME}] WebView creation FAILED in {:?}: {}", dir, e);
                        self.env.borrow_mut().take();
                        self.after_failure(dir, &e);
                    }
                    return;
                }
            };
            if self.closed() || self.window_gone() {
                // The window closed while the page was being built.
                // SAFETY: a controller nobody else holds.
                let _ = unsafe { controller.Close() };
                return;
            }
            elog!(
                "[{NAME}] WebView created successfully in {:?} after {} ms",
                dir,
                self.ms()
            );
            *self.dir.borrow_mut() = Some(dir.to_path_buf());
            // SAFETY: a live controller, on its own thread.
            if let Err(e) = unsafe { self.set_up(&controller) } {
                // A browser process that died on the way, say. wry's build failed here and the
                // next folder was tried; a window left blank helps nobody.
                elog!(
                    "[{NAME}] WebView creation FAILED in {:?} while setting it up: {}",
                    dir,
                    e
                );
                // SAFETY: a controller that is ours and no longer used.
                let _ = unsafe { controller.Close() };
                self.controller.borrow_mut().take();
                self.webview.borrow_mut().take();
                self.env.borrow_mut().take();
                self.dir.borrow_mut().take();
                self.gate.borrow_mut().webview_lost();
                self.next_environment();
            }
        }

        /// wry's settings and handlers, then the start-up scripts; the page opens once they are in.
        unsafe fn set_up(
            self: &Rc<Self>,
            controller: &ICoreWebView2Controller,
        ) -> windows::core::Result<()> {
            let webview = controller.CoreWebView2()?;
            if let Ok(controller2) = controller.cast::<ICoreWebView2Controller2>() {
                let (r, g, b) = self.background;
                let _ = controller2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                    R: r,
                    G: g,
                    B: b,
                    A: 255,
                });
            }
            // A file dragged onto the window was taken by the page as a download: in FL, a
            // plug-in dragged from the browser onto the open window put Edge's Downloads panel
            // over the interface. The interface takes no files.
            if let Ok(controller4) = controller.cast::<ICoreWebView2Controller4>() {
                let _ = controller4.SetAllowExternalDrop(false);
            }
            let settings = webview.Settings()?;
            settings.SetIsStatusBarEnabled(false)?;
            settings.SetAreDefaultContextMenusEnabled(true)?;
            settings.SetIsZoomControlEnabled(false)?;
            settings.SetAreDevToolsEnabled(false)?;
            // No Ctrl+P, Ctrl+R, F5, F12 and the like: the page is the plug-in, not a browser.
            if let Ok(settings3) = settings.cast::<ICoreWebView2Settings3>() {
                settings3.SetAreBrowserAcceleratorKeysEnabled(false)?;
            }
            if let Ok(settings5) = settings.cast::<ICoreWebView2Settings5>() {
                settings5.SetIsPinchZoomEnabled(false)?;
            }
            if let Ok(settings6) = settings.cast::<ICoreWebView2Settings6>() {
                settings6.SetIsSwipeNavigationEnabled(false)?;
            }
            if let Ok(settings9) = settings.cast::<ICoreWebView2Settings9>() {
                settings9.SetIsNonClientRegionSupportEnabled(true)?;
            }

            let mut token = EventRegistrationToken::default();
            let weak = Rc::downgrade(self);
            webview.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                    let (Some(args), Some(page)) = (args, weak.upgrade()) else {
                        return Ok(());
                    };
                    let mut body = PWSTR::null();
                    args.TryGetWebMessageAsString(&mut body)?;
                    let body = take_pwstr(body);
                    if let Ok(on_message) = page.on_message.try_borrow() {
                        if let Some(on_message) = on_message.as_ref() {
                            on_message(&body);
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )?;
            if self.allow_navigation.borrow().is_some() {
                let weak = Rc::downgrade(self);
                webview.add_NavigationStarting(
                    &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                        let (Some(args), Some(page)) = (args, weak.upgrade()) else {
                            return Ok(());
                        };
                        if page.own_navigation.replace(false) {
                            return Ok(());
                        }
                        let mut uri = PWSTR::null();
                        args.Uri(&mut uri)?;
                        let uri = take_pwstr(uri);
                        let allowed = page
                            .allow_navigation
                            .try_borrow()
                            .ok()
                            .and_then(|allow| allow.as_ref().map(|allow| allow(&uri)))
                            .unwrap_or(true);
                        if !allowed {
                            args.SetCancel(true)?;
                        }
                        Ok(())
                    })),
                    &mut token,
                )?;
            }
            // Every page load in the log, start and finish, so the log says whether the page the
            // window was given ever loaded; the first one also says how long the window took.
            webview.add_ContentLoading(
                &ContentLoadingEventHandler::create(Box::new(move |webview, _| {
                    if let Some(webview) = webview {
                        elog!("[{NAME}] page load started: {}", page_url(&webview));
                    }
                    Ok(())
                })),
                &mut token,
            )?;
            let weak = Rc::downgrade(self);
            webview.add_NavigationCompleted(
                &NavigationCompletedEventHandler::create(Box::new(move |webview, args| {
                    let (Some(webview), Some(args), Some(page)) = (webview, args, weak.upgrade())
                    else {
                        return Ok(());
                    };
                    let mut ok = BOOL::default();
                    let _ = args.IsSuccess(&mut ok);
                    elog!(
                        "[{NAME}] page load {}: {}",
                        if ok.as_bool() { "finished" } else { "failed" },
                        page_url(&webview)
                    );
                    if !page.loaded.replace(true) {
                        elog!("[{NAME}] page: first page after {} ms", page.ms());
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            *self.webview.borrow_mut() = Some(webview.clone());
            *self.controller.borrow_mut() = Some(controller.clone());
            self.fit_to_parent();
            controller.SetIsVisible(true)?;
            // wry moved the keyboard into the page as it opened. The page now arrives seconds
            // later, so only while the host's window is still the one in front.
            if GetForegroundWindow() == GetAncestor(self.parent, GA_ROOT) {
                let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
            }

            self.gate.borrow_mut().webview_ready(self.scripts.len());
            for script in &self.scripts {
                let weak = Rc::downgrade(self);
                let added = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(
                    move |result, _id| {
                        if let Some(page) = weak.upgrade() {
                            if let Err(e) = result {
                                elog!("[{NAME}] page: a start-up script was refused: {}", e);
                            }
                            page.gate.borrow_mut().script_added();
                            page.open_when_ready();
                        }
                        Ok(())
                    },
                ));
                if let Err(e) = webview
                    .AddScriptToExecuteOnDocumentCreated(&HSTRING::from(script.as_str()), &added)
                {
                    elog!("[{NAME}] page: a start-up script was refused: {}", e);
                    self.gate.borrow_mut().script_added();
                }
            }
            self.open_when_ready();
            Ok(())
        }

        fn open_when_ready(&self) {
            let Some(reachable) = self.gate.borrow_mut().take() else {
                return;
            };
            let Some(webview) = self.webview.borrow().clone() else {
                return;
            };
            self.own_navigation.set(true);
            // SAFETY: a live WebView, on its own thread.
            let opened = unsafe {
                if reachable {
                    webview.Navigate(&HSTRING::from(self.url.as_str()))
                } else {
                    webview.NavigateToString(&HSTRING::from(self.offline_html.as_str()))
                }
            };
            match opened {
                Ok(()) => elog!(
                    "[{NAME}] page: opening {} after {} ms",
                    if reachable {
                        "the interface"
                    } else {
                        "the offline page"
                    },
                    self.ms()
                ),
                Err(e) => elog!("[{NAME}] page: could not open the page: {}", e),
            }
        }

        fn fit_to_parent(&self) {
            let mut rect = RECT::default();
            // SAFETY: a valid RECT to write to.
            if unsafe { GetClientRect(self.parent, &mut rect) }.is_ok() {
                let _ = self.fit(rect.right - rect.left, rect.bottom - rect.top);
            }
        }

        fn fit(&self, width: i32, height: i32) -> Result<(), String> {
            if width <= 0 || height <= 0 {
                return Ok(());
            }
            // SAFETY: our own window.
            let _ = unsafe {
                SetWindowPos(
                    self.container.get(),
                    HWND::default(),
                    0,
                    0,
                    width,
                    height,
                    SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOMOVE,
                )
            };
            let Some(controller) = self.controller.borrow().clone() else {
                return Ok(());
            };
            // SAFETY: a controller we hold.
            unsafe {
                controller.SetBounds(RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                })
            }
            .map_err(|e| e.to_string())
        }

        fn focus(&self) {
            if let Some(controller) = self.controller.borrow().clone() {
                // SAFETY: a controller we hold.
                let _ =
                    unsafe { controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) };
            }
        }

        fn moved(&self) {
            if let Some(controller) = self.controller.borrow().clone() {
                // SAFETY: a controller we hold.
                let _ = unsafe { controller.NotifyParentWindowPositionChanged() };
            }
        }
    }

    /// The page's address as it may go into the log: a `data:` page carries the whole page and
    /// a query may carry anything, so neither is written down.
    fn page_url(webview: &ICoreWebView2) -> String {
        let mut uri = PWSTR::null();
        // SAFETY: a live WebView; the string it hands back is ours to free, which take_pwstr does.
        if unsafe { webview.Source(&mut uri) }.is_err() {
            return String::new();
        }
        let uri = take_pwstr(uri);
        if uri
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case("data:"))
        {
            return "data:".to_string();
        }
        uri.split(['?', '#']).next().unwrap_or_default().to_string()
    }

    fn present<T>(value: Option<T>) -> windows::core::Result<T> {
        value.ok_or_else(|| windows::core::Error::from(E_POINTER))
    }

    /// The newest page whose host window, or whose own window, is `hwnd`.
    impl Shared {
        /// A closing page that was the last on its browser leaves the browser to `CLOSING`, so
        /// a page opened before it has exited starts in another folder.
        fn remember_browser(self: &Rc<Self>) {
            let Some(dir) = self.dir.try_borrow().ok().and_then(|d| d.clone()) else {
                return;
            };
            let Some(pid) = self
                .webview
                .try_borrow()
                .ok()
                .and_then(|w| w.as_ref().and_then(browser_of))
            else {
                return;
            };
            if browser_in_use(pid, Some(self)) {
                return;
            }
            elog!("[{NAME}] page: closed; its browser (pid {pid}) is left to exit");
            if let Ok(mut closing) = CLOSING.lock() {
                closing.closed(dir, pid);
            }
        }
    }

    /// The folders to try, the first moved back when a closed page's browser still runs there.
    fn ordered_dirs(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
        let Ok(mut closing) = CLOSING.lock() else {
            return dirs;
        };
        let (dirs, closing_browser) =
            closing.order(dirs, process_running, |pid| browser_in_use(pid, None));
        if let Some(pid) = closing_browser {
            elog!(
                "[{NAME}] page: the closed editor's browser (pid {pid}) is still running; starting in the next folder"
            );
        }
        dirs
    }

    /// The closed page's browser to wait for before starting one on `dir`, if any.
    fn closing_browser(dir: &Path) -> Option<u32> {
        let mut closing = CLOSING.lock().ok()?;
        closing.running_on(dir, process_running, |pid| browser_in_use(pid, None))
    }

    fn browser_of(webview: &ICoreWebView2) -> Option<u32> {
        let mut pid = 0u32;
        // SAFETY: a live WebView on its own thread; the answer goes into `pid`.
        unsafe { webview.BrowserProcessId(&mut pid) }.ok()?;
        (pid != 0).then_some(pid)
    }

    /// Whether a page on this thread other than `except` has a WebView on browser `pid`.
    fn browser_in_use(pid: u32, except: Option<&Rc<Shared>>) -> bool {
        PAGES
            .try_with(|pages| {
                let Ok(pages) = pages.try_borrow() else {
                    return false;
                };
                pages.iter().filter_map(Weak::upgrade).any(|p| {
                    !except.is_some_and(|e| Rc::ptr_eq(e, &p))
                        && p.webview
                            .try_borrow()
                            .ok()
                            .and_then(|w| w.as_ref().and_then(browser_of))
                            == Some(pid)
                })
            })
            .unwrap_or(false)
    }

    fn process_running(pid: u32) -> bool {
        // SAFETY: a plain query.
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(handle) => {
                // SAFETY: the handle just opened.
                let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
                // SAFETY: closed once.
                let _ = unsafe { CloseHandle(handle) };
                running
            }
            Err(_) => false,
        }
    }

    fn page_for(hwnd: HWND, own_window: bool) -> Option<Rc<Shared>> {
        PAGES
            .try_with(|pages| {
                let pages = pages.try_borrow().ok()?;
                pages.iter().rev().filter_map(Weak::upgrade).find(|p| {
                    if own_window {
                        p.container.get() == hwnd
                    } else {
                        p.parent == hwnd
                    }
                })
            })
            .ok()
            .flatten()
    }

    /// The host's window, as wry handled it: the page follows its size, takes the keyboard when it
    /// does, and is told when it moves so drop-downs open in the right place.
    unsafe extern "system" fn parent_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        id: usize,
        _data: usize,
    ) -> LRESULT {
        match msg {
            WM_SIZE if wparam.0 != SIZE_MINIMIZED as usize => {
                if let Some(page) = page_for(hwnd, false) {
                    page.fit_to_parent();
                }
            }
            WM_SETFOCUS | WM_ENTERSIZEMOVE => {
                if let Some(page) = page_for(hwnd, false) {
                    page.focus();
                }
            }
            WM_WINDOWPOSCHANGED => {
                if let Some(page) = page_for(hwnd, false) {
                    page.moved();
                }
            }
            WM_NCDESTROY => {
                // The host's window is going: a subclass has to leave before it does.
                let _ = RemoveWindowSubclass(hwnd, Some(parent_proc), id);
                let _ = PAGES.try_with(|pages| {
                    if let Ok(pages) = pages.try_borrow() {
                        for page in pages.iter().filter_map(Weak::upgrade) {
                            if page.parent == hwnd {
                                page.hooked.set(false);
                            }
                        }
                    }
                });
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    /// The page's own window: painted in the background colour, and where the probe's answer
    /// arrives.
    unsafe extern "system" fn container_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == probe_message() {
            if let Some(page) = page_for(hwnd, true) {
                page.probed(wparam.0 != 0);
            }
            return LRESULT(0);
        }
        if msg == WM_TIMER && wparam.0 == WAIT_TIMER {
            if let Some(page) = page_for(hwnd, true) {
                page.wait_tick();
            }
            return LRESULT(0);
        }
        if msg == WM_NCDESTROY {
            // Gone with the host's window. The handle can be given to a new window, which the
            // page must then not destroy when it closes.
            if let Some(page) = page_for(hwnd, true) {
                page.container.set(HWND::default());
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn probe_message() -> u32 {
        static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        // SAFETY: a constant name.
        *MESSAGE.get_or_init(|| unsafe {
            RegisterWindowMessageW(&HSTRING::from(format!("{NAME}.InterfaceProbed")))
        })
    }

    /// This module, not the host's executable. wry registered its window class under the host's
    /// executable, so a second copy of a plug-in found the first copy's class; under our own
    /// module each copy has its own.
    fn module_instance() -> HINSTANCE {
        let mut module = HMODULE::default();
        // SAFETY: any address inside this module names it; UNCHANGED_REFCOUNT takes no reference.
        let _ = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(module_instance as *const () as *const u16),
                &mut module,
            )
        };
        HINSTANCE(module.0)
    }

    /// The page window's class, registered once. Its brush is the first page's background colour,
    /// which for this plug-in is the only one.
    fn container_class(background: (u8, u8, u8)) -> Option<PCWSTR> {
        static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static NAME_W: std::sync::OnceLock<HSTRING> = std::sync::OnceLock::new();
        let name = PCWSTR(
            NAME_W
                .get_or_init(|| HSTRING::from(format!("{NAME}Page")))
                .as_ptr(),
        );
        let registered = *REGISTERED.get_or_init(|| {
            let (r, g, b) = background;
            // SAFETY: plain values; the brush lives for the life of the process, as the class does.
            let class = unsafe {
                WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(container_proc),
                    hInstance: module_instance(),
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    hbrBackground: CreateSolidBrush(COLORREF(
                        u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16),
                    )),
                    lpszClassName: name,
                    ..Default::default()
                }
            };
            // SAFETY: a filled-in class; the procedure lives in this module, which is pinned.
            unsafe { RegisterClassExW(&class) != 0 }
        });
        registered.then_some(name)
    }

    /// wry's environment options, so the browser starts the way it always did for this plug-in.
    fn environment_options(browser_args: &str) -> ICoreWebView2EnvironmentOptions {
        let options = CoreWebView2EnvironmentOptions::default();
        // SAFETY: plain values into a fresh options object.
        unsafe {
            options.set_additional_browser_arguments(browser_args.to_string());
            options.set_are_browser_extensions_enabled(false);
            options.set_language(user_language());
            options.set_scroll_bar_style(COREWEBVIEW2_SCROLLBAR_STYLE_DEFAULT);
        }
        ICoreWebView2EnvironmentOptions::from(options)
    }

    fn user_language() -> String {
        let mut name = [0u16; MAX_LOCALE_NAME as usize];
        // SAFETY: a buffer of the documented maximum size.
        let len = unsafe {
            LCIDToLocaleName(
                u32::from(GetUserDefaultUILanguage()),
                Some(&mut name),
                LOCALE_ALLOW_NEUTRAL_NAMES,
            )
        };
        let len = usize::try_from(len)
            .unwrap_or(0)
            .saturating_sub(1)
            .min(name.len());
        String::from_utf16_lossy(&name[..len])
    }
}

#[cfg(test)]
mod wait_tests {
    use super::{try_again_when_in_use, wait_over};
    use std::time::Duration;

    const CAP: Duration = Duration::from_secs(10);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_page_waits_while_the_closed_browser_runs() {
        assert!(!wait_over(ms(300), ms(0), true, CAP));
        assert!(wait_over(ms(300), ms(0), false, CAP));
    }

    #[test]
    fn a_page_stops_waiting_at_the_cap() {
        assert!(wait_over(ms(10_000), ms(0), true, CAP));
    }

    #[test]
    fn a_folder_in_use_is_left_alone_for_the_pause_first() {
        assert!(!wait_over(ms(500), ms(1_000), false, CAP));
        assert!(wait_over(ms(1_000), ms(1_000), false, CAP));
    }

    #[test]
    fn only_the_last_folder_is_tried_again_when_in_use() {
        assert!(try_again_when_in_use(true, 0, 0, 5));
        assert!(!try_again_when_in_use(true, 1, 0, 5));
        assert!(!try_again_when_in_use(false, 0, 0, 5));
    }

    #[test]
    fn a_folder_in_use_is_tried_five_times() {
        assert!(try_again_when_in_use(true, 0, 4, 5));
        assert!(!try_again_when_in_use(true, 0, 5, 5));
    }
}

#[cfg(test)]
mod closing_tests {
    use super::Closing;
    use std::path::PathBuf;

    fn dirs() -> Vec<PathBuf> {
        vec![
            PathBuf::from("a"),
            PathBuf::from("a-2"),
            PathBuf::from("a-3"),
        ]
    }

    fn names(dirs: &[PathBuf]) -> Vec<&str> {
        dirs.iter().map(|d| d.to_str().unwrap()).collect()
    }

    #[test]
    fn nothing_closing_keeps_the_order() {
        let mut closing = Closing::new();
        let (order, pid) = closing.order(dirs(), |_| true, |_| false);
        assert_eq!(names(&order), ["a", "a-2", "a-3"]);
        assert_eq!(pid, None);
    }

    #[test]
    fn a_running_browser_nobody_uses_moves_its_folder_back() {
        let mut closing = Closing::new();
        closing.closed(PathBuf::from("a"), 7);
        let (order, pid) = closing.order(dirs(), |_| true, |_| false);
        assert_eq!(names(&order), ["a-2", "a", "a-3"]);
        assert_eq!(pid, Some(7));
    }

    #[test]
    fn a_browser_that_is_gone_is_forgotten() {
        let mut closing = Closing::new();
        closing.closed(PathBuf::from("a"), 7);
        let (order, _) = closing.order(dirs(), |_| false, |_| false);
        assert_eq!(names(&order), ["a", "a-2", "a-3"]);
        let (order, _) = closing.order(dirs(), |_| true, |_| false);
        assert_eq!(names(&order), ["a", "a-2", "a-3"]);
    }

    #[test]
    fn a_browser_an_open_page_still_uses_keeps_the_order() {
        let mut closing = Closing::new();
        closing.closed(PathBuf::from("a"), 7);
        let (order, pid) = closing.order(dirs(), |_| true, |pid| pid == 7);
        assert_eq!(names(&order), ["a", "a-2", "a-3"]);
        assert_eq!(pid, None);
    }

    #[test]
    fn a_closing_browser_on_another_folder_keeps_the_order() {
        let mut closing = Closing::new();
        closing.closed(PathBuf::from("a-2"), 7);
        let (order, _) = closing.order(dirs(), |_| true, |_| false);
        assert_eq!(names(&order), ["a", "a-2", "a-3"]);
    }

    #[test]
    fn a_closed_browser_still_running_on_a_folder_is_found() {
        let mut closing = Closing::new();
        closing.closed(PathBuf::from("a"), 7);
        assert_eq!(
            closing.running_on(&PathBuf::from("a"), |_| true, |_| false),
            Some(7)
        );
        assert_eq!(
            closing.running_on(&PathBuf::from("a"), |_| true, |pid| pid == 7),
            None
        );
        assert_eq!(
            closing.running_on(&PathBuf::from("b"), |_| true, |_| false),
            None
        );
        assert_eq!(
            closing.running_on(&PathBuf::from("a"), |_| false, |_| false),
            None
        );
    }

    #[test]
    fn a_single_folder_stays_put() {
        let mut closing = Closing::new();
        closing.closed(PathBuf::from("a"), 7);
        let (order, pid) = closing.order(vec![PathBuf::from("a")], |_| true, |_| false);
        assert_eq!(names(&order), ["a"]);
        assert_eq!(pid, None);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::Gate;

    fn webview_with_scripts(n: usize) -> Gate {
        let mut gate = Gate::default();
        gate.webview_ready(n);
        gate
    }

    #[test]
    fn nothing_opens_before_the_webview_exists() {
        let mut gate = Gate::default();
        gate.probed(true);
        assert_eq!(gate.take(), None);
    }

    #[test]
    fn the_page_waits_for_every_start_up_script() {
        let mut gate = webview_with_scripts(2);
        gate.probed(true);
        gate.script_added();
        assert_eq!(gate.take(), None);
        gate.script_added();
        assert_eq!(gate.take(), Some(true));
    }

    #[test]
    fn the_page_waits_for_the_probe() {
        let mut gate = webview_with_scripts(0);
        assert_eq!(gate.take(), None);
        gate.probed(true);
        assert_eq!(gate.take(), Some(true));
    }

    #[test]
    fn an_unreachable_interface_opens_the_offline_page() {
        let mut gate = webview_with_scripts(0);
        gate.probed(false);
        assert_eq!(gate.take(), Some(false));
    }

    #[test]
    fn the_page_opens_once() {
        let mut gate = webview_with_scripts(1);
        gate.probed(true);
        gate.script_added();
        assert_eq!(gate.take(), Some(true));
        gate.script_added();
        gate.probed(true);
        assert_eq!(gate.take(), None);
    }

    #[test]
    fn a_webview_given_up_before_it_opened_waits_for_the_next() {
        let mut gate = webview_with_scripts(0);
        gate.webview_lost();
        gate.probed(true);
        assert_eq!(gate.take(), None);
        gate.webview_ready(0);
        assert_eq!(gate.take(), Some(true));
    }

    #[test]
    fn nothing_opens_once_the_window_is_closed() {
        let mut gate = webview_with_scripts(0);
        gate.close();
        gate.probed(true);
        assert_eq!(gate.take(), None);
    }
}
