//! Timing and runtime details: the work behind a send, and the timer policy that
//! was actually applied.
//!
//! Separate from the Cadence row deliberately. Cadence answers "is the schedule
//! being kept"; this answers "what did the work cost, under what policy".

use super::*;

pub(in crate::gui) const TIMING_TOOLTIP: Hover = Hover::new(
    "How long each part of a send took.\n\n\
     Late is how long after its scheduled time the channel got to a send. Sends held back \
     during a retry wait are included. Missed sends are not, because the channel never got to \
     them.\n\n\
     Render is the time taken to build the message, timestamp included. Send call is the time \
     until the interface accepted the message or reported an error, so failed attempts are \
     included. Neither shows that the data reached the wire or a receiver.\n\n\
     The line states its number of sends once. A figure adds its own count only where it covers \
     a different number of sends. Render and send call show their longest time first; a \
     percentile follows only when it differs, which takes at least a hundred sends. A \
     percentile is an upper bound: the true value is at or below it.\n\n\
     Recent figures cover up to the last ten seconds, ending at the latest update. A slow or \
     idle schedule can leave them unchanged for a while; \"updated\" says when they were \
     worked out, not when the newest send happened. While the channel runs, figures are no \
     longer used once their update is ten seconds old. After a normal stop the channel keeps \
     its exact timing from the last ~10 s; after an abnormal exit, older timing may be shown \
     instead.",
);

pub(in crate::gui) const TIMER_TOOLTIP: Hover = Hover::new(
    "How the channel waits for each scheduled time.\n\n\
     The shortest active interval decides. On Windows, with an interval under 32 ms the channel \
     keeps the 1 ms timer resolution on for as long as it runs. At 32 ms or longer it turns 1 ms \
     on only for the last 32 ms before each scheduled time, and lets it go before building and \
     sending the message. The setting is shared, so another channel may still be keeping it \
     on.\n\n\
     The first send of an Immediate schedule has no such precise wait before it. A command such \
     as Stop interrupts the wait.\n\n\
     Other platforms use their own precise waits. This affects when the channel wakes, not how \
     accurate its timestamps are or when the data arrives.",
);

pub(in crate::gui) const ALIGNMENT_TOOLTIP: Hover = Hover::new(
    "Cadence alignment decides when each message's first send is due.\n\n\
     Immediate: every active message is due as soon as the interface opens.\n\n\
     UTC phase: each message's first send is due at the next UTC time that is a whole multiple \
     of its interval, counted from the start of 1970. A 1000 ms interval lands on whole UTC \
     seconds; a 1500 ms interval alternates between whole and half seconds. After that, sends \
     follow the interval.\n\n\
     At most once a second, the channel compares the system clock with its own count of \
     elapsed time. If they differ by 250 ms or more, it re-aligns future sends. It does not send \
     the times it jumped over, and does not count them as missed.\n\n\
     This sets when sends are due, not when the interface finishes writing or when the data \
     arrives.",
);

pub(in crate::gui) fn timing_detail_text(
    recent: SendTimingTelemetry,
    cumulative: SendTimingTelemetry,
    snapshot_state: RecentSnapshotState,
) -> String {
    let run_samples = cumulative.deadline_lateness.sample_count();
    if run_samples == 0 {
        return "Work per send: awaiting first send".to_owned();
    }

    let run_max = cumulative
        .deadline_lateness
        .max()
        .map(compact_duration)
        .unwrap_or_else(|| "n/a".to_owned());
    match snapshot_state {
        RecentSnapshotState::Expired(_) => format!(
            "Work per send: {} · run max late {run_max}",
            recent_snapshot_label(snapshot_state)
        ),
        RecentSnapshotState::Pending => format!(
            "Work per send: {} · run max late {run_max}",
            recent_snapshot_label(snapshot_state)
        ),
        // Deadline lateness is deliberately absent: the Cadence row renders the
        // same recent histogram with the schedule context that makes it
        // readable, so repeating it here was one fact in two places. What is
        // left is what nothing else shows — how long the two stages of the work
        // itself took, channel-wide and recently, since the per-message table is
        // cumulative — plus the run-wide lateness ceiling the Cadence row omits
        // while a current window is available.
        RecentSnapshotState::Current(_) | RecentSnapshotState::Final => {
            // Both boundaries share one population, so the count belongs to the
            // line rather than to each figure on it.
            let samples = recent.send_duration.sample_count();
            format!(
                "Work per send ({}, {} sends): {} · {} · run max late {run_max}",
                recent_snapshot_label(snapshot_state),
                thousands(samples),
                longest_metric("render", recent.render_duration, samples),
                longest_metric("send call", recent.send_duration, samples),
            )
        }
    }
}

pub(in crate::gui) fn timer_status_detail(status: TimerStatus) -> (String, bool) {
    let shortest = status
        .shortest_active_interval()
        .map(compact_duration)
        .unwrap_or_else(|| "no active messages".to_owned());
    match (status.intent, status.mode) {
        (TimerIntent::ContinuousHighRate, TimerMode::WindowsOneMillisecond) => (
            format!("Windows 1 ms continuous (high rate) · shortest {shortest}"),
            false,
        ),
        (TimerIntent::ContinuousHighRate, TimerMode::WindowsRequestFailed) => (
            format!("Windows 1 ms request failed (high rate) · shortest {shortest}"),
            true,
        ),
        (TimerIntent::ContinuousHighRate, TimerMode::NativeDeadlineWaits) => (
            format!("native deadline waits (high rate) · shortest {shortest}"),
            false,
        ),
        (TimerIntent::ContinuousHighRate, TimerMode::NoRequest) => (
            format!("high-rate timer request pending · shortest {shortest}"),
            false,
        ),
        (TimerIntent::PrecisionWindow, TimerMode::WindowsOneMillisecond) => (
            format!("Windows 1 ms deadline windows · shortest {shortest}"),
            false,
        ),
        (TimerIntent::PrecisionWindow, TimerMode::WindowsRequestFailed) => (
            format!("Windows 1 ms request failed · shortest {shortest}"),
            true,
        ),
        (TimerIntent::PrecisionWindow, TimerMode::NativeDeadlineWaits) => (
            format!("native deadline waits · shortest {shortest}"),
            false,
        ),
        (TimerIntent::PrecisionWindow, TimerMode::NoRequest) => (
            format!("first waited deadline window pending · shortest {shortest}"),
            false,
        ),
        (TimerIntent::None, _) if status.active_cadence.is_some() => {
            (format!("ordinary deadline waits · {shortest}"), false)
        }
        (TimerIntent::None, _) => (
            "idle · no active messages; no timer request".to_owned(),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::timing::{
        ActiveCadence, CadenceAlignment, TimerIntent, TimerMode, TimerStatus,
    };

    #[test]
    fn a_near_threshold_status_names_the_deadline_window_policy() {
        let (detail, hot) = timer_status_detail(TimerStatus {
            mode: TimerMode::WindowsOneMillisecond,
            intent: TimerIntent::PrecisionWindow,
            active_cadence: Some(ActiveCadence {
                messages: 1,
                shortest: Duration::from_millis(50),
            }),
            cadence_alignment: CadenceAlignment::Immediate,
            clock_realignments: 0,
        });

        assert!(detail.contains("deadline windows"), "{detail}");
        assert!(!detail.contains("continuous"), "{detail}");
        assert!(!hot);
    }

    #[test]
    fn dormant_timer_status_names_idle_state_and_no_request() {
        // The timer policy follows the schedule alone, so a channel with
        // nothing to send has exactly one state to report.
        let (dormant, hot) = timer_status_detail(TimerStatus::default());
        assert_eq!(dormant, "idle · no active messages; no timer request");
        assert!(!hot);
    }
}
