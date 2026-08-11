mod gui_layer;

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, reload, util::SubscriberInitExt, Layer, Registry,
};

pub use gui_layer::{GuiLogLayer, LogEvent};

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
pub struct LogLevelHandle(reload::Handle<LevelFilter, Registry>);

impl LogLevelHandle {
    /// Set the global filter to `level`. Errors only if the
    /// subscriber has been torn down.
    pub fn set(&self, level: LogLevel) -> anyhow::Result<()> {
        self.0
            .modify(|f| *f = to_level_filter(level))
            .map_err(|e| anyhow::anyhow!("updating log level: {e}"))
    }
}

/// Keeps background logging threads alive for the process lifetime.
///
/// Drop this only when the application is shutting down. Dropping it earlier
/// will stop file log flushing.
pub struct LoggingHandle {
    _guards: Vec<tracing_appender::non_blocking::WorkerGuard>,
    level: LogLevelHandle,
}

impl LoggingHandle {
    /// A cloneable handle for runtime log-level changes. Hand to the
    /// GUI so a ComboBox can adjust the filter without taking the
    /// `LoggingHandle` away from `run()`'s scope.
    pub fn level_handle(&self) -> LogLevelHandle {
        self.level.clone()
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

    // Wrap the level filter in a reload layer so the GUI can change
    // the level at runtime via [`LogLevelHandle::set`].
    let (filter_layer, level_handle) = reload::Layer::new(to_level_filter(config.level));

    // Build all layers into a single vec so the subscriber type stays
    // `Registry` throughout and dynamic dispatch compiles cleanly.
    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync + 'static>> =
        vec![filter_layer.boxed()];

    if config.stdout {
        layers.push(tracing_subscriber::fmt::layer().with_target(false).boxed());
    }

    if let Some(fc) = &config.file {
        let appender = make_rolling_appender(fc);
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);
        guards.push(guard);
        layers.push(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(non_blocking)
                .boxed(),
        );
    }

    if let Some(sender) = gui_sender {
        layers.push(GuiLogLayer::new(sender).boxed());
    }

    tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .context("installing global tracing subscriber (already initialized?)")?;

    Ok(LoggingHandle {
        _guards: guards,
        level: LogLevelHandle(level_handle),
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

fn make_rolling_appender(config: &FileLogConfig) -> tracing_appender::rolling::RollingFileAppender {
    match config.rotation {
        Rotation::Never => tracing_appender::rolling::never(&config.directory, &config.prefix),
        Rotation::Hourly => tracing_appender::rolling::hourly(&config.directory, &config.prefix),
        Rotation::Daily => tracing_appender::rolling::daily(&config.directory, &config.prefix),
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
