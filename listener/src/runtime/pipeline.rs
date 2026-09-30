//! Per-Channel stream pipeline (spec §102, §99.1, §108).
//!
//! One stage: each received chunk fans out, non-blocking, to the raw-recording
//! tap (§53), the stream scrollback (§87), per-view display recording (§54),
//! find/trigger evaluation (§50.2), and diagnostics. There is no extraction,
//! metadata, or decoding stage (ADR-010) — the bytes are never reframed.
//!
//! Acquisition priority (§5.9, §100): every edge here is non-blocking — the
//! scrollback ring drops oldest, a recorder faults on overflow. The only edge
//! permitted to stall the reader is the Transport→Pipeline channel (§97.1),
//! the bounded `tokio::sync::mpsc` that feeds [`run_channel`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::matchrule::{FiredRule, MatchRuleSet};
use super::snapshot::{MarkRender, TriggeredMatch};

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::activity::ActivityMeter;
use super::snapshot::{
    ChannelSnapshot, ChannelStats, DiagnosticsSnapshot, DisplayViewSnapshot, PipelineRequest,
    QueueDepth, StreamDelta,
};
use super::telemetry::{
    ChunkShape, CounterAvailability, DurationHistogram, IdleDeadlineTimerMode,
    IdleDeadlineTimerSummary, RecentDurationHistogram, SerialStallSummary, TransportHealth,
};
use crate::config::{
    DiskGuard, DiskThreshold, LowDiskAction, MarkPosition, MarkTimestampStyle, MatchAction,
    MatchRule, RecordControl, RecordTarget,
};

use crate::core::{
    zda_sentence, ChannelId, ChunkTime, DisplayViewId, MatchRuleId, RecordingState, RecordingTap,
    RuntimeEvent,
};
use crate::diagnostics::{Diagnostic, DiagnosticLog};
use crate::display::{
    AnnotationPlacement, DisplayView, RenderAnnotation, RenderedOutput, StreamRenderer,
};
use crate::record::{
    start_display_recording, start_raw_recording, DisplayFileRecorder, FileRotationPolicy,
    OverwritePolicy, RawFileRecorder, Recording, RecordingStopReason, RotatingDisplayRecorder,
    RotatingRawRecorder,
};
use crate::transport::{ReceivedData, SerialStallState, TransportNotice};

use super::queue::DropOldestQueue;

/// An inline Mark annotation produced by a firing this chunk (§50.2): the
/// **absolute** stream-space anchor (first match byte for `Before`, final byte for
/// `After`), its placement around that byte, and already-formatted text. The
/// pipeline rebases the anchor onto the current chunk for `.disp` and ships the
/// same text to the snapshot for the live view.
#[derive(Clone, Debug)]
struct MarkAnnotation {
    offset: u64,
    before: bool,
    text: String,
}

/// Rebase this chunk's Mark annotations (absolute offsets) onto within-chunk byte
/// offsets for the `.disp` render. An anchor in a *prior* chunk (for example a
/// `Before` annotation on a boundary-split match) cannot be spliced into this
/// chunk's already-recorded text and is dropped from `.disp`; an `After` anchor on
/// the completing byte still lands. Offsets past the chunk are ignored.
fn render_annotations_for_chunk(
    marks: &[MarkAnnotation],
    chunk_offset: u64,
    chunk_len: usize,
) -> Vec<RenderAnnotation> {
    marks
        .iter()
        .filter_map(|m| {
            let within = m.offset.checked_sub(chunk_offset)? as usize;
            (within <= chunk_len).then(|| RenderAnnotation {
                offset: within,
                placement: if m.before {
                    AnnotationPlacement::Before
                } else {
                    AnnotationPlacement::After
                },
                text: m.text.clone(),
            })
        })
        .collect()
}

/// Bounded capacities for a Channel's fan-out edges (§99, §124).
#[derive(Clone, Copy, Debug)]
pub struct PipelineCapacities {
    /// The bounded Transport→Pipeline queue — the only edge that may stall the
    /// reader (§97.1, §99).
    pub ingest: usize,
    /// Stream scrollback (§87): how many of the most recent received bytes to
    /// keep for display. Byte-capped — there are no Message boundaries (§18).
    pub stream_display: usize,
    pub raw_recording: usize,
    /// Per-type retained-diagnostic limits (§88): events, warnings, errors.
    /// `None` falls back to the retention backstop (~1 M entries) — the defaults
    /// below set real limits instead, because a recurring diagnostic (a flapping
    /// warning, a per-occurrence `Notify` rule) would otherwise grow the log —
    /// and the full-log snapshot clone (§137, 5 Hz) — for weeks (§124).
    pub event_retention: Option<usize>,
    pub warning_retention: Option<usize>,
    pub error_retention: Option<usize>,
    /// Runtime→UI event stream ([`RuntimeEvent`], §137).
    pub events: usize,
}

/// Default per-severity retained-diagnostic limit (§88): plenty of history for
/// review, small enough that the 5 Hz full-log snapshot clone stays cheap.
const DIAGNOSTIC_RETENTION: usize = 500;

impl Default for PipelineCapacities {
    fn default() -> Self {
        Self {
            ingest: 256,
            stream_display: 128 * 1024,
            raw_recording: 1024,
            event_retention: Some(DIAGNOSTIC_RETENTION),
            warning_retention: Some(DIAGNOSTIC_RETENTION),
            error_retention: Some(DIAGNOSTIC_RETENTION),
            events: 256,
        }
    }
}

/// A handle to one Display View's runtime pause state (§11). Cloneable and
/// shareable across the pipeline-task boundary: a UI/orchestrator holds a clone
/// and flips pause/resume without a command channel into the pipeline.
#[derive(Clone, Debug)]
pub struct DisplayViewHandle {
    pub id: DisplayViewId,
    paused: Arc<AtomicBool>,
}

impl DisplayViewHandle {
    fn new_active() -> Self {
        Self {
            id: DisplayViewId::new(),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Pause this view: it stops accumulating new stream bytes. Reception,
    /// recording, and *other* views are unaffected (§50).
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// A Display View's optional recorder: its **streaming** renderer (per-recording
/// state so the `.disp` is the exact rendered stream, ADR-018) plus the
/// display-recording handle (§54). Recording runs regardless of pause (§58).
struct ViewRecorder {
    renderer: StreamRenderer,
    recording: Recording<RenderedOutput>,
}

/// One Display View's runtime state: pause handle and an optional Display
/// Recording (§54). The displayed content itself is the shared stream
/// scrollback (§87) rendered per view; views hold no per-view history.
struct PipelineDisplayView {
    handle: DisplayViewHandle,
    recorder: Option<ViewRecorder>,
}

impl PipelineDisplayView {
    fn new() -> Self {
        Self {
            handle: DisplayViewHandle::new_active(),
            recorder: None,
        }
    }
}

/// A detached recorder stop's outcome: the arguments
/// [`ChannelPipeline::note_recording_stop`] needs to report it honestly —
/// `(tap, fault_already_reported, terminal_fault, announce_clean)`.
type RetiredRecording = (RecordingTap, bool, Option<String>, bool);

/// One Channel's stream pipeline (§102). Driven synchronously via
/// [`ChannelPipeline::ingest`]; [`run_channel`] is the async loop around it.
pub struct ChannelPipeline {
    channel_id: ChannelId,
    /// Opaque identity for this pipeline run. Stream offsets restart at zero for
    /// every fresh pipeline, so the GUI needs an identity alongside the offset to
    /// reject stale deltas without confusing them with a restart.
    stream_generation: u64,
    /// Display Views (§48): v2.1 is one logical view per Channel (ADR-023) —
    /// the first entry, created by `new`, is *the* view. The `Vec` shape is
    /// retained latitude for the deferred multi-view feature (Appendix A);
    /// entries beyond the first are ignored by pause gating and the GUI.
    display_views: Vec<PipelineDisplayView>,
    /// Retained diagnostics, count-limited per type (§88).
    diagnostics: DiagnosticLog,
    /// Last shared diagnostics snapshot and the log revision it was built from,
    /// so repeated polls of an unchanged log cost an `Arc` clone (§124).
    diagnostics_cache: Option<(u64, Arc<DiagnosticsSnapshot>)>,
    /// Raw Recording handle (§53). `None` when recording is disabled or its
    /// enable failed (§55). Faults non-blockingly on overflow (§56.1).
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
    /// Queue high-water mark retained after a Raw recorder is finalized or
    /// restarted. The active handle owns the live counter; this preserves its
    /// run-level evidence after that handle is gone.
    raw_recording_queue_history: Option<QueueDepth>,
    /// Whether the raw recorder's fault has already been reported.
    recording_fault_reported: bool,
    /// Whether a display recorder's fault has already been reported —
    /// `recording_fault_reported`'s Display sibling (§54 parity).
    display_fault_reported: bool,
    /// A `begin_recording` that failed to even open the file leaves no recorder, so the
    /// recording state would otherwise read back as "off". This sticky flag makes it
    /// read `Faulted` instead (so the GUI shows ⚠, not ■). Cleared on a successful
    /// begin or a stop.
    begin_faulted: bool,
    /// Display-recording sibling of `begin_faulted` (§54, ADR-012).
    display_begin_faulted: bool,
    /// Display-recording sibling of `recording_settings`: the settings a live
    /// Display begin uses (§54). `None` = no destination set, so a begin faults.
    display_recording_settings: Option<DisplayRecordingSettings>,
    /// Disk-space guard for recording (§56.2, §168): the policy and the path whose
    /// filesystem free space is polled. `None` = no guard.
    disk_guard: Option<(DiskGuard, PathBuf)>,
    /// Whether the low-disk condition has already been reported (debounce).
    disk_low_reported: bool,
    /// Per-Channel liveness facts (§91.1): rolling throughput + last-data time.
    activity: ActivityMeter,
    /// Post-read capture to pipeline-start delay, accumulated without retaining
    /// individual samples.
    ingest_delay: DurationHistogram,
    recent_ingest_delay: RecentDurationHistogram,
    /// Pipeline-start through completion of synchronous per-chunk work.
    ingest_processing: DurationHistogram,
    recent_ingest_processing: RecentDurationHistogram,
    /// Cumulative read-chunk sizes and post-read completion gaps for this run.
    chunk_shape: ChunkShape,
    last_read_completed_at: Option<Instant>,
    transport_health: TransportHealth,
    /// Reader-owned Serial stall truth. `None` for non-Serial transports.
    serial_stall_state: Option<SerialStallState>,
    /// Idle-rule firing lateness against each rule's monotonic deadline.
    rule_timer_lateness: DurationHistogram,
    recent_rule_timer_lateness: RecentDurationHistogram,
    idle_deadline_timer: IdleDeadlineTimerSummary,
    events: Option<Sender<RuntimeEvent>>,
    /// Compiled find/trigger rules (§50.2, §165); empty when none are configured.
    match_rules: MatchRuleSet,
    /// Bounded log of recent rule firings, surfaced in the snapshot (§165).
    recent_matches: DropOldestQueue<TriggeredMatch>,
    /// `Record` actions queued by rule evaluation, applied asynchronously by
    /// [`apply_pending_records`](Self::apply_pending_records) (file I/O is async).
    pending_record_controls: Vec<PendingRecord>,
    /// Recorder stops detached from the acquisition loop (§56, and §142's
    /// never-block-the-producer rule): each task drains its recorder's
    /// accepted backlog and finalizes off the pipeline task, yielding the
    /// arguments for [`note_recording_stop`](Self::note_recording_stop).
    /// `run_channel` reaps completions between reads; `begin_*` and `finish`
    /// drain first, so a new file never opens while its predecessor is still
    /// closing and a channel stop still reports every outcome (§56.1).
    retiring: JoinSet<RetiredRecording>,
    /// The Raw recording settings used to build the recording on demand (§50.2,
    /// lazy-create: nothing on disk until a `Begin` fires). `None` = no destination
    /// set, so a `Begin` can't record.
    recording_settings: Option<RawRecordingSettings>,
    /// "Record on start" (§53): when set, `run_channel` calls `begin_recording` once at
    /// startup. Begin goes through the same path as the live Record toggle, so a start
    /// failure (e.g. Refuse over an existing file) records a diagnostic and emits
    /// `RecordingFaulted` — it doesn't fail silently.
    auto_begin_recording: bool,
    /// Anchor for the `Idle` condition before any data has arrived (§50.2): idle is
    /// measured from the last data, or from this instant when none has arrived yet.
    created_at: Instant,
    /// The stream scrollback (§87): the most recent received bytes, verbatim.
    /// Trimmed from the front to `stream_cap` bytes.
    stream_buf: VecDeque<u8>,
    stream_cap: usize,
    /// Total bytes evicted from the front of `stream_buf` since Start. The absolute
    /// stream offset of `stream_buf[0]` is exactly this value, so a consumer holding
    /// an absolute cursor can ask for "bytes since N" and we can locate N in the ring
    /// (or tell it its cursor was evicted). `stream_dropped + stream_buf.len()` is the
    /// absolute end offset.
    stream_dropped: u64,
    /// Ingest queue occupancy (§99) reported by `run_channel` each loop turn: the most
    /// recent depth/capacity and the high-water mark since Start. Surfaced in
    /// `ChannelStats.ingest_queue` for stress testing — a rising `peak` is the first
    /// sign reception is outrunning the pipeline.
    ingest_depth: usize,
    ingest_peak: usize,
    ingest_capacity: usize,
}

/// A `Record` action queued for asynchronous application (§50.2).
struct PendingRecord {
    target: RecordTarget,
    control: RecordControl,
}

/// The Raw recording settings needed to build the recording when a `Record { Begin }`
/// fires (§50.2, §165) — the destination/overwrite/timestamps/rotation, packaged so
/// they can be sent to the pipeline task (which runs on its own async task and can't
/// read the config directly). Built from the channel's recording config, either at
/// channel start or — for a live toggle — read from the editor at click time (ADR-012).
#[derive(Clone, Debug)]
pub struct RawRecordingSettings {
    pub destination: PathBuf,
    pub channel_name: String,
    pub overwrite: OverwritePolicy,
    pub timestamps: bool,
    pub file_rotation: FileRotationPolicy,
    pub capacity: usize,
}

/// The Display recording settings for a live begin (§54, ADR-012/-013): the Raw
/// fields' sibling, plus the renderer — the `.disp` records a *view's* rendered
/// output, so the begin must know how that view renders. Packaged like
/// [`RawRecordingSettings`] so the pipeline task never reads config directly.
#[derive(Clone, Debug)]
pub struct DisplayRecordingSettings {
    pub destination: PathBuf,
    pub channel_name: String,
    pub overwrite: OverwritePolicy,
    pub file_rotation: FileRotationPolicy,
    pub capacity: usize,
    pub renderer: DisplayView,
}

/// Bound on the retained recent-match log (§165) — generous but constant (§124).
const RECENT_MATCHES_CAP: usize = 256;
static NEXT_STREAM_GENERATION: AtomicU64 = AtomicU64::new(1);

impl ChannelPipeline {
    pub fn new(channel_id: ChannelId, caps: PipelineCapacities) -> Self {
        Self {
            channel_id,
            stream_generation: NEXT_STREAM_GENERATION.fetch_add(1, Ordering::Relaxed),
            display_views: vec![PipelineDisplayView::new()],
            diagnostics: DiagnosticLog::new(
                caps.event_retention,
                caps.warning_retention,
                caps.error_retention,
            ),
            diagnostics_cache: None,
            raw_recorder: None,
            raw_recording_queue_history: None,
            recording_fault_reported: false,
            display_fault_reported: false,
            begin_faulted: false,
            display_begin_faulted: false,
            display_recording_settings: None,
            disk_guard: None,
            disk_low_reported: false,
            activity: ActivityMeter::new(),
            ingest_delay: DurationHistogram::default(),
            recent_ingest_delay: RecentDurationHistogram::default(),
            ingest_processing: DurationHistogram::default(),
            recent_ingest_processing: RecentDurationHistogram::default(),
            chunk_shape: ChunkShape::default(),
            last_read_completed_at: None,
            transport_health: TransportHealth::default(),
            serial_stall_state: None,
            rule_timer_lateness: DurationHistogram::default(),
            recent_rule_timer_lateness: RecentDurationHistogram::default(),
            idle_deadline_timer: IdleDeadlineTimerSummary::default(),
            events: None,
            match_rules: MatchRuleSet::compile(&[]),
            recent_matches: DropOldestQueue::with_capacity(RECENT_MATCHES_CAP),
            pending_record_controls: Vec::new(),
            retiring: JoinSet::new(),
            recording_settings: None,
            auto_begin_recording: false,
            created_at: Instant::now(),
            stream_buf: VecDeque::new(),
            stream_cap: caps.stream_display,
            stream_dropped: 0,
            ingest_depth: 0,
            ingest_peak: 0,
            ingest_capacity: caps.ingest,
        }
    }

    /// Record the ingest queue's occupancy (§99), tracking the high-water mark — called
    /// by `run_channel` each loop turn with the live `Receiver` depth/capacity. The
    /// receiver lives in `run_channel`, not here, so it is pushed in rather than polled.
    pub fn record_ingest_depth(&mut self, current: usize, capacity: usize) {
        self.ingest_depth = current;
        self.ingest_capacity = capacity;
        self.ingest_peak = self.ingest_peak.max(current);
    }

    /// Attach compiled find/trigger rules (§50.2, §165). `BytePattern` rules are
    /// evaluated per chunk; `Idle` via a timer. Actions are presentation/control only.
    pub fn with_match_rules(mut self, rules: &[MatchRule]) -> Self {
        self.match_rules = MatchRuleSet::compile(rules);
        self
    }

    /// Seed the diagnostics log with the previous run's entries so a restarted Channel
    /// keeps its log across a stop/start within a session (§88). Bounded by the same
    /// per-severity caps. Call before the pipeline records anything new.
    pub fn with_prior_diagnostics(mut self, prior: Vec<Diagnostic>) -> Self {
        self.diagnostics.seed(prior);
        self
    }

    /// Provide the Raw recording settings (§50.2): the recorder is created only when a
    /// `Record { Begin }` action fires, so nothing is written before then.
    pub fn with_recording_settings(mut self, settings: RawRecordingSettings) -> Self {
        self.recording_settings = Some(settings);
        self
    }

    /// Provide the Display recording settings (§50.2/§54) — the Raw sibling: a
    /// match-triggered `Record { target: Display | Both, Begin }` lazily creates
    /// the `.disp` from these; nothing is written before a firing.
    pub fn with_display_recording_settings(mut self, settings: DisplayRecordingSettings) -> Self {
        self.display_recording_settings = Some(settings);
        self
    }

    /// Mark "Record on start" (§53): `run_channel` begins recording once at startup,
    /// via the same path as the live toggle. Pair with `with_recording_settings`.
    pub fn with_auto_begin_recording(mut self) -> Self {
        self.auto_begin_recording = true;
        self
    }

    /// Whether "Record on start" was set (consumed by `run_channel` at startup).
    pub fn should_auto_begin_recording(&self) -> bool {
        self.auto_begin_recording
    }

    /// Attach a Raw Recording handle (§53). The orchestrator creates it at Start
    /// (the file open is async and may fail per §55); the pipeline only feeds and
    /// finalizes it.
    pub fn with_raw_recorder(mut self, recorder: Recording<Arc<ReceivedData>>) -> Self {
        self.raw_recorder = Some(recorder);
        self
    }

    /// Attach a disk-space guard (§56.2, §168): `path`'s filesystem free space is
    /// polled, and on a low condition the guard warns and, per its policy, stops
    /// recording.
    pub fn with_disk_guard(mut self, guard: DiskGuard, path: PathBuf) -> Self {
        self.disk_guard = Some((guard, path));
        self
    }

    /// Attach a runtime event sender (§137). Events are advisory, so emission is
    /// non-blocking and drops on a full event channel — the authoritative record
    /// lives in recording/diagnostics, not the event stream.
    pub fn with_event_sender(mut self, events: Sender<RuntimeEvent>) -> Self {
        self.events = Some(events);
        self
    }

    /// Attach the Serial reader's per-run authoritative stall state.
    pub(crate) fn with_serial_stall_state(mut self, state: SerialStallState) -> Self {
        self.serial_stall_state = Some(state);
        self
    }

    /// Process one received chunk through the pipeline (§102).
    ///
    /// Distribution order follows §99.1: the raw recorder is offered the chunk
    /// first with a non-blocking enqueue, then the stream scrollback, display
    /// recording, and find/trigger evaluation. Every edge is non-blocking.
    pub fn ingest(&mut self, data: ReceivedData) {
        let ingest_started = Instant::now();
        let ingest_delay = ingest_started.saturating_duration_since(data.received_at.monotonic);
        self.ingest_delay.record(ingest_delay);
        self.recent_ingest_delay
            .record_at(ingest_started, ingest_delay);
        self.transport_health
            .arrival_timestamps
            .record(data.received_at.wall_clock_source);
        self.chunk_shape.sizes.record(data.payload.bytes().len());
        if let Some(previous) = self
            .last_read_completed_at
            .replace(data.received_at.monotonic)
        {
            self.chunk_shape.inter_read_gaps.record(
                data.received_at
                    .monotonic
                    .saturating_duration_since(previous),
            );
        }
        let data = Arc::new(data);
        // The chunk's start offset in the stream (total bytes before it) — the
        // anchor for find/trigger firings (§50.2: byte offsets, not numbers).
        let chunk_offset = self.activity.total_bytes();

        // Liveness (§91.1): count received bytes at the chunk's arrival time.
        self.activity
            .record_chunk(data.received_at.monotonic, data.payload.bytes().len());
        // Data arrived: re-arm any `Idle` rule so it can fire again on the next
        // quiet episode (§50.2). Cheap no-op when there are no idle rules.
        self.match_rules.note_activity();

        // 1. Raw recorder tap (§53). Non-blocking: a full recorder queue faults
        // the recording rather than stalling reception (§56.1). `try_record`
        // no-ops once faulted; faults are reported (once) by the
        // `check_recording_faults` call at the end of this method.
        if let Some(recorder) = self.raw_recorder.as_mut() {
            recorder.try_record(Arc::clone(&data));
        }

        // 2. Stream scrollback (§87): keep the most recent bytes exactly as
        // received — the wire, regardless of read-chunk boundaries. A byte ring,
        // trimmed from the front. Honors the default view's pause (§50): a paused
        // view freezes its display while reception and recording keep going.
        let bytes = data.payload.bytes();
        let view_paused = self
            .display_views
            .first()
            .is_some_and(|v| v.handle.is_paused());
        if !view_paused {
            if bytes.len() >= self.stream_cap {
                // A single chunk already exceeds the cap: keep only its tail. Every
                // currently-buffered byte plus the dropped prefix of this chunk is
                // evicted; account for all of it in the absolute offset.
                let dropped = self.stream_buf.len() as u64 + (bytes.len() - self.stream_cap) as u64;
                self.stream_dropped += dropped;
                self.stream_buf.clear();
                self.stream_buf
                    .extend(bytes[bytes.len() - self.stream_cap..].iter().copied());
            } else {
                self.stream_buf.extend(bytes.iter().copied());
                let overflow = self.stream_buf.len().saturating_sub(self.stream_cap);
                if overflow > 0 {
                    self.stream_buf.drain(..overflow);
                    self.stream_dropped += overflow as u64;
                }
            }
        }

        // Where chunk[0] sits in **view (scrollback) space** after the append —
        // the space `StreamDelta` offsets live in. `None` while the view is
        // paused: those bytes never enter the view, so a firing on them has no
        // view position (§50). Stream offsets (`chunk_offset`, from the activity
        // total) count *every* byte, so the two spaces diverge after any pause;
        // firings are translated so the live viewer's splice stays aligned.
        let view_chunk_base = (!view_paused).then(|| self.stream_end_offset() - bytes.len() as u64);

        // 3. Find/triggers (§50.2): evaluate `BytePattern` rules against this chunk
        // **before** rendering for Display Recording, so a `Mark` timestamp can be
        // spliced into this same chunk's `.disp` render. Matching spans earlier
        // chunks' boundaries via the rule set's carry; each firing carries its true
        // match start offset (which may fall in an earlier chunk for a boundary split).
        let mark_annotations = if !self.match_rules.is_empty() {
            let fired = self.match_rules.evaluate_stream(bytes, chunk_offset);
            if fired.is_empty() {
                Vec::new()
            } else {
                self.apply_fired_rules(fired, data.received_at, view_chunk_base, chunk_offset)
            }
        } else {
            Vec::new()
        };

        // 4. Display Recording (§54, §58): render this chunk per recording view and
        // record it — regardless of pause (pausing presentation never pauses
        // recording). Non-blocking; a full queue faults that recording only. Mark
        // timestamps for this chunk are spliced inline at their within-chunk offset
        // (§50.2) — the `.disp` mirrors what the live display shows, never `.raw`.
        let channel_id = self.channel_id;
        let chunk_marks =
            render_annotations_for_chunk(&mark_annotations, chunk_offset, bytes.len());
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.as_mut() {
                // Streaming render (ADR-018): the concatenated .disp is the exact
                // rendered stream — read boundaries leave no trace. Possibly
                // empty (a chunk held back as an incomplete multi-byte tail).
                // The chunk's arrival time rides on the rendered output so a
                // rotating display recorder picks its period file from arrival,
                // not write time (§59) — the two differ under a backlog.
                let text = rec.renderer.render_chunk(bytes, &chunk_marks);
                if !text.is_empty() {
                    rec.recording.try_record(RenderedOutput {
                        channel_id,
                        text,
                        timestamp: Some(data.received_at),
                    });
                }
            }
        }

        // 5. Surface any recorder fault (raw or display) exactly once (§56.1).
        self.check_recording_faults();

        let ingest_finished = Instant::now();
        let processing = ingest_finished.saturating_duration_since(ingest_started);
        self.ingest_processing.record(processing);
        self.recent_ingest_processing
            .record_at(ingest_finished, processing);
    }

    /// Report any recorder fault once: an error diagnostic carrying the
    /// recorder's terminal error, plus a `RecordingFaulted` event (§56.1, §137).
    ///
    /// Called after every ingest *and* from `run_channel`'s periodic tick — the
    /// recorder task dies asynchronously (write/flush failure), so on a stream
    /// that then goes quiet there is no later enqueue to trip on; without the
    /// periodic check the UI would show a dead recording as Enabled forever.
    pub fn check_recording_faults(&mut self) {
        if !self.recording_fault_reported
            && self
                .raw_recorder
                .as_ref()
                .is_some_and(|r| r.state() == RecordingState::Faulted)
        {
            self.recording_fault_reported = true;
            let why = self
                .raw_recorder
                .as_ref()
                .and_then(|r| r.fault_error())
                .unwrap_or("recorder task ended unexpectedly")
                .to_owned();
            self.diagnostics.record(Diagnostic::error(format!(
                "raw recording faulted on channel {}: {why}",
                self.channel_id
            )));
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::RecordingFaulted(
                    self.channel_id,
                    RecordingTap::Raw,
                ));
            }
        }
        let display_fault = self.display_views.iter().find_map(|v| {
            v.recorder
                .as_ref()
                .filter(|r| r.recording.state() == RecordingState::Faulted)
                .map(|r| {
                    r.recording
                        .fault_error()
                        .unwrap_or("recorder task ended unexpectedly")
                        .to_owned()
                })
        });
        if let Some(why) = display_fault {
            if !self.display_fault_reported {
                self.display_fault_reported = true;
                self.diagnostics.record(Diagnostic::error(format!(
                    "display recording faulted on channel {}: {why}",
                    self.channel_id
                )));
                if let Some(events) = &self.events {
                    let _ = events.try_send(RuntimeEvent::RecordingFaulted(
                        self.channel_id,
                        RecordingTap::Display,
                    ));
                }
            }
        }
    }

    /// Apply the actions of every rule that fired (§50.2). Synchronous actions —
    /// `Notify`, `Mark`, `PauseDisplay` — take effect immediately;
    /// `Record` actions are queued for asynchronous application (file I/O). Every
    /// firing is observable: it is logged for the snapshot and emits a
    /// `MatchTriggered` event (§137). Each firing carries its own byte offset
    /// (`None` for an idle firing); a boundary-split firing also records the
    /// where/why measurement diagnostic.
    ///
    /// Returns the inline **Mark annotations** produced this chunk (absolute
    /// stream offsets), which the caller splices into the `.disp` render. `arrival` is
    /// the chunk's arrival time — the source of a Mark timestamp (§50.2).
    /// `view_base`/`chunk_offset` translate each firing's stream offset into view
    /// (scrollback) space for the live viewer (`view_base` = chunk[0]'s view offset,
    /// `None` while the view is paused; idle callers pass `None`/`0`).
    fn apply_fired_rules(
        &mut self,
        fired: Vec<FiredRule>,
        arrival: ChunkTime,
        view_base: Option<u64>,
        chunk_offset: u64,
    ) -> Vec<MarkAnnotation> {
        let mut annotations = Vec::new();
        for rule in fired {
            let byte_offset = rule.match_offset;
            // The match-start byte's position in view space (§50): stream offsets count
            // every received byte, but the view skips bytes that arrived while
            // paused, so the spaces diverge after any pause — translate here so the
            // live viewer's Mark splice stays aligned. A boundary-split match can
            // start before the chunk (checked_sub guards the pre-window edge).
            let view_offset = match (byte_offset, view_base) {
                (Some(m), Some(base)) => (base + m).checked_sub(chunk_offset),
                _ => None,
            };
            let mut mark_render = None;
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::MatchTriggered(self.channel_id, rule.id));
            }
            // Measurement (§50.2): when a match was a cross-chunk boundary split,
            // record *where* (the stream offset) and *why* (the split) so an
            // operator can see that read-chunk boundaries are splitting patterns —
            // and, via `boundary_saves` in the snapshot, *how often*. An info-level
            // diagnostic: it is a recovered match, not a fault.
            if rule.boundary_split {
                let at = byte_offset
                    .map(|n| format!(" at stream offset {n}"))
                    .unwrap_or_default();
                self.diagnostics.record(Diagnostic::event(format!(
                    "match rule {} on channel {} spanned a read-chunk boundary{at} \
                     (recovered by cross-chunk carry)",
                    rule.id, self.channel_id
                )));
            }
            for action in &rule.actions {
                match action {
                    MatchAction::Notify { severity } => {
                        let where_ = byte_offset
                            .map(|n| format!(" (stream offset {n})"))
                            .unwrap_or_default();
                        self.diagnostics.record(Diagnostic::new(
                            *severity,
                            format!("match rule fired on channel {}{where_}", self.channel_id),
                        ));
                    }
                    // A timestamped Mark splices either compact local time or a ZDA
                    // annotation; a bare Mark writes the `‹MARK …›` marker line
                    // (§50.2). Both only ever touch display and `.disp`, never `.raw`.
                    MatchAction::Mark {
                        timestamp: Some(ts),
                    } => {
                        if let Some(offset) = byte_offset {
                            let before = matches!(ts.position, MarkPosition::Before);
                            let anchor_offset = if before {
                                Some(offset)
                            } else {
                                offset.checked_add(rule.match_len.saturating_sub(1) as u64)
                            };
                            let rendered = match &ts.style {
                                MarkTimestampStyle::Plain => ts.format.format(arrival.wall_clock),
                                MarkTimestampStyle::NmeaZda { talker } => {
                                    match zda_sentence(
                                        talker,
                                        arrival.wall_clock,
                                        ts.format.include_millis,
                                    ) {
                                        Ok(text) => text,
                                        Err(error) => {
                                            self.diagnostics.record(Diagnostic::error(format!(
                                                "could not render NMEA ZDA Mark on channel {}: {error}",
                                                self.channel_id
                                            )));
                                            continue;
                                        }
                                    }
                                }
                            };
                            // The configured separator is verbatim and may include
                            // line breaks; StreamRenderer updates its row/column state
                            // while splicing it into every display mode.
                            let text = format!("{rendered}{}", ts.separator);
                            if let Some(anchor_offset) = anchor_offset {
                                let anchor_view_offset = view_base.and_then(|base| {
                                    (base + anchor_offset).checked_sub(chunk_offset)
                                });
                                annotations.push(MarkAnnotation {
                                    offset: anchor_offset,
                                    before,
                                    text: text.clone(),
                                });
                                mark_render = Some(MarkRender {
                                    text,
                                    before,
                                    view_offset: anchor_view_offset,
                                });
                            }
                        }
                    }
                    MatchAction::Mark { timestamp: None } => self.write_mark(rule.id, byte_offset),
                    MatchAction::PauseDisplay { view } => self.pause_views(*view),
                    MatchAction::Record { target, control } => {
                        self.pending_record_controls.push(PendingRecord {
                            target: *target,
                            control: *control,
                        });
                    }
                }
            }
            self.recent_matches.push(TriggeredMatch {
                rule_id: rule.id,
                byte_offset,
                view_offset,
                mark: mark_render,
            });
        }
        annotations
    }

    /// `Mark` action (§50.2): drop a correlation marker into every Display View's
    /// Display Recording (`.disp`) — **never** the raw `.raw` stream, which stays
    /// byte-exact (§5.6/§49). The `MatchTriggered` event and `recent_matches` log
    /// (written by the caller) are the marker's display/event surfaces.
    fn write_mark(&mut self, rule_id: MatchRuleId, byte_offset: Option<u64>) {
        let suffix = byte_offset
            .map(|n| format!(" offset={n}"))
            .unwrap_or_default();
        // Framed with explicit newlines: the recorder appends verbatim now
        // (ADR-018), so the marker provides its own line breaks.
        let text = format!("\n\u{2039}MARK rule={rule_id}{suffix}\u{203a}\n");
        let channel_id = self.channel_id;
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.as_mut() {
                rec.recording.try_record(RenderedOutput {
                    channel_id,
                    text: text.clone(),
                    timestamp: None,
                });
            }
        }
    }

    /// `PauseDisplay` action (§50.2, §50): freeze one Display View by index, or all
    /// views when `None`. Reception and recording continue (§58).
    fn pause_views(&mut self, view: Option<usize>) {
        match view {
            Some(idx) => {
                if let Some(v) = self.display_views.get(idx) {
                    v.handle.pause();
                }
            }
            None => {
                for v in &self.display_views {
                    v.handle.pause();
                }
            }
        }
    }

    /// Idle rules (§50.2): fire any whose quiet-time has reached its timeout.
    /// `now` anchors "time since last data" (or since channel start before any data
    /// arrives). A no-op when no idle rule is configured.
    pub fn evaluate_idle_rules(&mut self, now: Instant) {
        if !self.match_rules.has_idle_rule() {
            return;
        }
        let last = self.activity.last_data_at().unwrap_or(self.created_at);
        let idle_for = now.saturating_duration_since(last);
        let fired = self.match_rules.evaluate_idle(idle_for);
        if !fired.is_empty() {
            for lateness in fired.iter().filter_map(|rule| rule.timer_lateness) {
                self.rule_timer_lateness.record(lateness);
                self.recent_rule_timer_lateness.record_at(now, lateness);
            }
            // Idle firings carry no byte offset, so they produce no inline Mark
            // timestamp (and no view offset); the returned annotations are empty.
            let _ = self.apply_fired_rules(fired, ChunkTime::now(), None, 0);
        }
    }

    /// Earliest pending Idle-rule deadline for the current quiet episode.
    pub fn next_idle_deadline(&self, now: Instant) -> Option<Instant> {
        if !self.match_rules.has_idle_rule() {
            return None;
        }
        let last = self.activity.last_data_at().unwrap_or(self.created_at);
        let idle_for = now.saturating_duration_since(last);
        let wait = self.match_rules.next_idle_wait(idle_for)?;
        now.checked_add(wait)
    }

    /// Apply queued `Record` actions (§50.2). Called from the async ingest loop,
    /// since recorder creation/finalization is async. `Raw` drives the byte-exact
    /// recorder, `Display` the rendered `.disp`, `Both` drives both — each through
    /// the same lazy begin / clean finalize paths as the live toggles (ADR-012),
    /// so a missing destination faults with a clear diagnostic, never silently.
    pub async fn apply_pending_records(&mut self) {
        if self.pending_record_controls.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_record_controls);
        for req in pending {
            let raw = matches!(req.target, RecordTarget::Raw | RecordTarget::Both);
            let display = matches!(req.target, RecordTarget::Display | RecordTarget::Both);
            match req.control {
                RecordControl::Begin => {
                    if raw {
                        self.begin_recording().await;
                    }
                    if display {
                        self.begin_display_recording().await;
                    }
                }
                RecordControl::Stop => {
                    if raw {
                        self.stop_recording().await;
                    }
                    if display {
                        self.stop_display_recording().await;
                    }
                }
            }
        }
    }

    /// Begin or stop Raw recording live, without a restart (§50.2, ADR-012). The
    /// manual counterpart of the match-rule `Record` action: it drives the same lazy
    /// begin / clean finalize path, so a user-toggled recording and a rule-triggered
    /// one are byte-identical and share the idempotency rules.
    ///
    /// `settings`, when present, replace the pipeline's recording settings first — so
    /// the live toggle records to whatever the editor showed at click time, with no
    /// restart. They're only swapped in while not actively recording (including while
    /// faulted), so a begin can't change the destination out from under an open file.
    pub async fn set_recording(&mut self, enabled: bool, settings: Option<RawRecordingSettings>) {
        if let Some(settings) = settings {
            if self
                .raw_recorder
                .as_ref()
                .is_none_or(|recorder| recorder.state() == RecordingState::Faulted)
            {
                self.recording_settings = Some(settings);
            }
        }
        if enabled {
            self.begin_recording().await;
        } else {
            self.stop_recording().await;
        }
    }

    /// Lazily create the Raw recording on a `Begin` (§50.2): nothing is on disk until
    /// now. A no-op if a recording is already active; if no destination is set it
    /// reports a fault rather than silently doing nothing; an open failure (e.g. an
    /// existing file under a Refuse policy) reports the reason without faulting the
    /// Channel (§55).
    async fn begin_recording(&mut self) {
        // A predecessor's detached stop must fully land first: a begin to the
        // same destination would otherwise race the closing file.
        self.drain_retiring().await;
        match self.raw_recorder.as_ref().map(|r| r.state()) {
            // Already recording — `Begin` is idempotent.
            Some(state) if state != RecordingState::Faulted => return,
            // A faulted recording still occupies the slot, and Begin is the
            // user's retry (the button reads "Record" — it must work without
            // a channel restart). Drop the dead recording, then recreate.
            Some(_) => {
                if let Some(recorder) = self.take_raw_recorder() {
                    let _ = recorder.finalize(RecordingStopReason::Disabled).await;
                }
            }
            None => {}
        }
        let Some(settings) = self.recording_settings.clone() else {
            // No destination set — a Begin can't record. Surface it instead of a
            // silent no-op (the live toggle would otherwise appear to do nothing).
            self.begin_faulted = true;
            self.diagnostics.record(Diagnostic::error(
                "can't begin Raw recording: no destination is set — set one in the Raw \
                 recording setup, then press Record again",
            ));
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::RecordingFaulted(
                    self.channel_id,
                    RecordingTap::Raw,
                ));
            }
            return;
        };
        let created = if settings.file_rotation == FileRotationPolicy::None {
            RawFileRecorder::create(
                &settings.destination,
                settings.overwrite,
                settings.timestamps,
            )
            .await
            .map(|r| start_raw_recording(r, settings.capacity))
        } else {
            RotatingRawRecorder::create(
                &settings.destination,
                &settings.channel_name,
                ".raw",
                settings.overwrite,
                settings.timestamps,
                settings.file_rotation,
            )
            .await
            .map(|r| start_raw_recording(r, settings.capacity))
        };
        match created {
            Ok(rec) => {
                self.raw_recorder = Some(rec);
                self.recording_fault_reported = false;
                self.begin_faulted = false; // a successful begin clears the prior fault
                                            // Positive feedback: record an INFO so the headline becomes "recording
                                            // started" (pushing a prior begin-error off the headline) and the user
                                            // sees the begin actually took.
                self.diagnostics.record(Diagnostic::event(format!(
                    "Raw recording started → {}",
                    settings.destination.display(),
                )));
                if let Some(events) = &self.events {
                    let _ = events.try_send(RuntimeEvent::RecordingStarted(
                        self.channel_id,
                        RecordingTap::Raw,
                    ));
                }
            }
            Err(err) => {
                self.begin_faulted = true;
                // Record *why* the begin failed (e.g. Refuse over an existing file) in
                // the diagnostic log, and signal it as a recording fault so the GUI can
                // surface it — a silent no-op left the user clicking "Record now" with
                // no feedback (§55).
                self.diagnostics.record(Diagnostic::error(format!(
                    "could not begin Raw recording to {} (on-exists: {:?}): {err}",
                    settings.destination.display(),
                    settings.overwrite,
                )));
                if let Some(events) = &self.events {
                    let _ = events.try_send(RuntimeEvent::RecordingFaulted(
                        self.channel_id,
                        RecordingTap::Raw,
                    ));
                }
            }
        }
    }

    /// Stop the Raw recording on a `Record { Stop }` (§50.2, §56): the recorder
    /// retires **detached** — draining its accepted backlog and closing the file
    /// must not stall reception (§142's never-block-the-producer rule; a slow
    /// disk here used to back the bounded ingest queue up into the transport).
    /// The stop outcome is reported when the retirement completes. A no-op if
    /// none is active. Also clears a prior begin-fault so the state reads
    /// "off" again, not ⚠.
    async fn stop_recording(&mut self) {
        self.begin_faulted = false;
        if let Some(recorder) = self.take_raw_recorder() {
            let already_reported = self.recording_fault_reported;
            self.retiring.spawn(async move {
                let fault = recorder.finalize(RecordingStopReason::Disabled).await;
                (RecordingTap::Raw, already_reported, fault, true)
            });
        }
    }

    /// Report one detached retirement's outcome (see `retiring`).
    fn note_retired_recording(
        &mut self,
        retired: Result<RetiredRecording, tokio::task::JoinError>,
    ) {
        match retired {
            Ok((tap, already_reported, fault, announce_clean)) => {
                self.note_recording_stop(tap, already_reported, fault, announce_clean);
            }
            // A panicked finalize task cannot report which tap it served;
            // don't let it pass as a clean stop silently (§56.1).
            Err(err) => {
                self.diagnostics.record(Diagnostic::error(format!(
                    "a recording finalize task failed: {err} — the file's tail may be lost"
                )));
            }
        }
    }

    /// Non-blockingly reap every **completed** detached retirement and report
    /// its outcome. `run_channel` calls this each loop pass — the idle tick
    /// guarantees a pass even on a quiet stream, so an outcome's note lands
    /// within one tick without reception ever waiting on a file close.
    pub fn reap_retired_recordings(&mut self) {
        while let Some(retired) = self.retiring.try_join_next() {
            self.note_retired_recording(retired);
        }
    }

    /// Await every in-flight retirement and report each outcome. Runs before a
    /// `begin_*` (a new file must never open while its predecessor is still
    /// closing — same-destination restarts would collide on the open file) and
    /// in `finish` (a channel stop reports every outcome before returning,
    /// §56.1).
    async fn drain_retiring(&mut self) {
        while let Some(retired) = self.retiring.join_next().await {
            self.note_retired_recording(retired);
        }
    }

    /// Record the outcome of a recording stop honestly (§56.1): a clean stop
    /// gets its INFO note (when `announce_clean`); a dirty one — accepted
    /// backlog truncated or finalize failed — gets an error diagnostic and a
    /// `RecordingFaulted` event, unless that fault was already reported live.
    fn note_recording_stop(
        &mut self,
        tap: RecordingTap,
        already_reported: bool,
        fault: Option<String>,
        announce_clean: bool,
    ) {
        let what = format!("{} recording", tap.label());
        match fault {
            None => {
                if announce_clean {
                    self.diagnostics
                        .record(Diagnostic::event(format!("{what} stopped")));
                }
            }
            Some(why) => {
                if !already_reported {
                    self.diagnostics.record(Diagnostic::error(format!(
                        "{what} faulted while stopping on channel {}: {why}",
                        self.channel_id
                    )));
                    if let Some(events) = &self.events {
                        let _ =
                            events.try_send(RuntimeEvent::RecordingFaulted(self.channel_id, tap));
                    }
                }
            }
        }
    }

    /// Begin or stop **Display** recording live, without a restart (§54, ADR-012)
    /// — the Raw toggle's sibling, driving the same lazy begin / clean finalize
    /// shape. `settings`, when present, replace the stored display settings first;
    /// they're only swapped in while not actively recording (including while
    /// faulted), so a begin can't change the destination out from under an open file.
    pub async fn set_display_recording(
        &mut self,
        enabled: bool,
        settings: Option<DisplayRecordingSettings>,
    ) {
        if let Some(settings) = settings {
            if self
                .display_views
                .first()
                .and_then(|view| view.recorder.as_ref())
                .is_none_or(|recorder| recorder.recording.state() == RecordingState::Faulted)
            {
                self.display_recording_settings = Some(settings);
            }
        }
        if enabled {
            self.begin_display_recording().await;
        } else {
            self.stop_display_recording().await;
        }
    }

    /// Lazily create the Display recording on a begin (§54, §55): a no-op if one
    /// is already active; no destination reports a fault rather than a silent
    /// no-op; an open failure reports the reason without faulting the Channel.
    async fn begin_display_recording(&mut self) {
        // Same rule as the Raw begin: land any detached predecessor stop
        // before opening a file it might still hold.
        self.drain_retiring().await;
        match self
            .display_views
            .first()
            .and_then(|v| v.recorder.as_ref())
            .map(|r| r.recording.state())
        {
            // Already recording — begin is idempotent.
            Some(state) if state != RecordingState::Faulted => return,
            // Faulted: Begin is the retry — drop the dead recording first
            // (the Raw sibling's rule).
            Some(_) => {
                if let Some(rec) = self
                    .display_views
                    .first_mut()
                    .and_then(|v| v.recorder.take())
                {
                    let _ =
                        finalize_view_recorder(self.channel_id, rec, RecordingStopReason::Disabled)
                            .await;
                }
            }
            None => {}
        }
        let Some(settings) = self.display_recording_settings.clone() else {
            self.display_begin_faulted = true;
            self.diagnostics.record(Diagnostic::error(
                "can't begin Display recording: no destination is set — set one in the \
                 Record Display setup, then press Record again",
            ));
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::RecordingFaulted(
                    self.channel_id,
                    RecordingTap::Display,
                ));
            }
            return;
        };
        let created = if settings.file_rotation == FileRotationPolicy::None {
            DisplayFileRecorder::create(&settings.destination, settings.overwrite)
                .await
                .map(|r| start_display_recording(r, settings.capacity))
        } else {
            RotatingDisplayRecorder::create(
                &settings.destination,
                &settings.channel_name,
                ".disp",
                settings.overwrite,
                settings.file_rotation,
            )
            .await
            .map(|r| start_display_recording(r, settings.capacity))
        };
        match created {
            Ok(rec) => {
                self.set_display_recorder(settings.renderer.clone(), rec);
                self.display_fault_reported = false;
                self.display_begin_faulted = false;
                self.diagnostics.record(Diagnostic::event(format!(
                    "Display recording started → {}",
                    settings.destination.display(),
                )));
                if let Some(events) = &self.events {
                    let _ = events.try_send(RuntimeEvent::RecordingStarted(
                        self.channel_id,
                        RecordingTap::Display,
                    ));
                }
            }
            Err(err) => {
                self.display_begin_faulted = true;
                self.diagnostics.record(Diagnostic::error(format!(
                    "could not begin Display recording to {} (on-exists: {:?}): {err}",
                    settings.destination.display(),
                    settings.overwrite,
                )));
                if let Some(events) = &self.events {
                    let _ = events.try_send(RuntimeEvent::RecordingFaulted(
                        self.channel_id,
                        RecordingTap::Display,
                    ));
                }
            }
        }
    }

    /// Stop the Display recording (§54): retires **detached**, like the Raw
    /// sibling — reception and the Raw recording continue, and the acquisition
    /// loop never waits on the drain. A no-op if none is active. Clears a
    /// prior begin-fault so the state reads "off" again, not ⚠.
    async fn stop_display_recording(&mut self) {
        self.display_begin_faulted = false;
        let taken = self
            .display_views
            .first_mut()
            .and_then(|v| v.recorder.take());
        if let Some(rec) = taken {
            let already_reported = self.display_fault_reported;
            let channel_id = self.channel_id;
            self.retiring.spawn(async move {
                let fault =
                    finalize_view_recorder(channel_id, rec, RecordingStopReason::Disabled).await;
                (RecordingTap::Display, already_reported, fault, true)
            });
        }
    }

    /// Record a non-terminal transport notice (§95, §101; ADR-007). The transport
    /// states *what happened*; the pipeline — the channel's `DiagnosticLog` owner —
    /// decides how it is recorded and reported, keeping the §95 diagnostic and the
    /// §137 event paired in one place.
    pub fn record_notice(&mut self, notice: TransportNotice) {
        match notice {
            TransportNotice::ReceptionStalled {
                channel_id,
                stalled_for,
            } => {
                self.diagnostics.record(Diagnostic::warning(format!(
                    "reception stalled {} ms on channel {channel_id}; possible transport-specific \
                     loss (UART/driver overrun) — lost byte count is not observable (§101)",
                    stalled_for.as_millis(),
                )));
                // The matching event (§137); advisory, non-blocking like the rest.
                // Dedicated `ReceptionStalled` (v1.2) so observers can tell a stall
                // apart from any other warning (ADR-007).
                if let Some(events) = &self.events {
                    let _ =
                        events.try_send(RuntimeEvent::ReceptionStalled(channel_id, stalled_for));
                }
            }
            TransportNotice::UdpKernelDrops {
                channel_id: _,
                dropped,
            } => {
                self.transport_health.udp_kernel_drops = dropped.map_or(
                    CounterAvailability::Unsupported,
                    CounterAvailability::Available,
                );
            }
            TransportNotice::UdpArrivalTimestamps {
                channel_id: _,
                status,
            } => {
                self.transport_health.arrival_timestamps.status = status;
            }
            TransportNotice::TransportFaulted { channel_id, cause } => {
                // The fault's CAUSE, retained where an operator will look for
                // it (the diagnostics log, which survives stop via the
                // retained snapshot). The paired `ChannelFaulted` lifecycle
                // event (§137) is emitted by the fault monitor, not here.
                self.diagnostics.record(Diagnostic::error(format!(
                    "transport fault on channel {channel_id}: {cause}"
                )));
            }
        }
    }

    fn transport_health_at(&self, now: Instant) -> TransportHealth {
        let mut health = self.transport_health;
        if let Some(state) = &self.serial_stall_state {
            let stalls = state.snapshot();
            health.serial_stalls = Some(SerialStallSummary {
                episodes: stalls.completed_episodes,
                total: stalls.completed_total,
                max: stalls.completed_max,
                active_for: stalls
                    .active_since
                    .map(|started| now.saturating_duration_since(started)),
            });
        }
        health
    }

    /// Poll the recording filesystem's free space and act on a low condition
    /// (§56.2, §168). Called periodically (not per write). Warns once per low
    /// episode and, if the policy is `StopRecording`, finalizes and stops the
    /// recordings while reception continues (§96). A failed space query is ignored.
    pub async fn check_disk_guard(&mut self) {
        let Some((guard, path)) = self.disk_guard.clone() else {
            return;
        };
        // Only meaningful while a recording is active.
        if self.raw_recorder.is_none() && !self.display_views.iter().any(|v| v.recorder.is_some()) {
            return;
        }
        // Filesystem space queries are synchronous syscalls — off the
        // acquisition task (a hung network share would otherwise stall
        // reception), and bounded so even the blocking pool handoff can't
        // wedge the loop for more than a beat.
        let query_path = path.clone();
        let query = tokio::task::spawn_blocking(move || {
            (
                fs4::available_space(&query_path),
                fs4::total_space(&query_path),
            )
        });
        let Ok(Ok((Ok(free), Ok(total)))) =
            tokio::time::timeout(Duration::from_secs(2), query).await
        else {
            return; // cannot determine free space (or the query hung); do not act
        };
        if !disk_is_low(free, total, guard.min_free) {
            self.disk_low_reported = false;
            return;
        }
        if self.disk_low_reported {
            return; // already reported this episode
        }
        self.disk_low_reported = true;
        self.diagnostics.record(Diagnostic::warning(format!(
            "low disk for recording on channel {}: {free} bytes free",
            self.channel_id
        )));
        if let Some(events) = &self.events {
            let _ = events.try_send(RuntimeEvent::DiskSpaceLow(self.channel_id));
        }
        if guard.on_low == LowDiskAction::StopRecording {
            self.stop_all_recording().await;
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::RecordingStoppedLowDisk(self.channel_id));
            }
        }
    }

    /// Stop and drop every recording on this Channel (§56), each retiring
    /// detached (this runs from the disk guard on the acquisition task — the
    /// low-disk stop must not itself stall reception, §96).
    async fn stop_all_recording(&mut self) {
        if let Some(recorder) = self.take_raw_recorder() {
            let already = self.recording_fault_reported;
            self.retiring.spawn(async move {
                let fault = recorder.finalize(RecordingStopReason::Disabled).await;
                (RecordingTap::Raw, already, fault, false)
            });
        }
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.take() {
                let already = self.display_fault_reported;
                let channel_id = self.channel_id;
                self.retiring.spawn(async move {
                    let fault =
                        finalize_view_recorder(channel_id, rec, RecordingStopReason::Disabled)
                            .await;
                    (RecordingTap::Display, already, fault, false)
                });
            }
        }
    }

    /// Record an INFO diagnostic (§88) — a lifecycle note for the diagnostics log.
    fn record_event(&mut self, message: impl Into<String>) {
        self.diagnostics.record(Diagnostic::event(message));
    }

    /// Called at Channel stop (§110, §112): finalize the recorders — flush and
    /// close (§56). In-flight bytes already accepted by a recorder are written. The
    /// stop-time INFO notes recorded here reach the GUI because the orchestrator takes
    /// one final snapshot of the returned pipeline after this runs (see `drain_handle`).
    pub async fn finish(&mut self) {
        // Land any detached stops first: a channel stop reports every
        // recording outcome before the final snapshot is taken (§56.1).
        self.drain_retiring().await;
        if let Some(recorder) = self.take_raw_recorder() {
            let already = self.recording_fault_reported;
            let fault = recorder.finalize(RecordingStopReason::ChannelStopped).await;
            self.note_recording_stop(RecordingTap::Raw, already, fault, true);
        }
        let mut display_faults = Vec::new();
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.take() {
                display_faults.push(
                    finalize_view_recorder(
                        self.channel_id,
                        rec,
                        RecordingStopReason::ChannelStopped,
                    )
                    .await,
                );
            }
        }
        for fault in display_faults {
            let already = self.display_fault_reported;
            self.note_recording_stop(RecordingTap::Display, already, fault, false);
        }
        self.record_event("Channel stopped");
    }

    // --- Inspection (used by the runtime and tests) ---

    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Add a Display View and return its pause handle (§48). The caller keeps
    /// the handle to pause/resume the view across the pipeline-task boundary.
    pub fn add_display_view(&mut self) -> DisplayViewHandle {
        let view = PipelineDisplayView::new();
        let handle = view.handle.clone();
        self.display_views.push(view);
        handle
    }

    /// Attach a Display Recording to the primary (first) Display View (§54),
    /// rendering each chunk with `renderer`. v1 records the primary view;
    /// per-view display-recording config is deferred (Appendix A).
    pub fn set_display_recorder(
        &mut self,
        renderer: DisplayView,
        recording: Recording<RenderedOutput>,
    ) {
        if let Some(view) = self.display_views.first_mut() {
            view.recorder = Some(ViewRecorder {
                renderer: StreamRenderer::new(renderer),
                recording,
            });
        }
    }

    /// Pause/resume handles for every Display View, default first (§48).
    pub fn display_view_handles(&self) -> Vec<DisplayViewHandle> {
        self.display_views
            .iter()
            .map(|v| v.handle.clone())
            .collect()
    }

    pub fn diagnostics(&self) -> &DiagnosticLog {
        &self.diagnostics
    }

    /// Current raw-recording state (§53). `None` if no recorder is attached and none
    /// was attempted; `Some(Faulted)` if a begin failed to open the file (so the GUI
    /// shows ⚠ rather than "off" when "Record on start" couldn't start).
    pub fn raw_recording_state(&self) -> Option<RecordingState> {
        match self.raw_recorder.as_ref() {
            Some(r) => Some(r.state()),
            None if self.begin_faulted => Some(RecordingState::Faulted),
            None => None,
        }
    }

    /// Current display-recording state (§54), mirroring
    /// [`raw_recording_state`](Self::raw_recording_state): the primary view's
    /// recorder state, `Some(Faulted)` after a failed begin, `None` when off.
    pub fn display_recording_state(&self) -> Option<RecordingState> {
        match self.display_views.first().and_then(|v| v.recorder.as_ref()) {
            Some(rec) => Some(rec.recording.state()),
            None if self.display_begin_faulted => Some(RecordingState::Faulted),
            None => None,
        }
    }

    /// Cheap O(1) counters for a multi-channel overview (§91.1): liveness and
    /// timing plus per-severity diagnostic counts and the boundary-save total. Polled
    /// per-Channel each tick; [`snapshot`](Self::snapshot) (the full diagnostic/match
    /// detail) is reserved for the Channel actually on screen, and the scrollback
    /// bytes come from [`stream_delta`](Self::stream_delta) — neither call clones the
    /// scrollback.
    pub fn stats(&self) -> ChannelStats {
        let now = Instant::now();
        ChannelStats {
            // Placeholders: lifecycle is the orchestrator's, not the pipeline's —
            // `Listener::channel_stats` stamps the effective state before serving.
            state: crate::core::ChannelState::Running,
            reconnect_pending: false,
            activity: self.activity.snapshot(now),
            event_count: self.diagnostics.events().count(),
            warning_count: self.diagnostics.warnings().count(),
            error_count: self.diagnostics.errors().count(),
            raw_recording: self.raw_recording_state(),
            display_recording: self.display_recording_state(),
            match_boundary_saves: self.match_rules.boundary_saves(),
            ingest_delay: self.ingest_delay,
            recent_ingest_delay: self.recent_ingest_delay.snapshot_at(now),
            ingest_processing: self.ingest_processing,
            recent_ingest_processing: self.recent_ingest_processing.snapshot_at(now),
            chunk_shape: self.chunk_shape,
            transport_health: self.transport_health_at(now),
            rule_timer_lateness: self.rule_timer_lateness,
            recent_rule_timer_lateness: self.recent_rule_timer_lateness.snapshot_at(now),
            idle_deadline_timer: self.idle_deadline_timer,
            ingest_queue: self.ingest_queue(),
            raw_recording_queue: self.raw_recording_queue(),
        }
    }

    /// Ingest queue occupancy for the stats/snapshot views (§99).
    fn ingest_queue(&self) -> QueueDepth {
        QueueDepth {
            current: self.ingest_depth,
            peak: self.ingest_peak,
            capacity: self.ingest_capacity,
        }
    }

    /// Take the active Raw recorder while preserving its queue high-water mark.
    fn take_raw_recorder(&mut self) -> Option<Recording<Arc<ReceivedData>>> {
        if let Some(recorder) = self.raw_recorder.as_ref() {
            let (_, peak, capacity) = recorder.queue_depth();
            let retained = self.raw_recording_queue_history.get_or_insert_default();
            retained.peak = retained.peak.max(peak);
            retained.capacity = retained.capacity.max(capacity);
            retained.current = 0;
        }
        self.raw_recorder.take()
    }

    /// Raw-recording queue occupancy, retaining the run peak after a recorder is
    /// finalized or replaced (§56.1).
    fn raw_recording_queue(&self) -> Option<QueueDepth> {
        let active = self.raw_recorder.as_ref().map(|r| {
            let (current, peak, capacity) = r.queue_depth();
            QueueDepth {
                current,
                peak,
                capacity,
            }
        });
        match (self.raw_recording_queue_history, active) {
            (Some(history), Some(mut active)) => {
                active.peak = active.peak.max(history.peak);
                active.capacity = active.capacity.max(history.capacity);
                Some(active)
            }
            (history, None) => history,
            (None, active) => active,
        }
    }

    /// Build an owned, point-in-time snapshot of the *small* observable state (§137,
    /// ADR-006): per-view pause state, diagnostics, recent match firings, recording
    /// state, liveness, and the stream's end offset. The scrollback bytes are **not**
    /// included — they are fetched incrementally via [`stream_delta`](Self::stream_delta)
    /// so this stays cheap at high throughput.
    /// The retained diagnostics as a shared snapshot, rebuilt only when the log
    /// has actually changed (§88, §124).
    ///
    /// The GUI polls this several times a second for the selected Channel, for the
    /// life of a run that may last weeks, while the log itself changes rarely. The
    /// revision check turns that steady state into an `Arc` clone instead of a deep
    /// copy of every retained entry and its message string.
    fn diagnostics_snapshot(&mut self) -> Arc<DiagnosticsSnapshot> {
        let revision = self.diagnostics.revision();
        if let Some((cached_revision, cached)) = &self.diagnostics_cache {
            if *cached_revision == revision {
                return Arc::clone(cached);
            }
        }
        let rebuilt = Arc::new(DiagnosticsSnapshot {
            events: self.diagnostics.events().cloned().collect(),
            warnings: self.diagnostics.warnings().cloned().collect(),
            errors: self.diagnostics.errors().cloned().collect(),
        });
        self.diagnostics_cache = Some((revision, Arc::clone(&rebuilt)));
        rebuilt
    }

    pub fn snapshot(&mut self) -> ChannelSnapshot {
        let now = Instant::now();
        let diagnostics = self.diagnostics_snapshot();
        ChannelSnapshot {
            channel_id: self.channel_id,
            // Placeholders — the orchestrator stamps the effective lifecycle state
            // before serving (see `ChannelStats::state`).
            state: crate::core::ChannelState::Running,
            reconnect_pending: false,
            last_run_summary: None,
            display_views: self
                .display_views
                .iter()
                .map(|v| DisplayViewSnapshot {
                    id: v.handle.id,
                    paused: v.handle.is_paused(),
                })
                .collect(),
            diagnostics,
            raw_recording: self.raw_recording_state(),
            display_recording: self.display_recording_state(),
            activity: self.activity.snapshot(now),
            matches: self.recent_matches.iter().cloned().collect(),
            match_boundary_saves: self.match_rules.boundary_saves(),
            ingest_delay: self.ingest_delay,
            recent_ingest_delay: self.recent_ingest_delay.snapshot_at(now),
            ingest_processing: self.ingest_processing,
            recent_ingest_processing: self.recent_ingest_processing.snapshot_at(now),
            chunk_shape: self.chunk_shape,
            transport_health: self.transport_health_at(now),
            rule_timer_lateness: self.rule_timer_lateness,
            recent_rule_timer_lateness: self.recent_rule_timer_lateness.snapshot_at(now),
            idle_deadline_timer: self.idle_deadline_timer,
            // The scrollback bytes are fetched incrementally (StreamDelta), not
            // bundled here — only the cursor target travels in the snapshot.
            stream_end_offset: self.stream_end_offset(),
            ingest_queue: self.ingest_queue(),
            raw_recording_queue: self.raw_recording_queue(),
        }
    }

    /// Absolute stream offset just past the last retained byte (§87): total bytes
    /// accepted into the scrollback since Start.
    pub fn stream_end_offset(&self) -> u64 {
        self.stream_dropped + self.stream_buf.len() as u64
    }

    /// Incremental scrollback read (§87): the bytes at or after absolute offset
    /// `since`. Returns only what is new since the consumer's cursor — O(returned
    /// bytes), not O(buffer) — so the runtime ships just the delta and the consumer
    /// renders just the delta.
    ///
    /// If `since` is at/after the end, the delta is empty. If `since` is behind the
    /// retained window (its bytes were evicted), the whole window is returned with
    /// `base_offset > since`, signalling the consumer to reset rather than append.
    pub fn stream_delta(&self, since: u64) -> StreamDelta {
        let start = self.stream_dropped; // absolute offset of stream_buf[0]
        let end = start + self.stream_buf.len() as u64;
        // Clamp the requested cursor into the retained window.
        let from = since.clamp(start, end);
        let skip = (from - start) as usize;
        // Bulk-copy the two ring segments (one memcpy each) instead of a per-byte
        // iterator walk.
        let (front, back) = self.stream_buf.as_slices();
        let mut out = Vec::with_capacity(self.stream_buf.len() - skip);
        if skip < front.len() {
            out.extend_from_slice(&front[skip..]);
            out.extend_from_slice(back);
        } else {
            out.extend_from_slice(&back[skip - front.len()..]);
        }
        let bytes: Arc<[u8]> = out.into();
        StreamDelta {
            generation: self.stream_generation,
            base_offset: from,
            bytes,
            end_offset: end,
        }
    }

    /// Test/diagnostic accessor: the full retained scrollback, verbatim (§87). Live
    /// consumers use [`stream_delta`](Self::stream_delta) instead.
    #[cfg(test)]
    fn stream_tail(&self) -> Vec<u8> {
        self.stream_buf.iter().copied().collect()
    }
}

/// Finalize one Display recording: flush the streaming renderer's tail (a
/// still-carried incomplete sequence and any deferred annotations — rendered
/// lossily now that the stream has truly ended) into the recording, then
/// finalize the file (§56, ADR-018).
async fn finalize_view_recorder(
    channel_id: ChannelId,
    mut rec: ViewRecorder,
    reason: RecordingStopReason,
) -> Option<String> {
    let tail = rec.renderer.finish();
    if !tail.is_empty() {
        rec.recording.try_record(RenderedOutput {
            channel_id,
            text: tail,
            timestamp: None,
        });
    }
    rec.recording.finalize(reason).await
}

/// Whether `free` bytes is below the disk-guard threshold (§168).
fn disk_is_low(free: u64, total: u64, threshold: DiskThreshold) -> bool {
    match threshold {
        DiskThreshold::Bytes { bytes } => free < bytes,
        DiskThreshold::Percent { percent } => {
            total > 0 && (free as u128) * 100 < (total as u128) * (percent as u128)
        }
    }
}

/// The async ingest loop for one Channel (§102, §110, §111).
///
/// Drains the bounded Transport→Pipeline channel until cancelled or the sender
/// is dropped, then returns the pipeline so the caller can finalize/inspect it.
/// Cancellation is cooperative and checked first (`biased`) so shutdown does not
/// depend on draining the queue (§111).
///
/// Between reads it also serves snapshot/stats/stream-delta requests (§137, ADR-006,
/// ADR-011) and records transport notices (§95, §101, ADR-007): a requester sends a
/// oneshot reply on `requests` and the loop answers (the small snapshot, cheap stats,
/// or an incremental stream delta) from current state; a transport sends a
/// `TransportNotice` on `notices` and the loop records it as a diagnostic. Both are checked ahead of reads (they are rare and cheap) so
/// they are serviced promptly; a closed `requests`/`notices` channel simply stops
/// being polled.
pub async fn run_channel(
    mut ingest: Receiver<ReceivedData>,
    mut requests: Receiver<PipelineRequest>,
    mut notices: Receiver<TransportNotice>,
    mut pipeline: ChannelPipeline,
    cancel: CancellationToken,
) -> ChannelPipeline {
    let mut requests_open = true;
    let mut notices_open = true;
    let mut ingest_open = true;
    // Periodic disk-space guard poll (§56.2, §168) — not per write. Cheap when no
    // guard is configured (the check returns immediately).
    let mut disk_check = tokio::time::interval(Duration::from_secs(5));
    disk_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Recorder maintenance cadence, independent of rule timing. It bounds
    // asynchronous fault visibility and detached-stop outcome reaping on a quiet
    // stream; Idle rules use exact deadlines below rather than this poll.
    let mut recorder_check = tokio::time::interval(Duration::from_millis(250));
    recorder_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    pipeline.record_event("Channel started");
    // "Record on start" (§53): begin once at startup through the same path as the live
    // Record toggle, so a failure (e.g. Refuse over an existing file) records a
    // diagnostic and emits RecordingFaulted instead of failing silently.
    if pipeline.should_auto_begin_recording() {
        pipeline.begin_recording().await;
    }
    loop {
        // Detached recorder stops (§56.1): report any outcome that landed
        // since the last pass. Non-blocking — reception never waits on a
        // file close; the recorder-maintenance tick guarantees a pass on quiet input.
        pipeline.reap_retired_recordings();
        let idle_deadline = pipeline.next_idle_deadline(Instant::now());
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            req = requests.recv(), if requests_open => match req {
                Some(PipelineRequest::Snapshot(tx)) => {
                    let _ = tx.send(pipeline.snapshot());
                }
                Some(PipelineRequest::Stats(tx)) => {
                    let _ = tx.send(pipeline.stats());
                }
                Some(PipelineRequest::StreamDelta { since, reply }) => {
                    let _ = reply.send(pipeline.stream_delta(since));
                }
                Some(PipelineRequest::SetRecording { enabled, settings }) => {
                    pipeline.set_recording(enabled, settings).await;
                }
                Some(PipelineRequest::SetDisplayRecording { enabled, settings }) => {
                    pipeline.set_display_recording(enabled, settings).await;
                }
                None => requests_open = false, // all requesters gone; keep running
            },
            notice = notices.recv(), if notices_open => match notice {
                Some(notice) => pipeline.record_notice(notice),
                // Every notice sender is gone (transport ended, fault monitor
                // done). Once ingest is closed too, nothing more can arrive.
                None => {
                    notices_open = false;
                    if !ingest_open {
                        break;
                    }
                }
            },
            _ = disk_check.tick() => pipeline.check_disk_guard().await,
            _ = recorder_check.tick() => pipeline.check_recording_faults(),
            timer_mode = wait_for_idle_deadline(idle_deadline) => {
                pipeline.idle_deadline_timer.record(timer_mode);
                pipeline.evaluate_idle_rules(Instant::now());
                pipeline.apply_pending_records().await;
                // Recorder tasks fault asynchronously; on a quiet stream this
                // tick is the only place the fault gets reported (§56.1).
                pipeline.check_recording_faults();
            }
            maybe = ingest.recv(), if ingest_open => match maybe {
                Some(data) => {
                    // Sample ingest occupancy for stress testing (§99): `len()` after a
                    // recv is the backlog still waiting — +1 for the item just taken is
                    // the depth at arrival. Tracks a high-water mark a 5 Hz poll misses.
                    pipeline.record_ingest_depth(ingest.len() + 1, ingest.max_capacity());
                    pipeline.ingest(data);
                    // A `Record` action may have been queued by a rule (§50.2).
                    pipeline.apply_pending_records().await;
                }
                // Transport gone and backlog fully drained (mpsc `None` =
                // closed AND empty, §110). Don't break yet: a spontaneous
                // fault's CAUSE may still be in flight on the notices channel
                // — the monitor learns the outcome only after the transport's
                // senders drop, so the notice can trail the ingest close. The
                // loop ends when the notices side closes too (the monitor
                // always terminates right after the transport, so this cannot
                // hang).
                None => {
                    if !notices_open {
                        break;
                    }
                    ingest_open = false;
                }
            },
        }
    }
    pipeline.finish().await;
    pipeline
}

const IDLE_PRECISION_WINDOW: Duration = Duration::from_millis(32);

async fn wait_for_idle_deadline(deadline: Option<Instant>) -> IdleDeadlineTimerMode {
    match deadline {
        Some(deadline) => {
            #[cfg(windows)]
            {
                let now = Instant::now();
                let window_start = deadline.checked_sub(IDLE_PRECISION_WINDOW).unwrap_or(now);
                if now < window_start {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(window_start)).await;
                }
                let guard = wiredata_timing::high_resolution();
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                if guard.is_effective() {
                    IdleDeadlineTimerMode::WindowsOneMillisecond
                } else {
                    IdleDeadlineTimerMode::WindowsRequestFailed
                }
            }
            #[cfg(not(windows))]
            {
                let _ = IDLE_PRECISION_WINDOW;
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                IdleDeadlineTimerMode::NativeDeadlineWait
            }
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MatchCondition;
    use crate::core::ChunkTime;
    use crate::transport::ReceivedPayload;

    fn pipeline(cid: ChannelId, caps: PipelineCapacities) -> ChannelPipeline {
        ChannelPipeline::new(cid, caps)
    }

    fn bytes_chunk(cid: ChannelId, data: &[u8]) -> ReceivedData {
        ReceivedData {
            channel_id: cid,
            payload: ReceivedPayload::Bytes(data.to_vec()),
            received_at: ChunkTime::now(),
        }
    }

    fn datagram(cid: ChannelId, data: &[u8]) -> ReceivedData {
        ReceivedData {
            channel_id: cid,
            payload: ReceivedPayload::Datagram(data.to_vec()),
            received_at: ChunkTime::now(),
        }
    }

    #[test]
    fn ingest_records_handoff_delay_and_total_processing_time() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        let captured = Instant::now()
            .checked_sub(Duration::from_millis(10))
            .expect("monotonic clock has at least 10 ms of history");

        p.ingest(ReceivedData {
            channel_id: cid,
            payload: ReceivedPayload::Bytes(vec![0xAB]),
            received_at: ChunkTime {
                monotonic: captured,
                wall_clock: std::time::SystemTime::now(),
                wall_clock_source: crate::core::ArrivalTimestampSource::PostRead,
            },
        });

        let stats = p.stats();
        assert_eq!(stats.ingest_delay.sample_count(), 1);
        assert_eq!(stats.ingest_processing.sample_count(), 1);
        assert_eq!(stats.recent_ingest_processing.sample_count(), 1);
        assert_eq!(stats.chunk_shape.chunk_count(), 1);
        assert_eq!(stats.chunk_shape.sizes.max(), Some(1));
        assert_eq!(stats.chunk_shape.inter_read_gaps.sample_count(), 0);
        assert!(
            stats.ingest_delay.max().unwrap() >= Duration::from_millis(10),
            "the measured boundary includes the deliberate pre-ingest delay"
        );
        let snapshot = p.snapshot();
        assert_eq!(snapshot.ingest_delay, stats.ingest_delay);
        assert_eq!(snapshot.ingest_processing, stats.ingest_processing);
        assert_eq!(
            snapshot.recent_ingest_processing,
            stats.recent_ingest_processing
        );
        assert_eq!(snapshot.chunk_shape, stats.chunk_shape);
    }

    #[test]
    fn chunk_shape_uses_transport_capture_times_not_pipeline_queue_time() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        let captured = Instant::now()
            .checked_sub(Duration::from_millis(20))
            .expect("monotonic clock has at least 20 ms of history");

        for (offset_ms, payload) in [(0, vec![0; 8]), (7, vec![0; 1_500])] {
            p.ingest(ReceivedData {
                channel_id: cid,
                payload: ReceivedPayload::Bytes(payload),
                received_at: ChunkTime {
                    monotonic: captured + Duration::from_millis(offset_ms),
                    wall_clock: std::time::SystemTime::now(),
                    wall_clock_source: crate::core::ArrivalTimestampSource::PostRead,
                },
            });
        }

        let stats = p.stats();
        assert_eq!(stats.chunk_shape.chunk_count(), 2);
        assert_eq!(stats.chunk_shape.sizes.max(), Some(1_500));
        assert_eq!(stats.chunk_shape.inter_read_gaps.sample_count(), 1);
        assert_eq!(
            stats.chunk_shape.inter_read_gaps.max(),
            Some(Duration::from_millis(7))
        );
    }

    #[test]
    fn datagram_boundaries_do_not_segment_the_stream() {
        // v2 invariant (§15, §18): UDP datagram boundaries are a reception/recording
        // detail only. The stream concatenates datagram payloads verbatim — nothing
        // is inserted between them and nothing is reframed.
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.ingest(datagram(cid, b"$GPGGA,1*00\r\n"));
        p.ingest(datagram(cid, b"$GPRMC,2*00\r\n"));
        assert_eq!(&p.stream_tail()[..], &b"$GPGGA,1*00\r\n$GPRMC,2*00\r\n"[..]);
        // Mixed chunk kinds (a serial-style Bytes chunk after datagrams) still
        // append verbatim: one stream, regardless of transport read shape.
        p.ingest(bytes_chunk(cid, b"tail"));
        assert_eq!(
            &p.stream_tail()[..],
            &b"$GPGGA,1*00\r\n$GPRMC,2*00\r\ntail"[..]
        );
    }

    #[test]
    fn stream_tail_is_verbatim_across_chunks_and_byte_capped() {
        let cid = ChannelId::new();
        // Small cap so trimming is observable.
        let caps = PipelineCapacities {
            stream_display: 8,
            ..PipelineCapacities::default()
        };
        let mut p = pipeline(cid, caps);
        // Reconstructs the wire across read-chunk boundaries.
        p.ingest(bytes_chunk(cid, b"$ABC"));
        p.ingest(bytes_chunk(cid, b"\r\n"));
        assert_eq!(&p.stream_tail()[..], &b"$ABC\r\n"[..]);
        // Over the cap: keep only the most recent `stream_display` bytes.
        p.ingest(bytes_chunk(cid, b"123456")); // "$ABC\r\n123456" (12) → drop front 4
        assert_eq!(&p.stream_tail()[..], &b"\r\n123456"[..]);
        // A single chunk larger than the cap keeps just its tail.
        p.ingest(bytes_chunk(cid, b"0123456789"));
        assert_eq!(&p.stream_tail()[..], &b"23456789"[..]);
    }

    #[test]
    fn stream_delta_serves_only_new_bytes_since_a_cursor() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.ingest(bytes_chunk(cid, b"hello"));
        // From the start: the whole window.
        let d0 = p.stream_delta(0);
        assert_eq!(d0.base_offset, 0);
        assert_eq!(&*d0.bytes, &b"hello"[..]);
        assert_eq!(d0.end_offset, 5);

        // From the prior end cursor: only the new bytes (no re-ship of "hello").
        p.ingest(bytes_chunk(cid, b"world"));
        let d1 = p.stream_delta(d0.end_offset);
        assert_eq!(d1.base_offset, 5);
        assert_eq!(&*d1.bytes, &b"world"[..]);
        assert_eq!(d1.end_offset, 10);

        // Caught up: an at-end cursor yields nothing.
        let d2 = p.stream_delta(d1.end_offset);
        assert!(d2.bytes.is_empty());
        assert_eq!(d2.base_offset, 10);
        assert_eq!(d2.end_offset, 10);
    }

    #[test]
    fn stream_delta_resets_when_the_cursor_was_evicted() {
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            stream_display: 4,
            ..PipelineCapacities::default()
        };
        let mut p = pipeline(cid, caps);
        p.ingest(bytes_chunk(cid, b"AB")); // offsets 0..2
        let d0 = p.stream_delta(0); // cursor now 2
        assert_eq!(&*d0.bytes, &b"AB"[..]);

        // Push past the cap so offsets 0..2 are evicted (cap 4): buffer holds 4..8.
        p.ingest(bytes_chunk(cid, b"CDEF")); // "ABCDEF" → keep "CDEF", dropped 2
                                             // A stale cursor (2) is behind the window start (2 dropped → start=2);
                                             // here start==2 so it's still valid. Drop more to force a reset.
        p.ingest(bytes_chunk(cid, b"GH")); // "CDEFGH" → keep "EFGH", dropped total 4

        // Cursor 2 is now behind the window start (4): the delta resets to the
        // window with base_offset > since, signalling the consumer to re-seed.
        let d1 = p.stream_delta(d0.end_offset); // since = 2
        assert_eq!(d1.base_offset, 4, "cursor was evicted; base jumps forward");
        assert_eq!(&*d1.bytes, &b"EFGH"[..]);
        assert_eq!(d1.end_offset, 8);
    }

    #[test]
    fn paused_view_freezes_the_stream_tail() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        let view = p.display_view_handles()[0].clone();
        p.ingest(bytes_chunk(cid, b"AB"));
        assert_eq!(&p.stream_tail()[..], &b"AB"[..]);
        // Pause: the displayed stream freezes (§50), but reception still counts
        // the bytes — the liveness counter keeps moving.
        view.pause();
        p.ingest(bytes_chunk(cid, b"CD"));
        assert_eq!(&p.stream_tail()[..], &b"AB"[..]);
        assert_eq!(p.snapshot().activity.total_bytes, 4);
        // Resume: the stream continues from live data (no backfill of the gap).
        view.resume();
        p.ingest(bytes_chunk(cid, b"EF"));
        assert_eq!(&p.stream_tail()[..], &b"ABEF"[..]);
    }

    #[test]
    fn default_capacities_bound_the_diagnostics_log() {
        // §88/§124: with no configured limits, the default caps still bound each
        // severity — a recurring warning can't grow the log (and the 5 Hz snapshot
        // clone) unbounded over a weeks-long run.
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        for i in 0..(DIAGNOSTIC_RETENTION + 50) {
            p.diagnostics.record(Diagnostic::warning(format!("w{i}")));
        }
        assert_eq!(p.diagnostics().warnings().count(), DIAGNOSTIC_RETENTION);
    }

    #[test]
    fn repeated_polls_of_an_unchanged_log_reuse_one_shared_snapshot() {
        // §124: the selected channel is polled several times a second for the life
        // of a run, while the log itself changes rarely. An unchanged log must cost
        // a pointer clone, not a deep copy of every retained entry.
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.diagnostics.record(Diagnostic::warning("first"));

        let first = p.snapshot().diagnostics;
        let second = p.snapshot().diagnostics;
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged log must not be rebuilt"
        );

        // A new diagnostic invalidates the cache, and the rebuilt snapshot carries
        // it — the reuse must never serve a stale log.
        p.diagnostics.record(Diagnostic::error("second"));
        let third = p.snapshot().diagnostics;
        assert!(!Arc::ptr_eq(&second, &third), "a changed log is rebuilt");
        assert_eq!(third.warnings.len(), 1);
        assert_eq!(third.errors.len(), 1);
        // The previously handed-out snapshot is unaffected by the rebuild.
        assert_eq!(first.errors.len(), 0);
    }

    #[test]
    fn evicting_a_diagnostic_still_counts_as_a_change() {
        // The retained count is unchanged when a push evicts the oldest entry, so
        // the revision — not the length — is what the cache keys on.
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            warning_retention: Some(1),
            ..PipelineCapacities::default()
        };
        let mut p = pipeline(cid, caps);
        p.diagnostics.record(Diagnostic::warning("oldest"));
        let before = p.snapshot().diagnostics;

        p.diagnostics.record(Diagnostic::warning("newest"));
        let after = p.snapshot().diagnostics;

        assert!(!Arc::ptr_eq(&before, &after), "eviction is a change");
        assert_eq!(after.warnings.len(), 1);
        assert!(after.warnings[0].message.contains("newest"));
    }

    #[test]
    fn diagnostic_warning_retention_limit_is_applied() {
        // §88: the per-type diagnostic limit bounds retained warnings.
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            warning_retention: Some(2),
            ..PipelineCapacities::default()
        };
        let mut p = pipeline(cid, caps);
        // Four warnings recorded, capped at two retained.
        for i in 0..4 {
            p.diagnostics.record(Diagnostic::warning(format!("w{i}")));
        }
        assert_eq!(p.diagnostics().warnings().count(), 2);
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "listener-pipeline-{tag}-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[tokio::test]
    async fn raw_recording_captures_received_bytes() {
        // §53: the raw tap records every received chunk byte-exact, in order.
        let cid = ChannelId::new();
        let path = temp_path("raw");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let recording = start_raw_recording(recorder, 64);
        let mut p = pipeline(cid, PipelineCapacities::default()).with_raw_recorder(recording);

        p.ingest(bytes_chunk(cid, b"$GPGLL,1*00\r\n"));
        p.ingest(datagram(cid, b"$GPGLL,2*00\r\n"));
        p.finish().await;

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"$GPGLL,1*00\r\n$GPGLL,2*00\r\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn display_recording_captures_rendered_output() {
        // §54: the display recorder writes each chunk's *rendered* output.
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let path = temp_path("disp");
        let recorder = DisplayFileRecorder::create(&path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let recording = start_display_recording(recorder, 64);
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.set_display_recorder(DisplayView::default(), recording);

        p.ingest(bytes_chunk(cid, b"hello"));
        p.finish().await;

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("hello"));
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn disp_is_the_exact_rendered_stream_across_read_boundaries() {
        // ADR-018: the .disp concatenation carries no read-boundary artifacts —
        // no injected newlines (the old line-per-chunk behavior), and a UTF-8
        // character split across two reads decodes as itself.
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let path = temp_path("exact-disp");
        let disp = DisplayFileRecorder::create(&path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.set_display_recorder(
            DisplayView {
                mode: crate::display::DisplayMode::Rendered,
                ..DisplayView::default()
            },
            start_display_recording(disp, 64),
        );

        // One sentence split arbitrarily across three reads, with the é split
        // mid-character between chunks 2 and 3.
        let full = "temp 21°C\r\nnése\r\n";
        let bytes = full.as_bytes();
        p.ingest(bytes_chunk(cid, &bytes[..4]));
        p.ingest(bytes_chunk(cid, &bytes[4..13])); // ends inside the é of "nése"
        p.ingest(bytes_chunk(cid, &bytes[13..]));
        p.finish().await;

        // Rendered mode: CRLF collapses to \n; otherwise the text is exactly the
        // data — line structure from the data, not from the reads.
        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(written, "temp 21°C\nnése\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn display_recording_continues_while_the_view_is_paused() {
        // §58: pausing presentation never pauses recording.
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let path = temp_path("disp-paused");
        let recorder = DisplayFileRecorder::create(&path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let recording = start_display_recording(recorder, 64);
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.set_display_recorder(DisplayView::default(), recording);
        let view = p.display_view_handles()[0].clone();

        view.pause();
        p.ingest(bytes_chunk(cid, b"while-paused"));
        p.finish().await;

        // The paused view's scrollback stayed empty, but the recording captured it.
        assert!(p.stream_tail().is_empty());
        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("while-paused"));
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn recording_overflow_faults_emits_event_and_reception_continues() {
        // §56.1: a recorder that cannot keep up faults; reception continues.
        use crate::record::RawRecorder;
        struct StallingRecorder(tokio::sync::oneshot::Receiver<()>);
        #[async_trait::async_trait]
        impl RawRecorder for StallingRecorder {
            async fn write_chunk(
                &mut self,
                _chunk: &ReceivedData,
            ) -> Result<(), crate::core::RecordError> {
                // Park forever: the queue backs up and overflows.
                let _ = (&mut self.0).await;
                Ok(())
            }
            async fn flush(&mut self) -> Result<(), crate::core::RecordError> {
                Ok(())
            }
            async fn finalize(
                &mut self,
                _reason: RecordingStopReason,
            ) -> Result<(), crate::core::RecordError> {
                Ok(())
            }
        }

        let cid = ChannelId::new();
        let (_hold_tx, hold_rx) = tokio::sync::oneshot::channel();
        let recording = start_raw_recording(StallingRecorder(hold_rx), 1);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(recording)
            .with_event_sender(event_tx);

        // Flood: the 1-slot queue fills while the writer is parked; the recording
        // faults, reception continues, and the stream keeps accumulating.
        for _ in 0..8 {
            p.ingest(bytes_chunk(cid, b"x"));
            tokio::task::yield_now().await;
        }
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Faulted));
        assert_eq!(p.snapshot().activity.total_bytes, 8);
        let mut saw_fault = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, RuntimeEvent::RecordingFaulted(id, RecordingTap::Raw) if id == cid) {
                saw_fault = true;
            }
        }
        assert!(saw_fault);
    }

    #[tokio::test]
    async fn run_channel_drains_then_stops_when_sender_drops() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (_req_tx, req_rx) = tokio::sync::mpsc::channel(1);
        let (notice_tx, notice_rx) = tokio::sync::mpsc::channel(1);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel));

        tx.send(bytes_chunk(cid, b"abc")).await.unwrap();
        // Natural end = BOTH senders gone (as in production: the transport
        // and its fault monitor drop them after the transport ends). The loop
        // deliberately outlives the ingest close alone, so a fault-cause
        // notice can never be outrun.
        drop(tx);
        drop(notice_tx);
        let p = task.await.unwrap();
        assert_eq!(&p.stream_tail()[..], &b"abc"[..]);
    }

    #[tokio::test]
    async fn run_channel_stops_on_cancellation_with_live_sender() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (_req_tx, req_rx) = tokio::sync::mpsc::channel(1);
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(1);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel.clone()));

        cancel.cancel();
        let _p = task.await.unwrap();
        drop(tx); // sender stayed alive the whole time
    }

    #[tokio::test]
    async fn run_channel_serves_snapshots_while_running() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(1);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel.clone()));

        tx.send(bytes_chunk(cid, b"live")).await.unwrap();
        // Snapshot reports the channel and the stream end offset (cursor target);
        // the bytes themselves come via the incremental StreamDelta request.
        let snapshot = loop {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            req_tx
                .send(PipelineRequest::Snapshot(reply_tx))
                .await
                .unwrap();
            let s = reply_rx.await.unwrap();
            if s.stream_end_offset > 0 {
                break s;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(snapshot.channel_id, cid);
        assert_eq!(snapshot.stream_end_offset, 4);

        // Fetch the new bytes from offset 0 via the incremental path.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        req_tx
            .send(PipelineRequest::StreamDelta {
                since: 0,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let delta = reply_rx.await.unwrap();
        assert_eq!(delta.base_offset, 0);
        assert_eq!(&*delta.bytes, &b"live"[..]);
        assert_eq!(delta.end_offset, 4);

        cancel.cancel();
        let _ = task.await.unwrap();
    }

    #[tokio::test]
    async fn run_channel_serves_cheap_stats_while_running() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(1);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel.clone()));

        tx.send(bytes_chunk(cid, b"12345")).await.unwrap();
        let stats = loop {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            req_tx.send(PipelineRequest::Stats(reply_tx)).await.unwrap();
            let s = reply_rx.await.unwrap();
            if s.activity.total_bytes > 0 {
                break s;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(stats.activity.total_bytes, 5);

        cancel.cancel();
        let _ = task.await.unwrap();
    }

    #[tokio::test]
    async fn run_channel_records_a_transport_notice_as_a_diagnostic() {
        let cid = ChannelId::new();
        let (_tx, rx) = tokio::sync::mpsc::channel(8);
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (notice_tx, notice_rx) = tokio::sync::mpsc::channel(4);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel.clone()));

        notice_tx
            .send(TransportNotice::ReceptionStalled {
                channel_id: cid,
                stalled_for: Duration::from_millis(750),
            })
            .await
            .unwrap();
        let snapshot = loop {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            req_tx
                .send(PipelineRequest::Snapshot(reply_tx))
                .await
                .unwrap();
            let s = reply_rx.await.unwrap();
            if !s.diagnostics.warnings.is_empty() {
                break s;
            }
            tokio::task::yield_now().await;
        };
        assert!(snapshot.diagnostics.warnings[0]
            .message
            .contains("reception stalled 750 ms"));

        cancel.cancel();
        let _ = task.await.unwrap();
    }

    #[test]
    fn saturated_event_queue_omits_stall_event_but_keeps_diagnostic_and_metrics() {
        let cid = ChannelId::new();
        let stall_state = SerialStallState::default();
        let started = Instant::now();
        assert!(stall_state.begin_at(started));
        assert!(stall_state.finish_at(started + Duration::from_millis(25)));

        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(1);
        events_tx
            .try_send(RuntimeEvent::WarningRaised(cid))
            .unwrap();
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_serial_stall_state(stall_state)
            .with_event_sender(events_tx);
        let before = p.stats().transport_health.serial_stalls;

        p.record_notice(TransportNotice::ReceptionStalled {
            channel_id: cid,
            stalled_for: Duration::from_millis(750),
        });

        assert!(p
            .snapshot()
            .diagnostics
            .warnings
            .iter()
            .any(|diagnostic| diagnostic.message.contains("reception stalled 750 ms")));
        assert_eq!(
            events_rx.try_recv(),
            Ok(RuntimeEvent::WarningRaised(cid)),
            "the pre-existing event should remain the only queued event"
        );
        assert!(
            events_rx.try_recv().is_err(),
            "the advisory stall event must drop rather than block on saturation"
        );
        assert_eq!(
            p.stats().transport_health.serial_stalls,
            before,
            "advisory event loss must not alter authoritative stall metrics"
        );
    }

    #[test]
    fn transport_health_distinguishes_active_serial_stalls_and_udp_support() {
        let cid = ChannelId::new();
        let stall_state = SerialStallState::default();
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_serial_stall_state(stall_state.clone());
        assert_eq!(
            p.snapshot().transport_health.serial_stalls,
            Some(SerialStallSummary::default())
        );

        let started = Instant::now();
        assert!(stall_state.begin_at(started));
        let active = p.snapshot().transport_health.serial_stalls.unwrap();
        assert!(active.active_for.is_some());
        assert_eq!(active.episodes, 0);
        assert_eq!(active.total, Duration::ZERO);

        assert!(stall_state.finish_at(started + Duration::from_millis(800)));
        let completed = p.snapshot().transport_health.serial_stalls.unwrap();
        assert_eq!(completed.active_for, None);
        assert_eq!(completed.episodes, 1);
        assert_eq!(completed.total, Duration::from_millis(800));

        p.record_notice(TransportNotice::UdpKernelDrops {
            channel_id: cid,
            dropped: None,
        });
        assert_eq!(
            p.stats().transport_health.udp_kernel_drops,
            CounterAvailability::Unsupported
        );
        p.record_notice(TransportNotice::UdpKernelDrops {
            channel_id: cid,
            dropped: Some(3),
        });
        assert_eq!(
            p.stats().transport_health.udp_kernel_drops,
            CounterAvailability::Available(3)
        );
    }

    #[test]
    fn disk_is_low_compares_bytes_and_percent_thresholds() {
        assert!(disk_is_low(9, 100, DiskThreshold::Bytes { bytes: 10 }));
        assert!(!disk_is_low(10, 100, DiskThreshold::Bytes { bytes: 10 }));
        assert!(disk_is_low(4, 100, DiskThreshold::Percent { percent: 5 }));
        assert!(!disk_is_low(5, 100, DiskThreshold::Percent { percent: 5 }));
        // A zero-total filesystem is never "low" (avoids division weirdness).
        assert!(!disk_is_low(0, 0, DiskThreshold::Percent { percent: 5 }));
    }

    #[tokio::test]
    async fn disk_guard_stops_recording_once_and_emits_events() {
        // §56.2/§168: an impossible byte threshold (u64::MAX) is always "low", so
        // the guard warns, stops the recording cleanly, and debounces the report.
        let cid = ChannelId::new();
        let path = temp_path("guard");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let recording = start_raw_recording(recorder, 64);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(recording)
            .with_disk_guard(
                DiskGuard {
                    min_free: DiskThreshold::Bytes { bytes: u64::MAX },
                    on_low: LowDiskAction::StopRecording,
                },
                std::env::temp_dir(),
            )
            .with_event_sender(event_tx);

        p.ingest(bytes_chunk(cid, b"data"));
        p.check_disk_guard().await;
        // The recording was stopped and finalized; reception continues.
        assert!(p.raw_recording_state().is_none());
        let mut low = 0;
        let mut stopped = 0;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                RuntimeEvent::DiskSpaceLow(id) if id == cid => low += 1,
                RuntimeEvent::RecordingStoppedLowDisk(id) if id == cid => stopped += 1,
                _ => {}
            }
        }
        assert_eq!((low, stopped), (1, 1));
        // Debounced: a second poll in the same low episode does not re-report.
        p.check_disk_guard().await;
        assert!(event_rx.try_recv().is_err());
        let _ = tokio::fs::remove_file(&path).await;
    }

    // --- Find & Triggers (§50.2, §165) ---

    fn byte_rule(name: &str, pattern: &[u8], actions: Vec<MatchAction>) -> MatchRule {
        MatchRule {
            name: name.to_string(),
            condition: MatchCondition::BytePattern {
                pattern: pattern.to_vec(),
            },
            actions,
            enabled: true,
        }
    }

    #[test]
    fn notify_rule_records_a_diagnostic_logs_the_firing_and_emits_an_event() {
        let cid = ChannelId::new();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[byte_rule(
                "gga",
                b"GGA",
                vec![MatchAction::Notify {
                    severity: crate::diagnostics::DiagnosticSeverity::Warning,
                }],
            )])
            .with_event_sender(event_tx);

        p.ingest(bytes_chunk(cid, b"$GPGLL,...")); // no match (10 bytes, offsets 0..9)
        p.ingest(bytes_chunk(cid, b"$GPGGA,...")); // "GGA" at chunk index 3 → offset 13
        assert_eq!(p.diagnostics().warnings().count(), 1);
        let snapshot = p.snapshot();
        assert_eq!(snapshot.matches.len(), 1);
        // The firing is anchored at the match's exact stream offset (§50.2): chunk 2
        // starts at offset 10 and "GGA" begins 3 bytes into it.
        assert_eq!(snapshot.matches[0].byte_offset, Some(13));
        // A within-chunk match is not a boundary split.
        assert_eq!(snapshot.match_boundary_saves, 0);
        let mut saw_match = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, RuntimeEvent::MatchTriggered(id, _) if id == cid) {
                saw_match = true;
            }
        }
        assert!(saw_match);
    }

    #[test]
    fn byte_pattern_spanning_two_chunks_fires_and_is_measured() {
        // §50.2 cross-chunk carry: a pattern split across two received chunks still
        // matches, anchors on its true start offset, and is counted + diagnosed as a
        // boundary save (where / why / how often).
        let cid = ChannelId::new();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[byte_rule(
                "gga",
                b"GGA",
                vec![MatchAction::Notify {
                    severity: crate::diagnostics::DiagnosticSeverity::Warning,
                }],
            )])
            .with_event_sender(event_tx);

        // "GG" ends chunk 1 (offsets 0..4); "A" begins chunk 2 (offset 5). A per-
        // chunk scan would miss "GGA"; the carry recovers it.
        p.ingest(bytes_chunk(cid, b"$GPGG"));
        assert_eq!(
            p.snapshot().matches.len(),
            0,
            "nothing fires within chunk 1"
        );
        p.ingest(bytes_chunk(cid, b"A,123"));

        let snap = p.snapshot();
        assert_eq!(snap.matches.len(), 1, "the split pattern fires on chunk 2");
        // "GGA" starts at stream offset 3 (inside chunk 1).
        assert_eq!(snap.matches[0].byte_offset, Some(3));
        // How often: exactly one boundary save measured.
        assert_eq!(snap.match_boundary_saves, 1);
        // Where / why: an event diagnostic records the offset and the cause.
        let boundary_note = p
            .diagnostics()
            .events()
            .any(|d| d.message.contains("read-chunk boundary") && d.message.contains("offset 3"));
        assert!(boundary_note, "a where/why diagnostic is recorded");
        // The recovered match still emits a normal MatchTriggered event.
        let mut saw_match = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, RuntimeEvent::MatchTriggered(id, _) if id == cid) {
                saw_match = true;
            }
        }
        assert!(saw_match);
    }

    #[test]
    fn match_view_offset_tracks_the_paused_view_not_the_raw_stream() {
        // §50/§50.2: stream offsets count every received byte, but the view skips
        // bytes that arrive while paused, so the two spaces diverge after a pause.
        // A firing carries both — `byte_offset` (stream space, what diagnostics
        // quote) and `view_offset` (view/scrollback space, where the live viewer
        // splices a Mark timestamp) — so the splice stays aligned.
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[byte_rule(
            "m",
            b"M",
            vec![MatchAction::Notify {
                severity: crate::diagnostics::DiagnosticSeverity::Event,
            }],
        )]);
        let view = p.display_view_handles()[0].clone();

        // Before any pause the two spaces are identical.
        p.ingest(bytes_chunk(cid, b"aM")); // match at stream offset 1
        let snap = p.snapshot();
        assert_eq!(snap.matches[0].byte_offset, Some(1));
        assert_eq!(snap.matches[0].view_offset, Some(1));

        // Paused: the bytes never enter the view → no view position.
        view.pause();
        p.ingest(bytes_chunk(cid, b"cM")); // match at stream offset 3
        let snap = p.snapshot();
        assert_eq!(snap.matches[1].byte_offset, Some(3));
        assert_eq!(snap.matches[1].view_offset, None);

        // Resumed: stream space is now 2 bytes ahead of view space.
        view.resume();
        p.ingest(bytes_chunk(cid, b"eM")); // stream offset 5; view holds "aMeM" → 3
        let snap = p.snapshot();
        assert_eq!(snap.matches[2].byte_offset, Some(5));
        assert_eq!(snap.matches[2].view_offset, Some(3));
        assert_eq!(
            snap.stream_end_offset, 4,
            "view space excludes the paused bytes"
        );
    }

    #[tokio::test]
    async fn every_occurrence_in_a_chunk_gets_its_own_mark_timestamp() {
        // §50.2: a chunk carrying several occurrences fires the rule per occurrence,
        // so each one gets an inline timestamp in the `.disp` — not just the first.
        use crate::config::{MarkPosition, MarkTimestamp};
        use crate::core::TimestampConfig;
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let disp_path = temp_path("multi-mark");
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mark = MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: MarkPosition::Before,
                style: Default::default(),
                format: TimestampConfig::default(), // HH:MM:SS
                separator: String::new(),
            }),
        };
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[byte_rule(
            "dollar",
            b"$",
            vec![mark],
        )]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));

        p.ingest(bytes_chunk(cid, b"$GPGGA,1 $GPRMC,2"));
        let snap = p.snapshot();
        p.finish().await;

        assert_eq!(snap.matches.len(), 2, "both occurrences fired");
        assert!(snap.matches.iter().all(|m| m.mark.is_some()));
        // Two HH:MM:SS splices → four ':' (the data itself has none).
        let disp_written = tokio::fs::read_to_string(&disp_path).await.unwrap();
        assert_eq!(
            disp_written.matches(':').count(),
            4,
            "each occurrence carries its own inline timestamp: {disp_written:?}"
        );
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn mark_separator_trails_the_timestamp_in_disp_and_snapshot() {
        // §50.2: the optional separator is appended to the formatted timestamp —
        // `[ts][sep]match…` for Before — in both the .disp and the snapshot's
        // MarkRender (so the live view shows the same text).
        use crate::config::{MarkPosition, MarkTimestamp};
        use crate::core::TimestampConfig;
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let disp_path = temp_path("sep-mark");
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mark = MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: MarkPosition::Before,
                style: Default::default(),
                format: TimestampConfig::default(), // HH:MM:SS
                separator: ", ".to_string(),
            }),
        };
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[byte_rule(
            "gga",
            b"$GPGGA",
            vec![mark],
        )]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));

        p.ingest(bytes_chunk(cid, b"xx$GPGGA,1"));
        let snap = p.snapshot();
        p.finish().await;

        let mark = snap.matches[0].mark.as_ref().expect("mark render");
        assert!(
            mark.text.ends_with(", "),
            "separator trails the timestamp: {:?}",
            mark.text
        );
        // The .disp shows `HH:MM:SS, ` immediately before the matched pattern.
        let disp_written = tokio::fs::read_to_string(&disp_path).await.unwrap();
        let at = disp_written.find("$GPGGA").expect("match rendered");
        assert!(
            disp_written[..at].ends_with(", "),
            "separator sits between the timestamp and the match: {disp_written:?}"
        );
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[test]
    fn pause_display_rule_freezes_the_targeted_view() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[byte_rule(
            "freeze",
            b"STOP",
            vec![MatchAction::PauseDisplay { view: Some(0) }],
        )]);
        assert!(!p.snapshot().display_views[0].paused);
        p.ingest(bytes_chunk(cid, b"...STOP..."));
        assert!(p.snapshot().display_views[0].paused);
    }

    #[test]
    fn idle_rule_fires_once_then_rearms_after_data() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[MatchRule {
            name: "quiet".to_string(),
            condition: MatchCondition::Idle { timeout_ms: 0 },
            actions: vec![MatchAction::Notify {
                severity: crate::diagnostics::DiagnosticSeverity::Event,
            }],
            enabled: true,
        }]);
        // Quiet from creation: the idle rule fires once (offset is None).
        p.evaluate_idle_rules(Instant::now());
        p.evaluate_idle_rules(Instant::now());
        assert_eq!(p.snapshot().matches.len(), 1);
        assert_eq!(p.snapshot().matches[0].byte_offset, None);
        // Data re-arms it; quiet again → a second firing.
        p.ingest(bytes_chunk(cid, b"x"));
        p.evaluate_idle_rules(Instant::now() + Duration::from_secs(1));
        assert_eq!(p.snapshot().matches.len(), 2);
        assert_eq!(p.snapshot().rule_timer_lateness.sample_count(), 2);
        assert_eq!(p.snapshot().recent_rule_timer_lateness.sample_count(), 2);
    }

    #[test]
    fn idle_rule_deadline_and_lateness_use_the_same_monotonic_anchor() {
        let cid = ChannelId::new();
        let base = Instant::now();
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[MatchRule {
            name: "quiet".to_string(),
            condition: MatchCondition::Idle { timeout_ms: 500 },
            actions: vec![],
            enabled: true,
        }]);
        p.created_at = base;

        assert_eq!(
            p.next_idle_deadline(base + Duration::from_millis(100)),
            Some(base + Duration::from_millis(500))
        );
        p.evaluate_idle_rules(base + Duration::from_millis(620));
        let timing = p.stats().rule_timer_lateness;
        assert_eq!(timing.sample_count(), 1);
        assert_eq!(timing.max(), Some(Duration::from_millis(120)));
        assert_eq!(
            p.next_idle_deadline(base + Duration::from_millis(700)),
            None
        );
    }

    #[tokio::test]
    async fn idle_deadline_wait_reports_the_platform_timer_policy() {
        let mode = wait_for_idle_deadline(Some(Instant::now() + Duration::from_millis(2))).await;
        #[cfg(windows)]
        assert!(matches!(
            mode,
            IdleDeadlineTimerMode::WindowsOneMillisecond
                | IdleDeadlineTimerMode::WindowsRequestFailed
        ));
        #[cfg(not(windows))]
        assert_eq!(mode, IdleDeadlineTimerMode::NativeDeadlineWait);
    }

    #[tokio::test]
    async fn record_action_begins_and_stops_from_the_match_forward() {
        // §50.2/§158: Record{Begin} creates the recording lazily and captures only
        // data from the match forward; Record{Stop} finalizes it.
        let cid = ChannelId::new();
        let path = temp_path("armed");
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[
                byte_rule(
                    "begin",
                    b"BEGIN",
                    vec![MatchAction::Record {
                        target: RecordTarget::Raw,
                        control: RecordControl::Begin,
                    }],
                ),
                byte_rule(
                    "stop",
                    b"STOP",
                    vec![MatchAction::Record {
                        target: RecordTarget::Raw,
                        control: RecordControl::Stop,
                    }],
                ),
            ])
            .with_recording_settings(RawRecordingSettings {
                destination: path.clone(),
                channel_name: "armed".to_string(),
                overwrite: OverwritePolicy::Refuse,
                timestamps: false,
                file_rotation: FileRotationPolicy::None,
                capacity: 64,
            });

        // Before the match: nothing on disk, nothing recorded.
        p.ingest(bytes_chunk(cid, b"before "));
        p.apply_pending_records().await;
        assert!(p.raw_recording_state().is_none());

        // The BEGIN chunk fires the rule; recording starts *from the match forward*
        // (the BEGIN chunk itself is not backfilled, §158).
        p.ingest(bytes_chunk(cid, b"BEGIN"));
        p.apply_pending_records().await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"captured"));

        // STOP finalizes; later data is not written.
        p.ingest(bytes_chunk(cid, b"STOP"));
        p.apply_pending_records().await;
        assert!(p.raw_recording_state().is_none());
        p.ingest(bytes_chunk(cid, b"after"));
        p.finish().await;

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"capturedSTOP");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn record_action_display_target_begins_and_stops_display_recording() {
        // §50.2/§54: Record { target: Display } drives the .disp — lazily created
        // at BEGIN from the spawn-time settings, finalized at STOP — while the
        // Raw side is untouched (no raw recorder ever attaches).
        let cid = ChannelId::new();
        let path = temp_path("armed-disp");
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[
                byte_rule(
                    "begin",
                    b"BEGIN",
                    vec![MatchAction::Record {
                        target: RecordTarget::Display,
                        control: RecordControl::Begin,
                    }],
                ),
                byte_rule(
                    "stop",
                    b"STOP",
                    vec![MatchAction::Record {
                        target: RecordTarget::Display,
                        control: RecordControl::Stop,
                    }],
                ),
            ])
            .with_display_recording_settings(DisplayRecordingSettings {
                destination: path.clone(),
                channel_name: "armed-disp".to_string(),
                overwrite: OverwritePolicy::Refuse,
                file_rotation: FileRotationPolicy::None,
                capacity: 64,
                renderer: DisplayView::default(),
            });

        p.ingest(bytes_chunk(cid, b"before "));
        p.apply_pending_records().await;
        assert!(p.display_recording_state().is_none());

        p.ingest(bytes_chunk(cid, b"BEGIN"));
        p.apply_pending_records().await;
        assert_eq!(p.display_recording_state(), Some(RecordingState::Enabled));
        assert!(p.raw_recording_state().is_none(), "Raw is untouched");
        p.ingest(bytes_chunk(cid, b"captured"));

        p.ingest(bytes_chunk(cid, b"STOP"));
        p.apply_pending_records().await;
        assert!(p.display_recording_state().is_none());
        p.ingest(bytes_chunk(cid, b"after"));
        p.finish().await;

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("captured"), "{written:?}");
        assert!(
            !written.contains("before") && !written.contains("after"),
            "{written:?}"
        );
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn record_action_both_target_drives_raw_and_display_together() {
        // §50.2: Record { target: Both } begins/stops the byte-exact .raw and the
        // rendered .disp in one firing — each via its own lazy-create settings.
        let cid = ChannelId::new();
        let raw_path = temp_path("both-raw");
        let disp_path = temp_path("both-disp");
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[
                byte_rule(
                    "begin",
                    b"BEGIN",
                    vec![MatchAction::Record {
                        target: RecordTarget::Both,
                        control: RecordControl::Begin,
                    }],
                ),
                byte_rule(
                    "stop",
                    b"STOP",
                    vec![MatchAction::Record {
                        target: RecordTarget::Both,
                        control: RecordControl::Stop,
                    }],
                ),
            ])
            .with_recording_settings(RawRecordingSettings {
                destination: raw_path.clone(),
                channel_name: "both".to_string(),
                overwrite: OverwritePolicy::Refuse,
                timestamps: false,
                file_rotation: FileRotationPolicy::None,
                capacity: 64,
            })
            .with_display_recording_settings(DisplayRecordingSettings {
                destination: disp_path.clone(),
                channel_name: "both".to_string(),
                overwrite: OverwritePolicy::Refuse,
                file_rotation: FileRotationPolicy::None,
                capacity: 64,
                renderer: DisplayView::default(),
            });

        p.ingest(bytes_chunk(cid, b"BEGIN"));
        p.apply_pending_records().await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        assert_eq!(p.display_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"captured"));

        p.ingest(bytes_chunk(cid, b"STOP"));
        p.apply_pending_records().await;
        assert!(p.raw_recording_state().is_none());
        assert!(p.display_recording_state().is_none());
        p.finish().await;

        // Raw is byte-exact from the match forward (the BEGIN chunk itself is not
        // backfilled, §158); the .disp rendered the same span.
        assert_eq!(tokio::fs::read(&raw_path).await.unwrap(), b"capturedSTOP");
        let disp = tokio::fs::read_to_string(&disp_path).await.unwrap();
        assert!(disp.contains("captured"), "{disp:?}");
        let _ = tokio::fs::remove_file(&raw_path).await;
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn faulted_raw_retry_uses_the_latest_settings() {
        // A faulted recording still occupies the recorder slot; Begin must be
        // the user's retry, not a silent "already recording" no-op that
        // forces a channel restart. Fault deterministically via queue
        // overflow: capacity 1, and the recorder task is starved (no await
        // between ingests on the single-threaded test runtime).
        let cid = ChannelId::new();
        let first_path = temp_path("refault-first");
        let retry_path = temp_path("refault-retry");
        let first_settings = RawRecordingSettings {
            destination: first_path.clone(),
            channel_name: "refault".to_string(),
            overwrite: OverwritePolicy::Overwrite,
            timestamps: false,
            file_rotation: FileRotationPolicy::None,
            capacity: 1,
        };
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.set_recording(true, Some(first_settings.clone())).await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));

        p.ingest(bytes_chunk(cid, b"a"));
        p.ingest(bytes_chunk(cid, b"b"));
        p.ingest(bytes_chunk(cid, b"c")); // queue full → overflow fault
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Faulted));

        // The editor changed after the fault. Begin again must drop the dead
        // recorder and recreate it from the click-time settings, not reopen
        // the old destination merely because its handle still occupied the slot.
        let retry_settings = RawRecordingSettings {
            destination: retry_path.clone(),
            capacity: 64,
            ..first_settings
        };
        p.set_recording(true, Some(retry_settings)).await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"recovered"));

        p.finish().await;
        assert_eq!(
            tokio::fs::read(&retry_path).await.unwrap(),
            b"recovered",
            "the retry must use the latest destination and capacity"
        );
        let _ = tokio::fs::remove_file(&first_path).await;
        let _ = tokio::fs::remove_file(&retry_path).await;
    }

    #[tokio::test]
    async fn set_recording_begins_and_stops_raw_recording_live() {
        // ADR-012: live Record begin/stop without a restart, driven by set_recording
        // (no match rules). Shares the lazy begin / clean finalize path with the
        // match-rule Record action, so capture starts from the toggle forward and a
        // Stop finalizes byte-exactly.
        let cid = ChannelId::new();
        let path = temp_path("live");
        let mut p = pipeline(cid, PipelineCapacities::default()).with_recording_settings(
            RawRecordingSettings {
                destination: path.clone(),
                channel_name: "live".to_string(),
                overwrite: OverwritePolicy::Refuse,
                timestamps: false,
                file_rotation: FileRotationPolicy::None,
                capacity: 64,
            },
        );

        // Before enabling: nothing on disk, nothing recorded.
        p.ingest(bytes_chunk(cid, b"before "));
        assert!(p.raw_recording_state().is_none());

        // Live Begin: recording starts from here forward.
        p.set_recording(true, None).await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"captured"));

        // Begin is idempotent — a second enable while recording is a no-op.
        p.set_recording(true, None).await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"more"));

        // Live Stop finalizes; later data is not written.
        p.set_recording(false, None).await;
        assert!(p.raw_recording_state().is_none());
        p.ingest(bytes_chunk(cid, b"after"));
        p.finish().await;

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"capturedmore");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn recorder_stop_retires_detached_and_still_reports_the_outcome() {
        // §142's never-block-the-producer rule: a live Stop must not make the
        // acquisition loop wait for the recorder's backlog drain + file close.
        // The stop detaches; state reads "off" immediately, ingest continues,
        // and the honest outcome note (§56.1) lands via the loop's
        // non-blocking reap — not synchronously inside the stop.
        let cid = ChannelId::new();
        let path = temp_path("detached-stop");
        let mut p = pipeline(cid, PipelineCapacities::default()).with_recording_settings(
            RawRecordingSettings {
                destination: path.clone(),
                channel_name: "detached".to_string(),
                overwrite: OverwritePolicy::Refuse,
                timestamps: false,
                file_rotation: FileRotationPolicy::None,
                capacity: 64,
            },
        );
        p.set_recording(true, None).await;
        p.ingest(bytes_chunk(cid, b"data"));
        p.set_recording(false, None).await;

        // The stop returned with the retirement possibly still in flight:
        // the recording already reads off, and ingest keeps flowing.
        assert!(p.raw_recording_state().is_none());
        let queue = p
            .snapshot()
            .raw_recording_queue
            .expect("queue peak survives recorder teardown");
        assert!(queue.peak >= 1);
        assert_eq!(queue.capacity, 64);
        p.ingest(bytes_chunk(cid, b"while closing"));

        // The outcome is reported through the reap path.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            p.reap_retired_recordings();
            let noted = p
                .snapshot()
                .diagnostics
                .events
                .iter()
                .any(|d| d.message.contains("Raw recording stopped"));
            if noted {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "detached stop outcome never reported"
            );
            tokio::task::yield_now().await;
        }
        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"data", "capture ends exactly at the stop");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn two_rules_with_interleaved_offsets_both_splice_into_disp() {
        // Regression (renderer walker): apply_fired_rules collects annotations
        // rule-major, so two timestamped Mark rules firing in one chunk can
        // interleave offsets (rule ZZ at 5 collected before rule AA at 2). Both
        // timestamps must land in the .disp — the renderer sorts internally.
        use crate::config::{MarkPosition, MarkTimestamp};
        use crate::core::TimestampConfig;
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let disp_path = temp_path("interleaved-marks");
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mark = |sep: &str| MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: MarkPosition::Before,
                style: Default::default(),
                format: TimestampConfig::default(), // HH:MM:SS
                separator: sep.to_string(),
            }),
        };
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[
            byte_rule("zz", b"ZZ", vec![mark("|z|")]),
            byte_rule("aa", b"AA", vec![mark("|a|")]),
        ]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));

        // "AA" at offset 1 fires the SECOND rule; "ZZ" at offset 4 fires the
        // first — collected order [ZZ@4, AA@1], i.e. offsets out of order.
        p.ingest(bytes_chunk(cid, b"xAAxZZx"));
        p.finish().await;

        let disp_written = tokio::fs::read_to_string(&disp_path).await.unwrap();
        assert!(
            disp_written.contains("|a|AA"),
            "the lower-offset rule's timestamp splices too: {disp_written:?}"
        );
        assert!(disp_written.contains("|z|ZZ"), "{disp_written:?}");
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn after_mark_splices_after_the_complete_multibyte_match() {
        use crate::config::{MarkPosition, MarkTimestamp};
        use crate::core::TimestampConfig;
        use crate::record::{start_display_recording, DisplayFileRecorder};

        let cid = ChannelId::new();
        let disp_path = temp_path("after-complete-match");
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mark = MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: MarkPosition::After,
                style: Default::default(),
                format: TimestampConfig::default(),
                separator: "|".to_string(),
            }),
        };
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[byte_rule(
            "abc",
            b"ABC",
            vec![mark],
        )]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));

        p.ingest(bytes_chunk(cid, b"xABCy"));
        let snapshot = p.snapshot();
        p.finish().await;

        let mark = snapshot.matches[0].mark.as_ref().unwrap();
        assert_eq!(snapshot.matches[0].byte_offset, Some(1));
        assert_eq!(snapshot.matches[0].view_offset, Some(1));
        assert_eq!(mark.view_offset, Some(3));
        let written = tokio::fs::read_to_string(&disp_path).await.unwrap();
        assert!(
            written.starts_with("xABC"),
            "after mark split the match: {written:?}"
        );
        assert!(
            written.contains('|'),
            "timestamp separator missing: {written:?}"
        );
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn after_mark_on_a_boundary_split_lands_in_the_completing_chunk() {
        use crate::config::{MarkPosition, MarkTimestamp};
        use crate::core::TimestampConfig;
        use crate::record::{start_display_recording, DisplayFileRecorder};

        let cid = ChannelId::new();
        let disp_path = temp_path("after-boundary-match");
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mark = MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: MarkPosition::After,
                style: Default::default(),
                format: TimestampConfig::default(),
                separator: "|".to_string(),
            }),
        };
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[byte_rule(
            "abc",
            b"ABC",
            vec![mark],
        )]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));

        p.ingest(bytes_chunk(cid, b"xAB"));
        p.ingest(bytes_chunk(cid, b"Cy"));
        p.finish().await;

        let written = tokio::fs::read_to_string(&disp_path).await.unwrap();
        assert!(
            written.starts_with("xABC"),
            "split match was not kept whole: {written:?}"
        );
        assert!(
            written.contains('|'),
            "boundary mark was dropped: {written:?}"
        );
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn set_display_recording_begins_and_stops_display_recording_live() {
        // §54/ADR-012: the Display sibling of the live Raw toggle — lazy begin
        // from click-time settings, clean finalize on stop; the .disp captures
        // only the span in between.
        let cid = ChannelId::new();
        let path = temp_path("disp-live");
        let mut p = pipeline(cid, PipelineCapacities::default());

        p.ingest(bytes_chunk(cid, b"before "));
        assert!(p.display_recording_state().is_none());

        let settings = DisplayRecordingSettings {
            destination: path.clone(),
            channel_name: "disp-live".to_string(),
            overwrite: OverwritePolicy::Refuse,
            file_rotation: FileRotationPolicy::None,
            capacity: 64,
            renderer: DisplayView::default(),
        };
        p.set_display_recording(true, Some(settings)).await;
        assert_eq!(p.display_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"captured"));

        // Begin is idempotent — a second enable while recording is a no-op.
        p.set_display_recording(true, None).await;
        assert_eq!(p.display_recording_state(), Some(RecordingState::Enabled));

        // Stop finalizes; later data is not written.
        p.set_display_recording(false, None).await;
        assert!(p.display_recording_state().is_none());
        p.ingest(bytes_chunk(cid, b"after"));
        p.finish().await;

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("captured"), "{written:?}");
        assert!(
            !written.contains("before") && !written.contains("after"),
            "only the toggled span is recorded: {written:?}"
        );
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn display_begin_without_destination_reports_clean_setup_instruction() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());

        p.set_display_recording(true, None).await;

        assert_eq!(p.display_recording_state(), Some(RecordingState::Faulted));
        let error = p
            .diagnostics()
            .errors()
            .last()
            .expect("missing destination should report an error");
        assert_eq!(
            error.message,
            "can't begin Display recording: no destination is set — set one in the Record Display setup, then press Record again"
        );
    }

    #[tokio::test]
    async fn faulted_display_retry_uses_the_latest_settings() {
        // Display has the same retry contract as Raw: a faulted recorder is
        // inactive, so click-time settings may replace its old destination.
        let cid = ChannelId::new();
        let first_path = temp_path("disp-refault-first");
        let retry_path = temp_path("disp-refault-retry");
        let first_settings = DisplayRecordingSettings {
            destination: first_path.clone(),
            channel_name: "disp-refault".to_string(),
            overwrite: OverwritePolicy::Overwrite,
            file_rotation: FileRotationPolicy::None,
            capacity: 1,
            renderer: DisplayView::default(),
        };
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.set_display_recording(true, Some(first_settings.clone()))
            .await;
        assert_eq!(p.display_recording_state(), Some(RecordingState::Enabled));

        p.ingest(bytes_chunk(cid, b"a"));
        p.ingest(bytes_chunk(cid, b"b"));
        p.ingest(bytes_chunk(cid, b"c")); // queue full -> overflow fault
        assert_eq!(p.display_recording_state(), Some(RecordingState::Faulted));

        let retry_settings = DisplayRecordingSettings {
            destination: retry_path.clone(),
            capacity: 64,
            ..first_settings
        };
        p.set_display_recording(true, Some(retry_settings)).await;
        assert_eq!(p.display_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"recovered"));
        p.finish().await;

        let written = tokio::fs::read_to_string(&retry_path).await.unwrap();
        assert!(
            written.contains("recovered"),
            "the Display retry must use the latest destination: {written:?}"
        );
        let _ = tokio::fs::remove_file(&first_path).await;
        let _ = tokio::fs::remove_file(&retry_path).await;
    }

    #[tokio::test]
    async fn mark_action_annotates_the_display_recording_not_the_raw_stream() {
        // §50.2: Mark writes a marker into the display recording (`.disp`) but
        // never the raw byte stream, which stays byte-exact.
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let raw_path = temp_path("mark-raw");
        let disp_path = temp_path("mark-disp");
        let raw = RawFileRecorder::create(&raw_path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(start_raw_recording(raw, 64))
            .with_match_rules(&[byte_rule(
                "mark",
                b"HERE",
                vec![MatchAction::Mark { timestamp: None }],
            )]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));

        p.ingest(bytes_chunk(cid, b"data HERE data"));
        p.finish().await;

        let raw_written = tokio::fs::read(&raw_path).await.unwrap();
        assert_eq!(raw_written, b"data HERE data"); // byte-exact, no marker
        let disp_written = tokio::fs::read_to_string(&disp_path).await.unwrap();
        assert!(disp_written.contains("MARK rule="));
        let _ = tokio::fs::remove_file(&raw_path).await;
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn timestamped_mark_splices_inline_time_into_disp_not_raw() {
        // §50.2: a Mark carrying a timestamp splices the local arrival time inline,
        // before the matched pattern, into the display recording — never `.raw`.
        use crate::config::{MarkPosition, MarkTimestamp};
        use crate::core::TimestampConfig;
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let raw_path = temp_path("tsmark-raw");
        let disp_path = temp_path("tsmark-disp");
        let raw = RawFileRecorder::create(&raw_path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mark = MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: MarkPosition::Before,
                style: Default::default(),
                format: TimestampConfig::default(), // HH:MM:SS
                separator: String::new(),
            }),
        };
        // The default Display view is Raw/Native, so the disp render is the bytes
        // verbatim with the timestamp spliced before the '$'.
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(start_raw_recording(raw, 64))
            .with_match_rules(&[byte_rule("gga", b"$GPGGA", vec![mark])]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));

        p.ingest(bytes_chunk(cid, b"xx$GPGGA,1"));
        let snap = p.snapshot();
        p.finish().await;

        // Raw is byte-exact — no timestamp, no marker.
        assert_eq!(tokio::fs::read(&raw_path).await.unwrap(), b"xx$GPGGA,1");

        // The disp has an inline HH:MM:SS immediately before "$GPGGA".
        let disp_written = tokio::fs::read_to_string(&disp_path).await.unwrap();
        let at = disp_written.find("$GPGGA").expect("GGA rendered");
        let prefix = &disp_written[..at];
        // The 8 chars before "$GPGGA" look like a time (HH:MM:SS).
        let ts = &prefix[prefix.len() - 8..];
        let bytes = ts.as_bytes();
        assert!(
            bytes[2] == b':' && bytes[5] == b':',
            "expected HH:MM:SS, got {ts:?}"
        );

        // The snapshot carries the same inline mark for the live view.
        let mark = snap
            .matches
            .iter()
            .find_map(|m| m.mark.as_ref())
            .expect("mark render in snapshot");
        assert!(mark.before);
        assert_eq!(mark.text, ts);

        let _ = tokio::fs::remove_file(&raw_path).await;
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn zda_mark_splices_valid_custom_sentence_and_newline_only_into_display() {
        use crate::config::{MarkPosition, MarkTimestamp, MarkTimestampStyle};
        use crate::core::{zda_sentence, TimestampConfig};
        use crate::record::{start_display_recording, DisplayFileRecorder};

        let cid = ChannelId::new();
        let raw_path = temp_path("zda-mark-raw");
        let disp_path = temp_path("zda-mark-disp");
        let raw = RawFileRecorder::create(&raw_path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse)
            .await
            .unwrap();
        let mark = MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: MarkPosition::Before,
                style: MarkTimestampStyle::NmeaZda {
                    talker: "RECEIVER_A".to_string(),
                },
                format: TimestampConfig {
                    include_millis: true,
                    ..Default::default()
                },
                separator: "\r\n".to_string(),
            }),
        };
        let mut pipeline = pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(start_raw_recording(raw, 64))
            .with_match_rules(&[byte_rule("zda", b"$GP", vec![mark])]);
        pipeline.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));
        let arrival = ChunkTime {
            monotonic: Instant::now(),
            wall_clock: std::time::UNIX_EPOCH + Duration::from_millis(1_784_282_096_789),
            wall_clock_source: crate::core::ArrivalTimestampSource::PostRead,
        };
        pipeline.ingest(ReceivedData {
            channel_id: cid,
            payload: ReceivedPayload::Bytes(b"xx$GPGGA,1".to_vec()),
            received_at: arrival,
        });
        let snapshot = pipeline.snapshot();
        pipeline.finish().await;

        assert_eq!(tokio::fs::read(&raw_path).await.unwrap(), b"xx$GPGGA,1");
        let annotation = format!(
            "{}\r\n",
            zda_sentence("RECEIVER_A", arrival.wall_clock, true).unwrap()
        );
        assert_eq!(
            tokio::fs::read_to_string(&disp_path).await.unwrap(),
            format!("xx{annotation}$GPGGA,1")
        );
        let mark = snapshot
            .matches
            .iter()
            .find_map(|matched| matched.mark.as_ref())
            .expect("ZDA mark render in snapshot");
        assert_eq!(mark.text, annotation);
        nmea0183::NmeaSentence::parse(mark.text.trim_end_matches(['\r', '\n'])).unwrap();

        let _ = tokio::fs::remove_file(&raw_path).await;
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn transport_to_pipeline_queue_is_bounded() {
        // §99: the ingest queue is the bounded backpressure edge; try_send on a
        // full queue is refused rather than growing without bound.
        let (tx, _rx) = tokio::sync::mpsc::channel::<ReceivedData>(2);
        let cid = ChannelId::new();
        assert!(tx.try_send(bytes_chunk(cid, b"1")).is_ok());
        assert!(tx.try_send(bytes_chunk(cid, b"2")).is_ok());
        assert!(tx.try_send(bytes_chunk(cid, b"3")).is_err());
    }
}
