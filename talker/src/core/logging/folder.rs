use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use anyhow::Context as _;

type Notify = Arc<dyn Fn() + Send + Sync>;
type OpenDirectory = Arc<dyn Fn(&Path) -> anyhow::Result<()> + Send + Sync>;

/// Current state of the GUI's non-blocking **Open folder** action.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum LogFolderState {
    #[default]
    Idle,
    Opening,
    Failed(String),
}

/// Cloneable control that creates and opens the GUI log directory away from
/// both the UI and file-log worker threads.
#[derive(Clone)]
pub(crate) struct LogFolderOpener {
    state: Arc<Mutex<LogFolderState>>,
    notify: Arc<Mutex<Option<Notify>>>,
    open_directory: OpenDirectory,
}

impl Default for LogFolderOpener {
    fn default() -> Self {
        Self::new(Arc::new(create_and_open_directory))
    }
}

impl LogFolderOpener {
    fn new(open_directory: OpenDirectory) -> Self {
        Self {
            state: Arc::new(Mutex::new(LogFolderState::Idle)),
            notify: Arc::new(Mutex::new(None)),
            open_directory,
        }
    }

    pub(crate) fn state(&self) -> LogFolderState {
        lock(&self.state).clone()
    }

    pub(crate) fn set_notify(&self, notify: Notify) {
        *lock(&self.notify) = Some(notify);
    }

    /// Start one background open request.
    ///
    /// Returns `false` when a request is already running. A prior failure may
    /// be retried; accepting that retry replaces the visible failure with
    /// [`LogFolderState::Opening`]. Failures are published through [`Self::state`]
    /// so callers have one diagnostic source.
    pub(crate) fn open(&self, path: PathBuf) -> bool {
        {
            let mut state = lock(&self.state);
            if matches!(*state, LogFolderState::Opening) {
                return false;
            }
            *state = LogFolderState::Opening;
        }
        // Publish Opening before the worker can finish. The GUI also stores
        // this accepted transition locally because both callbacks can be
        // coalesced before its next frame.
        notify(&self.notify);

        let state = Arc::clone(&self.state);
        let notify_slot = Arc::clone(&self.notify);
        let open_directory = Arc::clone(&self.open_directory);
        match std::thread::Builder::new()
            .name("talker-log-folder".into())
            .spawn(move || match open_directory(&path) {
                Ok(()) => publish_state(&state, &notify_slot, LogFolderState::Idle),
                Err(error) => publish_state(
                    &state,
                    &notify_slot,
                    LogFolderState::Failed(format!("{error:#}")),
                ),
            }) {
            Ok(_) => true,
            Err(error) => {
                let message = format!("starting the log-folder helper: {error}");
                publish_state(&self.state, &self.notify, LogFolderState::Failed(message));
                true
            }
        }
    }
}

fn create_and_open_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating the log folder {}", path.display()))?;
    launch_directory(path)
}

#[cfg(target_os = "windows")]
fn launch_directory(path: &Path) -> anyhow::Result<()> {
    launch("explorer.exe", path)
}

#[cfg(target_os = "macos")]
fn launch_directory(path: &Path) -> anyhow::Result<()> {
    launch("open", path)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launch_directory(path: &Path) -> anyhow::Result<()> {
    launch("xdg-open", path)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
fn launch_directory(_path: &Path) -> anyhow::Result<()> {
    anyhow::bail!("opening a folder is not supported on this platform")
}

#[cfg(any(target_os = "windows", target_os = "macos", unix))]
fn launch(program: &str, path: &Path) -> anyhow::Result<()> {
    let status = Command::new(program)
        .arg(path.as_os_str())
        .status()
        .with_context(|| format!("opening the log folder {}", path.display()))?;
    anyhow::ensure!(
        status.success(),
        "opening the log folder {} returned {status}",
        path.display()
    );
    Ok(())
}

fn publish_state(
    state: &Mutex<LogFolderState>,
    notify_slot: &Mutex<Option<Notify>>,
    next: LogFolderState,
) {
    *lock(state) = next;
    notify(notify_slot);
}

fn notify(slot: &Mutex<Option<Notify>>) {
    let callback = lock(slot).clone();
    if let Some(callback) = callback {
        callback();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use super::*;

    fn notification_receiver(opener: &LogFolderOpener) -> crossbeam_channel::Receiver<()> {
        let (tx, rx) = crossbeam_channel::unbounded();
        opener.set_notify(Arc::new(move || {
            let _ = tx.send(());
        }));
        rx
    }

    fn wait_for_notifications(rx: &crossbeam_channel::Receiver<()>, count: usize) {
        for _ in 0..count {
            rx.recv_timeout(Duration::from_secs(2))
                .expect("folder state notification");
        }
    }

    #[test]
    fn exact_path_is_opened_on_a_helper_thread() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let opener = LogFolderOpener::new(Arc::new(move |path| {
            tx.send((path.to_path_buf(), std::thread::current().id()))
                .unwrap();
            Ok(())
        }));
        let notifications = notification_receiver(&opener);
        let caller = std::thread::current().id();
        let path = PathBuf::from(r"C:\logs with spaces\café");

        assert!(opener.open(path.clone()));
        let (opened, thread) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(opened, path);
        assert_ne!(thread, caller);
        wait_for_notifications(&notifications, 2);
        assert_eq!(opener.state(), LogFolderState::Idle);
    }

    #[test]
    fn a_second_request_is_rejected_while_the_first_is_running() {
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let opener = LogFolderOpener::new(Arc::new(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        }));
        let notifications = notification_receiver(&opener);

        assert!(opener.open(PathBuf::from("first")));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!opener.open(PathBuf::from("second")));
        release_tx.send(()).unwrap();
        wait_for_notifications(&notifications, 2);
        assert_eq!(opener.state(), LogFolderState::Idle);
    }

    #[test]
    fn completion_and_failure_notify_and_failure_remains_visible() {
        let opener = LogFolderOpener::new(Arc::new(|_| anyhow::bail!("launcher denied")));
        let notifications = notification_receiver(&opener);

        assert!(opener.open(PathBuf::from("logs")));
        wait_for_notifications(&notifications, 2);
        let state = opener.state();
        assert!(
            matches!(state, LogFolderState::Failed(message) if message.contains("launcher denied"))
        );
        assert!(notifications.is_empty());
    }

    #[test]
    fn a_failed_request_can_be_retried() {
        let attempts = Arc::new(AtomicU64::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let opener = LogFolderOpener::new(Arc::new(move |_| {
            if worker_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("first launch failed");
            }
            Ok(())
        }));
        let notifications = notification_receiver(&opener);

        assert!(opener.open(PathBuf::from("logs")));
        wait_for_notifications(&notifications, 2);
        assert!(matches!(opener.state(), LogFolderState::Failed(_)));
        assert!(opener.open(PathBuf::from("logs")));
        wait_for_notifications(&notifications, 2);
        assert_eq!(opener.state(), LogFolderState::Idle);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_identical_failure_is_published_for_each_retry() {
        let attempts = Arc::new(AtomicU64::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let opener = LogFolderOpener::new(Arc::new(move |_| {
            worker_attempts.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("same launcher failure")
        }));
        let notifications = notification_receiver(&opener);

        assert!(opener.open(PathBuf::from("logs")));
        wait_for_notifications(&notifications, 2);
        assert!(opener.open(PathBuf::from("logs")));
        wait_for_notifications(&notifications, 2);

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(matches!(
            opener.state(),
            LogFolderState::Failed(message) if message.contains("same launcher failure")
        ));
        assert!(notifications.is_empty());
    }
}
