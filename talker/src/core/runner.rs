//! The per-channel send loop shared by the CLI and the GUI.
//!
//! The loop is business logic, so it lives in core (spec §2.2) — `cli` and
//! `gui` only wire channels to it. One [`run`] call owns one interface and
//! its schedule and executes until a [`TalkerCommand::Stop`] arrives or every
//! command sender is dropped.
//!
//! Waiting uses deadline-bounded blocking receives on the command channel
//! (ADR-002): a due message fires on time (no sleep-slice jitter), a command
//! is handled the moment it arrives, and an idle channel consumes no CPU.

use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};

use crate::core::{
    channel::{ChannelId, Interface, InterfaceConfig, MissingRetryConfiguration, PartialWrite},
    internal_fault::InternalFaultTally,
    run_summary::{RunEndReason, RunId, RunSummary},
    scheduler::{Schedule, Tick},
    telemetry::{MessageTiming, MessageTimingRecorder, SendTimingRecorder, SendTimingReport},
    timing,
};

/// Who a runner is, fixed at start (ADR-020): the stable [`ChannelId`] every
/// status and structured log field carries (attribution that survives slot
/// shifts), and the human label used in log *text* ("channel 3 …",
/// "channel 'GPS' …"). The label is frozen for the run — a rename shows up
/// on the next start; the id is what routing trusts.
#[derive(Clone, Debug)]
pub struct RunnerIdentity {
    pub id: ChannelId,
    pub label: String,
    pub run_id: RunId,
}

/// Process-unique identity of a live control command. The id lets observers
/// correlate an enqueue attempt with the runner's eventual execution result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(u64);

impl CommandId {
    pub fn mint() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The independently recoverable control target a command mutates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CommandTarget {
    Stop,
    Interface,
    MessageInterval(usize),
}

/// What the runner did with an enqueued command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandExecution {
    Applied,
    Failed(String),
}

/// Reliable runner-to-owner control state. This uses a dedicated bounded lane,
/// separate from drop-and-count telemetry: configuration truth must not disappear
/// merely because the sampled observer queue is full.
#[derive(Clone, Debug)]
pub enum RunnerControlStatus {
    /// The runner successfully opened its start-time interface.
    InterfaceOpened {
        channel: ChannelId,
        config: InterfaceConfig,
    },
    /// One enqueued live mutation finished executing.
    CommandCompleted {
        channel: ChannelId,
        id: CommandId,
        target: CommandTarget,
        execution: CommandExecution,
    },
    /// Exact, self-contained facts retained after one send loop completes.
    RunFinished {
        channel: ChannelId,
        summary: Box<RunSummary>,
    },
}

/// A command sent from the owning thread (UI or CLI) to a channel's runner.
pub enum TalkerCommand {
    Stop,
    /// Reopen the channel's interface with a new configuration.
    UpdateInterface {
        id: CommandId,
        config: InterfaceConfig,
    },
    /// Change message `index`'s send interval, effective immediately.
    SetInterval {
        id: CommandId,
        index: usize,
        interval_ms: u64,
    },
}

/// A status update from a channel's runner.
///
/// Every variant names its channel by stable [`ChannelId`] (ADR-020), so
/// statuses stay self-describing even when several channels share one status
/// receiver — the CLI funnels all channels into a single channel; the
/// supervisor keeps one receiver per slot and routes by slot. A slot index
/// would go stale the moment a channel above is removed; the id never does.
pub enum TalkerStatus {
    /// Periodic counters (ADR-018 lane 1): cumulative totals, **no message
    /// payload**.
    /// Emitted at most once per [`ObserverPolicy::counter_interval`] on the
    /// send path, plus once when the runner stops (so totals are exact at
    /// rest). Cumulative totals and the current failure-episode state repeat,
    /// so the reader self-corrects even when some updates were dropped by a
    /// full queue.
    Counters {
        channel: ChannelId,
        /// Running send count across all messages in this channel: complete
        /// messages only.
        total_count: u64,
        /// Cumulative wire bytes the interface accepted across all messages in
        /// this channel, counted apart from complete messages: it includes the
        /// accepted part of each possibly-partial send (§4.4).
        total_bytes: u64,
        /// Per-message running send counts, indexed by the message's
        /// position in the compiled schedule.
        per_message_counts: Vec<u64>,
        /// Per-message cumulative timing on the same index basis, including
        /// the delay each message's sends imposed on the others (ADR-045).
        per_message_timing: Vec<MessageTiming>,
        /// Cumulative count of status updates this channel discarded because
        /// the receiver was full.
        dropped_statuses: u64,
        /// Cumulative count of sends skipped under the scheduler's stall
        /// policy (`Schedule::missed_sends`) — the channel couldn't keep to
        /// its configured cadence.
        missed_sends: u64,
        /// Cumulative send calls that reached the interface and failed before
        /// it accepted any byte.
        failed_sends: u64,
        /// Cumulative send calls that failed after the interface accepted part
        /// of the message (§4.4, ADR-059): the receiver may hold a fragment.
        /// Never resent, and not in `failed_sends`.
        possibly_partial_sends: u64,
        /// Bytes the TCP peer sent during this run, read and discarded before
        /// each write (ADR-059, §4.5): "peer sent N bytes". `None` for a
        /// transport that does not read its peer.
        peer_bytes: Option<u64>,
        /// Cumulative send-failure episodes. Repeated attempts inside one
        /// bounded-backoff episode do not increase this count.
        send_failure_episodes: u64,
        /// Cumulative due fires suppressed by the bounded-backoff gate.
        suppressed_sends: u64,
        /// The current write or reopen error from the open failure episode.
        ///
        /// This repeats the immediate [`ConnectionError`](Self::ConnectionError)
        /// edge so a later counter snapshot repairs observer state if that edge
        /// was dropped. Later retry errors refresh the live explanation without
        /// opening another counted episode. `None` likewise repairs a dropped
        /// [`SendRecovered`](Self::SendRecovered) edge.
        active_send_error: Option<String>,
        /// Cumulative bounded measurements of deadline handling, payload
        /// rendering, and the application-level interface send call.
        timing: Box<SendTimingReport>,
        /// Exact monotonic instant at which `timing` was collapsed. This is
        /// snapshot provenance, not necessarily the newest sample time.
        captured_at: Instant,
        /// Whether this is the mandatory exact-at-rest snapshot emitted after
        /// the send loop ends. Finality travels with the measurement rather
        /// than being inferred from observer thread lifecycle.
        final_snapshot: bool,
        /// Current platform deadline-wait policy and the shortest active
        /// interval that selected it.
        timer: timing::TimerStatus,
    },
    /// Low-frequency edge update when a schedule change selects a different
    /// timer policy. Counters repeat the same state while sends are active;
    /// this edge also keeps a newly idle channel's readout current.
    TimerStatus {
        channel: ChannelId,
        status: timing::TimerStatus,
    },
    /// A sampled send (ADR-018 lane 2): the exact wire bytes of one send,
    /// for the Output pane. Newest-per-interval — the first send after
    /// [`ObserverPolicy::sample_interval`] elapses carries its payload — so
    /// the pane shows a live, bounded sample rather than every message.
    /// `ObserverPolicy::every_send` (CLI `--echo`) makes this every send.
    SendSample {
        channel: ChannelId,
        /// Which message in the channel's schedule fired.
        message_index: usize,
        /// Exact bytes put on the wire.
        payload: Vec<u8>,
        /// Byte positions in `payload` produced by lossy code-page fallback.
        /// Literal `?` bytes are deliberately absent.
        replacement_wire_offsets: Vec<usize>,
    },
    /// A send failed. **Edge-triggered**: only
    /// the *first* failure of a failing episode is reported; repeats are
    /// counted, not re-reported, and [`SendRecovered`](Self::SendRecovered)
    /// closes the episode with the totals.
    ConnectionError { channel: ChannelId, message: String },
    /// Sending resumed after a failing episode. Carries the episode's cost so
    /// the observer can state what was lost: `failures` sends were attempted
    /// and failed (the first was reported as `ConnectionError`), `suppressed`
    /// due fires were skipped by the bounded-backoff retry policy without an
    /// interface write; some may have found that the handle could not yet be
    /// reopened.
    SendRecovered {
        channel: ChannelId,
        failures: u64,
        suppressed: u64,
    },
    /// Opening the interface failed; the runner exits after sending this.
    OpenFailed { channel: ChannelId, message: String },
}

/// First retry delay after a send failure (the **bounded-backoff** retry
/// policy): while an interface is failing, due fires are suppressed — counted,
/// with no interface write — until the next retry instant. At that instant a
/// transport may first prepare or replace its failed handle; an unavailable
/// replacement withholds that fire too. Each unsuccessful retry doubles the wait up to
/// [`RETRY_BACKOFF_MAX`], and the first successful send closes the episode.
/// Without this, a 100 Hz schedule against a dead TCP/serial target retries
/// (and used to log) 100 times a second, burying the original failure.
const RETRY_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
/// Retry delay cap: a persistently dead interface is probed at most once per
/// this interval. Retrying stays automatic — there is no manual Retry state —
/// and transports that require a new OS handle can prepare it at that probe.
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// How a runner reports to its observer (ADR-018): the named policy the owner
/// passes to [`run`]/[`open_and_run`]. Counters and payload samples are
/// **rate-limited lanes**; errors ([`TalkerStatus::ConnectionError`] /
/// [`TalkerStatus::SendRecovered`] / [`TalkerStatus::OpenFailed`]) are always
/// immediate and never rate-limited.
#[derive(Clone, Copy, Debug)]
pub struct ObserverPolicy {
    /// Minimum spacing between [`TalkerStatus::Counters`] emissions. A final
    /// one is always emitted when the runner stops.
    pub counter_interval: Duration,
    /// Minimum spacing between payload-bearing [`TalkerStatus::SendSample`]s.
    /// `Duration::ZERO` = every send carries its payload (CLI `--echo`).
    pub sample_interval: Duration,
}

impl ObserverPolicy {
    /// The GUI default: ~5 Hz counters, ~10 Hz payload samples — display cost
    /// stays constant regardless of send rate (ADR-018).
    pub fn sampled() -> Self {
        Self {
            counter_interval: Duration::from_millis(200),
            sample_interval: Duration::from_millis(100),
        }
    }

    /// Every send emits its payload (CLI `--echo` — the one consumer that
    /// genuinely wants every wire message). Counters stay periodic.
    pub fn every_send() -> Self {
        Self {
            sample_interval: Duration::ZERO,
            ..Self::sampled()
        }
    }
}

impl Default for ObserverPolicy {
    fn default() -> Self {
        Self::sampled()
    }
}

/// One failing episode: from the first failed send (reported) to the first
/// successful one (reported with these counts). See [`RETRY_BACKOFF_INITIAL`].
struct FailureEpisode {
    /// The obstacle currently preventing a successful write, repeated in
    /// counter snapshots until a successful write closes the episode. The
    /// episode's first failure remains in its edge-triggered log/status event;
    /// later retries update this live description without creating new edges.
    active_error: String,
    /// Sends attempted and failed, ≥ 1 (the reported first one), including
    /// those that were possibly partial.
    failures: u64,
    /// Of `failures`, those that failed after the interface accepted part of
    /// the message.
    possibly_partial: u64,
    /// Due fires suppressed by the backoff gate without an interface write.
    suppressed: u64,
    backoff: Duration,
    next_attempt: Instant,
    /// The last attempted handle failed and must be prepared before it is used
    /// again. A successful interface update clears this for its first send.
    handle_needs_recovery: bool,
    /// At least one preparation call replaced the OS handle during this
    /// episode. Retained across later failures so the one recovery edge can
    /// distinguish replacement from a transient write recovery.
    handle_replaced: bool,
}

/// How long a channel must go without skipping a scheduled send before its
/// off-cadence episode is closed.
///
/// Recovery cannot be "the first poll that skipped nothing". A marginal channel
/// skips intermittently, so closing on the first clean tick would log a start
/// and an end for every pair of ticks — at a 10 ms cadence, a hundred pairs a
/// second, which is the flood the edge-triggering exists to prevent. The window
/// only has to outlast the skip's cause, and the coarsest ordinary one is an OS
/// scheduling quantum, measured in tens of milliseconds. Seconds is therefore
/// the right order of magnitude, and five is a settle time chosen within it
/// rather than a threshold proven by anything.
///
/// **It is a minimum, not a deadline.** The test runs only where the skip count
/// arrives, on a reached cadence point, so a channel is not woken to announce
/// its own recovery: a 15 s schedule reports at its next send, not at 5 s. A run
/// that stops first never reports one at all, which is why the end of the run
/// states the missed total instead.
const MISS_RECOVERY_SETTLE: Duration = Duration::from_secs(5);

/// One episode of a channel failing to keep its cadence: from the first skipped
/// send (reported) to [`MISS_RECOVERY_SETTLE`] without another (reported with
/// these counts).
///
/// Edge-triggered for the reason the failure path is, only more so. Skips are
/// counted at every poll, and they concentrate on the shortest interval, so a
/// line per skipped send would emit thousands a second under exactly the
/// overload it is describing — into a log pane that is itself a queue consumer.
/// The first skip and the return to schedule are the two facts worth a line;
/// the count in between belongs on the line that closes the episode.
struct MissEpisode {
    /// Scheduled sends skipped since the episode opened, ≥ 1.
    skipped: u64,
    /// When the most recent skip was observed, for the settle test.
    last_skip: Instant,
}

/// What one poll's skip count is worth reporting, if anything.
#[derive(Debug, PartialEq, Eq)]
enum MissReport {
    /// Either nothing was skipped and nothing was open, or the episode is
    /// still running and this poll only added to it.
    Silent,
    /// The channel has just fallen off cadence. Carries this poll's own count,
    /// which is what is known at the moment it is reported.
    FellBehind(u64),
    /// The channel has held its cadence for [`MISS_RECOVERY_SETTLE`]. Carries
    /// the episode's total.
    BackOnSchedule(u64),
}

/// Fold one poll's skip count into the off-cadence episode.
///
/// Separated from the send loop so the edge behaviour can be exercised
/// directly: what has to hold is that a sustained overload logs twice and not
/// once per skipped send, and that is a statement about this transition table
/// rather than about any particular run.
fn observe_skips(episode: &mut Option<MissEpisode>, skipped: u64, now: Instant) -> MissReport {
    match (skipped > 0, episode.as_mut()) {
        (true, Some(open)) => {
            open.skipped = open.skipped.saturating_add(skipped);
            open.last_skip = now;
            MissReport::Silent
        }
        (true, None) => {
            *episode = Some(MissEpisode {
                skipped,
                last_skip: now,
            });
            MissReport::FellBehind(skipped)
        }
        (false, Some(open))
            if now.saturating_duration_since(open.last_skip) >= MISS_RECOVERY_SETTLE =>
        {
            let total = open.skipped;
            *episode = None;
            MissReport::BackOnSchedule(total)
        }
        _ => MissReport::Silent,
    }
}

/// The owning side's handle for a running talker thread.
pub struct TalkerHandle {
    pub cmd_tx: Sender<TalkerCommand>,
    pub control_rx: Receiver<RunnerControlStatus>,
    pub status_rx: Receiver<TalkerStatus>,
    pub thread: std::thread::JoinHandle<()>,
}

/// Called after each status is queued, so an event-driven owner (the GUI)
/// can wake and drain instead of polling. Kept as a plain closure — core
/// stays UI-framework-free; the GUI passes `ctx.request_repaint`.
pub type StatusNotify = Box<dyn Fn() + Send>;

/// Runner-to-owner reporting endpoints and cadence policy. Keeping this wiring
/// together prevents entry points from growing parallel positional arguments as
/// observer and reliable-control lanes evolve.
pub struct RunnerObserver {
    control_tx: Option<Sender<RunnerControlStatus>>,
    status_tx: Sender<TalkerStatus>,
    notify: Option<StatusNotify>,
    policy: ObserverPolicy,
}

impl RunnerObserver {
    pub fn new(status_tx: Sender<TalkerStatus>, policy: ObserverPolicy) -> Self {
        Self {
            control_tx: None,
            status_tx,
            notify: None,
            policy,
        }
    }

    pub fn with_control(mut self, control_tx: Sender<RunnerControlStatus>) -> Self {
        self.control_tx = Some(control_tx);
        self
    }

    pub fn with_notify(mut self, notify: StatusNotify) -> Self {
        self.notify = Some(notify);
        self
    }
}

/// How long the first retry of a failed open waits; each failure doubles the
/// wait, up to [`OPEN_RETRY_MAX`] (talker ADR-060).
pub const OPEN_RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between retries of a failed open.
pub const OPEN_RETRY_MAX: Duration = Duration::from_secs(30);

/// Open `cfg`'s interface, retrying a failed open — 1 s, doubling to 30 s —
/// until it opens or Stop arrives, then run the send loop. For the unattended
/// CLI (talker ADR-060): a channel whose port is not there yet starts once it
/// is, while the others send.
///
/// Each failed attempt is reported as [`TalkerStatus::OpenFailed`], and the
/// open as [`RunnerControlStatus::InterfaceOpened`]. Commands other than Stop
/// have nothing to act on until the channel opens, and are ignored. Call it on
/// the channel's own thread.
pub fn open_retrying_and_run(
    who: RunnerIdentity,
    cfg: InterfaceConfig,
    schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
) {
    open_retrying_and_run_with(
        who,
        cfg,
        schedule,
        cmd_rx,
        observer,
        OPEN_RETRY_FIRST,
        OPEN_RETRY_MAX,
    );
}

fn open_retrying_and_run_with(
    who: RunnerIdentity,
    cfg: InterfaceConfig,
    schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
    first: Duration,
    max: Duration,
) {
    let mut wait = first;
    let mut failures = 0u64;
    loop {
        match cfg.open() {
            Ok(interface) => {
                emit_control(
                    &observer.control_tx,
                    &observer.notify,
                    RunnerControlStatus::InterfaceOpened {
                        channel: who.id,
                        config: cfg.clone(),
                    },
                );
                run(who, interface, Some(cfg), schedule, cmd_rx, observer);
                return;
            }
            Err(e) => {
                let message = format!("{e:#}");
                failures += 1;
                // The first failure is the news; the retries that follow
                // are the caller's to summarize, not the log's to repeat.
                if failures == 1 {
                    tracing::warn!(
                        channel = who.id.as_u64(),
                        "channel {} did not open: {message}; retrying",
                        who.label
                    );
                } else {
                    tracing::debug!(
                        channel = who.id.as_u64(),
                        "channel {} still did not open (attempt {failures}): {message}",
                        who.label
                    );
                }
                if observer
                    .status_tx
                    .try_send(TalkerStatus::OpenFailed {
                        channel: who.id,
                        message,
                    })
                    .is_ok()
                {
                    if let Some(n) = &observer.notify {
                        n();
                    }
                }
                let deadline = Instant::now() + wait;
                loop {
                    match cmd_rx.recv_deadline(deadline) {
                        Ok(TalkerCommand::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                        Ok(_) => continue,
                        Err(RecvTimeoutError::Timeout) => break,
                    }
                }
                wait = (wait * 2).min(max);
            }
        }
    }
}

/// Open `cfg`'s interface, then run the send loop.
///
/// Meant to be called *on the channel's own thread* (the GUI path), so the
/// open — real I/O that can block for seconds on a TCP connect — never runs
/// on the UI thread. A failed open is reported as
/// [`TalkerStatus::OpenFailed`] and the call returns.
pub fn open_and_run(
    who: RunnerIdentity,
    cfg: InterfaceConfig,
    schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
) {
    match cfg.open() {
        Ok(interface) => {
            emit_control(
                &observer.control_tx,
                &observer.notify,
                RunnerControlStatus::InterfaceOpened {
                    channel: who.id,
                    config: cfg.clone(),
                },
            );
            run(who, interface, Some(cfg), schedule, cmd_rx, observer);
        }
        Err(e) => {
            tracing::error!(
                channel = who.id.as_u64(),
                "failed to open channel {}: {e:#}",
                who.label
            );
            if observer
                .status_tx
                .try_send(TalkerStatus::OpenFailed {
                    channel: who.id,
                    message: format!("{e:#}"),
                })
                .is_ok()
            {
                if let Some(n) = &observer.notify {
                    n();
                }
            }
        }
    }
}

/// Run one channel's send loop until [`TalkerCommand::Stop`] arrives or the
/// command channel disconnects (the owning handle was dropped).
///
/// Log text names the channel by `who.label`; the structured `channel` field
/// carries the stable id (ADR-020).
///
/// `current_config` is required when a built-in transport may need to replace
/// its failed handle or apply a live configuration update. Purpose-built
/// interfaces that can recover their existing handle may pass `None`.
pub fn run(
    who: RunnerIdentity,
    interface: Box<dyn Interface>,
    current_config: Option<InterfaceConfig>,
    schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
) {
    tracing::info!(
        channel = who.id.as_u64(),
        "channel {} running ({}-message schedule)",
        who.label,
        schedule.len()
    );
    run_loop(&who, interface, current_config, schedule, cmd_rx, observer);
    tracing::info!(channel = who.id.as_u64(), "channel {} stopped", who.label);
}

enum Flow {
    Continue,
    Stop,
}

/// Reconciles one runner's timer-resolution guard with its current schedule and
/// owns the observer state coupled to timer-policy edges. Keeping these fields
/// together makes acquire/stage/release transitions explicit and prevents a timer
/// change from drifting away from its immediate counter refresh.
struct TimerReconciler<'a> {
    who: &'a RunnerIdentity,
    status_tx: &'a Sender<TalkerStatus>,
    notify: &'a Option<StatusNotify>,
    intent: timing::TimerIntent,
    active_cadence: Option<timing::ActiveCadence>,
    cadence_alignment: timing::CadenceAlignment,
    clock_realignments: u64,
    guard: Option<timing::HighResolutionGuard>,
    current: timing::TimerStatus,
    last_counters: Option<Instant>,
    dropped_statuses: u64,
    #[cfg(test)]
    guard_acquisitions: u64,
    #[cfg(test)]
    guard_releases: u64,
}

impl<'a> TimerReconciler<'a> {
    fn new(
        who: &'a RunnerIdentity,
        status_tx: &'a Sender<TalkerStatus>,
        notify: &'a Option<StatusNotify>,
    ) -> Self {
        Self {
            who,
            status_tx,
            notify,
            intent: timing::TimerIntent::None,
            active_cadence: None,
            cadence_alignment: timing::CadenceAlignment::Immediate,
            clock_realignments: 0,
            guard: None,
            current: timing::TimerStatus::default(),
            last_counters: None,
            dropped_statuses: 0,
            #[cfg(test)]
            guard_acquisitions: 0,
            #[cfg(test)]
            guard_releases: 0,
        }
    }

    /// Apply schedule changes before polling. Entering a windowed policy from
    /// another intent releases any continuous guard immediately; an interrupted
    /// final-window wait retains its guard until the recomputed wait plan says
    /// whether the new deadline is still inside that window.
    fn reconcile_schedule(
        &mut self,
        active_cadence: Option<timing::ActiveCadence>,
        cadence_alignment: timing::CadenceAlignment,
        clock_realignments: u64,
    ) {
        let shortest_interval = active_cadence.map(|cadence| cadence.shortest);
        let next_intent = timing::timer_intent(shortest_interval);
        match next_intent {
            timing::TimerIntent::ContinuousHighRate => self.acquire(),
            timing::TimerIntent::None => self.release(),
            timing::TimerIntent::PrecisionWindow
                if self.intent != timing::TimerIntent::PrecisionWindow =>
            {
                self.release();
            }
            timing::TimerIntent::PrecisionWindow => {}
        }
        self.intent = next_intent;
        self.active_cadence = active_cadence;
        self.cadence_alignment = cadence_alignment;
        self.clock_realignments = clock_realignments;
        self.refresh_status();
    }

    /// Release a bounded-window request before rendering or sending a due item.
    fn before_due(&mut self) {
        if self.intent == timing::TimerIntent::PrecisionWindow {
            self.release();
        }
    }

    /// Select and prepare the next interruptible wait, including the guard
    /// transition at the start of a Precise deadline window.
    fn prepare_wait(&mut self, now: Instant, deadline: Instant) -> Instant {
        match timing::wait_plan(self.intent, now, deadline) {
            timing::WaitPlan::Direct(deadline) => {
                if self.intent != timing::TimerIntent::ContinuousHighRate {
                    self.release();
                }
                deadline
            }
            timing::WaitPlan::Stage(window_start) => {
                self.release();
                window_start
            }
            timing::WaitPlan::Precision(deadline) => {
                self.acquire();
                self.refresh_status();
                deadline
            }
        }
    }

    fn acquire(&mut self) {
        if self.guard.is_none() {
            self.guard = Some(timing::high_resolution());
            #[cfg(test)]
            {
                self.guard_acquisitions += 1;
            }
        }
    }

    fn release(&mut self) {
        if let Some(guard) = self.guard.take() {
            #[cfg(test)]
            {
                self.guard_releases += 1;
            }
            drop(guard);
        }
    }

    fn refresh_status(&mut self) {
        let next = timing::timer_status(timing::TimerStatusInput {
            intent: self.intent,
            active_cadence: self.active_cadence,
            guard: self.guard.as_ref(),
            previous_mode: self.current.mode,
            cadence_alignment: self.cadence_alignment,
            clock_realignments: self.clock_realignments,
        });
        if next == self.current {
            return;
        }
        if next.mode == timing::TimerMode::WindowsRequestFailed
            && self.current.mode != timing::TimerMode::WindowsRequestFailed
        {
            tracing::warn!(
                channel = self.who.id.as_u64(),
                "channel {} could not enable Windows 1 ms timer resolution; sends may start late",
                self.who.label
            );
        }
        self.current = next;
        // A due fire should carry the changed state immediately instead of
        // waiting for the ordinary counter cadence.
        self.last_counters = None;
        self.emit(TalkerStatus::TimerStatus {
            channel: self.who.id,
            status: self.current,
        });
    }

    fn counters_due(&mut self, now: Instant, interval: Duration) -> bool {
        if self.last_counters.is_some_and(|last| now - last < interval) {
            return false;
        }
        self.last_counters = Some(now);
        true
    }

    fn emit(&mut self, status: TalkerStatus) {
        emit_status(
            self.status_tx,
            self.notify,
            self.who,
            &mut self.dropped_statuses,
            status,
        );
    }

    fn status(&self) -> timing::TimerStatus {
        self.current
    }

    fn dropped_statuses(&self) -> u64 {
        self.dropped_statuses
    }

    /// End guard ownership before the final blocking observer delivery.
    fn finish(mut self) -> (timing::TimerStatus, u64) {
        self.release();
        (self.current, self.dropped_statuses)
    }
}

fn run_loop(
    who: &RunnerIdentity,
    mut interface: Box<dyn Interface>,
    mut current_config: Option<InterfaceConfig>,
    mut schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
) {
    // Cadence starts only now: any profile preflight, predecessor join, TCP
    // connect, or serial open happened before this runner boundary and must not
    // inflate missed-send telemetry or shift the first-fire grid.
    let started_at = SystemTime::now();
    let started_mono = Instant::now();
    schedule.arm_at(started_mono, started_at);
    let RunnerObserver {
        control_tx,
        status_tx,
        notify,
        policy,
    } = observer;
    let mut total_count = 0u64;
    let mut total_bytes = 0u64;
    let mut failed_sends = 0u64;
    let mut possibly_partial_sends = 0u64;
    let mut peer_bytes = 0u64;
    let mut send_failure_episodes = 0u64;
    let mut suppressed_sends = 0u64;
    let mut send_timing = SendTimingRecorder::default();
    // Lane rate limits (ADR-018): `None` = nothing emitted yet, so the first
    // send always produces both a sample and counters (instant first paint).
    let mut last_sample: Option<Instant> = None;
    // Sample rotation: the scheduler breaks grid ties by lowest index, so an
    // aligned multi-message schedule would sample message 0 forever if the
    // lane just took the first due send. Skip a repeat of the last sampled
    // message while due — but never longer than one full cycle, so a
    // single-active-message schedule still samples.
    let mut last_sampled_index: Option<usize> = None;
    let mut repeats_skipped_while_due = 0u64;
    // Per-message send counts, indexed by the message's position in the
    // compiled schedule. The resize is defensive; the schedule's size is
    // fixed at compile time.
    let mut per_message_counts: Vec<u64> = vec![0; schedule.len()];
    // Per-message timing on the same index basis, including which message's
    // sends delayed the others (ADR-045).
    let mut per_message_timing = MessageTimingRecorder::new(schedule.len());

    let handle = |cmd: TalkerCommand,
                  interface: &mut Box<dyn Interface>,
                  current_config: &mut Option<InterfaceConfig>,
                  schedule: &mut Schedule,
                  episode: &mut Option<FailureEpisode>|
     -> Flow {
        match cmd {
            TalkerCommand::Stop => Flow::Stop,
            TalkerCommand::UpdateInterface { id, config } => {
                let execution = match current_config.as_ref() {
                    Some(current) => match interface.reconfigure(current, &config) {
                        Ok(true) => CommandExecution::Applied,
                        Ok(false) => match config.open() {
                            Ok(new) => {
                                *interface = new;
                                CommandExecution::Applied
                            }
                            Err(e) => CommandExecution::Failed(format!("{e:#}")),
                        },
                        Err(e) => CommandExecution::Failed(format!("{e:#}")),
                    },
                    None => match config.open() {
                        Ok(new) => {
                            *interface = new;
                            CommandExecution::Applied
                        }
                        Err(e) => CommandExecution::Failed(format!("{e:#}")),
                    },
                };
                match &execution {
                    CommandExecution::Applied => {
                        *current_config = Some(config);
                        // A fresh interface deserves an immediate attempt:
                        // pull the next retry forward. The episode's counts
                        // stay — only a successful send closes it (and
                        // reports what was lost).
                        if let Some(ep) = episode.as_mut() {
                            ep.next_attempt = Instant::now();
                            ep.handle_needs_recovery = false;
                            // The explicit update supersedes any automatic
                            // replacement earlier in this episode.
                            ep.handle_replaced = false;
                        }
                        tracing::info!(
                            channel = who.id.as_u64(),
                            "channel {} interface updated",
                            who.label
                        );
                    }
                    CommandExecution::Failed(message) => {
                        tracing::warn!(
                            channel = who.id.as_u64(),
                            "channel {} interface update failed: {message}",
                            who.label,
                        );
                    }
                }
                emit_control(
                    &control_tx,
                    &notify,
                    RunnerControlStatus::CommandCompleted {
                        channel: who.id,
                        id,
                        target: CommandTarget::Interface,
                        execution,
                    },
                );
                Flow::Continue
            }
            TalkerCommand::SetInterval {
                id,
                index,
                interval_ms,
            } => {
                let execution = if schedule.set_interval(index, interval_ms, Instant::now()) {
                    CommandExecution::Applied
                } else {
                    CommandExecution::Failed(format!(
                        "message index {index} is outside the {}-message schedule",
                        schedule.len()
                    ))
                };
                emit_control(
                    &control_tx,
                    &notify,
                    RunnerControlStatus::CommandCompleted {
                        channel: who.id,
                        id,
                        target: CommandTarget::MessageInterval(index),
                        execution,
                    },
                );
                Flow::Continue
            }
        }
    };

    // Timer guard, policy-edge notification, counter invalidation, and dropped
    // observer accounting advance as one state machine. SetInterval re-evaluates
    // its policy on the next loop pass.
    let mut timer = TimerReconciler::new(who, &status_tx, &notify);

    // The current failing episode, if any (bounded-backoff retry policy —
    // see [`RETRY_BACKOFF_INITIAL`]). `None` while sends are succeeding.
    let mut episode: Option<FailureEpisode> = None;

    // The current off-cadence episode, if any. `None` while every scheduled
    // send is being reached. Independent of `episode` above: a channel can miss
    // its cadence with a perfectly healthy interface, and can fail every write
    // while keeping perfect cadence.
    let mut miss_episode: Option<MissEpisode> = None;

    // Renders that returned nothing for an index the schedule handed out. Per
    // run rather than per slot, because a runner outlives no run but its own.
    let mut render_faults = InternalFaultTally::default();
    // Missing confirmed configuration during handle replacement is a separate
    // invariant from rendering, so its first occurrence must not suppress the
    // render invariant's first report (ADR-054).
    let mut retry_config_faults = InternalFaultTally::default();

    let end_reason = 'run: loop {
        // Drain anything already queued so back-to-back sends can't starve
        // command handling.
        for cmd in cmd_rx.try_iter() {
            if let Flow::Stop = handle(
                cmd,
                &mut interface,
                &mut current_config,
                &mut schedule,
                &mut episode,
            ) {
                break 'run RunEndReason::StopCommand;
            }
        }

        if schedule.reconcile_wall_clock(Instant::now()) {
            tracing::info!(
                channel = who.id.as_u64(),
                realignments = schedule.clock_realignments(),
                "channel {} re-aligned its future sends after the system clock jumped",
                who.label
            );
        }

        // Reconcile continuous intent only after applying queued interval
        // changes. Windowed intent is finalized once `poll` supplies the next
        // deadline; dormant/ordinary schedules release before they block.
        timer.reconcile_schedule(
            schedule.active_cadence(),
            schedule.cadence_alignment(),
            schedule.clock_realignments(),
        );

        let poll_at = Instant::now();
        match schedule.poll(poll_at) {
            Tick::Due {
                index,
                scheduled_for,
                interval,
                skipped,
            } => {
                // A bounded-window request has done its job once the deadline
                // wait returns. Rendering and clock reads do not benefit from
                // holding it through the send call.
                timer.before_due();
                let due_handled_at = Instant::now();
                send_timing.record_deadline_lateness(
                    due_handled_at,
                    due_handled_at.saturating_duration_since(scheduled_for),
                );
                per_message_timing.record_due(index, scheduled_for, due_handled_at);
                // Order matters: `record_due` settles which send this tick's
                // backlog belongs to, and the skipped points belong to the
                // same one.
                per_message_timing.record_skips(index, scheduled_for, interval, skipped);
                // Report falling off cadence and returning to it, and nothing
                // in between. The first skip is reported for the same reason
                // the first failed send is: it is the moment the reader could
                // have acted, and every later one says only "still".
                match observe_skips(&mut miss_episode, skipped, due_handled_at) {
                    MissReport::FellBehind(now_skipped) => tracing::warn!(
                        channel = who.id.as_u64(),
                        "channel {} missed {now_skipped} scheduled sends — output is running \
                         below its configured rate",
                        who.label
                    ),
                    // Not "back to normal": this is a statement about cadence
                    // points only. Sends can still be failing or withheld by
                    // retry backoff while every point is reached, and that
                    // episode reports itself separately.
                    MissReport::BackOnSchedule(total) => tracing::info!(
                        channel = who.id.as_u64(),
                        "channel {} has missed no further scheduled sends for {} seconds \
                         ({total} missed in that episode)",
                        who.label,
                        MISS_RECOVERY_SETTLE.as_secs()
                    ),
                    MissReport::Silent => {}
                }
                let backoff_withheld = episode
                    .as_ref()
                    .is_some_and(|ep| due_handled_at < ep.next_attempt);
                let needs_recovery = episode.as_ref().is_some_and(|ep| ep.handle_needs_recovery);
                let mut reopen_withheld = false;
                if !backoff_withheld && needs_recovery {
                    match interface.prepare_retry(current_config.as_ref()) {
                        Ok(handle_replaced) => {
                            // If the following send fails it sets this back to
                            // true in the ordinary failure branch below.
                            if let Some(ep) = episode.as_mut() {
                                ep.handle_needs_recovery = false;
                                ep.handle_replaced |= handle_replaced;
                            }
                        }
                        Err(e) => {
                            // No interface write was attempted, so this due fire
                            // is withheld rather than failed. It has no render or
                            // send-call timing either; reopen work is outside
                            // both measurement boundaries.
                            if let Some(ep) = episode.as_mut() {
                                ep.active_error = format!("{e:#}");
                                ep.suppressed += 1;
                                ep.backoff = (ep.backoff * 2).min(RETRY_BACKOFF_MAX);
                                ep.next_attempt = Instant::now() + ep.backoff;
                                if e.downcast_ref::<MissingRetryConfiguration>().is_some() {
                                    if let Some(line) = retry_config_faults.report(
                                        &who.label,
                                        "recovery had no confirmed interface settings",
                                        "That send was withheld; automatic reopen cannot proceed \
                                         until the interface is updated or the channel is restarted",
                                    ) {
                                        tracing::error!("{line}");
                                    }
                                } else {
                                    tracing::debug!(
                                        channel = who.id.as_u64(),
                                        "channel {} interface still unavailable ({} failed sends, \
                                         {} withheld while retrying): {e:#}",
                                        who.label,
                                        ep.failures,
                                        ep.suppressed
                                    );
                                }
                            }
                            reopen_withheld = true;
                        }
                    }
                }
                if backoff_withheld || reopen_withheld {
                    // Backoff gate: this due fire is suppressed — counted, but
                    // not rendered or written. A retry-edge fire may have tried
                    // to reopen the handle first. The scheduler has already
                    // advanced, consistent with the stall policy (cadence over
                    // count).
                    if backoff_withheld {
                        if let Some(ep) = episode.as_mut() {
                            ep.suppressed += 1;
                        }
                    }
                    suppressed_sends += 1;
                } else {
                    let render_started = Instant::now();
                    let Some(payload) = schedule.render(index) else {
                        // `poll` obtains this index from the same immutable-size
                        // schedule. Stay panic-free if that invariant ever
                        // changes — and report it as talker disagreeing with
                        // itself rather than anything about the link (ADR-054).
                        //
                        // Tallied like the supervisor's: this sits on the send
                        // path, so a broken invariant would otherwise emit a
                        // line at every due point, which on a 10 ms cadence is
                        // a hundred a second for as long as the run lasts.
                        if let Some(line) = render_faults.report(
                            &who.label,
                            format!("message {} could not be built", index + 1),
                            "That send was skipped; the channel is still running",
                        ) {
                            tracing::error!("{line}");
                        }
                        continue;
                    };
                    let render_finished = Instant::now();
                    send_timing.record_render_duration(
                        render_finished,
                        render_finished.saturating_duration_since(render_started),
                    );
                    per_message_timing.record_render(
                        index,
                        render_finished.saturating_duration_since(render_started),
                    );

                    let send_started = Instant::now();
                    let send_result = interface.send(&payload);
                    peer_bytes += interface.take_peer_bytes();
                    let send_finished = Instant::now();
                    send_timing.record_send_duration(
                        send_finished,
                        send_finished.saturating_duration_since(send_started),
                    );
                    // Recorded for a failed send too: a write that blocked and
                    // then errored held the thread just as long.
                    per_message_timing.record_send(index, send_started, send_finished);
                    match send_result {
                        Ok(()) => {
                            if let Some(ep) = episode.take() {
                                // "Suppressed" is defined on the send-outcomes
                                // line and nowhere the log reader can see, so
                                // this states what happened to those sends.
                                // A replaced handle names what it reconnected
                                // to (ADR-059, §4.5).
                                let replaced = match current_config.as_ref() {
                                    Some(InterfaceConfig::Serial(config)) if ep.handle_replaced => {
                                        format!("reopened serial port {}; ", config.port)
                                    }
                                    Some(InterfaceConfig::TcpClient(config))
                                        if ep.handle_replaced =>
                                    {
                                        format!("reconnected to {}; ", config.address)
                                    }
                                    _ => String::new(),
                                };
                                tracing::info!(
                                    channel = who.id.as_u64(),
                                    "channel {} {replaced}sending recovered after {}",
                                    who.label,
                                    recovery_cost(
                                        ep.failures - ep.possibly_partial,
                                        ep.possibly_partial,
                                        ep.suppressed
                                    )
                                );
                                timer.emit(TalkerStatus::SendRecovered {
                                    channel: who.id,
                                    failures: ep.failures,
                                    suppressed: ep.suppressed,
                                });
                            }
                            total_count += 1;
                            total_bytes += payload.len() as u64;
                            if index >= per_message_counts.len() {
                                per_message_counts.resize(index + 1, 0);
                            }
                            per_message_counts[index] += 1;
                            // The payload observer lane is rate-limited and
                            // best-effort (ADR-018): a full receiver drops the
                            // sample rather than backpressuring send cadence.
                            let now = Instant::now();
                            let due = last_sample.is_none_or(|t| now - t >= policy.sample_interval);
                            if due {
                                let repeat =
                                    last_sampled_index == Some(index) && schedule.len() > 1;
                                if !repeat || repeats_skipped_while_due >= schedule.len() as u64 {
                                    last_sample = Some(now);
                                    last_sampled_index = Some(index);
                                    repeats_skipped_while_due = 0;
                                    let replacement_wire_offsets =
                                        schedule.replacement_wire_offsets(index).to_vec();
                                    timer.emit(TalkerStatus::SendSample {
                                        channel: who.id,
                                        message_index: index,
                                        payload,
                                        replacement_wire_offsets,
                                    });
                                } else {
                                    repeats_skipped_while_due += 1;
                                }
                            }
                        }
                        Err(e) => {
                            // A write that failed after the interface accepted
                            // part of the message is possibly partial (§4.4,
                            // ADR-059). Its bytes count as wire bytes; the
                            // message is never resent.
                            let written = e
                                .downcast_ref::<PartialWrite>()
                                .map(|partial| partial.written);
                            match written {
                                Some(written) => {
                                    possibly_partial_sends += 1;
                                    total_bytes += written as u64;
                                }
                                None => failed_sends += 1,
                            }
                            let partial = u64::from(written.is_some());
                            match episode.as_mut() {
                                // Edge-triggered: only the episode's first failure is
                                // reported (warn + `ConnectionError`); it opens the episode.
                                None => {
                                    send_failure_episodes += 1;
                                    tracing::warn!(
                                        channel = who.id.as_u64(),
                                        "channel {} send failed (retrying, with a growing delay \
                                         between attempts): {e:#}",
                                        who.label
                                    );
                                    let message = format!("{e:#}");
                                    episode = Some(FailureEpisode {
                                        active_error: message.clone(),
                                        failures: 1,
                                        possibly_partial: partial,
                                        suppressed: 0,
                                        backoff: RETRY_BACKOFF_INITIAL,
                                        next_attempt: Instant::now() + RETRY_BACKOFF_INITIAL,
                                        handle_needs_recovery: true,
                                        handle_replaced: false,
                                    });
                                    timer.emit(TalkerStatus::ConnectionError {
                                        channel: who.id,
                                        message,
                                    });
                                }
                                // A failed retry deepens the backoff; no re-report.
                                Some(ep) => {
                                    ep.active_error = format!("{e:#}");
                                    ep.failures += 1;
                                    ep.possibly_partial += partial;
                                    ep.handle_needs_recovery = true;
                                    ep.backoff = (ep.backoff * 2).min(RETRY_BACKOFF_MAX);
                                    ep.next_attempt = Instant::now() + ep.backoff;
                                    tracing::debug!(
                                        channel = who.id.as_u64(),
                                        "channel {} send still failing ({} failures so far): {e:#}",
                                        who.label,
                                        ep.failures
                                    );
                                }
                            }
                        }
                    }
                }

                // Cumulative outcomes remain observable even when every due
                // send is failing or suppressed. This lane is rate-limited
                // and best-effort, so it cannot slow the scheduler hot path.
                let now = Instant::now();
                if timer.counters_due(now, policy.counter_interval) {
                    let drops_so_far = timer.dropped_statuses();
                    per_message_timing.set_schedule(schedule.message_demand());
                    timer.emit(TalkerStatus::Counters {
                        channel: who.id,
                        total_count,
                        total_bytes,
                        per_message_counts: per_message_counts.clone(),
                        per_message_timing: per_message_timing.snapshot(),
                        dropped_statuses: drops_so_far,
                        missed_sends: schedule.missed_sends(),
                        failed_sends,
                        possibly_partial_sends,
                        peer_bytes: peer_sent(current_config.as_ref(), peer_bytes),
                        send_failure_episodes,
                        suppressed_sends,
                        active_send_error: episode.as_ref().map(|ep| ep.active_error.clone()),
                        timing: Box::new(send_timing.snapshot_at(now)),
                        captured_at: now,
                        final_snapshot: false,
                        timer: timer.status(),
                    });
                }
            }
            // Nothing due yet: block on the command channel until the next
            // fire deadline. Wakes instantly for a command, exactly on time
            // for the schedule, and detects a dropped handle.
            Tick::Wait(until) => {
                let wait_until = timer.prepare_wait(Instant::now(), until);
                match cmd_rx.recv_deadline(wait_until) {
                    Ok(cmd) => {
                        if let Flow::Stop = handle(
                            cmd,
                            &mut interface,
                            &mut current_config,
                            &mut schedule,
                            &mut episode,
                        ) {
                            break 'run RunEndReason::StopCommand;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        break 'run RunEndReason::OwnerDisconnected;
                    }
                }
            }
            // No active messages: nothing can happen until a command arrives,
            // so block indefinitely — zero wakeups.
            Tick::Idle => match cmd_rx.recv() {
                Ok(cmd) => {
                    if let Flow::Stop = handle(
                        cmd,
                        &mut interface,
                        &mut current_config,
                        &mut schedule,
                        &mut episode,
                    ) {
                        break 'run RunEndReason::StopCommand;
                    }
                }
                Err(_) => break 'run RunEndReason::OwnerDisconnected,
            },
        }
    };

    // Stop paying the platform timer-resolution cost before the exact final
    // status delivery, which may briefly wait for a full observer queue.
    let (timer_status, dropped_statuses) = timer.finish();

    // Final counters (ADR-018): the rate-limited lane can be up to one
    // interval stale when the runner stops — emit once more so the observer's
    // totals are exact at rest. A **blocking** send, deliberately: the runner
    // is exiting so cadence no longer matters, and this is the one status
    // that must not be lost to a momentarily full queue ("exact at rest" is
    // a promise, not best-effort). The owner keeps draining a stopped
    // runner's receiver until the thread exits (supervisor `poll`/`join_all`,
    // the CLI's funnel loop), and a dropped receiver returns an error
    // immediately — so this cannot hang.
    let finished_mono = Instant::now();
    let finished_at = SystemTime::now();
    let missed_sends = schedule.missed_sends();
    // A run that stops mid-episode never reaches a settle window, so the log
    // would otherwise end on the warning with no total against it. The run's
    // own figure is stated rather than the episode's: it is exact, channel-wide,
    // and the same number the completed-run summary carries.
    if miss_episode.is_some() {
        tracing::warn!(
            channel = who.id.as_u64(),
            "channel {} stopped while off schedule — {missed_sends} scheduled sends missed \
             during this run",
            who.label
        );
    }
    let final_timing = send_timing.snapshot_at(finished_mono);
    per_message_timing.set_schedule(schedule.message_demand());
    let final_per_message = per_message_timing.snapshot();
    let _ = status_tx.send(TalkerStatus::Counters {
        channel: who.id,
        total_count,
        total_bytes,
        per_message_counts: per_message_counts.clone(),
        per_message_timing: final_per_message.clone(),
        dropped_statuses,
        missed_sends,
        failed_sends,
        possibly_partial_sends,
        peer_bytes: peer_sent(current_config.as_ref(), peer_bytes),
        send_failure_episodes,
        suppressed_sends,
        active_send_error: episode.as_ref().map(|ep| ep.active_error.clone()),
        timing: Box::new(final_timing),
        captured_at: finished_mono,
        final_snapshot: true,
        timer: timer_status,
    });
    if let Some(n) = &notify {
        n();
    }
    emit_control(
        &control_tx,
        &notify,
        RunnerControlStatus::RunFinished {
            channel: who.id,
            summary: Box::new(RunSummary {
                run_id: who.run_id,
                channel: who.id,
                label: who.label.clone(),
                started_at,
                finished_at,
                elapsed: finished_mono.saturating_duration_since(started_mono),
                end_reason,
                total_count,
                total_bytes,
                per_message_counts,
                per_message_timing: final_per_message,
                dropped_statuses,
                missed_sends,
                failed_sends,
                possibly_partial_sends,
                peer_bytes: peer_sent(current_config.as_ref(), peer_bytes),
                suppressed_sends,
                timing: final_timing,
                timer: timer_status,
            }),
        },
    );
}

/// The run's peer-reply total, for a transport that reads its peer: only the
/// TCP client does (ADR-059, §4.5).
fn peer_sent(config: Option<&InterfaceConfig>, bytes: u64) -> Option<u64> {
    matches!(config, Some(InterfaceConfig::TcpClient(_))).then_some(bytes)
}

/// What a closed failure episode cost, for its recovery line: the sends that
/// failed, those that were possibly partial, and the due sends withheld.
fn recovery_cost(failed: u64, possibly_partial: u64, withheld: u64) -> String {
    let sends = |n: u64| if n == 1 { "send" } else { "sends" };
    let mut cost = Vec::new();
    if failed > 0 || possibly_partial == 0 {
        cost.push(format!("{failed} failed {}", sends(failed)));
    }
    if possibly_partial > 0 {
        cost.push(format!(
            "{possibly_partial} possibly partial {}",
            sends(possibly_partial)
        ));
    }
    format!("{} and {withheld} withheld while retrying", cost.join(", "))
}

/// Queue one status update, best-effort (never blocks the send cadence): a
/// full receiver counts a drop (`dropped_statuses` — cumulative fields in the
/// next delivered `Counters` self-correct), a disconnected one is ignored.
fn emit_status(
    status_tx: &Sender<TalkerStatus>,
    notify: &Option<StatusNotify>,
    who: &RunnerIdentity,
    dropped_statuses: &mut u64,
    status: TalkerStatus,
) {
    match status_tx.try_send(status) {
        Ok(()) => {
            if let Some(n) = notify {
                n();
            }
        }
        Err(TrySendError::Full(_)) => {
            *dropped_statuses += 1;
            // Stated as the consequence, not the mechanism. "Status receiver is
            // falling behind" named an internal queue the reader cannot see and
            // read as a fault on the send path, which is the one thing this can
            // never be. What it costs is display freshness, and nothing else.
            if *dropped_statuses == 1 {
                tracing::warn!(
                    channel = who.id.as_u64(),
                    "channel {}: the live output display may lag with no disruption of \
                     output count or cadence",
                    who.label
                );
            }
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Queue control truth reliably. Commands are rare and the control queue is sized
/// from the command queue, so an owner that keeps polling cannot lose a completion;
/// a dropped owner releases the send immediately with `Disconnected`.
fn emit_control(
    control_tx: &Option<Sender<RunnerControlStatus>>,
    notify: &Option<StatusNotify>,
    status: RunnerControlStatus,
) {
    let Some(control_tx) = control_tx else {
        return;
    };
    if control_tx.send(status).is_ok() {
        if let Some(n) = notify {
            n();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::core::channel::{SerialConfig, TcpClientConfig};
    use crate::core::message::{CodePage, MessageConfig, PayloadConfig};

    /// Sustained overload must log twice, not once per skipped send. This is
    /// the whole reason the episode exists: skips concentrate on the shortest
    /// interval, so a line each would flood the log with the fault's own
    /// symptom at the moment the log is least able to carry it.
    #[test]
    fn falling_off_cadence_reports_the_edges_and_not_every_skipped_send() {
        let start = Instant::now();
        let mut episode = None;

        // The first skip is the one the reader could have acted on.
        assert_eq!(
            observe_skips(&mut episode, 3, start),
            MissReport::FellBehind(3)
        );

        // A thousand polls of continuing overload say nothing further, and the
        // count accrues for the line that closes the episode.
        for tick in 1..=1_000 {
            assert_eq!(
                observe_skips(&mut episode, 2, start + Duration::from_millis(tick)),
                MissReport::Silent,
                "a continuing episode must not log again at tick {tick}"
            );
        }

        // A clean poll inside the settle window is not yet a recovery: a
        // marginal channel alternates, and reporting each clean tick would
        // produce the same flood with two lines instead of one.
        assert_eq!(
            observe_skips(&mut episode, 0, start + MISS_RECOVERY_SETTLE),
            MissReport::Silent
        );

        // Settle is measured from the last skip, not from the episode's start.
        let last_skip = start + Duration::from_millis(1_000);
        assert_eq!(
            observe_skips(&mut episode, 0, last_skip + MISS_RECOVERY_SETTLE),
            MissReport::BackOnSchedule(3 + 2 * 1_000)
        );
        assert!(episode.is_none(), "recovery closes the episode");

        // And a quiet channel stays quiet.
        assert_eq!(
            observe_skips(&mut episode, 0, last_skip + MISS_RECOVERY_SETTLE * 10),
            MissReport::Silent
        );
    }

    /// A channel that recovers and later falls behind again is two episodes,
    /// each with its own count — not one running total that outlives the fault
    /// it described.
    #[test]
    fn a_second_lapse_is_counted_from_zero() {
        let start = Instant::now();
        let mut episode = None;

        observe_skips(&mut episode, 5, start);
        assert_eq!(
            observe_skips(&mut episode, 0, start + MISS_RECOVERY_SETTLE),
            MissReport::BackOnSchedule(5)
        );

        let later = start + MISS_RECOVERY_SETTLE * 4;
        assert_eq!(
            observe_skips(&mut episode, 1, later),
            MissReport::FellBehind(1)
        );
        assert_eq!(
            observe_skips(&mut episode, 0, later + MISS_RECOVERY_SETTLE),
            MissReport::BackOnSchedule(1),
            "the second episode must not carry the first one's total"
        );
    }

    /// An [`Interface`] that records every payload (or fails on demand).
    struct MockInterface {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        fail: bool,
        /// Hold the channel thread for this long when a payload starts with
        /// the given byte, standing in for a slow write on one message only.
        slow: Option<(u8, Duration)>,
    }

    impl Interface for MockInterface {
        fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
            anyhow::ensure!(!self.fail, "mock send failure");
            if let Some((marker, block)) = self.slow {
                if data.first() == Some(&marker) {
                    std::thread::sleep(block);
                }
            }
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    fn spawn_runner(
        messages: &[MessageConfig],
        fail: bool,
    ) -> (Arc<Mutex<Vec<Vec<u8>>>>, TalkerHandle, ChannelId) {
        // Tests default to the every-send policy so per-send behaviour stays
        // directly observable; the sampled lanes have their own test.
        spawn_runner_with(messages, fail, ObserverPolicy::every_send())
    }

    fn spawn_runner_with(
        messages: &[MessageConfig],
        fail: bool,
        policy: ObserverPolicy,
    ) -> (Arc<Mutex<Vec<Vec<u8>>>>, TalkerHandle, ChannelId) {
        spawn_runner_with_mode(messages, fail, policy)
    }

    /// One active message at `interval_ms` — the schedule shape the timer
    /// reconciler tests care about, since timer policy reads only the shortest.
    fn one_cadence(interval_ms: u64) -> Option<timing::ActiveCadence> {
        let interval = Duration::from_millis(interval_ms);
        Some(timing::ActiveCadence {
            messages: 1,
            shortest: interval,
        })
    }

    fn spawn_runner_with_mode(
        messages: &[MessageConfig],
        fail: bool,
        policy: ObserverPolicy,
    ) -> (Arc<Mutex<Vec<Vec<u8>>>>, TalkerHandle, ChannelId) {
        spawn_runner_full(messages, fail, policy, None, None)
    }

    fn spawn_runner_full(
        messages: &[MessageConfig],
        fail: bool,
        policy: ObserverPolicy,
        slow: Option<(u8, Duration)>,
        log_tx: Option<crossbeam_channel::Sender<crate::core::logging::LogEvent>>,
    ) -> (Arc<Mutex<Vec<Vec<u8>>>>, TalkerHandle, ChannelId) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let interface = Box::new(MockInterface {
            sent: Arc::clone(&sent),
            fail,
            slow,
        });
        let schedule = Schedule::compile(messages, Instant::now()).unwrap();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        let (control_tx, control_rx) = crossbeam_channel::bounded(16);
        let (status_tx, status_rx) = crossbeam_channel::bounded(256);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "test".into(),
            run_id: RunId::mint(),
        };
        let id = who.id;
        let thread = std::thread::spawn(move || {
            let go = move || {
                run(
                    who,
                    interface,
                    None,
                    schedule,
                    cmd_rx,
                    RunnerObserver::new(status_tx, policy).with_control(control_tx),
                )
            };
            // A subscriber installed inside the runner's own thread. Tracing's
            // default is thread-local, so this captures exactly this run's
            // lines: no global to install, nothing shared with another test,
            // and no window between spawning and subscribing.
            match log_tx {
                Some(tx) => crate::core::logging::with_gui_test_subscriber(tx, go),
                None => go(),
            }
        });
        (
            sent,
            TalkerHandle {
                cmd_tx,
                control_rx,
                status_rx,
                thread,
            },
            id,
        )
    }

    /// Wait (bounded) for the runner thread to finish.
    fn join_within(handle: TalkerHandle, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !handle.thread.is_finished() {
            assert!(Instant::now() < deadline, "runner did not stop in time");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.thread.join().unwrap();
    }

    fn msg(hex: &str, interval_ms: u64) -> MessageConfig {
        MessageConfig::new(PayloadConfig::raw_hex(hex), interval_ms)
    }

    /// A runner under test: the payloads it wrote, its handle, and its log.
    type LoggingRunner = (
        Arc<Mutex<Vec<Vec<u8>>>>,
        TalkerHandle,
        crossbeam_channel::Receiver<crate::core::logging::LogEvent>,
    );

    /// A runner whose log lines are delivered to the returned receiver.
    fn spawn_logging_runner(
        messages: &[MessageConfig],
        fail: bool,
        slow: Option<(u8, Duration)>,
    ) -> LoggingRunner {
        let (log_tx, log_rx) = crossbeam_channel::unbounded();
        let (sent, handle, _id) = spawn_runner_full(
            messages,
            fail,
            ObserverPolicy::every_send(),
            slow,
            Some(log_tx),
        );
        (sent, handle, log_rx)
    }

    /// Wait (bounded) for a line containing `want`, accumulating into `seen`.
    ///
    /// A test asserting that something is *absent* must first wait for
    /// something present, or it proves only that it looked too early.
    fn wait_for_line(
        log_rx: &crossbeam_channel::Receiver<crate::core::logging::LogEvent>,
        seen: &mut Vec<String>,
        want: &str,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            seen.extend(log_rx.try_iter().map(|event| event.message));
            if seen.iter().any(|line| line.contains(want)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no line containing {want:?} within 5 s; saw {seen:#?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The edge-triggering, through the real send path rather than the
    /// transition table: overload that skips hundreds of cadence points must
    /// still produce one line, and a run stopped mid-episode must not leave the
    /// warning unanswered.
    ///
    /// 120 ms writes against a 10 ms cadence skip at least a dozen points per
    /// send, on any machine — the assertion is on the count of *lines*, which
    /// the edge-trigger fixes at one however many points are lost.
    #[test]
    fn a_stalled_channel_logs_one_warning_and_answers_it_at_stop() {
        let (sent, handle, log_rx) = spawn_logging_runner(
            &[msg("AB", 10)],
            false,
            Some((0xAB, Duration::from_millis(120))),
        );
        let mut lines = Vec::new();

        // Two completed slow writes guarantee skipped points between them.
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.lock().unwrap().len() < 2 {
            assert!(Instant::now() < deadline, "the slow interface never sent");
            std::thread::sleep(Duration::from_millis(10));
        }
        wait_for_line(&log_rx, &mut lines, "missed");

        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(5));
        wait_for_line(&log_rx, &mut lines, "stopped while off schedule");

        let fell_behind = lines
            .iter()
            .filter(|line| line.contains("missed") && line.contains("below its configured rate"))
            .count();
        assert_eq!(
            fell_behind, 1,
            "the episode opens once, however many points are skipped: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("no further")),
            "five seconds never elapsed without a skip, so nothing recovered: {lines:#?}"
        );
    }

    /// Cadence and delivery are independent, and the log must not blur them.
    /// Every write fails here while every deadline is reached, so the failure
    /// path speaks and the cadence path stays silent — the seam that makes
    /// "back on schedule" a claim about cadence alone.
    #[test]
    fn failing_sends_do_not_make_a_channel_look_off_cadence() {
        // 200 ms apart and failing instantly: reaching each deadline needs an
        // OS delay of a fifth of a second to miss, which is not a hiccup.
        let (_sent, handle, log_rx) = spawn_logging_runner(&[msg("AB", 200)], true, None);
        let mut lines = Vec::new();

        wait_for_line(&log_rx, &mut lines, "mock send failure");
        std::thread::sleep(Duration::from_millis(450));

        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(5));

        lines.extend(log_rx.try_iter().map(|event| event.message));
        assert!(
            !lines.iter().any(|line| line.contains("missed")),
            "failing sends are not missed sends: {lines:#?}"
        );
    }

    /// End to end through the real send path: the message whose write holds the
    /// thread is charged, and the fast message it delays is not — even though
    /// the fast message is the one recording all the lateness.
    #[test]
    fn a_slow_message_is_charged_for_the_delay_it_imposes_on_a_fast_one() {
        // #0 every 10 ms and instant; #1 every 200 ms and blocks for 120 ms.
        let (_sent, handle, _id) = spawn_runner_full(
            &[msg("AA", 10), msg("BB", 200)],
            false,
            ObserverPolicy::every_send(),
            Some((0xBB, Duration::from_millis(120))),
            None,
        );

        std::thread::sleep(Duration::from_millis(700));
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();

        let mut final_timing = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while final_timing.is_none() {
            for status in handle.status_rx.try_iter() {
                if let TalkerStatus::Counters {
                    per_message_timing,
                    final_snapshot: true,
                    ..
                } = status
                {
                    final_timing = Some(per_message_timing);
                }
            }
            assert!(Instant::now() < deadline, "no final counter snapshot");
            std::thread::sleep(Duration::from_millis(5));
        }
        let timing = final_timing.unwrap();
        join_within(handle, Duration::from_secs(5));

        let fast = timing[0];
        let slow = timing[1];
        // The fast message is the victim: its 10 ms deadlines pass inside every
        // 120 ms block, so it carries the lateness.
        assert!(
            fast.deadline_lateness.max().unwrap_or_default() >= Duration::from_millis(50),
            "fast message should have recorded the delay it suffered: {fast:?}"
        );
        // The slow message is the culprit, and the counter says so.
        assert!(
            slow.blocking_sends >= 1,
            "the blocking send was not counted: {slow:?}"
        );
        assert!(
            slow.blocked_others > fast.blocked_others,
            "blame landed on the victim rather than the blocker: fast={fast:?} slow={slow:?}"
        );
        assert!(
            slow.blocked_others >= Duration::from_millis(50),
            "attributed delay is implausibly small for a 120 ms block: {slow:?}"
        );
        // The same write also destroys cadence points outright — about eleven
        // of the fast message's per 120 ms block — and the runner charges
        // those where they happened rather than leaving them to be inferred
        // from the one deadline it did reach.
        assert!(
            slow.missed_others >= 5,
            "skipped points were not charged to the blocking send: {slow:?}"
        );
        assert_eq!(
            fast.missed_others, 0,
            "the message that lost the points must not be charged for them: {fast:?}"
        );
    }

    #[test]
    fn timer_reconciler_retains_high_rate_guard_and_windows_fifty_milliseconds() {
        let (status_tx, _status_rx) = crossbeam_channel::bounded(16);
        let notify = None;
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "timer-test".into(),
            run_id: RunId::mint(),
        };
        let mut timer = TimerReconciler::new(&who, &status_tx, &notify);

        timer.reconcile_schedule(one_cadence(10), timing::CadenceAlignment::Immediate, 0);
        assert_eq!(timer.intent, timing::TimerIntent::ContinuousHighRate);
        assert_eq!(timer.status().reason, timing::TimerReason::HighRate);
        assert!(timer.guard.is_some());
        assert_eq!(timer.guard_acquisitions, 1);
        assert_eq!(timer.guard_releases, 0);

        let now = Instant::now();
        for multiple in 1..=3 {
            let short_deadline = now + Duration::from_millis(10 * multiple);
            assert_eq!(timer.prepare_wait(now, short_deadline), short_deadline);
            timer.before_due();
            assert!(timer.guard.is_some());
        }
        assert_eq!(
            (timer.guard_acquisitions, timer.guard_releases),
            (1, 0),
            "automatic high-rate deadlines retain one continuous guard"
        );

        timer.reconcile_schedule(None, timing::CadenceAlignment::Immediate, 0);
        assert!(timer.guard.is_none(), "an idle schedule releases the guard");
        assert_eq!(timer.status().mode, timing::TimerMode::Standard);
        assert_eq!((timer.guard_acquisitions, timer.guard_releases), (1, 1));

        timer.reconcile_schedule(one_cadence(50), timing::CadenceAlignment::Immediate, 0);
        assert_eq!(timer.intent, timing::TimerIntent::PrecisionWindow);
        assert_eq!(timer.status().reason, timing::TimerReason::PrecisionWindow);
        assert!(timer.guard.is_none());
        assert_eq!((timer.guard_acquisitions, timer.guard_releases), (1, 1));

        let near_threshold_deadline = now + Duration::from_millis(50);
        #[cfg(windows)]
        {
            assert_eq!(
                timer.prepare_wait(now, near_threshold_deadline),
                near_threshold_deadline - timing::PRECISION_WINDOW,
                "Windows stages before the final 32 ms precision window"
            );
            assert!(timer.guard.is_none());
            assert_eq!(
                timer.prepare_wait(
                    near_threshold_deadline - Duration::from_millis(1),
                    near_threshold_deadline,
                ),
                near_threshold_deadline
            );
            assert!(timer.guard.is_some());
            assert_eq!((timer.guard_acquisitions, timer.guard_releases), (2, 1));
            timer.before_due();
            assert!(timer.guard.is_none());
            assert_eq!((timer.guard_acquisitions, timer.guard_releases), (2, 2));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                timer.prepare_wait(now, near_threshold_deadline),
                near_threshold_deadline,
                "native-wait platforms do not add a staging wake"
            );
            assert!(timer.guard.is_none());
        }
        #[cfg(windows)]
        assert_eq!((timer.guard_acquisitions, timer.guard_releases), (2, 2));
        #[cfg(not(windows))]
        assert_eq!((timer.guard_acquisitions, timer.guard_releases), (1, 1));
    }

    #[test]
    fn timer_reconciler_owns_policy_edge_notification_and_counter_refresh() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (status_tx, status_rx) = crossbeam_channel::bounded(1);
        let notifications = Arc::new(AtomicUsize::new(0));
        let notify_count = Arc::clone(&notifications);
        let notify: Option<StatusNotify> = Some(Box::new(move || {
            notify_count.fetch_add(1, Ordering::Relaxed);
        }));
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "timer-edge-test".into(),
            run_id: RunId::mint(),
        };
        let mut timer = TimerReconciler::new(&who, &status_tx, &notify);
        let now = Instant::now();

        timer.reconcile_schedule(None, timing::CadenceAlignment::UtcPhase, 0);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert!(timer.counters_due(now, Duration::from_secs(60)));
        assert!(!timer.counters_due(now, Duration::from_secs(60)));

        // The first edge still occupies the one-slot queue, so the changed
        // clock count is dropped and accounted without blocking.
        timer.reconcile_schedule(None, timing::CadenceAlignment::UtcPhase, 1);
        assert_eq!(timer.dropped_statuses(), 1);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert!(
            timer.counters_due(now, Duration::from_secs(60)),
            "every timer-policy edge invalidates the counter rate limit"
        );

        let first = status_rx.try_recv().expect("first timer edge");
        assert!(matches!(first, TalkerStatus::TimerStatus { .. }));
        timer.reconcile_schedule(None, timing::CadenceAlignment::UtcPhase, 2);
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
        let latest = status_rx.try_recv().expect("latest timer edge");
        assert!(matches!(
            latest,
            TalkerStatus::TimerStatus {
                status: timing::TimerStatus {
                    clock_realignments: 2,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn sends_on_schedule_and_reports_self_describing_counts() {
        let (sent, handle, id) = spawn_runner(&[msg("AB", 10)], false);
        // Wait (bounded) for a few fires rather than assuming a wall-clock
        // window: under the stall policy (skip the backlog, stay on grid) a
        // stalled CI VM can legitimately fire only once in a fixed 60 ms —
        // that's the policy working, not a defect (flaked on macOS CI).
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().len() < 3 {
            assert!(
                Instant::now() < deadline,
                "expected ≥3 sends within 2 s, got {}",
                sent.lock().unwrap().len()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        // Collect statuses only after the runner has fully stopped — a send
        // can land between an early drain and the Stop being processed, which
        // would desync `last_total` from the payload count below.
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));
        let statuses: Vec<TalkerStatus> = status_rx.try_iter().collect();

        let payloads = sent.lock().unwrap();
        assert!(payloads.iter().all(|p| p == &vec![0xAB]));

        // Lanes carry identity; counters are monotonic and exact at rest
        // (the final Counters emitted at stop — ADR-018).
        let mut samples = 0usize;
        let mut last_total = 0u64;
        let mut counter_finality = Vec::new();
        for s in &statuses {
            match s {
                TalkerStatus::SendSample {
                    channel,
                    message_index,
                    payload,
                    replacement_wire_offsets,
                } => {
                    assert_eq!(*channel, id, "samples carry the stable id");
                    assert_eq!(*message_index, 0);
                    assert_eq!(payload, &vec![0xAB]);
                    assert!(replacement_wire_offsets.is_empty());
                    samples += 1;
                }
                TalkerStatus::Counters {
                    channel,
                    total_count,
                    total_bytes,
                    per_message_counts,
                    dropped_statuses,
                    final_snapshot,
                    ..
                } => {
                    assert_eq!(*channel, id, "counters carry the stable id");
                    assert!(*total_count >= last_total, "counters must be monotonic");
                    last_total = *total_count;
                    // Every payload is the single byte 0xAB, so the byte
                    // total tracks the send count exactly.
                    assert_eq!(*total_bytes, *total_count);
                    assert_eq!(per_message_counts.iter().sum::<u64>(), *total_count);
                    assert_eq!(*dropped_statuses, 0);
                    counter_finality.push(*final_snapshot);
                }
                TalkerStatus::TimerStatus { channel, status } => {
                    assert_eq!(*channel, id, "timer status carries the stable id");
                    assert_eq!(
                        status.shortest_active_interval(),
                        Some(Duration::from_millis(10))
                    );
                    assert_ne!(status.mode, timing::TimerMode::Standard);
                }
                _ => panic!("unexpected status variant"),
            }
        }
        // every-send policy: one sample per send; the final Counters makes
        // the totals exact.
        assert_eq!(samples, payloads.len());
        assert_eq!(last_total as usize, payloads.len());
        assert!(
            counter_finality.len() > 1,
            "the run should include periodic and final snapshots"
        );
        assert!(
            counter_finality[..counter_finality.len() - 1]
                .iter()
                .all(|final_snapshot| !final_snapshot),
            "periodic snapshots must not claim final provenance"
        );
        assert_eq!(counter_finality.last(), Some(&true));
    }

    #[test]
    fn finished_summary_matches_the_exact_final_counters() {
        let (sent, handle, id) = spawn_runner(&[msg("AB", 5)], false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().len() < 3 {
            assert!(Instant::now() < deadline, "runner did not send in time");
            std::thread::sleep(Duration::from_millis(2));
        }

        let status_rx = handle.status_rx.clone();
        let control_rx = handle.control_rx.clone();
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));

        let final_counters = status_rx
            .try_iter()
            .filter_map(|status| match status {
                TalkerStatus::Counters {
                    total_count,
                    total_bytes,
                    per_message_counts,
                    dropped_statuses,
                    missed_sends,
                    failed_sends,
                    suppressed_sends,
                    timing,
                    timer,
                    ..
                } => Some((
                    total_count,
                    total_bytes,
                    per_message_counts,
                    dropped_statuses,
                    missed_sends,
                    failed_sends,
                    suppressed_sends,
                    *timing,
                    timer,
                )),
                _ => None,
            })
            .last()
            .expect("final counters");
        let summary = control_rx
            .try_iter()
            .find_map(|status| match status {
                RunnerControlStatus::RunFinished { channel, summary } => {
                    assert_eq!(channel, id);
                    Some(*summary)
                }
                _ => None,
            })
            .expect("run completion");

        assert_eq!(summary.channel, id);
        assert_eq!(summary.end_reason, RunEndReason::StopCommand);
        assert_eq!(summary.total_count, final_counters.0);
        assert_eq!(summary.total_bytes, final_counters.1);
        assert_eq!(summary.per_message_counts, final_counters.2);
        assert_eq!(summary.dropped_statuses, final_counters.3);
        assert_eq!(summary.missed_sends, final_counters.4);
        assert_eq!(summary.failed_sends, final_counters.5);
        assert_eq!(summary.suppressed_sends, final_counters.6);
        assert_eq!(summary.timing, final_counters.7);
        assert_eq!(summary.timer, final_counters.8);
        assert_eq!(summary.total_count, sent.lock().unwrap().len() as u64);
    }

    #[test]
    fn send_sample_carries_code_page_replacement_provenance() {
        let message = MessageConfig::new(
            PayloadConfig::Ascii {
                text: "?—".to_string(),
                code_page: CodePage::Iso8859_1,
            },
            10,
        );
        let (sent, handle, _) = spawn_runner(&[message], false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "expected one send within 2 s");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));

        let sample = status_rx
            .try_iter()
            .find_map(|status| match status {
                TalkerStatus::SendSample {
                    payload,
                    replacement_wire_offsets,
                    ..
                } => Some((payload, replacement_wire_offsets)),
                _ => None,
            })
            .expect("runner should emit a payload sample");
        assert_eq!(sample.0, b"??");
        assert_eq!(sample.1, vec![1]);
    }

    /// The sample lane rotates across message indices: with an aligned
    /// two-message schedule the low-index tie-break used to sample message 0
    /// forever; every message must reach the Output pane.
    #[test]
    fn sample_lane_rotates_across_messages() {
        let policy = ObserverPolicy {
            counter_interval: Duration::from_secs(3600),
            sample_interval: Duration::from_millis(5),
        };
        let (sent, handle, _id) = spawn_runner_with(&[msg("AB", 5), msg("CD", 5)], false, policy);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().len() < 40 {
            assert!(Instant::now() < deadline, "expected sends within 2 s");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));

        let mut seen = [false; 2];
        for s in status_rx.try_iter() {
            if let TalkerStatus::SendSample { message_index, .. } = s {
                if message_index < 2 {
                    seen[message_index] = true;
                }
            }
        }
        assert!(
            seen[0] && seen[1],
            "both messages must be sampled (got 0: {}, 1: {})",
            seen[0],
            seen[1]
        );
    }

    #[test]
    fn sampled_policy_bounds_payload_traffic() {
        // A huge sample interval: only the *first* send carries its payload,
        // however many sends happen; a zero counter interval keeps totals
        // exact per send. Pins the ADR-018 claim that display cost is
        // decoupled from send rate.
        let policy = ObserverPolicy {
            counter_interval: Duration::ZERO,
            sample_interval: Duration::from_secs(3600),
        };
        let (sent, handle, _id) = spawn_runner_with(&[msg("AB", 5)], false, policy);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().len() < 3 {
            assert!(Instant::now() < deadline, "expected ≥3 sends within 2 s");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));

        let mut samples = 0usize;
        let mut last_total = 0u64;
        for s in status_rx.try_iter() {
            match s {
                TalkerStatus::SendSample { .. } => samples += 1,
                TalkerStatus::Counters { total_count, .. } => last_total = total_count,
                TalkerStatus::TimerStatus { .. } => {}
                _ => panic!("unexpected status variant"),
            }
        }
        assert_eq!(samples, 1, "one payload sample regardless of send count");
        assert_eq!(last_total as usize, sent.lock().unwrap().len());
    }

    #[test]
    fn stop_is_prompt_even_when_idle() {
        // All-dormant schedule → the runner blocks on the command channel.
        let (_, handle, _id) = spawn_runner(&[msg("AB", 0)], false);
        std::thread::sleep(Duration::from_millis(20));
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(1));
    }

    #[test]
    fn dormant_runner_has_no_counter_heartbeat_but_still_reports_a_final_snapshot() {
        let policy = ObserverPolicy {
            counter_interval: Duration::ZERO,
            sample_interval: Duration::ZERO,
        };
        let (_, handle, _id) = spawn_runner_with(&[msg("AB", 0)], false, policy);
        let status_rx = handle.status_rx.clone();

        // A completed no-op interval command proves that the runner has
        // started and processed its dormant receive loop; the assertion
        // below therefore cannot pass merely because the thread was late to
        // start.
        let barrier_id = CommandId::mint();
        handle
            .cmd_tx
            .send(TalkerCommand::SetInterval {
                id: barrier_id,
                index: 0,
                interval_ms: 0,
            })
            .unwrap();
        loop {
            match handle.control_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(RunnerControlStatus::CommandCompleted { id, execution, .. })
                    if id == barrier_id =>
                {
                    assert_eq!(execution, CommandExecution::Applied);
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("dormant runner did not reach command barrier: {error}"),
            }
        }
        status_rx.try_iter().for_each(drop);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            status_rx
                .try_iter()
                .all(|status| !matches!(status, TalkerStatus::Counters { .. })),
            "an idle runner must not wake merely to refresh counters"
        );

        let stop_requested_at = Instant::now();
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(1));
        let snapshots = status_rx
            .try_iter()
            .filter_map(|status| match status {
                TalkerStatus::Counters {
                    captured_at,
                    final_snapshot,
                    ..
                } => Some((captured_at, final_snapshot)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            snapshots.len(),
            1,
            "a dormant run still emits one exact final counter snapshot"
        );
        assert!(snapshots[0].0 >= stop_requested_at);
        assert!(snapshots[0].0 <= Instant::now());
        assert!(
            snapshots[0].1,
            "the exact-at-rest snapshot must carry final provenance"
        );
    }

    #[test]
    fn making_fast_schedule_dormant_releases_timer_policy_before_idle() {
        let policy = ObserverPolicy {
            counter_interval: Duration::from_secs(3600),
            sample_interval: Duration::from_secs(3600),
        };
        let (_, handle, id) = spawn_runner_with(&[msg("AB", 5)], false, policy);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_fast = false;
        while !saw_fast {
            for status in handle.status_rx.try_iter() {
                if let TalkerStatus::TimerStatus { channel, status } = status {
                    assert_eq!(channel, id);
                    saw_fast = status.shortest_active_interval() == Some(Duration::from_millis(5));
                }
            }
            assert!(Instant::now() < deadline, "fast timer status not reported");
            std::thread::sleep(Duration::from_millis(2));
        }

        handle
            .cmd_tx
            .send(TalkerCommand::SetInterval {
                id: CommandId::mint(),
                index: 0,
                interval_ms: 0,
            })
            .unwrap();
        let mut dormant = None;
        while dormant.is_none() {
            for status in handle.status_rx.try_iter() {
                if let TalkerStatus::TimerStatus { status, .. } = status {
                    if status.shortest_active_interval().is_none() {
                        dormant = Some(status);
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "dormant timer status not reported before idle"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(dormant.unwrap().mode, timing::TimerMode::Standard);

        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(1));
    }

    #[test]
    fn precise_slow_schedule_uses_windowed_policy_and_dormant_releases_it() {
        let policy = ObserverPolicy {
            counter_interval: Duration::from_secs(3600),
            sample_interval: Duration::from_secs(3600),
        };
        let (_, handle, id) = spawn_runner_with_mode(&[msg("AB", 100)], false, policy);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_window_policy = false;
        let mut saw_platform_result = false;
        while !saw_platform_result {
            for status in handle.status_rx.try_iter() {
                if let TalkerStatus::TimerStatus { channel, status } = status {
                    assert_eq!(channel, id);
                    if status.reason == timing::TimerReason::PrecisionWindow {
                        saw_window_policy = true;
                        #[cfg(windows)]
                        {
                            saw_platform_result = matches!(
                                status.mode,
                                timing::TimerMode::WindowsOneMillisecond
                                    | timing::TimerMode::WindowsRequestFailed
                            );
                        }
                        #[cfg(not(windows))]
                        {
                            saw_platform_result =
                                status.mode == timing::TimerMode::NativeDeadlineWaits;
                        }
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "precise deadline-window status not reported"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(saw_window_policy);

        handle
            .cmd_tx
            .send(TalkerCommand::SetInterval {
                id: CommandId::mint(),
                index: 0,
                interval_ms: 0,
            })
            .unwrap();
        let mut dormant = None;
        while dormant.is_none() {
            for status in handle.status_rx.try_iter() {
                if let TalkerStatus::TimerStatus { status, .. } = status {
                    if status.shortest_active_interval().is_none() {
                        dormant = Some(status);
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "dormant precise policy not reported before idle"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let dormant = dormant.unwrap();
        assert_eq!(dormant.reason, timing::TimerReason::None);
        assert_eq!(dormant.mode, timing::TimerMode::Standard);

        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(1));
    }

    #[test]
    fn dropped_command_handle_stops_the_runner() {
        let (_, handle, _id) = spawn_runner(&[msg("AB", 0)], false);
        let TalkerHandle {
            cmd_tx,
            control_rx,
            status_rx: _status_rx,
            thread,
        } = handle;
        drop(cmd_tx); // owner went away without Stop
        let deadline = Instant::now() + Duration::from_secs(1);
        while !thread.is_finished() {
            assert!(Instant::now() < deadline, "runner leaked after disconnect");
            std::thread::sleep(Duration::from_millis(5));
        }
        thread.join().unwrap();
        let end_reason = control_rx.try_iter().find_map(|status| match status {
            RunnerControlStatus::RunFinished { summary, .. } => Some(summary.end_reason),
            _ => None,
        });
        assert_eq!(end_reason, Some(RunEndReason::OwnerDisconnected));
    }

    #[test]
    fn send_failure_reports_connection_error_and_keeps_running() {
        let (_, handle, id) = spawn_runner(&[msg("AB", 10)], true);
        std::thread::sleep(Duration::from_millis(40));
        let mut saw_error = false;
        for s in handle.status_rx.try_iter() {
            if let TalkerStatus::ConnectionError { channel, message } = s {
                assert_eq!(channel, id, "errors carry the stable id");
                assert!(message.contains("mock send failure"));
                saw_error = true;
            }
        }
        assert!(saw_error, "expected at least one ConnectionError");
        assert!(
            !handle.thread.is_finished(),
            "runner must survive send errors"
        );
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
    }

    #[test]
    fn repeated_send_failures_report_one_connection_error() {
        // 5 ms fires against a permanently failing interface: without the
        // edge trigger every retry could emit a ConnectionError; with it,
        // exactly one is emitted for the whole failure episode.
        let (_, handle, _id) = spawn_runner(&[msg("AB", 5)], true);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut statuses = Vec::new();
        loop {
            statuses.extend(handle.status_rx.try_iter());
            let saw_live_outcomes = statuses.iter().any(|status| {
                matches!(
                    status,
                    TalkerStatus::Counters {
                        failed_sends,
                        suppressed_sends,
                        ..
                    } if *failed_sends >= 1 && *suppressed_sends >= 1
                )
            });
            if saw_live_outcomes {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "failed and suppressed counters did not become live"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let errors = statuses
            .iter()
            .filter(|s| matches!(s, TalkerStatus::ConnectionError { .. }))
            .count();
        assert_eq!(
            errors, 1,
            "edge-triggered: only the episode's first failure is reported"
        );
        let outcomes = statuses.iter().rev().find_map(|status| match status {
            TalkerStatus::Counters {
                total_count,
                failed_sends,
                suppressed_sends,
                active_send_error,
                timing,
                ..
            } => Some((
                *total_count,
                *failed_sends,
                *suppressed_sends,
                active_send_error.clone(),
                **timing,
            )),
            _ => None,
        });
        let (sent, failed, suppressed, active_send_error, timing) =
            outcomes.expect("live cumulative counters");
        let cumulative = timing.cumulative;
        assert_eq!(sent, 0);
        assert!(failed >= 1, "the failed attempt is visible before stop");
        assert!(
            active_send_error
                .as_deref()
                .is_some_and(|message| message.contains("mock send failure")),
            "counter snapshots repeat the open failure episode"
        );
        assert!(
            suppressed >= 1,
            "backoff-suppressed sends are visible before stop"
        );
        assert_eq!(
            cumulative.deadline_lateness.sample_count(),
            sent + failed + suppressed,
            "every handled due fire contributes one deadline sample"
        );
        assert_eq!(
            cumulative.render_duration.sample_count(),
            sent + failed,
            "suppressed due fires must not render payloads"
        );
        assert_eq!(
            cumulative.send_duration.sample_count(),
            sent + failed,
            "only attempted sends contribute send-call samples"
        );
        assert_eq!(
            timing.recent.deadline_lateness.sample_count(),
            cumulative.deadline_lateness.sample_count(),
            "this short run fits entirely inside the recent window"
        );
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
    }

    /// An [`Interface`] whose failure mode can be flipped at runtime — for the
    /// recovery path.
    struct FlakyInterface {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Interface for FlakyInterface {
        fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
            anyhow::ensure!(
                !self.fail.load(std::sync::atomic::Ordering::SeqCst),
                "mock send failure"
            );
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    /// Start the real runner loop around a purpose-built interface. Recovery
    /// tests use this instead of `MockInterface` because the distinction being
    /// tested lives in the handle, not in the schedule.
    fn spawn_interface_runner(
        interface: Box<dyn Interface>,
        current_config: Option<InterfaceConfig>,
    ) -> TalkerHandle {
        spawn_interface_runner_observed(interface, current_config, None)
    }

    fn spawn_logging_interface_runner(
        interface: Box<dyn Interface>,
        current_config: Option<InterfaceConfig>,
    ) -> (
        TalkerHandle,
        crossbeam_channel::Receiver<crate::core::logging::LogEvent>,
    ) {
        let (log_tx, log_rx) = crossbeam_channel::unbounded();
        (
            spawn_interface_runner_observed(interface, current_config, Some(log_tx)),
            log_rx,
        )
    }

    fn spawn_interface_runner_observed(
        interface: Box<dyn Interface>,
        current_config: Option<InterfaceConfig>,
        log_tx: Option<crossbeam_channel::Sender<crate::core::logging::LogEvent>>,
    ) -> TalkerHandle {
        let schedule = Schedule::compile(&[msg("AB", 5)], Instant::now()).unwrap();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        let (control_tx, control_rx) = crossbeam_channel::bounded(16);
        let (status_tx, status_rx) = crossbeam_channel::bounded(256);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "test".into(),
            run_id: RunId::mint(),
        };
        let thread = std::thread::spawn(move || {
            let go = move || {
                run(
                    who,
                    interface,
                    current_config,
                    schedule,
                    cmd_rx,
                    RunnerObserver::new(status_tx, ObserverPolicy::every_send())
                        .with_control(control_tx),
                )
            };
            match log_tx {
                Some(tx) => crate::core::logging::with_gui_test_subscriber(tx, go),
                None => go(),
            }
        });
        TalkerHandle {
            cmd_tx,
            control_rx,
            status_rx,
            thread,
        }
    }

    #[test]
    fn recovery_reports_send_recovered_with_episode_counts() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let interface = Box::new(FlakyInterface {
            sent: Arc::clone(&sent),
            fail: Arc::clone(&fail),
        });
        let handle = spawn_interface_runner(interface, None);

        // Let the episode open (first failure) and some fires get suppressed,
        // then heal the interface: the next backoff retry closes the episode.
        std::thread::sleep(Duration::from_millis(50));
        fail.store(false, std::sync::atomic::Ordering::SeqCst);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut recovered = None;
        while recovered.is_none() {
            assert!(
                Instant::now() < deadline,
                "expected a SendRecovered after the interface healed"
            );
            for s in handle.status_rx.try_iter() {
                if let TalkerStatus::SendRecovered {
                    failures,
                    suppressed,
                    ..
                } = s
                {
                    recovered = Some((failures, suppressed));
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let (failures, suppressed) = recovered.unwrap();
        assert!(failures >= 1, "the reported first failure is counted");
        assert!(
            suppressed >= 1,
            "5 ms fires during the 250 ms backoff make no interface write"
        );
        assert!(
            !sent.lock().unwrap().is_empty(),
            "sending resumed after recovery"
        );
        let final_statuses = handle.status_rx.clone();
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
        let final_outcomes = final_statuses.try_iter().find_map(|status| match status {
            TalkerStatus::Counters {
                failed_sends,
                suppressed_sends,
                active_send_error,
                final_snapshot: true,
                ..
            } => Some((failed_sends, suppressed_sends, active_send_error)),
            _ => None,
        });
        let (failed_total, suppressed_total, active_send_error) =
            final_outcomes.expect("final cumulative counters");
        assert!(failed_total >= failures);
        assert!(suppressed_total >= suppressed);
        assert!(
            active_send_error.is_none(),
            "the final counter snapshot repairs a dropped recovery edge"
        );
    }

    #[test]
    fn a_recovery_line_states_what_the_episode_cost() {
        assert_eq!(
            recovery_cost(1, 0, 3),
            "1 failed send and 3 withheld while retrying"
        );
        assert_eq!(
            recovery_cost(2, 1, 0),
            "2 failed sends, 1 possibly partial send and 0 withheld while retrying"
        );
        assert_eq!(
            recovery_cost(0, 2, 5),
            "2 possibly partial sends and 5 withheld while retrying"
        );
    }

    /// §4.4, ADR-059: a write that fails after the interface accepted part of
    /// the message is its own outcome. Its bytes are wire bytes; it is not a
    /// complete message, not a failed send, and never resent.
    #[test]
    fn a_write_that_fails_part_way_is_possibly_partial() {
        struct CutsOff {
            sent: Arc<Mutex<Vec<Vec<u8>>>>,
            cut: Arc<std::sync::atomic::AtomicBool>,
        }

        impl Interface for CutsOff {
            fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
                if self.cut.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return Err(anyhow::anyhow!("connection reset").context(PartialWrite {
                        written: 1,
                        total: data.len(),
                    }));
                }
                self.sent.lock().unwrap().push(data.to_vec());
                Ok(())
            }
        }

        let sent = Arc::new(Mutex::new(Vec::new()));
        let cut = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (handle, logs) = spawn_logging_interface_runner(
            Box::new(CutsOff {
                sent: Arc::clone(&sent),
                cut: Arc::clone(&cut),
            }),
            None,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.lock().unwrap().len() < 2 {
            assert!(Instant::now() < deadline, "sending never resumed");
            std::thread::sleep(Duration::from_millis(5));
        }
        let statuses = handle.status_rx.clone();
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));

        let (count, bytes, failed, partial) = statuses
            .try_iter()
            .find_map(|status| match status {
                TalkerStatus::Counters {
                    total_count,
                    total_bytes,
                    failed_sends,
                    possibly_partial_sends,
                    final_snapshot: true,
                    ..
                } => Some((
                    total_count,
                    total_bytes,
                    failed_sends,
                    possibly_partial_sends,
                )),
                _ => None,
            })
            .expect("final counters");
        assert_eq!((failed, partial), (0, 1));
        assert_eq!(count, sent.lock().unwrap().len() as u64);
        // Each complete "AB" message is one byte; the cut-off one wrote 1.
        assert_eq!(bytes, count + 1, "the partial write's byte is a wire byte");

        let lines: Vec<String> = logs.try_iter().map(|event| event.message).collect();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("sending recovered after 1 possibly partial send and")),
            "{lines:#?}"
        );
    }

    /// ADR-059, §4.5: what a TCP peer sends is counted, and only a TCP
    /// client carries the count at all.
    #[test]
    fn a_tcp_peer_s_replies_are_counted_in_the_run() {
        use std::io::Write;
        use std::net::TcpListener;

        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = InterfaceConfig::TcpClient(crate::core::channel::TcpClientConfig::new(
            server.local_addr().unwrap(),
        ));
        let interface = config.open().unwrap();
        let (mut peer, _) = server.accept().unwrap();
        let handle = spawn_interface_runner(interface, Some(config));
        peer.write_all(b"OK\r\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut counted = None;
        while counted != Some(Some(4)) {
            assert!(
                Instant::now() < deadline,
                "the reply was never counted: {counted:?}"
            );
            for status in handle.status_rx.try_iter() {
                if let TalkerStatus::Counters { peer_bytes, .. } = status {
                    counted = Some(peer_bytes);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));

        assert_eq!(peer_sent(None, 9), None, "no transport, no peer count");
    }

    /// ADR-059, §4.5: the receiving server is killed and restarted on the
    /// same address. Sending resumes on a new connection without a click, and
    /// one line closes the episode, naming the address.
    #[test]
    fn a_restarted_tcp_server_gets_the_next_message_on_a_new_connection() {
        use std::io::Read;
        use std::net::TcpListener;

        fn accept_within(listener: &TcpListener, limit: Duration) -> std::net::TcpStream {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + limit;
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        return stream;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "talker never connected");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept failed: {e}"),
                }
            }
        }

        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let config =
            InterfaceConfig::TcpClient(crate::core::channel::TcpClientConfig::new(address));
        let interface = config.open().unwrap();
        let mut first = accept_within(&server, Duration::from_secs(5));
        let (handle, logs) = spawn_logging_interface_runner(interface, Some(config));
        let mut buf = [0u8; 2];
        first.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &[0xAB, 0xAB]);

        // Kill the server, and wait until talker sees a write fail.
        drop(first);
        drop(server);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "no write ever failed");
            if handle
                .status_rx
                .try_iter()
                .any(|s| matches!(s, TalkerStatus::ConnectionError { .. }))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // Restart it on the same address: the next message arrives there.
        let server = TcpListener::bind(address).expect("the port can be bound again");
        let mut second = accept_within(&server, Duration::from_secs(20));
        let mut buf = [0u8; 2];
        second.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &[0xAB, 0xAB], "whole messages on the new connection");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle
            .status_rx
            .try_iter()
            .any(|s| matches!(s, TalkerStatus::SendRecovered { .. }))
        {
            assert!(Instant::now() < deadline, "no recovery was reported");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));

        let recoveries: Vec<String> = logs
            .try_iter()
            .map(|event| event.message)
            .filter(|line| line.contains("sending recovered"))
            .collect();
        assert_eq!(recoveries.len(), 1, "{recoveries:#?}");
        assert!(
            recoveries[0].contains(&format!("reconnected to {address}; sending recovered")),
            "{recoveries:#?}"
        );
    }

    /// Model the serial failure mode where a removed device's old handle stays
    /// unusable after a replacement appears under the same port name. Recovery
    /// must prepare a replacement handle rather than assuming every transport
    /// can heal its existing one in place.
    #[test]
    fn a_replaced_device_reopens_before_the_backoff_retry() {
        struct ReplacementOnlyInterface {
            stale_handle: bool,
            reopen_attempts: Arc<std::sync::atomic::AtomicU64>,
            sent: Arc<Mutex<Vec<Vec<u8>>>>,
        }

        impl Interface for ReplacementOnlyInterface {
            fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
                anyhow::ensure!(!self.stale_handle, "removed device handle");
                self.sent.lock().unwrap().push(data.to_vec());
                Ok(())
            }

            fn prepare_retry(&mut self, current: Option<&InterfaceConfig>) -> anyhow::Result<bool> {
                anyhow::ensure!(
                    matches!(current, Some(InterfaceConfig::Serial(_))),
                    "serial configuration missing"
                );
                let attempt = self
                    .reopen_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                anyhow::ensure!(attempt > 1, "replacement device is absent");
                self.stale_handle = false;
                Ok(true)
            }
        }

        let reopen_attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let interface = Box::new(ReplacementOnlyInterface {
            stale_handle: true,
            reopen_attempts: Arc::clone(&reopen_attempts),
            sent: Arc::clone(&sent),
        });
        let (handle, log_rx) = spawn_logging_interface_runner(
            interface,
            Some(InterfaceConfig::Serial(SerialConfig::new("COM4"))),
        );

        // No command is sent to the runner. The fake makes the first open find
        // the device absent and the second find its replacement. Sequencing by
        // attempt keeps the backoff contract without racing the test thread's
        // scheduling against a two-second deadline.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut errors = 0;
        let mut active_errors = Vec::new();
        let mut recovered = None;
        while recovered.is_none() && Instant::now() < deadline {
            match handle.status_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(TalkerStatus::ConnectionError { .. }) => errors += 1,
                Ok(TalkerStatus::Counters {
                    active_send_error: Some(error),
                    ..
                }) => active_errors.push(error),
                Ok(TalkerStatus::SendRecovered {
                    failures,
                    suppressed,
                    ..
                }) => recovered = Some((failures, suppressed)),
                Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        let (failures, suppressed) = recovered.expect("replacement should resume the same run");
        assert_eq!(errors, 1, "one failure episode has one error edge");
        assert!(
            active_errors
                .iter()
                .any(|error| error.contains("replacement device is absent")),
            "counter snapshots should show the current reopen obstacle: {active_errors:#?}"
        );
        assert_eq!(
            failures, 1,
            "failed reopen attempts are withheld, not reported as failed writes"
        );
        assert!(
            suppressed >= 1,
            "backoff remains active while the port is absent"
        );
        assert_eq!(
            reopen_attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one failed reopen is followed by one successful replacement open"
        );
        assert!(!sent.lock().unwrap().is_empty(), "sending resumed");

        let final_statuses = handle.status_rx.clone();
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
        let (
            sent_total,
            failed_total,
            suppressed_total,
            render_samples,
            send_samples,
            message_render_samples,
            message_send_samples,
        ) = final_statuses
            .try_iter()
            .find_map(|status| match status {
                TalkerStatus::Counters {
                    total_count,
                    failed_sends,
                    suppressed_sends,
                    timing,
                    per_message_timing,
                    final_snapshot: true,
                    ..
                } => Some((
                    total_count,
                    failed_sends,
                    suppressed_sends,
                    timing.cumulative.render_duration.sample_count(),
                    timing.cumulative.send_duration.sample_count(),
                    per_message_timing
                        .first()
                        .map_or(0, |message| message.render_duration.sample_count()),
                    per_message_timing
                        .first()
                        .map_or(0, |message| message.send_duration.sample_count()),
                )),
                _ => None,
            })
            .expect("final cumulative counters");
        assert_eq!(failed_total, 1);
        assert!(suppressed_total >= suppressed);
        assert_eq!(
            render_samples,
            sent_total + failed_total,
            "reopen attempts are outside the render timing boundary"
        );
        assert_eq!(
            message_render_samples, render_samples,
            "failed reopens do not create per-message render samples"
        );
        assert_eq!(
            send_samples,
            sent_total + failed_total,
            "reopen attempts are outside the send-call timing boundary"
        );
        assert_eq!(
            message_send_samples, send_samples,
            "failed reopens do not create per-message send windows"
        );

        let recovery_lines = log_rx
            .try_iter()
            .filter(|event| {
                event.level == tracing::Level::INFO && event.message.contains("sending recovered")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            recovery_lines.len(),
            1,
            "one failure episode has one recovery log edge"
        );
        assert!(
            recovery_lines[0]
                .message
                .contains("reopened serial port COM4"),
            "the recovery edge distinguishes handle replacement: {recovery_lines:#?}"
        );
    }

    #[test]
    fn missing_serial_retry_configuration_is_an_internal_fault() {
        struct MissingConfigInterface;

        impl Interface for MissingConfigInterface {
            fn send(&mut self, _data: &[u8]) -> anyhow::Result<()> {
                anyhow::bail!("failed serial handle")
            }

            fn prepare_retry(
                &mut self,
                _current: Option<&InterfaceConfig>,
            ) -> anyhow::Result<bool> {
                Err(MissingRetryConfiguration.into())
            }
        }

        let (handle, log_rx) =
            spawn_logging_interface_runner(Box::new(MissingConfigInterface), None);
        let deadline = Instant::now() + Duration::from_secs(2);
        let internal = loop {
            assert!(
                Instant::now() < deadline,
                "missing retry configuration was not reported"
            );
            if let Ok(event) = log_rx.recv_timeout(Duration::from_millis(20)) {
                if event
                    .message
                    .contains("recovery had no confirmed interface settings")
                {
                    break event;
                }
            }
        };

        assert_eq!(internal.level, tracing::Level::ERROR);
        assert_eq!(
            internal.channel, None,
            "an internal state disagreement is not blamed on the channel"
        );
        assert!(internal.message.contains("Please report this"));

        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
    }

    fn free_tcp_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn a_channel_that_cannot_open_starts_once_its_port_appears() {
        // ADR-060: start what can start. The open is retried until the peer
        // is there, then the channel runs.
        let port = free_tcp_port();
        let cfg = InterfaceConfig::TcpClient(TcpClientConfig::new(
            format!("127.0.0.1:{port}").parse().unwrap(),
        ));
        let schedule = Schedule::compile(&[msg("AB", 100)], Instant::now()).unwrap();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<TalkerCommand>(8);
        let (status_tx, status_rx) = crossbeam_channel::bounded(64);
        let (control_tx, control_rx) = crossbeam_channel::bounded(8);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "1".into(),
            run_id: RunId::mint(),
        };
        let runner = std::thread::spawn(move || {
            open_retrying_and_run_with(
                who,
                cfg,
                schedule,
                cmd_rx,
                RunnerObserver::new(status_tx, ObserverPolicy::every_send())
                    .with_control(control_tx),
                Duration::from_millis(10),
                Duration::from_millis(20),
            )
        });
        match status_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(TalkerStatus::OpenFailed { .. }) => {}
            other => panic!("expected OpenFailed, got {:?}", other.is_ok()),
        }

        let peer = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let opened = loop {
            match control_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(RunnerControlStatus::InterfaceOpened { .. }) => break true,
                Ok(_) => continue,
                Err(_) => break false,
            }
        };
        assert!(
            opened,
            "the retry opened the channel once the peer was there"
        );
        cmd_tx.send(TalkerCommand::Stop).unwrap();
        runner.join().unwrap();
        drop(peer);
    }

    #[test]
    fn stop_ends_a_channel_still_waiting_to_open() {
        let port = free_tcp_port();
        let cfg = InterfaceConfig::TcpClient(TcpClientConfig::new(
            format!("127.0.0.1:{port}").parse().unwrap(),
        ));
        let schedule = Schedule::compile(&[msg("AB", 100)], Instant::now()).unwrap();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<TalkerCommand>(8);
        let (status_tx, status_rx) = crossbeam_channel::bounded(64);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "1".into(),
            run_id: RunId::mint(),
        };
        let runner = std::thread::spawn(move || {
            open_retrying_and_run_with(
                who,
                cfg,
                schedule,
                cmd_rx,
                RunnerObserver::new(status_tx, ObserverPolicy::every_send()),
                Duration::from_secs(60),
                Duration::from_secs(60),
            )
        });
        assert!(matches!(
            status_rx.recv_timeout(Duration::from_secs(5)),
            Ok(TalkerStatus::OpenFailed { .. })
        ));
        let started = Instant::now();
        cmd_tx.send(TalkerCommand::Stop).unwrap();
        runner.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "Stop does not wait out the retry"
        );
    }

    #[test]
    fn open_and_run_reports_open_failed() {
        // Port 1 on loopback refuses immediately on every platform we target.
        let cfg = InterfaceConfig::TcpClient(TcpClientConfig::new("127.0.0.1:1".parse().unwrap()));
        let schedule = Schedule::compile(&[msg("AB", 100)], Instant::now()).unwrap();
        let (_cmd_tx, cmd_rx) = crossbeam_channel::bounded::<TalkerCommand>(8);
        let (status_tx, status_rx) = crossbeam_channel::bounded(8);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "3".into(),
            run_id: RunId::mint(),
        };
        let id = who.id;
        open_and_run(
            who,
            cfg,
            schedule,
            cmd_rx,
            RunnerObserver::new(status_tx, ObserverPolicy::every_send()),
        );
        match status_rx.try_recv() {
            Ok(TalkerStatus::OpenFailed { channel, message }) => {
                assert_eq!(channel, id, "OpenFailed carries the stable id");
                assert!(message.contains("127.0.0.1:1"), "message was: {message}");
            }
            other => panic!("expected OpenFailed, got {:?}", other.is_ok()),
        }
    }
}
