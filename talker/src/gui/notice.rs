//! Warnings the reader has acknowledged, and what brings them back.
//!
//! A warning that cannot be dismissed teaches the reader to look past it; one
//! that never returns is a warning deleted rather than acknowledged. Both
//! notices here stand on a counter that only grows within a run, which gives an
//! honest middle: record the count at the moment of dismissal, and speak again
//! when it is exceeded. The reader is told once per occurrence, and a fault that
//! is still spreading is never silent.
//!
//! This is presentation state only. Nothing here changes what is counted, and
//! the counted totals stay on screen either way — dismissing the missed-send
//! routing hides the advice, not the misses, which remain on the send-outcomes
//! line, marked ⚠.

/// A count-backed warning the reader has dismissed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DismissedNotice {
    /// The count as it stood when Dismiss was pressed; `None` while armed.
    acknowledged: Option<u64>,
}

impl DismissedNotice {
    /// Whether the notice should be shown at `count`.
    ///
    /// Takes `&mut self` because it also re-arms itself. These counters restart
    /// at zero each run, so a record left over from a previous run would
    /// suppress a fresh fault until it grew past a number the reader last saw
    /// somewhere else entirely. A count below the record can only mean the
    /// counter was reset, so the record is discarded rather than trusted.
    pub(super) fn showing(&mut self, count: u64) -> bool {
        if self.acknowledged.is_some_and(|seen| count < seen) {
            self.acknowledged = None;
        }
        self.acknowledged.is_none_or(|seen| count > seen)
    }

    /// Acknowledge the condition as it stands. It speaks again if it worsens.
    pub(super) fn dismiss(&mut self, count: u64) {
        self.acknowledged = Some(count);
    }
}

/// One channel's dismissed warnings, cleared when it starts a fresh run.
///
/// Kept apart from [`super::display::ChannelDisplay`] and from the supervisor's
/// telemetry because it belongs to neither: it is what *this reader* has already
/// seen, not what the pane holds or what the runner measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ChannelNotices {
    /// Diagnostic updates the runner discarded because the UI queue was full.
    /// Shown in the Output pane, whose completeness is what the drops cost.
    pub(super) dropped_updates: DismissedNotice,
    /// Scheduled sends the channel never reached. Shown as the routing callout
    /// in the diagnostics card.
    pub(super) missed_sends: DismissedNotice,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole contract in one pass: silent at the acknowledged count, back
    /// as soon as it is exceeded.
    #[test]
    fn a_dismissed_notice_returns_only_when_the_count_grows() {
        let mut notice = DismissedNotice::default();
        assert!(notice.showing(37), "an untouched notice is showing");

        notice.dismiss(37);
        assert!(!notice.showing(37), "the acknowledged count stays quiet");
        assert!(
            notice.showing(38),
            "one more occurrence is a fact the reader has not seen"
        );

        // Dismissing again acknowledges the new count, not the old one.
        notice.dismiss(38);
        assert!(!notice.showing(38));
    }

    /// A counter that restarts at zero must not be read against a record from
    /// the run before it — that would hide a new fault behind an old number.
    #[test]
    fn a_restarted_run_rearms_a_dismissed_notice() {
        let mut notice = DismissedNotice::default();
        notice.dismiss(500);
        assert!(!notice.showing(500));

        // The next run counts from zero and reaches one drop.
        assert!(
            notice.showing(1),
            "a new run's first occurrence must be reported"
        );
        // And the stale record is gone, not merely stepped over.
        assert!(notice.showing(1), "the record was discarded, not retained");
    }
}
