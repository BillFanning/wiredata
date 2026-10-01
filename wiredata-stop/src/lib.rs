//! Requests from the operating system to stop, for the wiredata command-line
//! applications (listener ADR-049; talker ADR-060's "stop on every OS stop
//! signal").
//!
//! Windows needs two things the usual signal handling does not give:
//!
//! - **Logoff and shutdown** arrive at a hidden window. Windows sends its
//!   console logoff and shutdown events only to programs that have not loaded
//!   `user32.dll` or `gdi32.dll`, and these applications always have: their GUI
//!   shares the binary, and the serial-port support loads `setupapi.dll`, which
//!   loads both. A window receives `WM_ENDSESSION` instead.
//! - **A held stop.** Windows ends the process as soon as the handler for a
//!   console close, logoff or shutdown returns, so the handler holds it until
//!   the application says its graceful stop has finished ([`StopHold`]).
//!
//! Two ways in:
//!
//! - With the `tokio` feature, [`listen`] gives one async stream of every
//!   request, Ctrl-C and SIGTERM included (listener).
//! - Without it, [`on_windows_stop`] calls a function for Ctrl-C, Ctrl-Break,
//!   console close, logoff and shutdown on Windows, and needs no async runtime
//!   (talker, ADR-002). On Unix it installs nothing; the application keeps its
//!   own signal handling there.
//!
//! What each request means for the application — its graceful stop, its time
//! limits, what it prints — stays in the application.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
#[cfg(windows)]
use std::time::Duration;

#[cfg(feature = "tokio")]
mod stream;
#[cfg(feature = "tokio")]
pub use stream::{listen, StopRequests};

/// A request from the operating system to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopRequest {
    /// Ctrl-C, or SIGINT on Unix.
    Interrupt,
    /// Ctrl-Break on Windows.
    Break,
    /// SIGTERM on Unix.
    Terminate,
    /// The console window was closed, on Windows.
    ConsoleClose,
    /// The user is signing out, on Windows.
    Logoff,
    /// The system is shutting down or restarting, on Windows.
    Shutdown,
}

impl fmt::Display for StopRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Interrupt => "Ctrl-C",
            Self::Break => "Ctrl-Break",
            Self::Terminate => "SIGTERM",
            Self::ConsoleClose => "console close",
            Self::Logoff => "logoff",
            Self::Shutdown => "system shutdown",
        })
    }
}

/// How long a held Windows stop waits for [`StopHold::finish`]. Windows may end
/// the process sooner; this only keeps a stop that never finishes from holding
/// a thread forever.
#[cfg(windows)]
const HOLD_LIMIT: Duration = Duration::from_secs(30);

/// A function told about each stop request, on whatever thread received it.
#[cfg(windows)]
type OnRequest = Arc<dyn Fn(StopRequest) + Send + Sync>;

/// Set once the application's graceful stop has finished.
#[derive(Default)]
struct Finished {
    done: Mutex<bool>,
    changed: Condvar,
}

impl Finished {
    fn set(&self) {
        *self.done.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.changed.notify_all();
    }

    /// Wait until set, or `limit` passes.
    #[cfg(windows)]
    fn wait(&self, limit: Duration) {
        let done = self.done.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self
            .changed
            .wait_timeout_while(done, limit, |done| !*done)
            .unwrap_or_else(|p| p.into_inner());
    }
}

/// Holds a Windows console close, logoff or shutdown until the graceful stop
/// has finished. Call [`finish`](Self::finish) as late as possible — after the
/// last file is flushed — since Windows may end the process straight away.
/// Dropping it finishes too, and stops the requests it registered.
pub struct StopHold {
    finished: Arc<Finished>,
    #[cfg(windows)]
    console: Option<u64>,
    /// The session-end window, by handle; it lives on its own thread for the
    /// life of the process, so the handle is only needed to test it.
    #[cfg(windows)]
    #[cfg_attr(not(test), allow(dead_code))]
    window: isize,
}

impl StopHold {
    /// Say the graceful stop has finished, so a held Windows stop can go ahead.
    pub fn finish(&self) {
        self.finished.set();
    }
}

impl Drop for StopHold {
    fn drop(&mut self) {
        self.finish();
        #[cfg(windows)]
        if let Some(id) = self.console.take() {
            console::unregister(id);
        }
    }
}

/// Call `on_request` when Windows ends the session — logoff or shutdown — and
/// hold the session until the returned [`StopHold`] finishes. On other
/// platforms it does nothing.
pub fn on_session_end(
    on_request: impl Fn(StopRequest) + Send + Sync + 'static,
) -> std::io::Result<StopHold> {
    let finished = Arc::new(Finished::default());
    #[cfg(windows)]
    let window = session::open(Arc::new(on_request), Arc::clone(&finished))?;
    #[cfg(not(windows))]
    let _ = on_request;
    Ok(StopHold {
        finished,
        #[cfg(windows)]
        console: None,
        #[cfg(windows)]
        window,
    })
}

/// Call `on_request` for every Windows stop request — Ctrl-C, Ctrl-Break,
/// console close, logoff and shutdown — holding the last three until the
/// returned [`StopHold`] finishes. Needs no async runtime. On other platforms
/// it installs nothing: keep the platform's own signal handling there.
pub fn on_windows_stop(
    on_request: impl Fn(StopRequest) + Send + Sync + 'static,
) -> std::io::Result<StopHold> {
    #[cfg(windows)]
    {
        let on_request: OnRequest = Arc::new(on_request);
        let finished = Arc::new(Finished::default());
        let window = session::open(Arc::clone(&on_request), Arc::clone(&finished))?;
        let console = console::register(on_request, Arc::clone(&finished))?;
        Ok(StopHold {
            finished,
            console: Some(console),
            window,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = on_request;
        Ok(StopHold {
            finished: Arc::new(Finished::default()),
        })
    }
}

/// The Windows console control handler: Ctrl-C and Ctrl-Break are passed on;
/// a console close, logoff or shutdown is held until the stop finishes.
#[cfg(windows)]
mod console {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    };

    use super::{Finished, OnRequest, StopRequest, HOLD_LIMIT};

    struct Registration {
        id: u64,
        on_request: OnRequest,
        finished: Arc<Finished>,
    }

    static REGISTRY: Mutex<Vec<Registration>> = Mutex::new(Vec::new());
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    /// Whether the handler is installed: one per process, shared by every
    /// registration.
    static INSTALLED: OnceLock<Result<(), i32>> = OnceLock::new();

    pub(super) fn register(on_request: OnRequest, finished: Arc<Finished>) -> std::io::Result<u64> {
        let installed = INSTALLED.get_or_init(|| {
            // SAFETY: `handler` is a `'static` function with the signature
            // Windows expects.
            if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
                Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
            } else {
                Ok(())
            }
        });
        if let Err(code) = installed {
            return Err(std::io::Error::from_raw_os_error(*code));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        REGISTRY
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Registration {
                id,
                on_request,
                finished,
            });
        Ok(id)
    }

    pub(super) fn unregister(id: u64) {
        REGISTRY
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|r| r.id != id);
    }

    /// Windows calls this on a thread of its own for each console event.
    pub(super) unsafe extern "system" fn handler(event: u32) -> BOOL {
        let request = match event {
            CTRL_C_EVENT => StopRequest::Interrupt,
            CTRL_BREAK_EVENT => StopRequest::Break,
            CTRL_CLOSE_EVENT => StopRequest::ConsoleClose,
            CTRL_LOGOFF_EVENT => StopRequest::Logoff,
            CTRL_SHUTDOWN_EVENT => StopRequest::Shutdown,
            _ => return 0,
        };
        let registrations: Vec<(OnRequest, Arc<Finished>)> = REGISTRY
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|r| (Arc::clone(&r.on_request), Arc::clone(&r.finished)))
            .collect();
        if registrations.is_empty() {
            // Nobody is listening: let the default handler end the process.
            return 0;
        }
        for (on_request, _) in &registrations {
            on_request(request);
        }
        if matches!(
            request,
            StopRequest::ConsoleClose | StopRequest::Logoff | StopRequest::Shutdown
        ) {
            // Windows ends the process once this returns.
            for (_, finished) in &registrations {
                finished.wait(HOLD_LIMIT);
            }
        }
        1
    }
}

/// The hidden window that receives Windows' session-end messages.
#[cfg(windows)]
mod session {
    use std::cell::RefCell;
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
        TranslateMessage, ENDSESSION_LOGOFF, MSG, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
        WS_OVERLAPPED,
    };

    use super::{Finished, OnRequest, StopRequest, HOLD_LIMIT};

    thread_local! {
        /// What the window on this thread tells, and waits on. Messages,
        /// including those sent from other threads, are handled on the
        /// window's own thread, so a thread-local reaches the procedure.
        static SESSION: RefCell<Option<(OnRequest, Arc<Finished>)>> = const { RefCell::new(None) };
    }

    /// Create the window on a thread of its own, which runs its message loop
    /// for the life of the process, and wait until it exists. Returns its
    /// handle as a number.
    pub(super) fn open(on_request: OnRequest, finished: Arc<Finished>) -> std::io::Result<isize> {
        let (created_tx, created) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("session-end".into())
            .spawn(move || {
                SESSION.with(|s| *s.borrow_mut() = Some((on_request, finished)));
                // SAFETY: plain Win32 calls with valid, NUL-terminated strings
                // that outlive them; the window procedure is a `'static`
                // function.
                let handle = unsafe { create_window() };
                let created = handle.is_ok();
                let _ = created_tx.send(handle);
                if !created {
                    return;
                }
                // SAFETY: a zeroed MSG is valid; the loop runs on the thread
                // that created the window.
                unsafe {
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })?;
        created
            .recv()
            .map_err(|_| std::io::Error::other("the window thread ended"))?
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// Register the class (once per process; a second registration fails
    /// harmlessly) and create the hidden top-level window. Top-level, not
    /// message-only: message-only windows do not receive session-end
    /// broadcasts.
    unsafe fn create_window() -> std::io::Result<isize> {
        let class = wide("wiredata-session-end");
        let instance = GetModuleHandleW(std::ptr::null());
        let mut wndclass: WNDCLASSW = std::mem::zeroed();
        wndclass.lpfnWndProc = Some(window_procedure);
        wndclass.hInstance = instance;
        wndclass.lpszClassName = class.as_ptr();
        RegisterClassW(&wndclass);
        let title = wide("wiredata session end");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(hwnd as isize)
        }
    }

    /// Let the session end, then hold it at `WM_ENDSESSION` until the graceful
    /// stop finishes: once this returns, Windows may end the process.
    unsafe extern "system" fn window_procedure(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION if wparam != 0 => {
                let request = if (lparam as u32) & ENDSESSION_LOGOFF != 0 {
                    StopRequest::Logoff
                } else {
                    StopRequest::Shutdown
                };
                let session = SESSION.with(|s| s.borrow().clone());
                if let Some((on_request, finished)) = session {
                    on_request(request);
                    finished.wait(HOLD_LIMIT);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_name_themselves_in_words() {
        assert_eq!(StopRequest::Terminate.to_string(), "SIGTERM");
        assert_eq!(StopRequest::Shutdown.to_string(), "system shutdown");
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_session_end_waits_for_the_graceful_stop() {
        // Windows ends the process once WM_ENDSESSION returns, so the window
        // must hold it until the application says its stop has finished.
        use std::time::{Duration, Instant};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageW, ENDSESSION_LOGOFF, WM_ENDSESSION, WM_QUERYENDSESSION,
        };
        let (tx, requests) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let hold = on_session_end(move |request| {
            let _ = tx.lock().unwrap().send(request);
        })
        .unwrap();
        let hwnd = hold.window;
        // SAFETY: the window exists for the life of the process.
        let allowed = unsafe { SendMessageW(hwnd as _, WM_QUERYENDSESSION, 0, 0) };
        assert_eq!(allowed, 1, "the session may end");

        let session = std::thread::spawn(move || {
            // SAFETY: as above; SendMessageW blocks until the procedure returns.
            unsafe { SendMessageW(hwnd as _, WM_ENDSESSION, 1, ENDSESSION_LOGOFF as isize) }
        });
        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(request, StopRequest::Logoff);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!session.is_finished(), "held until the stop finishes");

        hold.finish();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_finished() {
            assert!(Instant::now() < deadline, "never released");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(session.join().unwrap(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_console_events_pass_on_ctrl_c_and_hold_a_close() {
        // Windows ends the process once the console handler returns from a
        // close, so the close is held; Ctrl-C returns straight away.
        use std::time::{Duration, Instant};
        use windows_sys::Win32::System::Console::{CTRL_CLOSE_EVENT, CTRL_C_EVENT};
        let (tx, requests) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let hold = on_windows_stop(move |request| {
            let _ = tx.lock().unwrap().send(request);
        })
        .unwrap();

        // SAFETY: the handler is what Windows calls; calling it directly
        // stands in for a console event.
        assert_eq!(unsafe { console::handler(CTRL_C_EVENT) }, 1);
        assert_eq!(requests.try_recv(), Ok(StopRequest::Interrupt));

        // SAFETY: as above.
        let close = std::thread::spawn(|| unsafe { console::handler(CTRL_CLOSE_EVENT) });
        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(request, StopRequest::ConsoleClose);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!close.is_finished(), "held until the stop finishes");

        drop(hold);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !close.is_finished() {
            assert!(Instant::now() < deadline, "never released");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(close.join().unwrap(), 1);
        // SAFETY: as above.
        assert_eq!(
            unsafe { console::handler(CTRL_C_EVENT) },
            0,
            "with nobody registered, the default handler acts"
        );
    }
}
