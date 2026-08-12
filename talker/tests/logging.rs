use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use talker::core::logging::{self, FileLogConfig, FileLogState, LogLevel, LoggingConfig, Rotation};

#[test]
fn production_level_control_gates_the_gui_and_runtime_file_together() {
    let directory = unique_log_directory();
    let cleanup = RemoveOnDrop(directory.clone());
    let (gui_tx, gui_rx) = crossbeam_channel::bounded(32);
    let mut config = LoggingConfig::new(LogLevel::Info);
    config.stdout = false;
    let logging = logging::init(&config, Some(gui_tx)).unwrap();
    let pane_health = logging
        .gui_log_health()
        .expect("GUI pane health is installed with GUI capture");
    let file = logging.file_log_toggle().expect("GUI runtime file layer");
    let mut file_config = FileLogConfig::new(directory.clone());
    file_config.prefix = "integration.log".into();
    file_config.rotation = Rotation::Never;
    file.enable(file_config).unwrap();
    wait_for_file_state(&file, |state| matches!(state, FileLogState::Enabled { .. }));

    tracing::debug!("debug excluded at info");
    tracing::trace!("trace excluded at info");
    tracing::info!("info included at info");

    logging.level_handle().set(LogLevel::Debug).unwrap();
    tracing::debug!("debug included at debug");
    tracing::trace!("trace excluded at debug");

    logging.level_handle().set(LogLevel::Trace).unwrap();
    tracing::trace!("trace included at trace");

    file.disable().unwrap();
    wait_for_file_state(&file, |state| matches!(state, FileLogState::Disabled));
    drop(logging);

    let pane = gui_rx
        .try_iter()
        .map(|event| event.message)
        .collect::<Vec<_>>()
        .join("\n");
    assert_threshold_results(&pane);
    assert_eq!(pane_health.dropped_events(), 0);

    let saved = read_log_files(&directory);
    assert_threshold_results(&saved);

    drop(cleanup);
}

fn assert_threshold_results(text: &str) {
    assert!(text.contains("info included at info"), "{text}");
    assert!(text.contains("debug included at debug"), "{text}");
    assert!(text.contains("trace included at trace"), "{text}");
    assert!(!text.contains("debug excluded at info"), "{text}");
    assert!(!text.contains("trace excluded at info"), "{text}");
    assert!(!text.contains("trace excluded at debug"), "{text}");
}

fn wait_for_file_state(file: &logging::FileLogToggle, wanted: impl Fn(&FileLogState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = file.state();
        if wanted(&state) {
            return;
        }
        assert!(Instant::now() < deadline, "file state stayed at {state:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn unique_log_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "wiredata-talker-logging-{}-{nonce}",
        std::process::id()
    ))
}

fn read_log_files(directory: &Path) -> String {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
