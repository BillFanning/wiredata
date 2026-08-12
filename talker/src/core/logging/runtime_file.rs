use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

use anyhow::Context;
use tracing_subscriber::fmt::MakeWriter;

use super::{make_rolling_appender, FileLogConfig};

const FILE_EVENT_QUEUE_CAP: usize = 4_096;

type FileWriter = Box<dyn Write + Send>;
type OpenFileFn = Box<dyn Fn(&FileLogConfig) -> anyhow::Result<FileWriter> + Send>;
type Notify = Arc<dyn Fn() + Send + Sync>;

/// Runtime state of the GUI's optional file destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileLogState {
    Disabled,
    Enabling,
    Enabled { directory: PathBuf, prefix: String },
    Disabling,
    Failed(String),
}

/// Cloneable, non-blocking control for the GUI's optional file destination.
///
/// Requests cross a channel to the file worker. Opening, writing, flushing, and
/// closing the file therefore never happen on the UI or talker threads.
#[derive(Clone)]
pub struct FileLogToggle {
    commands: crossbeam_channel::Sender<FileCommand>,
    state: Arc<Mutex<FileLogState>>,
    accepting: Arc<AtomicBool>,
    dropped_events: Arc<AtomicU64>,
    notify: Arc<Mutex<Option<Notify>>>,
}

impl FileLogToggle {
    pub fn state(&self) -> FileLogState {
        lock_state(&self.state).clone()
    }

    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    pub fn set_notify(&self, notify: Notify) {
        *lock_notify(&self.notify) = Some(notify);
    }

    pub fn enable(&self, config: FileLogConfig) -> anyhow::Result<()> {
        // Do not enqueue events until the worker has proved it can open the
        // destination. Events emitted while the file is opening still reach
        // the pane and any other installed sink.
        self.accepting.store(false, Ordering::Release);
        publish_state(&self.state, &self.notify, FileLogState::Enabling);
        if self.commands.send(FileCommand::Enable(config)).is_err() {
            let error = "file-log worker is not running".to_owned();
            publish_state(
                &self.state,
                &self.notify,
                FileLogState::Failed(error.clone()),
            );
            anyhow::bail!(error);
        }
        Ok(())
    }

    pub fn disable(&self) -> anyhow::Result<()> {
        // Stop accepting at the request boundary; the worker owns the actual
        // flush and close.
        self.accepting.store(false, Ordering::Release);
        publish_state(&self.state, &self.notify, FileLogState::Disabling);
        if self.commands.send(FileCommand::Disable).is_err() {
            let error = "file-log worker is not running".to_owned();
            publish_state(
                &self.state,
                &self.notify,
                FileLogState::Failed(error.clone()),
            );
            anyhow::bail!(error);
        }
        Ok(())
    }
}

pub(super) struct RuntimeFileSink {
    pub writer: RuntimeFileMakeWriter,
    pub toggle: FileLogToggle,
    pub guard: RuntimeFileGuard,
}

pub(super) fn spawn() -> anyhow::Result<RuntimeFileSink> {
    spawn_with(Box::new(|config| {
        Ok(Box::new(make_rolling_appender(config)?) as FileWriter)
    }))
}

fn spawn_with(open: OpenFileFn) -> anyhow::Result<RuntimeFileSink> {
    let (event_tx, event_rx) = crossbeam_channel::bounded(FILE_EVENT_QUEUE_CAP);
    let (command_tx, command_rx) = crossbeam_channel::unbounded();
    let state = Arc::new(Mutex::new(FileLogState::Disabled));
    let accepting = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicU64::new(0));
    let dropped_events = Arc::new(AtomicU64::new(0));
    let notify = Arc::new(Mutex::new(None));
    let worker_state = Arc::clone(&state);
    let worker_accepting = Arc::clone(&accepting);
    let worker_generation = Arc::clone(&generation);
    let worker_notify = Arc::clone(&notify);
    let thread = std::thread::Builder::new()
        .name("talker-file-log".into())
        .spawn(move || {
            run_worker(
                event_rx,
                command_rx,
                worker_state,
                worker_accepting,
                worker_generation,
                worker_notify,
                open,
            )
        })
        .context("starting file-log worker")?;

    Ok(RuntimeFileSink {
        writer: RuntimeFileMakeWriter {
            events: event_tx,
            accepting: Arc::clone(&accepting),
            generation: Arc::clone(&generation),
            dropped_events: Arc::clone(&dropped_events),
        },
        toggle: FileLogToggle {
            commands: command_tx.clone(),
            state,
            accepting,
            dropped_events,
            notify,
        },
        guard: RuntimeFileGuard {
            commands: command_tx,
            thread: Some(thread),
        },
    })
}

enum FileCommand {
    Enable(FileLogConfig),
    Disable,
    Shutdown,
}

fn run_worker(
    events: crossbeam_channel::Receiver<FileEvent>,
    commands: crossbeam_channel::Receiver<FileCommand>,
    state: Arc<Mutex<FileLogState>>,
    accepting: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    notify: Arc<Mutex<Option<Notify>>>,
    open: OpenFileFn,
) {
    let mut file: Option<ActiveFile> = None;
    loop {
        crossbeam_channel::select_biased! {
            recv(commands) -> command => match command {
                Ok(FileCommand::Enable(config)) => {
                    accepting.store(false, Ordering::Release);
                    if let Err(error) = finish_file(&events, &mut file) {
                        publish_state(
                            &state,
                            &notify,
                            FileLogState::Failed(format!(
                                "finishing the previous file log: {error}"
                            )),
                        );
                        continue;
                    }
                    match open(&config) {
                        Ok(opened) => {
                            // The worker owns generation allocation as well as
                            // file replacement. Concurrent control clones cannot
                            // reorder a caller-side id and its command.
                            let generation = generation.fetch_add(1, Ordering::AcqRel) + 1;
                            file = Some(ActiveFile {
                                generation,
                                writer: opened,
                            });
                            accepting.store(true, Ordering::Release);
                            publish_state(
                                &state,
                                &notify,
                                FileLogState::Enabled {
                                    directory: config.directory,
                                    prefix: config.prefix,
                                },
                            );
                        }
                        Err(error) => {
                            publish_state(
                                &state,
                                &notify,
                                FileLogState::Failed(format!("{error:#}")),
                            );
                        }
                    }
                }
                Ok(FileCommand::Disable) => {
                    accepting.store(false, Ordering::Release);
                    let result = finish_file(&events, &mut file);
                    publish_state(
                        &state,
                        &notify,
                        match result {
                            Ok(()) => FileLogState::Disabled,
                            Err(error) => FileLogState::Failed(format!(
                                "finishing the file log while disabling it: {error}"
                            )),
                        },
                    );
                }
                Ok(FileCommand::Shutdown) | Err(_) => {
                    accepting.store(false, Ordering::Release);
                    let _ = finish_file(&events, &mut file);
                    break;
                }
            },
            recv(events) -> event => match event {
                Ok(event) => {
                    let Some(active) = file.as_mut() else {
                        continue;
                    };
                    if event.generation != active.generation {
                        continue;
                    }
                    if let Err(error) = active.writer.write_all(&event.bytes) {
                        accepting.store(false, Ordering::Release);
                        file = None;
                        publish_state(
                            &state,
                            &notify,
                            FileLogState::Failed(format!("writing the file log: {error}")),
                        );
                    }
                }
                Err(_) => break,
            }
        }
    }
}

struct ActiveFile {
    generation: u64,
    writer: FileWriter,
}

struct FileEvent {
    generation: u64,
    bytes: Vec<u8>,
}

/// Write the current session's records already queued at the stop/replacement
/// boundary, then flush and close its destination. Generation tags quarantine
/// any late prior-session record so it cannot enter a later file.
fn finish_file(
    events: &crossbeam_channel::Receiver<FileEvent>,
    file: &mut Option<ActiveFile>,
) -> io::Result<()> {
    let result = match file.as_mut() {
        Some(active) => (|| {
            for event in events.try_iter() {
                if event.generation == active.generation {
                    active.writer.write_all(&event.bytes)?;
                }
            }
            active.writer.flush()
        })(),
        None => {
            events.try_iter().for_each(drop);
            Ok(())
        }
    };
    *file = None;
    result
}

fn lock_state(state: &Mutex<FileLogState>) -> std::sync::MutexGuard<'_, FileLogState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_notify(notify: &Mutex<Option<Notify>>) -> std::sync::MutexGuard<'_, Option<Notify>> {
    notify
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn publish_state(state: &Mutex<FileLogState>, notify: &Mutex<Option<Notify>>, next: FileLogState) {
    *lock_state(state) = next;
    let callback = lock_notify(notify).clone();
    if let Some(callback) = callback {
        callback();
    }
}

#[derive(Clone)]
pub(super) struct RuntimeFileMakeWriter {
    events: crossbeam_channel::Sender<FileEvent>,
    accepting: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    dropped_events: Arc<AtomicU64>,
}

impl<'a> MakeWriter<'a> for RuntimeFileMakeWriter {
    type Writer = RuntimeEventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RuntimeEventWriter {
            events: self.events.clone(),
            bytes: Vec::new(),
            accepted_at_start: self.accepting.load(Ordering::Acquire),
            accepting: Arc::clone(&self.accepting),
            generation: self.generation.load(Ordering::Acquire),
            current_generation: Arc::clone(&self.generation),
            dropped_events: Arc::clone(&self.dropped_events),
        }
    }
}

pub(super) struct RuntimeEventWriter {
    events: crossbeam_channel::Sender<FileEvent>,
    bytes: Vec<u8>,
    accepted_at_start: bool,
    accepting: Arc<AtomicBool>,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    dropped_events: Arc<AtomicU64>,
}

impl Write for RuntimeEventWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.accepted_at_start {
            self.bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RuntimeEventWriter {
    fn drop(&mut self) {
        if !self.accepted_at_start
            || !self.accepting.load(Ordering::Acquire)
            || self.bytes.is_empty()
            || self.current_generation.load(Ordering::Acquire) != self.generation
        {
            return;
        }
        if self
            .events
            .try_send(FileEvent {
                generation: self.generation,
                bytes: std::mem::take(&mut self.bytes),
            })
            .is_err()
        {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(super) struct RuntimeFileGuard {
    commands: crossbeam_channel::Sender<FileCommand>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for RuntimeFileGuard {
    fn drop(&mut self) {
        let _ = self.commands.send(FileCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt as _, reload, Layer as _};

    fn config() -> FileLogConfig {
        FileLogConfig::new("unused-by-the-in-memory-writer")
    }

    fn wait_for_state(
        toggle: &FileLogToggle,
        wanted: impl Fn(&FileLogState) -> bool,
    ) -> FileLogState {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let state = toggle.state();
            if wanted(&state) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "file-log state stayed at {state:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn toggle_opens_writes_and_closes_only_on_the_worker() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened_on = Arc::new(Mutex::new(None));
        let writes_on = Arc::new(Mutex::new(Vec::new()));
        let flushes_on = Arc::new(Mutex::new(Vec::new()));
        let dropped_on = Arc::new(Mutex::new(None));
        let worker_written = Arc::clone(&written);
        let worker_opened_on = Arc::clone(&opened_on);
        let worker_writes_on = Arc::clone(&writes_on);
        let worker_flushes_on = Arc::clone(&flushes_on);
        let worker_dropped_on = Arc::clone(&dropped_on);
        let sink = spawn_with(Box::new(move |_| {
            *worker_opened_on.lock().unwrap() = Some(std::thread::current().id());
            Ok(Box::new(ObservedWriter {
                bytes: Arc::clone(&worker_written),
                writes_on: Arc::clone(&worker_writes_on),
                flushes_on: Arc::clone(&worker_flushes_on),
                dropped_on: Arc::clone(&worker_dropped_on),
            }))
        }))
        .unwrap();

        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });
        assert_ne!(
            *opened_on.lock().unwrap(),
            Some(std::thread::current().id()),
            "opening must not run on the caller/UI thread"
        );

        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"one complete event\n").unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while written.lock().unwrap().is_empty() {
            assert!(
                Instant::now() < deadline,
                "file worker did not write the event"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });
        let caller = std::thread::current().id();
        let write_threads = writes_on.lock().unwrap();
        assert!(!write_threads.is_empty(), "the worker must write the event");
        assert!(write_threads.iter().all(|thread| *thread != caller));
        let flush_threads = flushes_on.lock().unwrap();
        assert!(!flush_threads.is_empty(), "disable must flush the writer");
        assert!(flush_threads.iter().all(|thread| *thread != caller));
        assert!(
            dropped_on
                .lock()
                .unwrap()
                .is_some_and(|thread| thread != caller),
            "the worker must close the writer"
        );
        let before = written.lock().unwrap().clone();
        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"hidden while disabled\n").unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(*written.lock().unwrap(), before);
    }

    #[test]
    fn failed_open_is_reported_and_keeps_the_sink_disabled() {
        let sink = spawn_with(Box::new(|_| anyhow::bail!("permission denied"))).unwrap();
        sink.toggle.enable(config()).unwrap();
        let state = wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Failed(_))
        });
        assert!(
            matches!(state, FileLogState::Failed(message) if message.contains("permission denied"))
        );
        assert!(!sink.writer.accepting.load(Ordering::Acquire));
    }

    #[test]
    fn a_failed_open_can_be_retried_without_rebuilding_the_subscriber() {
        let attempts = Arc::new(AtomicU64::new(0));
        let written = Arc::new(Mutex::new(Vec::new()));
        let worker_attempts = Arc::clone(&attempts);
        let worker_written = Arc::clone(&written);
        let sink = spawn_with(Box::new(move |_| {
            if worker_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("device is not ready");
            }
            Ok(Box::new(SharedWriter(Arc::clone(&worker_written))))
        }))
        .unwrap();

        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Failed(_))
        });
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });
        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"written after retry\n").unwrap();
        }
        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert!(text.contains("written after retry"), "{text}");
    }

    #[test]
    fn concurrent_enable_requests_leave_the_final_destination_accepting() {
        let opens = Arc::new(AtomicU64::new(0));
        let written = Arc::new(Mutex::new(Vec::new()));
        let worker_opens = Arc::clone(&opens);
        let worker_written = Arc::clone(&written);
        let sink = spawn_with(Box::new(move |_| {
            worker_opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(SharedWriter(Arc::clone(&worker_written))))
        }))
        .unwrap();
        let first = sink.toggle.clone();
        let second = sink.toggle.clone();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let first_barrier = Arc::clone(&barrier);
        let first_request = std::thread::spawn(move || {
            first_barrier.wait();
            first.enable(config()).unwrap();
        });
        let second_barrier = Arc::clone(&barrier);
        let second_request = std::thread::spawn(move || {
            second_barrier.wait();
            second.enable(config()).unwrap();
        });
        barrier.wait();
        first_request.join().unwrap();
        second_request.join().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
                && opens.load(Ordering::SeqCst) == 2
                && sink.writer.generation.load(Ordering::Acquire) == 2
                && sink.writer.accepting.load(Ordering::Acquire)
        });

        {
            let mut event = sink.writer.make_writer();
            event
                .write_all(b"accepted by the final destination\n")
                .unwrap();
        }
        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });

        let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert!(text.contains("accepted by the final destination"), "{text}");
    }

    #[test]
    fn write_failure_disables_the_sink_and_notifies_the_gui() {
        let notifications = Arc::new(AtomicU64::new(0));
        let notified = Arc::clone(&notifications);
        let sink = spawn_with(Box::new(|_| Ok(Box::new(FailingWriter)))).unwrap();
        sink.toggle.set_notify(Arc::new(move || {
            notified.fetch_add(1, Ordering::SeqCst);
        }));
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });
        let before_failure = notifications.load(Ordering::SeqCst);

        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"write will fail\n").unwrap();
        }
        let state = wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Failed(_))
        });
        assert!(matches!(state, FileLogState::Failed(message) if message.contains("disk full")));
        assert!(notifications.load(Ordering::SeqCst) > before_failure);
        assert!(!sink.writer.accepting.load(Ordering::Acquire));
    }

    #[test]
    fn flush_failure_is_reported_when_file_logging_stops() {
        let sink = spawn_with(Box::new(|_| Ok(Box::new(FlushFailingWriter)))).unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        sink.toggle.disable().unwrap();
        let state = wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Failed(_))
        });
        assert!(matches!(state, FileLogState::Failed(message) if message.contains("flush denied")));
        assert!(!sink.writer.accepting.load(Ordering::Acquire));
    }

    #[test]
    fn shared_recording_threshold_gates_the_runtime_file_sink() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let worker_written = Arc::clone(&written);
        let sink = spawn_with(Box::new(move |_| {
            Ok(Box::new(SharedWriter(Arc::clone(&worker_written))))
        }))
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        let layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = vec![tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(sink.writer.clone())
            .boxed()];
        let sinks = tracing_subscriber::registry().with(layers);
        let (filter, handle) = reload::Layer::new(LevelFilter::INFO);
        let subscriber = sinks.with(filter);
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!("trace before trace threshold");
            tracing::debug!("debug before debug threshold");
            tracing::info!("info at info threshold");

            handle.modify(|level| *level = LevelFilter::DEBUG).unwrap();
            tracing::debug!("debug at debug threshold");
            tracing::trace!("trace before trace threshold again");

            handle.modify(|level| *level = LevelFilter::TRACE).unwrap();
            tracing::trace!("trace at trace threshold");
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
            if text.contains("trace at trace threshold") {
                assert!(text.contains("info at info threshold"));
                assert!(text.contains("debug at debug threshold"));
                assert!(!text.contains("debug before debug threshold"));
                assert!(!text.contains("trace before trace threshold"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "file worker did not drain: {text}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_full_file_queue_increments_the_exposed_drop_count() {
        let (events, _receiver) = crossbeam_channel::bounded(0);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let generation = Arc::new(AtomicU64::new(1));
        let writer = RuntimeFileMakeWriter {
            events,
            accepting: Arc::new(AtomicBool::new(true)),
            generation,
            dropped_events: Arc::clone(&dropped_events),
        };
        {
            let mut event = writer.make_writer();
            event.write_all(b"cannot be queued").unwrap();
        }
        assert_eq!(dropped_events.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn disable_writes_every_record_accepted_before_the_request() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let worker_written = Arc::clone(&written);
        let sink = spawn_with(Box::new(move |_| {
            Ok(Box::new(SharedWriter(Arc::clone(&worker_written))))
        }))
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        for line in [b"first accepted record\n".as_slice(), b"tail record\n"] {
            let mut event = sink.writer.make_writer();
            event.write_all(line).unwrap();
        }
        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });

        let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert!(text.contains("first accepted record"), "{text}");
        assert!(text.contains("tail record"), "{text}");
    }

    #[test]
    fn an_event_still_being_formatted_at_disable_is_not_queued_after_close() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let worker_written = Arc::clone(&written);
        let sink = spawn_with(Box::new(move |_| {
            Ok(Box::new(SharedWriter(Arc::clone(&worker_written))))
        }))
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        let mut unfinished = sink.writer.make_writer();
        unfinished
            .write_all(b"still formatting at disable\n")
            .unwrap();
        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });
        drop(unfinished);

        assert!(written.lock().unwrap().is_empty());
    }

    #[test]
    fn shutdown_writes_the_accepted_tail_before_closing() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let worker_written = Arc::clone(&written);
        let sink = spawn_with(Box::new(move |_| {
            Ok(Box::new(SharedWriter(Arc::clone(&worker_written))))
        }))
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"last event before shutdown\n").unwrap();
        }
        drop(sink.guard);

        let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert!(text.contains("last event before shutdown"), "{text}");
    }

    #[test]
    fn an_event_writer_from_an_old_session_cannot_enter_the_new_file() {
        let first_file = Arc::new(Mutex::new(Vec::new()));
        let second_file = Arc::new(Mutex::new(Vec::new()));
        let opens = Arc::new(AtomicU64::new(0));
        let worker_opens = Arc::clone(&opens);
        let worker_first = Arc::clone(&first_file);
        let worker_second = Arc::clone(&second_file);
        let sink = spawn_with(Box::new(move |_| {
            let destination = if worker_opens.fetch_add(1, Ordering::SeqCst) == 0 {
                Arc::clone(&worker_first)
            } else {
                Arc::clone(&worker_second)
            };
            Ok(Box::new(SharedWriter(destination)))
        }))
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        let mut old_event = sink.writer.make_writer();
        old_event.write_all(b"belongs to old session\n").unwrap();
        let old_generation = old_event.generation;
        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. }) && opens.load(Ordering::SeqCst) == 2
        });
        // Model the narrow pre-check/send interleaving: even if an old event
        // reaches the queue after the new file opens, the worker rejects its
        // generation rather than writing it to the new destination.
        sink.writer
            .events
            .send(FileEvent {
                generation: old_generation,
                bytes: b"late old-generation record\n".to_vec(),
            })
            .unwrap();
        drop(old_event);
        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"belongs to new session\n").unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let text = String::from_utf8_lossy(&second_file.lock().unwrap()).into_owned();
            if text.contains("belongs to new session") {
                assert!(!text.contains("belongs to old session"), "{text}");
                assert!(!text.contains("late old-generation record"), "{text}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new file did not receive its event"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn reenable_does_not_replay_records_queued_for_the_old_file() {
        let (write_started_tx, write_started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let second_file = Arc::new(Mutex::new(Vec::new()));
        let opens = Arc::new(AtomicU64::new(0));
        let worker_opens = Arc::clone(&opens);
        let worker_release = Arc::clone(&release_rx);
        let worker_second_file = Arc::clone(&second_file);
        let sink = spawn_with(Box::new(move |_| {
            if worker_opens.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::new(BlockingWriter {
                    started: write_started_tx.clone(),
                    release: worker_release.lock().unwrap().take(),
                }))
            } else {
                Ok(Box::new(SharedWriter(Arc::clone(&worker_second_file))))
            }
        }))
        .unwrap();

        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });
        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"write already in progress\n").unwrap();
        }
        write_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first writer never blocked");
        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"stale queued record\n").unwrap();
        }

        sink.toggle.disable().unwrap();
        sink.toggle.enable(config()).unwrap();
        release_tx.send(()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. }) && opens.load(Ordering::SeqCst) == 2
        });
        {
            let mut event = sink.writer.make_writer();
            event.write_all(b"fresh record\n").unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let text = String::from_utf8_lossy(&second_file.lock().unwrap()).into_owned();
            if text.contains("fresh record") {
                assert!(!text.contains("stale queued record"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new file did not receive fresh record"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ObservedWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        writes_on: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        flushes_on: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        dropped_on: Arc<Mutex<Option<std::thread::ThreadId>>>,
    }

    impl Write for ObservedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes_on
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes_on
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            Ok(())
        }
    }

    impl Drop for ObservedWriter {
        fn drop(&mut self) {
            *self.dropped_on.lock().unwrap() = Some(std::thread::current().id());
        }
    }

    struct BlockingWriter {
        started: crossbeam_channel::Sender<()>,
        release: Option<crossbeam_channel::Receiver<()>>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let _ = self.started.send(());
            if let Some(release) = self.release.take() {
                let _ = release.recv();
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FlushFailingWriter;

    impl Write for FlushFailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush denied"))
        }
    }
}
