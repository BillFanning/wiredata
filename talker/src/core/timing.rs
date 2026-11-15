//! OS timer-resolution management for automatic high-rate and explicit
//! precision schedules (ADR-017, ADR-034).
//!
//! The runner's deadline waits (`recv_deadline`, ADR-002) wake on the OS
//! scheduler tick — **15.625 ms** by default on Windows. That is finer than
//! any sensible sleep for a 10 Hz schedule, but *coarser than the interval*
//! of a 100 Hz one: every wake arrives more than one interval late, the
//! stall policy skips the backlog (spec §8.1), and ~36% of sends are
//! missed. Requesting 1 ms resolution (`timeBeginPeriod(1)`) while a
//! sub-[`HIGH_RATE_THRESHOLD`] schedule is running fixes the miss rate.
//!
//! A schedule at or above that threshold instead stages a coarse wait, requests
//! high resolution for the final [`PRECISION_WINDOW`] before a slower send,
//! then releases it after the deadline wake. This tightens deadline wakes without
//! holding the Windows power policy continuously. [`CadenceAlignment`] is the
//! separate choice that can phase deadlines to UTC; neither setting makes the wall
//! clock itself more accurate or proves when bytes reach the wire.
//!
//! - **No elevation needed**; the request is per-process since Windows 10
//!   2004, so other processes never see it.
//! - **Leak-proof across death**: the kernel releases a process's resolution
//!   request on termination (including a kill), so the RAII guard here is
//!   about releasing *early* when the last fast channel stops, not about
//!   correctness after a crash.
//! - **Windows 11 throttling**: by default the OS ignores the request while
//!   the process window is minimized/occluded — exactly when a long soak
//!   runs. [`keep_timer_resolution_when_minimized`] opts out once at startup.
//!
//! The shared resolution guard is an effective no-op off Windows because native
//! deadline waits are not governed by the Windows timer period. A future macOS App
//! Nap activity policy is separate from timer resolution; see talker TODO.md.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use wiredata_timing::{
    high_resolution, keep_timer_resolution_when_minimized, HighResolutionGuard,
};

/// The timer resolution `wiredata_timing::high_resolution` asks Windows for.
pub const REQUESTED_TIMER_RESOLUTION: Duration = Duration::from_millis(1);

/// Windows' default timer tick (15.625 ms), rounded up to a whole millisecond:
/// the granularity of a wait when the timer-resolution request is refused. It
/// is not a limit on lateness: a busy computer can run a thread later still.
pub const WINDOWS_DEFAULT_TICK: Duration = Duration::from_millis(16);

/// Intervals shorter than this want 1 ms timer resolution: two default
/// Windows scheduler ticks (2 × 15.625 ms), the point below which a deadline
/// wait can no longer be trusted to land inside its own interval.
pub const HIGH_RATE_THRESHOLD: Duration = Duration::from_millis(32);

/// Lead time used by explicit precision mode. Two default Windows timer
/// ticks give the coarse first wait enough margin to enter the final window
/// before the actual send deadline.
pub const PRECISION_WINDOW: Duration = HIGH_RATE_THRESHOLD;

/// How a Channel establishes each message's cadence phase when a run starts.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CadenceAlignment {
    /// Preserve existing behavior: every active message is due immediately.
    #[default]
    Immediate,
    /// First fire at the next UTC/Unix-epoch interval boundary, then continue
    /// on a monotonic grid. Intervals need not divide a second: alignment means
    /// exact epoch multiples, so 1.5 s alternates whole- and half-second phases.
    /// Wall-clock steps rebase future deadlines only.
    UtcPhase,
}

impl CadenceAlignment {
    pub fn is_immediate(&self) -> bool {
        *self == Self::Immediate
    }
}

/// What the schedule calls for from the timer, chosen from the shortest active
/// interval alone ([`timer_intent`]).
///
/// The runner waits by it, and [`TimerStatus`] reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimerIntent {
    /// Every message is dormant: nothing to wait for, no request.
    #[default]
    None,
    /// The shortest active interval is below [`HIGH_RATE_THRESHOLD`], so the
    /// 1 ms resolution request is held continuously.
    ContinuousHighRate,
    /// The shortest active interval is at or above [`HIGH_RATE_THRESHOLD`], so
    /// the request is held only in a window before each waited deadline.
    PrecisionWindow,
}

/// Select the deadline-wait policy from the schedule alone.
///
/// Below [`HIGH_RATE_THRESHOLD`] the request is held continuously; at or above
/// it, only in a [`PRECISION_WINDOW`] before each waited deadline. The window is
/// never the wrong call above the threshold, because its cost falls with the
/// interval while its benefit does not, and below it there is no coarse phase
/// to stage, which is why the two constants are the same value (ADR-047).
pub fn timer_intent(shortest_active_interval: Option<Duration>) -> TimerIntent {
    match shortest_active_interval {
        None => TimerIntent::None,
        Some(interval) if interval < HIGH_RATE_THRESHOLD => TimerIntent::ContinuousHighRate,
        Some(_) => TimerIntent::PrecisionWindow,
    }
}

/// One blocking deadline selected for the runner's next command wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitPlan {
    /// One native deadline wait; no resolution staging is needed.
    Direct(Instant),
    /// Coarse wait to the beginning of the precision window.
    Stage(Instant),
    /// Final wait while the platform timer-resolution guard is held.
    Precision(Instant),
}

pub(crate) fn wait_plan(intent: TimerIntent, now: Instant, deadline: Instant) -> WaitPlan {
    wait_plan_for_platform(intent, now, deadline, cfg!(windows))
}

fn wait_plan_for_platform(
    intent: TimerIntent,
    now: Instant,
    deadline: Instant,
    supports_resolution_request: bool,
) -> WaitPlan {
    if intent != TimerIntent::PrecisionWindow || !supports_resolution_request {
        return WaitPlan::Direct(deadline);
    }

    let window_start = deadline.checked_sub(PRECISION_WINDOW).unwrap_or(now);
    if now < window_start {
        WaitPlan::Stage(window_start)
    } else {
        WaitPlan::Precision(deadline)
    }
}

/// The deadline-wait policy currently in effect for one channel runner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimerMode {
    /// No timer-resolution request is held: every message is dormant, or the
    /// channel waits by windows and the first one has not opened yet.
    #[default]
    NoRequest,
    /// Windows accepted the process-wide 1 ms timer-resolution request.
    WindowsOneMillisecond,
    /// Windows rejected the process-wide 1 ms timer-resolution request.
    WindowsRequestFailed,
    /// This platform's native deadline waits are used; no Windows-style
    /// process-wide resolution request is needed.
    NativeDeadlineWaits,
}

/// The independent send cadences a channel is currently running — one per
/// non-dormant message.
///
/// A channel schedules each of its messages separately, so a running channel is
/// several cadences at once, not one. Timing telemetry pools every message's
/// deadlines into a single histogram, which is only readable if the reader also
/// knows how many cadences fed that pool.
///
/// Deliberately just a count and the timer-policy input. The interval
/// *distribution* is carried per message on the counters lane, so it has one
/// source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveCadence {
    /// Non-dormant messages in the running schedule.
    pub messages: usize,
    /// Shortest active interval. Also the timer-policy input, and the
    /// denominator lateness is compared against.
    pub shortest: Duration,
}

/// User-facing timer policy plus the schedule input that selected it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerStatus {
    /// What the platform did with the request.
    pub mode: TimerMode,
    /// What the schedule asked for.
    pub intent: TimerIntent,
    /// `None` when every message is dormant.
    pub active_cadence: Option<ActiveCadence>,
    pub cadence_alignment: CadenceAlignment,
    pub clock_realignments: u64,
}

impl TimerStatus {
    /// The interval that selected this timer policy, or `None` when every
    /// message is dormant.
    pub fn shortest_active_interval(self) -> Option<Duration> {
        self.active_cadence.map(|cadence| cadence.shortest)
    }
}

/// Describe the timer policy after the runner has reconciled its guard with
/// the current schedule.
pub(crate) struct TimerStatusInput<'a> {
    pub intent: TimerIntent,
    pub active_cadence: Option<ActiveCadence>,
    pub guard: Option<&'a HighResolutionGuard>,
    pub previous_mode: TimerMode,
    pub cadence_alignment: CadenceAlignment,
    pub clock_realignments: u64,
}

pub(crate) fn timer_status(input: TimerStatusInput<'_>) -> TimerStatus {
    let TimerStatusInput {
        intent,
        active_cadence,
        guard,
        previous_mode,
        cadence_alignment,
        clock_realignments,
    } = input;
    let mode = match intent {
        TimerIntent::None => TimerMode::NoRequest,
        TimerIntent::ContinuousHighRate | TimerIntent::PrecisionWindow => {
            #[cfg(windows)]
            {
                match guard {
                    Some(guard) if guard.is_effective() => TimerMode::WindowsOneMillisecond,
                    Some(_) => TimerMode::WindowsRequestFailed,
                    None if intent == TimerIntent::PrecisionWindow => match previous_mode {
                        TimerMode::WindowsOneMillisecond | TimerMode::WindowsRequestFailed => {
                            previous_mode
                        }
                        _ => TimerMode::NoRequest,
                    },
                    None => TimerMode::WindowsRequestFailed,
                }
            }
            #[cfg(not(windows))]
            {
                let _ = (guard, previous_mode);
                TimerMode::NativeDeadlineWaits
            }
        }
    };
    TimerStatus {
        mode,
        intent,
        active_cadence,
        cadence_alignment,
        clock_realignments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One active message. Timer policy reads only the shortest interval, so
    /// the span is irrelevant to every assertion in this module.
    fn uniform_cadence(interval: Duration) -> Option<ActiveCadence> {
        Some(ActiveCadence {
            messages: 1,
            shortest: interval,
        })
    }

    #[test]
    fn intent_follows_the_schedule_with_no_configured_mode() {
        let just_below_high_rate = HIGH_RATE_THRESHOLD - Duration::from_nanos(1);

        assert_eq!(
            timer_intent(None),
            TimerIntent::None,
            "dormant messages never hold timer resolution"
        );
        assert_eq!(
            timer_intent(Some(just_below_high_rate)),
            TimerIntent::ContinuousHighRate,
            "an interval shorter than the window leaves no coarse phase to stage"
        );
        // The threshold is where the mechanism changes, not where it starts:
        // a windowed request at exactly the threshold has 100% duty, matching
        // the continuous request it takes over from, and cheapens from there.
        assert_eq!(
            timer_intent(Some(HIGH_RATE_THRESHOLD)),
            TimerIntent::PrecisionWindow
        );
        assert_eq!(
            timer_intent(Some(Duration::from_millis(50))),
            TimerIntent::PrecisionWindow
        );
        assert_eq!(
            timer_intent(Some(Duration::from_secs(1))),
            TimerIntent::PrecisionWindow,
            "a slow schedule is where the window is cheapest, not where it is skipped"
        );
    }

    #[test]
    fn precision_wait_stages_before_the_final_deadline_window() {
        let now = std::time::Instant::now();
        let deadline = now + Duration::from_secs(1);
        assert_eq!(
            wait_plan_for_platform(TimerIntent::PrecisionWindow, now, deadline, true),
            WaitPlan::Stage(deadline - PRECISION_WINDOW)
        );
        assert_eq!(
            wait_plan_for_platform(
                TimerIntent::PrecisionWindow,
                deadline - Duration::from_millis(10),
                deadline,
                true,
            ),
            WaitPlan::Precision(deadline)
        );
        assert_eq!(
            wait_plan_for_platform(TimerIntent::PrecisionWindow, now, deadline, false),
            WaitPlan::Direct(deadline),
            "native high-resolution waits need no staging wake"
        );
        assert_eq!(
            wait_plan_for_platform(TimerIntent::None, now, deadline, true),
            WaitPlan::Direct(deadline)
        );
    }

    #[test]
    fn timer_status_explains_dormant_high_rate_and_windowed_waits() {
        let dormant = timer_status(TimerStatusInput {
            intent: TimerIntent::None,
            active_cadence: uniform_cadence(HIGH_RATE_THRESHOLD),
            guard: None,
            previous_mode: TimerMode::NoRequest,
            cadence_alignment: CadenceAlignment::Immediate,
            clock_realignments: 0,
        });
        assert_eq!(dormant.mode, TimerMode::NoRequest);
        assert_eq!(dormant.intent, TimerIntent::None);
        assert_eq!(
            dormant.shortest_active_interval(),
            Some(HIGH_RATE_THRESHOLD)
        );

        let guard = high_resolution();
        let fast = timer_status(TimerStatusInput {
            intent: TimerIntent::ContinuousHighRate,
            active_cadence: uniform_cadence(Duration::from_millis(1)),
            guard: Some(&guard),
            previous_mode: TimerMode::NoRequest,
            cadence_alignment: CadenceAlignment::Immediate,
            clock_realignments: 0,
        });
        assert_eq!(fast.intent, TimerIntent::ContinuousHighRate);
        #[cfg(windows)]
        assert_eq!(
            fast.mode,
            if guard.is_effective() {
                TimerMode::WindowsOneMillisecond
            } else {
                TimerMode::WindowsRequestFailed
            }
        );
        #[cfg(not(windows))]
        assert_eq!(fast.mode, TimerMode::NativeDeadlineWaits);

        let retained = timer_status(TimerStatusInput {
            intent: TimerIntent::PrecisionWindow,
            active_cadence: uniform_cadence(Duration::from_secs(1)),
            guard: None,
            previous_mode: TimerMode::WindowsOneMillisecond,
            cadence_alignment: CadenceAlignment::UtcPhase,
            clock_realignments: 2,
        });
        assert_eq!(retained.intent, TimerIntent::PrecisionWindow);
        assert_eq!(retained.cadence_alignment, CadenceAlignment::UtcPhase);
        assert_eq!(retained.clock_realignments, 2);
        #[cfg(windows)]
        assert_eq!(retained.mode, TimerMode::WindowsOneMillisecond);
        #[cfg(not(windows))]
        assert_eq!(retained.mode, TimerMode::NativeDeadlineWaits);
    }

    #[test]
    fn guard_acquires_and_releases_the_real_request() {
        // Exercises the real timeBeginPeriod/timeEndPeriod path (a no-op off
        // Windows); nesting and dropping must not panic or underflow.
        let a = high_resolution();
        let b = high_resolution();
        drop(a);
        drop(b);
    }
}
