//! On-demand snapshots of a Channel's observable pipeline state (spec §10, §137;
//! listener ADR-006).
//!
//! The pipeline owns its stream scrollback, diagnostics, and recent match firings
//! by value inside the [`run_channel`](super::pipeline::run_channel) task, so they
//! are not directly readable while a Channel runs. A [`ChannelSnapshot`] is a
//! point-in-time, owned copy of the *small* observable state (diagnostics, matches,
//! liveness, view pause, the stream's end offset); the scrollback bytes themselves
//! are fetched separately and incrementally via [`StreamDelta`] so a high-throughput
//! viewer never re-ships the whole retained buffer (up to the 256 KB scroll cap —
//! §87, ADR-009).
//!
//! This is the *pull* half of the observability surface. The *push* half is the
//! [`RuntimeEvent`](crate::core::RuntimeEvent) stream, which stays authoritative
//! for presentation observers (ADR-006): a UI folds events for liveness and
//! requests a snapshot/stream-delta when it needs the actual retained content.
//! Observers never own or block the pipeline.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::oneshot;

use crate::config::DiskGuard;
use crate::core::{ChannelId, ChannelState, DisplayViewId, MatchRuleId, RecordingState};
use crate::diagnostics::{Diagnostic, DiagnosticSeverity};

use super::activity::ChannelActivity;
use super::pipeline::{DisplayRecordingSettings, RawRecordingSettings};
use super::run_summary::ListenerRunSummary;
use super::telemetry::{ChunkShape, DurationHistogram, IdleDeadlineTimerSummary, TransportHealth};

/// A query the pipeline task answers from its current state, replying on a
/// oneshot. Dropping the reply sender simply yields nothing.
///
/// Three granularities so observers pay only for what they show (listener ADR-006):
/// - [`Stats`](Self::Stats): O(1) counters for per-tab health. A multi-channel
///   overview polls this for *every* Channel.
/// - [`Snapshot`](Self::Snapshot): the small observable state (diagnostics, recent
///   matches, liveness, view pause, the stream end offset) — bounded, so it is cheap
///   even for the on-screen Channel. It carries **no** scrollback bytes.
/// - [`StreamDelta`](Self::StreamDelta): the scrollback bytes new since a cursor —
///   the only O(bytes) reply, and only of the *new* bytes, not the whole buffer.
pub enum PipelineRequest {
    Snapshot(oneshot::Sender<ChannelSnapshot>),
    Stats(oneshot::Sender<ChannelStats>),
    /// Incremental stream bytes since the requester's cursor (§87, ADR-009). The
    /// pipeline returns only what is new (or a reset window if the cursor fell
    /// behind eviction), so a live high-throughput viewer never re-ships or
    /// re-renders the whole ~1 MB scrollback each poll.
    StreamDelta {
        since: u64,
        reply: oneshot::Sender<StreamDelta>,
    },
    /// Begin or stop Raw recording on a running Channel without a restart (§50.2,
    /// ADR-012): the live counterpart of the match-rule `Record` action, driven by
    /// the same lazy begin/finalize path. `enabled = true` begins (idempotent if
    /// already recording); `false` stops and finalizes. `settings`, when present,
    /// apply the recording config the caller read at click time first — so a
    /// destination set *after* the channel started still records live, no restart
    /// needed. Fire-and-forget — the outcome surfaces through the next snapshot's
    /// recording state and, on a begin failure, a `RecordingFaulted` event (§55).
    SetRecording {
        enabled: bool,
        settings: Option<RawRecordingSettings>,
    },
    /// Begin or stop **Display** recording live (§54, ADR-012): the Raw variant's
    /// sibling, same lazy begin/finalize contract and fire-and-forget semantics.
    SetDisplayRecording {
        enabled: bool,
        settings: Option<DisplayRecordingSettings>,
    },
}

/// An incremental slice of a Channel's stream scrollback (§87), answering "what
/// stream bytes exist at or after offset `since`?". Offsets are absolute stream
/// positions (bytes received since Start, modulo display pause).
#[derive(Clone, Debug)]
pub struct StreamDelta {
    /// Opaque identity of the pipeline run that owns these offsets. Absolute
    /// offsets restart at zero for each run, so consumers use this to distinguish
    /// a genuine restart from a delayed, duplicate, or overlapping read.
    pub generation: u64,
    /// Absolute stream offset of `bytes[0]`. Normally equals the requested `since`;
    /// it is **greater** when the requester's cursor had already been evicted from
    /// the front of the bounded scrollback — a signal to the consumer to reset its
    /// view to this window rather than append.
    pub base_offset: u64,
    /// The new (or reset-window) bytes, oldest → newest.
    pub bytes: Arc<[u8]>,
    /// Absolute offset just past the last retained byte (`base_offset + bytes.len()`
    /// for a fresh fetch). The consumer stores this as its next cursor.
    pub end_offset: u64,
}

/// Occupancy of one bounded pipeline queue (§99, §124) — for stress testing and
/// backpressure diagnosis. `current` is the depth at snapshot time; `peak` is the
/// high-water mark since Start (the value that matters — a transient spike a 5 Hz poll
/// would miss); `capacity` is the bound. A `peak` approaching `capacity` means the queue
/// is backing up: the recorder/disk (or the reader) can't keep up, the precursor to a
/// reception stall or a recording-queue-overflow gap. The ingest queue counts chunks;
/// a recording queue counts bytes (ADR-043).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueDepth {
    pub current: usize,
    pub peak: usize,
    pub capacity: usize,
}

/// Where an automatic reconnect stands (§9.1, ADR-045), so status can say it
/// in words: "Reconnecting — attempt 3, next try in 8 s", "Gave up after 10
/// attempts".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconnectProgress {
    /// Waiting to make attempt number `attempt`, due in `next_try_in`.
    Retrying { attempt: u32, next_try_in: Duration },
    /// The policy's `max_attempts` ran out after `attempts` attempts.
    GaveUp { attempts: u32 },
}

/// What status shows about one running recording (§56.2): where it is writing,
/// how much, how much room is left, and its lasting faults.
///
/// The sizes come from a poll every few seconds, so they lag the file by up to
/// one poll; `None` until the first poll lands or while checks are failing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordingStatus {
    /// The file being written; `None` while opening or in a gap.
    pub current_file: Option<PathBuf>,
    /// Bytes in the current file.
    pub bytes_written: u64,
    /// Total size of this recording's files at its destination.
    pub total_size: Option<u64>,
    /// Free space on the destination's filesystem.
    pub free_space: Option<u64>,
    /// When the next rotation is due, when the recording rotates (§59).
    pub next_rotation: Option<SystemTime>,
    /// The disk guard, when one is configured (§56.2).
    pub guard: Option<DiskGuard>,
    /// Lasting fault: free space is below the guard's threshold. It clears
    /// once free space is 10% above the threshold (§56.2).
    pub low_disk: bool,
    /// Lasting fault: faults keep repeating, so retries have slowed. It
    /// clears when the recording is stopped (§56.1).
    pub unstable: bool,
}

impl RecordingStatus {
    /// Whether a lasting fault is active — the ones that stay on screen.
    pub fn has_lasting_fault(&self) -> bool {
        self.low_disk || self.unstable
    }
}

/// Cheap, O(1) health counters for a Channel — everything a multi-channel
/// overview needs per tab without cloning the stream scrollback (the expensive
/// part of a full [`ChannelSnapshot`]).
#[derive(Clone, Debug)]
pub struct ChannelStats {
    /// The Channel's effective lifecycle state (§8) at serve time, stamped by the
    /// orchestrator (the lifecycle authority — the pipeline can't know it and fills
    /// a placeholder). This is what makes the polled lane **self-correcting**
    /// (ADR-006): lifecycle `RuntimeEvent`s are advisory `try_send`s that can drop
    /// under load, and a consumer that folds only events would then show a stale
    /// state forever; the poll re-derives it.
    pub state: ChannelState,
    /// Whether an auto-reconnect is armed or in progress for a `Faulted` channel
    /// (§9.1, §162) — pending reads as "Reconnecting" in a UI. `false` once the
    /// backoff gives up (that is a plain `Faulted`). Orchestrator-stamped, like
    /// `state`.
    pub reconnect_pending: bool,
    /// Where an automatic reconnect stands, for status in words (§9.1);
    /// `None` when none is armed. Orchestrator-stamped.
    pub reconnect: Option<ReconnectProgress>,
    /// Liveness facts: rolling throughput, total bytes + last-data time (§91.1, §166).
    pub activity: ChannelActivity,
    /// Retained-diagnostic counts by severity (§88) — for per-tab health.
    pub event_count: usize,
    pub warning_count: usize,
    pub error_count: usize,
    /// Raw-recording state, or `None` when raw recording isn't attached (§53).
    pub raw_recording: Option<RecordingState>,
    /// Display-recording state (§54), or `None` when it isn't attached.
    pub display_recording: Option<RecordingState>,
    /// What status shows about the running Raw recording (§56.2), or `None`
    /// when none is running.
    pub raw_recording_status: Option<RecordingStatus>,
    /// The Display sibling of `raw_recording_status`.
    pub display_recording_status: Option<RecordingStatus>,
    /// How many `BytePattern` matches were recovered only because a pattern spanned
    /// a read-chunk boundary (§50.2) — the cross-chunk-carry measurement. A nonzero,
    /// rising count tells an operator that read boundaries are routinely splitting
    /// the patterns they search for (the "how often").
    pub match_boundary_saves: u64,
    /// Delay from the transport's post-read timestamp (captured before payload
    /// copying) until the pipeline began processing the chunk.
    pub ingest_delay: DurationHistogram,
    /// Approximate last-ten-seconds view of [`Self::ingest_delay`].
    pub recent_ingest_delay: DurationHistogram,
    /// Total synchronous time spent processing each chunk inside
    /// `ChannelPipeline::ingest`, accumulated for the current run.
    pub ingest_processing: DurationHistogram,
    /// Approximate last-ten-seconds view of [`Self::ingest_processing`].
    pub recent_ingest_processing: DurationHistogram,
    /// Cumulative read-chunk count, sizes, and post-read completion gaps.
    pub chunk_shape: ChunkShape,
    /// Transport-specific stall/loss counters with explicit support state.
    pub transport_health: TransportHealth,
    /// Cumulative Idle-rule firing lateness against monotonic deadlines.
    pub rule_timer_lateness: DurationHistogram,
    /// Approximate last-ten-seconds view of [`Self::rule_timer_lateness`].
    pub recent_rule_timer_lateness: DurationHistogram,
    /// Platform timer mechanism used for completed Idle deadline waits.
    pub idle_deadline_timer: IdleDeadlineTimerSummary,
    /// Depth of the Transport→Pipeline ingest queue (§99) — the edge that backpressures
    /// the reader. A rising `peak` is the first sign reception is outrunning processing.
    pub ingest_queue: QueueDepth,
    /// Depth of the Raw-recording queue **in bytes** (§56.1, ADR-043), or `None` when
    /// no recorder is attached. A `peak` near `capacity` precedes a queue-overflow
    /// gap — i.e. the disk can't keep up with the inflow.
    pub raw_recording_queue: Option<QueueDepth>,
}

/// A point-in-time, owned copy of one Channel's *small* observable pipeline state
/// (diagnostics, recent matches, liveness, view pause, the stream end offset).
///
/// Built by the pipeline task in response to a snapshot request, so reading it
/// neither blocks reception nor shares mutable pipeline state. Every field is
/// bounded (§88, §124) — the unbounded scrollback bytes are **not** here; they are
/// fetched incrementally via [`StreamDelta`] — so a snapshot is always cheap to
/// build and ship, even at high throughput.
#[derive(Clone, Debug)]
pub struct ChannelSnapshot {
    pub channel_id: ChannelId,
    /// Effective lifecycle state at serve time — see [`ChannelStats::state`].
    pub state: ChannelState,
    /// Auto-reconnect armed/in progress — see [`ChannelStats::reconnect_pending`].
    pub reconnect_pending: bool,
    /// See [`ChannelStats::reconnect`].
    pub reconnect: Option<ReconnectProgress>,
    /// Newest completed run for this stable Channel, retained across restarts.
    pub last_run_summary: Option<ListenerRunSummary>,
    /// One entry per Display View (§48), in creation order (default view first).
    pub display_views: Vec<DisplayViewSnapshot>,
    /// Retained diagnostics, separated by severity (§88).
    ///
    /// Shared rather than owned: the producer rebuilds it only when the log
    /// changes, so a poll of an unchanged log costs a pointer clone (§124).
    pub diagnostics: Arc<DiagnosticsSnapshot>,
    /// Raw-recording state, or `None` when raw recording isn't attached (§53).
    pub raw_recording: Option<RecordingState>,
    /// Display-recording state (§54), or `None` when it isn't attached.
    pub display_recording: Option<RecordingState>,
    /// What status shows about the running Raw recording (§56.2), or `None`
    /// when none is running.
    pub raw_recording_status: Option<RecordingStatus>,
    /// The Display sibling of `raw_recording_status`.
    pub display_recording_status: Option<RecordingStatus>,
    /// Liveness facts: rolling throughput + last-data time (§91.1, §166).
    pub activity: ChannelActivity,
    /// Recent Match Rule firings, oldest → newest, bounded (§50.2, §165) — a
    /// rolling window (the runtime keeps the last 256), not the full history. A
    /// GUI cross-references these (by `view_offset`) against the accumulated
    /// stream to annotate, and folds them into its own longer-lived store.
    pub matches: Vec<TriggeredMatch>,
    /// How many `BytePattern` matches were recovered only because a pattern spanned
    /// a read-chunk boundary (§50.2). The aggregate "how often" of the cross-chunk
    /// measurement; per-occurrence detail (where/why) is in `diagnostics`.
    pub match_boundary_saves: u64,
    /// Transport post-read to pipeline-start delay; see [`ChannelStats::ingest_delay`].
    pub ingest_delay: DurationHistogram,
    /// Approximate last-ten-seconds view; see [`ChannelStats::recent_ingest_delay`].
    pub recent_ingest_delay: DurationHistogram,
    /// Cumulative pipeline processing time; see [`ChannelStats::ingest_processing`].
    pub ingest_processing: DurationHistogram,
    /// Approximate last-ten-seconds processing view; see
    /// [`ChannelStats::recent_ingest_processing`].
    pub recent_ingest_processing: DurationHistogram,
    /// Cumulative chunk-shape facts; see [`ChannelStats::chunk_shape`].
    pub chunk_shape: ChunkShape,
    /// Transport-specific stall/loss counters; see [`ChannelStats::transport_health`].
    pub transport_health: TransportHealth,
    /// Idle-rule deadline lateness; see [`ChannelStats::rule_timer_lateness`].
    pub rule_timer_lateness: DurationHistogram,
    /// Recent deadline lateness; see [`ChannelStats::recent_rule_timer_lateness`].
    pub recent_rule_timer_lateness: DurationHistogram,
    /// Platform timer mechanism used for completed Idle deadline waits.
    pub idle_deadline_timer: IdleDeadlineTimerSummary,
    /// Absolute stream offset just past the last received byte (§87): the total
    /// bytes accepted into the scrollback since Start. The live viewer uses this as
    /// its cursor target and fetches the bytes themselves incrementally via
    /// [`PipelineRequest::StreamDelta`] — the big scrollback is **not** bundled into
    /// every snapshot (that was O(buffer) at 5 Hz).
    pub stream_end_offset: u64,
    /// Ingest queue occupancy (§99) — see [`ChannelStats::ingest_queue`].
    pub ingest_queue: QueueDepth,
    /// Raw-recording queue occupancy in bytes (§56.1), or `None` when no recorder is
    /// attached — see [`ChannelStats::raw_recording_queue`].
    pub raw_recording_queue: Option<QueueDepth>,
}

/// A single rule firing (§50.2). Records which rule fired and, for a data
/// condition, the **stream byte offset** it fired at (`None` for an `Idle`
/// firing, which is not tied to data). This is the observable record of a `Mark`
/// and of any rule's trigger; `Notify` also lands in diagnostics and every firing
/// emits a `MatchTriggered` event.
///
/// When the firing was a `Mark` carrying an inline annotation, `mark` holds the
/// formatted local time or NMEA ZDA text and its exact anchor, so a live viewer can
/// splice it into the rendered stream (§50.2) exactly as Display Recording does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggeredMatch {
    pub rule_id: MatchRuleId,
    /// Absolute **stream** offset of the match's first byte — counts every byte
    /// received since Start (`None` for an `Idle` firing). This is the offset
    /// diagnostics quote, and it equals the byte's position in a `.raw`
    /// recording that ran from Start.
    pub byte_offset: Option<u64>,
    /// The same byte's offset in the **view (scrollback) space** — the space
    /// `StreamDelta`/`stream_end_offset` use, which skips bytes received while
    /// the view was paused (§50), so it can lag `byte_offset` after a pause.
    /// This remains the diagnostic match-start position; an inline Mark uses
    /// `MarkRender::view_offset`, which may be the match's final byte. `None`
    /// when the byte never entered the view (it arrived while paused) or for `Idle`.
    pub view_offset: Option<u64>,
    #[allow(clippy::doc_markdown)]
    pub mark: Option<MarkRender>,
}

/// The inline annotation a `Mark` firing contributes to the rendered display and
/// Display Recording (§50.2). `text` is already formatted; `before` is its
/// placement around the byte at `view_offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkRender {
    pub text: String,
    pub before: bool,
    /// Absolute view-space offset of the byte the annotation is attached to.
    /// This is the match's first byte for `Before` and final byte for `After`.
    /// `None` when the matched bytes did not enter the paused view.
    pub view_offset: Option<u64>,
}

/// One Display View's snapshot: its identity and pause state (§50). The viewed
/// content is the shared stream scrollback (fetched via [`StreamDelta`]), rendered
/// per view.
#[derive(Clone, Debug)]
pub struct DisplayViewSnapshot {
    pub id: DisplayViewId,
    pub paused: bool,
}

/// Retained diagnostics by severity (§92–§95), oldest → newest within each.
#[derive(Clone, Debug, Default)]
pub struct DiagnosticsSnapshot {
    pub events: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    pub errors: Vec<Diagnostic>,
}

impl DiagnosticsSnapshot {
    /// Group a flat list of diagnostics back into the per-severity buckets (the inverse
    /// of [`into_sorted_vec`](Self::into_sorted_vec)). Used to rebuild a snapshot from a
    /// retained `Vec<Diagnostic>`.
    pub fn from_diagnostics(diagnostics: impl IntoIterator<Item = Diagnostic>) -> Self {
        let mut snap = Self::default();
        for d in diagnostics {
            match d.severity {
                DiagnosticSeverity::Event => snap.events.push(d),
                DiagnosticSeverity::Warning => snap.warnings.push(d),
                DiagnosticSeverity::Error => snap.errors.push(d),
            }
        }
        snap
    }

    /// Flatten all severities into one chronological `Vec` (oldest → newest). The single
    /// timeline a consumer that doesn't care about severity buckets wants — the retained
    /// log and the GUI render order both use this.
    pub fn into_sorted_vec(self) -> Vec<Diagnostic> {
        let mut all: Vec<_> = self
            .events
            .into_iter()
            .chain(self.warnings)
            .chain(self.errors)
            .collect();
        all.sort_by_key(|d| d.timestamp);
        all
    }

    /// [`into_sorted_vec`](Self::into_sorted_vec) for a shared snapshot, which a
    /// consumer holds behind an `Arc` and cannot consume.
    pub fn to_sorted_vec(&self) -> Vec<Diagnostic> {
        let mut all: Vec<_> = self
            .events
            .iter()
            .chain(&self.warnings)
            .chain(&self.errors)
            .cloned()
            .collect();
        all.sort_by_key(|d| d.timestamp);
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(secs: u64, sev: DiagnosticSeverity, msg: &str) -> Diagnostic {
        Diagnostic::at(sev, msg, UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn diagnostics_round_trip_through_grouping_and_flattening() {
        use DiagnosticSeverity::*;
        // Out-of-order across severities; from_diagnostics groups, into_sorted_vec
        // flattens back into one chronological timeline.
        let input = vec![
            at(3, Error, "boom"),
            at(1, Event, "started"),
            at(2, Warning, "slow"),
        ];
        let grouped = DiagnosticsSnapshot::from_diagnostics(input);
        assert_eq!(grouped.events.len(), 1);
        assert_eq!(grouped.warnings.len(), 1);
        assert_eq!(grouped.errors.len(), 1);

        let sorted = grouped.into_sorted_vec();
        let flat: Vec<&str> = sorted.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(flat, vec!["started", "slow", "boom"], "chronological order");
    }
}
