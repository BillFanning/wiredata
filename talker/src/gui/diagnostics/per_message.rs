//! The per-message table: what each message suffered beside what it cost the
//! others (ADR-045, ADR-051).
//!
//! The point of a row is reading across it. A channel handles its messages one
//! at a time, so the message recording the lateness and the message causing it
//! are routinely different rows — the normal shape of a cadence problem, not an
//! anomaly.

use super::*;

/// One message's row in the per-message breakdown.
///
/// The point of the row is reading across it: `late_p99` is what this message
/// suffered, `blocked_others` is what it cost the rest. A channel where the
/// two land on different rows is the normal case, not an anomaly — the message
/// with the tightest interval absorbs the delay, and a slow infrequent one
/// causes it.
pub(in crate::gui) struct MessageRow {
    pub label: String,
    pub interval: String,
    pub sends: String,
    pub late: String,
    pub send_call: String,
    /// Longest single send of this message that delayed another: an elapsed
    /// hold time, unlike [`Self::cost_to_others`].
    pub longest_block: String,
    /// What this message cost the rest, in the two currencies that are not
    /// convertible into each other: combined waiting it imposed (summed across
    /// every message it delayed, so it can exceed `longest_block` and is never
    /// an elapsed time), and cadence points others lost outright.
    pub cost_to_others: String,
    /// This message delayed or displaced others, so its row carries the
    /// attention tone.
    pub costs_others: bool,
}

/// Per-message cells use the same model as every other timing readout
/// (ADR-046) — but the row already carries a Sends column, so repeating an
/// identical count in every cell just crowds out the figures.
///
/// The count appears only when this boundary's population differs from that
/// column, which is exactly when it is worth reading: lateness is sampled for
/// sends withheld by retry backoff, and the send call is timed for writes that
/// failed, so a gap here is evidence rather than noise.
fn cell_summary(histogram: DurationHistogram, sends: u64) -> String {
    let Some(figures) = timing_figures(histogram) else {
        return NO_MEASUREMENT.to_owned();
    };
    let samples = histogram.sample_count();
    if samples == sends {
        figures
    } else {
        format!("{figures} of {}", thousands(samples))
    }
}

pub(in crate::gui) fn per_message_rows(
    counts: &[u64],
    timing: &[MessageTiming],
) -> Vec<MessageRow> {
    timing
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let sends = counts.get(index).copied().unwrap_or(0);
            MessageRow {
                // Messages are identified by their position in the Messages
                // editor, one-based to match what the editor shows.
                label: format!("#{}", index + 1),
                interval: if message.interval.is_zero() {
                    "dormant".to_owned()
                } else if message.interval_changed {
                    // The histograms beside this are cumulative, so they span the
                    // cadence this message used to run at as well as this one.
                    format!("{} (changed)", format_interval(message.interval))
                } else {
                    format_interval(message.interval)
                },
                sends: thousands(sends),
                late: cell_summary(message.deadline_lateness, sends),
                // No render column: payload construction is a clock read, an
                // allocation and a memcpy — typically 1-3 us, below the 50 us
                // first bucket of the histogram that would report it. It stays
                // in the clipboard report and in the channel-wide work line,
                // where it costs nothing and still catches a pathological
                // payload, rather than taking a column that reads the same
                // forever.
                send_call: cell_summary(message.send_duration, sends),
                longest_block: if message.longest_block.is_zero() {
                    NO_MEASUREMENT.to_owned()
                } else {
                    format!(
                        "{} in {} sends",
                        compact_duration(message.longest_block),
                        thousands(message.blocking_sends)
                    )
                },
                // Two units in one cell, each with its noun attached, because
                // they answer one question — what did this message cost the
                // others — and a reader comparing rows should not have to
                // track which of two columns moved.
                cost_to_others: match (message.blocked_others.is_zero(), message.missed_others == 0)
                {
                    (true, true) => NO_MEASUREMENT.to_owned(),
                    (false, true) => format!("{} late", compact_duration(message.blocked_others)),
                    (true, false) => format!("{} missed", thousands(message.missed_others)),
                    (false, false) => format!(
                        "{} late · {} missed",
                        compact_duration(message.blocked_others),
                        thousands(message.missed_others)
                    ),
                },
                costs_others: !message.blocked_others.is_zero() || message.missed_others > 0,
            }
        })
        .collect()
}

pub(in crate::gui) const PER_MESSAGE_TOOLTIP: &str =
    "One row per message, numbered as in the Messages editor. Late is what that message \
suffered: how long after its own scheduled moment the channel got to it. Longest block and Cost to \
others are what it cost everything else — the longest single send of this message that held the \
channel against another, then the waiting that imposed added up across every message delayed and \
the cadence points others lost outright while this message was sending. Those are three different \
quantities: the combined wait sums several messages, so it can exceed the send that caused it and \
is not an elapsed time, and a missed point is a send that never happened rather than a late one. \
Misses are counted at the moment they are skipped, so they stay attributable when overload gets \
bad enough that few deadlines are reached at all. Suffering and causing usually land on different \
rows, and that is the normal shape of a cadence problem: a channel handles its messages one at a \
time, so a slow infrequent message can delay a fast one badly while recording almost no lateness \
itself. Read across a row, not down a column. Sends counts writes that succeeded; a timing figure \
repeats a count only where its own differs — lateness is sampled before retry backoff can withhold \
a scheduled send, and the send call is timed for writes that failed. Figures cover the whole \
run, and a percentile appears only where it differs from the worst value; the rolling ten-second \
view is channel-wide and appears in the Cadence row instead.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::telemetry::MessageTiming;

    /// The row exists so a reader can see the victim and the culprit are
    /// different messages. This pins that shape.
    #[test]
    fn per_message_rows_separate_what_a_message_suffered_from_what_it_caused() {
        let mut victim = MessageTiming {
            interval: Duration::from_millis(50),
            ..MessageTiming::default()
        };
        victim.deadline_lateness.record(Duration::from_millis(9));
        victim.send_duration.record(Duration::from_micros(300));

        let mut culprit = MessageTiming {
            interval: Duration::from_secs(2),
            blocked_others: Duration::from_millis(430),
            blocking_sends: 4,
            longest_block: Duration::from_millis(120),
            missed_others: 37,
            ..MessageTiming::default()
        };
        culprit.deadline_lateness.record(Duration::from_micros(80));
        culprit.send_duration.record(Duration::from_millis(120));

        let rows = per_message_rows(&[600, 4], &[victim, culprit]);
        assert_eq!(rows.len(), 2);

        // #1 is late and blames nobody.
        assert_eq!(rows[0].label, "#1");
        assert_eq!(rows[0].interval, "50 ms");
        assert_eq!(rows[0].sends, "600");
        assert_eq!(rows[0].late, "worst 9 ms of 1");
        assert_eq!(rows[0].longest_block, "—");
        assert_eq!(rows[0].cost_to_others, "—");
        assert!(!rows[0].costs_others);

        // #2 is barely late itself, and is charged for what it cost.
        assert_eq!(rows[1].label, "#2");
        assert_eq!(rows[1].late, "worst 80.0 us of 1");
        assert_eq!(rows[1].send_call, "worst 120 ms of 1");
        // The hold is its own cell because it is the one elapsed time here:
        // the combined waiting sums across victims and the misses are a count,
        // so neither may be read as a duration the channel actually spent.
        assert_eq!(rows[1].longest_block, "120 ms in 4 sends");
        assert_eq!(rows[1].cost_to_others, "430 ms late · 37 missed");
        assert!(rows[1].costs_others);
    }

    /// The Sends column already carries the count, so repeating it in all three
    /// timing cells said the same number four times per row. It survives only
    /// where it differs — which is the case that carries information.
    #[test]
    fn per_message_cells_repeat_the_send_count_only_when_it_differs() {
        let mut matching = MessageTiming::default();
        for _ in 0..3 {
            matching.deadline_lateness.record(Duration::from_millis(1));
            matching.send_duration.record(Duration::from_micros(200));
        }
        let rows = per_message_rows(&[3], &[matching]);
        assert_eq!(rows[0].sends, "3");
        assert_eq!(rows[0].late, "worst 1.00 ms", "count is already a column");
        assert_eq!(rows[0].send_call, "worst 200 us");

        // Retry backoff samples lateness for sends it then withholds, so this
        // population outruns the successful-send count — and that gap is the
        // evidence, so it is stated.
        let mut withheld = matching;
        for _ in 0..9 {
            withheld.deadline_lateness.record(Duration::from_millis(1));
        }
        let rows = per_message_rows(&[3], &[withheld]);
        assert_eq!(rows[0].late, "worst 1.00 ms of 12");
        assert_eq!(rows[0].send_call, "worst 200 us");
    }

    #[test]
    fn per_message_rows_mark_dormant_messages_and_missing_measurements() {
        let dormant = MessageTiming::default();
        let rows = per_message_rows(&[], &[dormant]);
        assert_eq!(rows[0].interval, "dormant");
        assert_eq!(rows[0].sends, "0");
        // No samples is not the same as a measured zero.
        assert_eq!(rows[0].late, "—");
        assert_eq!(rows[0].send_call, "—");
        assert_eq!(rows[0].longest_block, "—");
        assert_eq!(rows[0].cost_to_others, "—");
    }
}
