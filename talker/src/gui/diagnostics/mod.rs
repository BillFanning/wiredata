//! Diagnostics: the text of Talker's decision card and its detail rows.
//!
//! Classification is Talker-owned; only the egui chrome that renders it is
//! shared, through `wiredata_ui::diagnostics` (talker ADR-041). One submodule
//! per readout family, each owning its own wording, thresholds and tests:
//!
//! | Module | Row |
//! |---|---|
//! | [`outcomes`] | Send outcomes |
//! | [`capacity`] | Capacity, and measured service headroom |
//! | [`cadence`] | The schedule, and lateness against it |
//! | [`per_message`] | The per-message table |
//! | [`missed_routing`] | Where to look when sends are skipped |
//! | [`timing`] | Timing and runtime details, timer policy, freshness |
//!
//! What stays here is what more than one row needs: the card types, the shared
//! readout vocabulary ([`timing_figures`], [`timing_metric`],
//! [`recent_snapshot_label`]) that keeps every row — and both applications —
//! describing a measurement the same way, and the numeric formatters. A helper
//! used by exactly one row belongs in that row's module; this file is the
//! vocabulary, not a junk drawer.
//!
//! Re-exported flat, so a caller keeps one import surface and moving an item
//! between rows is not a call-site change.

use super::draft::ConnKind;
use crate::core::capacity::{
    service_sample_count, ChannelDemand, MessageDemand, ServiceEstimate, MIN_SERVICE_SAMPLES,
};
use crate::core::telemetry::{
    DurationHistogram, MessageTiming, RecentSnapshotState, SendTimingTelemetry, RECENT_WINDOW,
};
use crate::core::timing::{ActiveCadence, TimerMode, TimerReason, TimerStatus};
use wiredata_ui::diagnostics::SignalTone;
use wiredata_ui::format::{compact_duration, interval as format_interval, percent, thousands};

use super::MessageAnalysisCache;

mod cadence;
mod capacity;
mod missed_routing;
mod outcomes;
mod per_message;
mod timing;

pub(super) use cadence::*;
pub(super) use capacity::*;
pub(super) use missed_routing::*;
pub(super) use outcomes::*;
pub(super) use per_message::*;
pub(super) use timing::*;

pub(in crate::gui) fn compact_rate(value: f64, unit: &str) -> String {
    let (value, prefix) = if value >= 1_000_000.0 {
        (value / 1_000_000.0, "M")
    } else if value >= 1_000.0 {
        (value / 1_000.0, "k")
    } else {
        (value, "")
    };
    let precision = if value < 10.0 { 2 } else { 1 };
    format!("{value:.precision$} {prefix}{unit}")
}

pub(in crate::gui) fn compact_factor(factor: f64) -> String {
    if factor >= 1_000.0 {
        ">999x".to_owned()
    } else if factor >= 10.0 {
        format!("{factor:.1}x")
    } else {
        format!("{factor:.2}x")
    }
}

pub(in crate::gui) fn recent_snapshot_label(state: RecentSnapshotState) -> String {
    // "Snapshot" said how the figure travels, not what it covers, and it reads
    // as an instant — which is the one thing this is not. Every one of these
    // summarizes up to a whole window, so each state names that window and then
    // says what is wrong with it, if anything.
    let window = RECENT_WINDOW.as_secs();
    match state {
        RecentSnapshotState::Pending => "awaiting timing data".to_owned(),
        // Under a second old, the age is noise the reader would have to read
        // past on every frame to learn nothing.
        RecentSnapshotState::Current(age) if age < std::time::Duration::from_secs(1) => {
            format!("last ~{window} s")
        }
        RecentSnapshotState::Current(age) => {
            format!("last ~{window} s · updated {} ago", compact_duration(age))
        }
        // Past the window there is no "last ~10 s" left to describe: the figures
        // are older than the span they claim, so the state leads with that
        // rather than qualifying a window it can no longer stand behind.
        RecentSnapshotState::Expired(age) => {
            format!(
                "recent timing unavailable · last update {} ago",
                compact_duration(age)
            )
        }
        RecentSnapshotState::Final => format!("final ~{window} s before stop"),
    }
}

/// Summarize a histogram without a sample-count gate.
///
/// There was one — below twenty samples the readout showed the maximum and
/// called it a warm-up. It never guarded a bad computation: `rank` is
/// `ceil(samples × 99 / 100)`, which equals `samples` for any count up to 99,
/// so below a hundred samples the p99 bucket *is* the maximum's bucket. The
/// gate only relabelled the same number, and it did so at 20 while the two
/// statistics actually separate at 100.
///
/// So state the maximum, which is exact and true at one sample, and add the
/// percentile only where it is genuinely a different figure. `p99 < max` is
/// exactly that test: within one bucket the bound is `>=` the maximum, so the
/// comparison is false; it becomes true only when the p99 bucket sits strictly
/// below the maximum's. The sample count carries the weight the label used to
/// imply, so nothing needs a warm-up disclaimer at any count.
pub(in crate::gui) fn timing_figures(histogram: DurationHistogram) -> Option<String> {
    let max = histogram.max()?;
    let p99 = histogram.percentile_upper_bound(99)?;
    Some(if p99 < max {
        format!(
            "99% ≤ {}, worst {}",
            compact_duration(p99),
            compact_duration(max)
        )
    } else {
        format!("worst {}", compact_duration(max))
    })
}

/// One boundary's figures for a line that already states a sample count.
///
/// `line_samples` is that count, so this appends its own only when the two
/// populations differ — the same rule the per-message cells use. Render and the
/// send call are recorded in lockstep, so on a line carrying both, one shared
/// count is exact; lateness is sampled before retry backoff can withhold a
/// scheduled send, so it states its own whenever that gap opens.
pub(in crate::gui) fn timing_metric(
    label: &str,
    histogram: DurationHistogram,
    line_samples: u64,
) -> String {
    let Some(figures) = timing_figures(histogram) else {
        return format!("{label} no samples");
    };
    let samples = histogram.sample_count();
    if samples == line_samples {
        format!("{label} {figures}")
    } else {
        format!("{label} {figures} of {}", thousands(samples))
    }
}

/// A work boundary named by its longest observation.
///
/// "Worst" is the right superlative for lateness, where the reader wants the
/// biggest miss. A render or a send call is not *bad* for taking a long time,
/// it is just long, so these read "longest render" and "longest send call" —
/// which also puts the name of the figure before the figure rather than after
/// the boundary it belongs to.
///
/// The percentile trails in parentheses when it differs, so the leading figure
/// keeps its position as the two converge and the line does not re-lay itself
/// out around a clause that appears and disappears.
pub(in crate::gui) fn longest_metric(
    label: &str,
    histogram: DurationHistogram,
    line_samples: u64,
) -> String {
    let Some(max) = histogram.max() else {
        return format!("longest {label} no samples");
    };
    let samples = histogram.sample_count();
    let percentile = histogram
        .percentile_upper_bound(99)
        .filter(|p99| *p99 < max)
        .map(|p99| format!(" (99% ≤ {})", compact_duration(p99)))
        .unwrap_or_default();
    let population = if samples == line_samples {
        String::new()
    } else {
        format!(" of {}", thousands(samples))
    };
    format!(
        "longest {label} {}{percentile}{population}",
        compact_duration(max)
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::gui) struct DecisionSignal {
    pub(in crate::gui) text: String,
    pub(in crate::gui) tone: SignalTone,
}

/// The badge on the diagnostics card.
///
/// Every input is something the card itself shows or is shown immediately above
/// it. Discarded live updates used to raise this badge too, and no longer do:
/// that shared-queue warning now has one home above Output. A badge reading
/// ATTENTION over otherwise calm diagnostic rows would make queue pressure look
/// like a channel problem found by those measurements.
pub(in crate::gui) fn diagnostic_card_tone(
    delivery: SignalTone,
    capacity: SignalTone,
    timer_request_failed: bool,
    running: bool,
) -> SignalTone {
    if delivery == SignalTone::Fault || capacity == SignalTone::Fault {
        SignalTone::Fault
    } else if delivery == SignalTone::Warning
        || capacity == SignalTone::Warning
        || timer_request_failed
    {
        SignalTone::Warning
    } else if running {
        SignalTone::Healthy
    } else {
        SignalTone::Neutral
    }
}

const NO_MEASUREMENT: &str = "—";

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::telemetry::{DurationHistogram, RecentSnapshotState, SendTimingTelemetry};
    use crate::core::timing::ActiveCadence;

    use wiredata_ui::diagnostics::SignalTone;

    /// The send-outcome counts moved out of the card and above it, but their
    /// tone still has to reach the badge — otherwise a failing interface reads
    /// as a calm card. This pins the coupling that survived that move.
    ///
    /// It also pins the half that must *not* survive: a run whose only
    /// complaint is skipped sends still raises the badge from the outcome
    /// counts, so dismissing the routing callout inside the card cannot make
    /// the card look clean.
    #[test]
    fn send_outcome_tone_still_escalates_the_card_badge() {
        let failing = send_outcomes(98, 1, 0, 0, 1);
        assert_eq!(
            diagnostic_card_tone(failing.tone, SignalTone::Neutral, false, true),
            SignalTone::Fault
        );

        let shortfall = send_outcomes(98, 0, 0, 1, 1);
        assert_eq!(
            diagnostic_card_tone(shortfall.tone, SignalTone::Neutral, false, true),
            SignalTone::Warning
        );

        let missed_only = send_outcomes(98, 0, 0, 0, 2);
        assert_eq!(
            diagnostic_card_tone(missed_only.tone, SignalTone::Neutral, false, true),
            SignalTone::Warning
        );

        let clean = send_outcomes(100, 0, 0, 0, 0);
        assert_eq!(
            diagnostic_card_tone(clean.tone, SignalTone::Neutral, false, true),
            SignalTone::Healthy
        );
    }

    #[test]
    fn card_tone_surfaces_faults_warnings_and_clean_live_state() {
        assert_eq!(
            diagnostic_card_tone(SignalTone::Healthy, SignalTone::Fault, false, true),
            SignalTone::Fault
        );
        assert_eq!(
            diagnostic_card_tone(SignalTone::Healthy, SignalTone::Neutral, true, true),
            SignalTone::Warning
        );
        assert_eq!(
            diagnostic_card_tone(SignalTone::Healthy, SignalTone::Neutral, false, true),
            SignalTone::Healthy
        );
        assert_eq!(
            diagnostic_card_tone(SignalTone::Healthy, SignalTone::Neutral, false, false),
            SignalTone::Neutral
        );
    }

    /// A clustered schedule — two fast messages and one slow one — which is the
    /// shape a span renders misleadingly and a grouping renders honestly.
    pub(super) fn mixed_cadence() -> Option<ActiveCadence> {
        Some(ActiveCadence {
            messages: 3,
            shortest: Duration::from_millis(50),
        })
    }

    pub(super) fn mixed_groups() -> Vec<(Duration, usize)> {
        cadence_groups([
            Duration::from_millis(50),
            Duration::from_millis(50),
            Duration::from_secs(1),
        ])
    }

    /// The rule that replaced the warm-up gate, stated directly: the percentile
    /// earns its place only when it differs from the maximum.
    #[test]
    fn timing_figures_add_a_percentile_only_when_it_differs_from_the_maximum() {
        assert_eq!(timing_figures(DurationHistogram::default()), None);

        let mut single = DurationHistogram::default();
        single.record(Duration::from_millis(3));
        assert_eq!(
            timing_figures(single).unwrap(),
            "worst 3 ms",
            "one sample is reportable without a warm-up disclaimer"
        );

        // 99 identical samples: rank == samples, so p99 lands in the maximum's
        // own bucket and adds nothing.
        let mut identical = DurationHistogram::default();
        for _ in 0..99 {
            identical.record(Duration::from_millis(1));
        }
        assert_eq!(timing_figures(identical).unwrap(), "worst 1.00 ms");

        let mut spread = identical;
        for _ in 99..200 {
            spread.record(Duration::from_millis(1));
        }
        spread.record(Duration::from_millis(40));
        assert_eq!(
            timing_figures(spread).unwrap(),
            "99% ≤ 1.00 ms, worst 40 ms"
        );
    }

    #[test]
    fn expired_snapshot_is_not_presented_as_current_cadence_or_timing() {
        let mut timing = SendTimingTelemetry::default();
        for _ in 0..20 {
            timing.deadline_lateness.record(Duration::from_millis(1));
        }
        let state = RecentSnapshotState::Expired(Duration::from_secs(11));

        let cadence = cadence_decision(
            timing,
            timing,
            mixed_cadence(),
            &mixed_groups(),
            false,
            state,
        );
        // Past the window there is no "last ~10 s" to claim, so the state says
        // the recent figure is unavailable and dates the last one instead. A
        // reader must never take a run-wide number for current behaviour.
        assert_eq!(
            cadence.text,
            "3 messages: 2 at 50 ms, 1 at 1,000 ms · recent timing unavailable · last \
             update 11.0 s ago · worst this run 1.00 ms (2% of the shortest interval)"
        );
        assert_eq!(
            timing_detail_text(timing, timing, state),
            "Work per send: recent timing unavailable · last update 11.0 s ago · run max late 1.00 ms"
        );
    }

    /// Each state names the window it covers, because none of these is an
    /// instant — every one summarizes up to a whole window of samples, which is
    /// what "snapshot" hid.
    #[test]
    fn a_recent_window_label_states_its_span_and_what_is_wrong_with_it() {
        assert_eq!(
            recent_snapshot_label(RecentSnapshotState::Current(Duration::ZERO)),
            "last ~10 s"
        );
        assert_eq!(
            recent_snapshot_label(RecentSnapshotState::Current(Duration::from_millis(2_300))),
            "last ~10 s · updated 2.30 s ago"
        );
        assert_eq!(
            recent_snapshot_label(RecentSnapshotState::Final),
            "final ~10 s before stop"
        );
        assert_eq!(
            recent_snapshot_label(RecentSnapshotState::Expired(Duration::from_secs(11))),
            "recent timing unavailable · last update 11.0 s ago"
        );
        assert_eq!(
            recent_snapshot_label(RecentSnapshotState::Pending),
            "awaiting timing data"
        );

        // Every consumer must use that vocabulary; the work line previously
        // bypassed the helper and kept the retired "snapshot pending" phrase.
        let mut cumulative = SendTimingTelemetry::default();
        cumulative
            .deadline_lateness
            .record(Duration::from_millis(1));
        assert_eq!(
            timing_detail_text(
                SendTimingTelemetry::default(),
                cumulative,
                RecentSnapshotState::Pending,
            ),
            "Work per send: awaiting timing data · run max late 1.00 ms"
        );
        assert!(
            !TIMING_TOOLTIP.contains("snapshot"),
            "technician-facing timing help names the period, not its transport shape"
        );

        // The span comes from the window constant, so a change to the engine's
        // retention cannot leave five labels quoting a number it stopped using.
        assert!(
            recent_snapshot_label(RecentSnapshotState::Current(Duration::ZERO))
                .contains(&RECENT_WINDOW.as_secs().to_string()),
            "the label states the real window"
        );
    }

    #[test]
    fn timing_metrics_report_each_boundary_at_its_own_sample_count() {
        let mut timing = SendTimingTelemetry::default();
        for _ in 0..20 {
            timing.deadline_lateness.record(Duration::from_millis(1));
        }
        timing.render_duration.record(Duration::from_micros(100));
        timing.send_duration.record(Duration::from_micros(200));

        // The line states one count; a boundary repeats it only where its own
        // population differs. Render and the send call are recorded in
        // lockstep, so they stay silent; lateness carries the suppressed sends
        // that never reached a write, so it says so.
        let line_samples = timing.send_duration.sample_count();
        assert_eq!(
            timing_metric("deadline", timing.deadline_lateness, line_samples),
            "deadline worst 1.00 ms of 20"
        );
        assert_eq!(
            timing_metric("render", timing.render_duration, line_samples),
            "render worst 100 us"
        );
        assert_eq!(
            timing_metric("send call", timing.send_duration, line_samples),
            "send call worst 200 us"
        );
        assert_eq!(
            timing_metric("render", DurationHistogram::default(), line_samples),
            "render no samples"
        );
    }

    /// The work boundaries name their figure before stating it, and the
    /// percentile trails rather than leads — so gaining or losing one leaves
    /// every word before it where the reader last saw it.
    #[test]
    fn a_work_boundary_reads_as_its_longest_observation() {
        let mut single = DurationHistogram::default();
        single.record(Duration::from_micros(100));
        assert_eq!(longest_metric("render", single, 1), "longest render 100 us");

        let mut spread = DurationHistogram::default();
        for _ in 0..200 {
            spread.record(Duration::from_micros(100));
        }
        spread.record(Duration::from_millis(4));
        let line = longest_metric("send call", spread, spread.sample_count());
        assert!(
            line.starts_with("longest send call 4 ms (99% ≤ "),
            "the longest figure leads and the percentile follows it: {line}"
        );

        // A boundary sampled less often than the line says so, after both
        // figures rather than between them.
        assert!(longest_metric("render", single, 9).ends_with(" of 1"));
        assert_eq!(
            longest_metric("render", DurationHistogram::default(), 1),
            "longest render no samples"
        );
    }
}
