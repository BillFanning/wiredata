//! Recording continuity (listener ADR-043, spec §56.1, §59).
//!
//! One controller task runs each recording tap, Raw or Display. It owns the file
//! lifecycle — opening, rotating and finalizing segments — so the pipeline only
//! enqueues and never waits on a file (§100).
//!
//! A fault does not end the recording. The controller ends the current segment,
//! opens a **gap**, waits, and continues in a new segment:
//!
//! ```text
//! Off → Opening → Recording → Gap → Opening → …
//! any state → Stopping → Off
//! ```
//!
//! During a gap, bytes are deliberately omitted, never buffered: the producer
//! stops enqueueing and drops, and anything already queued is discarded. One gap
//! stays open until a segment actually starts. Its record — the first byte not
//! recorded, the first byte recorded again, both times, and the reason — reaches
//! the pipeline as a [`RecorderReport`], which turns it into a diagnostic and an
//! event-log line.
//!
//! Where segments come from is a [`SegmentSource`]: the file implementation
//! (`segments`) allocates numbered files under the destination lock and refuses
//! to recreate a vanished folder; tests supply scripted ones.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{RecorderWriter, RecordingStopReason};
use crate::core::{GapReason, RecordError, RecordingState};
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

/// Where an item sits in the Channel's received stream: the stream offset of
/// its first byte (§25) and its arrival wall-clock time. Gap records carry one
/// at each end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamPos {
    pub offset: u64,
    pub at: SystemTime,
}

/// What the controller needs to know about a queued item.
pub trait RecordItem: Send + Sync + 'static {
    /// Bytes this item adds to its file — for the size cap and queue budget.
    fn byte_len(&self) -> usize;
}

impl RecordItem for Arc<ReceivedData> {
    fn byte_len(&self) -> usize {
        self.payload.bytes().len()
    }
}

impl RecordItem for RenderedOutput {
    fn byte_len(&self) -> usize {
        self.text.len()
    }
}

/// Bookkeeping each queued item costs beyond its bytes, so a stream of tiny
/// chunks is bounded as honestly as a stream of large ones.
pub(crate) const PER_ITEM_OVERHEAD: usize = 64;

/// The default queue budget per recording, in bytes (ADR-043, §79).
pub const DEFAULT_QUEUE_BUDGET: usize = 8 * 1024 * 1024;

const OVERFLOW_DETAIL: &str = "the recording queue filled faster than the disk drained it";

/// Why a segment is being opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenKind {
    /// The first file of an enable; the overwrite policy applies (§55).
    First,
    /// A new rotation period began (§59).
    Rotation,
    /// The current file reached its size cap (§59).
    SizeCap,
    /// Recording resumes after a gap (§56.1).
    Recovery,
}

/// A request for the next segment.
#[derive(Clone, Debug)]
pub struct OpenRequest {
    pub kind: OpenKind,
    /// The rotation period the segment belongs to, when the recording rotates.
    pub period: Option<String>,
}

/// Why a segment could not be opened.
#[derive(Debug)]
pub enum OpenError {
    /// Retrying cannot fix it: the user must act (§55). Only meaningful for the
    /// first open of an enable.
    Terminal(RecordError),
    /// It may work later: the recording waits in a gap and tries again.
    Retry {
        reason: GapReason,
        error: RecordError,
    },
}

/// Where a recording's segments come from.
#[async_trait::async_trait]
pub trait SegmentSource<I: RecordItem>: Send + 'static {
    /// The rotation period of an item arriving at `at`; `None` when the
    /// recording does not rotate. Periods compare as strings in time order.
    fn period_of(&self, _at: SystemTime) -> Option<String> {
        None
    }

    /// The soft size cap per segment, in bytes (§59); `None` for no cap.
    fn size_cap(&self) -> Option<u64> {
        None
    }

    /// Open the next segment.
    async fn open(&mut self, request: OpenRequest)
        -> Result<Box<dyn RecorderWriter<I>>, OpenError>;
}

/// What a recording tells the pipeline, which turns each into a diagnostic and
/// an event-log line (§56.1).
#[derive(Clone, Debug, PartialEq)]
pub enum RecorderReport {
    /// A segment opened; `kind` says why.
    SegmentOpened {
        path: Option<PathBuf>,
        kind: OpenKind,
    },
    /// A gap began: bytes are being omitted.
    GapOpened { reason: GapReason, detail: String },
    /// A gap ended: recording resumed with the byte at `end`. `start` is the
    /// first byte not recorded, when any was offered during the gap.
    GapClosed {
        reason: GapReason,
        start: Option<StreamPos>,
        end: StreamPos,
    },
    /// The recording ended while in a gap.
    GapEndedByStop {
        reason: GapReason,
        start: Option<StreamPos>,
    },
    /// Faults are repeating — `faults` of them within `window` — so retries
    /// slow to `retry`, and the recording stays marked unstable until it is
    /// stopped.
    Unstable {
        faults: usize,
        window: Duration,
        retry: Duration,
    },
    /// The first open failed in a way retrying cannot fix (§55).
    CouldNotBegin { detail: String },
}

/// How the controller paces retries and flushes. Tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    /// The first wait after a fault; it doubles up to `max_retry`.
    pub first_retry: Duration,
    pub max_retry: Duration,
    /// Faults within this window count toward instability.
    pub unstable_window: Duration,
    /// This many faults within the window mark the recording unstable.
    pub unstable_after: usize,
    /// The wait between retries once unstable.
    pub unstable_retry: Duration,
    /// How often buffered output is flushed to the OS (§56).
    pub flush: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            first_retry: Duration::from_secs(1),
            max_retry: Duration::from_secs(30),
            unstable_window: Duration::from_secs(10 * 60),
            unstable_after: 3,
            unstable_retry: Duration::from_secs(5 * 60),
            flush: Duration::from_secs(1),
        }
    }
}

/// How a recording ended.
#[derive(Debug, Default)]
pub struct Finalized {
    /// Set when the stop was not clean — accepted data truncated, or the
    /// finalize failed — so it never reads as clean (§56.1).
    pub fault: Option<String>,
    /// Reports not yet taken, including any the stop itself made.
    pub reports: Vec<RecorderReport>,
}

/// Keep at most this many unread reports; older ones are dropped first. The
/// pipeline drains them every loop pass, so this bounds a wedged reader only.
const MAX_PENDING_REPORTS: usize = 256;

struct Queued<I> {
    item: I,
    pos: StreamPos,
    cost: usize,
}

enum Command {
    /// The producer hit the byte budget; end the segment once the queue drains.
    Overflowed,
    /// Free space fell below, or rose back above, the disk guard (§56.2).
    LowDisk(bool),
}

/// State shared by the producer handle and the controller task.
struct Shared {
    /// Whether the producer may enqueue. False during a gap.
    accepting: AtomicBool,
    /// The producer hit the budget; the gap starts once the queue drains.
    overflowed: AtomicBool,
    queued_bytes: AtomicUsize,
    /// The first byte not recorded in the current gap.
    gap_start: Mutex<Option<StreamPos>>,
    state: Mutex<RecordingState>,
    unstable: AtomicBool,
    reports: Mutex<VecDeque<RecorderReport>>,
    /// The file being written and its length, for status (§56.2).
    file: Mutex<Option<PathBuf>>,
    file_len: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn state(&self) -> RecordingState {
        *lock(&self.state)
    }

    fn set_state(&self, state: RecordingState) {
        *lock(&self.state) = state;
    }

    /// Note the first byte not recorded in this gap; later ones change nothing.
    fn note_gap_start(&self, pos: StreamPos) {
        lock(&self.gap_start).get_or_insert(pos);
    }

    fn report(&self, report: RecorderReport) {
        let mut reports = lock(&self.reports);
        if reports.len() == MAX_PENDING_REPORTS {
            reports.pop_front();
        }
        reports.push_back(report);
    }
}

/// Producer handle for a running recording. Enqueueing never blocks: a full
/// budget starts a gap rather than stalling reception (§56.1, §100).
pub struct Recording<I: RecordItem> {
    items: mpsc::UnboundedSender<Queued<I>>,
    commands: mpsc::UnboundedSender<Command>,
    terminate: Option<oneshot::Sender<RecordingStopReason>>,
    shared: Arc<Shared>,
    task: JoinHandle<Option<String>>,
    budget: usize,
    peak_bytes: usize,
}

impl<I: RecordItem> Recording<I> {
    pub fn state(&self) -> RecordingState {
        self.shared.state()
    }

    /// The file being written; `None` while opening or in a gap (§56.2).
    pub fn current_file(&self) -> Option<PathBuf> {
        lock(&self.shared.file).clone()
    }

    /// Bytes in the file being written (§56.2).
    pub fn bytes_written(&self) -> u64 {
        self.shared.file_len.load(Ordering::Relaxed)
    }

    /// Whether faults have repeated enough to slow retries (ADR-043). Stays
    /// set until the recording is stopped.
    pub fn is_unstable(&self) -> bool {
        self.shared.unstable.load(Ordering::Acquire)
    }

    /// Take the reports the controller has made since the last call.
    pub fn take_reports(&self) -> Vec<RecorderReport> {
        lock(&self.shared.reports).drain(..).collect()
    }

    /// Queued bytes now, the most ever queued, and the budget — for
    /// backpressure diagnosis (§99).
    pub fn queue_depth(&self) -> (usize, usize, usize) {
        let current = self.shared.queued_bytes.load(Ordering::Relaxed);
        (current, self.peak_bytes.max(current), self.budget)
    }

    /// Offer one item. Never blocks: in a gap the item is dropped, and an item
    /// that would exceed the byte budget starts one (§56.1).
    pub fn try_record(&mut self, item: I, pos: StreamPos) {
        if !self.shared.state().is_on() {
            return;
        }
        if !self.shared.accepting.load(Ordering::Acquire) {
            self.shared.note_gap_start(pos);
            return;
        }
        let cost = item.byte_len() + PER_ITEM_OVERHEAD;
        let queued = self.shared.queued_bytes.load(Ordering::Acquire);
        if queued + cost > self.budget {
            self.shared.accepting.store(false, Ordering::Release);
            self.shared.overflowed.store(true, Ordering::Release);
            self.shared.note_gap_start(pos);
            // Bytes are lost from here, so the gap is reported now: the
            // controller may be stuck in a write that never returns.
            self.shared
                .set_state(RecordingState::Gap(GapReason::QueueOverflow));
            self.shared.report(RecorderReport::GapOpened {
                reason: GapReason::QueueOverflow,
                detail: OVERFLOW_DETAIL.to_owned(),
            });
            // Wake the controller even if no more items arrive to do it.
            let _ = self.commands.send(Command::Overflowed);
            return;
        }
        self.shared.queued_bytes.fetch_add(cost, Ordering::AcqRel);
        self.peak_bytes = self.peak_bytes.max(queued + cost);
        let _ = self.items.send(Queued { item, pos, cost });
    }

    /// Tell the controller free space is low, or recovered (§56.2). While low,
    /// the recording is in a gap and does not retry.
    pub fn set_low_disk(&self, low: bool) {
        let _ = self.commands.send(Command::LowDisk(low));
    }

    /// Stop recording (user disable or Channel stop): the controller writes the
    /// accepted backlog and finalizes (§56, §110).
    pub async fn finalize(mut self, reason: RecordingStopReason) -> Finalized {
        if let Some(terminate) = self.terminate.take() {
            let _ = terminate.send(reason);
        }
        drop(self.items);
        let fault = self
            .task
            .await
            .unwrap_or_else(|join| Some(format!("the recording task failed: {join}")));
        self.shared.set_state(RecordingState::Disabled);
        Finalized {
            fault,
            reports: lock(&self.shared.reports).drain(..).collect(),
        }
    }
}

/// Start a recording drawing its segments from `source`.
pub fn start_recording<I, S>(source: S, budget: usize, timings: Timings) -> Recording<I>
where
    I: RecordItem,
    S: SegmentSource<I>,
{
    let (items_tx, items_rx) = mpsc::unbounded_channel();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let (terminate_tx, terminate_rx) = oneshot::channel();
    let shared = Arc::new(Shared {
        // Items queue while the first segment opens (§56.1).
        accepting: AtomicBool::new(true),
        overflowed: AtomicBool::new(false),
        queued_bytes: AtomicUsize::new(0),
        gap_start: Mutex::new(None),
        state: Mutex::new(RecordingState::Enabled),
        unstable: AtomicBool::new(false),
        reports: Mutex::new(VecDeque::new()),
        file: Mutex::new(None),
        file_len: AtomicU64::new(0),
    });
    let controller = Controller {
        source,
        shared: Arc::clone(&shared),
        timings,
        segment: None,
        period: None,
        gap: None,
        closing_gap: false,
        ever_opened: false,
        low_disk: false,
        backoff: timings.first_retry,
        retry_at: Some(Instant::now()),
        faults: VecDeque::new(),
        unflushed_since: None,
        last_fault: None,
    };
    let task = tokio::spawn(controller.run(items_rx, commands_rx, terminate_rx));
    Recording {
        items: items_tx,
        commands: commands_tx,
        terminate: Some(terminate_tx),
        shared,
        task,
        budget,
        peak_bytes: 0,
    }
}

/// The controller task's state (see the module docs).
struct Controller<I: RecordItem, S: SegmentSource<I>> {
    source: S,
    shared: Arc<Shared>,
    timings: Timings,
    segment: Option<Box<dyn RecorderWriter<I>>>,
    /// The rotation period of the current or last segment. Rotation only moves
    /// forward from it (§59).
    period: Option<String>,
    /// The open gap, if any.
    gap: Option<GapReason>,
    /// A segment opened after a gap; the next item written closes the gap.
    closing_gap: bool,
    ever_opened: bool,
    low_disk: bool,
    backoff: Duration,
    /// When the next open is due while there is no segment.
    retry_at: Option<Instant>,
    /// Recent fault times, for instability.
    faults: VecDeque<Instant>,
    /// The first item written since the last successful flush: bytes from here
    /// on may not be on disk if a write or flush then fails.
    unflushed_since: Option<StreamPos>,
    /// The detail of the last fault that opened a gap, so a stop can tell
    /// whether writing its backlog lost anything.
    last_fault: Option<String>,
}

/// How a controller run ended.
enum Flow {
    Continue,
    Stop(Option<String>),
}

impl<I: RecordItem, S: SegmentSource<I>> Controller<I, S> {
    async fn run(
        mut self,
        mut items: mpsc::UnboundedReceiver<Queued<I>>,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut terminate: oneshot::Receiver<RecordingStopReason>,
    ) -> Option<String> {
        let mut flush_tick =
            tokio::time::interval_at(Instant::now() + self.timings.flush, self.timings.flush);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if self.segment.is_none() && !self.low_disk {
                if let Some(at) = self.retry_at {
                    if Instant::now() >= at {
                        if let Flow::Stop(fault) = self.open_next(&mut items).await {
                            return fault;
                        }
                    }
                }
            }
            let retry_at = self
                .retry_at
                .filter(|_| self.segment.is_none() && !self.low_disk);
            tokio::select! {
                biased;
                reason = &mut terminate => {
                    let reason = reason.unwrap_or(RecordingStopReason::ChannelStopped);
                    return self.stop(reason, &mut items).await;
                }
                Some(command) = commands.recv() => self.on_command(command, &mut items).await,
                maybe = items.recv() => match maybe {
                    Some(queued) => self.on_item(queued, &mut items).await,
                    None => return self.stop(RecordingStopReason::ChannelStopped, &mut items).await,
                },
                _ = flush_tick.tick(), if self.segment.is_some() => self.flush(&mut items).await,
                _ = tokio::time::sleep_until(retry_at.unwrap_or_else(Instant::now)), if retry_at.is_some() => {}
            }
        }
    }

    /// Open the segment the current state calls for.
    async fn open_next(&mut self, items: &mut mpsc::UnboundedReceiver<Queued<I>>) -> Flow {
        let kind = if !self.ever_opened && self.gap.is_none() {
            OpenKind::First
        } else {
            OpenKind::Recovery
        };
        let period = self.source.period_of(SystemTime::now());
        self.open(kind, period, items).await
    }

    async fn open(
        &mut self,
        kind: OpenKind,
        period: Option<String>,
        items: &mut mpsc::UnboundedReceiver<Queued<I>>,
    ) -> Flow {
        self.retry_at = None;
        let request = OpenRequest {
            kind,
            period: period.clone(),
        };
        match self.source.open(request).await {
            Ok(segment) => {
                let path = segment.path().map(PathBuf::from);
                *lock(&self.shared.file) = path.clone();
                self.shared
                    .file_len
                    .store(segment.bytes_written(), Ordering::Relaxed);
                self.segment = Some(segment);
                self.period = period.or(self.period.take());
                self.ever_opened = true;
                self.backoff = self.timings.first_retry;
                self.shared
                    .report(RecorderReport::SegmentOpened { path, kind });
                if self.gap.is_some() {
                    self.closing_gap = true;
                }
                // After an overflow the producer stays stopped: the queued
                // items are written, then the segment ends where the loss
                // began (`end_after_overflow`).
                if !self.shared.overflowed.load(Ordering::Acquire) {
                    self.shared.set_state(RecordingState::Enabled);
                    self.shared.accepting.store(true, Ordering::Release);
                }
                Flow::Continue
            }
            Err(OpenError::Terminal(error)) if !self.ever_opened => {
                self.shared.accepting.store(false, Ordering::Release);
                self.shared.set_state(RecordingState::Faulted);
                self.shared.report(RecorderReport::CouldNotBegin {
                    detail: error.to_string(),
                });
                while items.try_recv().is_ok() {}
                Flow::Stop(None)
            }
            Err(OpenError::Terminal(error)) => {
                self.begin_gap(GapReason::OpenFailed, error.to_string(), None, items);
                Flow::Continue
            }
            Err(OpenError::Retry { reason, error }) => {
                self.begin_gap(reason, error.to_string(), None, items);
                Flow::Continue
            }
        }
    }

    async fn on_item(&mut self, queued: Queued<I>, items: &mut mpsc::UnboundedReceiver<Queued<I>>) {
        self.write_item(queued, items).await;
        self.end_after_overflow(items).await;
    }

    /// Write one item, first starting the segment it belongs in: a new period
    /// (§59) or, at the size cap, the next numbered segment. A fault opens a
    /// gap and the item is omitted.
    async fn write_item(
        &mut self,
        queued: Queued<I>,
        items: &mut mpsc::UnboundedReceiver<Queued<I>>,
    ) {
        self.shared
            .queued_bytes
            .fetch_sub(queued.cost, Ordering::AcqRel);
        if self.segment.is_none() {
            // In a gap (or its open failed): this byte is omitted.
            self.shared.note_gap_start(queued.pos);
            return;
        }
        // Rotation only moves forward (§59): an earlier period, from a clock
        // stepped back, keeps writing the current file.
        if let Some(period) = self.source.period_of(queued.pos.at) {
            if self
                .period
                .as_ref()
                .is_some_and(|current| period > *current)
            {
                self.end_segment().await;
                if let Flow::Stop(_) = self.open(OpenKind::Rotation, Some(period), items).await {
                    return;
                }
            }
        }
        if let (Some(cap), Some(segment)) = (self.source.size_cap(), &self.segment) {
            let len = segment.bytes_written();
            if len > 0 && len + queued.item.byte_len() as u64 > cap {
                let period = self.period.clone();
                self.end_segment().await;
                if let Flow::Stop(_) = self.open(OpenKind::SizeCap, period, items).await {
                    return;
                }
            }
        }
        let Some(segment) = self.segment.as_mut() else {
            // The rotation or size-cap open failed: this item begins the gap.
            self.shared.note_gap_start(queued.pos);
            return;
        };
        match segment.write(&queued.item).await {
            Ok(()) => {
                self.shared
                    .file_len
                    .store(segment.bytes_written(), Ordering::Relaxed);
                self.unflushed_since.get_or_insert(queued.pos);
                if self.closing_gap {
                    self.closing_gap = false;
                    if let Some(reason) = self.gap.take() {
                        let start = lock(&self.shared.gap_start).take();
                        self.shared.report(RecorderReport::GapClosed {
                            reason,
                            start,
                            end: queued.pos,
                        });
                    }
                }
            }
            Err(error) => {
                let start = self.unflushed_since.take().unwrap_or(queued.pos);
                self.end_segment().await;
                self.begin_gap(
                    GapReason::WriteFailed,
                    error.to_string(),
                    Some(start),
                    items,
                );
            }
        }
    }

    /// After an overflow, the queued items precede the lost ones and are
    /// written; once they are, the segment ends and the gap begins.
    async fn end_after_overflow(&mut self, items: &mut mpsc::UnboundedReceiver<Queued<I>>) {
        if self.shared.overflowed.load(Ordering::Acquire) && items.is_empty() {
            self.end_segment().await;
            // The producer reported this gap when the loss began.
            self.gap.get_or_insert(GapReason::QueueOverflow);
            self.begin_gap(
                GapReason::QueueOverflow,
                OVERFLOW_DETAIL.to_owned(),
                None,
                items,
            );
        }
    }

    async fn on_command(
        &mut self,
        command: Command,
        items: &mut mpsc::UnboundedReceiver<Queued<I>>,
    ) {
        match command {
            Command::Overflowed => self.end_after_overflow(items).await,
            Command::LowDisk(true) if !self.low_disk => {
                self.low_disk = true;
                if self.segment.is_some() {
                    self.end_segment().await;
                }
                // Low disk is a condition, not a fault: it does not count
                // toward instability, and it waits for space, not a timer.
                self.shared.accepting.store(false, Ordering::Release);
                self.retry_at = None;
                self.enter_gap(
                    GapReason::LowDisk,
                    "free disk space is below the guard's threshold".to_owned(),
                    items,
                );
            }
            Command::LowDisk(false) if self.low_disk => {
                self.low_disk = false;
                self.retry_at = Some(Instant::now());
            }
            Command::LowDisk(_) => {}
        }
    }

    async fn flush(&mut self, items: &mut mpsc::UnboundedReceiver<Queued<I>>) {
        let Some(segment) = self.segment.as_mut() else {
            return;
        };
        match segment.flush().await {
            Ok(()) => self.unflushed_since = None,
            Err(error) => {
                let start = self.unflushed_since.take();
                self.end_segment().await;
                self.begin_gap(GapReason::WriteFailed, error.to_string(), start, items);
            }
        }
    }

    /// Finalize the current segment as a clean boundary. A failure here is
    /// ignored beyond the gap that usually follows: the segment's bytes are
    /// already accounted for by the gap's start.
    async fn end_segment(&mut self) {
        if let Some(mut segment) = self.segment.take() {
            let _ = segment.finalize(RecordingStopReason::ChannelStopped).await;
        }
        self.unflushed_since = None;
        *lock(&self.shared.file) = None;
        self.shared.file_len.store(0, Ordering::Relaxed);
    }

    /// A fault: open a gap, count the fault, and schedule the retry.
    fn begin_gap(
        &mut self,
        reason: GapReason,
        detail: String,
        start: Option<StreamPos>,
        items: &mut mpsc::UnboundedReceiver<Queued<I>>,
    ) {
        if let Some(start) = start {
            self.shared.note_gap_start(start);
        }
        self.shared.accepting.store(false, Ordering::Release);
        self.last_fault = Some(detail.clone());
        self.enter_gap(reason, detail, items);
        let now = Instant::now();
        self.faults.push_back(now);
        while self
            .faults
            .front()
            .is_some_and(|at| now.duration_since(*at) > self.timings.unstable_window)
        {
            self.faults.pop_front();
        }
        let unstable = self.faults.len() >= self.timings.unstable_after;
        if unstable && !self.shared.unstable.swap(true, Ordering::AcqRel) {
            self.shared.report(RecorderReport::Unstable {
                faults: self.faults.len(),
                window: self.timings.unstable_window,
                retry: self.timings.unstable_retry,
            });
        }
        let delay = if unstable {
            self.timings.unstable_retry
        } else {
            let delay = self.backoff;
            self.backoff = (self.backoff * 2).min(self.timings.max_retry);
            delay
        };
        self.retry_at = Some(now + delay);
    }

    /// Mark the recording as in a gap and discard what is queued.
    fn enter_gap(
        &mut self,
        reason: GapReason,
        detail: String,
        items: &mut mpsc::UnboundedReceiver<Queued<I>>,
    ) {
        self.shared.overflowed.store(false, Ordering::Release);
        while let Ok(queued) = items.try_recv() {
            self.shared
                .queued_bytes
                .fetch_sub(queued.cost, Ordering::AcqRel);
            self.shared.note_gap_start(queued.pos);
        }
        self.closing_gap = false;
        // One gap stays open until a segment starts: a second fault during it
        // keeps the first reason and start.
        if self.gap.is_none() {
            self.gap = Some(reason);
            self.shared
                .report(RecorderReport::GapOpened { reason, detail });
        }
        self.shared.set_state(RecordingState::Gap(reason));
    }

    /// Stop: write the accepted backlog on a graceful stop — under the same
    /// rotation and size-cap rules as while running — then finalize. Returns
    /// the fault when the stop was not clean.
    async fn stop(
        mut self,
        reason: RecordingStopReason,
        items: &mut mpsc::UnboundedReceiver<Queued<I>>,
    ) -> Option<String> {
        self.last_fault = None;
        let graceful = !matches!(reason, RecordingStopReason::Faulted(_));
        while let Ok(queued) = items.try_recv() {
            if graceful {
                self.write_item(queued, items).await;
            } else {
                self.shared
                    .queued_bytes
                    .fetch_sub(queued.cost, Ordering::AcqRel);
            }
        }
        let mut fault = self.last_fault.take();
        if let Some(mut segment) = self.segment.take() {
            if let Err(error) = segment.finalize(reason).await {
                fault.get_or_insert(format!("finalize failed: {error}"));
            }
        }
        // A gap still open — an overflow not yet ended, a reopen that wrote
        // nothing, or a fault while writing the backlog — ends with the
        // recording.
        if self.gap.is_some() || self.shared.overflowed.load(Ordering::Acquire) {
            let reason = self.gap.take().unwrap_or(GapReason::QueueOverflow);
            let start = lock(&self.shared.gap_start).take();
            self.shared
                .report(RecorderReport::GapEndedByStop { reason, start });
        }
        fault
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime};
    use crate::transport::ReceivedPayload;
    use std::sync::atomic::AtomicU32;

    type Item = Arc<ReceivedData>;

    fn chunk(bytes: &[u8]) -> Item {
        Arc::new(ReceivedData {
            channel_id: ChannelId::new(),
            payload: ReceivedPayload::Bytes(bytes.to_vec()),
            received_at: ChunkTime::now(),
        })
    }

    fn pos(offset: u64) -> StreamPos {
        StreamPos {
            offset,
            at: SystemTime::now(),
        }
    }

    fn fast() -> Timings {
        Timings {
            first_retry: Duration::from_millis(10),
            max_retry: Duration::from_millis(40),
            unstable_window: Duration::from_secs(60),
            unstable_after: 3,
            unstable_retry: Duration::from_secs(3600),
            flush: Duration::from_secs(3600),
        }
    }

    /// What one scripted segment does, and what it received.
    #[derive(Default)]
    struct SegmentLog {
        written: Mutex<Vec<Vec<u8>>>,
    }

    struct ScriptedSegment {
        path: PathBuf,
        log: Arc<SegmentLog>,
        len: u64,
        fail_write_at: Option<usize>,
        writes: usize,
        write_delay: Duration,
    }

    #[async_trait::async_trait]
    impl RecorderWriter<Item> for ScriptedSegment {
        async fn write(&mut self, item: &Item) -> Result<(), RecordError> {
            tokio::time::sleep(self.write_delay).await;
            self.writes += 1;
            if self.fail_write_at == Some(self.writes) {
                return Err(RecordError::Io(std::io::Error::other("device removed")));
            }
            let bytes = item.payload.bytes().to_vec();
            self.len += bytes.len() as u64;
            lock(&self.log.written).push(bytes);
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
        fn bytes_written(&self) -> u64 {
            self.len
        }
        fn path(&self) -> Option<&std::path::Path> {
            Some(&self.path)
        }
    }

    /// Opens scripted segments. `opens` counts attempts; `plan` decides each.
    struct ScriptedSource {
        segments: Arc<Mutex<Vec<Arc<SegmentLog>>>>,
        opens: Arc<AtomicU32>,
        plan: Box<dyn Fn(u32) -> Result<Option<usize>, OpenError> + Send>,
        open_delay: Duration,
        write_delay: Duration,
        cap: Option<u64>,
    }

    impl ScriptedSource {
        fn new(plan: impl Fn(u32) -> Result<Option<usize>, OpenError> + Send + 'static) -> Self {
            Self {
                segments: Arc::default(),
                opens: Arc::default(),
                plan: Box::new(plan),
                open_delay: Duration::ZERO,
                write_delay: Duration::ZERO,
                cap: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl SegmentSource<Item> for ScriptedSource {
        fn size_cap(&self) -> Option<u64> {
            self.cap
        }
        async fn open(
            &mut self,
            _request: OpenRequest,
        ) -> Result<Box<dyn RecorderWriter<Item>>, OpenError> {
            tokio::time::sleep(self.open_delay).await;
            let attempt = self.opens.fetch_add(1, Ordering::SeqCst) + 1;
            let fail_write_at = (self.plan)(attempt)?;
            let log = Arc::new(SegmentLog::default());
            lock(&self.segments).push(Arc::clone(&log));
            Ok(Box::new(ScriptedSegment {
                path: PathBuf::from(format!("segment-{attempt}")),
                log,
                len: 0,
                fail_write_at,
                writes: 0,
                write_delay: self.write_delay,
            }))
        }
    }

    fn written(segments: &Mutex<Vec<Arc<SegmentLog>>>) -> Vec<Vec<Vec<u8>>> {
        lock(segments)
            .iter()
            .map(|log| lock(&log.written).clone())
            .collect()
    }

    async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn items_queue_while_the_first_segment_opens() {
        // §56.1: bytes offered while Opening wait in the bounded queue, so a slow
        // open costs nothing when the queue holds them.
        let mut source = ScriptedSource::new(|_| Ok(None));
        source.open_delay = Duration::from_millis(50);
        let segments = Arc::clone(&source.segments);
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        recording.try_record(chunk(b"AB"), pos(0));
        recording.try_record(chunk(b"CD"), pos(2));
        assert!(recording
            .finalize(RecordingStopReason::Disabled)
            .await
            .fault
            .is_none());
        assert_eq!(
            written(&segments),
            vec![vec![b"AB".to_vec(), b"CD".to_vec()]]
        );
    }

    #[tokio::test]
    async fn a_write_failure_gaps_then_continues_in_a_new_segment() {
        // §56.1: a fault is not the end. The segment ends, a gap is recorded with
        // its reason and both ends, and recording resumes in a new segment.
        let source = ScriptedSource::new(|attempt| Ok((attempt == 1).then_some(2)));
        let segments = Arc::clone(&source.segments);
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());

        recording.try_record(chunk(b"AB"), pos(0));
        recording.try_record(chunk(b"CD"), pos(2)); // this write fails
        wait_for("the gap", || {
            matches!(
                recording.state(),
                RecordingState::Gap(GapReason::WriteFailed)
            )
        })
        .await;
        recording.try_record(chunk(b"EF"), pos(4)); // omitted: in the gap
        wait_for("the new segment", || {
            recording.state() == RecordingState::Enabled
        })
        .await;
        recording.try_record(chunk(b"GH"), pos(6));
        assert!(recording
            .finalize(RecordingStopReason::Disabled)
            .await
            .fault
            .is_none());

        assert_eq!(
            written(&segments),
            vec![vec![b"AB".to_vec()], vec![b"GH".to_vec()]]
        );
    }

    #[tokio::test]
    async fn the_handle_shows_the_current_file_and_its_length_until_a_gap() {
        // §56.2: status shows the file being written and how much is in it.
        let source = ScriptedSource::new(|attempt| Ok((attempt == 1).then_some(2)));
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        recording.try_record(chunk(b"ABC"), pos(0));
        wait_for("the first write", || recording.bytes_written() == 3).await;
        assert_eq!(recording.current_file(), Some(PathBuf::from("segment-1")));

        recording.try_record(chunk(b"DE"), pos(3)); // this write fails
        wait_for("the gap", || {
            matches!(recording.state(), RecordingState::Gap(_))
        })
        .await;
        assert_eq!(recording.current_file(), None);
        assert_eq!(recording.bytes_written(), 0);
        let _ = recording.finalize(RecordingStopReason::Disabled).await;
    }

    #[tokio::test]
    async fn gap_reports_carry_start_end_and_reason() {
        let source = ScriptedSource::new(|attempt| Ok((attempt == 1).then_some(1)));
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        recording.try_record(chunk(b"AB"), pos(10)); // fails
        let mut reports = Vec::new();
        wait_for("recovery", || {
            reports.extend(recording.take_reports());
            recording.state() == RecordingState::Enabled && reports.len() >= 3
        })
        .await;
        recording.try_record(chunk(b"CD"), pos(12));
        wait_for("the gap to close", || {
            reports.extend(recording.take_reports());
            reports
                .iter()
                .any(|r| matches!(r, RecorderReport::GapClosed { .. }))
        })
        .await;
        let _ = recording.finalize(RecordingStopReason::Disabled).await;

        assert!(reports.iter().any(|r| matches!(
            r,
            RecorderReport::GapOpened { reason: GapReason::WriteFailed, detail } if detail.contains("device removed")
        )));
        let closed = reports
            .iter()
            .find_map(|r| match r {
                RecorderReport::GapClosed { reason, start, end } => Some((*reason, *start, *end)),
                _ => None,
            })
            .unwrap();
        assert_eq!(closed.0, GapReason::WriteFailed);
        assert_eq!(closed.1.map(|p| p.offset), Some(10));
        assert_eq!(closed.2.offset, 12);
    }

    #[tokio::test]
    async fn an_overflow_writes_what_was_queued_then_gaps() {
        // The queued items precede the lost one, so they are written; the gap
        // starts at the item that did not fit.
        let source = ScriptedSource::new(|_| Ok(None));
        let segments = Arc::clone(&source.segments);
        let budget = 2 * (2 + PER_ITEM_OVERHEAD); // room for two 2-byte items
        let mut recording = start_recording(source, budget, fast());
        recording.try_record(chunk(b"AB"), pos(0));
        recording.try_record(chunk(b"CD"), pos(2));
        recording.try_record(chunk(b"EF"), pos(4)); // does not fit
        let mut reports = Vec::new();
        wait_for("recovery after the overflow", || {
            reports.extend(recording.take_reports());
            reports.iter().any(|r| {
                matches!(
                    r,
                    RecorderReport::SegmentOpened {
                        kind: OpenKind::Recovery,
                        ..
                    }
                )
            })
        })
        .await;
        recording.try_record(chunk(b"GH"), pos(6));
        let _ = recording.finalize(RecordingStopReason::Disabled).await;

        assert_eq!(
            written(&segments),
            vec![vec![b"AB".to_vec(), b"CD".to_vec()], vec![b"GH".to_vec()]]
        );
        assert!(reports.iter().any(|r| matches!(
            r,
            RecorderReport::GapOpened {
                reason: GapReason::QueueOverflow,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn an_overflow_is_reported_while_the_disk_is_still_busy() {
        // The loss begins at the overflow, so the gap shows then — not when a
        // write that may never return finally does.
        let mut source = ScriptedSource::new(|_| Ok(None));
        source.open_delay = Duration::from_secs(3600);
        let budget = 2 * (2 + PER_ITEM_OVERHEAD);
        let mut recording = start_recording(source, budget, fast());
        recording.try_record(chunk(b"AB"), pos(0));
        recording.try_record(chunk(b"CD"), pos(2));
        recording.try_record(chunk(b"EF"), pos(4)); // does not fit
        assert_eq!(
            recording.state(),
            RecordingState::Gap(GapReason::QueueOverflow)
        );
        assert!(recording.take_reports().iter().any(|r| matches!(
            r,
            RecorderReport::GapOpened {
                reason: GapReason::QueueOverflow,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn after_an_overflow_nothing_joins_the_segment_it_ends() {
        // Once bytes are lost, the segment holding what came before must end
        // there: an item offered after the loss never lands behind it.
        let mut source = ScriptedSource::new(|_| Ok(None));
        source.write_delay = Duration::from_millis(50);
        let segments = Arc::clone(&source.segments);
        let budget = 2 * (2 + PER_ITEM_OVERHEAD);
        let mut recording = start_recording(source, budget, fast());
        recording.try_record(chunk(b"AB"), pos(0));
        recording.try_record(chunk(b"CD"), pos(2));
        recording.try_record(chunk(b"EF"), pos(4)); // lost
        tokio::time::sleep(Duration::from_millis(20)).await; // the first segment is open, writing AB
        recording.try_record(chunk(b"GH"), pos(6)); // lost too: the gap has begun
        wait_for("recovery", || recording.state() == RecordingState::Enabled).await;
        recording.try_record(chunk(b"IJ"), pos(8));
        let finalized = recording.finalize(RecordingStopReason::Disabled).await;
        assert!(finalized.fault.is_none());
        assert_eq!(
            written(&segments),
            vec![vec![b"AB".to_vec(), b"CD".to_vec()], vec![b"IJ".to_vec()]]
        );
        let closed = finalized
            .reports
            .iter()
            .find_map(|r| match r {
                RecorderReport::GapClosed { start, end, .. } => Some((*start, *end)),
                _ => None,
            })
            .expect("the gap closed when IJ was written");
        assert_eq!(closed.0.map(|p| p.offset), Some(4));
        assert_eq!(closed.1.offset, 8);
    }

    #[tokio::test]
    async fn repeated_faults_mark_the_recording_unstable_and_slow_retries() {
        // ADR-043: a third fault within the window stretches the retry and marks
        // the recording unstable, so sustained overload cannot churn segments.
        let source = ScriptedSource::new(|_| Ok(Some(1))); // every segment's first write fails
        let opens = Arc::clone(&source.opens);
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        let mut offset = 0;
        wait_for("instability", || {
            if recording.state() == RecordingState::Enabled {
                recording.try_record(chunk(b"X"), pos(offset));
                offset += 1;
            }
            recording.is_unstable()
        })
        .await;
        let opens_when_unstable = opens.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            opens.load(Ordering::SeqCst),
            opens_when_unstable,
            "an unstable recording waits the long interval before retrying"
        );
        assert!(recording
            .take_reports()
            .iter()
            .any(|r| matches!(r, RecorderReport::Unstable { .. })));
        let _ = recording.finalize(RecordingStopReason::Disabled).await;
    }

    #[tokio::test]
    async fn a_terminal_first_open_faults_without_retrying() {
        // §55: a refused overwrite cannot be fixed by retrying.
        let source = ScriptedSource::new(|_| {
            Err(OpenError::Terminal(RecordError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "exists",
            ))))
        });
        let opens = Arc::clone(&source.opens);
        let recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        wait_for("the fault", || recording.state() == RecordingState::Faulted).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert!(recording
            .take_reports()
            .iter()
            .any(|r| matches!(r, RecorderReport::CouldNotBegin { .. })));
    }

    #[tokio::test]
    async fn a_missing_destination_waits_in_a_gap_until_it_returns() {
        let source = ScriptedSource::new(|attempt| {
            if attempt < 3 {
                Err(OpenError::Retry {
                    reason: GapReason::DestinationMissing,
                    error: RecordError::DestinationMissing("E:\\logs".into()),
                })
            } else {
                Ok(None)
            }
        });
        let segments = Arc::clone(&source.segments);
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        wait_for("the gap", || {
            recording.state() == RecordingState::Gap(GapReason::DestinationMissing)
        })
        .await;
        recording.try_record(chunk(b"lost"), pos(0));
        wait_for("the destination to return", || {
            recording.state() == RecordingState::Enabled
        })
        .await;
        recording.try_record(chunk(b"kept"), pos(4));
        let _ = recording.finalize(RecordingStopReason::Disabled).await;
        assert_eq!(written(&segments), vec![vec![b"kept".to_vec()]]);
    }

    #[tokio::test]
    async fn the_size_cap_starts_a_new_segment_without_splitting_an_item() {
        let mut source = ScriptedSource::new(|_| Ok(None));
        source.cap = Some(5);
        let segments = Arc::clone(&source.segments);
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        for (i, bytes) in [b"AB".as_slice(), b"CD", b"EF", b"GHIJK"]
            .into_iter()
            .enumerate()
        {
            recording.try_record(chunk(bytes), pos(i as u64 * 2));
        }
        let _ = recording.finalize(RecordingStopReason::Disabled).await;
        assert_eq!(
            written(&segments),
            vec![
                vec![b"AB".to_vec(), b"CD".to_vec()],
                vec![b"EF".to_vec()],
                vec![b"GHIJK".to_vec()],
            ],
            "each segment stays within the cap, and no item is split"
        );
    }

    #[tokio::test]
    async fn low_disk_gaps_until_space_returns() {
        let source = ScriptedSource::new(|_| Ok(None));
        let segments = Arc::clone(&source.segments);
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        recording.try_record(chunk(b"AB"), pos(0));
        wait_for("the first segment", || !written(&segments).is_empty()).await;
        recording.set_low_disk(true);
        wait_for("the low-disk gap", || {
            recording.state() == RecordingState::Gap(GapReason::LowDisk)
        })
        .await;
        recording.try_record(chunk(b"CD"), pos(2)); // omitted
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(written(&segments).len(), 1, "no retry while space is low");
        recording.set_low_disk(false);
        wait_for("recovery", || recording.state() == RecordingState::Enabled).await;
        recording.try_record(chunk(b"EF"), pos(4));
        assert!(
            !recording.is_unstable(),
            "low disk is a condition, not a fault"
        );
        let _ = recording.finalize(RecordingStopReason::Disabled).await;
        assert_eq!(
            written(&segments),
            vec![vec![b"AB".to_vec()], vec![b"EF".to_vec()]]
        );
    }
}
