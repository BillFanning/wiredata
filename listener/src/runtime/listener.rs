//! The runtime orchestrator (spec §10, §13, §97.1, §113).
//!
//! [`Listener`] owns the Channel registry and drives the [`ChannelState`]
//! lifecycle (§8/§9): it builds live transports from validated config (via
//! [`super::build`]), opens/binds them at Start (resource failures → `Faulted`,
//! §71/§8.2), wires the stream pipeline, and stops them on request. All channels
//! share one [`RuntimeEvent`] stream (§137).
//!
//! Commands are exposed as async methods (`start`/`stop`/`apply_pending`, …) —
//! this method API *is* the command surface (there is no separate command enum;
//! ADR-012). Raw and Display recording are wired for serial/UDP channels from the
//! independent `raw_recording`/`display_recording` configs (ADR-013); a
//! recording-enable failure records a diagnostic and emits `RecordingFaulted` without
//! faulting the Channel (§55) — reception continues. Accepted TCP **connection**
//! channels run the same stream pipeline as their listener (§16.2); per-connection
//! recording and snapshots remain deferred (§59 filename templates; the supervisor
//! keeps no per-connection handle).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;

use crate::config::schema::InterfaceConfig;
use crate::config::ChannelConfig;
use crate::core::{lock_recover, ChannelId, ChannelState, DisplayViewId, RuntimeEvent};
use crate::display::DisplayView;
use crate::transport::{
    DataTransportRunner, SerialControlCommand, SerialControlHooks, SerialControlLines,
    SerialStallState, TransportNotice,
};

use super::activity::ChannelActivity;
use super::build::{build_display_view, build_serial, build_tcp_listener, build_udp, BuildError};
use super::channel::{spawn_monitored_channel, MatchSetup, MonitoredChannel, TRANSPORT_NOTICES};
use super::pipeline::{
    DisplayRecordingSettings, DisplayViewHandle, PipelineCapacities, RawRecordingSettings,
};
use super::run_summary::{ListenerRunSummary, RunEndReason, RunId};
use super::snapshot::{ChannelSnapshot, ChannelStats, DiagnosticsSnapshot, StreamDelta};
use super::tcp::{start_tcp_listener, TcpListenerHandle};
use super::telemetry::{ChunkShape, DurationHistogram, IdleDeadlineTimerSummary, TransportHealth};

/// How many Display Views a Channel runs (§48): one per configured view, at
/// least one (the pipeline's default view).
fn view_count(config: &ChannelConfig) -> usize {
    config.display.views.len().max(1)
}

/// A minimal [`ChannelSnapshot`] for a stopped/faulted Channel (no live pipeline):
/// retained diagnostics plus the completed run's activity and timing summaries.
fn retained_snapshot(id: ChannelId, channel: &ManagedChannel) -> ChannelSnapshot {
    ChannelSnapshot {
        channel_id: id,
        // Placeholders — the caller stamps the effective lifecycle state, exactly
        // as it does over a live pipeline's snapshot.
        state: ChannelState::Stopped,
        reconnect_pending: false,
        last_run_summary: None,
        display_views: Vec::new(),
        diagnostics: Arc::clone(&channel.retained_diagnostics_snapshot),
        raw_recording: None,
        display_recording: None,
        // The last run's liveness facts, exact at rest (`finish_stop` zeroed
        // the rate). Without this a stopped channel's byte total read 0 on
        // the next poll — precisely when the user cross-checks it against
        // the sender's total.
        activity: channel.retained_activity,
        matches: Vec::new(),
        match_boundary_saves: channel.retained_boundary_saves,
        ingest_delay: channel.retained_ingest_delay,
        recent_ingest_delay: channel.retained_recent_ingest_delay,
        ingest_processing: channel.retained_ingest_processing,
        recent_ingest_processing: channel.retained_recent_ingest_processing,
        chunk_shape: channel.retained_chunk_shape,
        transport_health: channel.retained_transport_health,
        rule_timer_lateness: channel.retained_rule_timer_lateness,
        recent_rule_timer_lateness: channel.retained_recent_rule_timer_lateness,
        idle_deadline_timer: channel.retained_idle_deadline_timer,
        stream_end_offset: 0,
        ingest_queue: crate::runtime::QueueDepth::default(),
        raw_recording_queue: None,
        raw_recording_status: None,
        display_recording_status: None,
    }
}

/// The [`ChannelStats`] counterpart of [`retained_snapshot`]: cheap per-tab health
/// for a Channel with no live pipeline, so the polled stats lane keeps serving
/// (and self-correcting) the lifecycle state and the retained totals after a
/// stop or fault instead of going silent.
fn retained_stats(channel: &ManagedChannel) -> ChannelStats {
    let (mut events, mut warnings, mut errors) = (0, 0, 0);
    for d in &channel.retained_diagnostics {
        match d.severity {
            crate::diagnostics::DiagnosticSeverity::Event => events += 1,
            crate::diagnostics::DiagnosticSeverity::Warning => warnings += 1,
            crate::diagnostics::DiagnosticSeverity::Error => errors += 1,
        }
    }
    ChannelStats {
        // Placeholders — the caller stamps the effective lifecycle state.
        state: ChannelState::Stopped,
        reconnect_pending: false,
        activity: channel.retained_activity,
        event_count: events,
        warning_count: warnings,
        error_count: errors,
        raw_recording: None,
        display_recording: None,
        match_boundary_saves: channel.retained_boundary_saves,
        ingest_delay: channel.retained_ingest_delay,
        recent_ingest_delay: channel.retained_recent_ingest_delay,
        ingest_processing: channel.retained_ingest_processing,
        recent_ingest_processing: channel.retained_recent_ingest_processing,
        chunk_shape: channel.retained_chunk_shape,
        transport_health: channel.retained_transport_health,
        rule_timer_lateness: channel.retained_rule_timer_lateness,
        recent_rule_timer_lateness: channel.retained_recent_rule_timer_lateness,
        idle_deadline_timer: channel.retained_idle_deadline_timer,
        ingest_queue: crate::runtime::QueueDepth::default(),
        raw_recording_queue: None,
        raw_recording_status: None,
        display_recording_status: None,
    }
}

/// Bounded inbox for live serial control-line commands (§161). Tiny: commands are
/// occasional operator actions.
const SERIAL_CONTROL_COMMANDS: usize = 8;

/// A live Channel's running tasks. Held by the orchestrator so it can stop them.
enum ChannelHandle {
    Data(MonitoredChannel),
    TcpListener(TcpListenerHandle),
}

/// Live serial control-line handle held by the orchestrator (§161): the command
/// inbox to the running serial reader, and the shared cell the reader updates with
/// the current line state.
struct SerialControl {
    commands: mpsc::Sender<SerialControlCommand>,
    state: Arc<Mutex<SerialControlLines>>,
}

/// Resources assembled while opening a stream transport and consumed together
/// when its monitored pipeline is spawned.
struct DataSpawnContext {
    faulted: Arc<AtomicBool>,
    serial_stall_state: Option<SerialStallState>,
    notices: (
        mpsc::Sender<TransportNotice>,
        mpsc::Receiver<TransportNotice>,
    ),
}

/// Auto-reconnect backoff state for one faulted Channel (§9.1, §162). Created when
/// `reconnect_tick` first sees the fault; cleared on a successful reconnect or a
/// manual stop.
#[derive(Clone)]
struct ReconnectState {
    /// Reconnect attempts made so far (each a Stop+Start cycle).
    attempts: u32,
    /// The current backoff delay (grows by `multiplier`, capped at `max_backoff`).
    backoff: Duration,
    /// When the next attempt is due.
    next_attempt_at: Instant,
    /// Whether `max_attempts` was exhausted (stop retrying, stay Faulted).
    gave_up: bool,
}

/// Start anchors for one successfully opened run. Wall time is reportable;
/// monotonic time makes elapsed duration immune to wall-clock corrections.
struct ActiveRun {
    id: RunId,
    started_at: SystemTime,
    started_monotonic: Instant,
}

impl ActiveRun {
    fn begin() -> Self {
        Self {
            id: RunId::mint(),
            started_at: SystemTime::now(),
            started_monotonic: Instant::now(),
        }
    }
}

fn transport_name(interface: &InterfaceConfig) -> &'static str {
    match interface {
        InterfaceConfig::Serial(_) => "serial",
        InterfaceConfig::Udp(_) => "udp",
        InterfaceConfig::TcpListener(_) => "tcp_listener",
    }
}

/// Registry entry: the configuration, any accepted-but-unapplied change (§13),
/// the lifecycle state, and the live handle — usually present while Running, though a
/// spontaneous fault can leave a handle attached (effective state Faulted) until a
/// stop/recovery consumes it.
struct ManagedChannel {
    config: ChannelConfig,
    pending: Option<ChannelConfig>,
    state: ChannelState,
    handle: Option<ChannelHandle>,
    /// Pause handles for the running Channel's Display Views (§11, §48); empty
    /// while Stopped.
    display_handles: Vec<DisplayViewHandle>,
    /// Shared fault flag (ADR-006). The detached fault monitor flips it when the
    /// transport ends on a spontaneous fault; the orchestrator can't be mutated
    /// from that task, so it reads the flag to keep `state()` and command
    /// validation honest. A fresh flag is installed at each `start`.
    faulted: Arc<AtomicBool>,
    /// Live serial control-line handle (§161); `Some` only while a serial Channel
    /// is running.
    serial_control: Option<SerialControl>,
    /// Auto-reconnect backoff state (§9.1, §162); `Some` while a reconnect is
    /// pending for a faulted Channel with reconnect enabled.
    reconnect_state: Option<ReconnectState>,
    /// Diagnostics retained across runs and faults for this Channel (§88, within
    /// session): the last run's pipeline diagnostics (folded in at stop) plus any
    /// start-time fault that never ran a pipeline (e.g. a bind conflict). Served by
    /// [`snapshot`](Listener::snapshot) when the Channel isn't running (so the GUI shows
    /// the log of a stopped/faulted Channel) and replayed into the next run's pipeline by
    /// [`prior_diagnostics`](Listener::prior_diagnostics). Bounded on use.
    retained_diagnostics: Vec<crate::diagnostics::Diagnostic>,
    /// The same retained log in the form [`snapshot`](Listener::snapshot) serves,
    /// rebuilt only when the retained entries change (§124). A stopped Channel is
    /// polled just as often as a running one, so regrouping per poll would keep
    /// paying for a log that by definition can no longer change on its own.
    retained_diagnostics_snapshot: Arc<DiagnosticsSnapshot>,
    /// The newest retained start fault and how often it has repeated, while it
    /// is still the newest retained entry. Cleared when a run's log replaces
    /// the retained list.
    repeated_start_fault: Option<RepeatedStartFault>,
    /// The last run's liveness facts, retained across Stop like the
    /// diagnostics — a stopped channel keeps reading its exact byte total at
    /// rest (the natural moment to cross-check against the sender). Replaced
    /// by each run's final snapshot with the rate zeroed; the *next start*
    /// is what resets the readout (talker semantics).
    retained_activity: ChannelActivity,
    /// The last run's boundary-save total, retained like the activity.
    retained_boundary_saves: u64,
    /// The last run's post-read-to-pipeline timing summary, retained at rest.
    retained_ingest_delay: DurationHistogram,
    retained_recent_ingest_delay: DurationHistogram,
    retained_ingest_processing: DurationHistogram,
    retained_recent_ingest_processing: DurationHistogram,
    retained_chunk_shape: ChunkShape,
    retained_transport_health: TransportHealth,
    retained_rule_timer_lateness: DurationHistogram,
    retained_recent_rule_timer_lateness: DurationHistogram,
    retained_idle_deadline_timer: IdleDeadlineTimerSummary,
    /// Start anchors for the currently running Channel, if one opened successfully.
    active_run: Option<ActiveRun>,
    /// Newest completed run, retained across ordinary restarts.
    last_run_summary: Option<ListenerRunSummary>,
}

/// A start fault as first reported, and how many times in a row it has occurred.
struct RepeatedStartFault {
    message: String,
    count: u64,
}

impl ManagedChannel {
    /// The state a caller should see: a tripped fault flag overrides the stored
    /// lifecycle state, so a spontaneous transport fault reads as `Faulted`
    /// without waiting for a command to reconcile it (ADR-006).
    fn effective_state(&self) -> ChannelState {
        if self.faulted.load(Ordering::Relaxed) {
            ChannelState::Faulted
        } else {
            self.state
        }
    }

    /// Retain one diagnostic that never reached a pipeline log — a start-time
    /// fault such as a bind conflict (§88).
    ///
    /// A reconnect loop retries the same failing start for as long as the fault
    /// lasts, so a fault identical to the newest retained one updates that entry
    /// — its count and time — instead of adding another. Each severity keeps at
    /// most `limit` entries, oldest evicted first (§88, §89).
    ///
    /// Rebuilds the served snapshot in the same step, so the retained list and
    /// the form `snapshot` hands out cannot drift apart.
    fn retain_diagnostic(&mut self, diagnostic: crate::diagnostics::Diagnostic, limit: usize) {
        let repeated = match (
            self.repeated_start_fault.as_mut(),
            self.retained_diagnostics.last_mut(),
        ) {
            (Some(repeat), Some(newest))
                if repeat.message == diagnostic.message
                    && newest.severity == diagnostic.severity =>
            {
                repeat.count += 1;
                newest.message = format!("{} (failed {} times)", repeat.message, repeat.count);
                newest.timestamp = diagnostic.timestamp;
                true
            }
            _ => false,
        };
        if !repeated {
            let severity = diagnostic.severity;
            self.repeated_start_fault = Some(RepeatedStartFault {
                message: diagnostic.message.clone(),
                count: 1,
            });
            self.retained_diagnostics.push(diagnostic);
            let same = |d: &crate::diagnostics::Diagnostic| d.severity == severity;
            let held = self.retained_diagnostics.iter().filter(|d| same(d)).count();
            // At least one, so the entry just pushed (the one a repeat updates)
            // is never the one evicted.
            let mut excess = held.saturating_sub(limit.max(1));
            self.retained_diagnostics.retain(|d| {
                let evict = excess > 0 && same(d);
                excess -= usize::from(evict);
                !evict
            });
        }
        self.retained_diagnostics_snapshot = Arc::new(DiagnosticsSnapshot::from_diagnostics(
            self.retained_diagnostics.iter().cloned(),
        ));
    }

    /// Whether an auto-reconnect is armed or in progress (§9.1) — the poll-lane
    /// counterpart of the `ChannelReconnecting` event, so a UI can distinguish
    /// "Faulted, retrying" from "Faulted, given up / not retrying" without
    /// depending on the advisory event stream.
    fn reconnect_pending(&self) -> bool {
        self.reconnect_state.as_ref().is_some_and(|r| !r.gave_up)
    }
}

/// Errors from orchestrating a Channel.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    // The id is kept in the variant (callers may want it) but left out of the user-
    // facing message: it's a raw UUID, and the GUI already attaches the error to the
    // right channel (by name) — see `Driver::push_channel_error`.
    #[error("unknown channel")]
    UnknownChannel(ChannelId),
    #[error("unknown display view on the channel")]
    UnknownDisplayView(DisplayViewId),
    #[error("illegal channel state transition from {from:?} to {to:?}")]
    IllegalTransition {
        from: ChannelState,
        to: ChannelState,
    },
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error("failed to open serial port: {0}")]
    SerialOpen(#[source] serialport::Error),
    #[error("failed to bind interface: {0}")]
    Bind(#[source] std::io::Error),
    #[error("serial control is not available (not a running serial channel)")]
    SerialControlUnavailable(ChannelId),
}

/// The runtime orchestrator (§97.1). Owns channels; drives their lifecycle.
pub struct Listener {
    channels: HashMap<ChannelId, ManagedChannel>,
    events_tx: mpsc::Sender<RuntimeEvent>,
    events_rx: Option<mpsc::Receiver<RuntimeEvent>>,
    caps: PipelineCapacities,
}

impl Listener {
    pub fn new(caps: PipelineCapacities) -> Self {
        wiredata_timing::keep_timer_resolution_when_minimized();
        let (events_tx, events_rx) = mpsc::channel(caps.events);
        Self {
            channels: HashMap::new(),
            events_tx,
            events_rx: Some(events_rx),
            caps,
        }
    }

    pub fn with_default_capacities() -> Self {
        Self::new(PipelineCapacities::default())
    }

    /// Take the shared runtime→UI event stream (§137). Available once.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<RuntimeEvent>> {
        self.events_rx.take()
    }

    /// Register a configured Channel (Stopped). The runtime mints its
    /// `ChannelId` (§97.1); the persisted `StableConfigId` in the config is a
    /// separate identity.
    pub fn add_channel(&mut self, config: ChannelConfig) -> ChannelId {
        let id = ChannelId::new();
        self.channels.insert(
            id,
            ManagedChannel {
                config,
                pending: None,
                state: ChannelState::Stopped,
                handle: None,
                display_handles: Vec::new(),
                faulted: Arc::new(AtomicBool::new(false)),
                serial_control: None,
                reconnect_state: None,
                retained_diagnostics: Vec::new(),
                retained_diagnostics_snapshot: Arc::new(DiagnosticsSnapshot::default()),
                repeated_start_fault: None,
                retained_activity: ChannelActivity {
                    last_data_at: None,
                    bytes_per_sec: 0.0,
                    total_bytes: 0,
                },
                retained_boundary_saves: 0,
                retained_ingest_delay: DurationHistogram::default(),
                retained_recent_ingest_delay: DurationHistogram::default(),
                retained_ingest_processing: DurationHistogram::default(),
                retained_recent_ingest_processing: DurationHistogram::default(),
                retained_chunk_shape: ChunkShape::default(),
                retained_transport_health: TransportHealth::default(),
                retained_rule_timer_lateness: DurationHistogram::default(),
                retained_recent_rule_timer_lateness: DurationHistogram::default(),
                retained_idle_deadline_timer: IdleDeadlineTimerSummary::default(),
                active_run: None,
                last_run_summary: None,
            },
        );
        id
    }

    pub fn state(&self, id: ChannelId) -> Option<ChannelState> {
        self.channels.get(&id).map(|c| c.effective_state())
    }

    /// Whether `id`'s reconnect retries ran out (§9.1).
    pub fn reconnect_exhausted(&self, id: ChannelId) -> bool {
        self.channels
            .get(&id)
            .and_then(|c| c.reconnect_state.as_ref())
            .is_some_and(|s| s.gave_up)
    }

    pub fn config(&self, id: ChannelId) -> Option<&ChannelConfig> {
        self.channels.get(&id).map(|c| &c.config)
    }

    /// Update a Channel's stored per-channel **view** config in place, without a
    /// restart (§78, §87): the display config and the scroll-buffer `retention`. The
    /// viewer's presentation (mode, font, colors) is rendered GUI-side and the GUI caps
    /// its own scrollback live, so neither affects the live transport/pipeline — this
    /// only keeps the stored config current so a profile save captures the settings and
    /// the runtime's retention adopts the new limit on the Channel's next start. Unknown
    /// id is ignored.
    pub fn set_view_config(
        &mut self,
        id: ChannelId,
        display: crate::config::DisplayConfig,
        retention: crate::config::RetentionConfig,
    ) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.config.display = display;
            channel.config.retention = retention;
        }
    }

    /// Update a Channel's stored **Raw recording** config in place, without a restart
    /// (ADR-012/-013). Raw recording is a live field — the running recorder is (re)armed
    /// separately by [`set_recording`](Self::set_recording); this only keeps the stored
    /// config current so a profile save captures the destination/rotation/"record on
    /// start" settings. Unknown id is ignored.
    pub fn set_raw_recording_config(
        &mut self,
        id: ChannelId,
        raw: crate::config::RawRecordingConfig,
    ) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.config.raw_recording = raw;
        }
    }

    /// Update a Channel's stored **Display recording** config in place, without a
    /// restart — the Raw sibling (ADR-012/-013). The live recorder is (re)armed
    /// separately by [`set_display_recording`](Self::set_display_recording); this
    /// keeps the stored config current so a profile save captures the settings and
    /// "record on start" applies on the next start. Unknown id is ignored.
    pub fn set_display_recording_config(
        &mut self,
        id: ChannelId,
        display: crate::config::DisplayRecordingConfig,
    ) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.config.display_recording = display;
        }
    }

    /// Request an on-demand snapshot of a Channel's *small* observable state (§137,
    /// ADR-006): diagnostics, recent match firings, per-view pause state, recording
    /// state, liveness, and the stream end offset. The scrollback bytes come
    /// separately via [`stream_delta`](Self::stream_delta).
    ///
    /// Always serves for a known Channel — a live pipeline answers when running;
    /// otherwise a minimal snapshot carries the retained history (the last run's
    /// diagnostics and its final liveness facts, so a stopped channel's byte total
    /// reads exact at rest). Every reply is stamped with the **effective lifecycle
    /// state**, which is what lets a polling consumer self-correct a lifecycle
    /// event that dropped (ADR-006: events are advisory; the poll is the truth).
    /// `None` only for an unknown id.
    pub async fn snapshot(&self, id: ChannelId) -> Option<ChannelSnapshot> {
        let channel = self.channels.get(&id)?;
        let live = match channel.handle.as_ref() {
            Some(ChannelHandle::Data(tasks)) => tasks.snapshot().await,
            // A TCP listener has no pipeline of its own (its connections are
            // snapshot targets in their own right; per-connection snapshots are
            // deferred) — serve the retained/lifecycle shell below.
            Some(ChannelHandle::TcpListener(_)) | None => None,
        };
        let mut snap = live.unwrap_or_else(|| retained_snapshot(id, channel));
        snap.state = channel.effective_state();
        snap.reconnect_pending = channel.reconnect_pending();
        snap.last_run_summary = channel.last_run_summary.clone();
        Some(snap)
    }

    /// Cheap O(1) liveness stats for a data Channel (§91.1, ADR-006) — the counters
    /// a multi-channel overview shows per tab, without cloning the scrollback.
    /// Like [`snapshot`](Self::snapshot), always serves for a known Channel (live
    /// counters when running, retained totals otherwise) and stamps the effective
    /// lifecycle state — the overview polls this for *every* tab, so this lane is
    /// what self-corrects a dropped lifecycle event on non-selected channels.
    /// `None` only for an unknown id.
    pub async fn channel_stats(&self, id: ChannelId) -> Option<ChannelStats> {
        let channel = self.channels.get(&id)?;
        let live = match channel.handle.as_ref() {
            Some(ChannelHandle::Data(tasks)) => tasks.stats().await,
            Some(ChannelHandle::TcpListener(_)) | None => None,
        };
        let mut stats = live.unwrap_or_else(|| retained_stats(channel));
        stats.state = channel.effective_state();
        stats.reconnect_pending = channel.reconnect_pending();
        Some(stats)
    }

    /// Incremental stream bytes since the consumer's cursor (§87, ADR-009): only
    /// what is new, so a live viewer never re-ships the whole retained scrollback
    /// (up to the 256 KB cap) each poll. `None` when unknown, not running, or a
    /// TCP listener.
    pub async fn stream_delta(&self, id: ChannelId, since: u64) -> Option<StreamDelta> {
        match self.channels.get(&id)?.handle.as_ref()? {
            ChannelHandle::Data(tasks) => tasks.stream_delta(since).await,
            ChannelHandle::TcpListener(_) => None,
        }
    }

    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.channels.keys().copied().collect()
    }

    /// Begin or stop Raw recording on a running data Channel live, without a restart
    /// (§50.2, ADR-012) — the manual counterpart of the match-rule `Record` action,
    /// sharing the pipeline's lazy begin / clean finalize path. `enabled = true`
    /// begins (a no-op if already recording); `false` stops and finalizes. Returns
    /// `false` when the Channel is unknown, not running, or a TCP listener. The outcome
    /// is observed via the snapshot's recording state; a begin failure — including no
    /// destination configured, or the destination in use (§121) — records a diagnostic
    /// and raises a `RecordingFaulted` event (§55), leaving the Channel Running.
    pub async fn set_recording(
        &self,
        id: ChannelId,
        enabled: bool,
        raw: crate::config::RawRecordingConfig,
    ) -> bool {
        // Use the settings the caller read at click time (the editor's current
        // values), not committed config — so Record records to exactly what's on
        // screen, with no Apply/restart (ADR-012). The channel name comes from the
        // committed config (it's a live rename, not part of the recording settings).
        let Some(channel) = self.channels.get(&id) else {
            return false;
        };
        let settings = self.settings_from(&raw, channel.config.name.as_str());
        match channel.handle.as_ref() {
            Some(ChannelHandle::Data(tasks)) => tasks.set_recording(enabled, settings).await,
            _ => false,
        }
    }

    /// Begin or stop **Display** recording on a running data Channel live, without
    /// a restart (§54, ADR-012) — the Raw toggle's sibling, same return and fault
    /// contract as [`set_recording`](Self::set_recording). Settings come from the
    /// caller's click-time config; the renderer comes from the channel's stored
    /// primary display view (kept current by `set_view_config`), so the `.disp`
    /// records what the view shows.
    pub async fn set_display_recording(
        &self,
        id: ChannelId,
        enabled: bool,
        display: crate::config::DisplayRecordingConfig,
    ) -> bool {
        let Some(channel) = self.channels.get(&id) else {
            return false;
        };
        let settings = self.display_settings_from(
            &display,
            channel.config.name.as_str(),
            display_renderer(&channel.config),
        );
        match channel.handle.as_ref() {
            Some(ChannelHandle::Data(tasks)) => {
                tasks.set_display_recording(enabled, settings).await
            }
            _ => false,
        }
    }

    /// Build [`DisplayRecordingSettings`] from a Display recording config + channel
    /// name + renderer — [`settings_from`](Self::settings_from)'s Display sibling,
    /// with the same rotation Refuse→Append coercion (§59). `None` when no
    /// destination is set (the pipeline faults the begin with a clear message).
    fn display_settings_from(
        &self,
        display: &crate::config::DisplayRecordingConfig,
        channel_name: &str,
        renderer: DisplayView,
    ) -> Option<DisplayRecordingSettings> {
        display
            .destination
            .clone()
            .map(|destination| DisplayRecordingSettings {
                destination,
                channel_name: channel_name.to_string(),
                overwrite: crate::record::effective_overwrite(
                    display.overwrite_policy,
                    display.file_rotation,
                ),
                file_rotation: display.file_rotation,
                queue_budget: self.caps.recording_queue_budget,
                size_cap: display.size_cap(),
                disk_guard: display.disk_guard,
                renderer,
            })
    }

    /// Drive the RTS output line of a running serial Channel (§161).
    pub async fn set_rts(&self, id: ChannelId, on: bool) -> Result<(), OrchestratorError> {
        self.serial_command(id, SerialControlCommand::SetRts(on))
            .await
    }

    /// Drive the DTR output line of a running serial Channel (§161).
    pub async fn set_dtr(&self, id: ChannelId, on: bool) -> Result<(), OrchestratorError> {
        self.serial_command(id, SerialControlCommand::SetDtr(on))
            .await
    }

    async fn serial_command(
        &self,
        id: ChannelId,
        command: SerialControlCommand,
    ) -> Result<(), OrchestratorError> {
        let control = self
            .channels
            .get(&id)
            .and_then(|c| c.serial_control.as_ref())
            .ok_or(OrchestratorError::SerialControlUnavailable(id))?;
        control
            .commands
            .send(command)
            .await
            .map_err(|_| OrchestratorError::SerialControlUnavailable(id))
    }

    /// The current serial control-line state of a running serial Channel (§161, the
    /// pull side of `ControlLinesChanged`); `None` if it is not a running serial
    /// Channel.
    ///
    /// `None` means exactly that — not a running serial Channel. A poisoned cell
    /// recovers its last value (`core::sync`) rather than reporting `None`, which
    /// the GUI would read as "no serial panel here" and hide the lines entirely.
    pub fn serial_control_lines(&self, id: ChannelId) -> Option<SerialControlLines> {
        let control = self.channels.get(&id)?.serial_control.as_ref()?;
        Some(*lock_recover(&control.state))
    }

    pub fn has_pending(&self, id: ChannelId) -> bool {
        self.channels.get(&id).is_some_and(|c| c.pending.is_some())
    }

    /// The accepted-but-unapplied configuration for a Channel (§13), if any.
    pub fn pending(&self, id: ChannelId) -> Option<&ChannelConfig> {
        self.channels.get(&id).and_then(|c| c.pending.as_ref())
    }

    /// The Display View ids of a running Channel (§48); empty while Stopped.
    pub fn display_views(&self, channel: ChannelId) -> Vec<DisplayViewId> {
        self.channels
            .get(&channel)
            .map(|c| c.display_handles.iter().map(|h| h.id).collect())
            .unwrap_or_default()
    }

    /// Pause one Display View (§11, §50): reception, recording, numbering, and
    /// other views are unaffected.
    pub fn pause_display(
        &self,
        channel: ChannelId,
        view: DisplayViewId,
    ) -> Result<(), OrchestratorError> {
        self.display_handle(channel, view)?.pause();
        Ok(())
    }

    /// Resume a paused Display View (§11).
    pub fn resume_display(
        &self,
        channel: ChannelId,
        view: DisplayViewId,
    ) -> Result<(), OrchestratorError> {
        self.display_handle(channel, view)?.resume();
        Ok(())
    }

    fn display_handle(
        &self,
        channel: ChannelId,
        view: DisplayViewId,
    ) -> Result<&DisplayViewHandle, OrchestratorError> {
        let managed = self
            .channels
            .get(&channel)
            .ok_or(OrchestratorError::UnknownChannel(channel))?;
        managed
            .display_handles
            .iter()
            .find(|h| h.id == view)
            .ok_or(OrchestratorError::UnknownDisplayView(view))
    }

    fn set_state(&mut self, id: ChannelId, state: ChannelState) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.state = state;
        }
    }

    /// Start a Channel (§10.1): Stopped → Starting → Running, opening the
    /// interface. A resource failure takes Starting → Faulted (§8.2, §71).
    pub async fn start(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        let config = {
            let channel = self
                .channels
                .get(&id)
                .ok_or(OrchestratorError::UnknownChannel(id))?;
            let from = channel.effective_state();
            if !from.can_transition_to(ChannelState::Starting) {
                return Err(OrchestratorError::IllegalTransition {
                    from,
                    to: ChannelState::Starting,
                });
            }
            channel.config.clone()
        };

        // Note: a recording-destination collision (§121, ADR-014) is **not** checked
        // here — it must not fault the *channel* (reception is fine; only recording can't
        // start). It is enforced by the advisory file lock when the recorder opens its
        // file, which surfaces a recording fault that leaves the channel Running. (An
        // earlier version faulted the whole channel here, which wrongly stopped reception
        // and showed the bind/port recourse.)

        // Install a fresh fault flag for this run (ADR-006); the monitor flips it
        // on a spontaneous fault and `state()`/validation read it back.
        let faulted = Arc::new(AtomicBool::new(false));
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.faulted = faulted.clone();
            channel.state = ChannelState::Starting;
            // `retained_diagnostics` is left in place: `spawn_data` reads it to seed the
            // new pipeline (§88). While Running, the live pipeline snapshot is served
            // instead (the handle exists), so this stale copy is never shown; the next
            // stop replaces it with the new run's log.
        }
        let active_run = ActiveRun::begin();
        match self.spawn_channel(id, &config, faulted).await {
            Ok((handle, serial_control)) => {
                let display_handles = match &handle {
                    ChannelHandle::Data(tasks) => tasks.display_handles().to_vec(),
                    // A TCP listener has no display itself; its connections do.
                    ChannelHandle::TcpListener(_) => Vec::new(),
                };
                if let Some(channel) = self.channels.get_mut(&id) {
                    channel.handle = Some(handle);
                    channel.display_handles = display_handles;
                    channel.serial_control = serial_control;
                    channel.state = ChannelState::Running;
                    channel.active_run = Some(active_run);
                }
                let _ = self.events_tx.try_send(RuntimeEvent::ChannelStarted(id));
                Ok(())
            }
            Err(err) => {
                self.set_state(id, ChannelState::Faulted);
                // Retain the start fault as an ERROR diagnostic (a bind conflict never
                // ran a pipeline, so it isn't in any pipeline log) — so it shows in the
                // diagnostics list and survives the next restart like the INFO entries.
                // Named for the channel, matching how the GUI labels it. Bounded by the
                // same error limit a running pipeline applies.
                let limit = self
                    .channel_caps(&config)
                    .error_retention
                    .unwrap_or(crate::retention::DEFAULT_BACKSTOP);
                if let Some(channel) = self.channels.get_mut(&id) {
                    let name = channel.config.name.as_str().to_owned();
                    let diagnostic =
                        crate::diagnostics::Diagnostic::error(format!("{name}: {err}"));
                    // Every failed attempt reaches the event log (§118), even when the
                    // retained list merges it into one counted entry.
                    crate::diagnostics::emit_to_event_log(&name, id, &diagnostic);
                    channel.retain_diagnostic(diagnostic, limit);
                }
                let _ = self.events_tx.try_send(RuntimeEvent::ChannelFaulted(id));
                Err(err)
            }
        }
    }

    /// Stop a Channel (§10.2). A Running Channel is stopped gracefully (§110); a
    /// Faulted Channel is returned to Stopped (§8.5). Other states are illegal.
    ///
    /// Split into three phases so a caller can bound only the *await* (the graceful
    /// drain) without abandoning the bookkeeping: `begin_stop` marks Stopping and takes
    /// the handle out (cheap, synchronous), `drain_handle` awaits the handle's
    /// shutdown (the only part that can hang), and `finish_stop` lands the channel in
    /// Stopped and announces it. `shutdown` uses the phases directly so a timed-out
    /// drain still runs `finish_stop`; everyone else uses this convenience wrapper.
    pub async fn stop(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        let handle = self.begin_stop(id)?;
        let final_snapshot = drain_handle(handle).await;
        self.finish_stop(id, final_snapshot);
        Ok(())
    }

    /// Phase 1 of [`stop`](Self::stop): validate the transition, mark a Running
    /// channel Stopping, and take its handle out (so the slow drain in `drain_handle`
    /// owns no `&mut self`). Returns the handle to drain (or `None` if there was none,
    /// e.g. a start-time fault). Illegal from any state but Running/Faulted.
    fn begin_stop(&mut self, id: ChannelId) -> Result<Option<ChannelHandle>, OrchestratorError> {
        let state = self
            .state(id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        match state {
            // Running → graceful drain; Faulted → §8.5 recovery. Both consume the
            // handle (a spontaneous fault leaves one whose tasks have already ended; a
            // start-time fault leaves none) and land in Stopped.
            ChannelState::Running | ChannelState::Faulted => {
                if state == ChannelState::Running {
                    self.set_state(id, ChannelState::Stopping);
                }
                Ok(self.channels.get_mut(&id).and_then(|c| c.handle.take()))
            }
            other => Err(OrchestratorError::IllegalTransition {
                from: other,
                to: ChannelState::Stopped,
            }),
        }
    }

    /// Land a Channel in Stopped: clear its Display Views, reset the fault flag
    /// (so a previously-faulted Channel reads Stopped, not Faulted), and announce
    /// the stop (§110).
    fn finish_stop(&mut self, id: ChannelId, final_snapshot: Option<ChannelSnapshot>) {
        if let Some(channel) = self.channels.get_mut(&id) {
            let ended_by_fault =
                channel.faulted.load(Ordering::Relaxed) || channel.state == ChannelState::Faulted;
            if let Some(run) = channel.active_run.take() {
                let finished_at = SystemTime::now();
                channel.last_run_summary = Some(ListenerRunSummary::completed(
                    run.id,
                    id,
                    channel.config.name.as_str().to_owned(),
                    transport_name(&channel.config.interface).to_owned(),
                    run.started_at,
                    finished_at,
                    run.started_monotonic.elapsed(),
                    if ended_by_fault {
                        RunEndReason::TransportFault
                    } else {
                        RunEndReason::StopRequested
                    },
                    final_snapshot.as_ref(),
                ));
            }
            channel.display_handles.clear();
            channel.serial_control = None;
            channel.reconnect_state = None;
            channel.state = ChannelState::Stopped;
            channel.faulted.store(false, Ordering::Relaxed);
            // Retain the final pipeline's diagnostics (its full log, including the
            // stop-time "Channel stopped"/"Raw recording stopped") so the GUI shows the
            // stopped channel's log and the next start carries it forward. The pipeline's
            // log already includes the prior run's retained entries (seeded at start), so
            // this *replaces* rather than appends — no growth across many cycles.
            // The final liveness facts are retained alongside, with the rolling rate
            // zeroed — at rest the throughput is 0 by definition, but the byte total is
            // the number the user cross-checks against the sender.
            if let Some(snap) = final_snapshot {
                channel.retained_activity = ChannelActivity {
                    bytes_per_sec: 0.0,
                    ..snap.activity
                };
                channel.retained_boundary_saves = snap.match_boundary_saves;
                channel.retained_ingest_delay = snap.ingest_delay;
                channel.retained_recent_ingest_delay = snap.recent_ingest_delay;
                channel.retained_ingest_processing = snap.ingest_processing;
                channel.retained_recent_ingest_processing = snap.recent_ingest_processing;
                channel.retained_chunk_shape = snap.chunk_shape;
                channel.retained_transport_health = snap.transport_health;
                channel.retained_rule_timer_lateness = snap.rule_timer_lateness;
                channel.retained_recent_rule_timer_lateness = snap.recent_rule_timer_lateness;
                channel.retained_idle_deadline_timer = snap.idle_deadline_timer;
                channel.retained_diagnostics = snap.diagnostics.to_sorted_vec();
                channel.repeated_start_fault = None;
                // The stopped channel is polled at the same cadence as a live one,
                // so keep the served form ready rather than regrouping per poll.
                channel.retained_diagnostics_snapshot = Arc::clone(&snap.diagnostics);
            }
        }
        let _ = self.events_tx.try_send(RuntimeEvent::ChannelStopped(id));
    }

    /// Accept a configuration change into pending state without applying it
    /// (§13). The active interface keeps its current settings until applied.
    pub fn set_pending_config(
        &mut self,
        id: ChannelId,
        config: ChannelConfig,
    ) -> Result<(), OrchestratorError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        channel.pending = Some(config);
        Ok(())
    }

    /// Apply a pending configuration via one coordinated restart (§13): stop,
    /// swap in the pending config, then start (only if it was Running).
    pub async fn apply_pending(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        if !self.channels.contains_key(&id) {
            return Err(OrchestratorError::UnknownChannel(id));
        }
        if !self.has_pending(id) {
            return Ok(());
        }
        let was_running = self.state(id) == Some(ChannelState::Running);
        if was_running {
            self.stop(id).await?;
        }
        if let Some(channel) = self.channels.get_mut(&id) {
            if let Some(pending) = channel.pending.take() {
                channel.config = pending;
            }
        }
        if was_running {
            self.start(id).await?;
        }
        Ok(())
    }

    /// Stop a Channel only if it is in a state `stop` accepts (Running or Faulted),
    /// returning whether a stop actually happened. A no-op (returns `false`) for any
    /// other state — chiefly Stopped — so callers that just want "make sure it's down"
    /// don't have to special-case the already-Stopped `IllegalTransition`. (Stopping is
    /// transient inside a single `stop` call and never observed across an await, so it
    /// isn't a reachable input here.) Unknown id still errors.
    pub async fn stop_if_live(&mut self, id: ChannelId) -> Result<bool, OrchestratorError> {
        let state = self
            .state(id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        if matches!(state, ChannelState::Running | ChannelState::Faulted) {
            self.stop(id).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// The one lifecycle primitive the command layer drives (§10, §13): optionally
    /// commit a new config, optionally (re)start, in one coordinated, server-side
    /// sequence — so single-channel and bulk flows share exactly one path and the
    /// Faulted→Stopped recovery lives here, not in the GUI.
    ///
    /// - `config: Some(c)` swaps `c` in as the active config (a live channel is
    ///   stopped first so it restarts onto `c`; a stopped one just adopts it).
    /// - `start = true` brings the channel up afterward. A Faulted channel is
    ///   normalized to Stopped first (§8.5), so the direct `Stopped → Starting`
    ///   transition is always legal — the illegal `Faulted → Starting` can't occur.
    ///
    /// With `config = None, start = true` this is a plain start/retry; with
    /// `config = Some, start = true` it is "Apply & Restart"/"Start with this config".
    /// With `config = Some, start = false` it commits the config and leaves the channel
    /// Stopped — note a *live* channel is stopped to adopt it (it does not stay up on
    /// the old config); use `start = true` to bring it back.
    pub async fn commit_and_start(
        &mut self,
        id: ChannelId,
        config: Option<ChannelConfig>,
        start: bool,
    ) -> Result<(), OrchestratorError> {
        if !self.channels.contains_key(&id) {
            return Err(OrchestratorError::UnknownChannel(id));
        }
        if let Some(config) = config {
            // Stop a live channel so it comes back up on the new config; then swap it in.
            self.stop_if_live(id).await?;
            if let Some(channel) = self.channels.get_mut(&id) {
                channel.pending = None; // the explicit config supersedes any queued one
                channel.config = config;
            }
        }
        if start {
            // Normalize Faulted/Reconnecting → Stopped so Start is a legal
            // Stopped→Starting transition (§8.5); a Running channel is already up.
            if self.state(id) != Some(ChannelState::Running) {
                self.stop_if_live(id).await?;
                self.start(id).await?;
            }
        }
        Ok(())
    }

    /// Rename a Channel in place (§6). The name is a user-facing label only and the
    /// pipeline does not key off it, so this takes effect immediately — no restart,
    /// no interface churn — unlike the full reconfigure path. A queued pending config
    /// is renamed too, so applying it later does not revert the name.
    pub fn rename(
        &mut self,
        id: ChannelId,
        name: crate::core::ChannelName,
    ) -> Result<(), OrchestratorError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        channel.config.name = name.clone();
        if let Some(pending) = &mut channel.pending {
            pending.name = name;
        }
        Ok(())
    }

    /// Remove a Channel from the registry entirely (e.g. a GUI "remove", or
    /// discarding a misconfigured channel). If it is Running or Faulted it is first
    /// stopped (best-effort) so its tasks and socket are released, then it is
    /// dropped. Unknown id is an error. After this, `state(id)` is `None`.
    pub async fn remove_channel(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        let state = self
            .state(id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        if matches!(state, ChannelState::Running | ChannelState::Faulted) {
            // Release the interface/tasks; ignore a stop error — we're discarding it.
            let _ = self.stop(id).await;
        }
        self.channels.remove(&id);
        Ok(())
    }

    /// The grace period for [`shutdown`](Self::shutdown) (§113): generous, since a
    /// normal graceful stop drains the small bounded ingest queue and finalizes
    /// files in milliseconds — this only guards against a stuck finalize, such
    /// as a write to a drive that stopped answering, holding up process exit.
    pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

    /// Stop every live Channel (§113, application exit), each independently and
    /// all at once, within [`SHUTDOWN_GRACE`](Self::SHUTDOWN_GRACE). Includes
    /// spontaneously faulted channels so their tasks and Display Views are
    /// cleaned up too.
    ///
    /// A blocked file operation cannot be cancelled, so a Channel whose stop has
    /// not finished when the grace expires is abandoned: it is logged as
    /// "finalization incomplete" (§118) and named in the outcome.
    pub async fn shutdown(&mut self) -> ShutdownOutcome {
        let live: Vec<ChannelId> = self
            .channels
            .iter()
            .filter(|(_, c)| {
                matches!(
                    c.effective_state(),
                    ChannelState::Running | ChannelState::Faulted
                )
            })
            .map(|(id, _)| *id)
            .collect();
        // Graceful stop (§110) preserves the accepted backlog — it drains the
        // bounded ingest queue into recordings before finalizing — so we prefer
        // it over a forced abort, which would abandon up to `caps.ingest`
        // buffered chunks. But it is *bounded*: should a recorder's finalize
        // hang, the timeout abandons the *wait* so process exit can't deadlock.
        //
        // Crucially we time out only the drain, not the bookkeeping: `begin_stop`
        // takes the handle out (cancelling the transport, so detached tasks wind
        // down on their own), and `finish_stop` always runs afterward — so even
        // a timed-out channel lands in Stopped, clears its handles, and emits
        // ChannelStopped, rather than being abandoned mid-`Stopping`.
        let drains = live
            .into_iter()
            // Not stoppable shouldn't happen — we filtered to live.
            .filter_map(|id| Some((id, drain_handle(self.begin_stop(id).ok()?))))
            .collect();
        let mut outcome = ShutdownOutcome::default();
        for (id, drained) in drain_all(drains, Self::SHUTDOWN_GRACE).await {
            // A timed-out drain has no final snapshot (and the app is exiting,
            // so there is no GUI to deliver it to anyway).
            let finished = drained.is_some();
            self.finish_stop(id, drained.flatten());
            if !finished {
                outcome
                    .incomplete
                    .push(self.note_finalization_incomplete(id));
            }
        }
        outcome
    }

    /// Log that `id`'s stop did not finish within the grace period (§113,
    /// §118), and return its name.
    fn note_finalization_incomplete(&mut self, id: ChannelId) -> String {
        let Some(channel) = self.channels.get(&id) else {
            return id.to_string();
        };
        let name = channel.config.name.as_str().to_owned();
        let limit = self
            .channel_caps(&channel.config)
            .error_retention
            .unwrap_or(crate::retention::DEFAULT_BACKSTOP);
        let diagnostic = crate::diagnostics::Diagnostic::error(format!(
            "finalization incomplete on channel {name}: its stop did not finish within {} s, \
             so the end of its recordings may be missing",
            Self::SHUTDOWN_GRACE.as_secs()
        ));
        crate::diagnostics::emit_to_event_log(&name, id, &diagnostic);
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.retain_diagnostic(diagnostic, limit);
        }
        name
    }

    /// Drive auto-reconnect (§9.1, §162). The application calls this periodically;
    /// the orchestrator has no background loop (ADR-006). For each Channel that is
    /// effectively Faulted with reconnect enabled, the first call arms a backoff
    /// timer; once the backoff elapses, this performs a Stop+Start reconnect
    /// (emitting `ChannelReconnecting`/`ChannelReconnected`), backing off
    /// exponentially on failure and giving up (`ChannelReconnectGaveUp`) after
    /// `max_attempts`. Stop+Start reuses the tested lifecycle (Faulted → Stopped →
    /// Starting → Running), so the events also include the intermediate
    /// `ChannelStopped`/`ChannelStarted`.
    pub async fn reconnect_tick(&mut self) {
        let now = Instant::now();
        let candidates: Vec<ChannelId> = self
            .channels
            .iter()
            .filter(|(_, c)| {
                c.effective_state() == ChannelState::Faulted && c.config.reconnect.enabled
            })
            .map(|(id, _)| *id)
            .collect();

        for id in candidates {
            let Some(policy) = self.channels.get(&id).map(|c| c.config.reconnect) else {
                continue;
            };
            let state = self
                .channels
                .get(&id)
                .and_then(|c| c.reconnect_state.clone());
            match state {
                // First observation of the fault: arm the backoff timer.
                None => {
                    let backoff = Duration::from_millis(policy.initial_backoff_ms);
                    self.set_reconnect_state(
                        id,
                        ReconnectState {
                            attempts: 0,
                            backoff,
                            next_attempt_at: now + backoff,
                            gave_up: false,
                        },
                    );
                }
                // Given up, or not due yet.
                Some(s) if s.gave_up || now < s.next_attempt_at => {}
                Some(s) => {
                    if policy.max_attempts.is_some_and(|max| s.attempts >= max) {
                        let _ = self
                            .events_tx
                            .try_send(RuntimeEvent::ChannelReconnectGaveUp(id));
                        if let Some(rs) = self
                            .channels
                            .get_mut(&id)
                            .and_then(|c| c.reconnect_state.as_mut())
                        {
                            rs.gave_up = true;
                        }
                        continue;
                    }
                    let attempt = s.attempts + 1;
                    let _ = self
                        .events_tx
                        .try_send(RuntimeEvent::ChannelReconnecting(id, attempt));
                    // stop() clears reconnect_state; we re-establish it on failure.
                    let _ = self.stop(id).await;
                    if self.start(id).await.is_ok() {
                        let _ = self
                            .events_tx
                            .try_send(RuntimeEvent::ChannelReconnected(id));
                    } else {
                        let next_ms = (s.backoff.as_millis() as f64 * policy.multiplier) as u64;
                        let backoff =
                            Duration::from_millis(next_ms.min(policy.max_backoff_ms).max(1));
                        self.set_reconnect_state(
                            id,
                            ReconnectState {
                                attempts: attempt,
                                backoff,
                                next_attempt_at: Instant::now() + backoff,
                                gave_up: false,
                            },
                        );
                    }
                }
            }
        }
    }

    fn set_reconnect_state(&mut self, id: ChannelId, state: ReconnectState) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.reconnect_state = Some(state);
        }
    }

    /// Per-channel pipeline capacities, applying this channel's retention limits
    /// (§80, §88) on top of the base capacities. A channel that sets no explicit
    /// diagnostic limit gets the base default (bounded), not the raw backstop —
    /// so an unconfigured channel can't grow its log unbounded for weeks (§124).
    fn channel_caps(&self, config: &ChannelConfig) -> PipelineCapacities {
        let retention = &config.retention;
        PipelineCapacities {
            stream_display: retention.byte_limit.unwrap_or(self.caps.stream_display),
            event_retention: retention.event_limit.or(self.caps.event_retention),
            warning_retention: retention.warning_limit.or(self.caps.warning_retention),
            error_retention: retention.error_limit.or(self.caps.error_retention),
            ..self.caps
        }
    }

    /// Build, open/bind, and wire a Channel's runtime tasks (§8.2). `faulted` is
    /// the run's shared fault flag (ADR-006), handed to the data-channel monitor.
    async fn spawn_channel(
        &self,
        id: ChannelId,
        config: &ChannelConfig,
        faulted: Arc<AtomicBool>,
    ) -> Result<(ChannelHandle, Option<SerialControl>), OrchestratorError> {
        match &config.interface {
            InterfaceConfig::Serial(serial) => {
                // Serial is the one transport whose reader can stall (§97.1); give
                // it a notice sender so a sustained stall becomes a retained
                // diagnostic + warning in the pipeline (§101, ADR-007).
                let (notice_tx, notice_rx) = mpsc::channel(TRANSPORT_NOTICES);
                // Live control lines (§161): a command inbox to the reader and a
                // shared cell it updates; ControlLinesChanged flows on the events.
                let (cmd_tx, cmd_rx) = mpsc::channel(SERIAL_CONTROL_COMMANDS);
                let state = Arc::new(Mutex::new(SerialControlLines::default()));
                let hooks = SerialControlHooks {
                    commands: cmd_rx,
                    state: state.clone(),
                    events: self.events_tx.clone(),
                };
                let opened = build_serial(id, serial)?
                    .open()
                    .await
                    .map_err(OrchestratorError::SerialOpen)?
                    .with_notice_sender(notice_tx.clone())
                    .with_control(hooks);
                let serial_stall_state = opened.stall_state();
                let handle = ChannelHandle::Data(self.spawn_data(
                    id,
                    opened,
                    config,
                    DataSpawnContext {
                        faulted,
                        serial_stall_state: Some(serial_stall_state),
                        notices: (notice_tx, notice_rx),
                    },
                ));
                Ok((
                    handle,
                    Some(SerialControl {
                        commands: cmd_tx,
                        state,
                    }),
                ))
            }
            InterfaceConfig::Udp(udp) => {
                let (notice_tx, notice_rx) = mpsc::channel(TRANSPORT_NOTICES);
                let bound = build_udp(id, udp)?
                    .bind()
                    .await
                    .map_err(OrchestratorError::Bind)?
                    .with_notice_sender(notice_tx.clone());
                // UDP is async and never stalls the reader. On Linux the transport
                // also publishes its cumulative SO_RXQ_OVFL socket-drop counter;
                // other platforms explicitly report that counter unsupported.
                let handle = ChannelHandle::Data(self.spawn_data(
                    id,
                    bound,
                    config,
                    DataSpawnContext {
                        faulted,
                        serial_stall_state: None,
                        notices: (notice_tx, notice_rx),
                    },
                ));
                Ok((handle, None))
            }
            // Per-connection faults stay supervised inside the listener task;
            // a spontaneous *acceptor* fault sets `faulted` like any other
            // transport, so polled state reads Faulted and reconnect can
            // engage (§162).
            InterfaceConfig::TcpListener(tcp) => {
                let bound = build_tcp_listener(id, tcp)?
                    .bind()
                    .await
                    .map_err(OrchestratorError::Bind)?;
                // Per-connection recording is deferred (§59).
                let handle = start_tcp_listener(
                    bound.channel_id(),
                    bound,
                    self.channel_caps(config),
                    tcp.max_connections,
                    faulted,
                    self.events_tx.clone(),
                );
                Ok((ChannelHandle::TcpListener(handle), None))
            }
        }
    }

    fn spawn_data<R: DataTransportRunner>(
        &self,
        id: ChannelId,
        runner: R,
        config: &ChannelConfig,
        context: DataSpawnContext,
    ) -> MonitoredChannel {
        let DataSpawnContext {
            faulted,
            serial_stall_state,
            notices,
        } = context;
        spawn_monitored_channel(
            id,
            runner,
            // No pre-built recorders: both taps begin in the pipeline (auto-begin
            // or the live toggle), so a failure surfaces as a recording fault
            // (§55) rather than a start fault.
            None,
            None,
            // One runtime Display View per configured view (§48); at least one.
            // Each recording's disk guard travels in its settings (§56.2).
            view_count(config),
            // Match Rules (§50.2, §165) plus the Raw recording settings a match-
            // triggered `Record` needs (lazy-create from the destination — nothing
            // until a match fires).
            MatchSetup {
                rules: config.match_rules.clone(),
                recording_settings: self.recording_settings(config),
                // ... and the Display settings a `Record { Display | Both }` needs
                // (§54): same lazy-create-from-destination contract as Raw.
                display_recording_settings: self.display_settings_from(
                    &config.display_recording,
                    config.name.as_str(),
                    display_renderer(config),
                ),
                // "Record on start": begin in the pipeline so a start failure surfaces
                // like the live toggle (diagnostic + RecordingFaulted), not silently —
                // including the no-destination case, which `begin_recording` faults.
                auto_begin_recording: config.raw_recording.enabled,
                auto_begin_display_recording: config.display_recording.enabled,
                // Carry the previous run's diagnostics forward so a restart keeps its
                // log (§88, within session).
                prior_diagnostics: self.prior_diagnostics(id),
                channel_name: Some(config.name.as_str().to_owned()),
            },
            self.channel_caps(config),
            self.events_tx.clone(),
            faulted,
            serial_stall_state,
            notices,
        )
    }

    /// The Raw recording settings for a `Record` action (§50.2): present whenever a
    /// Raw destination is configured — **independent of `raw_recording.enabled`**, so
    /// a match-triggered `Record` works even when auto-start recording is off. The
    /// auto-start, live-toggle and match-triggered begins all use these settings, so
    /// the three produce the same recording.
    fn recording_settings(&self, config: &ChannelConfig) -> Option<RawRecordingSettings> {
        self.settings_from(&config.raw_recording, config.name.as_str())
    }

    /// The previous run's retained diagnostics for Channel `id`, so a restart carries its
    /// log forward (§88, within session). Empty for a first start — the per-severity caps
    /// bound it on replay into the new pipeline.
    fn prior_diagnostics(&self, id: ChannelId) -> Vec<crate::diagnostics::Diagnostic> {
        self.channels
            .get(&id)
            .map(|c| c.retained_diagnostics.clone())
            .unwrap_or_default()
    }

    /// Build [`RawRecordingSettings`] from a Raw recording config + channel name — used
    /// by both the build-time path (`recording_settings`) and the live `set_recording`
    /// path, which passes the settings read from the editor at click time (ADR-012).
    /// `None` when no destination is set.
    fn settings_from(
        &self,
        raw: &crate::config::RawRecordingConfig,
        channel_name: &str,
    ) -> Option<RawRecordingSettings> {
        raw.destination
            .clone()
            .map(|destination| RawRecordingSettings {
                destination,
                channel_name: channel_name.to_string(),
                // `Refuse` is meaningless with rotation: each period is a *new* file, and
                // re-opening the current period's file on a restart must append, not fail.
                // Coerce it here — the single funnel for both the live and auto-begin
                // paths — so a UI that left the policy at Refuse can't cause a
                // spurious "file already exists" fault (§59, `effective_overwrite`).
                overwrite: crate::record::effective_overwrite(
                    raw.overwrite_policy,
                    raw.file_rotation,
                ),
                timestamps: raw.timestamp_enabled,
                file_rotation: raw.file_rotation,
                queue_budget: self.caps.recording_queue_budget,
                size_cap: raw.size_cap(),
                disk_guard: raw.disk_guard,
            })
    }
}

/// What application exit could not finish (§113).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShutdownOutcome {
    /// The Channels, by name, whose stop did not finish within the grace
    /// period: the end of their recordings may be missing.
    pub incomplete: Vec<String>,
}

impl ShutdownOutcome {
    /// Whether every Channel finished stopping.
    pub fn is_complete(&self) -> bool {
        self.incomplete.is_empty()
    }
}

/// How long dropping an async runtime may wait for its tasks (§113). A file
/// operation stuck in the blocking pool cannot be cancelled, so after
/// [`Listener::shutdown`] the runtime is shut down with this limit rather than
/// left to wait for it, which would keep the process alive.
pub const RUNTIME_SHUTDOWN_LIMIT: Duration = Duration::from_secs(2);

/// Drain every Channel at once, each bounded by `grace` (§113: each Channel
/// shuts down independently). `None` marks a drain that did not finish in
/// time; it is abandoned. A drain that panicked counts as finished, without a
/// final snapshot.
async fn drain_all<F>(
    drains: Vec<(ChannelId, F)>,
    grace: Duration,
) -> Vec<(ChannelId, Option<Option<ChannelSnapshot>>)>
where
    F: std::future::Future<Output = Option<ChannelSnapshot>> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    let mut ids = HashMap::new();
    for (id, drain) in drains {
        let task = set.spawn(async move { tokio::time::timeout(grace, drain).await.ok() });
        ids.insert(task.id(), id);
    }
    let mut drained = Vec::new();
    while let Some(joined) = set.join_next_with_id().await {
        match joined {
            Ok((task, result)) => drained.push((ids[&task], result)),
            Err(error) => drained.push((ids[&error.id()], Some(None))),
        }
    }
    drained
}

/// The renderer a Channel's `.disp` records with (§54): its stored primary
/// display view (kept current by `set_view_config`), or the default view.
fn display_renderer(config: &ChannelConfig) -> DisplayView {
    config
        .display
        .views
        .first()
        .map(build_display_view)
        .unwrap_or_default()
}

/// Phase 2 of [`Listener::stop`]: drain a Channel's handle to a stop and return
/// one **final snapshot** taken from the returned pipeline *after* it finalized
/// (so its last diagnostics — "Channel stopped", "Raw recording stopped" — are
/// included). Owns the handle (no `&mut Listener`), so a caller can wrap *this*
/// in a timeout and, if it fires, still run `Listener::finish_stop` — the
/// cleanup never depends on the drain completing. `None` for a TCP listener or a
/// start-time fault (no pipeline).
async fn drain_handle(handle: Option<ChannelHandle>) -> Option<ChannelSnapshot> {
    match handle {
        Some(ChannelHandle::Data(tasks)) => {
            // `None` if the pipeline task panicked — no final snapshot then,
            // which callers already tolerate (same as a TCP listener).
            tasks.stop().await.map(|mut pipeline| pipeline.snapshot())
        }
        Some(ChannelHandle::TcpListener(listener)) => {
            listener.stop().await;
            None
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{schema::InterfaceConfig, templates};
    use crate::record::FileRotationPolicy;

    fn udp_channel() -> ChannelConfig {
        // Template binds 0.0.0.0:0 (ephemeral) — binds cleanly in tests.
        templates::udp_template()
    }

    #[tokio::test]
    async fn shutdown_drains_every_channel_at_once_and_abandons_a_stuck_one() {
        // §113: each Channel shuts down independently, and a drain that cannot
        // finish is abandoned at the grace period instead of holding up exit.
        type Drain =
            std::pin::Pin<Box<dyn std::future::Future<Output = Option<ChannelSnapshot>> + Send>>;
        let (stuck, quick) = (ChannelId::new(), ChannelId::new());
        let drains: Vec<(ChannelId, Drain)> = vec![
            (stuck, Box::pin(std::future::pending())),
            (quick, Box::pin(async { None })),
        ];
        let started = std::time::Instant::now();
        let mut finished: Vec<(ChannelId, bool)> = drain_all(drains, Duration::from_millis(200))
            .await
            .into_iter()
            .map(|(id, drained)| (id, drained.is_some()))
            .collect();
        finished.sort_by_key(|(id, _)| *id != stuck);
        assert_eq!(finished, vec![(stuck, false), (quick, true)]);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the grace bounds the whole shutdown, not each channel in turn"
        );
    }

    #[test]
    fn an_incomplete_finalization_is_logged_naming_the_channel() {
        let mut listener = Listener::with_default_capacities();
        let mut config = udp_channel();
        config.name = crate::core::ChannelName::new("GPS");
        let id = listener.add_channel(config);
        assert_eq!(listener.note_finalization_incomplete(id), "GPS");
        let retained = &listener.channels[&id].retained_diagnostics;
        assert!(
            retained.iter().any(|d| d.message.starts_with(
                "finalization incomplete on channel GPS: its stop did not finish within 3 s"
            )),
            "{retained:?}"
        );
    }

    #[test]
    fn rotation_coerces_a_refuse_policy_to_append_in_the_recording_settings() {
        // Refuse is meaningless with rotation (each period is a fresh file; a restart
        // re-opens the current period and must append). `settings_from` coerces it so a
        // UI leaving the default Refuse can't cause a spurious "file exists" fault (§59).
        use crate::config::schema::RawRecordingConfig;
        use crate::record::OverwritePolicy;
        let listener = Listener::with_default_capacities();
        let raw = RawRecordingConfig {
            enabled: true,
            destination: Some(std::path::PathBuf::from("C:/tmp/rec")),
            timestamp_enabled: false,
            overwrite_policy: OverwritePolicy::Refuse,
            file_rotation: FileRotationPolicy::Hourly,
            disk_guard: None,
            size_cap: None,
        };
        let settings = listener.settings_from(&raw, "GPS").unwrap();
        assert_eq!(settings.overwrite, OverwritePolicy::AppendIfExists);

        // Without rotation, Refuse is preserved (it is a meaningful single-file policy).
        let no_rot = RawRecordingConfig {
            file_rotation: FileRotationPolicy::None,
            ..raw
        };
        assert_eq!(
            listener.settings_from(&no_rot, "GPS").unwrap().overwrite,
            OverwritePolicy::Refuse
        );
    }

    /// The timestamp sidecar reaches the recorder from config (§57). The writer
    /// was always there; until the GUI grew a control, nothing ever set the flag,
    /// so this pins the path the control drives.
    #[test]
    fn the_timestamp_sidecar_flag_reaches_the_recording_settings() {
        use crate::config::schema::RawRecordingConfig;
        use crate::record::OverwritePolicy;
        let listener = Listener::with_default_capacities();
        let off = RawRecordingConfig {
            enabled: true,
            destination: Some(std::path::PathBuf::from("C:/tmp/rec.raw")),
            timestamp_enabled: false,
            overwrite_policy: OverwritePolicy::Overwrite,
            file_rotation: FileRotationPolicy::None,
            disk_guard: None,
            size_cap: None,
        };
        assert!(!listener.settings_from(&off, "GPS").unwrap().timestamps);

        let on = RawRecordingConfig {
            timestamp_enabled: true,
            ..off
        };
        assert!(listener.settings_from(&on, "GPS").unwrap().timestamps);
    }

    #[test]
    fn unset_diagnostic_limits_fall_back_to_the_bounded_default() {
        // §88/§124: a channel config with no explicit event/warning/error limits
        // (the template default) still gets bounded diagnostic retention — the
        // base-caps default, not the ~1 M backstop. An explicit limit wins.
        let listener = Listener::with_default_capacities();
        let caps = listener.channel_caps(&udp_channel());
        assert!(caps.event_retention.is_some());
        assert!(caps.warning_retention.is_some());
        assert!(caps.error_retention.is_some());

        let mut config = udp_channel();
        config.retention.warning_limit = Some(7);
        assert_eq!(listener.channel_caps(&config).warning_retention, Some(7));
    }

    #[tokio::test]
    async fn remove_channel_drops_it_from_the_registry() {
        let mut listener = Listener::with_default_capacities();
        let id = listener.add_channel(udp_channel());
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));

        listener.remove_channel(id).await.unwrap();
        assert_eq!(listener.state(id), None, "the channel is gone");
        // Removing it again is an error (it's unknown now).
        assert!(listener.remove_channel(id).await.is_err());
    }

    #[tokio::test]
    async fn diagnostics_are_retained_across_a_stop_start_cycle() {
        // §88 (within session): a restarted channel keeps the previous run's diagnostics
        // log instead of starting blank. After start→stop→start, the new pipeline's
        // snapshot carries the prior run's "Channel stopped" alongside a fresh
        // "Channel started".
        let mut listener = Listener::with_default_capacities();
        let id = listener.add_channel(udp_channel());

        listener.start(id).await.unwrap();
        listener.stop(id).await.unwrap();
        listener.start(id).await.unwrap();

        let snap = listener
            .snapshot(id)
            .await
            .expect("a running channel snapshots");
        let events: Vec<&str> = snap
            .diagnostics
            .events
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            events.contains(&"Channel stopped"),
            "the prior run's diagnostics are retained: {events:?}"
        );
        assert!(
            events.contains(&"Channel started"),
            "the new run records its own start: {events:?}"
        );

        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn display_recording_setup_failure_records_a_diagnostic_not_a_silent_warning() {
        // A Display recording enabled with no destination can't start. Like raw recording,
        // it must record a concrete ERROR diagnostic (not a reason-less WarningRaised) and
        // not fault the channel — reception continues.
        use crate::config::schema::DisplayRecordingConfig;
        let mut config = udp_channel();
        config.display_recording = DisplayRecordingConfig {
            enabled: true,
            destination: None, // no destination → setup fails
            ..Default::default()
        };

        let mut listener = Listener::with_default_capacities();
        let id = listener.add_channel(config);
        listener.start(id).await.unwrap();
        assert_eq!(
            listener.state(id),
            Some(ChannelState::Running),
            "a display-recording setup failure does not fault the channel (§55)"
        );

        let snap = listener
            .snapshot(id)
            .await
            .expect("running channel snapshots");
        assert!(
            snap.diagnostics
                .errors
                .iter()
                .any(|e| e.message.contains("Display recording")),
            "the display-recording failure is a concrete diagnostic: {:?}",
            snap.diagnostics.errors
        );

        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn a_start_fault_is_retained_as_an_error_diagnostic() {
        // A bind/start fault never runs a pipeline, so it isn't in any pipeline log; it's
        // retained as an ERROR diagnostic so it shows in the diagnostics list (served via
        // a minimal snapshot for the faulted channel) and survives the next restart.
        let mut listener = Listener::with_default_capacities();
        let mut config = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "not-an-ip-address".to_string();
        }
        let id = listener.add_channel(config);

        assert!(listener.start(id).await.is_err());
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));

        // The faulted channel serves a snapshot carrying the fault as an ERROR.
        let snap = listener
            .snapshot(id)
            .await
            .expect("a faulted channel serves its retained diagnostics");
        assert!(
            snap.diagnostics
                .errors
                .iter()
                .any(|e| e.message.starts_with("UDP_Channel:")),
            "the start fault is retained as a channel-named ERROR: {:?}",
            snap.diagnostics.errors
        );
    }

    #[tokio::test]
    async fn remove_running_channel_stops_then_drops_it() {
        let mut listener = Listener::with_default_capacities();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        listener.remove_channel(id).await.unwrap();
        assert_eq!(listener.state(id), None);
    }

    #[tokio::test]
    async fn start_then_stop_drives_state_and_emits_events() {
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let id = listener.add_channel(udp_channel());
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));

        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelStarted(id)
        );

        listener.stop(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelStopped(id)
        );
    }

    #[tokio::test]
    async fn illegal_transitions_are_rejected() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());

        // Stop from Stopped is illegal.
        assert!(matches!(
            listener.stop(id).await,
            Err(OrchestratorError::IllegalTransition { .. })
        ));

        listener.start(id).await.unwrap();
        // Start from Running is illegal.
        assert!(matches!(
            listener.start(id).await,
            Err(OrchestratorError::IllegalTransition { .. })
        ));
        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn bad_config_faults_then_can_be_reset() {
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "not-an-ip-address".to_string();
        }
        let id = listener.add_channel(config);

        assert!(listener.start(id).await.is_err());
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelFaulted(id)
        );

        // Start from Faulted is illegal — it must be reset first (§9).
        assert!(matches!(
            listener.start(id).await,
            Err(OrchestratorError::IllegalTransition { .. })
        ));
        // Stop returns a Faulted channel to Stopped (§8.5).
        listener.stop(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    #[tokio::test]
    async fn commit_and_start_recovers_a_faulted_channel_without_an_explicit_stop() {
        // A Faulted channel, fixed by committing a good config and starting in one
        // call — commit_and_start owns the Faulted→Stopped→Starting recovery, so the
        // caller never issues the intermediate Stop (the bug the GUI worked around).
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let mut bad = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut bad.interface {
            udp.bind_address = "not-an-ip-address".to_string();
        }
        let id = listener.add_channel(bad);
        assert!(listener.start(id).await.is_err());
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));

        // A plain start from Faulted is still illegal directly...
        assert!(listener.start(id).await.is_err());
        // ...but commit_and_start with a good config recovers and comes up Running.
        listener
            .commit_and_start(id, Some(udp_channel()), true)
            .await
            .unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));
    }

    #[tokio::test]
    async fn commit_and_start_restarts_a_running_channel_onto_the_new_config() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        // Commit a fresh (still-valid) config and restart in one call.
        listener
            .commit_and_start(id, Some(udp_channel()), true)
            .await
            .unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));
    }

    #[tokio::test]
    async fn stop_if_live_is_a_noop_on_a_stopped_channel() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());

        // Stopped → no stop happened, and no IllegalTransition error to swallow.
        assert!(!listener.stop_if_live(id).await.unwrap());
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));

        listener.start(id).await.unwrap();
        // Running → it stops.
        assert!(listener.stop_if_live(id).await.unwrap());
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    #[tokio::test]
    async fn shutdown_lands_channels_in_stopped_and_emits_the_stop() {
        // shutdown times out only the drain, never the cleanup: every live channel
        // ends Stopped with a ChannelStopped emitted (see the cancel-vs-drain split).
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelStarted(id)
        );

        listener.shutdown().await;
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelStopped(id)
        );
    }

    #[tokio::test]
    async fn apply_pending_restarts_with_the_new_config() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();

        // Accept a renamed config; the running channel is unaffected until applied.
        let mut updated = udp_channel();
        updated.name = crate::core::ChannelName::new("renamed");
        listener.set_pending_config(id, updated).unwrap();
        assert!(listener.has_pending(id));
        assert_eq!(listener.config(id).unwrap().name.as_str(), "UDP_Channel");

        listener.apply_pending(id).await.unwrap();
        assert!(!listener.has_pending(id));
        assert_eq!(listener.config(id).unwrap().name.as_str(), "renamed");
        // Coordinated restart left it Running.
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        listener.shutdown().await;
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    #[tokio::test]
    async fn rename_takes_effect_immediately_without_restarting() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();

        listener
            .rename(id, crate::core::ChannelName::new("Bridge feed"))
            .unwrap();
        // The label changed and the channel kept Running — no coordinated restart.
        assert_eq!(listener.config(id).unwrap().name.as_str(), "Bridge feed");
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        listener.shutdown().await;
    }

    #[test]
    fn rename_also_updates_a_queued_pending_config() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());

        // A pending edit is queued (e.g. a port change the user has not applied).
        let mut pending = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut pending.interface {
            udp.port = 9100;
        }
        listener.set_pending_config(id, pending).unwrap();

        // Renaming updates the live config and the pending one, so applying the
        // pending edit later does not revert the new name.
        listener
            .rename(id, crate::core::ChannelName::new("Renamed"))
            .unwrap();
        assert_eq!(listener.config(id).unwrap().name.as_str(), "Renamed");
        assert_eq!(listener.pending(id).unwrap().name.as_str(), "Renamed");
    }

    #[tokio::test]
    async fn display_views_can_be_paused_by_id() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        // The UDP template configures a Raw + Hex display → two views (§84).
        let id = listener.add_channel(udp_channel());
        assert!(listener.display_views(id).is_empty()); // none while Stopped

        listener.start(id).await.unwrap();
        let views = listener.display_views(id);
        assert_eq!(views.len(), 2);

        // Pause/resume a known view; an unknown view id errors.
        assert!(listener.pause_display(id, views[0]).is_ok());
        assert!(listener.resume_display(id, views[0]).is_ok());
        assert!(matches!(
            listener.pause_display(id, DisplayViewId::new()),
            Err(OrchestratorError::UnknownDisplayView(_))
        ));

        listener.stop(id).await.unwrap();
        assert!(listener.display_views(id).is_empty()); // cleared on stop
    }

    #[tokio::test]
    async fn spontaneous_fault_reconciles_state_then_clears_on_stop() {
        // ADR-006: the detached fault monitor can't mutate the orchestrator, so it
        // trips the shared flag. We trip it directly here (the exact signal the
        // monitor leaves) and assert the orchestrator reconciles without a command.
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        listener.channels[&id]
            .faulted
            .store(true, Ordering::Relaxed);

        // state() reads Faulted even though the stored lifecycle state is Running.
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));
        // Command validation uses the reconciled state: Start is now illegal (§9).
        assert!(matches!(
            listener.start(id).await,
            Err(OrchestratorError::IllegalTransition { .. })
        ));

        // Stop recovers Faulted → Stopped (§8.5) and clears the flag.
        listener.stop(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    #[tokio::test]
    async fn auto_reconnect_restarts_a_faulted_channel() {
        // §162: with reconnect enabled, a faulted channel is re-Started after the
        // backoff when reconnect_tick is driven.
        use crate::config::ReconnectPolicy;
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        config.reconnect = ReconnectPolicy {
            enabled: true,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
            multiplier: 2.0,
            max_attempts: None,
        };
        let id = listener.add_channel(config);
        listener.start(id).await.unwrap();

        // Simulate a spontaneous fault (trip the flag, ADR-006).
        listener.channels[&id]
            .faulted
            .store(true, Ordering::Relaxed);
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));

        // First tick only arms the backoff timer; the channel is still faulted.
        listener.reconnect_tick().await;
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));

        // After the backoff elapses, a tick reconnects it.
        tokio::time::sleep(Duration::from_millis(5)).await;
        listener.reconnect_tick().await;
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        let mut reconnected = false;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, RuntimeEvent::ChannelReconnected(_)) {
                reconnected = true;
            }
        }
        assert!(reconnected, "expected a ChannelReconnected event");

        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn auto_reconnect_gives_up_after_max_attempts() {
        // §162: a channel that cannot start (bad config) gives up after the
        // configured number of attempts and stays faulted.
        use crate::config::ReconnectPolicy;
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "not-an-ip-address".to_string(); // start always fails
        }
        config.reconnect = ReconnectPolicy {
            enabled: true,
            initial_backoff_ms: 1,
            max_backoff_ms: 5,
            multiplier: 1.0,
            max_attempts: Some(1),
        };
        let id = listener.add_channel(config);

        // The initial start fails → Faulted; reconnect then retries and gives up.
        assert!(listener.start(id).await.is_err());
        for _ in 0..6 {
            listener.reconnect_tick().await;
            tokio::time::sleep(Duration::from_millis(3)).await;
        }

        let mut gave_up = false;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, RuntimeEvent::ChannelReconnectGaveUp(_)) {
                gave_up = true;
            }
        }
        assert!(gave_up, "expected a ChannelReconnectGaveUp event");
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));
        assert!(listener.reconnect_exhausted(id));
    }

    #[tokio::test]
    async fn a_channel_that_cannot_start_starts_once_its_resource_appears() {
        // §3.1: start what can start. The boot case — a port in use (or an
        // adapter not yet enumerated) — fails the first start, and the
        // reconnect policy starts the Channel once the resource is free.
        use crate::config::ReconnectPolicy;
        let port = crate::test_ports::reserve_udp_port();
        let blocker = std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap();
        let mut listener = Listener::with_default_capacities();
        let _events = listener.take_events();
        let mut config = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "127.0.0.1".to_string();
            udp.port = port;
        }
        config.reconnect = ReconnectPolicy {
            enabled: true,
            initial_backoff_ms: 1,
            max_backoff_ms: 5,
            multiplier: 1.0,
            max_attempts: None,
        };
        let id = listener.add_channel(config);
        assert!(listener.start(id).await.is_err(), "the port is taken");
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));
        for _ in 0..3 {
            listener.reconnect_tick().await;
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        assert_eq!(
            listener.state(id),
            Some(ChannelState::Faulted),
            "still taken"
        );

        drop(blocker);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while listener.state(id) != Some(ChannelState::Running) {
            assert!(tokio::time::Instant::now() < deadline, "never started");
            listener.reconnect_tick().await;
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        assert!(!listener.reconnect_exhausted(id));
        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn repeated_identical_start_faults_keep_one_counted_entry() {
        // A reconnect loop retries the same failing start for as long as the fault
        // lasts. Each failure must update one entry, not add another (§88).
        use crate::diagnostics::DiagnosticSeverity;
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let mut config = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "not-an-ip-address".to_string(); // start always fails
        }
        let id = listener.add_channel(config);
        // Stop then Start, as `reconnect_tick` does.
        for _ in 0..1000 {
            let _ = listener.stop(id).await;
            assert!(listener.start(id).await.is_err());
        }

        let snapshot = listener.snapshot(id).await.unwrap();
        let errors: Vec<_> = snapshot
            .diagnostics
            .to_sorted_vec()
            .into_iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .collect();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.ends_with("(failed 1000 times)"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn retained_start_faults_are_capped_per_severity() {
        // Different faults in a row each get an entry, but the severity's limit
        // still bounds them, oldest evicted first (§88, §89).
        let mut listener = Listener::with_default_capacities();
        let id = listener.add_channel(udp_channel());
        let channel = listener.channels.get_mut(&id).unwrap();
        for n in 0..10 {
            channel.retain_diagnostic(
                crate::diagnostics::Diagnostic::error(format!("fault {n}")),
                3,
            );
        }
        let messages: Vec<_> = channel
            .retained_diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(messages, ["fault 7", "fault 8", "fault 9"]);
    }

    #[tokio::test]
    async fn serial_control_is_unavailable_on_non_serial_channels() {
        // §161: control-line commands/queries only apply to running serial
        // Channels; a UDP channel reports it has no serial control.
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();

        assert!(matches!(
            listener.set_rts(id, true).await,
            Err(OrchestratorError::SerialControlUnavailable(_))
        ));
        assert!(listener.serial_control_lines(id).is_none());

        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_channel_is_an_error() {
        let mut listener = Listener::with_default_capacities();
        let ghost = ChannelId::new();
        assert!(matches!(
            listener.start(ghost).await,
            Err(OrchestratorError::UnknownChannel(_))
        ));
    }

    #[tokio::test]
    async fn recording_enable_failure_does_not_fault_the_channel() {
        use crate::config::schema::RawRecordingConfig;
        use crate::record::OverwritePolicy;

        // Pre-create the destination so a Refuse policy makes enabling fail
        // (§55/§121).
        let mut path = std::env::temp_dir();
        path.push(format!("listener-orch-{}.bin", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, b"existing").await.unwrap();

        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        config.raw_recording = RawRecordingConfig {
            enabled: true,
            destination: Some(path.clone()),
            timestamp_enabled: false,
            overwrite_policy: OverwritePolicy::Refuse,
            file_rotation: FileRotationPolicy::None,
            disk_guard: None,
            size_cap: None,
        };
        let id = listener.add_channel(config);

        // Start succeeds: reception runs despite the failed recording enable.
        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        // The start-time recording failure surfaces as RecordingFaulted (the same path
        // as the live Begin), so the GUI shows it — it previously only warned, which the
        // GUI didn't surface (start-time recording failed silently). "Record on start"
        // is now begun by the pipeline task, so the event arrives just after start.
        let mut saw_recording_fault = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
                Ok(Some(RuntimeEvent::RecordingFaulted(_, tap))) => {
                    assert_eq!(tap, crate::core::RecordingTap::Raw, "raw lane faulted");
                    saw_recording_fault = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => continue,
            }
        }
        assert!(
            saw_recording_fault,
            "a RecordingFaulted event should surface the failure"
        );
        // The pre-existing file was not clobbered (§121).
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"existing");

        listener.stop(id).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;
    }

    /// The at-rest cross-check that surfaced the retained-totals gap: after
    /// Stop, the channel's snapshot must still read the run's exact byte
    /// total (rate zeroed) — previously the stopped-channel snapshot served
    /// default (zero) liveness and the GUI's next poll wiped the readout,
    /// exactly when the user compares it against the sender's total.
    #[tokio::test]
    async fn stopped_channel_retains_exact_totals_at_rest() {
        let port = crate::test_ports::reserve_udp_port();
        let mut listener = Listener::with_default_capacities();
        let mut config = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "127.0.0.1".to_string();
            udp.port = port;
        }
        let id = listener.add_channel(config);
        listener.start(id).await.unwrap();

        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let payload = b"$GPGGA,retained-totals*00\r\n";
        for _ in 0..5 {
            sender.send_to(payload, ("127.0.0.1", port)).unwrap();
        }
        let expected = (5 * payload.len()) as u64;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let (
            live_handoff_timing,
            live_processing_timing,
            live_recent_processing,
            live_chunks,
            live_transport_health,
        ) = loop {
            if let Some(stats) = listener.channel_stats(id).await {
                if stats.activity.total_bytes == expected {
                    break (
                        stats.ingest_delay,
                        stats.ingest_processing,
                        stats.recent_ingest_processing,
                        stats.chunk_shape,
                        stats.transport_health,
                    );
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "datagrams did not arrive"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        listener.stop(id).await.unwrap();
        // No live pipeline: both polled lanes keep serving the retained final
        // liveness — exact total, zero rate — stamped with the effective state,
        // so an overview tab (stats) and the detail pane (snapshot) both read
        // exact at rest and self-correct a dropped ChannelStopped event.
        let stats = listener.channel_stats(id).await.expect("retained stats");
        assert_eq!(stats.state, ChannelState::Stopped, "state served at rest");
        assert_eq!(stats.activity.total_bytes, expected, "exact total at rest");
        assert_eq!(
            stats.ingest_delay, live_handoff_timing,
            "handoff timing retained at rest"
        );
        assert!(stats.ingest_delay.sample_count() > 0);
        assert_eq!(
            stats.ingest_processing, live_processing_timing,
            "processing timing retained at rest"
        );
        assert!(stats.ingest_processing.sample_count() > 0);
        assert_eq!(stats.recent_ingest_processing, live_recent_processing);
        assert_eq!(stats.chunk_shape, live_chunks);
        assert_eq!(stats.transport_health, live_transport_health);
        assert_eq!(stats.chunk_shape.chunk_count(), 5);
        assert!(stats.recent_ingest_delay.sample_count() > 0);
        let snap = listener.snapshot(id).await.expect("retained snapshot");
        assert_eq!(snap.state, ChannelState::Stopped, "state served at rest");
        assert_eq!(snap.activity.total_bytes, expected, "exact total at rest");
        assert_eq!(snap.activity.bytes_per_sec, 0.0, "no rate at rest");
        assert_eq!(
            snap.ingest_delay, live_handoff_timing,
            "both lanes agree on handoff timing at rest"
        );
        assert_eq!(
            snap.ingest_processing, live_processing_timing,
            "both lanes agree on processing timing at rest"
        );
        assert_eq!(snap.recent_ingest_processing, live_recent_processing);
        assert_eq!(snap.chunk_shape, live_chunks);
        assert_eq!(snap.transport_health, live_transport_health);
        assert_eq!(snap.recent_ingest_delay, stats.recent_ingest_delay);
        let summary = snap
            .last_run_summary
            .as_ref()
            .expect("completed run summary retained");
        assert!(summary.final_snapshot_complete);
        assert_eq!(summary.total_bytes, expected);
        assert_eq!(summary.chunk_shape.chunk_count(), 5);
        assert_eq!(summary.ingest_delay, live_handoff_timing);
        assert!(summary
            .to_report_text()
            .contains(&format!("received_bytes={expected}\n")));
    }

    #[tokio::test]
    async fn two_channels_cannot_record_to_one_destination_and_the_channel_stays_running() {
        // §121 / ADR-014: the second recording to the same file is refused by the advisory
        // lock as a *recording* fault — the channel keeps Running (reception is fine), it
        // is not channel-faulted. The lock is the enforcement point (the friendly named
        // pre-check was removed because it wrongly faulted the channel).
        use crate::config::schema::RawRecordingConfig;
        use crate::record::OverwritePolicy;
        let mut dir = std::env::temp_dir();
        dir.push(format!("listener-dest-{}.raw", uuid::Uuid::new_v4()));

        let raw_to = |path: &std::path::Path| RawRecordingConfig {
            enabled: true,
            destination: Some(path.to_path_buf()),
            timestamp_enabled: false,
            overwrite_policy: OverwritePolicy::Overwrite,
            file_rotation: FileRotationPolicy::None,
            disk_guard: None,
            size_cap: None,
        };

        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut first = udp_channel();
        first.name = crate::core::ChannelName::new("First");
        first.raw_recording = raw_to(&dir);
        let first_id = listener.add_channel(first);

        let mut second = udp_channel();
        second.name = crate::core::ChannelName::new("Second");
        second.raw_recording = raw_to(&dir); // same destination
        let second_id = listener.add_channel(second);

        // Both channels start fine (reception runs); the second's recording faults on the
        // lock. The first file opens in its recorder's task, so wait for it to start
        // before starting the second — otherwise either could win the lock.
        listener.start(first_id).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first recording never started"
            );
            if let Ok(Some(RuntimeEvent::RecordingStarted(id, _))) =
                tokio::time::timeout(Duration::from_millis(200), events.recv()).await
            {
                if id == first_id {
                    break;
                }
            }
        }
        listener.start(second_id).await.unwrap();
        assert_eq!(listener.state(second_id), Some(ChannelState::Running));

        let mut saw_recording_fault = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
                Ok(Some(RuntimeEvent::RecordingFaulted(id, _))) if id == second_id => {
                    saw_recording_fault = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => continue,
            }
        }
        assert!(
            saw_recording_fault,
            "the second recording should fault on the destination lock"
        );

        listener.stop(first_id).await.unwrap();
        listener.stop(second_id).await.unwrap();
        let _ = tokio::fs::remove_file(&dir).await;
        let mut lock = dir.clone().into_os_string();
        lock.push(".lock");
        let _ = tokio::fs::remove_file(std::path::PathBuf::from(lock)).await;
    }
}
