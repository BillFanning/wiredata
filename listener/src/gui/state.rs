//! The GUI view-model and its reducer (listener ADR-008).
//!
//! [`AppState`] is the egui App's entire model: a per-Channel [`ChannelView`]
//! folded from the [`UiUpdate`] stream. It is **pure** — no egui, no runtime — so
//! the fold is unit-tested directly. The App calls [`AppState::apply`] for each
//! update it drains, then lays widgets out by reading the resulting views.

use std::collections::HashMap;
use std::rc::Rc;

use crate::config::ChannelConfig;
use crate::config::LowDiskAction;
use crate::core::{
    ChannelId, ChannelState, MatchRuleId, RecordingState, RecordingTap, RuntimeEvent,
};
use crate::diagnostics::Diagnostic;
use crate::runtime::{ChannelSnapshot, RecordingStatus, TriggeredMatch};
use crate::transport::SerialControlLines;
use wiredata_ui::format::human_bytes;

use super::bridge::UiUpdate;

/// One inline Mark timestamp pinned to a view-space byte offset (§50.2), owned by
/// the GUI. The snapshot's `matches` is a bounded **rolling window** (the runtime
/// keeps the last 256 firings): deriving the on-screen splices from it directly
/// made a timestamp vanish as soon as its firing aged out — visibly, on a paused
/// view, where the bytes stay frozen while firings keep churning off-screen. So
/// firings are folded into this per-channel list instead, which lives exactly as
/// long as the annotated byte is in `stream_bytes`.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamMark {
    /// Absolute view-space offset of the annotated byte
    /// (`TriggeredMatch::view_offset` — the `StreamDelta` offset space).
    pub offset: u64,
    /// The firing rule — with `offset`, the dedup key across snapshots.
    pub rule_id: MatchRuleId,
    /// Splice the text before (`true`) or after the annotated byte.
    pub before: bool,
    /// The formatted timestamp text (separator included).
    pub text: String,
}

/// Safety cap on retained marks per channel, alongside the byte-window trim —
/// bounds memory if a dense rule marks nearly every byte of a large window.
const MAX_STREAM_MARKS: usize = 4096;

/// A Channel's lifecycle as the GUI understands it. Events are the *advisory*
/// push surface (a wake-up); the folded state is reconciled against the polled
/// snapshots/stats, which are authoritative (ADR-006, review round 2's polled
/// lifecycle truth).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelStatus {
    #[default]
    Stopped,
    Running,
    Faulted,
    Reconnecting,
}

/// One Channel's view-model: identity, derived status, the latest cheap liveness
/// facts, the most recent snapshot (diagnostics/matches) for detail panes, and the
/// incrementally-accumulated stream scrollback for the live viewer.
pub struct ChannelView {
    pub id: ChannelId,
    pub name: String,
    /// A one-line connection description (interface + endpoint).
    pub details: String,
    /// The channel's configuration, so the UI can edit it (e.g. change the port).
    /// Updated on reconfigure.
    pub config: ChannelConfig,
    pub status: ChannelStatus,
    /// Total bytes received since start (the Stream-oriented liveness counter — §18,
    /// ADR-009; Stream Mode has no Message count). From the latest snapshot/stats.
    pub bytes_total: u64,
    /// Rolling throughput from the latest snapshot (§91.1, §166).
    pub bytes_per_sec: f64,
    /// Retained per-severity diagnostic counts from the latest snapshot (§88), shown
    /// on the channel tab (#8).
    pub info: usize,
    pub warnings: usize,
    pub errors: usize,
    /// The reason the last command on this Channel failed (e.g. a bind conflict), shown
    /// on the ⚠ line above Configure and the channel tab. Cleared on a successful
    /// (re)start. `None` when there's nothing to report. (Not a diagnostic — the fault is
    /// recorded as an ERROR diagnostic by the runtime; this is just the inline status.)
    pub last_error: Option<String>,
    /// When `last_error` reports a **recording** fault, which lane it refers to —
    /// so only that lane's recovery (its `RecordingStarted`, or its polled state
    /// reading Enabled) clears the message; the other lane starting must not.
    /// `None` when `last_error` is absent or a non-recording (command) error.
    pub recording_fault: Option<RecordingTap>,
    /// The most recent snapshot, for detail panes (diagnostics, match firings,
    /// view pause, recording state). The stream bytes are *not* here — they
    /// accumulate separately in `stream_bytes` from incremental deltas. `None` until
    /// the first poll arrives.
    pub snapshot: Option<ChannelSnapshot>,
    /// The snapshot's diagnostics flattened into one chronological timeline,
    /// computed **once per snapshot arrival** (5 Hz) and shared by `Rc` — the
    /// detail pane reads it every frame, and clone+sorting ~1.5 k entries at
    /// repaint rate was the largest per-frame cost.
    pub sorted_diagnostics: Rc<Vec<Diagnostic>>,
    /// Live serial control/status lines (§161) while running; `None` otherwise.
    pub control_lines: Option<SerialControlLines>,
    /// Raw-recording state from the latest snapshot/stats (§53), or `None` when no
    /// recorder is attached. Drives the recording indicator in the detail pane.
    pub recording: Option<RecordingState>,
    /// Display-recording state from the latest snapshot/stats (§54) — the Record
    /// Display block's indicator. `None` when it isn't attached.
    pub display_recording: Option<RecordingState>,
    /// Bounded-queue occupancy from the latest snapshot/stats (§99) — for stress
    /// testing / backpressure diagnosis. Shown in the detail pane.
    pub ingest_delay: crate::runtime::DurationHistogram,
    pub recent_ingest_delay: crate::runtime::DurationHistogram,
    pub ingest_processing: crate::runtime::DurationHistogram,
    pub recent_ingest_processing: crate::runtime::DurationHistogram,
    pub chunk_shape: crate::runtime::ChunkShape,
    pub transport_health: crate::runtime::TransportHealth,
    pub rule_timer_lateness: crate::runtime::DurationHistogram,
    pub recent_rule_timer_lateness: crate::runtime::DurationHistogram,
    pub idle_deadline_timer: crate::runtime::IdleDeadlineTimerSummary,
    pub ingest_queue: crate::runtime::QueueDepth,
    pub raw_recording_queue: Option<crate::runtime::QueueDepth>,
    /// What status shows about each running recording (§56.2), including its
    /// lasting faults; `None` when that recording is not running.
    pub raw_recording_status: Option<RecordingStatus>,
    pub display_recording_status: Option<RecordingStatus>,
    /// Accumulated stream scrollback bytes for the live viewer (§87, ADR-009),
    /// grown incrementally from [`UiUpdate::StreamDelta`] so the driver never
    /// re-ships the whole buffer each poll. Capped (oldest dropped) at this channel's
    /// `view_prefs.scroll_buffer_bytes` — the same value the runtime retains.
    pub stream_bytes: std::collections::VecDeque<u8>,
    /// Inline Mark annotations pinned to the accumulated bytes (§50.2), sorted by
    /// `offset` (the renderer needs ascending annotations). Folded from snapshot
    /// firings by `merge_marks`; trimmed with the window;
    /// cleared on restart. See [`StreamMark`] for why this outlives the
    /// snapshot's bounded `matches` window.
    pub marks: Vec<StreamMark>,
    /// Bumped on every change to `marks` (merge, trim, clear). The stream-view
    /// row cache compares this before hashing the mark history, so an idle
    /// frame doesn't re-hash up to `MAX_STREAM_MARKS` entries just to conclude
    /// nothing moved.
    pub marks_version: u64,
    /// Next absolute stream offset to request — the cursor handed to
    /// `Listener::stream_delta`. Advances as deltas are folded.
    pub stream_cursor: u64,
    /// Absolute offset of `stream_bytes[0]`. Keeping the window start explicit
    /// avoids deriving it with subtraction after a discontinuity and makes the
    /// contiguous-window invariant directly testable.
    pub stream_base_offset: u64,
    /// Opaque pipeline-run identity carried by stream deltas. Offsets restart at
    /// zero on every run; this distinguishes that reset from stale same-run data.
    stream_generation: Option<u64>,
    /// Per-channel stream-view presentation (mode, ctrl-chars, font, colors — §42).
    /// Each channel renders independently; seeded from the config's first display view
    /// on add/load and folded back on save (see [`super::view_prefs`]).
    pub(crate) view_prefs: super::view_prefs::ViewPrefs,
}

impl ChannelView {
    fn new(id: ChannelId, name: String, details: String, config: ChannelConfig) -> Self {
        let view_prefs = super::view_prefs::ViewPrefs::from_config(&config);
        Self {
            id,
            name,
            details,
            config,
            view_prefs,
            status: ChannelStatus::Stopped,
            bytes_total: 0,
            bytes_per_sec: 0.0,
            info: 0,
            warnings: 0,
            errors: 0,
            last_error: None,
            recording_fault: None,
            snapshot: None,
            sorted_diagnostics: Rc::new(Vec::new()),
            control_lines: None,
            recording: None,
            display_recording: None,
            ingest_delay: crate::runtime::DurationHistogram::default(),
            recent_ingest_delay: crate::runtime::DurationHistogram::default(),
            ingest_processing: crate::runtime::DurationHistogram::default(),
            recent_ingest_processing: crate::runtime::DurationHistogram::default(),
            chunk_shape: crate::runtime::ChunkShape::default(),
            transport_health: crate::runtime::TransportHealth::default(),
            rule_timer_lateness: crate::runtime::DurationHistogram::default(),
            recent_rule_timer_lateness: crate::runtime::DurationHistogram::default(),
            idle_deadline_timer: crate::runtime::IdleDeadlineTimerSummary::default(),
            ingest_queue: crate::runtime::QueueDepth::default(),
            raw_recording_queue: None,
            raw_recording_status: None,
            display_recording_status: None,
            stream_bytes: std::collections::VecDeque::new(),
            marks: Vec::new(),
            marks_version: 0,
            stream_cursor: 0,
            stream_base_offset: 0,
            stream_generation: None,
        }
    }

    fn clear_stream_for_new_run(&mut self) {
        self.stream_bytes.clear();
        if !self.marks.is_empty() {
            self.marks.clear();
            self.marks_version += 1;
        }
        self.stream_cursor = 0;
        self.stream_base_offset = 0;
        self.stream_generation = None;
    }

    /// Fold a snapshot's recent firings into the persistent mark list. The
    /// snapshot window is bounded and rolling, so this must be **idempotent** (a
    /// firing appears in many consecutive snapshots — dedup on `(offset,
    /// rule_id)`) and **additive** (a firing evicted from the window must not
    /// take its on-screen timestamp with it). Firings without a mark or without
    /// a view position (paused-view bytes, idle rules) contribute nothing.
    fn merge_marks(&mut self, matches: &[TriggeredMatch]) {
        let mut changed = false;
        for m in matches {
            let Some(mark) = m.mark.as_ref() else {
                continue;
            };
            let Some(offset) = mark.view_offset else {
                continue;
            };
            // Sorted by offset: binary-search the equal-offset run for the dedup
            // check, and insert at its end to keep arrival order stable.
            let lo = self.marks.partition_point(|s| s.offset < offset);
            let run = self.marks[lo..]
                .iter()
                .take_while(|s| s.offset == offset)
                .count();
            if self.marks[lo..lo + run]
                .iter()
                .any(|s| s.rule_id == m.rule_id)
            {
                continue;
            }
            self.marks.insert(
                lo + run,
                StreamMark {
                    offset,
                    rule_id: m.rule_id,
                    before: mark.before,
                    text: mark.text.clone(),
                },
            );
            changed = true;
        }
        if self.marks.len() > MAX_STREAM_MARKS {
            let excess = self.marks.len() - MAX_STREAM_MARKS;
            self.marks.drain(..excess); // oldest (lowest offsets) first
            changed = true;
        }
        if changed {
            self.marks_version += 1;
        }
    }

    /// Fold an incremental stream delta (§87, ADR-009) into the accumulated view
    /// bytes. Appends new bytes; if the runtime's window had evicted past our cursor
    /// (`base_offset` jumped ahead), reset to the returned window. Caps the buffer.
    fn apply_stream_delta(
        &mut self,
        generation: u64,
        base_offset: u64,
        bytes: &[u8],
        end_offset: u64,
    ) {
        // StreamDelta is an internal runtime contract. Refuse a malformed range
        // defensively so bad metadata can never make the retained window incoherent.
        if end_offset.checked_sub(base_offset) != Some(bytes.len() as u64) {
            return;
        }

        if self.stream_generation != Some(generation) {
            // A fresh pipeline reuses offsets from zero. The generation makes this
            // reset reliable even if its advisory lifecycle event was dropped.
            self.stream_bytes.clear();
            if !self.marks.is_empty() {
                self.marks.clear();
                self.marks_version += 1;
            }
            self.stream_base_offset = base_offset;
            self.stream_cursor = base_offset;
            self.stream_generation = Some(generation);
        } else if end_offset <= self.stream_cursor {
            // A duplicate or stale same-run delta contributes no new bytes. In
            // particular, selecting a channel again may re-fetch an older window;
            // never append it or move the cursor backward.
            return;
        } else if base_offset > self.stream_cursor {
            // Our cursor was evicted and the runtime returned its current window.
            // Replace rather than append across the gap.
            self.stream_bytes.clear();
            self.stream_base_offset = base_offset;
            self.stream_cursor = base_offset;
        }

        // `base_offset <= cursor < end_offset`: append only the unseen suffix.
        // This also reconciles an overlapping re-fetch after channel selection.
        let Some(unseen) = self
            .stream_cursor
            .checked_sub(base_offset)
            .and_then(|count| usize::try_from(count).ok())
            .filter(|count| *count <= bytes.len())
        else {
            return;
        };
        self.stream_bytes.extend(bytes[unseen..].iter().copied());
        self.stream_cursor = end_offset;
        // Cap the GUI's accumulated copy at this channel's configured scroll buffer
        // (the same value the runtime retains — §87), so the viewer scrolls back
        // exactly as far as the setting allows.
        let cap = self.view_prefs.scroll_buffer_bytes;
        let overflow = self.stream_bytes.len().saturating_sub(cap);
        if overflow > 0 {
            self.stream_bytes.drain(..overflow);
            self.stream_base_offset += overflow as u64;
        }
        // Marks live exactly as long as their annotated byte: drop those whose
        // offset slid off the front of the window (including after a reset).
        let evicted = self
            .marks
            .partition_point(|s| s.offset < self.stream_base_offset);
        if evicted > 0 {
            self.marks.drain(..evicted);
            self.marks_version += 1;
        }
        debug_assert_eq!(
            self.stream_cursor.checked_sub(self.stream_base_offset),
            Some(self.stream_bytes.len() as u64),
            "stream byte window must remain contiguous"
        );
    }

    /// The accumulated stream bytes as a contiguous slice for rendering.
    pub fn stream_contiguous(&mut self) -> &[u8] {
        self.stream_bytes.make_contiguous()
    }
}

/// The GUI's whole model: Channels in registration order, folded from updates.
#[derive(Default)]
pub struct AppState {
    order: Vec<ChannelId>,
    views: HashMap<ChannelId, ChannelView>,
    /// A transient workspace-level status line — the result of the last profile
    /// Save/Load (e.g. "Saved profile.toml" or an error). `None` until one happens.
    workspace_status: Option<String>,
}

impl AppState {
    /// The Channels in registration order (for a stable list).
    pub fn channels(&self) -> impl Iterator<Item = &ChannelView> {
        self.order.iter().filter_map(|id| self.views.get(id))
    }

    /// Every lasting recording fault, in list order, one line each (§56.1,
    /// §56.2). They stay on screen until they clear, whichever Channel is
    /// selected, instead of scrolling away in the log.
    pub fn lasting_recording_faults(&self) -> Vec<String> {
        self.channels()
            .flat_map(|view| {
                [
                    (RecordingTap::Raw, &view.raw_recording_status),
                    (RecordingTap::Display, &view.display_recording_status),
                ]
                .into_iter()
                .filter_map(|(tap, status)| Some((tap, status.as_ref()?)))
                .flat_map(|(tap, status)| lasting_fault_lines(&view.name, tap, status))
                .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Look up one Channel's view-model.
    pub fn channel(&self, id: ChannelId) -> Option<&ChannelView> {
        self.views.get(&id)
    }

    /// Mutable access to a Channel's view — used by the detail pane to make its
    /// accumulated stream bytes contiguous for rendering (and to memoize rows).
    pub fn channel_mut(&mut self, id: ChannelId) -> Option<&mut ChannelView> {
        self.views.get_mut(&id)
    }

    /// The Channel adjacent to `id` in list order: the one above it, or — if `id` is
    /// first — the one below. `None` if `id` is unknown or the only Channel. Used to
    /// keep a selection after the selected Channel is removed (#2).
    pub fn neighbor(&self, id: ChannelId) -> Option<ChannelId> {
        let pos = self.order.iter().position(|c| *c == id)?;
        if pos > 0 {
            self.order.get(pos - 1).copied()
        } else {
            self.order.get(pos + 1).copied()
        }
    }

    /// The Channel before/after `id` in list order, wrapping around — for keyboard
    /// tab-cycling (#3). `None` if `id` is unknown.
    pub fn cycle(&self, id: ChannelId, forward: bool) -> Option<ChannelId> {
        let n = self.order.len();
        if n == 0 {
            return None;
        }
        let pos = self.order.iter().position(|c| *c == id)?;
        let next = if forward {
            (pos + 1) % n
        } else {
            (pos + n - 1) % n
        };
        self.order.get(next).copied()
    }

    /// The first Channel in list order, if any (for selecting something sensible
    /// when nothing is focused).
    pub fn first(&self) -> Option<ChannelId> {
        self.order.first().copied()
    }

    /// All Channel ids in list order (for bulk Start all / Stop all, #5).
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.order.clone()
    }

    /// Channels "Start all" should bring up: Stopped **or Faulted**. A faulted channel
    /// is included so that, after fixing what faulted it (e.g. freeing a bound port),
    /// "Start all" retries it — `CommitAndStart` normalizes Faulted→Stopped→Starting in
    /// the runtime, so no separate per-channel Retry is needed. Running/Reconnecting are
    /// excluded: a Start there would be an illegal transition (Reconnecting's only valid
    /// action mid-reconnect is Stop).
    pub fn startable_channel_ids(&self) -> Vec<ChannelId> {
        self.order
            .iter()
            .filter(|id| {
                self.views.get(id).is_some_and(|v| {
                    matches!(v.status, ChannelStatus::Stopped | ChannelStatus::Faulted)
                })
            })
            .copied()
            .collect()
    }

    /// Fold one update into the model.
    pub fn apply(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::ChannelAdded(id, name, details, config) => {
                if !self.views.contains_key(&id) {
                    self.order.push(id);
                    self.views
                        .insert(id, ChannelView::new(id, name, details, *config));
                }
            }
            UiUpdate::ChannelReconfigured(id, details, config) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.details = details;
                    // Mirror a renamed channel into the list row (#6).
                    view.name = config.name.as_str().to_string();
                    view.config = *config;
                }
            }
            UiUpdate::ChannelRenamed(id, name) => {
                if let Some(view) = self.views.get_mut(&id) {
                    // Keep both the list label and the editor's seed config in step,
                    // so re-opening the editor shows the new name (§6).
                    view.config.name = crate::core::ChannelName::new(name.clone());
                    view.name = name;
                }
            }
            UiUpdate::ChannelRemoved(id) => {
                self.views.remove(&id);
                self.order.retain(|c| *c != id);
            }
            UiUpdate::ChannelError(id, message) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.last_error = Some(message);
                    // The message now reports a command error, not a recording fault.
                    view.recording_fault = None;
                }
            }
            UiUpdate::Event(event) => self.apply_event(event),
            UiUpdate::Snapshot(id, snapshot) => {
                if let Some(view) = self.views.get_mut(&id) {
                    reconcile_status(view, snapshot.state, snapshot.reconnect_pending);
                    view.bytes_total = snapshot.activity.total_bytes;
                    view.bytes_per_sec = snapshot.activity.bytes_per_sec;
                    view.info = snapshot.diagnostics.events.len();
                    view.warnings = snapshot.diagnostics.warnings.len();
                    view.errors = snapshot.diagnostics.errors.len();
                    view.recording = snapshot.raw_recording;
                    view.display_recording = snapshot.display_recording;
                    view.ingest_delay = snapshot.ingest_delay;
                    view.recent_ingest_delay = snapshot.recent_ingest_delay;
                    view.ingest_processing = snapshot.ingest_processing;
                    view.recent_ingest_processing = snapshot.recent_ingest_processing;
                    view.chunk_shape = snapshot.chunk_shape;
                    view.transport_health = snapshot.transport_health;
                    view.rule_timer_lateness = snapshot.rule_timer_lateness;
                    view.recent_rule_timer_lateness = snapshot.recent_rule_timer_lateness;
                    view.idle_deadline_timer = snapshot.idle_deadline_timer;
                    view.ingest_queue = snapshot.ingest_queue;
                    view.raw_recording_queue = snapshot.raw_recording_queue;
                    view.raw_recording_status = snapshot.raw_recording_status.clone();
                    view.display_recording_status = snapshot.display_recording_status.clone();
                    // Pin this window's Mark annotations before the snapshot is
                    // replaced — the snapshot's matches roll over, the pins stay.
                    view.merge_marks(&snapshot.matches);
                    // Flatten+sort once per poll; the detail pane reads per frame.
                    view.sorted_diagnostics = Rc::new(snapshot.diagnostics.to_sorted_vec());
                    view.snapshot = Some(*snapshot);
                    clear_error_if_recording_ok(view);
                }
            }
            // Cheap per-tab health for non-selected channels (no scrollback bytes).
            UiUpdate::Stats(id, stats) => {
                if let Some(view) = self.views.get_mut(&id) {
                    reconcile_status(view, stats.state, stats.reconnect_pending);
                    view.bytes_total = stats.activity.total_bytes;
                    view.bytes_per_sec = stats.activity.bytes_per_sec;
                    view.info = stats.event_count;
                    view.warnings = stats.warning_count;
                    view.errors = stats.error_count;
                    view.recording = stats.raw_recording;
                    view.display_recording = stats.display_recording;
                    view.ingest_delay = stats.ingest_delay;
                    view.recent_ingest_delay = stats.recent_ingest_delay;
                    view.ingest_processing = stats.ingest_processing;
                    view.recent_ingest_processing = stats.recent_ingest_processing;
                    view.chunk_shape = stats.chunk_shape;
                    view.transport_health = stats.transport_health;
                    view.rule_timer_lateness = stats.rule_timer_lateness;
                    view.recent_rule_timer_lateness = stats.recent_rule_timer_lateness;
                    view.idle_deadline_timer = stats.idle_deadline_timer;
                    view.ingest_queue = stats.ingest_queue;
                    view.raw_recording_queue = stats.raw_recording_queue;
                    view.raw_recording_status = stats.raw_recording_status.clone();
                    view.display_recording_status = stats.display_recording_status.clone();
                    clear_error_if_recording_ok(view);
                }
            }
            UiUpdate::ControlLines(id, lines) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.control_lines = Some(lines);
                }
            }
            UiUpdate::StreamDelta(id, delta) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.apply_stream_delta(
                        delta.generation,
                        delta.base_offset,
                        &delta.bytes,
                        delta.end_offset,
                    );
                }
            }
            UiUpdate::ProfileSaved(path) => {
                let file = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("profile");
                self.workspace_status = Some(format!("Saved {file}"));
            }
            UiUpdate::ProfileLoaded(name) => {
                self.workspace_status = Some(format!("Loaded “{name}”"));
            }
            UiUpdate::ProfileError(message) => {
                self.workspace_status = Some(message);
            }
        }
    }

    /// The last profile Save/Load status line, if any (shown in the UI).
    pub fn workspace_status(&self) -> Option<&str> {
        self.workspace_status.as_deref()
    }

    /// Fold a forwarded `RuntimeEvent`. Events for Channels we have not registered
    /// (e.g. runtime-minted TCP connection channels, §16.4) are ignored for now —
    /// the list shows configured Channels; per-connection views come later.
    fn apply_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::ChannelStarted(id) => {
                self.set_status(id, ChannelStatus::Running);
                if let Some(view) = self.views.get_mut(&id) {
                    view.last_error = None; // a successful start clears the prior error
                    view.recording_fault = None;
                    // A fresh Start resets the runtime's stream offset to 0, so drop
                    // any accumulated bytes/cursor/marks from a previous run to avoid
                    // mixing old and new streams (§8.5).
                    view.clear_stream_for_new_run();
                    // Totals belong to a run (talker semantics): they survive
                    // Stop so an at-rest cross-check against the sender works,
                    // and zero the moment a new run begins — instantly, not on
                    // the first poll of the fresh pipeline.
                    view.bytes_total = 0;
                    view.bytes_per_sec = 0.0;
                    view.ingest_delay = crate::runtime::DurationHistogram::default();
                    view.recent_ingest_delay = crate::runtime::DurationHistogram::default();
                    view.ingest_processing = crate::runtime::DurationHistogram::default();
                    view.recent_ingest_processing = crate::runtime::DurationHistogram::default();
                    view.chunk_shape = crate::runtime::ChunkShape::default();
                    view.transport_health = crate::runtime::TransportHealth::default();
                    view.rule_timer_lateness = crate::runtime::DurationHistogram::default();
                    view.recent_rule_timer_lateness = crate::runtime::DurationHistogram::default();
                    view.idle_deadline_timer = crate::runtime::IdleDeadlineTimerSummary::default();
                }
            }
            RuntimeEvent::ChannelStopped(id) => {
                self.set_status(id, ChannelStatus::Stopped);
                if let Some(view) = self.views.get_mut(&id) {
                    view.control_lines = None; // no live lines while stopped
                    clear_live_pipeline_state(view);
                }
            }
            RuntimeEvent::ChannelFaulted(id) => {
                self.set_status(id, ChannelStatus::Faulted);
                if let Some(view) = self.views.get_mut(&id) {
                    // Neutralize the live-only indicators (recording/queues) — the pipeline
                    // is gone. The diagnostics log itself is left to the next snapshot: the
                    // runtime serves the faulted channel a snapshot carrying the retained
                    // fault ERROR, so the log/headline come from the runtime, not the GUI.
                    clear_live_pipeline_state(view);
                }
            }
            RuntimeEvent::ChannelReconnecting(id, _) => {
                self.set_status(id, ChannelStatus::Reconnecting)
            }
            RuntimeEvent::ChannelReconnected(id) => {
                self.set_status(id, ChannelStatus::Running);
                if let Some(view) = self.views.get_mut(&id) {
                    view.clear_stream_for_new_run();
                    view.ingest_delay = crate::runtime::DurationHistogram::default();
                    view.recent_ingest_delay = crate::runtime::DurationHistogram::default();
                    view.ingest_processing = crate::runtime::DurationHistogram::default();
                    view.recent_ingest_processing = crate::runtime::DurationHistogram::default();
                    view.chunk_shape = crate::runtime::ChunkShape::default();
                    view.transport_health = crate::runtime::TransportHealth::default();
                    view.rule_timer_lateness = crate::runtime::DurationHistogram::default();
                    view.recent_rule_timer_lateness = crate::runtime::DurationHistogram::default();
                    view.idle_deadline_timer = crate::runtime::IdleDeadlineTimerSummary::default();
                }
            }
            RuntimeEvent::ChannelReconnectGaveUp(id) => self.set_status(id, ChannelStatus::Faulted),
            // A recording fault (e.g. a begin that couldn't open the file — Refuse over
            // an existing file) surfaces inline so "Record now" gives feedback instead
            // of silently doing nothing. The specific reason is in the diagnostics log.
            RuntimeEvent::RecordingFaulted(id, tap) => {
                if let Some(view) = self.views.get_mut(&id) {
                    let msg = format!(
                        "{}: {} recording faulted — see Diagnostics for the reason \
                         (check the destination and on-exists policy)",
                        view.name,
                        tap.label()
                    );
                    view.last_error = Some(msg);
                    view.recording_fault = Some(tap);
                }
            }
            RuntimeEvent::RecordingStarted(id, tap) => {
                // This lane's recording now began OK — clear a prior recording fault
                // **on the same lane**. A recording fault leaves the Channel Running,
                // so ChannelStarted never re-fires to clear it; and the *other* lane
                // starting says nothing about this one, so it must not clear it.
                if let Some(view) = self.views.get_mut(&id) {
                    if view.recording_fault == Some(tap) {
                        view.last_error = None;
                        view.recording_fault = None;
                    }
                }
            }
            // `MessageReceived` no longer drives the list: liveness is byte-based now
            // (ADR-009), refreshed by the periodic snapshot/stats poll like throughput.
            // Other warning, disk, control-line, match, and TCP-connection events are
            // reflected through the periodic snapshot (or land in later panes); the list
            // view does not need them directly. `RuntimeEvent` is `#[non_exhaustive]`,
            // so this wildcard also keeps us forward-compatible.
            _ => {}
        }
    }

    fn set_status(&mut self, id: ChannelId, status: ChannelStatus) {
        if let Some(view) = self.views.get_mut(&id) {
            view.status = status;
        }
    }
}

/// Reconcile the view's derived status against the lifecycle state a poll served
/// (ADR-006). Lifecycle `RuntimeEvent`s are advisory `try_send`s and can drop under
/// load; the snapshot/stats poll always runs and carries the orchestrator's
/// effective state, so a dropped Started/Stopped/Faulted/Reconnect event
/// self-corrects within one poll instead of leaving the row stale forever.
///
/// `Faulted` splits on `reconnect_pending`: a fault the runtime is still retrying
/// reads as Reconnecting (matching the `ChannelReconnecting` event), a fault it
/// gave up on (or never retries) reads as Faulted — so neither direction depends
/// on the corresponding event having arrived. The transitional states are left
/// alone: they resolve within the runtime's own command call, and the event (or
/// the next poll) settles the row without flapping it through an intermediate.
fn reconcile_status(view: &mut ChannelView, state: ChannelState, reconnect_pending: bool) {
    view.status = match state {
        ChannelState::Running => ChannelStatus::Running,
        ChannelState::Stopped => ChannelStatus::Stopped,
        ChannelState::Faulted if reconnect_pending => ChannelStatus::Reconnecting,
        ChannelState::Faulted => ChannelStatus::Faulted,
        ChannelState::Starting | ChannelState::Stopping => return,
    };
}

/// Clear a recording-fault `last_error` once the snapshot/stats poll shows **the
/// faulted lane's** recording is actually enabled. The `RecordingStarted` event
/// already clears it, but events use `try_send` and can drop under load; the poll
/// always runs, so this guarantees a stale recording error doesn't outlive a
/// recording that's now working. Tap-aware: a Display fault is cleared only by the
/// Display lane reading Enabled (and vice versa) — the Raw lane recording happily
/// says nothing about a broken Display recording. A non-recording `last_error`
/// (`recording_fault == None`, e.g. a command error) is never cleared here.
fn clear_error_if_recording_ok(view: &mut ChannelView) {
    let lane_ok = match view.recording_fault {
        Some(RecordingTap::Raw) => view.recording == Some(RecordingState::Enabled),
        Some(RecordingTap::Display) => view.display_recording == Some(RecordingState::Enabled),
        None => false,
    };
    if lane_ok {
        view.last_error = None;
        view.recording_fault = None;
    }
}

/// Neutralize the view's **live-only** indicators when a channel stops or faults: the
/// recording state and the bounded-queue depths, which require a *running* pipeline to
/// be true. The **diagnostics snapshot is kept** so the last run's log stays visible
/// until the next snapshot replaces it (the runtime serves a stopped/faulted channel a
/// snapshot of its retained diagnostics). Byte totals/throughput are liveness facts kept
/// as-is.
fn clear_live_pipeline_state(view: &mut ChannelView) {
    view.recording = None;
    view.display_recording = None;
    view.ingest_queue = crate::runtime::QueueDepth::default();
    view.raw_recording_queue = None;
    view.raw_recording_status = None;
    view.display_recording_status = None;
}

/// One recording's lasting faults, a line each, naming the Channel and the
/// recording (§56.1, §56.2).
fn lasting_fault_lines(channel: &str, tap: RecordingTap, status: &RecordingStatus) -> Vec<String> {
    let what = format!("{channel} {} recording", tap.label());
    let mut lines = Vec::new();
    if status.low_disk {
        let free = status
            .free_space
            .map_or_else(String::new, |free| format!(" ({} free)", human_bytes(free)));
        let effect = match status.guard.map(|guard| guard.on_low) {
            Some(LowDiskAction::StopRecording) => {
                "is paused until free space recovers; reception continues"
            }
            _ => "continues, but the disk may fill",
        };
        lines.push(format!("Low disk{free}: {what} {effect}"));
    }
    if status.unstable {
        lines.push(format!(
            "Recording unstable: {what} keeps failing, so it retries less often until you stop it"
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::templates;
    use crate::runtime::snapshot::{ChannelSnapshot, DiagnosticsSnapshot};
    use crate::runtime::ChannelActivity;

    /// A `ChannelAdded` update with a throwaway config (the reducer tests don't
    /// inspect the config; the editor uses it).
    fn added(id: ChannelId, name: &str, details: &str) -> UiUpdate {
        UiUpdate::ChannelAdded(
            id,
            name.into(),
            details.into(),
            Box::new(templates::udp_template()),
        )
    }

    fn snapshot_with(
        id: ChannelId,
        total_bytes: u64,
        bps: f64,
        warnings: usize,
    ) -> ChannelSnapshot {
        ChannelSnapshot {
            channel_id: id,
            state: ChannelState::Running,
            reconnect_pending: false,
            last_run_summary: None,
            display_views: vec![],
            diagnostics: std::sync::Arc::new(DiagnosticsSnapshot {
                warnings: vec![crate::diagnostics::Diagnostic::warning("w"); warnings],
                ..DiagnosticsSnapshot::default()
            }),
            raw_recording: None,
            display_recording: None,
            activity: ChannelActivity {
                last_data_at: None,
                bytes_per_sec: bps,
                total_bytes,
            },
            matches: vec![],
            match_boundary_saves: 0,
            ingest_delay: crate::runtime::DurationHistogram::default(),
            recent_ingest_delay: crate::runtime::DurationHistogram::default(),
            ingest_processing: crate::runtime::DurationHistogram::default(),
            recent_ingest_processing: crate::runtime::DurationHistogram::default(),
            chunk_shape: crate::runtime::ChunkShape::default(),
            transport_health: crate::runtime::TransportHealth::default(),
            rule_timer_lateness: crate::runtime::DurationHistogram::default(),
            recent_rule_timer_lateness: crate::runtime::DurationHistogram::default(),
            idle_deadline_timer: crate::runtime::IdleDeadlineTimerSummary::default(),
            stream_end_offset: total_bytes,
            ingest_queue: crate::runtime::QueueDepth::default(),
            raw_recording_queue: None,
            raw_recording_status: None,
            display_recording_status: None,
        }
    }

    #[test]
    fn folds_registration_and_lifecycle() {
        let mut state = AppState::default();
        let id = ChannelId::new();

        state.apply(added(id, "udp", "UDP · test"));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Stopped);
        assert_eq!(state.channels().count(), 1);

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelFaulted(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Faulted);
    }

    #[test]
    fn reconnect_events_drive_status() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "serial", "Serial · COM3"));

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelReconnecting(id, 1)));
        assert_eq!(
            state.channel(id).unwrap().status,
            ChannelStatus::Reconnecting
        );
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelReconnected(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelReconnectGaveUp(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Faulted);
    }

    #[test]
    fn snapshot_fold_caches_the_sorted_diagnostics() {
        // The chronological flatten+sort happens once per snapshot arrival, not
        // per frame — the detail pane reads this cache.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        assert!(state.channel(id).unwrap().sorted_diagnostics.is_empty());

        state.apply(UiUpdate::Snapshot(
            id,
            Box::new(snapshot_with(id, 1, 0.0, 2)),
        ));
        assert_eq!(state.channel(id).unwrap().sorted_diagnostics.len(), 2);
    }

    #[test]
    fn snapshot_updates_liveness_and_is_retained() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::Snapshot(
            id,
            Box::new(snapshot_with(id, 4096, 42.0, 2)),
        ));
        let view = state.channel(id).unwrap();
        assert_eq!(view.bytes_total, 4096);
        assert_eq!(view.bytes_per_sec, 42.0);
        assert_eq!(view.warnings, 2);
        assert!(view.snapshot.is_some());
    }

    #[test]
    fn ingest_timing_is_retained_at_stop_and_reset_on_start() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        let mut snapshot = snapshot_with(id, 1, 0.0, 0);
        snapshot
            .ingest_delay
            .record(std::time::Duration::from_millis(4));
        snapshot
            .ingest_processing
            .record(std::time::Duration::from_millis(2));
        snapshot
            .recent_ingest_processing
            .record(std::time::Duration::from_millis(2));
        snapshot.chunk_shape.sizes.record(12);
        snapshot.transport_health.serial_stalls = Some(crate::runtime::SerialStallSummary {
            episodes: 1,
            total: std::time::Duration::from_millis(8),
            max: std::time::Duration::from_millis(8),
            active_for: None,
        });
        snapshot
            .rule_timer_lateness
            .record(std::time::Duration::from_millis(3));
        snapshot
            .recent_rule_timer_lateness
            .record(std::time::Duration::from_millis(3));

        state.apply(UiUpdate::Snapshot(id, Box::new(snapshot)));
        let view = state.channel(id).unwrap();
        assert_eq!(view.ingest_delay.sample_count(), 1);
        assert_eq!(view.ingest_processing.sample_count(), 1);
        assert_eq!(view.recent_ingest_processing.sample_count(), 1);
        assert_eq!(view.chunk_shape.chunk_count(), 1);
        assert_eq!(view.transport_health.serial_stalls.unwrap().episodes, 1);
        assert_eq!(view.rule_timer_lateness.sample_count(), 1);
        assert_eq!(view.recent_rule_timer_lateness.sample_count(), 1);

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStopped(id)));
        let view = state.channel(id).unwrap();
        assert_eq!(view.ingest_delay.sample_count(), 1);
        assert_eq!(
            view.ingest_processing.sample_count(),
            1,
            "the completed run remains inspectable at rest"
        );
        assert_eq!(view.recent_ingest_processing.sample_count(), 1);
        assert_eq!(view.chunk_shape.chunk_count(), 1);
        assert_eq!(view.transport_health.serial_stalls.unwrap().episodes, 1);
        assert_eq!(view.rule_timer_lateness.sample_count(), 1);
        assert_eq!(view.recent_rule_timer_lateness.sample_count(), 1);

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        let view = state.channel(id).unwrap();
        assert_eq!(view.ingest_delay.sample_count(), 0);
        assert_eq!(view.ingest_processing.sample_count(), 0);
        assert_eq!(view.recent_ingest_processing.sample_count(), 0);
        assert_eq!(view.chunk_shape.chunk_count(), 0);
        assert_eq!(
            view.transport_health,
            crate::runtime::TransportHealth::default()
        );
        assert_eq!(view.rule_timer_lateness.sample_count(), 0);
        assert_eq!(view.recent_rule_timer_lateness.sample_count(), 0);
    }

    #[test]
    fn faulting_keeps_the_diagnostics_log_but_clears_live_indicators() {
        // A fault keeps the diagnostics snapshot (so the last run's log persists across a
        // stop/start) but neutralizes the live-only indicators (recording, queues), which
        // require a running pipeline. Byte liveness is kept. The detail pane separately
        // ensures a faulted channel's *headline* shows the fault, not a stale snapshot.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        let mut snap = snapshot_with(id, 4096, 42.0, 0);
        snap.raw_recording = Some(RecordingState::Enabled);
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        assert!(state.channel(id).unwrap().snapshot.is_some());

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelFaulted(id)));
        let view = state.channel(id).unwrap();
        assert_eq!(view.status, ChannelStatus::Faulted);
        assert!(
            view.snapshot.is_some(),
            "the diagnostics log is kept across a fault"
        );
        assert_eq!(
            view.recording, None,
            "no recording indicator on a faulted channel"
        );
        assert_eq!(view.bytes_total, 4096, "byte liveness is kept");
    }

    /// A `Stats` update carrying just a lifecycle state (zeros elsewhere), for the
    /// poll-reconciliation tests.
    fn stats_with_state(id: ChannelId, state: ChannelState, reconnect_pending: bool) -> UiUpdate {
        UiUpdate::Stats(
            id,
            Box::new(crate::runtime::ChannelStats {
                state,
                reconnect_pending,
                activity: ChannelActivity {
                    last_data_at: None,
                    bytes_per_sec: 0.0,
                    total_bytes: 0,
                },
                event_count: 0,
                warning_count: 0,
                error_count: 0,
                raw_recording: None,
                display_recording: None,
                match_boundary_saves: 0,
                ingest_delay: crate::runtime::DurationHistogram::default(),
                recent_ingest_delay: crate::runtime::DurationHistogram::default(),
                ingest_processing: crate::runtime::DurationHistogram::default(),
                recent_ingest_processing: crate::runtime::DurationHistogram::default(),
                chunk_shape: crate::runtime::ChunkShape::default(),
                transport_health: crate::runtime::TransportHealth::default(),
                rule_timer_lateness: crate::runtime::DurationHistogram::default(),
                recent_rule_timer_lateness: crate::runtime::DurationHistogram::default(),
                idle_deadline_timer: crate::runtime::IdleDeadlineTimerSummary::default(),
                ingest_queue: crate::runtime::QueueDepth::default(),
                raw_recording_queue: None,
                raw_recording_status: None,
                display_recording_status: None,
            }),
        )
    }

    #[test]
    fn a_dropped_lifecycle_event_self_corrects_on_the_next_poll() {
        // ADR-006: lifecycle events are advisory try_sends. Simulate a dropped
        // ChannelFaulted (no event ever arrives) — the polled stats carry the
        // orchestrator's effective state and correct the row; a later Stopped
        // poll corrects again.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);

        state.apply(stats_with_state(id, ChannelState::Faulted, false));
        assert_eq!(
            state.channel(id).unwrap().status,
            ChannelStatus::Faulted,
            "polled state corrects a dropped ChannelFaulted"
        );

        state.apply(stats_with_state(id, ChannelState::Stopped, false));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Stopped);
    }

    #[test]
    fn polled_fault_with_reconnect_pending_reads_reconnecting() {
        // While the runtime is still retrying, the effective state is Faulted but
        // reconnect_pending distinguishes it — the row reads Reconnecting without
        // needing the ChannelReconnecting event; once the backoff gives up,
        // pending drops and the same polled state reads Faulted.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "serial", "Serial · COM3"));

        state.apply(stats_with_state(id, ChannelState::Faulted, true));
        assert_eq!(
            state.channel(id).unwrap().status,
            ChannelStatus::Reconnecting
        );
        state.apply(stats_with_state(id, ChannelState::Faulted, false));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Faulted);
    }

    #[test]
    fn transitional_polled_states_do_not_flap_the_row() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));

        // A poll racing a stop command may catch Stopping — leave the row alone;
        // the ChannelStopped event (or the next Stopped poll) settles it.
        state.apply(stats_with_state(id, ChannelState::Stopping, false));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);
        state.apply(stats_with_state(id, ChannelState::Starting, false));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);
    }

    #[test]
    fn lasting_recording_faults_name_the_channel_and_stay_until_they_clear() {
        // §56.1/§56.2: low disk and instability stay listed, whichever channel
        // is selected, until a poll shows them gone.
        use crate::config::{DiskGuard, DiskThreshold};
        let mut state = AppState::default();
        let (gps, ais) = (ChannelId::new(), ChannelId::new());
        state.apply(added(gps, "GPS", "UDP · test"));
        state.apply(added(ais, "AIS", "UDP · test"));
        assert!(state.lasting_recording_faults().is_empty());

        let mut snap = snapshot_with(gps, 0, 0.0, 0);
        snap.raw_recording_status = Some(RecordingStatus {
            free_space: Some(500_000),
            guard: Some(DiskGuard {
                min_free: DiskThreshold::Bytes { bytes: 1_000_000 },
                on_low: LowDiskAction::StopRecording,
            }),
            low_disk: true,
            ..RecordingStatus::default()
        });
        snap.display_recording_status = Some(RecordingStatus {
            unstable: true,
            ..RecordingStatus::default()
        });
        state.apply(UiUpdate::Snapshot(gps, Box::new(snap)));
        assert_eq!(
            state.lasting_recording_faults(),
            [
                "Low disk (500.000 kB free): GPS Raw recording is paused until free space \
                 recovers; reception continues",
                "Recording unstable: GPS Display recording keeps failing, so it retries \
                 less often until you stop it",
            ]
        );

        let mut snap = snapshot_with(gps, 0, 0.0, 0);
        snap.raw_recording_status = Some(RecordingStatus::default());
        state.apply(UiUpdate::Snapshot(gps, Box::new(snap)));
        assert!(state.lasting_recording_faults().is_empty());
    }

    #[test]
    fn recording_fault_clearing_is_tap_aware() {
        // Raw and Display recordings fault and recover independently: the Display
        // lane starting (event) or the Raw lane polling Enabled must not clear a
        // fault on the *other* lane — only the faulted lane's own recovery does.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::Event(RuntimeEvent::RecordingFaulted(
            id,
            RecordingTap::Display,
        )));
        assert!(state.channel(id).unwrap().last_error.is_some());

        // The RAW lane starting says nothing about the Display fault.
        state.apply(UiUpdate::Event(RuntimeEvent::RecordingStarted(
            id,
            RecordingTap::Raw,
        )));
        assert!(
            state.channel(id).unwrap().last_error.is_some(),
            "a Raw start must not clear a Display recording fault"
        );

        // Nor does a poll showing the RAW lane Enabled.
        let mut snap = snapshot_with(id, 0, 0.0, 0);
        snap.raw_recording = Some(RecordingState::Enabled);
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        assert!(
            state.channel(id).unwrap().last_error.is_some(),
            "the Raw lane polling Enabled must not clear a Display fault"
        );

        // The Display lane's own recovery clears it.
        state.apply(UiUpdate::Event(RuntimeEvent::RecordingStarted(
            id,
            RecordingTap::Display,
        )));
        assert!(state.channel(id).unwrap().last_error.is_none());
    }

    #[test]
    fn polled_recording_state_clears_only_the_faulted_lane() {
        // The event-lane clear can drop (try_send); the poll is the guaranteed
        // path. A Display fault clears when the poll shows the DISPLAY lane
        // Enabled — and a command error (no recording fault) is never cleared
        // by recording state at all.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::Event(RuntimeEvent::RecordingFaulted(
            id,
            RecordingTap::Display,
        )));
        let mut snap = snapshot_with(id, 0, 0.0, 0);
        snap.display_recording = Some(RecordingState::Enabled);
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        assert!(
            state.channel(id).unwrap().last_error.is_none(),
            "the faulted lane polling Enabled clears its fault"
        );

        // A command error is not a recording fault: recording states can't clear it.
        state.apply(UiUpdate::ChannelError(id, "illegal transition".into()));
        let mut snap = snapshot_with(id, 0, 0.0, 0);
        snap.raw_recording = Some(RecordingState::Enabled);
        snap.display_recording = Some(RecordingState::Enabled);
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        assert!(
            state.channel(id).unwrap().last_error.is_some(),
            "recording state must not clear a command error"
        );
    }

    #[test]
    fn totals_survive_stop_and_reset_on_start() {
        // Talker semantics: totals belong to a run — kept across Stop (so an
        // at-rest cross-check against the sender works), zeroed the moment a
        // new run begins.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::Snapshot(
            id,
            Box::new(snapshot_with(id, 22278, 42.0, 0)),
        ));

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStopped(id)));
        let view = state.channel(id).unwrap();
        assert_eq!(view.bytes_total, 22278, "totals survive Stop");

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        let view = state.channel(id).unwrap();
        assert_eq!(view.bytes_total, 0, "a fresh run zeroes the totals");
        assert_eq!(view.bytes_per_sec, 0.0);
    }

    fn generated_delta(
        generation: u64,
        base: u64,
        bytes: &[u8],
        end: u64,
    ) -> crate::runtime::StreamDelta {
        crate::runtime::StreamDelta {
            generation,
            base_offset: base,
            bytes: bytes.to_vec().into(),
            end_offset: end,
        }
    }

    fn delta(base: u64, bytes: &[u8], end: u64) -> crate::runtime::StreamDelta {
        generated_delta(1, base, bytes, end)
    }

    #[test]
    fn stream_deltas_accumulate_incrementally() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"alpha", 5))));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(5, b"bravo", 10))));
        let view = state.channel_mut(id).unwrap();
        assert_eq!(view.stream_contiguous(), b"alphabravo");
        assert_eq!(view.stream_cursor, 10);
    }

    #[test]
    fn overlapping_delta_behind_the_cursor_does_not_underflow() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::StreamDelta(
            id,
            Box::new(delta(0, b"0123456789", 10)),
        ));
        // This same-run delta is wholly behind the cursor. Appending it used to
        // leave 12 bytes under cursor 7 and made offset subtraction panic.
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(5, b"XX", 7))));
        let view = state.channel_mut(id).unwrap();
        assert_eq!(view.stream_contiguous(), b"0123456789");
        assert_eq!(view.stream_base_offset, 0);
        assert_eq!(view.stream_cursor, 10);
        assert_eq!(
            view.stream_cursor - view.stream_base_offset,
            view.stream_bytes.len() as u64
        );
    }

    #[test]
    fn malformed_delta_range_is_ignored_without_disturbing_the_window() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"live", 4))));

        // Three bytes cannot describe the claimed two-byte range.
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(4, b"bad", 6))));
        let view = state.channel_mut(id).unwrap();
        assert_eq!(view.stream_contiguous(), b"live");
        assert_eq!(view.stream_base_offset, 0);
        assert_eq!(view.stream_cursor, 4);
    }

    #[test]
    fn overlapping_delta_appends_only_its_unseen_suffix() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"abcde", 5))));

        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(2, b"cdefgh", 8))));
        let view = state.channel_mut(id).unwrap();
        assert_eq!(view.stream_contiguous(), b"abcdefgh");
        assert_eq!(view.stream_base_offset, 0);
        assert_eq!(view.stream_cursor, 8);
    }

    #[test]
    fn a_new_generation_replaces_old_bytes_even_without_a_lifecycle_event() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"old", 3))));

        state.apply(UiUpdate::StreamDelta(
            id,
            Box::new(generated_delta(2, 0, b"new", 3)),
        ));
        let view = state.channel_mut(id).unwrap();
        assert_eq!(view.stream_contiguous(), b"new");
        assert_eq!(view.stream_base_offset, 0);
        assert_eq!(view.stream_cursor, 3);
    }

    #[test]
    fn stream_delta_reset_on_eviction_replaces_rather_than_appends() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"old", 3))));
        // base_offset (10) jumped ahead of our cursor (3): the window was evicted, so
        // the view resets to the new window instead of leaving a gap.
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(10, b"new", 13))));
        let view = state.channel_mut(id).unwrap();
        assert_eq!(view.stream_contiguous(), b"new");
        assert_eq!(view.stream_base_offset, 10);
        assert_eq!(view.stream_cursor, 13);
    }

    fn mark_match(rule: MatchRuleId, view_offset: u64, text: &str) -> TriggeredMatch {
        TriggeredMatch {
            rule_id: rule,
            byte_offset: Some(view_offset),
            view_offset: Some(view_offset),
            mark: Some(crate::runtime::snapshot::MarkRender {
                text: text.into(),
                before: true,
                view_offset: Some(view_offset),
            }),
        }
    }

    #[test]
    fn marks_outlive_the_snapshot_match_window() {
        // The runtime's recent-matches ring is bounded and rolling: while a view
        // is paused, firings on invisible bytes churn the ring and evict the
        // visible firings — which used to make their on-screen timestamps vanish.
        // The GUI pins marks to its accumulated bytes, so the splice survives.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"abcdef", 6))));

        let rule = MatchRuleId::new();
        let mut snap = snapshot_with(id, 6, 0.0, 0);
        snap.matches = vec![mark_match(rule, 2, "[12:00:00]")];
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        assert_eq!(state.channel(id).unwrap().marks.len(), 1);

        // Re-delivering the same firing (it stays in the window across polls)
        // must not duplicate the pin.
        let mut again = snapshot_with(id, 6, 0.0, 0);
        again.matches = vec![mark_match(rule, 2, "[12:00:00]")];
        state.apply(UiUpdate::Snapshot(id, Box::new(again)));
        assert_eq!(
            state.channel(id).unwrap().marks.len(),
            1,
            "idempotent merge"
        );

        // Next poll: the firing has rolled out of the snapshot window (empty
        // matches) — the pinned mark stays with its byte.
        state.apply(UiUpdate::Snapshot(
            id,
            Box::new(snapshot_with(id, 6, 0.0, 0)),
        ));
        let view = state.channel(id).unwrap();
        assert_eq!(view.marks.len(), 1, "the mark outlives the rolling window");
        assert_eq!(view.marks[0].offset, 2);
    }

    #[test]
    fn mark_merge_uses_the_annotation_anchor_not_the_match_start() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"xABCy", 5))));

        let mut matched = mark_match(MatchRuleId::new(), 1, "[after]");
        matched.mark.as_mut().unwrap().view_offset = Some(3);
        let mut snapshot = snapshot_with(id, 5, 0.0, 0);
        snapshot.matches = vec![matched];
        state.apply(UiUpdate::Snapshot(id, Box::new(snapshot)));

        assert_eq!(state.channel(id).unwrap().marks[0].offset, 3);
    }

    #[test]
    fn marks_version_moves_with_the_mark_list_not_the_bytes() {
        // The stream-view cache trusts `marks_version` to decide whether the
        // mark history could have changed, so it must bump on every mark
        // mutation (merge / trim / clear) and stay put on plain byte appends.
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"abcdef", 6))));
        let v0 = state.channel(id).unwrap().marks_version;

        // A byte append with no mark activity: version unchanged.
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(6, b"gh", 8))));
        assert_eq!(state.channel(id).unwrap().marks_version, v0);

        // A merged mark bumps; re-delivering the same firing (no-op merge) doesn't.
        let rule = MatchRuleId::new();
        let mut snap = snapshot_with(id, 8, 0.0, 0);
        snap.matches = vec![mark_match(rule, 2, "[t]")];
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        let v1 = state.channel(id).unwrap().marks_version;
        assert_ne!(v1, v0, "merge bumps");
        let mut again = snapshot_with(id, 8, 0.0, 0);
        again.matches = vec![mark_match(rule, 2, "[t]")];
        state.apply(UiUpdate::Snapshot(id, Box::new(again)));
        assert_eq!(
            state.channel(id).unwrap().marks_version,
            v1,
            "idempotent re-merge does not bump"
        );

        // An eviction reset drops the mark with its byte: version bumps.
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(20, b"xyz", 23))));
        assert!(state.channel(id).unwrap().marks.is_empty());
        assert_ne!(state.channel(id).unwrap().marks_version, v1, "trim bumps");
    }

    #[test]
    fn marks_trim_with_the_window_and_clear_on_restart() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"abc", 3))));
        let mut snap = snapshot_with(id, 3, 0.0, 0);
        snap.matches = vec![mark_match(MatchRuleId::new(), 1, "[t]")];
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        assert_eq!(state.channel(id).unwrap().marks.len(), 1);

        // An eviction reset (base jumped past our cursor) replaces the window;
        // the mark's byte is gone, so the mark goes with it.
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(10, b"xyz", 13))));
        assert!(
            state.channel(id).unwrap().marks.is_empty(),
            "trimmed with its byte"
        );

        // A pinned mark in the new window is dropped by a restart, like the bytes.
        let mut snap = snapshot_with(id, 13, 0.0, 0);
        snap.matches = vec![mark_match(MatchRuleId::new(), 11, "[t]")];
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        assert_eq!(state.channel(id).unwrap().marks.len(), 1);
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        assert!(
            state.channel(id).unwrap().marks.is_empty(),
            "cleared on restart"
        );
    }

    #[test]
    fn restart_clears_accumulated_stream() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"before", 6))));
        // A fresh Start resets the runtime offset to 0; the view must drop the old
        // stream so post-restart bytes don't concatenate onto pre-restart ones.
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        let view = state.channel_mut(id).unwrap();
        assert!(view.stream_contiguous().is_empty());
        assert_eq!(view.stream_cursor, 0);
    }

    #[test]
    fn channel_error_is_recorded_and_cleared_on_a_successful_start() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · :9000"));
        assert!(state.channel(id).unwrap().last_error.is_none());
        assert_eq!(state.channel(id).unwrap().details, "UDP · :9000");

        // A failed start surfaces the reason.
        state.apply(UiUpdate::ChannelError(
            id,
            "failed to bind: address in use".into(),
        ));
        assert_eq!(
            state.channel(id).unwrap().last_error.as_deref(),
            Some("failed to bind: address in use")
        );

        // A later successful start clears it.
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        assert!(state.channel(id).unwrap().last_error.is_none());
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);
    }

    #[test]
    fn channel_reconfigured_updates_details_and_config() {
        use crate::config::InterfaceConfig;
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · :9000"));

        let mut new_config = templates::udp_template();
        if let InterfaceConfig::Udp(udp) = &mut new_config.interface {
            udp.port = 9100;
        }
        state.apply(UiUpdate::ChannelReconfigured(
            id,
            "UDP · :9100".into(),
            Box::new(new_config),
        ));

        let view = state.channel(id).unwrap();
        assert_eq!(view.details, "UDP · :9100");
        match &view.config.interface {
            InterfaceConfig::Udp(udp) => assert_eq!(udp.port, 9100),
            other => panic!("expected UDP, got {other:?}"),
        }
    }

    #[test]
    fn channel_removed_drops_it_from_the_model() {
        let mut state = AppState::default();
        let a = ChannelId::new();
        let b = ChannelId::new();
        state.apply(added(a, "a", "UDP"));
        state.apply(added(b, "b", "TCP"));

        state.apply(UiUpdate::ChannelRemoved(a));
        assert_eq!(state.channels().count(), 1);
        assert!(state.channel(a).is_none());
        assert!(state.channel(b).is_some());
    }

    #[test]
    fn events_for_unknown_channels_are_ignored() {
        let mut state = AppState::default();
        let known = ChannelId::new();
        state.apply(added(known, "a", "UDP"));

        // A runtime-minted (e.g. TCP connection) id we never registered.
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(
            ChannelId::new(),
        )));
        assert_eq!(state.channels().count(), 1);
        // The known channel is untouched.
        assert_eq!(state.channel(known).unwrap().status, ChannelStatus::Stopped);
    }

    #[test]
    fn registration_order_is_stable() {
        let mut state = AppState::default();
        let (a, b, c) = (ChannelId::new(), ChannelId::new(), ChannelId::new());
        state.apply(added(a, "a", "UDP"));
        state.apply(added(b, "b", "TCP"));
        state.apply(added(c, "c", "Serial"));
        // A duplicate add does not reorder or duplicate.
        state.apply(added(a, "a-again", "UDP"));

        let names: Vec<&str> = state.channels().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn neighbor_picks_the_channel_above_then_below() {
        let mut state = AppState::default();
        let (a, b, c) = (ChannelId::new(), ChannelId::new(), ChannelId::new());
        state.apply(added(a, "a", "UDP"));
        state.apply(added(b, "b", "TCP"));
        state.apply(added(c, "c", "Serial"));

        // Middle/last fall back to the one above; the first falls back to the next.
        assert_eq!(state.neighbor(b), Some(a));
        assert_eq!(state.neighbor(c), Some(b));
        assert_eq!(state.neighbor(a), Some(b));

        // The sole remaining channel has no neighbor.
        state.apply(UiUpdate::ChannelRemoved(b));
        state.apply(UiUpdate::ChannelRemoved(c));
        assert_eq!(state.neighbor(a), None);
        assert_eq!(state.neighbor(ChannelId::new()), None); // unknown
    }

    #[test]
    fn cycle_wraps_in_both_directions() {
        let mut state = AppState::default();
        let (a, b, c) = (ChannelId::new(), ChannelId::new(), ChannelId::new());
        state.apply(added(a, "a", "UDP"));
        state.apply(added(b, "b", "TCP"));
        state.apply(added(c, "c", "Serial"));

        assert_eq!(state.cycle(a, true), Some(b));
        assert_eq!(state.cycle(c, true), Some(a)); // wrap forward
        assert_eq!(state.cycle(a, false), Some(c)); // wrap back
        assert_eq!(state.first(), Some(a));
    }

    #[test]
    fn renamed_updates_both_the_label_and_the_editor_seed() {
        let mut state = AppState::default();
        let a = ChannelId::new();
        state.apply(added(a, "a", "UDP"));

        state.apply(UiUpdate::ChannelRenamed(a, "Bridge".into()));
        let v = state.channel(a).unwrap();
        assert_eq!(v.name, "Bridge");
        assert_eq!(v.config.name.as_str(), "Bridge");
    }

    #[test]
    fn start_all_includes_faulted_channels_so_a_freed_resource_retries() {
        // After a bind fail the channel is Faulted; "Start all" must still pick it up
        // (the runtime normalizes Faulted→Stopped→Starting), or freeing the port and
        // clicking Start all again would do nothing.
        let mut state = AppState::default();
        let stopped = ChannelId::new();
        let faulted = ChannelId::new();
        let running = ChannelId::new();
        let reconnecting = ChannelId::new();
        for (id, name) in [
            (stopped, "stopped"),
            (faulted, "faulted"),
            (running, "running"),
            (reconnecting, "reconnecting"),
        ] {
            state.apply(added(id, name, "UDP"));
        }
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelFaulted(faulted)));
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(running)));
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelReconnecting(
            reconnecting,
            1,
        )));

        let startable = state.startable_channel_ids();
        assert!(startable.contains(&stopped));
        assert!(
            startable.contains(&faulted),
            "faulted must be retried by Start all"
        );
        assert!(!startable.contains(&running), "running is already up");
        assert!(
            !startable.contains(&reconnecting),
            "reconnecting can't legally Start (only Stop)"
        );
    }
}
