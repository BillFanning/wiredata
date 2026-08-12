//! The send-outcomes line: what the run counted, stated as arithmetic.
//!
//! Leads with the schedule's own cadence points and subtracts each way a send
//! can fail to go out, so the line defines its own final term. Every name for
//! the successful remainder failed scrutiny — see [`send_outcomes`].

use super::*;

pub(in crate::gui) const SENT_MEANING_TOOLTIP: &str =
    "Sent means the configured-interface write returned success. It does not confirm that serial \
bits reached the wire, a network packet left the host, or a peer received the data.";

pub(in crate::gui) const THROUGHPUT_TOOLTIP: &str =
    "Total is every byte sent since Start and is retained after Stop. The two rates are a \
rolling five-second average of sent messages and bytes. Failed, retry-suppressed, and missed \
sends are excluded from all three. The fixed five-second denominator makes the rates ramp during \
startup and decay to zero after traffic stops, while the total stands still; they are not \
instantaneous line rate. Sent means the interface write returned success, not that any peer \
received the data.";

pub(in crate::gui) const LIVE_UPDATE_QUEUE_TOOLTIP: &str =
    "Current is how many live updates were waiting the last time the screen checked. The screen \
then removes them, so it is not the number waiting now. Peak is the largest value seen at one of \
those checks; a brief higher peak may be missed. A dropped update may affect an Output sample, a \
counter readout, timer status, or interface error notice. Sending never waits for this queue. \
Later updates correct the live totals and whether a send failure is still active; final run totals \
remain exact. Command results use a separate path.";

/// The run's counted send outcomes as one always-visible line.
///
/// Written as visible arithmetic — the schedule's own cadence points, less each
/// way a send can fail to go out — because every alternative required naming the
/// successful remainder in isolation, and no such name survived scrutiny:
/// *accepted* never says accepted by what, *sent* alone reads as delivery, and
/// *unaccepted* is false for the two categories the interface never saw. Stating
/// the subtraction removes the need: the line defines its own final term.
pub(in crate::gui) fn send_outcomes(
    sent: u64,
    failed: u64,
    suppressed: u64,
    missed: u64,
) -> DecisionSignal {
    let unsent = failed.saturating_add(suppressed).saturating_add(missed);
    let scheduled = sent.saturating_add(unsent);
    if scheduled == 0 {
        return DecisionSignal {
            text: "Send outcomes: no scheduled sends yet".to_owned(),
            tone: SignalTone::Neutral,
        };
    }

    DecisionSignal {
        text: format!(
            "Send outcomes: {} scheduled - {} failed - {} suppressed - {} missed = {} sent",
            thousands(scheduled),
            thousands(failed),
            thousands(suppressed),
            thousands(missed),
            thousands(sent),
        ),
        tone: if failed > 0 {
            SignalTone::Fault
        } else if unsent > 0 {
            SignalTone::Warning
        } else {
            SignalTone::Healthy
        },
    }
}

/// The one tooltip for the send-outcomes line: what each term counts, and where
/// in the send path each deduction happened.
pub(in crate::gui) fn send_outcomes_tooltip(
    sent: u64,
    failed: u64,
    suppressed: u64,
    missed: u64,
) -> String {
    let scheduled = sent
        .saturating_add(failed)
        .saturating_add(suppressed)
        .saturating_add(missed);
    format!(
        "{} scheduled counts every cadence point this run's schedule produced, less \
         the three ways a send does not go out. {} failed: the interface write was \
         attempted and returned an error. {} suppressed: after a failure, the send \
         was withheld during retry backoff and never attempted — these follow failures and \
         cannot occur without one. {} missed: the runner fell more than one interval \
         behind, so the cadence point was skipped before any send existed. The remaining \
         {} sent means the interface write returned success; it does not confirm that \
         bytes reached the wire or that any peer received them. These totals cover this run \
         and remain after sending recovers; the channel fault line says what is wrong now.",
        thousands(scheduled),
        thousands(failed),
        thousands(suppressed),
        thousands(missed),
        thousands(sent),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiredata_ui::diagnostics::SignalTone;

    /// Every count on this line and in its tooltip is grouped. A long run's
    /// scheduled total is the widest number in the pane, and it is the one a
    /// reader has to compare against the sent total by eye.
    #[test]
    fn send_outcomes_group_every_count_in_the_line_and_its_tooltip() {
        let long_run = send_outcomes(1_234_567, 0, 0, 2_400);
        assert_eq!(
            long_run.text,
            "Send outcomes: 1,236,967 scheduled - 0 failed - 0 suppressed - 2,400 missed \
             = 1,234,567 sent"
        );

        let tip = send_outcomes_tooltip(1_234_567, 0, 0, 2_400);
        assert!(tip.contains("1,236,967 scheduled"), "{tip}");
        assert!(tip.contains("2,400 missed"), "{tip}");
        assert!(tip.contains("1,234,567 sent"), "{tip}");
    }

    #[test]
    fn send_outcomes_distinguish_clean_shortfall_and_failure() {
        let clean = send_outcomes(100, 0, 0, 0);
        assert_eq!(
            clean.text,
            "Send outcomes: 100 scheduled - 0 failed - 0 suppressed - 0 missed = 100 sent"
        );
        assert_eq!(clean.tone, SignalTone::Healthy);

        // Stated as arithmetic: the schedule's own cadence points, less each
        // way a send fails to go out. Nothing has to name the remainder.
        let shortfall = send_outcomes(98, 0, 1, 1);
        assert_eq!(
            shortfall.text,
            "Send outcomes: 100 scheduled - 0 failed - 1 suppressed - 1 missed = 98 sent"
        );
        assert_eq!(shortfall.tone, SignalTone::Warning);

        // Same totals, but an attempted write returned an error: that is the
        // only component that escalates to a fault.
        let failed = send_outcomes(98, 1, 0, 1);
        assert_eq!(
            failed.text,
            "Send outcomes: 100 scheduled - 1 failed - 0 suppressed - 1 missed = 98 sent"
        );
        assert_eq!(failed.tone, SignalTone::Fault);
    }

    #[test]
    fn send_outcomes_are_neutral_before_any_schedule_fires() {
        let decision = send_outcomes(0, 0, 0, 0);
        assert_eq!(decision.text, "Send outcomes: no scheduled sends yet");
        assert_eq!(decision.tone, SignalTone::Neutral);
    }

    #[test]
    fn send_outcomes_tooltip_defines_each_term_and_its_limit() {
        let tip = send_outcomes_tooltip(98, 1, 0, 1);
        assert!(tip.contains("100 scheduled counts every cadence point"));
        // Suppressions are downstream of a failure, never an independent
        // fault — the tooltip has to say so or the two read as peers.
        assert!(tip.contains("cannot occur without one"));
        // The remainder is defined by the equation, but its limit still needs
        // stating: a successful write is not proof of delivery.
        assert!(tip.contains("does not confirm that bytes reached the wire"));
        assert!(tip.contains("totals cover this run and remain after sending recovers"));
        assert!(tip.contains("fault line says what is wrong now"));
    }
}
