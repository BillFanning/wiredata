//! One bounded log-file worker, shared by talker and listener (talker ADR-061).
//!
//! Formatting happens on the thread that emits an event; opening, writing,
//! flushing, rotating and closing the file happen only on a dedicated worker
//! behind a bounded, non-blocking handoff. When that handoff is full the event is
//! dropped and counted, and the worker writes a gap line once the records queued
//! ahead of the loss have drained. A failed open or write is published as a
//! state the application shows; it never stops the application.
//!
//! The crate owns only that mechanism, plus daily rotation and age-based
//! deletion of old log files. What is logged, its level and format, the folder
//! and file prefix, and how any of it is presented stay in the applications.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;
use tracing_subscriber::fmt::MakeWriter;

const FILE_EVENT_QUEUE_CAP: usize = 4_096;

/// How often a long-running log file checks for old files to delete. Daily
/// rotation makes one hour ample: a file is deleted within an hour of passing
/// its age limit.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

type FileWriter = Box<dyn Write + Send>;
type OpenFileFn = Box<dyn Fn(&LogFileConfig) -> anyhow::Result<FileWriter> + Send>;
type Notify = Arc<dyn Fn() + Send + Sync>;

/// Where a log file goes and how long old files are kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogFileConfig {
    pub directory: PathBuf,
    /// File name prefix; rotated files are named `<prefix>.<date>`.
    pub prefix: String,
    pub rotation: Rotation,
    /// Log files with this prefix older than this are deleted when the file
    /// opens and hourly after that. `None` keeps every file.
    pub max_age: Option<Duration>,
}

impl LogFileConfig {
    /// Daily rotation, keeping every file.
    pub fn new(directory: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            prefix: prefix.into(),
            rotation: Rotation::Daily,
            max_age: None,
        }
    }
}

/// When a log file is closed and the next one opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    Never,
    Hourly,
    #[default]
    Daily,
}

/// Open a rotating log file, first deleting files older than `max_age`.
///
/// The returned writer keeps deleting old files, at most hourly, while it is
/// written to. Deletion is best-effort: a file that cannot be deleted is left
/// and tried again at the next check.
pub fn open_rolling_file(config: &LogFileConfig) -> anyhow::Result<RollingLogFile> {
    let rotation = match config.rotation {
        Rotation::Never => tracing_appender::rolling::Rotation::NEVER,
        Rotation::Hourly => tracing_appender::rolling::Rotation::HOURLY,
        Rotation::Daily => tracing_appender::rolling::Rotation::DAILY,
    };
    if let Some(max_age) = config.max_age {
        prune_old_files(
            &config.directory,
            &config.prefix,
            max_age,
            SystemTime::now(),
        );
    }
    let inner = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(rotation)
        .filename_prefix(config.prefix.clone())
        .build(&config.directory)
        .with_context(|| {
            format!(
                "opening file log {:?} in {:?}",
                config.prefix, config.directory
            )
        })?;
    Ok(RollingLogFile {
        inner,
        directory: config.directory.clone(),
        prefix: config.prefix.clone(),
        max_age: config.max_age,
        next_prune: Instant::now() + PRUNE_INTERVAL,
    })
}

/// A rotating log file that also deletes its own old files.
pub struct RollingLogFile {
    inner: tracing_appender::rolling::RollingFileAppender,
    directory: PathBuf,
    prefix: String,
    max_age: Option<Duration>,
    next_prune: Instant,
}

impl Write for RollingLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(max_age) = self.max_age {
            let now = Instant::now();
            if now >= self.next_prune {
                self.next_prune = now + PRUNE_INTERVAL;
                prune_old_files(&self.directory, &self.prefix, max_age, SystemTime::now());
            }
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Delete the log files in `directory` with this prefix that were last written
/// more than `max_age` before `now`. Returns how many were deleted.
///
/// Only `<prefix>` itself and `<prefix>.<anything>` are candidates, so other
/// files in the folder are never touched.
fn prune_old_files(directory: &Path, prefix: &str, max_age: Duration, now: SystemTime) -> usize {
    let Some(cutoff) = now.checked_sub(max_age) else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    let dotted = format!("{prefix}.");
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == prefix || name.starts_with(&dotted)
        })
        .filter(|entry| {
            entry
                .metadata()
                .ok()
                .filter(std::fs::Metadata::is_file)
                .and_then(|meta| meta.modified().ok())
                .is_some_and(|modified| modified < cutoff)
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// Runtime state of an optional file destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileLogState {
    Disabled,
    Enabling,
    Enabled { directory: PathBuf, prefix: String },
    Disabling,
    Failed(String),
}

/// Cloneable, non-blocking control for an optional file destination.
///
/// Requests cross a channel to the file worker. Opening, writing, flushing, and
/// closing the file therefore never happen on a UI or application thread.
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

    pub fn enable(&self, config: impl Into<LogFileConfig>) -> anyhow::Result<()> {
        let config = config.into();
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

/// The three handles of a running file-log worker.
pub struct FileSink {
    /// The `tracing` writer: formats on the emitting thread and hands off
    /// without blocking.
    pub writer: FileMakeWriter,
    /// Turns the file on and off, and reports its state and loss count.
    pub toggle: FileLogToggle,
    /// Drains, flushes and stops the worker when dropped.
    pub guard: FileSinkGuard,
}

/// Start a file-log worker, with no file open yet.
///
/// `label` names the application in the worker thread's name and in gap lines,
/// such as "--- Talker file log gap: 3 earlier log entries were not saved. ---".
pub fn spawn(label: &'static str) -> anyhow::Result<FileSink> {
    spawn_with(
        label,
        Box::new(|config| Ok(Box::new(open_rolling_file(config)?) as FileWriter)),
    )
}

fn spawn_with(label: &'static str, open: OpenFileFn) -> anyhow::Result<FileSink> {
    spawn_with_capacity(label, open, FILE_EVENT_QUEUE_CAP)
}

fn spawn_with_capacity(
    label: &'static str,
    open: OpenFileFn,
    event_queue_cap: usize,
) -> anyhow::Result<FileSink> {
    let (event_tx, event_rx) = crossbeam_channel::bounded(event_queue_cap);
    let (command_tx, command_rx) = crossbeam_channel::unbounded();
    let state = Arc::new(Mutex::new(FileLogState::Disabled));
    let accepting = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicU64::new(0));
    let generation_drops = Arc::new(RwLock::new(Arc::new(GenerationDrops::new(0))));
    let dropped_events = Arc::new(AtomicU64::new(0));
    let notify = Arc::new(Mutex::new(None));
    let worker_state = Arc::clone(&state);
    let worker_accepting = Arc::clone(&accepting);
    let worker_generations = WorkerGenerations {
        current: Arc::clone(&generation),
        drops: Arc::clone(&generation_drops),
    };
    let worker_notify = Arc::clone(&notify);
    let thread = std::thread::Builder::new()
        .name(format!("{}-file-log", label.to_ascii_lowercase()))
        .spawn(move || {
            run_worker(
                event_rx,
                command_rx,
                worker_state,
                worker_accepting,
                worker_generations,
                worker_notify,
                Opener { label, open },
            )
        })
        .context("starting file-log worker")?;

    Ok(FileSink {
        writer: FileMakeWriter {
            events: event_tx,
            accepting: Arc::clone(&accepting),
            generation: Arc::clone(&generation),
            generation_drops,
            dropped_events: Arc::clone(&dropped_events),
            commands: command_tx.clone(),
        },
        toggle: FileLogToggle {
            commands: command_tx.clone(),
            state,
            accepting,
            dropped_events,
            notify,
        },
        guard: FileSinkGuard {
            commands: command_tx,
            thread: Some(thread),
        },
    })
}

enum FileCommand {
    Enable(LogFileConfig),
    Disable,
    CheckGap(Arc<GenerationDrops>),
    Shutdown,
}

struct WorkerGenerations {
    current: Arc<AtomicU64>,
    drops: Arc<RwLock<Arc<GenerationDrops>>>,
}

/// How the worker opens a destination, and the application label its gap lines
/// carry.
struct Opener {
    label: &'static str,
    open: OpenFileFn,
}

fn run_worker(
    events: crossbeam_channel::Receiver<FileEvent>,
    commands: crossbeam_channel::Receiver<FileCommand>,
    state: Arc<Mutex<FileLogState>>,
    accepting: Arc<AtomicBool>,
    generations: WorkerGenerations,
    notify: Arc<Mutex<Option<Notify>>>,
    opener: Opener,
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
                    match (opener.open)(&config) {
                        Ok(opened) => {
                            // The worker owns generation allocation as well as
                            // file replacement. Concurrent control clones cannot
                            // reorder a caller-side id and its command.
                            let generation =
                                generations.current.fetch_add(1, Ordering::AcqRel) + 1;
                            let drops = Arc::new(GenerationDrops::new(generation));
                            *write_generation_drops(&generations.drops) = Arc::clone(&drops);
                            file = Some(ActiveFile {
                                label: opener.label,
                                generation,
                                writer: opened,
                                marker_baseline: 0,
                                drops,
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
                Ok(FileCommand::CheckGap(drops)) => {
                    let result = file.as_mut().map_or(Ok(()), |active| {
                        if Arc::ptr_eq(&active.drops, &drops) {
                            write_ready_gap_markers(&events, active)
                        } else {
                            Ok(())
                        }
                    });
                    if file
                        .as_ref()
                        .is_none_or(|active| !Arc::ptr_eq(&active.drops, &drops))
                    {
                        // A stale notification can outlive its file generation.
                        // Close it so no late producer can leave bookkeeping
                        // pending on a tracker the worker no longer owns.
                        drops.close();
                    }
                    if let Err(error) = result {
                        accepting.store(false, Ordering::Release);
                        close_active_file(&mut file);
                        file = None;
                        publish_state(
                            &state,
                            &notify,
                            FileLogState::Failed(format!("writing the file log: {error}")),
                        );
                    }
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
                    let result: io::Result<()> = (|| {
                        if event.generation == active.generation {
                            active.writer.write_all(&event.bytes)?;
                        }
                        write_ready_gap_markers(&events, active)?;
                        Ok(())
                    })();
                    if let Err(error) = result {
                        accepting.store(false, Ordering::Release);
                        close_active_file(&mut file);
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
    label: &'static str,
    generation: u64,
    writer: FileWriter,
    drops: Arc<GenerationDrops>,
    marker_baseline: u64,
}

struct GenerationDrops {
    generation: u64,
    /// Queue-full producers touch this mutex only on the loss path. The worker
    /// holds it for bookkeeping only, never while draining the queue or writing
    /// the file, so a saturated producer cannot wait for disk I/O.
    state: Mutex<GenerationDropState>,
}

struct GenerationDropState {
    total: u64,
    /// Losses coalesce here until the worker claims their cumulative boundary.
    pending_target: Option<u64>,
    /// Fixed cumulative boundary claimed before the worker checks queue
    /// emptiness. Later losses go to `pending_target`, so their marker cannot
    /// move ahead of the records that were queued before them.
    claimed_target: Option<u64>,
    closed: bool,
}

impl GenerationDrops {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            state: Mutex::new(GenerationDropState {
                total: 0,
                pending_target: None,
                claimed_target: None,
                closed: false,
            }),
        }
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        lock_generation_drop_state(&self.state).total
    }

    fn claim_marker_target(&self) -> Option<u64> {
        let mut state = lock_generation_drop_state(&self.state);
        if state.claimed_target.is_none() {
            state.claimed_target = state.pending_target.take();
        }
        state.claimed_target
    }

    /// Close this generation and return the complete, now-stable drop count.
    fn close(&self) -> u64 {
        let mut state = lock_generation_drop_state(&self.state);
        state.closed = true;
        state.pending_target = None;
        state.claimed_target = None;
        state.total
    }

    /// Release one fixed marker batch and report whether loss arrived while it
    /// was being written.
    fn marker_written(&self, target: u64) -> bool {
        let mut state = lock_generation_drop_state(&self.state);
        debug_assert_eq!(state.claimed_target, Some(target));
        state.claimed_target = None;
        !state.closed && state.pending_target.is_some()
    }
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
            let final_drop_target = active.drops.close();
            for event in events.try_iter() {
                if event.generation == active.generation {
                    active.writer.write_all(&event.bytes)?;
                }
            }
            write_gap_marker(active, final_drop_target)?;
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

/// Write every marker batch whose causal queue records have already drained.
///
/// The target is copied before checking queue emptiness. A later loss therefore
/// belongs to a later target and cannot place its marker before the accepted
/// record that filled the queue ahead of it.
fn write_ready_gap_markers(
    events: &crossbeam_channel::Receiver<FileEvent>,
    active: &mut ActiveFile,
) -> io::Result<()> {
    loop {
        let Some(target) = active.drops.claim_marker_target() else {
            return Ok(());
        };
        if !events.is_empty() {
            return Ok(());
        }

        write_gap_marker(active, target)?;
        if !active.drops.marker_written(target) {
            return Ok(());
        }
    }
}

fn write_gap_marker(active: &mut ActiveFile, target: u64) -> io::Result<()> {
    let delta = target.saturating_sub(active.marker_baseline);
    if delta == 0 {
        return Ok(());
    }

    let marker = gap_marker_line(active.label, delta);
    active.writer.write_all(marker.as_bytes())?;
    active.marker_baseline = target;
    Ok(())
}

fn close_active_file(file: &mut Option<ActiveFile>) {
    if let Some(active) = file.as_ref() {
        active.drops.close();
    }
}

fn gap_marker_line(label: &str, dropped: u64) -> String {
    if dropped == 1 {
        format!("--- {label} file log gap: 1 earlier log entry was not saved. ---\n")
    } else {
        format!("--- {label} file log gap: {dropped} earlier log entries were not saved. ---\n")
    }
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

fn lock_generation_drop_state(
    state: &Mutex<GenerationDropState>,
) -> std::sync::MutexGuard<'_, GenerationDropState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_generation_drops(
    drops: &RwLock<Arc<GenerationDrops>>,
) -> std::sync::RwLockReadGuard<'_, Arc<GenerationDrops>> {
    drops
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_generation_drops(
    drops: &RwLock<Arc<GenerationDrops>>,
) -> std::sync::RwLockWriteGuard<'_, Arc<GenerationDrops>> {
    drops
        .write()
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
pub struct FileMakeWriter {
    events: crossbeam_channel::Sender<FileEvent>,
    accepting: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    generation_drops: Arc<RwLock<Arc<GenerationDrops>>>,
    dropped_events: Arc<AtomicU64>,
    commands: crossbeam_channel::Sender<FileCommand>,
}

impl<'a> MakeWriter<'a> for FileMakeWriter {
    type Writer = FileEventWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        let accepting = self.accepting.load(Ordering::Acquire);
        let generation = if accepting {
            self.generation.load(Ordering::Acquire)
        } else {
            0
        };
        FileEventWriter {
            events: &self.events,
            bytes: Vec::new(),
            accepted_at_start: accepting,
            accepting: &self.accepting,
            generation,
            current_generation: &self.generation,
            generation_drops: &self.generation_drops,
            dropped_events: &self.dropped_events,
            commands: &self.commands,
        }
    }
}

pub struct FileEventWriter<'a> {
    events: &'a crossbeam_channel::Sender<FileEvent>,
    bytes: Vec<u8>,
    accepted_at_start: bool,
    accepting: &'a AtomicBool,
    generation: u64,
    current_generation: &'a AtomicU64,
    generation_drops: &'a RwLock<Arc<GenerationDrops>>,
    dropped_events: &'a AtomicU64,
    commands: &'a crossbeam_channel::Sender<FileCommand>,
}

impl Write for FileEventWriter<'_> {
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

impl Drop for FileEventWriter<'_> {
    fn drop(&mut self) {
        if !self.accepted_at_start
            || !self.accepting.load(Ordering::Acquire)
            || self.bytes.is_empty()
            || self.current_generation.load(Ordering::Acquire) != self.generation
        {
            return;
        }
        match self.events.try_send(FileEvent {
            generation: self.generation,
            bytes: std::mem::take(&mut self.bytes),
        }) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Disconnected(_)) => {}
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                record_current_generation_full_queue_drop(
                    self.accepting,
                    self.current_generation,
                    self.generation,
                    self.generation_drops,
                    self.dropped_events,
                    self.commands,
                );
            }
        }
    }
}

fn record_current_generation_full_queue_drop(
    accepting: &AtomicBool,
    current_generation: &AtomicU64,
    writer_generation: u64,
    current_drops: &RwLock<Arc<GenerationDrops>>,
    dropped_events: &AtomicU64,
    commands: &crossbeam_channel::Sender<FileCommand>,
) {
    // Tracker lookup and cloning belong only to the already-Full path. Normal
    // accepted events neither wait for a generation replacement nor retain
    // loss-only command/tracker handles while they are being formatted.
    let generation_drops = Arc::clone(&read_generation_drops(current_drops));
    record_full_queue_drop(
        accepting,
        current_generation,
        writer_generation,
        &generation_drops,
        dropped_events,
        commands,
    );
}

fn record_full_queue_drop(
    accepting: &AtomicBool,
    current_generation: &AtomicU64,
    writer_generation: u64,
    generation_drops: &Arc<GenerationDrops>,
    dropped_events: &AtomicU64,
    commands: &crossbeam_channel::Sender<FileCommand>,
) {
    // The queue is already full before this lock is reached. Serialize this
    // rare loss path with generation close so the worker's final snapshot
    // either includes the loss or definitively rejects it. The worker never
    // holds this mutex during file I/O.
    let should_notify = {
        let mut state = lock_generation_drop_state(&generation_drops.state);
        if state.closed
            || generation_drops.generation != writer_generation
            || !accepting.load(Ordering::Acquire)
            || current_generation.load(Ordering::Acquire) != writer_generation
            || state.total == u64::MAX
        {
            return;
        }

        state.total += 1;
        dropped_events.fetch_add(1, Ordering::Relaxed);
        let should_notify = state.pending_target.is_none() && state.claimed_target.is_none();
        state.pending_target = Some(state.total);
        should_notify
    };

    if should_notify {
        let _ = commands.try_send(FileCommand::CheckGap(Arc::clone(generation_drops)));
    }
}

pub struct FileSinkGuard {
    commands: crossbeam_channel::Sender<FileCommand>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for FileSinkGuard {
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

    /// The label the tests' gap lines carry.
    const LABEL: &str = "Talker";

    fn spawn_with(open: OpenFileFn) -> anyhow::Result<FileSink> {
        super::spawn_with(LABEL, open)
    }

    fn spawn_with_capacity(open: OpenFileFn, cap: usize) -> anyhow::Result<FileSink> {
        super::spawn_with_capacity(LABEL, open, cap)
    }

    fn gap_marker_line(dropped: u64) -> String {
        super::gap_marker_line(LABEL, dropped)
    }

    fn config() -> LogFileConfig {
        LogFileConfig::new("unused-by-the-in-memory-writer", "talker.log")
    }

    // ── old-file deletion ─────────────────────────────────────────────────────

    /// A fresh, empty folder under the system temp directory.
    fn temp_folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wiredata_log_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file_last_written(path: &Path, when: SystemTime) {
        std::fs::write(path, b"x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn only_this_prefix_older_than_the_limit_is_deleted() {
        let dir = temp_folder("prune");
        let now = SystemTime::now();
        let old = now - Duration::from_secs(40 * 24 * 60 * 60);
        let recent = now - Duration::from_secs(2 * 24 * 60 * 60);
        file_last_written(&dir.join("listener.log.2026-08-01"), old);
        file_last_written(&dir.join("listener.log.2026-09-28"), recent);
        file_last_written(&dir.join("listener.logbook"), old); // not this prefix
        file_last_written(&dir.join("other.log.2026-08-01"), old);

        let deleted = prune_old_files(
            &dir,
            "listener.log",
            Duration::from_secs(30 * 24 * 60 * 60),
            now,
        );

        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(deleted, 1);
        assert_eq!(
            left,
            [
                "listener.log.2026-09-28",
                "listener.logbook",
                "other.log.2026-08-01"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_deletes_old_files_first_and_keeping_all_deletes_none() {
        let dir = temp_folder("open_prune");
        let old = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
        let stale = dir.join("app.log.2026-08-01");

        file_last_written(&stale, old);
        let keep_all = LogFileConfig::new(&dir, "app.log");
        drop(open_rolling_file(&keep_all).unwrap());
        assert!(stale.exists(), "max_age None keeps every file");

        let thirty_days = LogFileConfig {
            max_age: Some(Duration::from_secs(30 * 24 * 60 * 60)),
            ..keep_all
        };
        drop(open_rolling_file(&thirty_days).unwrap());
        assert!(!stale.exists(), "opening deletes files past the limit");
        let _ = std::fs::remove_dir_all(&dir);
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

    fn emit(writer: &FileMakeWriter, bytes: &[u8]) {
        let mut event = writer.make_writer();
        event.write_all(bytes).unwrap();
    }

    fn wait_for_text(written: &Mutex<Vec<u8>>, wanted: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
            if text.contains(wanted) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "file worker did not write {wanted:?}: {text}"
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
    fn an_ordinary_accepted_event_does_not_read_the_loss_tracker() {
        let (events, receiver) = crossbeam_channel::bounded(1);
        let generation = Arc::new(AtomicU64::new(1));
        let drops = Arc::new(GenerationDrops::new(1));
        let generation_drops = Arc::new(RwLock::new(Arc::clone(&drops)));
        let (commands, _command_receiver) = crossbeam_channel::unbounded();
        let writer = FileMakeWriter {
            events,
            accepting: Arc::new(AtomicBool::new(true)),
            generation,
            generation_drops: Arc::clone(&generation_drops),
            dropped_events: Arc::new(AtomicU64::new(0)),
            commands,
        };

        // A normal accepted event has no loss bookkeeping to perform. Holding
        // the tracker exclusively must therefore not delay formatting or queue
        // admission; only the already-Full branch may consult this tracker.
        let tracker_guard = write_generation_drops(&generation_drops);
        let (completed_tx, completed_rx) = crossbeam_channel::bounded(1);
        let producer = std::thread::spawn(move || {
            emit(&writer, b"ordinary event");
            completed_tx.send(()).unwrap();
        });
        let completed_without_tracker = completed_rx
            .recv_timeout(Duration::from_millis(250))
            .is_ok();
        drop(tracker_guard);
        producer.join().unwrap();

        assert!(
            completed_without_tracker,
            "an accepted event waited for the loss tracker"
        );
        assert_eq!(receiver.recv().unwrap().bytes, b"ordinary event");
    }

    #[test]
    fn a_full_file_queue_increments_the_exposed_drop_count() {
        let (events, _receiver) = crossbeam_channel::bounded(0);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let generation = Arc::new(AtomicU64::new(1));
        let drops = Arc::new(GenerationDrops::new(1));
        let generation_drops = Arc::new(RwLock::new(Arc::clone(&drops)));
        let (commands, command_receiver) = crossbeam_channel::unbounded();
        let writer = FileMakeWriter {
            events,
            accepting: Arc::new(AtomicBool::new(true)),
            generation,
            generation_drops,
            dropped_events: Arc::clone(&dropped_events),
            commands,
        };
        {
            let mut event = writer.make_writer();
            event.write_all(b"cannot be queued").unwrap();
        }
        assert_eq!(dropped_events.load(Ordering::Relaxed), 1);
        assert_eq!(drops.dropped(), 1);
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(FileCommand::CheckGap(notified)) if Arc::ptr_eq(&notified, &drops)
        ));
    }

    #[test]
    fn a_disconnected_file_queue_is_not_counted_as_overload_loss() {
        let (events, receiver) = crossbeam_channel::bounded(1);
        drop(receiver);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let generation = Arc::new(AtomicU64::new(1));
        let drops = Arc::new(GenerationDrops::new(1));
        let generation_drops = Arc::new(RwLock::new(Arc::clone(&drops)));
        let (commands, command_receiver) = crossbeam_channel::unbounded();
        let writer = FileMakeWriter {
            events,
            accepting: Arc::new(AtomicBool::new(true)),
            generation,
            generation_drops,
            dropped_events: Arc::clone(&dropped_events),
            commands,
        };

        emit(&writer, b"receiver has stopped");

        assert_eq!(dropped_events.load(Ordering::Relaxed), 0);
        assert_eq!(drops.dropped(), 0);
        assert!(command_receiver.try_recv().is_err());
    }

    #[test]
    fn a_stale_writer_cannot_charge_the_current_generation() {
        let (events, receiver) = crossbeam_channel::bounded(1);
        events
            .send(FileEvent {
                generation: 2,
                bytes: b"fills the queue".to_vec(),
            })
            .unwrap();
        let dropped_events = Arc::new(AtomicU64::new(0));
        let generation = Arc::new(AtomicU64::new(1));
        let old_drops = Arc::new(GenerationDrops::new(1));
        let generation_drops = Arc::new(RwLock::new(Arc::clone(&old_drops)));
        let (commands, command_receiver) = crossbeam_channel::unbounded();
        let writer = FileMakeWriter {
            events,
            accepting: Arc::new(AtomicBool::new(true)),
            generation: Arc::clone(&generation),
            generation_drops: Arc::clone(&generation_drops),
            dropped_events: Arc::clone(&dropped_events),
            commands,
        };
        let mut stale = writer.make_writer();
        stale.write_all(b"formatted for generation one").unwrap();

        let current_drops = Arc::new(GenerationDrops::new(2));
        *write_generation_drops(&generation_drops) = Arc::clone(&current_drops);
        generation.store(2, Ordering::Release);
        drop(stale);

        assert_eq!(dropped_events.load(Ordering::Relaxed), 0);
        assert_eq!(old_drops.dropped(), 0);
        assert_eq!(current_drops.dropped(), 0);
        assert!(command_receiver.try_recv().is_err());
        drop(receiver);
    }

    #[test]
    fn a_full_result_crossing_disable_or_reenable_is_not_counted() {
        let accepting = AtomicBool::new(true);
        let current_generation = AtomicU64::new(2);
        let old_drops = Arc::new(GenerationDrops::new(1));
        let dropped_events = AtomicU64::new(0);
        let (commands, command_receiver) = crossbeam_channel::unbounded();

        // Model the seam after try_send returned Full: the writer had already
        // passed Drop's first check, but a new generation is active now.
        record_full_queue_drop(
            &accepting,
            &current_generation,
            1,
            &old_drops,
            &dropped_events,
            &commands,
        );
        assert_eq!(dropped_events.load(Ordering::Relaxed), 0);
        assert_eq!(old_drops.dropped(), 0);
        assert!(command_receiver.try_recv().is_err());

        // The disable boundary is likewise normal shutdown, not overload.
        accepting.store(false, Ordering::Release);
        current_generation.store(1, Ordering::Release);
        record_full_queue_drop(
            &accepting,
            &current_generation,
            1,
            &old_drops,
            &dropped_events,
            &commands,
        );
        assert_eq!(dropped_events.load(Ordering::Relaxed), 0);
        assert_eq!(old_drops.dropped(), 0);
        assert!(command_receiver.try_recv().is_err());
    }

    #[test]
    fn closing_a_generation_linearizes_with_queue_full_accounting() {
        let accepting = AtomicBool::new(true);
        let current_generation = AtomicU64::new(1);
        let dropped_events = AtomicU64::new(0);
        let (commands, command_receiver) = crossbeam_channel::unbounded();

        let before_close = Arc::new(GenerationDrops::new(1));
        record_full_queue_drop(
            &accepting,
            &current_generation,
            1,
            &before_close,
            &dropped_events,
            &commands,
        );
        assert_eq!(before_close.close(), 1);
        assert_eq!(dropped_events.load(Ordering::Relaxed), 1);
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(FileCommand::CheckGap(notified)) if Arc::ptr_eq(&notified, &before_close)
        ));

        let after_close = Arc::new(GenerationDrops::new(1));
        assert_eq!(after_close.close(), 0);
        record_full_queue_drop(
            &accepting,
            &current_generation,
            1,
            &after_close,
            &dropped_events,
            &commands,
        );
        assert_eq!(after_close.dropped(), 0);
        assert_eq!(dropped_events.load(Ordering::Relaxed), 1);
        assert!(command_receiver.try_recv().is_err());
    }

    #[test]
    fn a_fixed_marker_target_keeps_later_loss_for_a_followup_batch() {
        let accepting = AtomicBool::new(true);
        let current_generation = AtomicU64::new(1);
        let drops = Arc::new(GenerationDrops::new(1));
        let dropped_events = AtomicU64::new(0);
        let (commands, command_receiver) = crossbeam_channel::unbounded();

        record_full_queue_drop(
            &accepting,
            &current_generation,
            1,
            &drops,
            &dropped_events,
            &commands,
        );
        let first_target = drops.claim_marker_target().unwrap();
        assert_eq!(first_target, 1);
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(FileCommand::CheckGap(notified)) if Arc::ptr_eq(&notified, &drops)
        ));

        // Model another loss while the worker is writing the first marker. It
        // must not move that marker's boundary or require a later normal event
        // to make its own follow-up batch visible.
        record_full_queue_drop(
            &accepting,
            &current_generation,
            1,
            &drops,
            &dropped_events,
            &commands,
        );
        assert_eq!(drops.dropped(), 2);
        assert_eq!(drops.claim_marker_target(), Some(first_target));
        assert_eq!(
            lock_generation_drop_state(&drops.state).pending_target,
            Some(2)
        );
        assert!(command_receiver.try_recv().is_err());

        assert!(drops.marker_written(first_target));
        assert_eq!(drops.claim_marker_target(), Some(2));
        assert!(!drops.marker_written(2));
        assert_eq!(drops.claim_marker_target(), None);
    }

    #[test]
    fn a_file_gap_marker_follows_the_accepted_records_that_preceded_it() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let worker_written = Arc::clone(&written);
        let sink = spawn_with_capacity(
            Box::new(move |_| {
                Ok(Box::new(GatedWriter {
                    bytes: Arc::clone(&worker_written),
                    started: started_tx.clone(),
                    releases: release_rx.clone(),
                }))
            }),
            1,
        )
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        emit(&sink.writer, b"block first record\n");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("file worker did not enter the gated write");
        emit(&sink.writer, b"accepted queued record\n");
        emit(&sink.writer, b"lost record\n");
        assert_eq!(sink.toggle.dropped_events(), 1);
        release_tx.send(()).unwrap();

        let marker = gap_marker_line(1);
        let text = wait_for_text(&written, &marker);
        assert_eq!(
            text,
            format!("block first record\naccepted queued record\n{marker}")
        );
    }

    #[test]
    fn loss_during_a_marker_waits_behind_its_record_and_writes_without_another_event() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (record_started_tx, record_started_rx) = crossbeam_channel::bounded(1);
        let (record_release_tx, record_release_rx) = crossbeam_channel::bounded(1);
        let (marker_started_tx, marker_started_rx) = crossbeam_channel::bounded(1);
        let (marker_release_tx, marker_release_rx) = crossbeam_channel::bounded(1);
        let worker_written = Arc::clone(&written);
        let sink = spawn_with_capacity(
            Box::new(move |_| {
                Ok(Box::new(MarkerRaceWriter {
                    bytes: Arc::clone(&worker_written),
                    record_started: record_started_tx.clone(),
                    record_release: record_release_rx.clone(),
                    marker_started: marker_started_tx.clone(),
                    marker_release: marker_release_rx.clone(),
                    first_marker: true,
                }))
            }),
            1,
        )
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        emit(&sink.writer, b"block initial record\n");
        record_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("file worker did not enter the gated record write");
        emit(&sink.writer, b"accepted before first loss\n");
        emit(&sink.writer, b"lost before first marker\n");
        record_release_tx.send(()).unwrap();
        marker_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("file worker did not enter the gated marker write");

        // The first marker is in progress. Fill the queue with a record, then
        // lose the next one. No later event is emitted to rescue the wakeup.
        emit(&sink.writer, b"accepted between markers\n");
        emit(&sink.writer, b"lost during first marker\n");
        assert_eq!(sink.toggle.dropped_events(), 2);
        marker_release_tx.send(()).unwrap();

        let marker = gap_marker_line(1);
        let expected = format!(
            "block initial record\naccepted before first loss\n{marker}accepted between markers\n{marker}"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
            if text == expected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "file worker did not preserve marker ordering: {text:?}"
            );
            std::thread::yield_now();
        }

        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });
    }

    #[test]
    fn disabling_writes_a_gap_marker_for_loss_at_the_queued_tail() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let worker_written = Arc::clone(&written);
        let sink = spawn_with_capacity(
            Box::new(move |_| {
                Ok(Box::new(GatedWriter {
                    bytes: Arc::clone(&worker_written),
                    started: started_tx.clone(),
                    releases: release_rx.clone(),
                }))
            }),
            1,
        )
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        emit(&sink.writer, b"block before disable\n");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("file worker did not enter the gated write");
        emit(&sink.writer, b"accepted tail\n");
        emit(&sink.writer, b"lost at tail\n");
        sink.toggle.disable().unwrap();
        release_tx.send(()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });

        let marker = gap_marker_line(1);
        let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert_eq!(
            text,
            format!("block before disable\naccepted tail\n{marker}")
        );
    }

    #[test]
    fn shutdown_writes_a_gap_marker_for_loss_at_the_queued_tail() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let worker_written = Arc::clone(&written);
        let sink = spawn_with_capacity(
            Box::new(move |_| {
                Ok(Box::new(GatedWriter {
                    bytes: Arc::clone(&worker_written),
                    started: started_tx.clone(),
                    releases: release_rx.clone(),
                }))
            }),
            1,
        )
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        emit(&sink.writer, b"block before shutdown\n");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("file worker did not enter the gated write");
        emit(&sink.writer, b"accepted shutdown tail\n");
        emit(&sink.writer, b"lost at shutdown\n");
        let shutdown = std::thread::spawn(move || drop(sink.guard));
        release_tx.send(()).unwrap();
        shutdown.join().unwrap();

        let marker = gap_marker_line(1);
        let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert_eq!(
            text,
            format!("block before shutdown\naccepted shutdown tail\n{marker}")
        );
    }

    #[test]
    fn a_gap_marker_write_failure_disables_the_file_sink() {
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let sink = spawn_with_capacity(
            Box::new(move |_| {
                Ok(Box::new(MarkerFailingGatedWriter {
                    started: started_tx.clone(),
                    releases: release_rx.clone(),
                }))
            }),
            1,
        )
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        emit(&sink.writer, b"block before marker failure\n");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("file worker did not enter the gated write");
        emit(&sink.writer, b"accepted before marker failure\n");
        emit(&sink.writer, b"lost before marker failure\n");
        release_tx.send(()).unwrap();

        let state = wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Failed(_))
        });
        assert!(
            matches!(state, FileLogState::Failed(message) if message.contains("marker denied"))
        );
        assert!(!sink.writer.accepting.load(Ordering::Acquire));
    }

    #[test]
    fn separate_loss_batches_write_delta_markers_without_recounting() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let worker_written = Arc::clone(&written);
        let sink = spawn_with_capacity(
            Box::new(move |_| {
                Ok(Box::new(GatedWriter {
                    bytes: Arc::clone(&worker_written),
                    started: started_tx.clone(),
                    releases: release_rx.clone(),
                }))
            }),
            1,
        )
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        emit(&sink.writer, b"block batch one\n");
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        emit(&sink.writer, b"accepted batch one\n");
        emit(&sink.writer, b"lost batch one\n");
        release_tx.send(()).unwrap();
        wait_for_text(&written, &gap_marker_line(1));

        emit(&sink.writer, b"block batch two\n");
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        emit(&sink.writer, b"accepted batch two\n");
        emit(&sink.writer, b"lost batch two a\n");
        emit(&sink.writer, b"lost batch two b\n");
        release_tx.send(()).unwrap();
        wait_for_text(&written, &gap_marker_line(2));
        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });

        assert_eq!(sink.toggle.dropped_events(), 3);
        let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert_eq!(text.matches(&gap_marker_line(1)).count(), 1, "{text}");
        assert_eq!(text.matches(&gap_marker_line(2)).count(), 1, "{text}");
    }

    #[test]
    fn reenable_starts_a_fresh_gap_baseline() {
        let first_file = Arc::new(Mutex::new(Vec::new()));
        let second_file = Arc::new(Mutex::new(Vec::new()));
        let opens = Arc::new(AtomicU64::new(0));
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let worker_first = Arc::clone(&first_file);
        let worker_second = Arc::clone(&second_file);
        let worker_opens = Arc::clone(&opens);
        let sink = spawn_with_capacity(
            Box::new(move |_| {
                if worker_opens.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(Box::new(GatedWriter {
                        bytes: Arc::clone(&worker_first),
                        started: started_tx.clone(),
                        releases: release_rx.clone(),
                    }))
                } else {
                    Ok(Box::new(SharedWriter(Arc::clone(&worker_second))))
                }
            }),
            1,
        )
        .unwrap();
        sink.toggle.enable(config()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. })
        });

        emit(&sink.writer, b"block old file\n");
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        emit(&sink.writer, b"accepted old tail\n");
        emit(&sink.writer, b"lost from old file\n");
        sink.toggle.disable().unwrap();
        sink.toggle.enable(config()).unwrap();
        release_tx.send(()).unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Enabled { .. }) && opens.load(Ordering::SeqCst) == 2
        });
        emit(&sink.writer, b"fresh file record\n");
        sink.toggle.disable().unwrap();
        wait_for_state(&sink.toggle, |state| {
            matches!(state, FileLogState::Disabled)
        });

        let first = String::from_utf8_lossy(&first_file.lock().unwrap()).into_owned();
        let second = String::from_utf8_lossy(&second_file.lock().unwrap()).into_owned();
        assert!(first.contains(&gap_marker_line(1)), "{first}");
        assert_eq!(second, "fresh file record\n");
        assert_eq!(sink.toggle.dropped_events(), 1);
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

    struct GatedWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        started: crossbeam_channel::Sender<()>,
        releases: crossbeam_channel::Receiver<()>,
    }

    impl Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.starts_with(b"block ") {
                let _ = self.started.send(());
                let _ = self.releases.recv();
            }
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct MarkerRaceWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        record_started: crossbeam_channel::Sender<()>,
        record_release: crossbeam_channel::Receiver<()>,
        marker_started: crossbeam_channel::Sender<()>,
        marker_release: crossbeam_channel::Receiver<()>,
        first_marker: bool,
    }

    impl Write for MarkerRaceWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.starts_with(b"block ") {
                let _ = self.record_started.send(());
                let _ = self.record_release.recv();
            }
            if self.first_marker && buf.starts_with(b"--- Talker file log gap:") {
                self.first_marker = false;
                let _ = self.marker_started.send(());
                let _ = self.marker_release.recv();
            }
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct MarkerFailingGatedWriter {
        started: crossbeam_channel::Sender<()>,
        releases: crossbeam_channel::Receiver<()>,
    }

    impl Write for MarkerFailingGatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.starts_with(b"block ") {
                let _ = self.started.send(());
                let _ = self.releases.recv();
            }
            if buf.starts_with(b"--- Talker file log gap:") {
                return Err(io::Error::other("marker denied"));
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
