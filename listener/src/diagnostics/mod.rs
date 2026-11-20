//! Runtime events, warnings, errors, and diagnostic logging (spec §91–§95,
//! §114–§118).
//!
//! This is `listener-diagnostics` (§128). It owns the diagnostic record model
//! (Events §92, Warnings §93, Errors §94), bounded per-type history
//! ([`DiagnosticLog`], §86/§88 — each severity count-capped, oldest evicted),
//! and diagnostic logging ([`init_logging`], §114).

mod console;

pub use console::{Console, ConsoleGuard, CONSOLE_QUEUE_LINES};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::Layer;
use wiredata_log::{FileLogState, FileLogToggle, FileSinkGuard, LogFileConfig, Rotation};

use crate::core::ChannelId;
use crate::retention::{CountBounded, DEFAULT_BACKSTOP};

/// Severity of a diagnostic, ordered low → high priority (§92–§95).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum DiagnosticSeverity {
    /// Something happened (§92: channel started, client connected, …).
    Event,
    /// May affect operation but does not prevent it (§93).
    Warning,
    /// Failure to complete or continue an operation (§94).
    Error,
}

/// A diagnostic record retained for review (§91–§95). `timestamp` is the wall-clock
/// time the record was created (millisecond display precision, §26-style).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    /// Wall-clock time only. On a coarse clock (Windows, about 15 ms) two
    /// entries can share a timestamp, and the newest is then chosen by
    /// severity rather than by order of recording. Lifecycle and recording
    /// entries are separated by I/O far longer than a tick, so this has not
    /// been seen; if it ever matters, add a per-Channel sequence number and
    /// order by that.
    pub timestamp: SystemTime,
}

impl Diagnostic {
    pub fn new(severity: DiagnosticSeverity, message: impl Into<String>) -> Self {
        Self {
            severity,
            message: message.into(),
            timestamp: SystemTime::now(),
        }
    }

    /// Construct with an explicit timestamp (for deterministic tests / replay).
    pub fn at(
        severity: DiagnosticSeverity,
        message: impl Into<String>,
        timestamp: SystemTime,
    ) -> Self {
        Self {
            severity,
            message: message.into(),
            timestamp,
        }
    }

    pub fn event(message: impl Into<String>) -> Self {
        Self::new(DiagnosticSeverity::Event, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(DiagnosticSeverity::Warning, message)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(DiagnosticSeverity::Error, message)
    }
}

/// Bounded diagnostic history (§86, §88). Events, Warnings, and Errors are
/// retained **separately**, each count-limited (§88, from `RetentionConfig`'s
/// `event_limit`/`warning_limit`/`error_limit`). Eviction is oldest-first (§89);
/// [`clear`](Self::clear) discards all retained diagnostics (§90). An unset limit
/// falls back to the hard backstop so memory stays bounded (§80, §124).
pub struct DiagnosticLog {
    events: CountBounded<Diagnostic>,
    warnings: CountBounded<Diagnostic>,
    errors: CountBounded<Diagnostic>,
    revision: u64,
    /// The Channel this log belongs to. When set, every recorded diagnostic is
    /// also emitted to the persistent event log (§118).
    channel: Option<ChannelLabel>,
}

/// How a Channel is named in the event log: its name for people, its UUID as
/// the stable key (§118).
struct ChannelLabel {
    name: String,
    id: ChannelId,
}

impl DiagnosticLog {
    pub fn new(
        event_limit: Option<usize>,
        warning_limit: Option<usize>,
        error_limit: Option<usize>,
    ) -> Self {
        let cap = |limit: Option<usize>| limit.unwrap_or(DEFAULT_BACKSTOP);
        Self {
            events: CountBounded::new(cap(event_limit)),
            warnings: CountBounded::new(cap(warning_limit)),
            errors: CountBounded::new(cap(error_limit)),
            revision: 0,
            channel: None,
        }
    }

    /// Attribute this log to a Channel, so each diagnostic recorded from now on
    /// also reaches the persistent event log (§118).
    pub fn for_channel(mut self, name: impl Into<String>, id: ChannelId) -> Self {
        self.channel = Some(ChannelLabel {
            name: name.into(),
            id,
        });
        self
    }

    /// How many times this log's contents have changed.
    ///
    /// A poll-driven consumer rebuilds its view only when this moves. The log is
    /// polled far more often than it is written — a quiet channel is still polled
    /// several times a second — so the cheap comparison saves cloning every
    /// retained entry on every poll (§124).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Record a diagnostic into the store for its severity (§88), and emit it to
    /// the persistent event log when this log belongs to a Channel (§118).
    pub fn record(&mut self, diagnostic: Diagnostic) {
        if let Some(channel) = &self.channel {
            emit_to_event_log(&channel.name, channel.id, &diagnostic);
        }
        self.store(diagnostic);
    }

    fn store(&mut self, diagnostic: Diagnostic) {
        match diagnostic.severity {
            DiagnosticSeverity::Event => self.events.push(diagnostic),
            DiagnosticSeverity::Warning => self.warnings.push(diagnostic),
            DiagnosticSeverity::Error => self.errors.push(diagnostic),
        }
        // Eviction changes the contents too, so every push is a new revision.
        self.revision = self.revision.saturating_add(1);
    }

    pub fn events(&self) -> impl Iterator<Item = &Diagnostic> {
        self.events.iter()
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.warnings.iter()
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.errors.iter()
    }

    /// Discard all retained diagnostics (§90).
    pub fn clear(&mut self) {
        self.events.clear();
        self.warnings.clear();
        self.errors.clear();
        self.revision = self.revision.saturating_add(1);
    }

    /// Seed this (fresh) log with prior diagnostics, so a restarted Channel keeps the
    /// previous run's log instead of starting blank (§88, within-session). The entries
    /// are stored in chronological order, so the per-severity caps still bound the
    /// result (oldest dropped). They are not emitted to the event log again: they
    /// were logged when they happened. Intended to be called once, on a freshly
    /// constructed log.
    pub fn seed(&mut self, mut prior: Vec<Diagnostic>) {
        prior.sort_by_key(|d| d.timestamp);
        for d in prior {
            self.store(d);
        }
    }
}

/// Emit one Channel diagnostic as a `tracing` event: the persistent event log's
/// source (§118). The Channel name leads the line; the UUID is a stable field.
pub(crate) fn emit_to_event_log(name: &str, id: ChannelId, diagnostic: &Diagnostic) {
    let message = &diagnostic.message;
    match diagnostic.severity {
        DiagnosticSeverity::Event => {
            tracing::info!(channel = %name, channel_id = %id, "{message}")
        }
        DiagnosticSeverity::Warning => {
            tracing::warn!(channel = %name, channel_id = %id, "{message}")
        }
        DiagnosticSeverity::Error => {
            tracing::error!(channel = %name, channel_id = %id, "{message}")
        }
    }
}

/// The event log's file name prefix; daily files are `listener.log.<date>`.
const EVENT_LOG_PREFIX: &str = "listener.log";

/// The platform's folder for event log files (§118): `listener/logs` under the
/// local-data directory. `None` when the platform has no such directory.
pub fn event_log_folder() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("listener").join("logs"))
}

/// Log line timestamps in local time with the UTC offset, such as
/// `2026-09-30T08:14:03.512-04:00`: the operator's clock, and still
/// unambiguous. The event log's files are named for the local day, so their
/// lines use the same clock.
struct LocalTime;

impl tracing_subscriber::fmt::time::FormatTime for LocalTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        )
    }
}

/// The least time between attempts to turn a failed event log back on. The
/// first attempt comes this long after the failure is first seen, so a
/// destination that fails at once is not retried in a loop.
pub const EVENT_LOG_RETRY_SPACING: Duration = Duration::from_secs(60);

/// A change in whether the event log is being saved, reported once by
/// [`EventLogStatus::tend`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventLogChange {
    /// The log has just stopped being saved, for this reason.
    StartedFailing(String),
    /// The log is being saved again after this long.
    Recovered { after: Duration },
}

/// What [`EventLogStatus::tend`] remembers between calls. Clones of the status
/// share it, so they agree about whether the log is failing.
#[derive(Default)]
struct FailureTrack {
    /// When the current failure was first seen; `None` while the log is well.
    first_seen: Option<Instant>,
    /// The reason the worker last gave.
    reason: Option<String>,
    /// When a re-enable was last requested for this failure.
    last_attempt: Option<Instant>,
}

/// Whether a failed event log is due another attempt: at least
/// [`EVENT_LOG_RETRY_SPACING`] after the last attempt, or after the failure
/// was first seen when none has been made.
fn retry_due(first_seen: Instant, last_attempt: Option<Instant>, now: Instant) -> bool {
    let since = last_attempt.unwrap_or(first_seen);
    now.saturating_duration_since(since) >= EVENT_LOG_RETRY_SPACING
}

/// What the CLI and GUI show about the persistent event log (§118). Cheap to
/// clone and to poll.
#[derive(Clone)]
pub struct EventLogStatus {
    folder: Option<PathBuf>,
    toggle: Option<FileLogToggle>,
    /// What the toggle was enabled with, so a failed log can be enabled again.
    config: Option<LogFileConfig>,
    setup_problem: Option<String>,
    track: Arc<Mutex<FailureTrack>>,
}

impl EventLogStatus {
    /// The status of a run with no event log (tests only).
    #[cfg(test)]
    pub(crate) fn absent() -> Self {
        EventLog::unavailable(None, "this test writes no event log".to_owned()).status
    }

    /// The folder the event log is written to, when there is one.
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    fn track(&self) -> std::sync::MutexGuard<'_, FailureTrack> {
        self.track
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Why the event log is not being written, if it is not (§117: this never
    /// stops Listener; it is reported so someone can act on it). While a
    /// retry after a failure is under way the last reason still stands.
    pub fn problem(&self) -> Option<String> {
        if let Some(problem) = &self.setup_problem {
            return Some(problem.clone());
        }
        match self.toggle.as_ref().map(FileLogToggle::state) {
            Some(FileLogState::Failed(reason)) => Some(reason),
            Some(FileLogState::Enabling) => {
                let track = self.track();
                track.first_seen.and(track.reason.clone())
            }
            _ => None,
        }
    }

    /// Check the event log, turn a failed one back on when a retry is due, and
    /// report a change once. Call it on the application's regular poll with
    /// the current time.
    ///
    /// A retry that fails again is the same failure and reports nothing. When
    /// the log is saved again, one WARN line in the log itself says how long
    /// entries are missing.
    pub fn tend(&self, now: Instant) -> Option<EventLogChange> {
        let toggle = self.toggle.as_ref()?;
        let state = toggle.state();
        let mut track = self.track();
        match state {
            FileLogState::Failed(reason) => {
                let newly_failed = track.first_seen.is_none();
                let first_seen = *track.first_seen.get_or_insert(now);
                track.reason = Some(reason.clone());
                if let Some(config) = &self.config {
                    if retry_due(first_seen, track.last_attempt, now) {
                        track.last_attempt = Some(now);
                        // A worker that is gone leaves the state `Failed`, and
                        // the next attempt finds that out again.
                        let _ = toggle.enable(config.clone());
                    }
                }
                newly_failed.then_some(EventLogChange::StartedFailing(reason))
            }
            FileLogState::Enabled { .. } => {
                track.last_attempt = None;
                let first_seen = track.first_seen.take()?;
                let reason = track.reason.take().unwrap_or_default();
                drop(track);
                let after = now.saturating_duration_since(first_seen);
                tracing::warn!(
                    "the event log was not saved for {}: {reason}; \
                     entries from that time are missing",
                    wiredata_cli::describe(after)
                );
                Some(EventLogChange::Recovered { after })
            }
            FileLogState::Disabled | FileLogState::Enabling | FileLogState::Disabling => None,
        }
    }

    /// Log lines dropped because the writer could not keep up. The file marks
    /// where they were lost.
    pub fn lost_entries(&self) -> u64 {
        self.toggle
            .as_ref()
            .map_or(0, FileLogToggle::dropped_events)
    }
}

/// The persistent event log (§118). Hold it for the life of the process:
/// dropping it drains the queued lines and flushes the file.
#[must_use = "dropping the event log stops it; hold it until the process exits"]
pub struct EventLog {
    status: EventLogStatus,
    guard: Option<FileSinkGuard>,
}

impl EventLog {
    pub fn status(&self) -> EventLogStatus {
        self.status.clone()
    }

    /// Stop the event log at application exit: write out the queued lines
    /// and flush the file, waiting at most `limit`. `None` when no event log
    /// was being written.
    ///
    /// A log on a drive that stopped answering cannot be waited out, so the
    /// outcome says whether its last lines were written.
    pub fn shutdown(mut self, limit: Duration) -> Option<wiredata_log::ShutdownOutcome> {
        self.guard.take().map(|guard| guard.shutdown(limit))
    }

    fn unavailable(folder: Option<PathBuf>, problem: String) -> Self {
        Self {
            status: EventLogStatus {
                folder,
                toggle: None,
                config: None,
                setup_problem: Some(problem),
                track: Arc::default(),
            },
            guard: None,
        }
    }
}

/// A `tracing` layer that writes INFO and above to daily files in `folder`
/// through the bounded `wiredata-log` worker (§118, ADR-044). Old files are
/// kept (talker ADR-069).
fn event_log_layer<S>(folder: &Path) -> anyhow::Result<(impl Layer<S> + Send + Sync, EventLog)>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let sink = wiredata_log::spawn("Listener")?;
    let config = LogFileConfig {
        directory: folder.to_path_buf(),
        prefix: EVENT_LOG_PREFIX.to_owned(),
        rotation: Rotation::DailyLocal,
    };
    sink.toggle.enable(config.clone())?;
    let layer = tracing_subscriber::fmt::layer()
        .with_timer(LocalTime)
        .with_ansi(false)
        .with_writer(sink.writer)
        .with_filter(LevelFilter::INFO);
    let log = EventLog {
        status: EventLogStatus {
            folder: Some(folder.to_path_buf()),
            toggle: Some(sink.toggle),
            config: Some(config),
            setup_problem: None,
            track: Arc::default(),
        },
        guard: Some(sink.guard),
    };
    Ok((layer, log))
}

/// Initialize diagnostic logging (§114, §118): stdout filtered by `RUST_LOG`
/// (default `info`), plus the persistent event log at INFO and above.
/// Logging failure is non-fatal (§117): the returned [`EventLog`] reports any
/// problem instead. Call once per process and hold both results until exit.
///
/// Stdout is written through the returned console's queue, so a console that
/// stops taking output never holds up the thread that logged.
pub fn init_logging() -> (EventLog, ConsoleGuard) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let console = ConsoleGuard::open();
    let stdout = tracing_subscriber::fmt::layer()
        .with_timer(LocalTime)
        .with_ansi(console::stdout_takes_colour())
        .with_writer(console.console().log_writer())
        .with_filter(filter);
    let folder = event_log_folder();
    let (file, log) = match &folder {
        Some(folder) => match event_log_layer(folder) {
            Ok((layer, log)) => (Some(layer), log),
            Err(error) => (
                None,
                EventLog::unavailable(Some(folder.clone()), format!("{error:#}")),
            ),
        },
        None => (
            None,
            EventLog::unavailable(None, "this system has no local data folder".to_owned()),
        ),
    };
    if tracing_subscriber::registry()
        .with(stdout)
        .with(file)
        .try_init()
        .is_err()
    {
        return (
            EventLog::unavailable(folder, "logging was already set up".to_owned()),
            console,
        );
    }
    (log, console)
}

/// Capturing what reaches the event log, for tests anywhere in the crate.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};

    /// Formatted log output, collected for assertions.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// Run `emit` with a scoped subscriber and return what it logged.
    ///
    /// `tracing` caches callsite interest process-wide. A callsite first reached
    /// on another test thread can otherwise be registered against that thread's
    /// empty default and stay invisible here. A second live dispatch makes the
    /// registrar consult its active list, and the mutex keeps two capturing
    /// tests from routing into each other while that cache is rebuilt.
    pub(crate) fn capture(emit: impl FnOnce()) -> String {
        let out = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(out.clone())
            .finish();
        with_subscriber(subscriber, emit);
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        text
    }

    /// Run `emit` with `subscriber` as this thread's default, under the same
    /// guard as [`capture`].
    pub(crate) fn with_subscriber(
        subscriber: impl tracing::Subscriber + Send + Sync + 'static,
        emit: impl FnOnce(),
    ) {
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _sentinel = tracing::Dispatch::new(tracing_subscriber::registry());
        tracing::subscriber::with_default(subscriber, emit);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::capture;
    use super::*;
    use crate::core::ChannelId;

    #[test]
    fn a_channel_diagnostic_also_reaches_the_event_log() {
        // §118: every diagnostic also goes to the persistent log, naming the
        // Channel and carrying its UUID as a stable field.
        let id = ChannelId::new();
        let text = capture(|| {
            let mut log = DiagnosticLog::new(None, None, None).for_channel("GPS", id);
            log.record(Diagnostic::error("serial read failed: device removed"));
        });
        assert!(text.contains("ERROR"), "{text}");
        assert!(
            text.contains("serial read failed: device removed"),
            "{text}"
        );
        assert!(text.contains("channel=GPS"), "{text}");
        assert!(text.contains(&id.to_string()), "{text}");
    }

    #[test]
    fn channel_diagnostics_are_saved_to_the_daily_event_log_file() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let folder =
            std::env::temp_dir().join(format!("listener_event_log_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let id = ChannelId::new();

        let (layer, log) = event_log_layer(&folder).unwrap();
        let toggle = log.status.toggle.clone().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !matches!(toggle.state(), FileLogState::Enabled { .. }) {
            assert!(
                std::time::Instant::now() < deadline,
                "event log never opened: {:?}",
                toggle.state()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        super::test_support::with_subscriber(tracing_subscriber::registry().with(layer), || {
            let mut diagnostics = DiagnosticLog::new(None, None, None).for_channel("GPS", id);
            diagnostics.record(Diagnostic::error(
                "transport fault on channel GPS: device removed",
            ));
        });
        drop(log); // drains the queue and flushes the file

        let mut text = String::new();
        for entry in std::fs::read_dir(&folder).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(name.starts_with("listener.log."), "unexpected file {name}");
            text.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
        let _ = std::fs::remove_dir_all(&folder);
        assert!(
            text.contains("transport fault on channel GPS: device removed"),
            "{text}"
        );
        assert!(text.contains("channel=GPS"), "{text}");
        assert!(text.contains(&id.to_string()), "{text}");
        assert!(
            !text.contains('\u{1b}'),
            "no colour codes in a file: {text:?}"
        );
    }

    #[test]
    fn a_retry_is_not_due_before_the_spacing_has_passed() {
        let first_seen = Instant::now();
        let almost = first_seen + EVENT_LOG_RETRY_SPACING - Duration::from_millis(1);
        assert!(!retry_due(first_seen, None, first_seen));
        assert!(!retry_due(first_seen, None, almost));
    }

    #[test]
    fn the_first_retry_is_due_one_spacing_after_the_failure_was_seen() {
        let first_seen = Instant::now();
        assert!(retry_due(
            first_seen,
            None,
            first_seen + EVENT_LOG_RETRY_SPACING
        ));
    }

    #[test]
    fn the_next_retry_is_due_one_spacing_after_the_last_attempt() {
        let first_seen = Instant::now();
        let attempt = first_seen + EVENT_LOG_RETRY_SPACING;
        let almost = attempt + EVENT_LOG_RETRY_SPACING - Duration::from_millis(1);
        assert!(!retry_due(first_seen, Some(attempt), attempt));
        assert!(!retry_due(first_seen, Some(attempt), almost));
        assert!(retry_due(
            first_seen,
            Some(attempt),
            attempt + EVENT_LOG_RETRY_SPACING
        ));
    }

    #[test]
    fn a_failed_event_log_is_reported_once_retried_and_reported_when_saved_again() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let root =
            std::env::temp_dir().join(format!("listener_event_log_tend_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&root);
        // A file where the log folder should be: the open fails until it goes.
        std::fs::write(&root, b"in the way").unwrap();

        let (layer, log) = event_log_layer(&root).unwrap();
        let status = log.status();
        let toggle = log.status.toggle.clone().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(toggle.state(), FileLogState::Failed(_)) {
            assert!(Instant::now() < deadline, "the open never failed");
            std::thread::sleep(Duration::from_millis(5));
        }

        let start = Instant::now();
        let reason = status.problem().expect("the failure is a problem");
        assert_eq!(
            status.tend(start),
            Some(EventLogChange::StartedFailing(reason))
        );
        assert_eq!(status.tend(start + Duration::from_secs(1)), None);
        assert_eq!(status.tend(start + EVENT_LOG_RETRY_SPACING / 2), None);
        assert!(status.problem().is_some());
        assert!(matches!(toggle.state(), FileLogState::Failed(_)));

        std::fs::remove_file(&root).unwrap();
        let later = start + EVENT_LOG_RETRY_SPACING + Duration::from_secs(1);
        let mut recovered = None;
        let text = super::test_support::capture(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while recovered.is_none() {
                // Once the log is saved again the problem has gone; until
                // then, including while the retry is in flight, it stands.
                let problem = status.problem();
                if problem.is_none() {
                    assert!(
                        matches!(toggle.state(), FileLogState::Enabled { .. }),
                        "no problem while {:?}",
                        toggle.state()
                    );
                }
                recovered = status.tend(later);
                assert!(Instant::now() < deadline, "the log never recovered");
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        assert_eq!(
            recovered,
            Some(EventLogChange::Recovered {
                after: later - start
            })
        );
        assert_eq!(status.problem(), None);
        assert_eq!(status.tend(later + EVENT_LOG_RETRY_SPACING), None);
        assert!(text.contains("WARN"), "{text}");
        assert!(
            text.contains("the event log was not saved for 1 min: "),
            "{text}"
        );
        assert!(
            text.contains("; entries from that time are missing"),
            "{text}"
        );

        // The reopened file takes lines again.
        super::test_support::with_subscriber(tracing_subscriber::registry().with(layer), || {
            DiagnosticLog::new(None, None, None)
                .for_channel("GPS", ChannelId::new())
                .record(Diagnostic::error("written after the recovery"));
        });
        drop(log);
        let mut saved = String::new();
        for entry in std::fs::read_dir(&root).unwrap().flatten() {
            saved.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(saved.contains("written after the recovery"), "{saved}");
    }

    #[test]
    fn replaying_a_previous_run_does_not_log_it_again() {
        // A restarted Channel keeps its earlier diagnostics (§89.1), but they
        // were logged when they happened.
        let text = capture(|| {
            let mut log = DiagnosticLog::new(None, None, None).for_channel("GPS", ChannelId::new());
            log.seed(vec![Diagnostic::warning("from the last run")]);
        });
        assert!(!text.contains("from the last run"), "{text}");
    }

    #[test]
    fn record_routes_by_severity() {
        let mut log = DiagnosticLog::new(None, None, None);
        log.record(Diagnostic::event("started"));
        log.record(Diagnostic::warning("checksum invalid"));
        log.record(Diagnostic::warning("queue overflow"));
        log.record(Diagnostic::error("port not found"));

        assert_eq!(log.events().count(), 1);
        assert_eq!(log.warnings().count(), 2);
        assert_eq!(log.errors().count(), 1);
    }

    #[test]
    fn per_type_limits_evict_oldest() {
        // Keep at most one warning; events/errors generous.
        let mut log = DiagnosticLog::new(None, Some(1), None);
        log.record(Diagnostic::warning("first"));
        log.record(Diagnostic::warning("second"));
        let warnings: Vec<&str> = log.warnings().map(|d| d.message.as_str()).collect();
        assert_eq!(warnings, vec!["second"]); // oldest evicted (§89)
    }

    #[test]
    fn clear_discards_all() {
        let mut log = DiagnosticLog::new(None, None, None);
        log.record(Diagnostic::event("e"));
        log.record(Diagnostic::error("x"));
        log.clear();
        assert_eq!(log.events().count(), 0);
        assert_eq!(log.errors().count(), 0);
    }

    #[test]
    fn severity_orders_low_to_high() {
        assert!(DiagnosticSeverity::Event < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Error);
    }

    #[test]
    fn diagnostics_carry_a_timestamp() {
        use std::time::{Duration, SystemTime};
        // Constructed records stamp "now"; `at` lets tests pin an exact time so a
        // merged log can be ordered chronologically across severities.
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_millis(250);
        let a = Diagnostic::at(DiagnosticSeverity::Event, "first", t0);
        let b = Diagnostic::at(DiagnosticSeverity::Error, "second", t1);
        assert!(a.timestamp < b.timestamp);

        let fresh = Diagnostic::event("now");
        assert!(fresh.timestamp <= SystemTime::now());
    }
}
