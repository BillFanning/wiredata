mod folder;
mod gui_layer;

use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, reload, util::SubscriberInitExt, Layer, Registry,
};

pub(crate) use folder::{LogFolderOpener, LogFolderState};
pub use gui_layer::{GuiLogHealth, GuiLogLayer, LogEvent};
pub use wiredata_log::{FileLogState, FileLogToggle};

/// Run a test closure with GUI log capture on its current thread.
///
/// `tracing` caches callsite interest process-wide. A callsite first reached
/// concurrently on another test thread can otherwise be registered against
/// that thread's empty default and remain invisible to this scoped subscriber.
/// Keeping a second live dispatch makes the registrar consult its active
/// dispatch list; the mutex keeps two capture tests from routing into one
/// another while that cache is rebuilt.
#[cfg(test)]
pub(crate) fn with_gui_test_subscriber<T>(
    sender: crossbeam_channel::Sender<LogEvent>,
    run: impl FnOnce() -> T,
) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    let _capture_guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _registration_sentinel = tracing::Dispatch::new(tracing_subscriber::registry());
    let capture =
        tracing::Dispatch::new(tracing_subscriber::registry().with(GuiLogLayer::new(sender)));
    tracing::dispatcher::with_default(&capture, run)
}

// ── Config types ──────────────────────────────────────────────────────────────

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub level: LogLevel,
    /// Whether log events are written to stdout.
    #[serde(default = "default_true")]
    pub stdout: bool,
    #[serde(default)]
    pub file: Option<FileLogConfig>,
}

fn default_true() -> bool {
    true
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::default(),
            stdout: true,
            file: None,
        }
    }
}

impl LoggingConfig {
    pub fn new(level: LogLevel) -> Self {
        Self {
            level,
            ..Self::default()
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileLogConfig {
    pub directory: PathBuf,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default)]
    pub rotation: Rotation,
}

fn default_prefix() -> String {
    "talker.log".to_string()
}

impl FileLogConfig {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            prefix: default_prefix(),
            rotation: Rotation::default(),
        }
    }
}

/// The profile's file-log settings as the shared worker takes them. Talker
/// keeps every log file (ADR-061), so no age limit is set.
impl From<FileLogConfig> for wiredata_log::LogFileConfig {
    fn from(config: FileLogConfig) -> Self {
        Self {
            directory: config.directory,
            prefix: config.prefix,
            rotation: match config.rotation {
                Rotation::Never => wiredata_log::Rotation::Never,
                Rotation::Hourly => wiredata_log::Rotation::Hourly,
                Rotation::Daily => wiredata_log::Rotation::Daily,
            },
            max_age: None,
        }
    }
}

/// Minimum log level emitted by the subscriber.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Log file rotation schedule.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rotation {
    Never,
    Hourly,
    #[default]
    Daily,
}

// ── Timestamps ────────────────────────────────────────────────────────────────

/// Log line timestamps in local time with the UTC offset and milliseconds,
/// such as `2026-09-30T08:14:03.512-04:00`.
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

// ── Console ───────────────────────────────────────────────────────────────────

/// Log lines the stdout queue holds for a console that is slow or paused.
/// Past this, lines are dropped and counted.
pub const CONSOLE_QUEUE_LINES: usize = 4_096;

/// `stream` behind a bounded queue and a worker thread, for the stdout log.
///
/// A console can stop taking output: selecting text pauses a classic Windows
/// console window, and a pipe stalls when its reader does. A talker thread
/// that logged straight to it would wait with it, in the middle of its
/// schedule. Through the queue it never waits; a full queue drops the line and
/// counts it.
fn console_writer(stream: impl std::io::Write + Send + 'static) -> (NonBlocking, WorkerGuard) {
    NonBlockingBuilder::default()
        .lossy(true)
        .buffered_lines_limit(CONSOLE_QUEUE_LINES)
        .thread_name("talker-console")
        .finish(stream)
}

/// Whether log lines on standard output may carry colour codes.
///
/// Only when something will turn the codes into colour. A classic Windows
/// console window prints them as text (`←[2m…`) until a program turns on its
/// virtual-terminal processing, and a file or a pipe keeps them as clutter. The
/// level is always written out as a word, so no line loses meaning without
/// colour. Listener applies the same rule to its own stdout log.
fn stdout_takes_colour() -> bool {
    use std::io::IsTerminal as _;
    colour_decision(
        anstyle_query::no_color(),
        anstyle_query::clicolor_force(),
        std::io::stdout().is_terminal(),
        terminal_interprets_colour,
    )
}

/// `NO_COLOR` always wins, then `CLICOLOR_FORCE`; otherwise colour only on a
/// terminal that interprets it. `interprets` is asked last because on Windows
/// it changes the console's mode.
fn colour_decision(
    no_color: bool,
    force: bool,
    is_terminal: bool,
    interprets: impl FnOnce() -> bool,
) -> bool {
    if no_color {
        false
    } else if force {
        true
    } else {
        is_terminal && interprets()
    }
}

/// On Windows, turn on the console's virtual-terminal processing and report
/// whether that worked; elsewhere, ask the terminal type.
fn terminal_interprets_colour() -> bool {
    if cfg!(windows) {
        anstyle_query::windows::enable_ansi_colors().unwrap_or(false)
    } else {
        anstyle_query::term_supports_ansi_color()
    }
}

// ── Runtime handle ────────────────────────────────────────────────────────────

/// Cloneable handle for changing the minimum log level at runtime. The
/// new level takes effect on the next emitted event.
#[derive(Clone)]
pub struct LogLevelHandle(Arc<dyn Fn(LogLevel) -> anyhow::Result<()> + Send + Sync>);

impl LogLevelHandle {
    /// Set the global filter to `level`. Errors only if the
    /// subscriber has been torn down.
    pub fn set(&self, level: LogLevel) -> anyhow::Result<()> {
        (self.0)(level)
    }
}

/// Keeps background logging threads alive for the process lifetime.
///
/// Drop this only when the application is shutting down. Dropping it earlier
/// will stop file log flushing.
pub struct LoggingHandle {
    _guards: Vec<WorkerGuard>,
    /// Counts the stdout log lines dropped at a full queue; `None` when
    /// nothing is logged to stdout.
    console_dropped: Option<ErrorCounter>,
    runtime_file_guard: Option<wiredata_log::FileSinkGuard>,
    level: LogLevelHandle,
    file_log: Option<FileLogToggle>,
    gui_log_health: Option<GuiLogHealth>,
}

impl LoggingHandle {
    /// A cloneable handle for runtime log-level changes. Hand to the
    /// GUI so a ComboBox can adjust the filter without taking the
    /// `LoggingHandle` away from `run()`'s scope.
    pub fn level_handle(&self) -> LogLevelHandle {
        self.level.clone()
    }

    /// Control and loss count for the file destination: runtime on/off for the
    /// GUI, read-only in effect for the CLI, whose destination is fixed at
    /// launch. `None` when nothing is logged to a file.
    pub fn file_log_toggle(&self) -> Option<FileLogToggle> {
        self.file_log.clone()
    }

    /// Health of the bounded GUI-pane event transport.
    ///
    /// `None` in CLI mode. Pane loss is independent of file-log loss.
    pub fn gui_log_health(&self) -> Option<GuiLogHealth> {
        self.gui_log_health.clone()
    }

    /// How many log lines stdout did not show because its queue was full: the
    /// console was not taking output.
    pub fn console_lines_not_shown(&self) -> u64 {
        self.console_dropped
            .as_ref()
            .map_or(0, |dropped| dropped.dropped_lines() as u64)
    }

    /// Stop logging at application exit: write out what the queues hold,
    /// waiting at most `limit` for the log file. `None` when nothing was
    /// logged to a file.
    ///
    /// A log file on a drive that stopped answering cannot be waited out, so
    /// the outcome says whether its last lines were written.
    pub fn shutdown(mut self, limit: std::time::Duration) -> Option<wiredata_log::ShutdownOutcome> {
        self.runtime_file_guard
            .take()
            .map(|guard| guard.shutdown(limit))
    }
}

// ── init ──────────────────────────────────────────────────────────────────────

/// Start the file-log worker when there is something to write to: a file named
/// in the configuration, or a GUI that may name one later.
///
/// An unwritable destination fails here, before anything runs, as the CLI has
/// always done: the worker opens its file on its own thread and would otherwise
/// report the failure only as a state nobody is watching.
fn start_file_sink(
    file: Option<&FileLogConfig>,
    toggleable: bool,
) -> anyhow::Result<Option<wiredata_log::FileSink>> {
    if file.is_none() && !toggleable {
        return Ok(None);
    }
    if let Some(file) = file {
        drop(wiredata_log::open_rolling_file(&file.clone().into())?);
    }
    let sink = wiredata_log::spawn("Talker")?;
    if let Some(file) = file {
        sink.toggle.enable(file.clone())?;
    }
    Ok(Some(sink))
}

/// Install the global tracing subscriber.
///
/// Must be called exactly once per process. Subsequent calls will return an
/// error (`already initialized`).
///
/// Pass `gui_sender` to attach a [`GuiLogLayer`] that forwards events to the
/// GUI status pane.
pub fn init(
    config: &LoggingConfig,
    gui_sender: Option<crossbeam_channel::Sender<LogEvent>>,
) -> anyhow::Result<LoggingHandle> {
    let mut guards: Vec<WorkerGuard> = vec![];
    let mut console_dropped = None;
    let (gui_layer, gui_log_health) = match gui_sender {
        Some(sender) => {
            let health = GuiLogHealth::new();
            (
                Some(GuiLogLayer::with_health(sender, health.clone())),
                Some(health),
            )
        }
        None => (None, None),
    };
    let gui_mode = gui_layer.is_some();
    let mut runtime_file_guard = None;
    let mut file_log = None;

    // Build all layers into a single vec so the subscriber type stays
    // `Registry` throughout and dynamic dispatch compiles cleanly.
    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync + 'static>> = vec![];

    if config.stdout {
        let (stdout, guard) = console_writer(std::io::stdout());
        console_dropped = Some(stdout.error_counter());
        guards.push(guard);
        layers.push(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_timer(LocalTime)
                .with_ansi(stdout_takes_colour())
                .with_writer(stdout)
                .boxed(),
        );
    }

    // One file path for both front ends: a worker thread behind a bounded,
    // non-blocking handoff, which counts what it drops and writes a gap line
    // after a loss (ADR-061). The GUI may turn its destination on and off after
    // the subscriber is installed; the CLI's is fixed at launch. Either way no
    // open, write, flush or close runs on a thread that logs.
    if let Some(sink) = start_file_sink(config.file.as_ref(), gui_mode)? {
        layers.push(
            tracing_subscriber::fmt::layer()
                .with_timer(LocalTime)
                .with_ansi(false)
                .with_writer(sink.writer)
                .boxed(),
        );
        file_log = Some(sink.toggle);
        runtime_file_guard = Some(sink.guard);
    }

    if let Some(layer) = gui_layer {
        layers.push(layer.boxed());
    }

    // The reloadable level filter must wrap the complete sink stack. Putting a
    // filtering layer inside the `Vec` is ineffective: another layer's
    // callsite interest can make the vector permanently interested before its
    // runtime `enabled` checks run.
    let sinks = tracing_subscriber::registry().with(layers);
    let (filter_layer, raw_level_handle) = reload::Layer::new(to_level_filter(config.level));
    sinks
        .with(filter_layer)
        .try_init()
        .context("installing global tracing subscriber (already initialized?)")?;
    let level_handle = LogLevelHandle(Arc::new(move |level| {
        raw_level_handle
            .modify(|filter| *filter = to_level_filter(level))
            .map_err(|error| anyhow::anyhow!("updating log level: {error}"))
    }));

    Ok(LoggingHandle {
        _guards: guards,
        console_dropped,
        runtime_file_guard,
        level: level_handle,
        file_log,
        gui_log_health,
    })
}

/// The OS-appropriate directory for log files when no path is configured.
///
/// Returns `None` if the platform's local-data directory cannot be determined.
pub fn default_log_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("talker").join("logs"))
}

fn to_level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Trace => LevelFilter::TRACE,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Error => LevelFilter::ERROR,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Console ───────────────────────────────────────────────────────────────

    /// A console that takes nothing until it is released: what a paused
    /// console window is to the thread writing to it.
    struct Paused(std::sync::mpsc::Receiver<()>);

    impl std::io::Write for Paused {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Colour codes reach stdout only where they become colour; the user's
    /// own `NO_COLOR` and `CLICOLOR_FORCE` come first, in that order.
    #[test]
    fn colour_is_used_only_where_it_will_show_as_colour() {
        let never = || -> bool { panic!("a decided case must not touch the console") };

        // A file or a pipe: plain text, and the console is not touched.
        assert!(!colour_decision(false, false, false, never));
        // A terminal that interprets the codes, and one that does not.
        assert!(colour_decision(false, false, true, || true));
        assert!(!colour_decision(false, false, true, || false));
        // NO_COLOR beats everything, CLICOLOR_FORCE beats the terminal check.
        assert!(!colour_decision(true, true, true, never));
        assert!(colour_decision(false, true, false, never));
    }

    #[test]
    fn a_console_that_takes_no_output_does_not_hold_up_the_thread_that_logs() {
        use std::io::Write as _;
        let (release, paused) = std::sync::mpsc::channel();
        let (mut stdout, guard) = console_writer(Paused(paused));
        let not_shown = stdout.error_counter();

        // Far more than the queue holds, one write per line as the log layer
        // makes them. Written directly, the first would wait for the console
        // and this test would never finish.
        let lines = CONSOLE_QUEUE_LINES * 3;
        let started = std::time::Instant::now();
        for n in 0..lines {
            stdout
                .write_all(format!("line {n}\n").as_bytes())
                .expect("the queue takes or drops every line");
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "logging waited on the console"
        );
        // The worker holds one line and the queue the rest it has room for.
        let dropped = not_shown.dropped_lines();
        assert!(
            dropped >= lines - CONSOLE_QUEUE_LINES - 1,
            "{dropped} of {lines} dropped"
        );
        assert!(dropped < lines, "some lines were queued");

        // Let the worker end, so the guard has nothing to wait for.
        drop(release);
        drop(guard);
    }

    // ── Timestamps ────────────────────────────────────────────────────────────

    /// A `Write` into a shared buffer, for reading back a formatted line.
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A line opens with local time, milliseconds and a `+hh:mm`/`-hh:mm`
    /// offset, then the level.
    #[test]
    fn a_log_line_opens_with_local_time_and_its_offset() {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_timer(LocalTime)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || tracing::info!("stamped"));

        let line = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let stamp = line.split_whitespace().next().unwrap();
        let b = stamp.as_bytes();
        assert_eq!(stamp.len(), 29, "{line}");
        let digits_at = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
        assert!(digits_at(0..4) && b[4] == b'-' && digits_at(5..7) && b[7] == b'-');
        assert!(digits_at(8..10) && b[10] == b'T' && digits_at(11..13) && b[13] == b':');
        assert!(digits_at(14..16) && b[16] == b':' && digits_at(17..19) && b[19] == b'.');
        assert!(digits_at(20..23), "{line}");
        assert!(b[23] == b'+' || b[23] == b'-', "{line}");
        assert!(
            digits_at(24..26) && b[26] == b':' && digits_at(27..29),
            "{line}"
        );
        assert!(line.contains("INFO") && line.contains("stamped"), "{line}");
    }

    // ── LogLevel ──────────────────────────────────────────────────────────────

    #[test]
    fn log_level_default_is_info() {
        assert_eq!(LogLevel::default(), LogLevel::Info);
    }

    #[test]
    fn log_level_as_str_covers_all_variants() {
        assert_eq!(LogLevel::Trace.as_str(), "trace");
        assert_eq!(LogLevel::Debug.as_str(), "debug");
        assert_eq!(LogLevel::Info.as_str(), "info");
        assert_eq!(LogLevel::Warn.as_str(), "warn");
        assert_eq!(LogLevel::Error.as_str(), "error");
    }

    // ── Rotation ──────────────────────────────────────────────────────────────

    #[test]
    fn rotation_default_is_daily() {
        assert_eq!(Rotation::default(), Rotation::Daily);
    }

    // ── LoggingConfig ─────────────────────────────────────────────────────────

    #[test]
    fn logging_config_default_has_no_file() {
        let c = LoggingConfig::default();
        assert_eq!(c.level, LogLevel::Info);
        assert!(c.file.is_none());
    }

    #[test]
    fn logging_config_defaults_stdout_on() {
        assert!(LoggingConfig::default().stdout);
        assert!(LoggingConfig::new(LogLevel::Debug).stdout);
    }

    #[test]
    fn logging_config_deserializes_stdout_default() {
        // A config without a `stdout` key defaults to enabled.
        let c: LoggingConfig = serde_json::from_str(r#"{"level":"info"}"#).unwrap();
        assert!(c.stdout);
    }

    #[test]
    fn logging_config_new() {
        let c = LoggingConfig::new(LogLevel::Debug);
        assert_eq!(c.level, LogLevel::Debug);
        assert!(c.file.is_none());
    }

    // ── FileLogConfig ─────────────────────────────────────────────────────────

    #[test]
    fn file_log_config_defaults() {
        let c = FileLogConfig::new("/tmp/logs");
        assert_eq!(c.prefix, "talker.log");
        assert_eq!(c.rotation, Rotation::Daily);
    }

    // ── serde round-trips ─────────────────────────────────────────────────────

    #[test]
    fn logging_config_round_trip_no_file() {
        let c = LoggingConfig::new(LogLevel::Warn);
        let json = serde_json::to_string(&c).unwrap();
        let back: LoggingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn logging_config_round_trip_with_file() {
        let c = LoggingConfig {
            level: LogLevel::Debug,
            stdout: false,
            file: Some(FileLogConfig {
                directory: PathBuf::from("/var/log/talker"),
                prefix: "app.log".to_string(),
                rotation: Rotation::Hourly,
            }),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: LoggingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn log_level_serde_uses_snake_case() {
        let json = serde_json::to_string(&LogLevel::Warn).unwrap();
        assert_eq!(json, "\"warn\"");
        let back: LogLevel = serde_json::from_str("\"warn\"").unwrap();
        assert_eq!(back, LogLevel::Warn);
    }

    #[test]
    fn rotation_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&Rotation::Never).unwrap(),
            "\"never\""
        );
        assert_eq!(
            serde_json::to_string(&Rotation::Hourly).unwrap(),
            "\"hourly\""
        );
        assert_eq!(
            serde_json::to_string(&Rotation::Daily).unwrap(),
            "\"daily\""
        );
    }

    /// The CLI's file destination is the shared worker, not a plain appender:
    /// it has a toggle that reports state and loss, and its guard flushes what
    /// was accepted. Loss itself is wiredata-log's to prove; this proves the
    /// CLI configuration reaches it.
    #[test]
    fn a_cli_file_log_runs_through_the_loss_counting_worker() {
        let directory =
            std::env::temp_dir().join(format!("talker-cli-file-log-{}", std::process::id()));
        let mut file = FileLogConfig::new(&directory);
        file.prefix = "cli.log".into();
        file.rotation = Rotation::Never;

        let sink = start_file_sink(Some(&file), false)
            .unwrap()
            .expect("a configured file starts the worker without a GUI");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(
            sink.toggle.state(),
            wiredata_log::FileLogState::Enabled { .. }
        ) {
            assert!(std::time::Instant::now() < deadline, "worker never opened");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            sink.toggle.dropped_events(),
            0,
            "loss is counted, from zero"
        );

        let wiredata_log::FileSink { writer, guard, .. } = sink;
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(writer)
            .finish();
        tracing::subscriber::with_default(subscriber, || tracing::info!("through the worker"));
        drop(guard);

        let saved = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<String>();
        let _ = std::fs::remove_dir_all(&directory);
        assert!(saved.contains("through the worker"), "{saved}");
    }

    #[test]
    fn no_file_and_no_gui_starts_no_worker() {
        assert!(start_file_sink(None, false).unwrap().is_none());
        assert!(start_file_sink(None, true).unwrap().is_some());
    }

    #[test]
    fn an_unwritable_cli_log_destination_fails_at_start() {
        let blocker = std::env::temp_dir().join(format!(
            "talker-cli-file-log-blocker-{}",
            std::process::id()
        ));
        std::fs::write(&blocker, b"a file where a folder is wanted").unwrap();
        let file = FileLogConfig::new(blocker.join("logs"));
        let result = start_file_sink(Some(&file), false);
        let _ = std::fs::remove_file(&blocker);
        assert!(result.is_err());
    }
}
