mod folder;
mod gui_layer;

use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use serde::{Deserialize, Serialize};
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
    _guards: Vec<tracing_appender::non_blocking::WorkerGuard>,
    _runtime_file_guard: Option<wiredata_log::FileSinkGuard>,
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

    /// Runtime control for the GUI's optional file destination.
    ///
    /// `None` in CLI mode, whose file destination is fixed at launch.
    pub fn file_log_toggle(&self) -> Option<FileLogToggle> {
        self.file_log.clone()
    }

    /// Health of the bounded GUI-pane event transport.
    ///
    /// `None` in CLI mode. Pane loss is independent of file-log loss.
    pub fn gui_log_health(&self) -> Option<GuiLogHealth> {
        self.gui_log_health.clone()
    }
}

// ── init ──────────────────────────────────────────────────────────────────────

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
    let mut guards: Vec<tracing_appender::non_blocking::WorkerGuard> = vec![];
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
        layers.push(tracing_subscriber::fmt::layer().with_target(false).boxed());
    }

    if !gui_mode {
        if let Some(fc) = &config.file {
            let appender = wiredata_log::open_rolling_file(&fc.clone().into())?;
            let (non_blocking, guard) = tracing_appender::non_blocking(appender);
            guards.push(guard);
            layers.push(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(non_blocking)
                    .boxed(),
            );
        }
    }

    // The GUI may turn its file destination on and off after the subscriber is
    // installed. Its layer is permanent, while the worker owns the optional
    // file handle; no open, write, flush, or close runs on the UI/talker thread.
    if gui_mode {
        let sink = wiredata_log::spawn("Talker")?;
        if let Some(config) = config.file.clone() {
            sink.toggle.enable(config)?;
        }
        layers.push(
            tracing_subscriber::fmt::layer()
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
        _runtime_file_guard: runtime_file_guard,
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
}
