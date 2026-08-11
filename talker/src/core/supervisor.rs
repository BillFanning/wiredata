//! Channel supervision shared by the CLI and the GUI (ADR-019).
//!
//! [`TalkerSupervisor`] owns the runner threads, their command/control/status
//! channels, the draining buckets, and the per-channel observer telemetry that
//! used to live in the GUI module — restoring the spec §2.2 boundary (channel
//! collection and management are business logic, so they live in core; `cli`
//! and `gui` are thin layers over this one API).
//!
//! Threading model is unchanged (ADR-002): no supervisor thread. The owner
//! calls [`poll`](TalkerSupervisor::poll) at its own cadence (the GUI each
//! frame, woken by the notify callback) and the supervisor drains statuses
//! non-blockingly. Runner threads are never joined on the caller's thread —
//! a stopped runner drains in the background and is reaped by `poll`; a slot
//! restart hands the predecessors to the *new* runner thread, which joins
//! them before reopening the interface (a serial port is exclusive — the old
//! holder must drop first).
//!
//! Unlike the old GUI-owned flow, a stopped runner's **status receiver is
//! kept** until its thread exits, so the final `Counters` emitted at stop
//! (ADR-018) still lands and totals read exact at rest.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{Receiver, TrySendError};

use crate::core::channel::{ChannelId, InterfaceConfig};
use crate::core::internal_fault::InternalFaultTally;
use crate::core::message::MessageConfig;
use crate::core::run_summary::{RunId, RunSummary};
use crate::core::runner::{
    self, CommandExecution, CommandId, CommandTarget, ObserverPolicy, RunnerControlStatus,
    RunnerIdentity, TalkerCommand, TalkerHandle, TalkerStatus,
};
use crate::core::scheduler::Schedule;
use crate::core::telemetry::{MessageTiming, SendTimingTelemetry};
use crate::core::timing::{CadenceAlignment, TimerStatus};

/// Bound on each runner's status queue. Occupancy near this cap means
/// observer updates are about to be dropped (and counted) — surfaced as the
/// "Status queue" readout.
pub const STATUS_QUEUE_CAP: usize = 256;

/// Bound on each runner's command queue. Commands are tiny and rare; a full
/// queue means the runner is wedged in a blocking send.
const CMD_QUEUE_CAP: usize = 32;

/// One reliable result for the start-time open, every command that can be
/// queued at once, and the final run summary. Keeping this separate from sampled
/// telemetry prevents observer pressure from erasing control truth.
const CONTROL_QUEUE_CAP: usize = CMD_QUEUE_CAP + 2;

#[derive(Clone, Debug)]
struct CommandFailure {
    id: CommandId,
    message: String,
}

/// Per-channel observer state, updated by [`TalkerSupervisor::poll`] from the
/// runner's ADR-018 status lanes. Everything cumulative comes straight from
/// `TalkerStatus::Counters`, so it self-corrects across dropped updates.
#[derive(Clone, Debug, Default)]
pub struct ChannelTelemetry {
    /// Running send count across all messages in this channel.
    pub total_count: u64,
    /// Cumulative wire bytes sent.
    pub total_bytes: u64,
    /// Per-message running send counts, indexed by schedule position.
    ///
    /// Shared rather than owned: the selected channel's telemetry is cloned
    /// once per repaint, and these grow with the message count, so a refcount
    /// bump keeps a wide schedule off the per-frame allocation path.
    pub per_message_counts: Arc<[u64]>,
    /// Per-message cumulative timing on the same index basis. Carries both
    /// halves of a cadence problem: what each message suffered
    /// (`deadline_lateness`) and what it cost the others (`blocked_others`).
    /// Shared for the same reason as `per_message_counts`, and larger.
    pub per_message_timing: Arc<[MessageTiming]>,
    /// Status updates the runner discarded because the queue was full.
    pub dropped_statuses: u64,
    /// Sends skipped under the scheduler's stall policy — cadence health.
    pub missed_sends: u64,
    /// Interface send attempts that failed.
    pub failed_sends: u64,
    /// Due fires intentionally suppressed while send retry backoff was active.
    pub suppressed_sends: u64,
    /// Bounded cumulative send-path timing measurements from the runner.
    pub timing: SendTimingTelemetry,
    /// Approximate last-ten-seconds timing from fixed one-second segments.
    pub recent_timing: SendTimingTelemetry,
    /// Exact monotonic instant when `recent_timing` was collapsed by the
    /// runner. `None` until the first counter snapshot arrives.
    pub recent_timing_captured_at: Option<Instant>,
    /// Whether the retained timing came from the runner's mandatory
    /// exact-at-rest snapshot.
    pub recent_timing_is_final: bool,
    /// Current platform deadline-wait policy and its schedule input.
    pub timer: TimerStatus,
    /// Status-queue occupancy sampled at the last poll, and its high-water
    /// mark since the channel started.
    pub queue_len: usize,
    pub queue_peak: usize,
    /// Errors observed since the channel started: connection/open errors
    /// plus undeliverable commands.
    pub errors_total: u64,
    /// The latest **interface** error (connection/open failure). Cleared by a
    /// delivered payload sample or a `SendRecovered` — live proof the
    /// interface works again — and on start.
    pub last_error: Option<String>,
    /// The latest **control-plane** error (an undeliverable Stop/interface-
    /// update/interval command). A healthy sample must NOT clear this — the
    /// wire working says nothing about a command that never arrived. Cleared
    /// only by a later successfully executed command for the same target, or
    /// on start.
    pub command_error: Option<String>,
    /// Latest failure per independently recoverable control target. The public
    /// `command_error` above is the newest entry, retained as the GUI-facing cache.
    command_failures: BTreeMap<CommandTarget, CommandFailure>,
}

impl ChannelTelemetry {
    /// The banner the UI shows: a pending control-plane failure (needs the
    /// user's attention — the on-screen state diverged from the runner's)
    /// wins over an interface error.
    pub fn banner_error(&self) -> Option<&str> {
        self.command_error.as_deref().or(self.last_error.as_deref())
    }

    fn record_command_failure(&mut self, id: CommandId, target: CommandTarget, message: String) {
        self.errors_total += 1;
        let replace = self
            .command_failures
            .get(&target)
            .is_none_or(|current| id >= current.id);
        if replace {
            self.command_failures
                .insert(target, CommandFailure { id, message });
        }
        self.refresh_command_error();
    }

    fn resolve_command(&mut self, id: CommandId, target: CommandTarget) {
        if self
            .command_failures
            .get(&target)
            .is_some_and(|failure| failure.id <= id)
        {
            self.command_failures.remove(&target);
            self.refresh_command_error();
        }
    }

    fn refresh_command_error(&mut self) {
        self.command_error = self
            .command_failures
            .values()
            .max_by_key(|failure| failure.id)
            .map(|failure| failure.message.clone());
    }
}

/// One sampled send returned by [`TalkerSupervisor::poll`] — the exact wire
/// bytes, for a display pane. Cadence is the runner's [`ObserverPolicy`].
pub struct PayloadSample {
    /// The **current** slot index the sample drained from — the right key for
    /// positional display routing (the supervisor's receivers travel with
    /// their slots, so this is correct even after removals; the status's
    /// embedded stable id serves consumers outside the slot structure).
    pub slot: usize,
    pub payload: Vec<u8>,
    /// Byte positions in `payload` produced by lossy code-page fallback.
    pub replacement_wire_offsets: Vec<usize>,
}

/// How a command enqueue attempt went. `NotRunning` covers both "no runner in this
/// slot" and "the runner already exited" — for a Stop that is moot, for
/// anything else it is surfaced in the telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    Enqueued,
    /// The command queue is full — the runner may be wedged in a blocking
    /// send. Recorded in the channel's telemetry.
    QueueFull,
    NotRunning,
}

/// The immediate result of submitting a live mutation. `Enqueued` means only that
/// the runner owns the command; [`CommandCompletion`] reports what execution did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSubmission {
    pub id: CommandId,
    pub outcome: CommandOutcome,
}

/// Configuration positively confirmed as owned by the live runner. Draft and
/// profile state are intentionally separate from this runtime fact.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedRunConfig {
    pub interface: InterfaceConfig,
    pub cadence_alignment: CadenceAlignment,
    pub messages: Vec<MessageConfig>,
}

/// The effect retained by the supervisor until the runner confirms execution.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandEffect {
    Interface(InterfaceConfig),
    MessageInterval { index: usize, interval_ms: u64 },
}

impl CommandEffect {
    fn target(&self) -> CommandTarget {
        match self {
            Self::Interface(_) => CommandTarget::Interface,
            Self::MessageInterval { index, .. } => CommandTarget::MessageInterval(*index),
        }
    }
}

/// Reliable completion surfaced to presentation layers after supervisor state has
/// been reconciled. Failed commands never mutate the applied runtime baseline.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandCompletion {
    Applied {
        channel: ChannelId,
        id: CommandId,
        effect: CommandEffect,
    },
    Failed {
        channel: ChannelId,
        id: CommandId,
        target: CommandTarget,
        message: String,
    },
}

struct PendingCommand {
    effect: CommandEffect,
}

/// A stopped runner still winding down: its thread, plus its status receiver
/// so the final `Counters` (ADR-018) is still drained into the telemetry.
struct DrainingRunner {
    thread: std::thread::JoinHandle<()>,
    control_rx: Receiver<RunnerControlStatus>,
    status_rx: Receiver<TalkerStatus>,
}

/// One channel slot's internal bookkeeping faults.
///
/// Every one of these means talker's own state machine disagreed with itself.
/// None of them is a statement about the channel's link, which is why they are
/// reported through [`InternalFaultTally`] — see [`crate::core::internal_fault`]
/// for the two rules that module keeps together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SlotFaults {
    /// The runner opened an interface that is not the one the start requested.
    interface_mismatch: InternalFaultTally,
    /// An interface open was confirmed with no start waiting for it.
    interface_unexpected: InternalFaultTally,
    /// A command result arrived for a request no longer tracked.
    unknown_command: InternalFaultTally,
    /// A command result arrived naming a different target than the request.
    command_target_mismatch: InternalFaultTally,
    /// A completed run's summary named a different channel than its slot.
    summary_misrouted: InternalFaultTally,
}

struct Slot {
    /// Stable identity, minted when the slot is created (ADR-020). Slots
    /// shift positionally on removal; the id travels with the slot, and
    /// everything the slot's runners ever emitted is attributed to it.
    id: ChannelId,
    /// Human label frozen at the last start ("3", "'GPS'") — used in the
    /// supervisor's own log text so it matches the runner's. `None` before
    /// the first start (falls back to the id's `#N` form).
    label: Option<String>,
    handle: Option<TalkerHandle>,
    draining: Vec<DrainingRunner>,
    /// Reliable tails whose predecessor threads were handed to a replacement
    /// runner to join. Sampled status is stale for the replacement, but the
    /// self-contained completion summary must still be retained.
    retired_control: Vec<Receiver<RunnerControlStatus>>,
    telemetry: ChannelTelemetry,
    /// Whole run configuration confirmed by the runner's successful open.
    applied_run: Option<AppliedRunConfig>,
    /// Start request retained until `InterfaceOpened` confirms it.
    pending_start: Option<AppliedRunConfig>,
    pending_commands: BTreeMap<CommandId, PendingCommand>,
    /// Newest completed run for this stable channel slot.
    last_run_summary: Option<RunSummary>,
    /// Talker's own bookkeeping faults for this slot — not the channel's.
    faults: SlotFaults,
}

impl Slot {
    fn new() -> Self {
        Self {
            id: ChannelId::mint(),
            label: None,
            handle: None,
            draining: Vec::new(),
            retired_control: Vec::new(),
            telemetry: ChannelTelemetry::default(),
            applied_run: None,
            pending_start: None,
            pending_commands: BTreeMap::new(),
            last_run_summary: None,
            faults: SlotFaults::default(),
        }
    }

    /// The label for log text: the start-time label, else the id ("#N").
    fn display_label(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.id.to_string())
    }
}

/// The channel collection (spec §2.2): index-stable slots, one per configured
/// channel, mirroring the profile's channel order.
pub struct TalkerSupervisor {
    slots: Vec<Slot>,
    policy: ObserverPolicy,
    /// Cloned into every runner thread's observer/control notify callback
    /// (the GUI passes its repaint coalescer; the CLI passes nothing).
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Threads of removed slots, still winding down. Reaped by `poll` — the
    /// old GUI flow silently detached these.
    orphans: Vec<DrainingRunner>,
    command_completions: Vec<CommandCompletion>,
}

impl TalkerSupervisor {
    pub fn new(policy: ObserverPolicy) -> Self {
        Self {
            slots: Vec::new(),
            policy,
            notify: None,
            orphans: Vec::new(),
            command_completions: Vec::new(),
        }
    }

    /// Install the wake callback cloned into every runner spawned from now
    /// on (the GUI's repaint coalescer).
    pub fn set_notify(&mut self, notify: Arc<dyn Fn() + Send + Sync>) {
        self.notify = Some(notify);
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Append one empty slot (a newly added channel), minting its stable id.
    pub fn push_slot(&mut self) {
        self.slots.push(Slot::new());
    }

    /// Slot `i`'s stable [`ChannelId`] (ADR-020) — the key log-count and
    /// status attribution use; `None` for an out-of-range index.
    pub fn channel_id(&self, i: usize) -> Option<ChannelId> {
        self.slots.get(i).map(|s| s.id)
    }

    /// Current slot of a stable channel id.
    pub fn slot_index(&self, id: ChannelId) -> Option<usize> {
        self.slots.iter().position(|slot| slot.id == id)
    }

    /// Interface the current runner has positively confirmed open.
    pub fn applied_interface(&self, i: usize) -> Option<&InterfaceConfig> {
        Some(&self.slots.get(i)?.applied_run.as_ref()?.interface)
    }

    /// Whole configuration the current runner positively confirmed open.
    pub fn applied_run_config(&self, i: usize) -> Option<&AppliedRunConfig> {
        self.slots.get(i)?.applied_run.as_ref()
    }

    /// Drain reliable command completions accumulated by [`poll`](Self::poll).
    pub fn take_command_completions(&mut self) -> Vec<CommandCompletion> {
        std::mem::take(&mut self.command_completions)
    }

    /// Remove slot `i`, shifting the ones above it down (mirrors the channel
    /// list). A still-running runner is stopped; its thread joins the orphan
    /// bucket and is reaped by `poll`.
    pub fn remove_slot(&mut self, i: usize) {
        if i >= self.slots.len() {
            return;
        }
        self.stop(i);
        let slot = self.slots.remove(i);
        self.orphans.extend(slot.draining);
    }

    /// Ensure exactly `n` slots exist (profile load), dropping extras via
    /// [`remove_slot`](Self::remove_slot) semantics.
    pub fn resize_slots(&mut self, n: usize) {
        while self.slots.len() > n {
            self.remove_slot(self.slots.len() - 1);
        }
        while self.slots.len() < n {
            self.push_slot();
        }
    }

    pub fn is_running(&self, i: usize) -> bool {
        self.slots.get(i).is_some_and(|s| s.handle.is_some())
    }

    pub fn any_running(&self) -> bool {
        self.slots.iter().any(|s| s.handle.is_some())
    }

    /// Whether any stopped runner is still winding down (blocking send /
    /// interface timeout). Callers keep polling while true so reaping and
    /// tail-draining continue.
    pub fn any_draining(&self) -> bool {
        !self.orphans.is_empty()
            || self
                .slots
                .iter()
                .any(|s| !s.draining.is_empty() || !s.retired_control.is_empty())
    }

    /// This slot's telemetry (zeroed default for an out-of-range index, so
    /// render code can read unconditionally).
    pub fn telemetry(&self, i: usize) -> ChannelTelemetry {
        self.slots
            .get(i)
            .map(|s| s.telemetry.clone())
            .unwrap_or_default()
    }

    /// Borrowed view of this slot's telemetry, for per-frame reads that touch
    /// a field or two — cloning the whole struct there drags
    /// `per_message_counts` and the command-failure map along for every
    /// channel on every repaint. `None` for an out-of-range index.
    pub fn telemetry_ref(&self, i: usize) -> Option<&ChannelTelemetry> {
        self.slots.get(i).map(|s| &s.telemetry)
    }

    /// Newest completed run retained for this slot, across ordinary restarts.
    pub fn last_run_summary(&self, i: usize) -> Option<&RunSummary> {
        self.slots.get(i)?.last_run_summary.as_ref()
    }

    /// Start (or restart) channel `i` with an interface config and a compiled
    /// schedule. `label` is the human name for log text (frozen for the run —
    /// ADR-020; attribution itself rides the slot's stable id). Telemetry
    /// resets; the previous runner (if any) is stopped and handed to the new
    /// thread as a predecessor to join before the interface reopens.
    pub fn start(
        &mut self,
        i: usize,
        label: impl Into<String>,
        cfg: InterfaceConfig,
        messages: Vec<MessageConfig>,
        schedule: Schedule,
    ) {
        let message_count = schedule.len();
        self.begin_start(i, message_count);
        let Some(slot) = self.slots.get_mut(i) else {
            return;
        };
        let label = label.into();
        let cadence_alignment = schedule.cadence_alignment();
        slot.label = Some(label.clone());
        slot.pending_start = Some(AppliedRunConfig {
            interface: cfg.clone(),
            cadence_alignment,
            messages,
        });
        let who = RunnerIdentity {
            id: slot.id,
            label,
            run_id: RunId::mint(),
        };
        let mut predecessors = Vec::new();
        for predecessor in std::mem::take(&mut slot.draining) {
            predecessors.push(predecessor.thread);
            slot.retired_control.push(predecessor.control_rx);
            // Its cumulative counters describe the old run and must not land
            // in telemetry that `begin_start` reset for the replacement.
            drop(predecessor.status_rx);
        }
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(CMD_QUEUE_CAP);
        let (control_tx, control_rx) = crossbeam_channel::bounded(CONTROL_QUEUE_CAP);
        let (status_tx, status_rx) = crossbeam_channel::bounded(STATUS_QUEUE_CAP);
        let notify: Option<runner::StatusNotify> = self.notify.clone().map(|n| {
            let f: runner::StatusNotify = Box::new(move || n());
            f
        });
        let policy = self.policy;
        tracing::info!(
            channel = who.id.as_u64(),
            "channel {} starting ({message_count}-message schedule)",
            who.label
        );
        let thread = std::thread::spawn(move || {
            for pred in predecessors {
                let _ = pred.join();
            }
            let observer = runner::RunnerObserver::new(status_tx, policy).with_control(control_tx);
            let observer = match notify {
                Some(notify) => observer.with_notify(notify),
                None => observer,
            };
            runner::open_and_run(who, cfg, schedule, cmd_rx, observer);
        });
        self.slots[i].handle = Some(TalkerHandle {
            cmd_tx,
            control_rx,
            status_rx,
            thread,
        });
    }

    /// Shared start prologue: stop any current runner and zero the telemetry
    /// (per-message counts sized to the new schedule).
    fn begin_start(&mut self, i: usize, message_count: usize) {
        self.stop(i);
        if let Some(slot) = self.slots.get_mut(i) {
            slot.telemetry = ChannelTelemetry {
                per_message_counts: vec![0; message_count].into(),
                ..ChannelTelemetry::default()
            };
            slot.applied_run = None;
            slot.pending_start = None;
            slot.pending_commands.clear();
        }
    }

    /// Stop channel `i` without blocking: send `Stop` and let the runner
    /// drain in the background (reaped by `poll`; predecessors joined by the
    /// next start of this slot). A runner that already exited makes the stop
    /// moot — only a full command queue is surfaced.
    pub fn stop(&mut self, i: usize) -> CommandOutcome {
        let Some(slot) = self.slots.get_mut(i) else {
            return CommandOutcome::NotRunning;
        };
        let Some(handle) = slot.handle.take() else {
            return CommandOutcome::NotRunning;
        };
        let outcome = match handle.cmd_tx.try_send(TalkerCommand::Stop) {
            Ok(()) => CommandOutcome::Enqueued,
            Err(TrySendError::Full(_)) => CommandOutcome::QueueFull,
            Err(TrySendError::Disconnected(_)) => CommandOutcome::NotRunning,
        };
        // Keep the receiver: the final Counters (ADR-018) still lands.
        slot.draining.push(DrainingRunner {
            thread: handle.thread,
            control_rx: handle.control_rx,
            status_rx: handle.status_rx,
        });
        fail_unfinished_commands(
            slot.id,
            &mut slot.pending_commands,
            &mut slot.telemetry,
            &mut self.command_completions,
            "the run was stopped before the command result was observed",
        );
        slot.pending_start = None;
        slot.applied_run = None;
        if outcome == CommandOutcome::QueueFull {
            self.record_enqueue_failure(
                i,
                CommandId::mint(),
                CommandTarget::Stop,
                "Stop",
                CommandOutcome::QueueFull,
            );
        }
        if let Some(slot) = self.slots.get(i) {
            tracing::info!(
                channel = slot.id.as_u64(),
                "channel {} stopping",
                slot.display_label()
            );
        }
        outcome
    }

    pub fn stop_all(&mut self) {
        for i in 0..self.slots.len() {
            let _ = self.stop(i);
        }
    }

    /// Reopen channel `i`'s interface with a new configuration, live.
    pub fn update_interface(&mut self, i: usize, cfg: InterfaceConfig) -> CommandSubmission {
        let id = CommandId::mint();
        self.send_command(
            i,
            id,
            CommandEffect::Interface(cfg.clone()),
            TalkerCommand::UpdateInterface { id, config: cfg },
            "the interface update",
        )
    }

    /// Change message `index`'s send interval on channel `i`, live.
    pub fn set_interval(&mut self, i: usize, index: usize, interval_ms: u64) -> CommandSubmission {
        let id = CommandId::mint();
        self.send_command(
            i,
            id,
            CommandEffect::MessageInterval { index, interval_ms },
            TalkerCommand::SetInterval {
                id,
                index,
                interval_ms,
            },
            "the interval change",
        )
    }

    fn send_command(
        &mut self,
        i: usize,
        id: CommandId,
        effect: CommandEffect,
        cmd: TalkerCommand,
        what: &str,
    ) -> CommandSubmission {
        let target = effect.target();
        let outcome = match self.slots.get(i).and_then(|s| s.handle.as_ref()) {
            Some(h) => match h.cmd_tx.try_send(cmd) {
                Ok(()) => CommandOutcome::Enqueued,
                Err(TrySendError::Full(_)) => CommandOutcome::QueueFull,
                Err(TrySendError::Disconnected(_)) => CommandOutcome::NotRunning,
            },
            None => CommandOutcome::NotRunning,
        };
        match outcome {
            CommandOutcome::Enqueued => {
                if let Some(slot) = self.slots.get_mut(i) {
                    slot.pending_commands.insert(id, PendingCommand { effect });
                }
            }
            _ => self.record_enqueue_failure(i, id, target, what, outcome),
        }
        CommandSubmission { id, outcome }
    }

    /// A command that could not be enqueued makes the on-screen state diverge from the
    /// runner's — to the user it looks like a no-op bug, so it lands in the
    /// channel's error telemetry, not just the log.
    fn record_enqueue_failure(
        &mut self,
        i: usize,
        id: CommandId,
        target: CommandTarget,
        what: &str,
        outcome: CommandOutcome,
    ) {
        // This reaches the reader twice — the log and the channel's own error
        // banner — so it is written for someone who has just clicked something
        // and seen nothing happen. "Enqueued" and "runner" describe how the
        // request travels, which is not what they are asking.
        let why = match outcome {
            CommandOutcome::QueueFull => {
                "the channel is not accepting commands — it may be stuck in a send that has not finished"
            }
            _ => "the channel has already stopped",
        };
        let msg = format!("{what} was not carried out: {why}");
        if let Some(slot) = self.slots.get(i) {
            tracing::warn!(
                channel = slot.id.as_u64(),
                "channel {}: {msg}",
                slot.display_label()
            );
        }
        if let Some(slot) = self.slots.get_mut(i) {
            slot.telemetry.record_command_failure(id, target, msg);
        }
    }

    /// Join every runner thread — running, draining, and orphaned.
    /// **Blocks**, bounded by the interfaces' send timeouts; exit path only,
    /// so serial ports and sockets close cleanly before the process dies
    /// instead of being killed mid-write.
    ///
    /// Safe on live handles: the command sender is dropped *before* the join,
    /// so a runner that never received Stop still exits on the disconnect
    /// (joining with the sender alive would deadlock — the runner would keep
    /// waiting for commands forever). Status receivers are drained first so a
    /// runner block-sending its final `Counters` can always complete.
    pub fn join_all(&mut self) {
        for slot in &mut self.slots {
            if let Some(h) = slot.handle.take() {
                let TalkerHandle {
                    cmd_tx,
                    control_rx,
                    status_rx,
                    thread,
                } = h;
                drop(cmd_tx);
                for _ in control_rx.try_iter() {}
                for _ in status_rx.try_iter() {}
                let _ = thread.join();
            }
            for d in slot.draining.drain(..) {
                for _ in d.control_rx.try_iter() {}
                for _ in d.status_rx.try_iter() {}
                let _ = d.thread.join();
            }
            slot.retired_control.clear();
        }
        for d in self.orphans.drain(..) {
            for _ in d.control_rx.try_iter() {}
            for _ in d.status_rx.try_iter() {}
            let _ = d.thread.join();
        }
    }

    /// Drain every runner's status queue into the telemetry, reap finished
    /// threads (running, draining, and orphaned), and return the payload
    /// samples for the display pane. Non-blocking; call at the UI cadence.
    pub fn poll(&mut self) -> Vec<PayloadSample> {
        let mut samples = Vec::new();
        let mut command_completions = Vec::new();
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if let Some(h) = &slot.handle {
                // Sample occupancy *before* draining so the peak reflects the
                // backlog as it stood, then drain. Finished check comes
                // before the drain: if the thread was already done, the drain
                // below is guaranteed complete (the sender is gone).
                let finished = h.thread.is_finished();
                let qlen = h.status_rx.len();
                slot.telemetry.queue_len = qlen;
                slot.telemetry.queue_peak = slot.telemetry.queue_peak.max(qlen);
                let label = slot.display_label();
                drain_control_statuses(
                    &h.control_rx,
                    slot.id,
                    &label,
                    ControlDrainState {
                        applied_run: &mut slot.applied_run,
                        pending_start: &mut slot.pending_start,
                        pending_commands: &mut slot.pending_commands,
                        last_run_summary: &mut slot.last_run_summary,
                        telemetry: &mut slot.telemetry,
                        faults: &mut slot.faults,
                    },
                    &mut command_completions,
                );
                drain_statuses(i, &h.status_rx, &mut slot.telemetry, &mut samples);
                if finished {
                    fail_unfinished_commands(
                        slot.id,
                        &mut slot.pending_commands,
                        &mut slot.telemetry,
                        &mut command_completions,
                        "the channel stopped before carrying this out",
                    );
                    slot.pending_start = None;
                    slot.handle = None; // runner exited on its own (open failed / disconnect)
                }
            }
            // Draining runners: keep collecting their tail (the final
            // Counters) until the thread exits. Finished check BEFORE the
            // drain — finished first guarantees the queue already holds
            // everything the runner ever sent, so nothing is lost when the
            // entry is dropped.
            let slot_id = slot.id;
            let last_run_summary = &mut slot.last_run_summary;
            let telemetry = &mut slot.telemetry;
            let misrouted = &mut slot.faults.summary_misrouted;
            slot.draining.retain_mut(|d| {
                let finished = d.thread.is_finished();
                // Results from a stopped predecessor no longer describe a live
                // interface. Retain only its self-contained completion summary;
                // drain the other results to keep the reliable lane unblocked.
                drain_finished_summaries(&d.control_rx, slot_id, last_run_summary, misrouted);
                drain_statuses(i, &d.status_rx, telemetry, &mut samples);
                !finished
            });
            slot.retired_control.retain_mut(|control_rx| {
                !drain_finished_summaries(control_rx, slot_id, last_run_summary, misrouted)
            });
        }
        self.orphans.retain_mut(|d| {
            for _ in d.control_rx.try_iter() {}
            // Discard status too (an orphan's slot is gone, so its telemetry
            // has no home). Draining is still required: the runner's final
            // Counters and run-summary sends can block on full queues, and a
            // never-drained orphan would wedge that thread — holding its
            // serial port / socket until exit (`join_all` is the only other
            // place that would unblock it).
            for _ in d.status_rx.try_iter() {}
            !d.thread.is_finished()
        });
        self.command_completions.extend(command_completions);
        samples
    }
}

struct ControlDrainState<'a> {
    applied_run: &'a mut Option<AppliedRunConfig>,
    pending_start: &'a mut Option<AppliedRunConfig>,
    pending_commands: &'a mut BTreeMap<CommandId, PendingCommand>,
    last_run_summary: &'a mut Option<RunSummary>,
    telemetry: &'a mut ChannelTelemetry,
    faults: &'a mut SlotFaults,
}

fn drain_control_statuses(
    control_rx: &Receiver<RunnerControlStatus>,
    slot_id: ChannelId,
    label: &str,
    state: ControlDrainState<'_>,
    completions: &mut Vec<CommandCompletion>,
) {
    let ControlDrainState {
        applied_run,
        pending_start,
        pending_commands,
        last_run_summary,
        telemetry,
        faults,
    } = state;
    for status in control_rx.try_iter() {
        match status {
            RunnerControlStatus::InterfaceOpened { channel, config } => {
                if channel != slot_id {
                    continue;
                }
                match pending_start.take() {
                    Some(run) if run.interface == config => *applied_run = Some(run),
                    Some(run) => {
                        if let Some(line) = faults.interface_mismatch.report(
                            label,
                            "the interface that opened is not the one the last start asked for",
                            "Sending is unaffected; the settings shown may not match what is \
                             running",
                        ) {
                            tracing::warn!("{line}");
                        }
                        *pending_start = Some(run);
                    }
                    None => {
                        if let Some(line) = faults.interface_unexpected.report(
                            label,
                            "an interface reported itself open with no start waiting for it",
                            "Sending is unaffected; the settings shown may not match what is \
                             running",
                        ) {
                            tracing::warn!("{line}");
                        }
                    }
                }
            }
            RunnerControlStatus::CommandCompleted {
                channel,
                id,
                target,
                execution,
            } => {
                if channel != slot_id {
                    continue;
                }
                let Some(pending) = pending_commands.remove(&id) else {
                    if let Some(line) = faults.unknown_command.report(
                        label,
                        "a result arrived for a request that is no longer being tracked",
                        "Sending is unaffected; a setting you changed may not be shown as applied",
                    ) {
                        tracing::warn!("{line}");
                    }
                    continue;
                };
                if pending.effect.target() != target {
                    if let Some(line) = faults.command_target_mismatch.report(
                        label,
                        "a result arrived naming a different setting than the one requested",
                        "Sending is unaffected; a setting you changed may not be shown as applied",
                    ) {
                        tracing::warn!("{line}");
                    }
                    continue;
                }
                match execution {
                    CommandExecution::Applied => {
                        telemetry.resolve_command(id, target);
                        if let Some(run) = applied_run.as_mut() {
                            match &pending.effect {
                                CommandEffect::Interface(config) => {
                                    run.interface = config.clone();
                                }
                                CommandEffect::MessageInterval { index, interval_ms } => {
                                    if let Some(message) = run.messages.get_mut(*index) {
                                        message.interval_ms = *interval_ms;
                                    }
                                }
                            }
                        }
                        completions.push(CommandCompletion::Applied {
                            channel,
                            id,
                            effect: pending.effect,
                        });
                    }
                    CommandExecution::Failed(message) => {
                        let message = match target {
                            CommandTarget::Interface => format!(
                                "interface update failed; the previous interface settings remain in use: {message}"
                            ),
                            CommandTarget::MessageInterval(index) => {
                                format!("message {index} interval update failed: {message}")
                            }
                            CommandTarget::Stop => format!("stop command failed: {message}"),
                        };
                        telemetry.record_command_failure(id, target, message.clone());
                        completions.push(CommandCompletion::Failed {
                            channel,
                            id,
                            target,
                            message,
                        });
                    }
                }
            }
            RunnerControlStatus::RunFinished { channel, summary } => {
                retain_run_summary(
                    slot_id,
                    channel,
                    summary,
                    last_run_summary,
                    &mut faults.summary_misrouted,
                );
            }
        }
    }
}

/// Drain a stopped predecessor's reliable lane without allowing stale live
/// configuration results to affect its replacement. Completed-run summaries
/// remain useful and are ordered independently by [`RunId`].
fn drain_finished_summaries(
    control_rx: &Receiver<RunnerControlStatus>,
    slot_id: ChannelId,
    last_run_summary: &mut Option<RunSummary>,
    misrouted: &mut InternalFaultTally,
) -> bool {
    loop {
        match control_rx.try_recv() {
            Ok(RunnerControlStatus::RunFinished { channel, summary }) => {
                retain_run_summary(slot_id, channel, summary, last_run_summary, misrouted);
            }
            Ok(_) => {}
            Err(crossbeam_channel::TryRecvError::Empty) => return false,
            Err(crossbeam_channel::TryRecvError::Disconnected) => return true,
        }
    }
}

fn retain_run_summary(
    slot_id: ChannelId,
    reported_channel: ChannelId,
    summary: Box<RunSummary>,
    retained: &mut Option<RunSummary>,
    misrouted: &mut InternalFaultTally,
) {
    if reported_channel != slot_id || summary.channel != slot_id {
        if let Some(line) = misrouted.report(
            slot_id,
            "a finished run reported itself against a different channel, so its summary was \
             discarded",
            "Sending is unaffected; Last completed run may be missing or stale",
        ) {
            tracing::warn!("{line}");
        }
        return;
    }
    if retained
        .as_ref()
        .is_none_or(|current| summary.run_id > current.run_id)
    {
        *retained = Some(*summary);
    }
}

fn fail_unfinished_commands(
    channel: ChannelId,
    pending_commands: &mut BTreeMap<CommandId, PendingCommand>,
    telemetry: &mut ChannelTelemetry,
    completions: &mut Vec<CommandCompletion>,
    reason: &str,
) {
    for (id, pending) in std::mem::take(pending_commands) {
        let target = pending.effect.target();
        let message = match target {
            CommandTarget::Interface => format!("interface update did not complete: {reason}"),
            CommandTarget::MessageInterval(index) => {
                format!("message {index} interval update did not complete: {reason}")
            }
            CommandTarget::Stop => format!("stop command did not complete: {reason}"),
        };
        telemetry.record_command_failure(id, target, message.clone());
        completions.push(CommandCompletion::Failed {
            channel,
            id,
            target,
            message,
        });
    }
}

/// Fold one receiver's pending statuses into `telemetry`, collecting payload
/// samples tagged with the slot the receiver currently occupies. Shared by
/// the live and draining paths.
fn drain_statuses(
    slot: usize,
    status_rx: &Receiver<TalkerStatus>,
    telemetry: &mut ChannelTelemetry,
    samples: &mut Vec<PayloadSample>,
) {
    for status in status_rx.try_iter() {
        match status {
            TalkerStatus::Counters {
                total_count,
                total_bytes,
                per_message_counts,
                per_message_timing,
                dropped_statuses,
                missed_sends,
                failed_sends,
                suppressed_sends,
                timing,
                captured_at,
                final_snapshot,
                timer,
                ..
            } => {
                telemetry.total_count = total_count;
                telemetry.total_bytes = total_bytes;
                telemetry.per_message_counts = per_message_counts.into();
                telemetry.per_message_timing = per_message_timing.into();
                telemetry.dropped_statuses = dropped_statuses;
                telemetry.missed_sends = missed_sends;
                telemetry.failed_sends = failed_sends;
                telemetry.suppressed_sends = suppressed_sends;
                telemetry.timing = timing.cumulative;
                telemetry.recent_timing = timing.recent;
                telemetry.recent_timing_captured_at = Some(captured_at);
                telemetry.recent_timing_is_final = final_snapshot;
                telemetry.timer = timer;
            }
            TalkerStatus::TimerStatus { status, .. } => telemetry.timer = status,
            TalkerStatus::SendSample {
                payload,
                replacement_wire_offsets,
                ..
            } => {
                // Live proof of a working interface: clear the banner. During
                // a failing episode sends are suppressed, so no samples
                // arrive and the banner correctly persists.
                telemetry.last_error = None;
                samples.push(PayloadSample {
                    slot,
                    payload,
                    replacement_wire_offsets,
                });
            }
            TalkerStatus::ConnectionError { message, .. }
            | TalkerStatus::OpenFailed { message, .. } => {
                telemetry.errors_total += 1;
                telemetry.last_error = Some(message);
            }
            TalkerStatus::SendRecovered { .. } => {
                telemetry.last_error = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::UdpSocket;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use super::*;
    use crate::core::channel::{Interface, TcpClientConfig, UdpConfig};
    use crate::core::message::{MessageConfig, PayloadConfig};

    /// Start slot `i` on a caller-supplied interface (no real I/O), through
    /// the same prologue/spawn shape as [`TalkerSupervisor::start`].
    fn start_with_interface(
        sup: &mut TalkerSupervisor,
        i: usize,
        interface: Box<dyn Interface>,
        schedule: Schedule,
    ) {
        sup.begin_start(i, schedule.len());
        let slot = sup.slots.get_mut(i).expect("slot exists");
        let who = RunnerIdentity {
            id: slot.id,
            label: format!("{}", i + 1),
            run_id: RunId::mint(),
        };
        let mut predecessors = Vec::new();
        for predecessor in std::mem::take(&mut slot.draining) {
            predecessors.push(predecessor.thread);
            slot.retired_control.push(predecessor.control_rx);
            drop(predecessor.status_rx);
        }
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(CMD_QUEUE_CAP);
        let (control_tx, control_rx) = crossbeam_channel::bounded(CONTROL_QUEUE_CAP);
        let (status_tx, status_rx) = crossbeam_channel::bounded(STATUS_QUEUE_CAP);
        let policy = sup.policy;
        let thread = std::thread::spawn(move || {
            for pred in predecessors {
                let _ = pred.join();
            }
            runner::run(
                who,
                interface,
                None,
                schedule,
                cmd_rx,
                runner::RunnerObserver::new(status_tx, policy).with_control(control_tx),
            );
        });
        sup.slots[i].handle = Some(TalkerHandle {
            cmd_tx,
            control_rx,
            status_rx,
            thread,
        });
    }

    struct CountingInterface {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Interface for CountingInterface {
        fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    fn msg(hex: &str, interval_ms: u64) -> MessageConfig {
        MessageConfig::new(PayloadConfig::raw_hex(hex), interval_ms)
    }

    fn schedule(messages: &[MessageConfig]) -> Schedule {
        Schedule::compile(messages, Instant::now()).unwrap()
    }

    fn poll_until(
        sup: &mut TalkerSupervisor,
        samples: &mut Vec<PayloadSample>,
        mut done: impl FnMut(&TalkerSupervisor) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(sup) {
            assert!(Instant::now() < deadline, "condition not reached in 5 s");
            samples.extend(sup.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn poll_for_completion(sup: &mut TalkerSupervisor, id: CommandId) -> CommandCompletion {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < deadline,
                "command {id:?} did not complete in 5 s"
            );
            let _ = sup.poll();
            if let Some(completion) =
                sup.take_command_completions()
                    .into_iter()
                    .find(|completion| match completion {
                        CommandCompletion::Applied { id: completed, .. }
                        | CommandCompletion::Failed { id: completed, .. } => *completed == id,
                    })
            {
                return completion;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn start_poll_stop_reaps_and_reads_exact_totals_at_rest() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::every_send());
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 5)]),
        );
        assert!(sup.is_running(0));

        // Wait for a few sends to land in the telemetry.
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| s.telemetry(0).total_count >= 3);
        assert!(
            !sup.telemetry(0).recent_timing_is_final,
            "live periodic telemetry must not claim final provenance"
        );

        assert_eq!(sup.stop(0), CommandOutcome::Enqueued);
        assert!(!sup.is_running(0));

        // The draining runner's tail (final Counters) is still collected, so
        // once fully reaped the telemetry equals the wire exactly.
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
        let telemetry = sup.telemetry(0);
        let wire = sent.lock().unwrap().len() as u64;
        assert_eq!(telemetry.total_count, wire, "totals exact at rest");
        assert!(
            telemetry.recent_timing_is_final,
            "the reaped exact-at-rest snapshot retains final provenance"
        );
        let summary = sup.last_run_summary(0).expect("completed run retained");
        assert_eq!(summary.total_count, wire, "summary is exact at rest");
        assert_eq!(summary.total_bytes, telemetry.total_bytes);
        assert_eq!(
            summary.per_message_counts.as_slice(),
            &*telemetry.per_message_counts
        );
        assert_eq!(
            summary.end_reason,
            crate::core::run_summary::RunEndReason::StopCommand
        );
        // every_send policy: one sample per send reached the display lane.
        assert_eq!(samples.len() as u64, wire);
        assert!(samples.iter().all(|s| s.slot == 0));
    }

    /// ADR-054's central claim, at a real call site rather than in prose: an
    /// internal fault must not carry the structured `channel` field, because
    /// the GUI log layer turns that field into a warning badge on the reader's
    /// channel row. A bug in our own bookkeeping summoning someone to their
    /// serial link is the failure this forbids, and nothing in the type system
    /// prevents a future edit from re-adding the field.
    #[test]
    fn an_internal_fault_does_not_claim_to_be_about_the_channel() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        let slot = ChannelId::mint();
        let mut retained = None;
        let mut misrouted = InternalFaultTally::default();

        crate::core::logging::with_gui_test_subscriber(tx, || {
            retain_run_summary(
                slot,
                ChannelId::mint(),
                Box::new(misrouted_summary()),
                &mut retained,
                &mut misrouted,
            );
        });

        let event = rx.try_recv().expect("a misrouted summary is reported");
        assert_eq!(
            event.channel, None,
            "an internal fault must not raise a badge on a channel row"
        );
        assert!(
            event.message.contains(&slot.to_string()),
            "the channel belongs in the text instead: {}",
            event.message
        );
        assert!(
            event.message.starts_with("internal fault on channel"),
            "and it must say it is internal: {}",
            event.message
        );
    }

    /// A summary that names a channel other than the slot it arrived on.
    fn misrouted_summary() -> RunSummary {
        RunSummary {
            run_id: RunId::mint(),
            channel: ChannelId::mint(),
            label: "test".to_owned(),
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: SystemTime::UNIX_EPOCH,
            elapsed: Duration::ZERO,
            end_reason: crate::core::run_summary::RunEndReason::StopCommand,
            total_count: 0,
            total_bytes: 0,
            per_message_counts: Vec::new(),
            per_message_timing: Vec::new(),
            dropped_statuses: 0,
            missed_sends: 0,
            failed_sends: 0,
            suppressed_sends: 0,
            timing: Default::default(),
            timer: Default::default(),
        }
    }

    #[test]
    fn newest_run_summary_wins_regardless_of_completion_arrival_order() {
        let channel = ChannelId::mint();
        let older_id = RunId::mint();
        let newer_id = RunId::mint();
        let make_summary = |run_id, total_count| {
            Box::new(RunSummary {
                run_id,
                channel,
                label: "test".to_owned(),
                started_at: SystemTime::UNIX_EPOCH,
                finished_at: SystemTime::UNIX_EPOCH,
                elapsed: Duration::ZERO,
                end_reason: crate::core::run_summary::RunEndReason::StopCommand,
                total_count,
                total_bytes: total_count,
                per_message_counts: vec![total_count],
                per_message_timing: vec![MessageTiming::default()],
                dropped_statuses: 0,
                missed_sends: 0,
                failed_sends: 0,
                suppressed_sends: 0,
                timing: Default::default(),
                timer: Default::default(),
            })
        };
        let mut retained = None;
        let mut misrouted = InternalFaultTally::default();

        retain_run_summary(
            channel,
            channel,
            make_summary(newer_id, 20),
            &mut retained,
            &mut misrouted,
        );
        retain_run_summary(
            channel,
            channel,
            make_summary(older_id, 10),
            &mut retained,
            &mut misrouted,
        );

        assert_eq!(
            misrouted,
            InternalFaultTally::default(),
            "correctly routed summaries are not an internal fault"
        );

        // A summary naming a different channel is discarded and counted, and
        // leaves what was already retained alone.
        retain_run_summary(
            channel,
            ChannelId::mint(),
            make_summary(RunId::mint(), 99),
            &mut retained,
            &mut misrouted,
        );
        assert_ne!(
            misrouted,
            InternalFaultTally::default(),
            "a misrouted summary is an internal fault and must be counted"
        );

        let retained = retained.expect("newest summary retained");
        assert_eq!(retained.run_id, newer_id);
        assert_eq!(
            retained.total_count, 20,
            "the misrouted summary must not displace the slot's own"
        );
    }

    #[test]
    fn removal_with_a_saturated_status_queue_still_reaps_the_orphan() {
        // Regression: the runner's final Counters send blocks on a full
        // status queue (deliberate — "exact at rest"), relying on the owner
        // draining the receiver until the thread exits. Orphans' status lanes
        // were never drained in `poll`, so removing a channel whose queue
        // filled while the UI wasn't polling wedged the runner forever and
        // leaked its interface until process exit.
        let mut sup = TalkerSupervisor::new(ObserverPolicy::every_send());
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 1)]),
        );
        // No polls while the 1 ms every-send cadence overfills the
        // STATUS_QUEUE_CAP lane (256 + margin sends).
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.lock().unwrap().len() <= STATUS_QUEUE_CAP + 16 {
            assert!(Instant::now() < deadline, "interface never saturated");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Remove: stops the runner (its exit path now hits the blocking
        // final send against a full queue) and orphans the thread.
        sup.remove_slot(0);
        assert_eq!(sup.len(), 0);
        // Poll must unblock and reap the orphan — before the fix this timed
        // out with the orphan (and its interface) held forever.
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }

    #[test]
    fn telemetry_ref_matches_telemetry_and_is_none_out_of_range() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        // Out of range: the clone accessor zeroes, the borrow accessor is None.
        assert_eq!(sup.telemetry(7).total_count, 0);
        assert!(sup.telemetry_ref(7).is_none());
        // In range, both views read the same slot state.
        assert_eq!(
            sup.set_interval(0, 0, 50).outcome,
            CommandOutcome::NotRunning
        );
        let owned = sup.telemetry(0);
        let borrowed = sup.telemetry_ref(0).expect("slot 0 exists");
        assert_eq!(borrowed.errors_total, owned.errors_total);
        assert_eq!(borrowed.banner_error(), owned.banner_error());
    }

    #[test]
    fn commands_on_a_stopped_slot_report_not_running_and_surface_it() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        assert_eq!(sup.stop(0), CommandOutcome::NotRunning);
        // A moot Stop is silent…
        assert!(sup.telemetry(0).last_error.is_none());
        // …but a lost interval change is surfaced (§ the on-screen state
        // would silently diverge otherwise).
        assert_eq!(
            sup.set_interval(0, 0, 50).outcome,
            CommandOutcome::NotRunning
        );
        let t = sup.telemetry(0);
        assert_eq!(t.errors_total, 1);
        assert!(t
            .command_error
            .as_deref()
            .unwrap()
            .contains("interval change"));
        assert!(t.banner_error().is_some());
    }

    #[test]
    fn interface_execution_result_controls_applied_state_and_scoped_error() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 5)]),
        );

        // The injected test interface stands in for this known applied config.
        let original = InterfaceConfig::Udp(UdpConfig::unicast(
            "127.0.0.1:9".parse().expect("socket address"),
        ));
        sup.slots[0].applied_run = Some(AppliedRunConfig {
            interface: original.clone(),
            cadence_alignment: CadenceAlignment::Immediate,
            messages: vec![msg("AB", 20)],
        });

        let refused = InterfaceConfig::TcpClient(TcpClientConfig::new(
            "127.0.0.1:1".parse().expect("socket address"),
        ));
        let failed = sup.update_interface(0, refused);
        assert_eq!(failed.outcome, CommandOutcome::Enqueued);
        assert_eq!(
            sup.applied_interface(0),
            Some(&original),
            "enqueue alone must not change runtime truth"
        );
        assert!(matches!(
            poll_for_completion(&mut sup, failed.id),
            CommandCompletion::Failed {
                target: CommandTarget::Interface,
                ..
            }
        ));
        assert_eq!(
            sup.applied_interface(0),
            Some(&original),
            "failed reopen keeps the old working interface"
        );
        assert!(sup.telemetry(0).command_error.is_some());

        // A healthy old interface and an unrelated successful interval command
        // say nothing about the failed reopen; neither may clear its banner.
        let interval = sup.set_interval(0, 0, 35);
        assert_eq!(interval.outcome, CommandOutcome::Enqueued);
        assert!(matches!(
            poll_for_completion(&mut sup, interval.id),
            CommandCompletion::Applied {
                effect: CommandEffect::MessageInterval {
                    index: 0,
                    interval_ms: 35
                },
                ..
            }
        ));
        assert_eq!(
            sup.applied_run_config(0).unwrap().messages[0].interval_ms,
            35
        );
        assert!(sup.telemetry(0).command_error.is_some());

        // Only a later success for the same target resolves the divergence.
        let sink = UdpSocket::bind("127.0.0.1:0").expect("bind UDP sink");
        let replacement =
            InterfaceConfig::Udp(UdpConfig::unicast(sink.local_addr().expect("sink address")));
        let succeeded = sup.update_interface(0, replacement.clone());
        assert_eq!(succeeded.outcome, CommandOutcome::Enqueued);
        assert!(matches!(
            poll_for_completion(&mut sup, succeeded.id),
            CommandCompletion::Applied {
                effect: CommandEffect::Interface(_),
                ..
            }
        ));
        assert_eq!(sup.applied_interface(0), Some(&replacement));
        assert!(sup.telemetry(0).command_error.is_none());

        let _ = sup.stop(0);
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }

    #[test]
    fn start_time_interface_becomes_applied_only_after_open_succeeds() {
        let sink = UdpSocket::bind("127.0.0.1:0").expect("bind UDP sink");
        let config =
            InterfaceConfig::Udp(UdpConfig::unicast(sink.local_addr().expect("sink address")));
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        let messages = vec![msg("AB", 20)];
        sup.start(
            0,
            "1",
            config.clone(),
            messages.clone(),
            schedule(&messages),
        );
        assert!(
            sup.applied_interface(0).is_none(),
            "spawn is not proof that open completed"
        );
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| {
            s.applied_interface(0) == Some(&config)
        });
        assert_eq!(sup.applied_run_config(0).unwrap().messages, messages);
        let _ = sup.stop(0);
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }

    #[test]
    fn accepted_command_fails_when_runner_exits_before_execution() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        sup.begin_start(0, 1);
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(CMD_QUEUE_CAP);
        let (control_tx, control_rx) = crossbeam_channel::bounded(CONTROL_QUEUE_CAP);
        let (status_tx, status_rx) = crossbeam_channel::bounded(STATUS_QUEUE_CAP);
        let (release_tx, release_rx) = crossbeam_channel::bounded(0);
        let thread = std::thread::spawn(move || {
            release_rx.recv().unwrap();
            drop(cmd_rx);
            drop(control_tx);
            drop(status_tx);
        });
        sup.slots[0].handle = Some(TalkerHandle {
            cmd_tx,
            control_rx,
            status_rx,
            thread,
        });

        let submission = sup.set_interval(0, 0, 10);
        assert_eq!(submission.outcome, CommandOutcome::Enqueued);
        release_tx.send(()).unwrap();
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| !s.is_running(0));

        assert!(sup.slots[0].pending_commands.is_empty());
        assert!(matches!(
            sup.take_command_completions().as_slice(),
            [CommandCompletion::Failed {
                id,
                target: CommandTarget::MessageInterval(0),
                message,
                ..
            }] if *id == submission.id && message.contains("stopped")
        ));
        assert!(sup.telemetry(0).command_error.is_some());
    }

    #[test]
    fn stop_resolves_pending_commands_and_clears_applied_run() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 20)]),
        );
        sup.slots[0].applied_run = Some(AppliedRunConfig {
            interface: InterfaceConfig::Udp(UdpConfig::unicast(
                "127.0.0.1:9".parse().expect("socket address"),
            )),
            cadence_alignment: CadenceAlignment::Immediate,
            messages: vec![msg("AB", 20)],
        });

        let submission = sup.set_interval(0, 0, 10);
        assert_eq!(submission.outcome, CommandOutcome::Enqueued);
        assert_eq!(sup.stop(0), CommandOutcome::Enqueued);

        assert!(sup.applied_run_config(0).is_none());
        assert!(sup.slots[0].pending_commands.is_empty());
        assert!(matches!(
            sup.take_command_completions().as_slice(),
            [CommandCompletion::Failed {
                id,
                target: CommandTarget::MessageInterval(0),
                message,
                ..
            }] if *id == submission.id && message.contains("run was stopped")
        ));

        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }

    #[test]
    fn rejected_interval_reports_execution_failure() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 20)]),
        );

        let submission = sup.set_interval(0, 7, 10);
        assert_eq!(submission.outcome, CommandOutcome::Enqueued);
        match poll_for_completion(&mut sup, submission.id) {
            CommandCompletion::Failed {
                target: CommandTarget::MessageInterval(7),
                message,
                ..
            } => assert!(message.contains("outside the 1-message schedule")),
            other => panic!("expected rejected interval completion, got {other:?}"),
        }
        assert!(sup
            .telemetry(0)
            .command_error
            .as_deref()
            .is_some_and(|message| message.contains("message 7 interval update failed")));

        let _ = sup.stop(0);
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }

    /// Error-class separation: a healthy payload sample clears an *interface*
    /// error but must NOT clear a *control-plane* one — the wire working says
    /// nothing about a command that never arrived.
    #[test]
    fn samples_clear_interface_errors_but_not_command_errors() {
        let mut telemetry = ChannelTelemetry {
            last_error: Some("send failed".into()),
            command_error: Some("the interval change was not carried out".into()),
            ..ChannelTelemetry::default()
        };
        let (tx, rx) = crossbeam_channel::bounded(4);
        tx.send(TalkerStatus::SendSample {
            channel: ChannelId::mint(),
            message_index: 0,
            payload: b"??".to_vec(),
            replacement_wire_offsets: vec![1],
        })
        .unwrap();
        drop(tx);
        let mut samples = Vec::new();
        drain_statuses(0, &rx, &mut telemetry, &mut samples);
        assert!(telemetry.last_error.is_none(), "interface error cleared");
        assert!(
            telemetry.command_error.is_some(),
            "control-plane error survives a healthy sample"
        );
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].payload, b"??");
        assert_eq!(samples[0].replacement_wire_offsets, vec![1]);
    }

    #[test]
    fn restart_resets_telemetry_and_joins_predecessors() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::every_send());
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 5)]),
        );
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| s.telemetry(0).total_count >= 2);

        // Restart without an explicit stop: start() stops the old runner and
        // the new thread joins it before running.
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::new(Mutex::new(Vec::new())),
            }),
            schedule(&[msg("CD", 5), msg("EF", 5)]),
        );
        // Telemetry was reset for the new two-message schedule.
        assert_eq!(sup.telemetry(0).total_count, 0);
        assert_eq!(sup.telemetry(0).per_message_counts.len(), 2);

        poll_until(&mut sup, &mut samples, |s| s.telemetry(0).total_count >= 2);
        let predecessor = sup
            .last_run_summary(0)
            .expect("restart retained the completed predecessor");
        assert!(predecessor.total_count >= 2);
        assert_eq!(
            predecessor.total_count,
            sent.lock().unwrap().len() as u64,
            "predecessor summary remains exact without replacing new telemetry"
        );
        sup.stop_all();
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }

    #[test]
    fn channel_ids_are_stable_across_slot_removal() {
        // The whole point of ADR-020: positions shift, identity doesn't. A
        // running runner keeps stamping the id its slot was minted with, so
        // log-count attribution keyed by id can never land on the wrong row.
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        sup.push_slot();
        let first = sup.channel_id(0).unwrap();
        let second = sup.channel_id(1).unwrap();
        assert_ne!(first, second);

        sup.remove_slot(0);
        assert_eq!(
            sup.channel_id(0),
            Some(second),
            "the surviving slot keeps its id after shifting down"
        );

        // A fresh slot mints a fresh id — removed ids are never reused.
        sup.push_slot();
        let third = sup.channel_id(1).unwrap();
        assert_ne!(third, first);
        assert_ne!(third, second);
    }

    #[test]
    fn remove_slot_shifts_telemetry_and_orphans_the_runner() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::every_send());
        sup.push_slot();
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            1,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 5)]),
        );
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| s.telemetry(1).total_count >= 1);

        sup.remove_slot(0);
        assert_eq!(sup.len(), 1);
        // Slot 1's telemetry shifted down to index 0.
        assert!(sup.telemetry(0).total_count >= 1);
        // Removing the (shifted) running slot orphans its runner; poll reaps.
        sup.remove_slot(0);
        assert_eq!(sup.len(), 0);
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }
}
