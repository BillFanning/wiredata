//! The send-outcomes line: what the run counted, stated as arithmetic.
//!
//! Leads with the schedule's own cadence points and subtracts each way a send
//! can fail to go out, so the line defines its own final term. Every name for
//! the successful remainder failed scrutiny — see [`send_outcomes`].

use super::*;

pub(in crate::gui) const SENT_MEANING_TOOLTIP: Hover = Hover::new(
    "Sent counts the messages the interface accepted without an error.\n\n\
     It does not confirm that the bits reached a serial line, that a network packet left this \
     computer, or that anything received the data.",
);

pub(in crate::gui) const THROUGHPUT_TOOLTIP: Hover = Hover::new(
    "The bytes and messages the interface accepted since this channel started.\n\n\
     The total includes the bytes of a possibly partial send that the interface took before it \
     failed. It stays on screen after Stop.\n\n\
     The two rates are averages over the last five seconds. They climb for the first five \
     seconds after Start and fall to zero over the five seconds after sending stops, while the \
     total stays put, so they are not the rate on the line at this instant.\n\n\
     Failed, possibly partial, withheld and missed sends are not counted as sent messages. \
     Sent means the interface accepted the data, not that anything received it.",
);

pub(in crate::gui) const LIVE_UPDATE_QUEUE_TOOLTIP: Hover = Hover::new(
    "Live updates bring each channel's progress to this screen.\n\n\
     The first number is how many updates were waiting the last time the screen checked. The \
     screen then takes them all, so it is not the number waiting now. Peak is the largest number \
     seen at one of those checks; a brief higher peak between checks can go unseen.\n\n\
     A dropped update can leave out an Output sample, a counter, the timer status or an \
     interface error notice. Later updates correct the live totals and show whether a send \
     failure is still happening, and the run's final totals are exact.\n\n\
     Sending never waits for this queue. Command results travel separately.",
);

pub(in crate::gui) const PEER_SENT_TOOLTIP: Hover = Hover::new(
    "The bytes the TCP peer sent back to Talker during this run.\n\n\
     Talker reads and discards them before each write, so the peer never stalls on a full \
     buffer, and closing the connection does not reset it.\n\n\
     They are counted but not shown. To see them, use Listener.",
);

/// The peer-reply readout (§4.5): "peer sent N bytes".
pub(in crate::gui) fn peer_sent_words(bytes: u64) -> String {
    format!(
        "Peer sent {} byte{}",
        thousands(bytes),
        if bytes == 1 { "" } else { "s" }
    )
}

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
    possibly_partial: u64,
    withheld: u64,
    missed: u64,
) -> DecisionSignal {
    let unsent = failed
        .saturating_add(possibly_partial)
        .saturating_add(withheld)
        .saturating_add(missed);
    let scheduled = sent.saturating_add(unsent);
    if scheduled == 0 {
        return DecisionSignal {
            text: "Send outcomes: no scheduled sends yet".to_owned(),
            tone: SignalTone::Neutral,
        };
    }

    DecisionSignal {
        text: format!(
            "Send outcomes: {} scheduled - {} failed - {} possibly partial - {} withheld - {} \
             missed = {} sent",
            thousands(scheduled),
            thousands(failed),
            thousands(possibly_partial),
            thousands(withheld),
            thousands(missed),
            thousands(sent),
        ),
        // A possibly partial send is a write that failed, too.
        tone: if failed > 0 || possibly_partial > 0 {
            SignalTone::Fault
        } else if unsent > 0 {
            SignalTone::Warning
        } else {
            SignalTone::Healthy
        },
    }
}

/// The send-outcomes line as shown: marked with ⚠ when it wants attention, as
/// a card row is, so the state never rests on its colour alone.
///
/// With the card's badge gone (ADR-066) this line is what says a run had
/// trouble once the missed-send callout is dismissed, so its mark matters.
pub(in crate::gui) fn send_outcomes_line(outcomes: &DecisionSignal) -> String {
    format!("{}{}", tone_marker(outcomes.tone), outcomes.text)
}

/// The one tooltip for the send-outcomes line: what each term counts, one
/// paragraph each and in the line's own order, so the reader can follow the
/// subtraction down the hover.
pub(in crate::gui) fn send_outcomes_tooltip(
    sent: u64,
    failed: u64,
    possibly_partial: u64,
    withheld: u64,
    missed: u64,
) -> Hover {
    let scheduled = sent
        .saturating_add(failed)
        .saturating_add(possibly_partial)
        .saturating_add(withheld)
        .saturating_add(missed);
    Hover::from_paragraphs([
        "Send outcomes shows how every scheduled send in this run ended.".to_owned(),
        format!(
            "{} scheduled: every send the schedule called for.",
            thousands(scheduled)
        ),
        format!(
            "{} failed: the interface was given the message and reported an error before \
             taking any of it.",
            thousands(failed)
        ),
        format!(
            "{} possibly partial: the interface reported an error after taking part of the \
             message, or, on Windows, a TCP write timed out without saying how much it sent. \
             The receiver may hold a fragment. It is never sent again, and the bytes taken are \
             in the byte total.",
            thousands(possibly_partial)
        ),
        format!(
            "{} withheld: after a failure, Talker waits before trying again, and a send that \
             comes due during that wait is held back and never tried. These only happen after \
             a failure.",
            thousands(withheld)
        ),
        format!(
            "{} missed: the channel fell more than one interval behind, so it skipped that \
             scheduled time.",
            thousands(missed)
        ),
        format!(
            "{} sent: the interface accepted the message. That does not confirm it reached the \
             wire or that anything received it.",
            thousands(sent)
        ),
        "These are totals for the whole run, so they stay after sending recovers. The \
         channel's error line says what is wrong now."
            .to_owned(),
    ])
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
        let long_run = send_outcomes(1_234_567, 0, 0, 0, 2_400);
        assert_eq!(
            long_run.text,
            "Send outcomes: 1,236,967 scheduled - 0 failed - 0 possibly partial - 0 withheld \
             - 2,400 missed = 1,234,567 sent"
        );

        let tip = send_outcomes_tooltip(1_234_567, 0, 0, 0, 2_400);
        let tip = tip.text();
        assert!(tip.contains("1,236,967 scheduled"), "{tip}");
        assert!(tip.contains("2,400 missed"), "{tip}");
        assert!(tip.contains("1,234,567 sent"), "{tip}");
    }

    #[test]
    fn send_outcomes_distinguish_clean_shortfall_and_failure() {
        let clean = send_outcomes(100, 0, 0, 0, 0);
        assert_eq!(
            clean.text,
            "Send outcomes: 100 scheduled - 0 failed - 0 possibly partial - 0 withheld - 0 \
             missed = 100 sent"
        );
        assert_eq!(clean.tone, SignalTone::Healthy);

        // Stated as arithmetic: the schedule's own cadence points, less each
        // way a send fails to go out. Nothing has to name the remainder.
        let shortfall = send_outcomes(98, 0, 0, 1, 1);
        assert_eq!(
            shortfall.text,
            "Send outcomes: 100 scheduled - 0 failed - 0 possibly partial - 1 withheld - 1 \
             missed = 98 sent"
        );
        assert_eq!(shortfall.tone, SignalTone::Warning);

        // Same totals, but an attempted write returned an error: that is the
        // only component that escalates to a fault.
        let failed = send_outcomes(98, 1, 0, 0, 1);
        assert_eq!(
            failed.text,
            "Send outcomes: 100 scheduled - 1 failed - 0 possibly partial - 0 withheld - 1 \
             missed = 98 sent"
        );
        assert_eq!(failed.tone, SignalTone::Fault);

        // §4.4: a write that failed part-way is its own term, and a fault too.
        let partial = send_outcomes(98, 0, 1, 0, 1);
        assert_eq!(
            partial.text,
            "Send outcomes: 100 scheduled - 0 failed - 1 possibly partial - 0 withheld - 1 \
             missed = 98 sent"
        );
        assert_eq!(partial.tone, SignalTone::Fault);
    }

    /// Amber or red alone is not a state a colour-blind reader can see. A run
    /// with any trouble is marked in the text too; a clean or empty one is
    /// not.
    #[test]
    fn a_send_outcomes_line_with_trouble_is_marked_without_colour() {
        let marked = |sent, failed, partial, withheld, missed| {
            send_outcomes_line(&send_outcomes(sent, failed, partial, withheld, missed))
        };
        assert!(marked(98, 0, 0, 0, 2).starts_with("\u{26A0} Send outcomes:"));
        assert!(marked(98, 0, 0, 2, 0).starts_with("\u{26A0} Send outcomes:"));
        assert!(marked(98, 1, 0, 0, 0).starts_with("\u{26A0} Send outcomes:"));
        assert!(marked(98, 0, 1, 0, 0).starts_with("\u{26A0} Send outcomes:"));
        assert!(marked(100, 0, 0, 0, 0).starts_with("Send outcomes:"));
        assert!(marked(0, 0, 0, 0, 0).starts_with("Send outcomes:"));
    }

    #[test]
    fn a_tcp_peer_s_replies_read_as_bytes_it_sent() {
        assert_eq!(peer_sent_words(0), "Peer sent 0 bytes");
        assert_eq!(peer_sent_words(1), "Peer sent 1 byte");
        assert_eq!(peer_sent_words(12_345), "Peer sent 12,345 bytes");
    }

    #[test]
    fn send_outcomes_are_neutral_before_any_schedule_fires() {
        let decision = send_outcomes(0, 0, 0, 0, 0);
        assert_eq!(decision.text, "Send outcomes: no scheduled sends yet");
        assert_eq!(decision.tone, SignalTone::Neutral);
    }

    #[test]
    fn send_outcomes_tooltip_defines_each_term_and_its_limit() {
        let hover = send_outcomes_tooltip(98, 1, 0, 0, 1);
        let paragraphs: Vec<&str> = hover.text().split("\n\n").collect();
        // One paragraph per term, in the line's own order, between the
        // heading and the closing note.
        let terms: Vec<&str> = paragraphs[1..paragraphs.len() - 1]
            .iter()
            .map(|paragraph| paragraph.split(':').next().unwrap())
            .collect();
        assert_eq!(
            terms,
            [
                "100 scheduled",
                "1 failed",
                "0 possibly partial",
                "0 withheld",
                "1 missed",
                "98 sent"
            ]
        );
        let tip = hover.text();
        // Withheld sends are downstream of a failure, never an independent
        // fault — the tooltip has to say so or the two read as peers.
        assert!(tip.contains("These only happen after a failure."));
        // The remainder is defined by the equation, but its limit still needs
        // stating: a successful write is not proof of delivery.
        assert!(tip.contains("That does not confirm it reached the wire"));
        assert!(tip.contains("totals for the whole run, so they stay after sending recovers"));
        assert!(tip.contains("error line says what is wrong now"));
    }
}
