//! Every request from the operating system to stop, as one stream, for the
//! wiredata command-line applications (listener ADR-049; talker ADR-060's
//! "stop on every OS stop signal").
//!
//! - Ctrl-C everywhere, and SIGTERM on Unix — what `systemctl stop` sends.
//! - Ctrl-Break and console close on Windows.
//! - Logoff and shutdown on Windows, through a hidden window. Windows sends its
//!   console logoff and shutdown events only to programs that have not loaded
//!   `user32.dll` or `gdi32.dll`, and these applications always have: their GUI
//!   shares the binary, and the serial-port support loads `setupapi.dll`, which
//!   loads both. A window receives `WM_ENDSESSION` instead, and holds the session
//!   open until the application says its graceful stop has finished.
//!
//! What each request means for the application — its graceful stop, its time
//! limits, what it prints — stays in the application.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

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

/// How long the Windows session-end window holds the session open waiting for
/// [`StopRequests::finish`]. Windows may end the process sooner; this only
/// keeps a stop that never finishes from holding the window thread forever.
const SESSION_END_WAIT: Duration = Duration::from_secs(30);

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
    #[cfg_attr(not(windows), allow(dead_code))]
    fn wait(&self, limit: Duration) {
        let done = self.done.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self
            .changed
            .wait_timeout_while(done, limit, |done| !*done)
            .unwrap_or_else(|p| p.into_inner());
    }
}

/// The stream of stop requests. Dropping it, or calling
/// [`finish`](Self::finish), tells a waiting Windows session end that the
/// graceful stop is done.
pub struct StopRequests {
    requests: mpsc::UnboundedReceiver<StopRequest>,
    finished: Arc<Finished>,
    problems: Vec<String>,
    /// The session-end window; it lives on its own thread for the life of the
    /// process, so the handle is only needed to test it.
    #[cfg(windows)]
    #[cfg_attr(not(test), allow(dead_code))]
    window: session::Window,
}

impl StopRequests {
    /// The next request; `None` only if every listener has ended.
    pub async fn recv(&mut self) -> Option<StopRequest> {
        self.requests.recv().await
    }

    /// The requests that could not be listened for, in words. The others
    /// still work; the application decides how to say so.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// Say the graceful stop has finished, so a Windows logoff or shutdown
    /// waiting on it can go ahead. Call it as late as possible — after the
    /// last file is flushed — since Windows may end the process straight away.
    pub fn finish(&self) {
        self.finished.set();
    }
}

impl Drop for StopRequests {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Start listening for every stop request. Each listener is registered before
/// this returns, so a request that arrives straight afterwards is not missed.
/// Call it inside a Tokio runtime.
pub fn listen() -> StopRequests {
    let (tx, requests) = mpsc::unbounded_channel();
    let finished = Arc::new(Finished::default());
    let mut problems = Vec::new();

    let interrupt = tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = interrupt.send(StopRequest::Interrupt);
        }
    });

    macro_rules! forward {
        ($request:expr, $listener:expr) => {
            match $listener {
                Ok(mut listener) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if listener.recv().await.is_some() {
                            let _ = tx.send($request);
                        }
                    });
                }
                Err(error) => problems.push(format!("cannot listen for {} ({error})", $request)),
            }
        };
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        forward!(StopRequest::Terminate, signal(SignalKind::terminate()));
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        forward!(StopRequest::Break, windows::ctrl_break());
        forward!(StopRequest::ConsoleClose, windows::ctrl_close());
    }

    #[cfg(windows)]
    let window = match session::Window::open(tx, Arc::clone(&finished)) {
        Ok(window) => window,
        Err(error) => {
            problems.push(format!("cannot listen for logoff and shutdown ({error})"));
            session::Window::none()
        }
    };

    StopRequests {
        requests,
        finished,
        problems,
        #[cfg(windows)]
        window,
    }
}

/// The hidden window that receives Windows' session-end messages.
#[cfg(windows)]
mod session {
    use std::cell::RefCell;
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
        TranslateMessage, ENDSESSION_LOGOFF, MSG, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
        WS_OVERLAPPED,
    };

    use super::{Finished, StopRequest, SESSION_END_WAIT};

    /// What the window thread needs when the session ends.
    struct Session {
        requests: mpsc::UnboundedSender<StopRequest>,
        finished: Arc<Finished>,
    }

    thread_local! {
        /// The session of the window this thread runs. Messages, including
        /// those sent from other threads, are handled on the window's own
        /// thread, so a thread-local reaches the window procedure.
        static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    }

    /// The window, by handle as a number — `Send`, unlike `HWND`.
    pub(super) struct Window {
        #[cfg_attr(not(test), allow(dead_code))]
        pub(super) handle: isize,
    }

    impl Window {
        pub(super) fn none() -> Self {
            Self { handle: 0 }
        }

        /// Create the window on a thread of its own, which runs its message
        /// loop for the life of the process, and wait until it exists.
        pub(super) fn open(
            requests: mpsc::UnboundedSender<StopRequest>,
            finished: Arc<Finished>,
        ) -> std::io::Result<Self> {
            let (created_tx, created) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("session-end".into())
                .spawn(move || {
                    SESSION.with(|s| *s.borrow_mut() = Some(Session { requests, finished }));
                    // SAFETY: plain Win32 calls with valid, NUL-terminated
                    // strings that outlive them; the window procedure is a
                    // `'static` function.
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
            let handle = created
                .recv()
                .map_err(|_| std::io::Error::other("the window thread ended"))??;
            Ok(Self { handle })
        }
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
                let finished = SESSION.with(|session| {
                    let session = session.borrow();
                    let session = session.as_ref()?;
                    session.requests.send(request).ok()?;
                    Some(Arc::clone(&session.finished))
                });
                if let Some(finished) = finished {
                    finished.wait(SESSION_END_WAIT);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_is_a_stop_request() {
        // `systemctl stop` sends SIGTERM, which must reach the application as
        // a request, not end the process where it stands.
        let mut requests = listen();
        assert!(requests.problems().is_empty(), "{:?}", requests.problems());
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success());
        let request = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .expect("the request arrives");
        assert_eq!(request, Some(StopRequest::Terminate));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_windows_session_end_waits_for_the_graceful_stop() {
        // Windows ends the process once WM_ENDSESSION returns, so the window
        // must hold it until the application says its stop has finished.
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageW, ENDSESSION_LOGOFF, WM_ENDSESSION, WM_QUERYENDSESSION,
        };
        let mut requests = listen();
        assert!(requests.problems().is_empty(), "{:?}", requests.problems());
        let hwnd = requests.window.handle;
        // SAFETY: the window exists for the life of the process.
        let allowed = unsafe { SendMessageW(hwnd as _, WM_QUERYENDSESSION, 0, 0) };
        assert_eq!(allowed, 1, "the session may end");

        let session = std::thread::spawn(move || {
            // SAFETY: as above; SendMessageW blocks until the procedure returns.
            unsafe { SendMessageW(hwnd as _, WM_ENDSESSION, 1, ENDSESSION_LOGOFF as isize) }
        });
        let request = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .expect("the request arrives");
        assert_eq!(request, Some(StopRequest::Logoff));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!session.is_finished(), "held until the stop finishes");

        requests.finish();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !session.is_finished() {
            assert!(std::time::Instant::now() < deadline, "never released");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(session.join().unwrap(), 0);
    }
}
