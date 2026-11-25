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

use super::{GapMark, RecorderWriter};
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
        self.payload.len()
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
    /// Opening a file to append repaired its timestamp index (§57).
    IndexRepaired {
        path: Option<PathBuf>,
        detail: String,
    },
}

/// How the controller paces retries and flushes. Tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    /// The wait after a fault, doubling to its longest (`crate::retry`).
    pub retry: crate::retry::Backoff,
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
            retry: crate::retry::Backoff::default(),
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
}

/// State shared by the producer handle and the controller task.
struct Shared {
    /// The fields that must change together; see [`Gate`].
    gate: Mutex<Gate>,
    queued_bytes: AtomicUsize,
    unstable: AtomicBool,
    reports: Mutex<VecDeque<RecorderReport>>,
    /// The file being written and its length, for status (§56.2).
    file: Mutex<Option<PathBuf>>,
    file_len: AtomicU64,
    /// Test seam: called where the producer and the controller can interleave,
    /// so a test can pause one side there.
    #[cfg(test)]
    hook: Mutex<Option<TestHook>>,
}

#[cfg(test)]
type TestHook = Arc<dyn Fn(Checkpoint) + Send + Sync>;

/// The four fields the producer and the controller both move through the
/// overflow, open and gap transitions, under one lock so every transition is
/// one step to a reader (listener ADR-043). Held only for plain field updates,
/// the non-blocking enqueue and the report push, never across an await or I/O.
struct Gate {
    state: RecordingState,
    /// Whether the producer may enqueue. False during a gap.
    accepting: bool,
    /// The producer hit the budget; the gap starts once the queue drains.
    overflowed: bool,
    /// The first byte not recorded in the current gap.
    gap_start: Option<StreamPos>,
}

/// Where the overflow transition can be paused in a test. Each point is inside
/// the critical section that makes the transition atomic.
#[cfg(test)]
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checkpoint {
    /// `try_record`, after clearing `accepting`.
    AfterAccepting,
    /// `try_record`, after setting `overflowed`.
    AfterOverflowed,
    /// `try_record`, after setting the Gap state.
    AfterGapState,
    /// `Controller::open`, after reading `overflowed` and before acting on it.
    OpenAfterOverflowCheck,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn new() -> Self {
        Self {
            gate: Mutex::new(Gate {
                // Items queue while the first segment opens (§56.1).
                state: RecordingState::Enabled,
                accepting: true,
                overflowed: false,
                gap_start: None,
            }),
            queued_bytes: AtomicUsize::new(0),
            unstable: AtomicBool::new(false),
            reports: Mutex::new(VecDeque::new()),
            file: Mutex::new(None),
            file_len: AtomicU64::new(0),
            #[cfg(test)]
            hook: Mutex::new(None),
        }
    }

    fn state(&self) -> RecordingState {
        lock(&self.gate).state
    }

    #[cfg(test)]
    fn accepting(&self) -> bool {
        lock(&self.gate).accepting
    }

    fn overflowed(&self) -> bool {
        lock(&self.gate).overflowed
    }

    fn take_gap_start(&self) -> Option<StreamPos> {
        lock(&self.gate).gap_start.take()
    }

    #[cfg(test)]
    fn checkpoint(&self, point: Checkpoint) {
        let hook = lock(&self.hook).clone();
        if let Some(hook) = hook {
            hook(point);
        }
    }

    fn set_state(&self, state: RecordingState) {
        lock(&self.gate).state = state;
    }

    /// Note the first byte not recorded in this gap; later ones change nothing.
    fn note_gap_start(&self, pos: StreamPos) {
        lock(&self.gate).gap_start.get_or_insert(pos);
    }

    /// Move the gap's start back to `pos` when that is earlier than the
    /// start noted so far.
    fn widen_gap_start(&self, pos: StreamPos) {
        let mut gate = lock(&self.gate);
        if gate.gap_start.is_none_or(|noted| pos.offset < noted.offset) {
            gate.gap_start = Some(pos);
        }
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
    terminate: Option<oneshot::Sender<()>>,
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
        // One critical section: the checks, the overflow transition and the
        // enqueue, so the controller never sees half of any of them and no
        // item is enqueued after it has closed the gate.
        let mut gate = lock(&self.shared.gate);
        if !gate.state.is_on() {
            return;
        }
        if !gate.accepting {
            gate.gap_start.get_or_insert(pos);
            return;
        }
        let cost = item.byte_len() + PER_ITEM_OVERHEAD;
        let queued = self.shared.queued_bytes.load(Ordering::Acquire);
        if queued + cost > self.budget {
            gate.accepting = false;
            #[cfg(test)]
            self.shared.checkpoint(Checkpoint::AfterAccepting);
            gate.overflowed = true;
            #[cfg(test)]
            self.shared.checkpoint(Checkpoint::AfterOverflowed);
            gate.gap_start.get_or_insert(pos);
            // Bytes are lost from here, so the gap is reported now: the
            // controller may be stuck in a write that never returns.
            gate.state = RecordingState::Gap(GapReason::QueueOverflow);
            #[cfg(test)]
            self.shared.checkpoint(Checkpoint::AfterGapState);
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

    /// Stop recording (user disable or Channel stop): the controller writes the
    /// accepted backlog and finalizes (§56, §110).
    pub async fn finalize(mut self) -> Finalized {
        if let Some(terminate) = self.terminate.take() {
            let _ = terminate.send(());
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
    let shared = Arc::new(Shared::new());
    let controller = Controller {
        source,
        shared: Arc::clone(&shared),
        timings,
        segment: None,
        period: None,
        gap: None,
        closing_gap: false,
        ever_opened: false,
        backoff: timings.retry.first,
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

/// A segment whose finalization failed: what failed, and the earliest stream
/// position whose bytes may not have reached the disk.
struct FailedEnd {
    detail: String,
    start: Option<StreamPos>,
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
        mut terminate: oneshot::Receiver<()>,
    ) -> Option<String> {
        let mut flush_tick =
            tokio::time::interval_at(Instant::now() + self.timings.flush, self.timings.flush);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if self.segment.is_none() {
                if let Some(at) = self.retry_at {
                    if Instant::now() >= at {
                        if let Flow::Stop(fault) = self.open_next(&mut items).await {
                            return fault;
                        }
                    }
                }
            }
            let retry_at = self.retry_at.filter(|_| self.segment.is_none());
            tokio::select! {
                biased;
                _ = &mut terminate => {
                    return self.stop(&mut items).await;
                }
                Some(command) = commands.recv() => self.on_command(command, &mut items).await,
                // Before the items, so a queue that never empties cannot
                // postpone an overdue flush.
                _ = flush_tick.tick(), if self.segment.is_some() => self.flush(&mut items).await,
                maybe = items.recv() => match maybe {
                    Some(queued) => self.on_item(queued, &mut items).await,
                    None => return self.stop(&mut items).await,
                },
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
            Ok(mut segment) => {
                let repaired = segment.take_open_note();
                let path = segment.path().map(PathBuf::from);
                *lock(&self.shared.file) = path.clone();
                self.shared
                    .file_len
                    .store(segment.bytes_written(), Ordering::Relaxed);
                self.segment = Some(segment);
                self.period = period.or(self.period.take());
                self.ever_opened = true;
                self.backoff = self.timings.retry.first;
                self.shared.report(RecorderReport::SegmentOpened {
                    path: path.clone(),
                    kind,
                });
                if let Some(detail) = repaired {
                    self.shared
                        .report(RecorderReport::IndexRepaired { path, detail });
                }
                if self.gap.is_some() {
                    self.closing_gap = true;
                }
                // After an overflow the producer stays stopped: the queued
                // items are written, then the segment ends where the loss
                // began (`end_after_overflow`).
                {
                    // One step: either the overflow is seen and left alone, or
                    // the producer is let in, never both.
                    let mut gate = lock(&self.shared.gate);
                    let overflowed = gate.overflowed;
                    #[cfg(test)]
                    self.shared.checkpoint(Checkpoint::OpenAfterOverflowCheck);
                    if !overflowed {
                        gate.state = RecordingState::Enabled;
                        gate.accepting = true;
                    }
                }
                Flow::Continue
            }
            Err(OpenError::Terminal(error)) if !self.ever_opened => {
                {
                    let mut gate = lock(&self.shared.gate);
                    gate.accepting = false;
                    gate.state = RecordingState::Faulted;
                }
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
                if self.end_rotated_segment(queued.pos, items).await {
                    return;
                }
                if let Flow::Stop(_) = self.open(OpenKind::Rotation, Some(period), items).await {
                    return;
                }
            }
        }
        if let (Some(cap), Some(segment)) = (self.source.size_cap(), &self.segment) {
            let len = segment.bytes_written();
            if len > 0 && len + queued.item.byte_len() as u64 > cap {
                let period = self.period.clone();
                if self.end_rotated_segment(queued.pos, items).await {
                    return;
                }
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
        // The first item after a gap: a recorder that marks gaps writes its line
        // now, so it comes just before the item the recording resumed with.
        let gap_start = lock(&self.shared.gate).gap_start;
        let marked = match (self.closing_gap, self.gap) {
            (true, Some(reason)) => match gap_start {
                Some(start) => {
                    segment
                        .mark_gap(&GapMark {
                            reason,
                            start,
                            end: queued.pos,
                        })
                        .await
                }
                None => Ok(()),
            },
            _ => Ok(()),
        };
        let written = match marked {
            Ok(()) => segment.write(&queued.item).await,
            Err(error) => Err(error),
        };
        match written {
            Ok(()) => {
                self.shared
                    .file_len
                    .store(segment.bytes_written(), Ordering::Relaxed);
                self.unflushed_since.get_or_insert(queued.pos);
                if self.closing_gap {
                    self.closing_gap = false;
                    if let Some(reason) = self.gap.take() {
                        let start = self.shared.take_gap_start();
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
                // The gap that follows accounts for a failed finalize.
                let _ = self.end_segment().await;
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
        if self.shared.overflowed() && items.is_empty() {
            let failed = self.end_segment().await;
            // The producer reported this gap when the loss began, so a failed
            // finalize cannot be a report of its own: it widens the gap back
            // to the bytes that may not have reached the disk and is named in
            // the fault's detail.
            let detail = match failed {
                Some(failed) => {
                    if let Some(start) = failed.start {
                        self.shared.widen_gap_start(start);
                    }
                    format!("{OVERFLOW_DETAIL}; {}", failed.detail)
                }
                None => OVERFLOW_DETAIL.to_owned(),
            };
            self.gap.get_or_insert(GapReason::QueueOverflow);
            self.begin_gap(GapReason::QueueOverflow, detail, None, items);
        }
    }

    async fn on_command(
        &mut self,
        command: Command,
        items: &mut mpsc::UnboundedReceiver<Queued<I>>,
    ) {
        match command {
            Command::Overflowed => self.end_after_overflow(items).await,
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
                // The gap that follows accounts for a failed finalize.
                let _ = self.end_segment().await;
                self.begin_gap(GapReason::WriteFailed, error.to_string(), start, items);
            }
        }
    }

    /// Finalize the current segment. A failed finalize is returned with the
    /// earliest position whose bytes were not known to be on disk; the caller
    /// reports it unless a gap that already covers those bytes follows.
    async fn end_segment(&mut self) -> Option<FailedEnd> {
        let start = self.unflushed_since.take();
        let failed = match self.segment.take() {
            Some(mut segment) => segment.finalize().await.err().map(|error| FailedEnd {
                detail: format!("the segment could not be finalized: {error}"),
                start,
            }),
            None => None,
        };
        *lock(&self.shared.file) = None;
        self.shared.file_len.store(0, Ordering::Relaxed);
        failed
    }

    /// End the segment at a rotation boundary. A failed finalize is a fault:
    /// it opens a gap from the first uncertain byte, or from `next` when
    /// every earlier byte was flushed, and the next segment opens through
    /// recovery. Returns whether that happened, so the caller omits `next`.
    async fn end_rotated_segment(
        &mut self,
        next: StreamPos,
        items: &mut mpsc::UnboundedReceiver<Queued<I>>,
    ) -> bool {
        let Some(failed) = self.end_segment().await else {
            return false;
        };
        let start = failed.start.unwrap_or(next);
        self.begin_gap(GapReason::WriteFailed, failed.detail, Some(start), items);
        true
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
            self.backoff = self.timings.retry.after(self.backoff);
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
        // Close the gate in one step, with the gap's state: from here the
        // producer enqueues nothing, so the drain below sees every item.
        {
            let mut gate = lock(&self.shared.gate);
            gate.accepting = false;
            gate.overflowed = false;
            gate.state = RecordingState::Gap(reason);
        }
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
    }

    /// Stop: write the accepted backlog — under the same
    /// rotation and size-cap rules as while running — then finalize. Returns
    /// the fault when the stop was not clean.
    async fn stop(mut self, items: &mut mpsc::UnboundedReceiver<Queued<I>>) -> Option<String> {
        self.last_fault = None;
        while let Ok(queued) = items.try_recv() {
            self.write_item(queued, items).await;
        }
        let mut fault = self.last_fault.take();
        if let Some(mut segment) = self.segment.take() {
            if let Err(error) = segment.finalize().await {
                fault.get_or_insert(format!("finalize failed: {error}"));
            }
        }
        // A gap still open — an overflow not yet ended, a reopen that wrote
        // nothing, or a fault while writing the backlog — ends with the
        // recording.
        if self.gap.is_some() || self.shared.overflowed() {
            let reason = self.gap.take().unwrap_or(GapReason::QueueOverflow);
            let start = self.shared.take_gap_start();
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
    use std::sync::atomic::AtomicU32;

    type Item = Arc<ReceivedData>;

    fn chunk(bytes: &[u8]) -> Item {
        Arc::new(ReceivedData {
            channel_id: ChannelId::new(),
            payload: bytes.to_vec(),
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
            retry: crate::retry::Backoff {
                first: Duration::from_millis(10),
                max: Duration::from_millis(40),
            },
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
        flushes: AtomicUsize,
        /// Gap lines the controller asked this segment to write.
        gap_marks: AtomicUsize,
    }

    struct ScriptedSegment {
        path: PathBuf,
        log: Arc<SegmentLog>,
        len: u64,
        fail_write_at: Option<usize>,
        writes: usize,
        write_delay: Duration,
        fail_finalize: bool,
        /// The flush, counted from 1, that fails.
        fail_flush_at: Option<usize>,
    }

    #[async_trait::async_trait]
    impl RecorderWriter<Item> for ScriptedSegment {
        async fn write(&mut self, item: &Item) -> Result<(), RecordError> {
            tokio::time::sleep(self.write_delay).await;
            self.writes += 1;
            if self.fail_write_at == Some(self.writes) {
                return Err(RecordError::Io(std::io::Error::other("device removed")));
            }
            let bytes = item.payload.clone();
            self.len += bytes.len() as u64;
            lock(&self.log.written).push(bytes);
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            let flushes = self.log.flushes.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_flush_at == Some(flushes) {
                return Err(RecordError::Io(std::io::Error::other("flush failed")));
            }
            Ok(())
        }
        async fn mark_gap(&mut self, _gap: &GapMark) -> Result<(), RecordError> {
            self.log.gap_marks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn finalize(&mut self) -> Result<(), RecordError> {
            if self.fail_finalize {
                return Err(RecordError::Io(std::io::Error::other("final write failed")));
            }
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
        /// Hours per rotation period, when the recording rotates.
        period_hours: Option<u64>,
        /// The open attempts whose segment fails its finalize.
        fail_finalize_on: Vec<u32>,
        /// The open attempt whose segment fails a flush, and which flush.
        fail_flush: Option<(u32, usize)>,
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
                period_hours: None,
                fail_finalize_on: Vec::new(),
                fail_flush: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl SegmentSource<Item> for ScriptedSource {
        fn size_cap(&self) -> Option<u64> {
            self.cap
        }
        fn period_of(&self, at: SystemTime) -> Option<String> {
            let hours = self.period_hours?;
            let secs = at.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
            Some(format!("{:010}", secs / (hours * 3600)))
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
                fail_finalize: self.fail_finalize_on.contains(&attempt),
                fail_flush_at: self
                    .fail_flush
                    .filter(|(on, _)| *on == attempt)
                    .map(|(_, at)| at),
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
        assert!(recording.finalize().await.fault.is_none());
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
        assert!(recording.finalize().await.fault.is_none());

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
        let _ = recording.finalize().await;
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
        let _ = recording.finalize().await;

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
        let _ = recording.finalize().await;

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
        let finalized = recording.finalize().await;
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
        let _ = recording.finalize().await;
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
        let _ = recording.finalize().await;
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
        let _ = recording.finalize().await;
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

    fn pos_at(offset: u64, at: SystemTime) -> StreamPos {
        StreamPos { offset, at }
    }

    /// Wait until a Recovery segment has opened, returning every report seen
    /// on the way.
    async fn reports_until_recovery(recording: &mut Recording<Item>) -> Vec<RecorderReport> {
        let mut reports = Vec::new();
        wait_for("the recovery segment", || {
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
        reports
    }

    /// §56.1: a gap that lost nothing puts no gap line in the file the
    /// recording resumes in. Here a scheduled flush fails when everything
    /// written had already been flushed, so no byte is in doubt.
    #[tokio::test]
    async fn a_gap_that_lost_no_bytes_leaves_no_line() {
        let mut source = ScriptedSource::new(|_| Ok(None));
        // The first flush hands "AB" on; the second fails with nothing waiting.
        source.fail_flush = Some((1, 2));
        let segments = Arc::clone(&source.segments);
        let timings = Timings {
            flush: Duration::from_millis(20),
            ..fast()
        };
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, timings);
        recording.try_record(chunk(b"AB"), pos(0));
        let reports = reports_until_recovery(&mut recording).await;
        assert!(
            reports.iter().any(|r| matches!(
                r,
                RecorderReport::GapOpened {
                    reason: GapReason::WriteFailed,
                    ..
                }
            )),
            "{reports:?}"
        );
        recording.try_record(chunk(b"GH"), pos(2));
        let finalized = recording.finalize().await;
        assert!(
            finalized
                .reports
                .iter()
                .any(|r| matches!(r, RecorderReport::GapClosed { start: None, .. })),
            "the gap lost nothing: {:?}",
            finalized.reports
        );
        assert_eq!(
            written(&segments),
            vec![vec![b"AB".to_vec()], vec![b"GH".to_vec()]]
        );
        let marks: usize = lock(&segments)
            .iter()
            .map(|log| log.gap_marks.load(Ordering::SeqCst))
            .sum();
        assert_eq!(marks, 0, "no gap line where nothing was lost");
    }

    fn finalize_gap(reports: &[RecorderReport]) -> Option<&RecorderReport> {
        reports.iter().find(|r| {
            matches!(
                r,
                RecorderReport::GapOpened {
                    reason: GapReason::WriteFailed,
                    detail,
                } if detail.contains("final write failed")
            )
        })
    }

    #[tokio::test]
    async fn a_failed_finalize_at_the_size_cap_is_reported_as_a_gap() {
        let mut source = ScriptedSource::new(|_| Ok(None));
        source.cap = Some(5);
        source.fail_finalize_on = vec![1];
        let segments = Arc::clone(&source.segments);
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        recording.try_record(chunk(b"AB"), pos(0));
        recording.try_record(chunk(b"CD"), pos(2));
        recording.try_record(chunk(b"EF"), pos(4)); // rotates; the first finalize fails
        let mut reports = reports_until_recovery(&mut recording).await;
        recording.try_record(chunk(b"GH"), pos(6));
        let finalized = recording.finalize().await;
        reports.extend(finalized.reports);

        assert!(finalized.fault.is_none(), "the later segment is clean");
        assert!(finalize_gap(&reports).is_some(), "{reports:?}");
        let closed = reports.iter().find_map(|r| match r {
            RecorderReport::GapClosed { start, end, .. } => Some((*start, *end)),
            _ => None,
        });
        let (start, end) = closed.expect("the gap closed");
        assert_eq!(start.map(|p| p.offset), Some(0), "from the unflushed bytes");
        assert_eq!(end.offset, 6);
        assert_eq!(
            written(&segments),
            vec![vec![b"AB".to_vec(), b"CD".to_vec()], vec![b"GH".to_vec()]]
        );
    }

    #[tokio::test]
    async fn a_failed_finalize_at_a_period_change_is_reported_as_a_gap() {
        let mut source = ScriptedSource::new(|_| Ok(None));
        source.period_hours = Some(1);
        source.fail_finalize_on = vec![1];
        let segments = Arc::clone(&source.segments);
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        let now = SystemTime::now();
        let later = now + Duration::from_secs(3 * 3600);
        recording.try_record(chunk(b"AB"), pos_at(0, now));
        recording.try_record(chunk(b"CD"), pos_at(2, later)); // a new period
        let mut reports = reports_until_recovery(&mut recording).await;
        recording.try_record(chunk(b"EF"), pos_at(4, now)); // an earlier period keeps the file
        let finalized = recording.finalize().await;
        reports.extend(finalized.reports);

        assert!(finalized.fault.is_none(), "the later segment is clean");
        assert!(finalize_gap(&reports).is_some(), "{reports:?}");
        let closed = reports.iter().find_map(|r| match r {
            RecorderReport::GapClosed { start, .. } => Some(*start),
            _ => None,
        });
        assert_eq!(closed.flatten().map(|p| p.offset), Some(0));
        assert_eq!(
            written(&segments),
            vec![vec![b"AB".to_vec()], vec![b"EF".to_vec()]]
        );
    }

    #[tokio::test]
    async fn a_failed_finalize_at_stop_is_still_reported_as_a_fault() {
        let mut source = ScriptedSource::new(|_| Ok(None));
        source.fail_finalize_on = vec![1];
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, fast());
        recording.try_record(chunk(b"AB"), pos(0));
        let finalized = recording.finalize().await;
        assert!(finalized
            .fault
            .is_some_and(|fault| fault.contains("finalize failed")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_ready_backlog_does_not_postpone_the_periodic_flush() {
        let mut source = ScriptedSource::new(|_| Ok(None));
        source.write_delay = Duration::from_millis(1);
        let segments = Arc::clone(&source.segments);
        let timings = Timings {
            flush: Duration::from_millis(5),
            ..fast()
        };
        let mut recording = start_recording(source, DEFAULT_QUEUE_BUDGET, timings);
        for i in 0..60u64 {
            recording.try_record(chunk(b"X"), pos(i));
        }
        // 25 ms of writing against a queue holding 60 items: the queue never
        // empties, so every flush is one the timer forced.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let flushes = lock(&segments)[0].flushes.load(Ordering::SeqCst);
        assert!(flushes >= 3, "only {flushes} flushes before Stop");
        let _ = recording.finalize().await;
    }

    // -- the overflow transition, interleaved with a segment open ----------
    //
    // `try_record` overflowing and `Controller::open` finishing both write
    // the state, `accepting` and `overflowed`, from two threads. These tests
    // pause one side inside its transition, through the `Checkpoint` seam in
    // the production code (which exists only under `cfg(test)`), and let the
    // other side run into it. The pause lasts `HOLD`, so a side that cannot
    // proceed (because the lock excludes it) is simply released afterwards;
    // either way the result is checked, and the order that produced it is
    // fixed by the pause, not by thread timing.
    //
    // Interleavings covered, for one producer overflow against one open:
    //  - the overflow completes before the open starts, or after it ends;
    //  - the overflow is offered inside the open, between its read of
    //    `overflowed` and its stores;
    //  - the producer is paused partway through the overflow (after clearing
    //    `accepting`, after setting `overflowed`, after setting the Gap state)
    //    while the open is run.
    //
    // What each must leave, whichever side ran first: the handle reads as in
    // a queue-overflow gap and refuses new items; the one gap is reported
    // once, starting at the item that did not fit; nothing offered after that
    // item reaches the segment; and recovery closes the gap with that start.
    // Before the four fields shared one lock, the second and third paused
    // orders left `accepting` true or the state Enabled with the gap open.
    // The tests do not cover two producers, or a controller fault racing the
    // overflow.

    use std::sync::mpsc as std_mpsc;

    /// How long a paused side waits for the other to act.
    const HOLD: Duration = Duration::from_millis(150);

    #[derive(Clone, Copy, Debug)]
    enum Interleave {
        BeforeOpen,
        InsideOpen,
        PausedAt(Checkpoint),
        AfterOpen,
    }

    async fn overflow_interleaved_with_open(order: Interleave) {
        let source = ScriptedSource::new(|_| Ok(None));
        let segments = Arc::clone(&source.segments);
        // The controller is stepped by hand, not spawned.
        let (items_tx, mut items) = mpsc::unbounded_channel();
        let (commands_tx, mut commands) = mpsc::unbounded_channel();
        let budget = 230;
        let shared = Arc::new(Shared::new());
        let mut controller = Controller {
            source,
            shared: Arc::clone(&shared),
            timings: fast(),
            segment: None,
            period: None,
            gap: None,
            closing_gap: false,
            ever_opened: false,
            backoff: fast().retry.first,
            retry_at: Some(Instant::now()),
            faults: VecDeque::new(),
            unflushed_since: None,
            last_fault: None,
        };
        let mut recording = Recording {
            items: items_tx,
            commands: commands_tx,
            terminate: None,
            shared: Arc::clone(&shared),
            task: tokio::spawn(async { None }),
            budget,
            peak_bytes: 0,
        };
        // Two items fill the queue to 148 of 230 bytes while the first
        // segment is still opening.
        recording.try_record(chunk(&[1; 10]), pos(0));
        recording.try_record(chunk(&[2; 10]), pos(10));

        let (go_tx, go_rx) = std_mpsc::channel::<()>();
        let (done_tx, done_rx) = std_mpsc::channel::<()>();
        let done_rx = Arc::new(Mutex::new(done_rx));
        let (reached_tx, reached_rx) = std_mpsc::channel::<()>();
        let (resume_tx, resume_rx) = std_mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        let go_in_hook = go_tx.clone();
        let done_in_hook = Arc::clone(&done_rx);
        // The pause acts once; the recovery open later passes the same point.
        let fired = AtomicBool::new(false);
        *lock(&shared.hook) = Some(Arc::new(move |point| match (order, point) {
            _ if fired.load(Ordering::SeqCst) => {}
            (Interleave::InsideOpen, Checkpoint::OpenAfterOverflowCheck) => {
                fired.store(true, Ordering::SeqCst);
                go_in_hook.send(()).unwrap();
                let _ = done_in_hook.lock().unwrap().recv_timeout(HOLD);
            }
            (Interleave::PausedAt(at), point) if at == point => {
                fired.store(true, Ordering::SeqCst);
                reached_tx.send(()).unwrap();
                let _ = resume_rx.lock().unwrap().recv_timeout(HOLD);
            }
            _ => {}
        }));

        // The producer: this 100-byte item (cost 164) does not fit.
        let producer = std::thread::spawn(move || {
            go_rx.recv().unwrap();
            recording.try_record(chunk(&[3; 100]), pos(20));
            done_tx.send(()).unwrap();
            recording
        });
        let wait_done = || done_rx.lock().unwrap().recv().unwrap();
        match order {
            Interleave::BeforeOpen => {
                go_tx.send(()).unwrap();
                wait_done();
                controller.open_next(&mut items).await;
            }
            Interleave::InsideOpen => {
                controller.open_next(&mut items).await;
            }
            Interleave::PausedAt(_) => {
                go_tx.send(()).unwrap();
                reached_rx.recv().unwrap();
                controller.open_next(&mut items).await;
                let _ = resume_tx.send(());
            }
            Interleave::AfterOpen => {
                controller.open_next(&mut items).await;
                go_tx.send(()).unwrap();
                wait_done();
            }
        }
        let mut recording = producer.join().unwrap();
        let why = format!("{order:?}");

        // Both sides are done and the controller has not yet seen the
        // command. What a reader of the handle sees must already be one
        // coherent state.
        assert_eq!(
            shared.state(),
            RecordingState::Gap(GapReason::QueueOverflow),
            "{why}: state"
        );
        assert!(
            !shared.accepting(),
            "{why}: the producer must not be accepting"
        );
        assert!(shared.overflowed(), "{why}: overflowed");
        let queued = shared.queued_bytes.load(Ordering::Acquire);
        recording.try_record(chunk(&[4; 10]), pos(30)); // cost 74: fits, if accepted
        assert_eq!(
            shared.queued_bytes.load(Ordering::Acquire),
            queued,
            "{why}: an item offered after the loss was accepted"
        );

        // The controller catches up.
        while let Ok(command) = commands.try_recv() {
            controller.on_command(command, &mut items).await;
        }
        while let Ok(queued) = items.try_recv() {
            controller.on_item(queued, &mut items).await;
        }
        assert_eq!(
            shared.state(),
            RecordingState::Gap(GapReason::QueueOverflow),
            "{why}: state after catching up"
        );
        assert!(!shared.accepting(), "{why}");
        assert!(!shared.overflowed(), "{why}");
        assert_eq!(
            lock(&shared.gate).gap_start.map(|p| p.offset),
            Some(20),
            "{why}: the gap starts at the item that did not fit"
        );
        assert_eq!(
            written(&segments),
            vec![vec![vec![1; 10], vec![2; 10]]],
            "{why}: only what preceded the loss is in the segment"
        );

        // Recovery resumes in a new segment and closes the one gap.
        controller.open_next(&mut items).await;
        assert_eq!(shared.state(), RecordingState::Enabled, "{why}");
        assert!(shared.accepting(), "{why}");
        recording.try_record(chunk(&[5; 10]), pos(40));
        while let Ok(queued) = items.try_recv() {
            controller.on_item(queued, &mut items).await;
        }
        let reports = recording.take_reports();
        let opened = reports
            .iter()
            .filter(|r| matches!(r, RecorderReport::GapOpened { .. }))
            .count();
        assert_eq!(opened, 1, "{why}: one gap, reported once: {reports:?}");
        let closed: Vec<_> = reports
            .iter()
            .filter_map(|r| match r {
                RecorderReport::GapClosed { reason, start, end } => {
                    Some((*reason, start.map(|p| p.offset), end.offset))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            closed,
            vec![(GapReason::QueueOverflow, Some(20), 40)],
            "{why}"
        );
        assert_eq!(
            written(&segments),
            vec![vec![vec![1; 10], vec![2; 10]], vec![vec![5; 10]]],
            "{why}"
        );
    }

    #[tokio::test]
    async fn overflow_before_the_open_leaves_one_coherent_gap() {
        overflow_interleaved_with_open(Interleave::BeforeOpen).await;
    }

    #[tokio::test]
    async fn overflow_inside_the_open_leaves_one_coherent_gap() {
        overflow_interleaved_with_open(Interleave::InsideOpen).await;
    }

    #[tokio::test]
    async fn overflow_paused_after_accepting_leaves_one_coherent_gap() {
        overflow_interleaved_with_open(Interleave::PausedAt(Checkpoint::AfterAccepting)).await;
    }

    #[tokio::test]
    async fn overflow_paused_after_overflowed_leaves_one_coherent_gap() {
        overflow_interleaved_with_open(Interleave::PausedAt(Checkpoint::AfterOverflowed)).await;
    }

    #[tokio::test]
    async fn overflow_paused_after_gap_state_leaves_one_coherent_gap() {
        overflow_interleaved_with_open(Interleave::PausedAt(Checkpoint::AfterGapState)).await;
    }

    #[tokio::test]
    async fn overflow_after_the_open_leaves_one_coherent_gap() {
        overflow_interleaved_with_open(Interleave::AfterOpen).await;
    }
}
