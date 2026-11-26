//! A channel's lifecycle as shown: its glyph and status word, the Start
//! button's decision, and the live-update queue gauge.

use std::time::Instant;

/// Lifecycle presentation shared by the detail header and the channel-list
/// rows: listener's glyph set and palette colors mapped onto talker's channel
/// states, plus a status word. Talker has no Faulted lifecycle state — a
/// channel with an error is either still running (sending with errors) or
/// stopped (its open failed / it exited) — so the ⚠ marks "has an error" in
/// both cases and the word carries the run state.
pub(in crate::gui) fn lifecycle_indicator(
    running: bool,
    retrying: bool,
    has_error: bool,
    pal: &wiredata_ui::palette::Palette,
) -> (&'static str, egui::Color32, &'static str) {
    use wiredata_ui::glyphs;
    match (running, has_error) {
        // Waiting to open, as listener shows a Channel reconnecting: on, but
        // not sending yet (ADR-074).
        (true, _) if retrying => (glyphs::RECONNECTING, pal.warning, "retrying"),
        (true, false) => (glyphs::RUNNING, pal.running, "running"),
        (true, true) => (glyphs::FAULT, pal.fault, "running"),
        (false, true) => (glyphs::FAULT, pal.fault, "faulted"),
        (false, false) => (glyphs::STOPPED, pal.idle, "stopped"),
    }
}

/// The detail header's words for a channel waiting to open (ADR-074): when it
/// tries again, rounded up so it never reads "0 s" while still waiting, and
/// "retrying now" once that time has come.
pub(in crate::gui) fn retry_status_words(next_try_at: Instant, now: Instant) -> String {
    let left = next_try_at.saturating_duration_since(now);
    if left.is_zero() {
        return "retrying now".to_owned();
    }
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    format!(
        "retrying in {}",
        wiredata_ui::format::wait(std::time::Duration::from_secs(secs))
    )
}

/// The status bar's run summary: how many channels are sending, and how many
/// are waiting for their interface to open (ADR-074), counted apart so
/// "running" never includes a channel that has not opened yet.
pub(in crate::gui) fn run_summary(total: usize, sending: usize, retrying: usize) -> String {
    match (sending, retrying) {
        (0, 0) => "\u{2022} Stopped".to_owned(),
        (sending, 0) if sending == total => "\u{2022} All running".to_owned(),
        (sending, 0) => format!("\u{2022} {sending}/{total} running"),
        (0, retrying) => format!("\u{2022} {retrying} retrying"),
        (sending, retrying) => {
            format!("\u{2022} {sending}/{total} running, {retrying} retrying")
        }
    }
}

/// The Start-side lifecycle button's label and enabled state — listener's
/// `start_button` decision mapped onto talker's states, so the two detail
/// panes carry drift and recovery the same way. Pure, unit-tested; the
/// detail pane renders the result and defers the click:
/// - not running, no error → "Start channel", enabled iff the draft is valid
/// - not running, error → "Retry channel" (the open/run failed), iff valid
/// - running + pending edits (interface or messages) → "Apply & Restart",
///   enabled iff the complete replacement draft is valid
/// - running, no edits → "Start channel", **disabled** (nothing to do)
pub(in crate::gui) fn start_button(
    running: bool,
    has_error: bool,
    drift: bool,
    can_start: bool,
) -> (&'static str, bool) {
    match (running, drift) {
        (true, true) => ("Apply & Restart", can_start),
        (true, false) => ("Start channel", false),
        (false, _) => (
            if has_error {
                "Retry channel"
            } else {
                "Start channel"
            },
            can_start,
        ),
    }
}

/// The runner→UI live-update queue as reported beside its drop warning.
///
/// Grouped rather than passed as three loose numbers: depth is only readable
/// against its capacity, and a peak means nothing without both.
pub(in crate::gui) struct LiveUpdateQueueGauge {
    pub len: usize,
    pub peak: usize,
    pub capacity: usize,
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn lifecycle_indicator_maps_states_to_the_shared_glyph_set() {
        use wiredata_ui::glyphs;
        use wiredata_ui::palette::LIGHT;
        let (g, c, w) = lifecycle_indicator(true, false, false, &LIGHT);
        assert_eq!((g, w), (glyphs::RUNNING, "running"));
        assert_eq!(c, LIGHT.running);
        // Running with a live error: the ⚠ carries the alarm, the word stays
        // honest about the run state; the detail header prints the error below.
        let (g, c, w) = lifecycle_indicator(true, false, true, &LIGHT);
        assert_eq!((g, w), (glyphs::FAULT, "running"));
        assert_eq!(c, LIGHT.fault);
        // Waiting to open: listener's reconnecting glyph, and the word says so.
        let (g, c, w) = lifecycle_indicator(true, true, true, &LIGHT);
        assert_eq!((g, w), (glyphs::RECONNECTING, "retrying"));
        assert_eq!(c, LIGHT.warning);
        let (g, _, w) = lifecycle_indicator(false, false, true, &LIGHT);
        assert_eq!((g, w), (glyphs::FAULT, "faulted"));
        let (g, c, w) = lifecycle_indicator(false, false, false, &LIGHT);
        assert_eq!((g, w), (glyphs::STOPPED, "stopped"));
        assert_eq!(c, LIGHT.idle);
    }

    #[test]
    fn a_channel_waiting_to_open_says_when_it_tries_again() {
        use std::time::Duration;
        let now = Instant::now();
        assert_eq!(
            retry_status_words(now + Duration::from_millis(3_200), now),
            "retrying in 4 s",
            "rounded up, so it never reads 0 s while waiting"
        );
        assert_eq!(
            retry_status_words(now + Duration::from_secs(30), now),
            "retrying in 30 s"
        );
        assert_eq!(retry_status_words(now, now), "retrying now");
    }

    /// A channel waiting to open is counted as retrying, never as running.
    #[test]
    fn the_run_summary_counts_retrying_channels_apart() {
        assert_eq!(run_summary(2, 0, 0), "\u{2022} Stopped");
        assert_eq!(run_summary(2, 2, 0), "\u{2022} All running");
        assert_eq!(run_summary(3, 2, 0), "\u{2022} 2/3 running");
        assert_eq!(run_summary(1, 0, 1), "\u{2022} 1 retrying");
        assert_eq!(run_summary(3, 1, 1), "\u{2022} 1/3 running, 1 retrying");
    }

    #[test]
    fn start_button_matches_listener_decision_table() {
        // Stopped, valid → plain Start, enabled.
        assert_eq!(
            start_button(false, false, false, true),
            ("Start channel", true)
        );
        // Stopped, invalid draft → Start, disabled (blockers on hover).
        assert_eq!(
            start_button(false, false, false, false),
            ("Start channel", false)
        );
        // Stopped with an error from the last run/open → Retry.
        assert_eq!(
            start_button(false, true, false, true),
            ("Retry channel", true)
        );
        assert_eq!(
            start_button(false, true, false, false),
            ("Retry channel", false)
        );
        // Drift on a stopped channel is irrelevant — Start applies drafts anyway.
        assert_eq!(
            start_button(false, false, true, true),
            ("Start channel", true)
        );
        // Running with valid pending edits → the coordinated restart.
        assert_eq!(
            start_button(true, false, true, true),
            ("Apply & Restart", true)
        );
        // An invalid replacement must not interrupt the active run.
        assert_eq!(
            start_button(true, true, true, false),
            ("Apply & Restart", false)
        );
        // Running, nothing to apply → disabled Start.
        assert_eq!(
            start_button(true, false, false, false),
            ("Start channel", false)
        );
    }
}
