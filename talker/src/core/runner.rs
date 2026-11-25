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

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError};
use wiredata_cli::doubling_retry;
use wiredata_ui::format::{compact_duration, interval as whole_ms, thousands};

use crate::core::{
    channel::{
        ChannelId, ControlLines, Interface, InterfaceConfig, MissingRetryConfiguration, OutputLine,
        PartialWrite,
    },
    internal_fault::InternalFaultTally,
    run_summary::{RunEndReason, RunId, RunSummary, RunTotals},
    scheduler::{ClockStep, Schedule, Tick},
    telemetry::{MessageTimingRecorder, SendTimingRecorder},
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
    /// A serial output line (ADR-070).
    Output(OutputLine),
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
    /// Set a serial output line on the open port, live; never reopens it
    /// (ADR-070).
    SetOutput {
        id: CommandId,
        line: OutputLine,
        high: bool,
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
        /// The run's cumulative outcomes. Boxed to keep this variant small;
        /// the final snapshot's value is the one the run summary carries.
        totals: Box<RunTotals>,
        /// Cumulative send-failure episodes. Repeated attempts inside one
        /// bounded-backoff episode do not increase this count.
        send_failure_episodes: u64,
        /// The current write or reopen error from the open failure episode.
        ///
        /// This repeats the immediate [`SendFailed`](Self::SendFailed)
        /// edge so a later counter snapshot repairs observer state if that edge
        /// was dropped. Later retry errors refresh the live explanation without
        /// opening another counted episode. `None` likewise repairs a dropped
        /// [`SendRecovered`](Self::SendRecovered) edge.
        active_send_error: Option<ActiveError>,
        /// Exact monotonic instant at which `totals.timing` was collapsed. This is
        /// snapshot provenance, not necessarily the newest sample time.
        captured_at: Instant,
        /// Whether this is the mandatory exact-at-rest snapshot emitted after
        /// the send loop ends. Finality travels with the measurement rather
        /// than being inferred from observer thread lifecycle.
        final_snapshot: bool,
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
    SendFailed { channel: ChannelId, message: String },
    /// Sending resumed after a failing episode. Carries the episode's cost so
    /// the observer can state what was lost: `failures` sends were attempted
    /// and failed (the first was reported as `SendFailed`), `withheld`
    /// due fires were skipped by the bounded-backoff retry policy without an
    /// interface write; some may have found that the handle could not yet be
    /// reopened.
    SendRecovered {
        channel: ChannelId,
        failures: u64,
        withheld: u64,
    },
    /// Opening the interface failed; the runner exits after sending this.
    OpenFailed { channel: ChannelId, message: String },
    /// The serial control lines (ADR-070): sent when they change and at
    /// least once a second while a serial port is open, so a dropped update
    /// repairs itself. `None` when they cannot be read, such as
    /// while an unplugged port waits to reopen.
    ControlLines {
        channel: ChannelId,
        lines: Option<ControlLines>,
    },
}

/// How often a serial channel's control lines are read.
const LINES_POLL: Duration = Duration::from_millis(100);
/// The longest a serial channel goes without reporting its control lines.
const LINES_REPEAT: Duration = Duration::from_secs(1);

/// First retry delay after a send failure (the **bounded-backoff** retry
/// policy): while an interface is failing, due fires are withheld — counted,
/// with no interface write — until the next retry instant. At that instant a
/// transport may first prepare or replace its failed handle; an unavailable
/// replacement withholds that fire too. Each unsuccessful retry doubles the wait up to
/// [`RETRY_BACKOFF_MAX`], and the first successful send closes the episode.
/// Without it, a 100 Hz schedule against a dead serial target would retry
/// 100 times a second.
pub const RETRY_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
/// Retry delay cap: a persistently dead interface is probed at most once per
/// this interval. Retrying stays automatic — there is no manual Retry state —
/// and transports that require a new OS handle can prepare it at that probe.
pub const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// How a runner reports to its observer (ADR-018): the named policy the owner
/// passes to [`run`]/[`open_and_run`]. Counters and payload samples are
/// **rate-limited lanes**; errors ([`TalkerStatus::SendFailed`] /
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

/// The obstacle a failing channel is stuck on, and which step met it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveError {
    /// The error with its context chain, as the UI shows it.
    pub message: String,
    /// Reopening the interface failed, not a write. Only a failed open is
    /// about whether the port is there at all, so only it is explained
    /// against the port list.
    pub reopening: bool,
}

impl ActiveError {
    fn write(message: String) -> Self {
        Self {
            message,
            reopening: false,
        }
    }

    fn reopen(message: String) -> Self {
        Self {
            message,
            reopening: true,
        }
    }
}

/// One failing episode: from the first failed send (reported) to the first
/// successful one (reported with these counts). See [`RETRY_BACKOFF_INITIAL`].
struct FailureEpisode {
    /// The obstacle currently preventing a successful write, repeated in
    /// counter snapshots until a successful write closes the episode. The
    /// episode's first failure remains in its edge-triggered log/status event;
    /// later retries update this live description without creating new edges.
    active_error: ActiveError,
    /// Sends attempted and failed, ≥ 1 (the reported first one), including
    /// those that were possibly partial.
    failures: u64,
    /// Of `failures`, those that failed after the interface accepted part of
    /// the message.
    possibly_partial: u64,
    /// Due fires withheld by the backoff gate without an interface write.
    withheld: u64,
    backoff: Duration,
    next_attempt: Instant,
    /// The last attempted handle failed and must be prepared before it is used
    /// again. A successful interface update clears this for its first send.
    handle_needs_recovery: bool,
    /// At least one preparation call replaced the OS handle during this
    /// episode. Retained across later failures so the one recovery edge can
    /// distinguish replacement from a transient write recovery.
    handle_replaced: bool,
    /// When the first send of the episode failed.
    since: Instant,
}

/// How long a channel must go without missing a scheduled send before its
/// off-cadence episode is closed.
///
/// Recovery is not "the first poll that missed nothing": the window has to
/// outlast an intermittent miss's cause, so it is measured in seconds. Five is
/// a settle time chosen within that range, not a proven threshold (ADR-053).
///
/// **It is a minimum, not a deadline.** The test runs only where the miss count
/// arrives, on a reached cadence point, so a channel is not woken to announce
/// its own recovery: a 15 s schedule reports at its next send, not at 5 s. A run
/// that stops first never reports one at all, which is why the end of the run
/// states the missed total instead.
const MISS_RECOVERY_SETTLE: Duration = Duration::from_secs(5);

/// One episode of a channel failing to keep its cadence: from the first missed
/// send (reported) to [`MISS_RECOVERY_SETTLE`] without another (reported with
/// these counts).
///
/// Edge-triggered (ADR-053): the first miss and the return to schedule each
/// get a line, and the count in between goes on the line that closes the
/// episode.
struct MissEpisode {
    /// Scheduled sends missed since the episode opened, ≥ 1.
    missed: u64,
    /// When the most recent miss was observed, for the settle test.
    last_miss: Instant,
    /// When the first miss was observed: the episode's start.
    since: Instant,
}

/// What one poll's miss count is worth reporting, if anything.
#[derive(Debug, PartialEq, Eq)]
enum MissReport {
    /// Either nothing was missed and nothing was open, or the episode is
    /// still running and this poll only added to it.
    Silent,
    /// The channel has just fallen off cadence. Carries this poll's own count,
    /// which is what is known at the moment it is reported.
    FellBehind(u64),
    /// The channel has held its cadence for [`MISS_RECOVERY_SETTLE`].
    BackOnSchedule {
        /// The episode's total.
        missed: u64,
        /// From the first miss to the last; the settle window is not in it.
        length: Duration,
    },
}

/// Fold one poll's miss count into the off-cadence episode.
///
/// Separated from the send loop so the edge behaviour can be exercised
/// directly: what has to hold is that a sustained overload logs twice and not
/// once per missed send, and that is a statement about this transition table
/// rather than about any particular run.
fn observe_misses(episode: &mut Option<MissEpisode>, missed: u64, now: Instant) -> MissReport {
    match (missed > 0, episode.as_mut()) {
        (true, Some(open)) => {
            open.missed = open.missed.saturating_add(missed);
            open.last_miss = now;
            MissReport::Silent
        }
        (true, None) => {
            *episode = Some(MissEpisode {
                missed,
                last_miss: now,
                since: now,
            });
            MissReport::FellBehind(missed)
        }
        (false, Some(open))
            if now.saturating_duration_since(open.last_miss) >= MISS_RECOVERY_SETTLE =>
        {
            let report = MissReport::BackOnSchedule {
                missed: open.missed,
                length: open.last_miss.saturating_duration_since(open.since),
            };
            *episode = None;
            report
        }
        _ => MissReport::Silent,
    }
}

/// "4 sends missed", with the count grouped in thousands.
fn sends_missed(count: u64) -> String {
    let sends = if count == 1 { "send" } else { "sends" };
    format!("{} {sends} missed", thousands(count))
}

/// The warning for the first missed send of an episode: which message, at
/// what interval, how late its send was handled, and how many points were lost.
/// `index` is the message's position in the schedule; people count from 1. The
/// interval is in milliseconds, as it is configured.
fn missed_line(
    label: &str,
    index: usize,
    interval: Duration,
    late: Duration,
    missed: u64,
) -> String {
    format!(
        "channel {label}: message {} ({} interval) was {} late; {}",
        index + 1,
        whole_ms(interval),
        compact_duration(late),
        sends_missed(missed)
    )
}

/// The line that closes an episode: the clean `quiet` stretch that ended it,
/// the sends it lost, and how long it lasted (left out for a single miss).
/// `quiet` is a fixed window, so it reads in whole units.
fn back_on_schedule_line(label: &str, quiet: Duration, missed: u64, length: Duration) -> String {
    let lasted = if length.is_zero() {
        String::new()
    } else {
        format!(" in {}", compact_duration(length))
    };
    format!(
        "channel {label}: on schedule again ({} without a miss); {}{lasted}",
        wiredata_cli::describe(quiet),
        sends_missed(missed)
    )
}

/// The line for a run that stops with an episode still open.
fn stopped_behind_line(label: &str, missed_this_run: u64) -> String {
    format!(
        "channel {label}: stopped while behind schedule; {} this run",
        sends_missed(missed_this_run)
    )
}

/// The warning for the first failed send of an episode. The error is the
/// reason; the retry schedule is the policy of [`RETRY_BACKOFF_INITIAL`] and
/// [`RETRY_BACKOFF_MAX`].
fn send_failed_line(label: &str, error: &str) -> String {
    format!(
        "channel {label}: send failed: {error}; {}",
        doubling_retry(RETRY_BACKOFF_INITIAL, RETRY_BACKOFF_MAX)
    )
}

/// The line that closes a failure episode. `down` is the time from the first
/// failed send to the first successful one; `via` names what was reopened or
/// reconnected, when the interface's handle was replaced.
fn recovery_line(
    label: &str,
    down: Duration,
    via: Option<&str>,
    failed: u64,
    possibly_partial: u64,
    withheld: u64,
) -> String {
    let via = via.map(|via| format!(" ({via})")).unwrap_or_default();
    format!(
        "channel {label}: sending again after {}{via}; {}",
        compact_duration(down),
        recovery_cost(failed, possibly_partial, withheld)
    )
}

/// The line for a step of the system clock that re-aligned the schedule, with
/// the step's size and direction signed.
fn clock_step_line(label: &str, step: ClockStep) -> String {
    let sign = if step.forward { '+' } else { '-' };
    format!(
        "channel {label}: system clock moved {sign}{}; sends re-aligned to UTC",
        compact_duration(step.by)
    )
}

/// The failure of an interface update whose replacement could not be opened.
/// The interface already in use was never touched, and the message says so.
fn replacement_open_failed(error: &anyhow::Error) -> String {
    format!(
        "the replacement interface could not be opened, so the interface already in use was \
         left unchanged: {error:#}"
    )
}

/// The warning for a refused Windows timer-resolution request.
fn timer_refused_line(label: &str) -> String {
    format!(
        "channel {label}: Windows refused {} timer resolution, so waits have about {} \
         granularity, and a send can be later than that when the computer is busy",
        whole_ms(timing::REQUESTED_TIMER_RESOLUTION),
        whole_ms(timing::WINDOWS_DEFAULT_TICK)
    )
}

/// The warning for the first status update dropped for a full receiver.
fn display_dropped_line(label: &str) -> String {
    format!("channel {label}: display updates dropped; sending is unaffected")
}

/// A failed open. `retry` is the first and longest wait of a retrying open;
/// `None` when the failure is final.
fn open_failed_line(label: &str, error: &str, retry: Option<(Duration, Duration)>) -> String {
    let retry = retry
        .map(|(first, max)| format!("; {}", doubling_retry(first, max)))
        .unwrap_or_default();
    format!("channel {label}: did not open: {error}{retry}")
}

/// Queue [`TalkerStatus::OpenFailed`], best-effort, and wake the owner.
fn emit_open_failed(who: &RunnerIdentity, observer: &RunnerObserver, message: String) {
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
                        "{}",
                        open_failed_line(&who.label, &message, Some((first, max)))
                    );
                } else {
                    tracing::debug!(
                        channel = who.id.as_u64(),
                        "channel {} still did not open (attempt {failures}): {message}",
                        who.label
                    );
                }
                emit_open_failed(&who, &observer, message);
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
/// open — real I/O that can block on a slow serial driver — never runs
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
            let message = format!("{e:#}");
            tracing::error!(
                channel = who.id.as_u64(),
                "{}",
                open_failed_line(&who.label, &message, None)
            );
            emit_open_failed(&who, &observer, message);
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
    /// transition at the start of a deadline window.
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
                "{}",
                timer_refused_line(&self.who.label)
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

/// The mutable state of one run, so each step of the loop can be read on its
/// own: command handling, one due fire, and the final delivery. The loop in
/// [`run_loop`] only decides which of them happens next.
struct RunState<'a> {
    who: &'a RunnerIdentity,
    interface: Box<dyn Interface>,
    current_config: Option<InterfaceConfig>,
    schedule: Schedule,
    control_tx: &'a Option<Sender<RunnerControlStatus>>,
    status_tx: &'a Sender<TalkerStatus>,
    notify: &'a Option<StatusNotify>,
    policy: ObserverPolicy,
    started_at: SystemTime,
    started_mono: Instant,
    total_count: u64,
    total_bytes: u64,
    failed_sends: u64,
    possibly_partial_sends: u64,
    send_failure_episodes: u64,
    withheld_sends: u64,
    send_timing: SendTimingRecorder,
    // Lane rate limits (ADR-018): `None` = nothing emitted yet, so the first
    // send always produces both a sample and counters (instant first paint).
    last_sample: Option<Instant>,
    // Sample rotation: the scheduler breaks grid ties by lowest index, so an
    // aligned multi-message schedule would sample message 0 forever if the
    // lane just took the first due send. Skip a repeat of the last sampled
    // message while due — but never longer than one full cycle, so a
    // single-active-message schedule still samples.
    last_sampled_index: Option<usize>,
    repeats_skipped_while_due: u64,
    // Per-message send counts, indexed by the message's position in the
    // compiled schedule. The resize is defensive; the schedule's size is
    // fixed at compile time.
    per_message_counts: Vec<u64>,
    // Per-message timing on the same index basis, including which message's
    // sends delayed the others (ADR-045).
    per_message_timing: MessageTimingRecorder,
    // Timer guard, policy-edge notification, counter invalidation, and dropped
    // observer accounting advance as one state machine. SetInterval re-evaluates
    // its policy on the next loop pass.
    timer: TimerReconciler<'a>,
    // The current failing episode, if any (bounded-backoff retry policy —
    // see [`RETRY_BACKOFF_INITIAL`]). `None` while sends are succeeding.
    episode: Option<FailureEpisode>,
    // The current off-cadence episode, if any. `None` while every scheduled
    // send is being reached. Independent of `episode` above: a channel can miss
    // its cadence with a perfectly healthy interface, and can fail every write
    // while keeping perfect cadence.
    miss_episode: Option<MissEpisode>,
    // Renders that returned nothing for an index the schedule handed out. Per
    // run rather than per slot, because a runner outlives no run but its own.
    render_faults: InternalFaultTally,
    // Missing confirmed configuration during handle replacement is a separate
    // invariant from rendering, so its first occurrence must not suppress the
    // render invariant's first report (ADR-054).
    retry_config_faults: InternalFaultTally,
    // When the control lines are next read. `None` unless the interface is
    // serial (ADR-070).
    lines_poll_at: Option<Instant>,
    // The control lines as last reported, and when.
    lines_reported: Option<(Option<ControlLines>, Instant)>,
}

/// When a run on `config` first reads its control lines: at once for a serial
/// port, never for anything else.
fn lines_poll_start(config: Option<&InterfaceConfig>, now: Instant) -> Option<Instant> {
    matches!(config, Some(InterfaceConfig::Serial(_))).then_some(now)
}

impl<'a> RunState<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        who: &'a RunnerIdentity,
        interface: Box<dyn Interface>,
        current_config: Option<InterfaceConfig>,
        schedule: Schedule,
        started_at: SystemTime,
        started_mono: Instant,
        control_tx: &'a Option<Sender<RunnerControlStatus>>,
        status_tx: &'a Sender<TalkerStatus>,
        notify: &'a Option<StatusNotify>,
        policy: ObserverPolicy,
    ) -> Self {
        let message_count = schedule.len();
        let lines_poll_at = lines_poll_start(current_config.as_ref(), Instant::now());
        Self {
            who,
            interface,
            current_config,
            schedule,
            control_tx,
            status_tx,
            notify,
            policy,
            started_at,
            started_mono,
            total_count: 0,
            total_bytes: 0,
            failed_sends: 0,
            possibly_partial_sends: 0,
            send_failure_episodes: 0,
            withheld_sends: 0,
            send_timing: SendTimingRecorder::default(),
            last_sample: None,
            last_sampled_index: None,
            repeats_skipped_while_due: 0,
            per_message_counts: vec![0; message_count],
            per_message_timing: MessageTimingRecorder::new(message_count),
            timer: TimerReconciler::new(who, status_tx, notify),
            episode: None,
            miss_episode: None,
            render_faults: InternalFaultTally::default(),
            retry_config_faults: InternalFaultTally::default(),
            lines_poll_at,
            lines_reported: None,
        }
    }

    /// Apply one queued command and report whether the run goes on.
    fn on_command(&mut self, cmd: TalkerCommand) -> Flow {
        match cmd {
            TalkerCommand::Stop => Flow::Stop,
            TalkerCommand::UpdateInterface { id, config } => {
                let execution = match self.current_config.as_ref() {
                    Some(current) => match self.interface.reconfigure(current, &config) {
                        Ok(true) => CommandExecution::Applied,
                        Ok(false) => match config.open() {
                            Ok(new) => {
                                self.interface = new;
                                CommandExecution::Applied
                            }
                            Err(e) => CommandExecution::Failed(replacement_open_failed(&e)),
                        },
                        Err(e) => CommandExecution::Failed(format!("{e:#}")),
                    },
                    None => match config.open() {
                        Ok(new) => {
                            self.interface = new;
                            CommandExecution::Applied
                        }
                        Err(e) => CommandExecution::Failed(replacement_open_failed(&e)),
                    },
                };
                match &execution {
                    CommandExecution::Applied => {
                        self.lines_poll_at = lines_poll_start(Some(&config), Instant::now());
                        self.current_config = Some(config);
                        // A fresh interface deserves an immediate attempt:
                        // pull the next retry forward. The episode's counts
                        // stay — only a successful send closes it (and
                        // reports what was lost).
                        if let Some(ep) = self.episode.as_mut() {
                            ep.next_attempt = Instant::now();
                            ep.handle_needs_recovery = false;
                            // The explicit update supersedes any automatic
                            // replacement earlier in this episode.
                            ep.handle_replaced = false;
                        }
                        tracing::info!(
                            channel = self.who.id.as_u64(),
                            "channel {} interface updated",
                            self.who.label
                        );
                    }
                    CommandExecution::Failed(message) => {
                        tracing::warn!(
                            channel = self.who.id.as_u64(),
                            "channel {} interface update failed: {message}",
                            self.who.label,
                        );
                    }
                }
                emit_control(
                    self.control_tx,
                    self.notify,
                    RunnerControlStatus::CommandCompleted {
                        channel: self.who.id,
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
                let execution = if self
                    .schedule
                    .set_interval(index, interval_ms, Instant::now())
                {
                    CommandExecution::Applied
                } else {
                    CommandExecution::Failed(format!(
                        "message {} is outside the {}-message schedule",
                        index + 1,
                        self.schedule.len()
                    ))
                };
                emit_control(
                    self.control_tx,
                    self.notify,
                    RunnerControlStatus::CommandCompleted {
                        channel: self.who.id,
                        id,
                        target: CommandTarget::MessageInterval(index),
                        execution,
                    },
                );
                Flow::Continue
            }
            TalkerCommand::SetOutput { id, line, high } => {
                let level = if high { "high" } else { "low" };
                let execution = match self.interface.set_output(line, high) {
                    Ok(()) => {
                        tracing::info!(
                            channel = self.who.id.as_u64(),
                            "channel {} set {} {level}",
                            self.who.label,
                            line.name()
                        );
                        // Show the new level at once rather than at the next read.
                        self.lines_poll_at = self.lines_poll_at.map(|_| Instant::now());
                        CommandExecution::Applied
                    }
                    Err(e) => {
                        let message = format!("{e:#}");
                        tracing::warn!(
                            channel = self.who.id.as_u64(),
                            "channel {} could not set {} {level}: {message}",
                            self.who.label,
                            line.name()
                        );
                        CommandExecution::Failed(message)
                    }
                };
                emit_control(
                    self.control_tx,
                    self.notify,
                    RunnerControlStatus::CommandCompleted {
                        channel: self.who.id,
                        id,
                        target: CommandTarget::Output(line),
                        execution,
                    },
                );
                Flow::Continue
            }
        }
    }

    /// When the control lines are next read, for the wait to wake at.
    fn lines_deadline(&self) -> Option<Instant> {
        self.lines_poll_at
    }

    /// Read a serial port's control lines when due, and report them when they
    /// changed or [`LINES_REPEAT`] has passed since the last report.
    fn poll_control_lines(&mut self) {
        let now = Instant::now();
        let Some(due) = self.lines_poll_at else {
            return;
        };
        if now < due {
            return;
        }
        self.lines_poll_at = Some(now + LINES_POLL);
        let lines = self.interface.control_lines();
        let report = match self.lines_reported {
            Some((last, at)) => last != lines || now.duration_since(at) >= LINES_REPEAT,
            None => true,
        };
        if report {
            self.lines_reported = Some((lines, now));
            self.timer.emit(TalkerStatus::ControlLines {
                channel: self.who.id,
                lines,
            });
        }
    }

    /// Everything that happens between applying commands and polling the
    /// schedule: the wall-clock check and the timer policy.
    fn reconcile_before_poll(&mut self) {
        if let Some(step) = self.schedule.reconcile_wall_clock(Instant::now()) {
            tracing::info!(
                channel = self.who.id.as_u64(),
                realignments = self.schedule.clock_realignments(),
                "{}",
                clock_step_line(&self.who.label, step)
            );
        }

        // Reconcile continuous intent only after applying queued interval
        // changes. Windowed intent is finalized once `poll` supplies the next
        // deadline; dormant/ordinary schedules release before they block.
        self.timer.reconcile_schedule(
            self.schedule.active_cadence(),
            self.schedule.cadence_alignment(),
            self.schedule.clock_realignments(),
        );
    }

    /// Handle one due fire: account for its lateness, apply the retry gate,
    /// render and send unless withheld, then emit counters if they are due.
    fn on_due(&mut self, index: usize, scheduled_for: Instant, interval: Duration, missed: u64) {
        // A bounded-window request has done its job once the deadline
        // wait returns. Rendering and clock reads do not benefit from
        // holding it through the send call.
        self.timer.before_due();
        let due_handled_at = Instant::now();
        self.send_timing.record_deadline_lateness(
            due_handled_at,
            due_handled_at.saturating_duration_since(scheduled_for),
        );
        self.per_message_timing
            .record_due(index, scheduled_for, due_handled_at);
        // Order matters: `record_due` settles which send this tick's
        // backlog belongs to, and the missed points belong to the
        // same one.
        self.per_message_timing
            .record_misses(index, scheduled_for, interval, missed);
        // Report falling off cadence and returning to it, and nothing
        // in between (ADR-053).
        match observe_misses(&mut self.miss_episode, missed, due_handled_at) {
            MissReport::FellBehind(now_missed) => tracing::warn!(
                channel = self.who.id.as_u64(),
                "{}",
                missed_line(
                    &self.who.label,
                    index,
                    interval,
                    due_handled_at.saturating_duration_since(scheduled_for),
                    now_missed
                )
            ),
            // A statement about cadence points only, not "back to normal":
            // a failure episode reports itself separately (ADR-053).
            MissReport::BackOnSchedule { missed, length } => tracing::info!(
                channel = self.who.id.as_u64(),
                "{}",
                back_on_schedule_line(&self.who.label, MISS_RECOVERY_SETTLE, missed, length)
            ),
            MissReport::Silent => {}
        }
        let backoff_withheld = self
            .episode
            .as_ref()
            .is_some_and(|ep| due_handled_at < ep.next_attempt);
        let needs_recovery = self
            .episode
            .as_ref()
            .is_some_and(|ep| ep.handle_needs_recovery);
        let mut reopen_withheld = false;
        if !backoff_withheld && needs_recovery {
            reopen_withheld = self.prepare_retry();
        }
        if backoff_withheld || reopen_withheld {
            // Backoff gate: this due fire is withheld — counted, but
            // not rendered or written. A retry-edge fire may have tried
            // to reopen the handle first. The scheduler has already
            // advanced, consistent with the stall policy (cadence over
            // count).
            if backoff_withheld {
                if let Some(ep) = self.episode.as_mut() {
                    ep.withheld += 1;
                }
            }
            self.withheld_sends += 1;
        } else if !self.render_and_send(index) {
            // Nothing was rendered, so there is nothing to count; the counters
            // lane waits for the next due fire.
            return;
        }

        self.emit_counters_if_due();
    }

    /// Try to bring a failed handle back before this due fire's send. Returns
    /// whether the fire is withheld because the reopen failed.
    fn prepare_retry(&mut self) -> bool {
        match self.interface.prepare_retry(self.current_config.as_ref()) {
            Ok(handle_replaced) => {
                // If the following send fails it sets this back to
                // true in the ordinary failure branch below.
                if let Some(ep) = self.episode.as_mut() {
                    ep.handle_needs_recovery = false;
                    ep.handle_replaced |= handle_replaced;
                }
                false
            }
            Err(e) => {
                // No interface write was attempted, so this due fire
                // is withheld rather than failed. It has no render or
                // send-call timing either; reopen work is outside
                // both measurement boundaries.
                if let Some(ep) = self.episode.as_mut() {
                    ep.active_error = ActiveError::reopen(format!("{e:#}"));
                    ep.withheld += 1;
                    ep.backoff = (ep.backoff * 2).min(RETRY_BACKOFF_MAX);
                    ep.next_attempt = Instant::now() + ep.backoff;
                    if e.downcast_ref::<MissingRetryConfiguration>().is_some() {
                        if let Some(line) = self.retry_config_faults.report(
                            &self.who.label,
                            "recovery had no confirmed interface settings",
                            "That send was withheld; automatic reopen cannot proceed \
                             until the interface is updated or the channel is restarted",
                        ) {
                            tracing::error!("{line}");
                        }
                    } else {
                        tracing::debug!(
                            channel = self.who.id.as_u64(),
                            "channel {} interface still unavailable ({} failed sends, \
                             {} withheld while retrying): {e:#}",
                            self.who.label,
                            ep.failures,
                            ep.withheld
                        );
                    }
                }
                true
            }
        }
    }

    /// Render and write one due message. Returns `false` when the schedule had
    /// nothing to render for `index`, which skips the rest of the due fire.
    fn render_and_send(&mut self, index: usize) -> bool {
        let render_started = Instant::now();
        let Some(payload) = self.schedule.render(index) else {
            // `poll` obtains this index from the same immutable-size
            // schedule. Stay panic-free if that invariant ever
            // changes — and report it as talker disagreeing with
            // itself rather than anything about the link (ADR-054).
            //
            // Tallied like the supervisor's: this sits on the send
            // path, so a broken invariant would otherwise emit a
            // line at every due point, which on a 10 ms cadence is
            // a hundred a second for as long as the run lasts.
            if let Some(line) = self.render_faults.report(
                &self.who.label,
                format!("message {} could not be built", index + 1),
                "That send was skipped; the channel is still running",
            ) {
                tracing::error!("{line}");
            }
            return false;
        };
        let render_finished = Instant::now();
        self.send_timing.record_render_duration(
            render_finished,
            render_finished.saturating_duration_since(render_started),
        );
        self.per_message_timing.record_render(
            index,
            render_finished.saturating_duration_since(render_started),
        );

        let send_started = Instant::now();
        let send_result = self.interface.send(&payload);
        let send_finished = Instant::now();
        self.send_timing.record_send_duration(
            send_finished,
            send_finished.saturating_duration_since(send_started),
        );
        // Recorded for a failed send too: a write that blocked and
        // then errored held the thread just as long.
        self.per_message_timing
            .record_send(index, send_started, send_finished);
        match send_result {
            Ok(()) => self.on_send_ok(index, payload, send_finished),
            Err(e) => self.on_send_failed(&e, send_finished),
        }
        true
    }

    fn on_send_ok(&mut self, index: usize, payload: Vec<u8>, send_finished: Instant) {
        if let Some(ep) = self.episode.take() {
            // "Withheld" is defined on the send-outcomes
            // line and nowhere the log reader can see, so
            // this states what happened to those sends.
            // A replaced handle names what was reopened.
            let replaced = match self.current_config.as_ref() {
                Some(InterfaceConfig::Serial(config)) if ep.handle_replaced => {
                    Some(format!("reopened serial port {}", config.port))
                }
                _ => None,
            };
            tracing::info!(
                channel = self.who.id.as_u64(),
                "{}",
                recovery_line(
                    &self.who.label,
                    send_finished.saturating_duration_since(ep.since),
                    replaced.as_deref(),
                    ep.failures - ep.possibly_partial,
                    ep.possibly_partial,
                    ep.withheld
                )
            );
            self.timer.emit(TalkerStatus::SendRecovered {
                channel: self.who.id,
                failures: ep.failures,
                withheld: ep.withheld,
            });
        }
        self.total_count += 1;
        self.total_bytes += payload.len() as u64;
        if index >= self.per_message_counts.len() {
            self.per_message_counts.resize(index + 1, 0);
        }
        self.per_message_counts[index] += 1;
        // The payload observer lane is rate-limited and
        // best-effort (ADR-018): a full receiver drops the
        // sample rather than backpressuring send cadence.
        let now = Instant::now();
        let due = self
            .last_sample
            .is_none_or(|t| now - t >= self.policy.sample_interval);
        if due {
            // A zero interval asks for every successful send, so the
            // repeat-suppression rotation does not apply.
            let repeat = !self.policy.sample_interval.is_zero()
                && self.last_sampled_index == Some(index)
                && self.schedule.len() > 1;
            if !repeat || self.repeats_skipped_while_due >= self.schedule.len() as u64 {
                self.last_sample = Some(now);
                self.last_sampled_index = Some(index);
                self.repeats_skipped_while_due = 0;
                let replacement_wire_offsets =
                    self.schedule.replacement_wire_offsets(index).to_vec();
                self.timer.emit(TalkerStatus::SendSample {
                    channel: self.who.id,
                    message_index: index,
                    payload,
                    replacement_wire_offsets,
                });
            } else {
                self.repeats_skipped_while_due += 1;
            }
        }
    }

    fn on_send_failed(&mut self, e: &anyhow::Error, send_finished: Instant) {
        // A write that failed after the interface accepted
        // part of the message is possibly partial (§4.4,
        // ADR-059). Its bytes count as wire bytes; the
        // message is never resent.
        let written = e
            .downcast_ref::<PartialWrite>()
            .map(|partial| partial.written);
        match written {
            Some(written) => {
                self.possibly_partial_sends += 1;
                self.total_bytes += written as u64;
            }
            None => self.failed_sends += 1,
        }
        let partial = u64::from(written.is_some());
        match self.episode.as_mut() {
            // Edge-triggered: only the episode's first failure is
            // reported (warn + `SendFailed`); it opens the episode.
            None => {
                self.send_failure_episodes += 1;
                let message = format!("{e:#}");
                tracing::warn!(
                    channel = self.who.id.as_u64(),
                    "{}",
                    send_failed_line(&self.who.label, &message)
                );
                self.episode = Some(FailureEpisode {
                    active_error: ActiveError::write(message.clone()),
                    failures: 1,
                    possibly_partial: partial,
                    withheld: 0,
                    backoff: RETRY_BACKOFF_INITIAL,
                    next_attempt: Instant::now() + RETRY_BACKOFF_INITIAL,
                    handle_needs_recovery: true,
                    handle_replaced: false,
                    since: send_finished,
                });
                self.timer.emit(TalkerStatus::SendFailed {
                    channel: self.who.id,
                    message,
                });
            }
            // A failed retry deepens the backoff; no re-report.
            Some(ep) => {
                ep.active_error = ActiveError::write(format!("{e:#}"));
                ep.failures += 1;
                ep.possibly_partial += partial;
                ep.handle_needs_recovery = true;
                ep.backoff = (ep.backoff * 2).min(RETRY_BACKOFF_MAX);
                ep.next_attempt = Instant::now() + ep.backoff;
                tracing::debug!(
                    channel = self.who.id.as_u64(),
                    "channel {} send still failing ({} failures so far): {e:#}",
                    self.who.label,
                    ep.failures
                );
            }
        }
    }

    /// Cumulative outcomes remain observable even when every due
    /// send is failing or withheld. This lane is rate-limited
    /// and best-effort, so it cannot slow the scheduler hot path.
    fn emit_counters_if_due(&mut self) {
        let now = Instant::now();
        if self.timer.counters_due(now, self.policy.counter_interval) {
            let drops_so_far = self.timer.dropped_statuses();
            self.per_message_timing
                .set_schedule(self.schedule.message_demand());
            let status = TalkerStatus::Counters {
                channel: self.who.id,
                totals: Box::new(RunTotals {
                    total_count: self.total_count,
                    total_bytes: self.total_bytes,
                    per_message_counts: self.per_message_counts.clone(),
                    per_message_timing: self.per_message_timing.snapshot(),
                    dropped_statuses: drops_so_far,
                    missed_sends: self.schedule.missed_sends(),
                    failed_sends: self.failed_sends,
                    possibly_partial_sends: self.possibly_partial_sends,
                    withheld_sends: self.withheld_sends,
                    timing: self.send_timing.snapshot_at(now),
                    timer: self.timer.status(),
                }),
                send_failure_episodes: self.send_failure_episodes,
                active_send_error: self.episode.as_ref().map(|ep| ep.active_error.clone()),
                captured_at: now,
                final_snapshot: false,
            };
            self.timer.emit(status);
        }
    }

    /// Select the deadline the next interruptible wait should end at.
    fn prepare_wait(&mut self, until: Instant) -> Instant {
        self.timer.prepare_wait(Instant::now(), until)
    }

    /// Deliver the run's end: release the timer, log the final cadence state,
    /// then send the exact final counters and the run summary.
    fn finish(mut self, end_reason: RunEndReason) {
        // Stop paying the platform timer-resolution cost before the exact final
        // status delivery, which may briefly wait for a full observer queue.
        let (timer_status, dropped_statuses) = self.timer.finish();

        // Final counters (ADR-018): the rate-limited lane can be up to one
        // interval stale when the runner stops — emit once more so the observer's
        // totals are exact at rest. A **blocking** send, deliberately: the runner
        // is exiting so cadence no longer matters, and this is the one status
        // that must not be lost to a momentarily full queue ("exact at rest" is
        // a promise, not best-effort). It waits only while the owner neither
        // drains nor drops the receiver; every owner keeps draining a stopped
        // runner's receiver until the thread exits (supervisor `poll`/`join_all`,
        // the CLI's funnel loop), and a dropped receiver returns an error
        // immediately.
        let finished_mono = Instant::now();
        let finished_at = SystemTime::now();
        let missed_sends = self.schedule.missed_sends();
        // A run that stops mid-episode never reaches a settle window, so the log
        // would otherwise end on the warning with no total against it. The run's
        // own figure is stated rather than the episode's: it is exact, channel-wide,
        // and the same number the completed-run summary carries.
        if self.miss_episode.is_some() {
            tracing::warn!(
                channel = self.who.id.as_u64(),
                "{}",
                stopped_behind_line(&self.who.label, missed_sends)
            );
        }
        self.per_message_timing
            .set_schedule(self.schedule.message_demand());
        let totals = RunTotals {
            total_count: self.total_count,
            total_bytes: self.total_bytes,
            per_message_counts: self.per_message_counts,
            per_message_timing: self.per_message_timing.snapshot(),
            dropped_statuses,
            missed_sends,
            failed_sends: self.failed_sends,
            possibly_partial_sends: self.possibly_partial_sends,
            withheld_sends: self.withheld_sends,
            timing: self.send_timing.snapshot_at(finished_mono),
            timer: timer_status,
        };
        let _ = self.status_tx.send(TalkerStatus::Counters {
            channel: self.who.id,
            totals: Box::new(totals.clone()),
            send_failure_episodes: self.send_failure_episodes,
            active_send_error: self.episode.as_ref().map(|ep| ep.active_error.clone()),
            captured_at: finished_mono,
            final_snapshot: true,
        });
        if let Some(n) = self.notify {
            n();
        }
        emit_control(
            self.control_tx,
            self.notify,
            RunnerControlStatus::RunFinished {
                channel: self.who.id,
                summary: Box::new(RunSummary {
                    run_id: self.who.run_id,
                    channel: self.who.id,
                    label: self.who.label.clone(),
                    started_at: self.started_at,
                    finished_at,
                    elapsed: finished_mono.saturating_duration_since(self.started_mono),
                    end_reason,
                    totals,
                }),
            },
        );
    }
}

fn run_loop(
    who: &RunnerIdentity,
    interface: Box<dyn Interface>,
    current_config: Option<InterfaceConfig>,
    mut schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
) {
    // Cadence starts only now: any profile preflight, predecessor join, or
    // serial open happened before this runner boundary and must not
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
    let mut run = RunState::new(
        who,
        interface,
        current_config,
        schedule,
        started_at,
        started_mono,
        &control_tx,
        &status_tx,
        &notify,
        policy,
    );

    let end_reason = 'run: loop {
        // Drain anything already queued so back-to-back sends can't starve
        // command handling.
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if let Flow::Stop = run.on_command(cmd) {
                        break 'run RunEndReason::StopCommand;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break 'run RunEndReason::OwnerDisconnected,
            }
        }

        run.poll_control_lines();

        run.reconcile_before_poll();

        let poll_at = Instant::now();
        match run.schedule.poll(poll_at) {
            Tick::Due {
                index,
                scheduled_for,
                interval,
                missed,
            } => run.on_due(index, scheduled_for, interval, missed),
            // Nothing due yet: block on the command channel until the next
            // fire deadline. Wakes instantly for a command, exactly on time
            // for the schedule, and detects a dropped handle.
            Tick::Wait(until) => {
                // A serial port's lines are read while waiting too, so they
                // stay current however far apart the sends are.
                let wait_until = run.prepare_wait(until);
                let wait_until = run
                    .lines_deadline()
                    .map_or(wait_until, |lines| lines.min(wait_until));
                match cmd_rx.recv_deadline(wait_until) {
                    Ok(cmd) => {
                        if let Flow::Stop = run.on_command(cmd) {
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
            // so block indefinitely — zero wakeups — except to read a serial
            // port's control lines.
            Tick::Idle => {
                let next = match run.lines_deadline() {
                    Some(lines) => cmd_rx.recv_deadline(lines),
                    None => cmd_rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                };
                match next {
                    Ok(cmd) => {
                        if let Flow::Stop = run.on_command(cmd) {
                            break 'run RunEndReason::StopCommand;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        break 'run RunEndReason::OwnerDisconnected;
                    }
                }
            }
        }
    };

    run.finish(end_reason);
}

/// What a closed failure episode cost, for its recovery line: the sends that
/// failed, those that were possibly partial, and the due sends withheld.
fn recovery_cost(failed: u64, possibly_partial: u64, withheld: u64) -> String {
    let mut cost = Vec::new();
    if failed > 0 || possibly_partial == 0 {
        cost.push(format!("{failed} failed"));
    }
    if possibly_partial > 0 {
        cost.push(format!("{possibly_partial} possibly partial"));
    }
    cost.push(format!("{withheld} withheld"));
    cost.join(", ")
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
            // The warning states the consequence, display freshness, not the
            // internal queue, which the reader cannot see and which never
            // affects the send path.
            if *dropped_statuses == 1 {
                tracing::warn!(
                    channel = who.id.as_u64(),
                    "{}",
                    display_dropped_line(&who.label)
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
    use crate::core::channel::{SerialConfig, UdpConfig};
    use crate::core::message::{CodePage, MessageConfig, PayloadConfig};

    /// Sustained overload must log twice, not once per missed send. This is
    /// the whole reason the episode exists: misses concentrate on the shortest
    /// interval, so a line each would flood the log with the fault's own
    /// symptom at the moment the log is least able to carry it.
    #[test]
    fn falling_off_cadence_reports_the_edges_and_not_every_missed_send() {
        let start = Instant::now();
        let mut episode = None;

        // The first miss is the one the reader could have acted on.
        assert_eq!(
            observe_misses(&mut episode, 3, start),
            MissReport::FellBehind(3)
        );

        // A thousand polls of continuing overload say nothing further, and the
        // count accrues for the line that closes the episode.
        for tick in 1..=1_000 {
            assert_eq!(
                observe_misses(&mut episode, 2, start + Duration::from_millis(tick)),
                MissReport::Silent,
                "a continuing episode must not log again at tick {tick}"
            );
        }

        // A clean poll inside the settle window is not yet a recovery: a
        // marginal channel alternates, and reporting each clean tick would
        // produce the same flood with two lines instead of one.
        assert_eq!(
            observe_misses(&mut episode, 0, start + MISS_RECOVERY_SETTLE),
            MissReport::Silent
        );

        // Settle is measured from the last miss, not from the episode's start.
        let last_miss = start + Duration::from_millis(1_000);
        assert_eq!(
            observe_misses(&mut episode, 0, last_miss + MISS_RECOVERY_SETTLE),
            MissReport::BackOnSchedule {
                missed: 3 + 2 * 1_000,
                length: Duration::from_millis(1_000),
            },
            "the length runs from the first miss to the last, not through the settle window"
        );
        assert!(episode.is_none(), "recovery closes the episode");

        // And a quiet channel stays quiet.
        assert_eq!(
            observe_misses(&mut episode, 0, last_miss + MISS_RECOVERY_SETTLE * 10),
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

        observe_misses(&mut episode, 5, start);
        assert_eq!(
            observe_misses(&mut episode, 0, start + MISS_RECOVERY_SETTLE),
            MissReport::BackOnSchedule {
                missed: 5,
                length: Duration::ZERO,
            }
        );

        let later = start + MISS_RECOVERY_SETTLE * 4;
        assert_eq!(
            observe_misses(&mut episode, 1, later),
            MissReport::FellBehind(1)
        );
        assert_eq!(
            observe_misses(&mut episode, 0, later + MISS_RECOVERY_SETTLE),
            MissReport::BackOnSchedule {
                missed: 1,
                length: Duration::ZERO,
            },
            "the second episode must not carry the first one's total"
        );
    }

    #[test]
    fn the_missed_send_line_states_the_message_its_interval_and_how_late() {
        assert_eq!(
            missed_line(
                "GPS",
                1,
                Duration::from_millis(10),
                Duration::from_millis(47),
                4
            ),
            "channel GPS: message 2 (10 ms interval) was 47 ms late; 4 sends missed"
        );
        assert_eq!(
            missed_line(
                "GPS",
                0,
                Duration::from_secs(2),
                Duration::from_micros(1_500),
                1
            ),
            "channel GPS: message 1 (2,000 ms interval) was 1.50 ms late; 1 send missed"
        );
    }

    #[test]
    fn the_back_on_schedule_line_states_the_settle_window_and_the_episode() {
        assert_eq!(
            back_on_schedule_line("GPS", MISS_RECOVERY_SETTLE, 1_204, Duration::from_secs(12)),
            "channel GPS: on schedule again (5 s without a miss); 1,204 sends missed in 12.0 s"
        );
        assert_eq!(
            back_on_schedule_line("GPS", MISS_RECOVERY_SETTLE, 1, Duration::ZERO),
            "channel GPS: on schedule again (5 s without a miss); 1 send missed",
            "an episode of one miss has no length to state"
        );
    }

    #[test]
    fn the_stopped_behind_schedule_line_states_the_runs_total() {
        assert_eq!(
            stopped_behind_line("GPS", 5_415),
            "channel GPS: stopped while behind schedule; 5,415 sends missed this run"
        );
    }

    #[test]
    fn the_send_failed_line_states_the_retry_schedule() {
        assert_eq!(
            send_failed_line("GPS", "writing to serial port: device removed"),
            "channel GPS: send failed: writing to serial port: device removed; \
             retrying in 250 ms, doubling to 5 s"
        );
    }

    #[test]
    fn the_recovery_line_states_how_long_sending_was_down() {
        assert_eq!(
            recovery_line(
                "GPS",
                Duration::from_millis(1_300),
                Some("reconnected to 10.0.0.5:4001"),
                1,
                0,
                13
            ),
            "channel GPS: sending again after 1.30 s (reconnected to 10.0.0.5:4001); \
             1 failed, 13 withheld"
        );
        assert_eq!(
            recovery_line("GPS", Duration::from_millis(40), None, 2, 1, 0),
            "channel GPS: sending again after 40 ms; 2 failed, 1 possibly partial, 0 withheld"
        );
    }

    #[test]
    fn the_clock_step_line_gives_the_size_and_direction_of_the_step() {
        let step = |by_ms, forward| ClockStep {
            by: Duration::from_millis(by_ms),
            forward,
        };
        assert_eq!(
            clock_step_line("GPS", step(1_200, true)),
            "channel GPS: system clock moved +1.20 s; sends re-aligned to UTC"
        );
        assert_eq!(
            clock_step_line("GPS", step(350, false)),
            "channel GPS: system clock moved -350 ms; sends re-aligned to UTC"
        );
    }

    #[test]
    fn the_timer_refused_line_states_the_granularity_and_not_a_lateness_limit() {
        assert_eq!(
            timer_refused_line("GPS"),
            "channel GPS: Windows refused 1 ms timer resolution, so waits have about 16 ms \
             granularity, and a send can be later than that when the computer is busy"
        );
    }

    #[test]
    fn the_display_lag_line_says_sending_is_unaffected() {
        assert_eq!(
            display_dropped_line("GPS"),
            "channel GPS: display updates dropped; sending is unaffected"
        );
    }

    #[test]
    fn an_open_failure_is_worded_the_same_with_and_without_a_retry() {
        assert_eq!(
            open_failed_line("GPS", "no device is present at COM4", None),
            "channel GPS: did not open: no device is present at COM4"
        );
        assert_eq!(
            open_failed_line(
                "GPS",
                "no device is present at COM4",
                Some((OPEN_RETRY_FIRST, OPEN_RETRY_MAX))
            ),
            "channel GPS: did not open: no device is present at COM4; \
             retrying in 1 s, doubling to 30 s"
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
    /// transition table: overload that misses hundreds of cadence points must
    /// still produce one line, and a run stopped mid-episode must not leave the
    /// warning unanswered.
    ///
    /// 120 ms writes against a 10 ms cadence miss at least a dozen points per
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

        // Two completed slow writes guarantee missed points between them.
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.lock().unwrap().len() < 2 {
            assert!(Instant::now() < deadline, "the slow interface never sent");
            std::thread::sleep(Duration::from_millis(10));
        }
        wait_for_line(&log_rx, &mut lines, "missed");

        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(5));
        wait_for_line(&log_rx, &mut lines, "stopped while behind schedule");

        let fell_behind = lines
            .iter()
            .filter(|line| line.contains("was ") && line.contains(" late; "))
            .count();
        assert_eq!(
            fell_behind, 1,
            "the episode opens once, however many points are missed: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("no further")),
            "five seconds never elapsed without a miss, so nothing recovered: {lines:#?}"
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
        // A loaded machine can genuinely delay the thread past a deadline, and
        // then a miss is real and must be reported. What must never happen is
        // a failing send reported as a miss: so every miss line has to show
        // lateness of at least one whole interval, and "stopped while behind
        // schedule" may follow only a real miss.
        let late_by = |line: &str| -> Option<Duration> {
            let value = line.split("was ").nth(1)?.split(" late").next()?;
            let (number, unit) = value.split_once(' ')?;
            let number: f64 = number.replace(',', "").parse().ok()?;
            match unit {
                "ms" => Some(Duration::from_secs_f64(number / 1000.0)),
                "s" => Some(Duration::from_secs_f64(number)),
                _ => None,
            }
        };
        let mut real_misses = 0;
        for line in lines.iter().filter(|line| line.contains("missed")) {
            if line.contains("stopped while behind schedule") {
                assert!(
                    real_misses > 0,
                    "a stop reported misses that never happened: {lines:#?}"
                );
            } else {
                let late = late_by(line)
                    .unwrap_or_else(|| panic!("a miss line without its lateness: {line}"));
                assert!(
                    late >= Duration::from_millis(200),
                    "failing sends are not missed sends: {lines:#?}"
                );
                real_misses += 1;
            }
        }
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
                    totals,
                    final_snapshot: true,
                    ..
                } = status
                {
                    final_timing = Some(totals.per_message_timing);
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
            "missed points were not charged to the blocking send: {slow:?}"
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
        assert_eq!(
            timer.status().intent,
            timing::TimerIntent::ContinuousHighRate
        );
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
        assert_eq!(timer.status().mode, timing::TimerMode::NoRequest);
        assert_eq!((timer.guard_acquisitions, timer.guard_releases), (1, 1));

        timer.reconcile_schedule(one_cadence(50), timing::CadenceAlignment::Immediate, 0);
        assert_eq!(timer.intent, timing::TimerIntent::PrecisionWindow);
        assert_eq!(timer.status().intent, timing::TimerIntent::PrecisionWindow);
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
                    totals,
                    final_snapshot,
                    ..
                } => {
                    let RunTotals {
                        total_count,
                        total_bytes,
                        per_message_counts,
                        dropped_statuses,
                        ..
                    } = &**totals;
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
                    assert_ne!(status.mode, timing::TimerMode::NoRequest);
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
                    totals,
                    final_snapshot: true,
                    ..
                } => Some(totals),
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
        // One value, built once: every total the two carry is the same.
        assert_eq!(summary.totals, *final_counters);
        assert_eq!(
            summary.totals.total_count,
            sent.lock().unwrap().len() as u64
        );
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
                TalkerStatus::Counters { totals, .. } => last_total = totals.total_count,
                TalkerStatus::TimerStatus { .. } => {}
                _ => panic!("unexpected status variant"),
            }
        }
        assert_eq!(samples, 1, "one payload sample regardless of send count");
        assert_eq!(last_total as usize, sent.lock().unwrap().len());
    }

    #[test]
    fn every_send_policy_samples_every_send_with_a_dormant_message_present() {
        let (sent, handle, _id) = spawn_runner(&[msg("AA", 1), msg("BB", 0)], false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.lock().unwrap().len() < 10 {
            assert!(Instant::now() < deadline, "expected 10 sends within 5 s");
            std::thread::sleep(Duration::from_millis(1));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));

        let samples = status_rx
            .try_iter()
            .filter(|s| matches!(s, TalkerStatus::SendSample { .. }))
            .count();
        let writes = sent.lock().unwrap().len();
        assert!(writes >= 10);
        assert_eq!(samples, writes, "every successful send must be sampled");
    }

    /// An interface whose every write overruns the schedule's interval.
    struct AlwaysSlowInterface {
        writes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Interface for AlwaysSlowInterface {
        fn send(&mut self, _data: &[u8]) -> anyhow::Result<()> {
            let n = self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            // A runner that never notices its owner is gone would write for
            // ever; end the test thread rather than leak it.
            assert!(n < 100, "runner kept sending after its owner disconnected");
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        }
    }

    #[test]
    fn a_permanently_overloaded_runner_still_notices_a_dropped_owner() {
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let interface = Box::new(AlwaysSlowInterface {
            writes: Arc::clone(&writes),
        });
        let schedule = Schedule::compile(&[msg("AB", 1)], Instant::now()).unwrap();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        let (control_tx, _control_rx) = crossbeam_channel::bounded(16);
        let (status_tx, _status_rx) = crossbeam_channel::bounded(256);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "test".into(),
            run_id: RunId::mint(),
        };
        drop(cmd_tx);
        let thread = std::thread::spawn(move || {
            run(
                who,
                interface,
                None,
                schedule,
                cmd_rx,
                RunnerObserver::new(status_tx, ObserverPolicy::every_send())
                    .with_control(control_tx),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !thread.is_finished() {
            assert!(Instant::now() < deadline, "runner did not stop in time");
            std::thread::sleep(Duration::from_millis(5));
        }
        thread.join().unwrap();
        assert!(
            writes.load(std::sync::atomic::Ordering::SeqCst) <= 2,
            "a disconnected owner must end the run within a pass or two"
        );
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
        assert_eq!(dormant.unwrap().mode, timing::TimerMode::NoRequest);

        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(1));
    }

    #[test]
    fn a_slow_schedule_uses_windowed_policy_and_dormant_releases_it() {
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
                    if status.intent == timing::TimerIntent::PrecisionWindow {
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
                "deadline-window status not reported"
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
                "dormant policy not reported before idle"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let dormant = dormant.unwrap();
        assert_eq!(dormant.intent, timing::TimerIntent::None);
        assert_eq!(dormant.mode, timing::TimerMode::NoRequest);

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
    fn send_failure_reports_send_failed_and_keeps_running() {
        let (_, handle, id) = spawn_runner(&[msg("AB", 10)], true);
        // Wait on the status itself; the timeout is only a failure guard.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut saw_error = false;
        while !saw_error {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match handle.status_rx.recv_timeout(remaining) {
                Ok(TalkerStatus::SendFailed { channel, message }) => {
                    assert_eq!(channel, id, "errors carry the stable id");
                    assert!(message.contains("mock send failure"));
                    saw_error = true;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_error, "expected at least one SendFailed");
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
        // edge trigger every retry could emit a SendFailed; with it,
        // exactly one is emitted for the whole failure episode.
        let (_, handle, _id) = spawn_runner(&[msg("AB", 5)], true);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut statuses = Vec::new();
        loop {
            statuses.extend(handle.status_rx.try_iter());
            let saw_live_outcomes = statuses.iter().any(|status| {
                matches!(
                    status,
                    TalkerStatus::Counters { totals, .. }
                        if totals.failed_sends >= 1 && totals.withheld_sends >= 1
                )
            });
            if saw_live_outcomes {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "failed and withheld counters did not become live"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let errors = statuses
            .iter()
            .filter(|s| matches!(s, TalkerStatus::SendFailed { .. }))
            .count();
        assert_eq!(
            errors, 1,
            "edge-triggered: only the episode's first failure is reported"
        );
        let outcomes = statuses.iter().rev().find_map(|status| match status {
            TalkerStatus::Counters {
                totals,
                active_send_error,
                ..
            } => Some((
                totals.total_count,
                totals.failed_sends,
                totals.withheld_sends,
                active_send_error.clone(),
                totals.timing,
            )),
            _ => None,
        });
        let (sent, failed, withheld, active_send_error, timing) =
            outcomes.expect("live cumulative counters");
        let cumulative = timing.cumulative;
        assert_eq!(sent, 0);
        assert!(failed >= 1, "the failed attempt is visible before stop");
        assert!(
            active_send_error.is_some_and(
                |error| !error.reopening && error.message.contains("mock send failure")
            ),
            "counter snapshots repeat the open failure episode, as a failed write"
        );
        assert!(
            withheld >= 1,
            "backoff-withheld sends are visible before stop"
        );
        assert_eq!(
            cumulative.deadline_lateness.sample_count(),
            sent + failed + withheld,
            "every handled due fire contributes one deadline sample"
        );
        assert_eq!(
            cumulative.render_duration.sample_count(),
            sent + failed,
            "withheld due fires must not render payloads"
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

        // Wait until the episode is open (first failure) and a fire has been
        // withheld, as the live counters say, then heal the interface: the
        // next backoff retry closes the episode. The deadline is only a
        // failure guard.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut healed = false;
        let mut recovered = None;
        while recovered.is_none() {
            assert!(
                Instant::now() < deadline,
                "expected a SendRecovered after the interface healed"
            );
            for s in handle.status_rx.try_iter() {
                match s {
                    TalkerStatus::Counters { totals, .. }
                        if !healed && totals.failed_sends >= 1 && totals.withheld_sends >= 1 =>
                    {
                        fail.store(false, std::sync::atomic::Ordering::SeqCst);
                        healed = true;
                    }
                    TalkerStatus::SendRecovered {
                        failures, withheld, ..
                    } => recovered = Some((failures, withheld)),
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(healed, "recovery cannot precede the heal");
        let (failures, withheld) = recovered.unwrap();
        assert!(failures >= 1, "the reported first failure is counted");
        assert!(
            withheld >= 1,
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
                totals,
                active_send_error,
                final_snapshot: true,
                ..
            } => Some((
                totals.failed_sends,
                totals.withheld_sends,
                active_send_error,
            )),
            _ => None,
        });
        let (failed_total, withheld_total, active_send_error) =
            final_outcomes.expect("final cumulative counters");
        assert!(failed_total >= failures);
        assert!(withheld_total >= withheld);
        assert!(
            active_send_error.is_none(),
            "the final counter snapshot repairs a dropped recovery edge"
        );
    }

    #[test]
    fn a_recovery_line_states_what_the_episode_cost() {
        assert_eq!(recovery_cost(1, 0, 3), "1 failed, 3 withheld");
        assert_eq!(
            recovery_cost(2, 1, 0),
            "2 failed, 1 possibly partial, 0 withheld"
        );
        assert_eq!(recovery_cost(0, 2, 5), "2 possibly partial, 5 withheld");
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
                    totals,
                    final_snapshot: true,
                    ..
                } => Some((
                    totals.total_count,
                    totals.total_bytes,
                    totals.failed_sends,
                    totals.possibly_partial_sends,
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
            lines.iter().any(|line| line.contains("sending again after")
                && line.contains("; 1 possibly partial,")),
            "{lines:#?}"
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
                Ok(TalkerStatus::SendFailed { .. }) => errors += 1,
                Ok(TalkerStatus::Counters {
                    active_send_error: Some(error),
                    ..
                }) => active_errors.push(error),
                Ok(TalkerStatus::SendRecovered {
                    failures, withheld, ..
                }) => recovered = Some((failures, withheld)),
                Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        let (failures, withheld) = recovered.expect("replacement should resume the same run");
        assert_eq!(errors, 1, "one failure episode has one error edge");
        assert!(
            active_errors
                .iter()
                .any(|error| error.reopening
                    && error.message.contains("replacement device is absent")),
            "counter snapshots should show the current reopen obstacle, as a reopen: \
             {active_errors:#?}"
        );
        assert!(
            active_errors
                .iter()
                .filter(|error| error.message.contains("removed device handle"))
                .all(|error| !error.reopening),
            "the write that failed is not a reopen: {active_errors:#?}"
        );
        assert_eq!(
            failures, 1,
            "failed reopen attempts are withheld, not reported as failed writes"
        );
        assert!(
            withheld >= 1,
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
            withheld_total,
            render_samples,
            send_samples,
            message_render_samples,
            message_send_samples,
        ) = final_statuses
            .try_iter()
            .find_map(|status| match status {
                TalkerStatus::Counters {
                    totals,
                    final_snapshot: true,
                    ..
                } => Some((
                    totals.total_count,
                    totals.failed_sends,
                    totals.withheld_sends,
                    totals.timing.cumulative.render_duration.sample_count(),
                    totals.timing.cumulative.send_duration.sample_count(),
                    totals
                        .per_message_timing
                        .first()
                        .map_or(0, |message| message.render_duration.sample_count()),
                    totals
                        .per_message_timing
                        .first()
                        .map_or(0, |message| message.send_duration.sample_count()),
                )),
                _ => None,
            })
            .expect("final cumulative counters");
        assert_eq!(failed_total, 1);
        assert!(withheld_total >= withheld);
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
                event.level == tracing::Level::INFO && event.message.contains("sending again after")
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

    /// A UDP interface whose local port another socket holds, so it cannot
    /// open until `holder` is dropped: a device that is not there yet.
    fn held_local_port() -> (InterfaceConfig, std::net::UdpSocket) {
        let holder = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let mut udp = UdpConfig::unicast("127.0.0.1:9".parse().unwrap());
        udp.local_port = Some(holder.local_addr().unwrap().port());
        (InterfaceConfig::Udp(udp), holder)
    }

    #[test]
    fn a_channel_that_cannot_open_starts_once_its_port_appears() {
        // ADR-060: start what can start. The open is retried until the port
        // is free, then the channel runs.
        let (cfg, holder) = held_local_port();
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

        drop(holder);
        let opened = loop {
            match control_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(RunnerControlStatus::InterfaceOpened { .. }) => break true,
                Ok(_) => continue,
                Err(_) => break false,
            }
        };
        assert!(
            opened,
            "the retry opened the channel once the port was free"
        );
        cmd_tx.send(TalkerCommand::Stop).unwrap();
        runner.join().unwrap();
    }

    #[test]
    fn stop_ends_a_channel_still_waiting_to_open() {
        let (cfg, _holder) = held_local_port();
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
        let (cfg, _holder) = held_local_port();
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
                assert!(
                    message.contains("binding UDP socket"),
                    "message was: {message}"
                );
            }
            other => panic!("expected OpenFailed, got {:?}", other.is_ok()),
        }
    }

    /// A serial port with control lines, for the runner: it records the
    /// outputs set and every reopen, and reports `lines` when read.
    struct LinesInterface {
        lines: Arc<Mutex<ControlLines>>,
        set: Arc<Mutex<Vec<(OutputLine, bool)>>>,
        reopens: Arc<std::sync::atomic::AtomicU64>,
    }

    impl Interface for LinesInterface {
        fn send(&mut self, _data: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }

        fn prepare_retry(&mut self, _current: Option<&InterfaceConfig>) -> anyhow::Result<bool> {
            self.reopens
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        }

        fn set_output(&mut self, line: OutputLine, high: bool) -> anyhow::Result<()> {
            self.set.lock().unwrap().push((line, high));
            let mut lines = self.lines.lock().unwrap();
            match line {
                OutputLine::Rts => lines.rts = high,
                OutputLine::Dtr => lines.dtr = high,
            }
            Ok(())
        }

        fn control_lines(&mut self) -> Option<ControlLines> {
            Some(*self.lines.lock().unwrap())
        }
    }

    /// The newest control-line reading the runner reports within two seconds
    /// that satisfies `wanted`.
    fn wait_for_lines(
        handle: &TalkerHandle,
        wanted: impl Fn(&ControlLines) -> bool,
    ) -> ControlLines {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(TalkerStatus::ControlLines {
                lines: Some(lines), ..
            }) = handle.status_rx.recv_timeout(Duration::from_millis(20))
            {
                if wanted(&lines) {
                    return lines;
                }
            }
        }
        panic!("no control-line reading matched in time");
    }

    fn completion(handle: &TalkerHandle, target: CommandTarget) -> CommandExecution {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(RunnerControlStatus::CommandCompleted {
                target: done,
                execution,
                ..
            }) = handle.control_rx.recv_timeout(Duration::from_millis(20))
            {
                if done == target {
                    return execution;
                }
            }
        }
        panic!("the command did not complete in time");
    }

    /// ADR-070: a serial channel reports its control lines while it runs,
    /// and a click sets an output on the open port without reopening it.
    #[test]
    fn control_lines_are_reported_and_an_output_is_set_without_a_reopen() {
        let lines = Arc::new(Mutex::new(ControlLines::default()));
        let set = Arc::new(Mutex::new(Vec::new()));
        let reopens = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let handle = spawn_interface_runner(
            Box::new(LinesInterface {
                lines: Arc::clone(&lines),
                set: Arc::clone(&set),
                reopens: Arc::clone(&reopens),
            }),
            Some(InterfaceConfig::Serial(SerialConfig::new("COM9"))),
        );

        wait_for_lines(&handle, |l| !l.cts);
        lines.lock().unwrap().cts = true;
        wait_for_lines(&handle, |l| l.cts);

        let id = CommandId::mint();
        handle
            .cmd_tx
            .send(TalkerCommand::SetOutput {
                id,
                line: OutputLine::Dtr,
                high: true,
            })
            .unwrap();
        assert_eq!(
            completion(&handle, CommandTarget::Output(OutputLine::Dtr)),
            CommandExecution::Applied
        );
        wait_for_lines(&handle, |l| l.dtr);
        assert_eq!(*set.lock().unwrap(), [(OutputLine::Dtr, true)]);
        assert_eq!(
            reopens.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "setting a line never reopens the port"
        );

        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
    }

    /// UDP has no control lines: no reading is reported, and a click
    /// that reaches the runner anyway is refused with a reason.
    #[test]
    fn an_interface_without_control_lines_reports_none_and_refuses_a_click() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_interface_runner(
            Box::new(MockInterface {
                sent,
                fail: false,
                slow: None,
            }),
            Some(InterfaceConfig::Udp(UdpConfig::unicast(
                "127.0.0.1:9".parse().unwrap(),
            ))),
        );
        handle
            .cmd_tx
            .send(TalkerCommand::SetOutput {
                id: CommandId::mint(),
                line: OutputLine::Rts,
                high: true,
            })
            .unwrap();
        match completion(&handle, CommandTarget::Output(OutputLine::Rts)) {
            CommandExecution::Failed(message) => {
                assert!(message.contains("no control lines"), "{message}");
            }
            CommandExecution::Applied => panic!("a UDP channel has no RTS"),
        }
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if let Ok(TalkerStatus::ControlLines { .. }) =
                handle.status_rx.recv_timeout(Duration::from_millis(20))
            {
                panic!("a UDP channel reported control lines");
            }
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
    }
}
