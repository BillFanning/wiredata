//! Where to look when scheduled sends are being skipped.
//!
//! Mostly a router. One branch may state an amount, because misses matched to a
//! send as they happen are evidence about the misses themselves (ADR-051); the
//! verbs carry which kind of claim is being made.

use super::*;

/// Hover text for the missed-send callout: what a miss is, and what to do.
///
/// Deliberately short. A technician hovering a fault wants the next action, not
/// the epistemology of the measurement; the limits that qualify the answer live
/// in [`MISSED_ROUTING_LIMITS`], under Timing & runtime details, where someone
/// who has followed the routing and wants to know how far to trust it will look.
pub(in crate::gui) const MISSED_ROUTING_TOOLTIP: &str =
    "A missed send is a scheduled send the channel never reached, having fallen more than one \
interval behind: nothing was attempted, so no timing exists for it. Work the line left to right \
— it is ordered by what would settle the question soonest. See Timing & runtime details for how \
far this evidence reaches.";

/// Plain-language limits behind the routing line, shown in the details section.
///
/// The labelled paragraphs distinguish a measured missed-send count from a
/// delay-based lead, then state what the remaining evidence can and cannot
/// establish. The history-sizing proof that makes the third paragraph true is
/// a durable implementation decision and stays in ADR-051 rather than on the
/// technician's screen.
pub(in crate::gui) const MISSED_ROUTING_LIMITS: &str = concat!(
    "How to read the missed-send result — and its limits\n\n",
    "• A count beside a message means its serial or network send was still in progress when other ",
    "sends were missed. Unlike delay evidence, it does not thin out during severe ",
    "slowdowns. Each message gets only its own share. For a message's own misses, compare its ",
    "longest send call with its interval.\n\n",
    "• “Check message #…” is a lead from observed delays, not proof.\n\n",
    "• “No serial or network send was recorded as being in progress” does not mean the channel ",
    "was idle; timing cannot identify the cause.\n\n",
    "• Other clues — fix a current send failure first, but do not assume it caused the misses. An ",
    "impossible serial schedule is direct evidence. Earlier failures may have cleared; application ",
    "capacity is an estimate."
);

/// Evidence available when scheduled sends are being skipped.
///
/// Grouped rather than passed loose because the honesty of the result depends
/// on which of these is *current* and which is a run total — a distinction the
/// caller has and a bare `u64` would lose.
pub(in crate::gui) struct MissedSendEvidence {
    /// Run-total skipped sends.
    pub missed: u64,
    /// An interface error is showing **now**, not merely somewhere in the run.
    pub interface_erroring: bool,
    /// Run-total failed writes, which may all predate the current state.
    pub failed: u64,
    pub serial_oversubscribed: bool,
    pub service: Option<ServiceEstimate>,
}

/// Where to look when scheduled sends are being skipped.
///
/// Mostly a router, occasionally a verdict, and the wording says which. One
/// input *is* measured at the instant a send was skipped —
/// [`MessageTiming::missed_others`], attributed to whichever send held the
/// thread as each point passed (ADR-051) — and where that lands on a message the
/// text states an amount rather than a place to look. Everything else here is a
/// run total or evidence from deadlines that were reached, so those branches
/// keep the hedged verbs: "check", "start from", "may not".
///
/// Capacity findings use the running schedule, not the editable draft, so an
/// unapplied edit is never blamed for a run's misses.
///
/// Order starts with the most actionable current condition, then moves through
/// direct findings and progressively weaker leads. A live interface fault comes
/// first because it can be fixed now, not because this evidence proves it
/// caused the misses. A physically impossible schedule comes next because no
/// amount of tuning elsewhere changes it.
pub(in crate::gui) fn missed_send_routing(
    evidence: &MissedSendEvidence,
    per_message: &[MessageTiming],
) -> Option<DecisionSignal> {
    let MissedSendEvidence {
        missed,
        interface_erroring,
        failed,
        serial_oversubscribed,
        service,
    } = *evidence;
    if missed == 0 {
        return None;
    }

    let blocker = per_message
        .iter()
        .enumerate()
        .filter(|(_, message)| !message.blocked_others.is_zero())
        .max_by_key(|(_, message)| message.blocked_others);
    // Measured where the miss happened rather than inferred from lateness, so
    // this outranks `blocker` when both are present.
    let convicted = per_message
        .iter()
        .enumerate()
        .filter(|(_, message)| message.missed_others > 0)
        .max_by_key(|(_, message)| message.missed_others);
    let active_messages = per_message
        .iter()
        .filter(|message| !message.interval.is_zero())
        .count();

    let text = if interface_erroring {
        "Missed sends: the interface is failing right now — fix that first in Send outcomes, then \
         see whether cadence recovers."
            .to_owned()
    } else if serial_oversubscribed {
        "Missed sends: the serial line cannot carry this schedule — see Capacity.".to_owned()
    } else if let Some((index, message)) = convicted {
        // The one branch entitled to state an amount rather than a lead: every
        // point counted here was attributed while some message's send held the
        // thread.
        //
        // All three quantities appear. Naming only the largest culprit and the
        // shortfall drops every other attributed message out of a sentence whose
        // numbers are supposed to add up — with 5 to #2, 4 to #3 and 3
        // unmatched, the old wording said 5 and 3 of 12.
        let attributed: u64 = per_message
            .iter()
            .map(|message| message.missed_others)
            .sum();
        let headline = if attributed >= missed {
            format!(
                "for all {}, Talker was waiting for another message's serial or network send to \
                 finish",
                thousands(missed)
            )
        } else {
            format!(
                "for {} of {}, Talker was waiting for another message's serial or network send to \
                 finish",
                thousands(attributed),
                thousands(missed)
            )
        };
        let largest = if message.missed_others == attributed {
            format!("; it was message #{} every time", index + 1)
        } else {
            format!(
                "; message #{} accounts for the largest share ({})",
                index + 1,
                thousands(message.missed_others)
            )
        };
        let hold = if message.longest_block.is_zero() {
            String::new()
        } else {
            format!(
                " Its longest send held the channel {}.",
                compact_duration(message.longest_block)
            )
        };
        // Unmatched is not idle, and nothing here can tell the causes apart.
        // What the record supports is one negative fact — no measured interface
        // write spanned the point — and a late wake, work outside the send
        // call, and a free thread all produce exactly that. Ageing out is *not*
        // among them: the history is sized from the schedule (ADR-051), so a
        // write that could have spanned the point is still there to be found.
        let remainder = if attributed >= missed {
            String::new()
        } else {
            format!(
                " For the other {}, no serial or network send was recorded as being in progress. \
                 This does not mean the channel was idle; the available timing cannot identify \
                 the cause.",
                thousands(missed - attributed)
            )
        };
        format!("Missed sends: {headline}{largest}.{hold}{remainder} See Per-message timing.")
    } else if let Some((index, message)) = blocker {
        // Two different quantities, and only one of them is an elapsed hold:
        // the longest blocking send is what the channel actually spent, while
        // the combined figure sums every delayed message's wait and can exceed
        // it. Stating the hold first keeps the larger number from reading as
        // one.
        format!(
            "Missed sends: check message #{} first — its longest send held the channel {}, causing \
             {} of combined waiting across other messages in {} sends. See Per-message timing.",
            index + 1,
            compact_duration(message.longest_block),
            compact_duration(message.blocked_others),
            thousands(message.blocking_sends),
        )
    } else if service.is_some_and(|estimate| estimate.headroom_factor() < 1.0) {
        // Still a projection even with the running schedule as its input: it
        // divides summed p99 bounds into a requested rate, so "may not" stays
        // the strongest honest verb.
        "Missed sends: rendering and the interface write together may not service the requested \
         rate — see Capacity."
            .to_owned()
    } else if active_messages == 1 {
        // With one active message there is nothing else to hold the channel, so
        // the blocking branch above can never fire. Saying "no single message
        // accounts for these" here would be true and useless — it describes the
        // absence of a cause that was never possible.
        //
        // What it must not say is that nothing is holding the channel up.
        // Something plainly is, or the points would have been reached; what is
        // ruled out is only *another message* being the cause. Naming the two
        // candidates that remain is the useful half of that.
        "Missed sends: this channel has one active message, so no other message is delaying it. \
         Either that message's own send takes longer than its interval, or the channel is being \
         woken late. Compare its send-call timing against its interval."
            .to_owned()
    } else if failed > 0 {
        // Cumulative, so this fault may have recovered long ago, and failure
        // timing is not correlated with miss timing. It remains a useful place
        // to check only after stronger current and measured evidence has been
        // exhausted.
        format!(
            "Missed sends: {} sends failed earlier in this run, but the timing does not show \
             whether those failures coincided with these misses — check Send outcomes above.",
            thousands(failed)
        )
    } else {
        let capacity = if service.is_some() {
            "the application capacity estimate does not point to overload"
        } else {
            "application capacity could not be estimated"
        };
        format!(
            "Missed sends: no message delayed another, and {capacity} — compare render and \
             send-call timing in Timing & runtime details."
        )
    };

    Some(DecisionSignal {
        text,
        tone: SignalTone::Warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::telemetry::MessageTiming;

    use wiredata_ui::diagnostics::SignalTone;

    /// The routing exists because the message showing the misses is rarely the
    /// one causing them. Each branch names a place to look, in decisiveness
    /// order, and none of them fires when nothing was skipped.
    #[test]
    fn missed_send_routing_names_a_cause_in_decisiveness_order() {
        let blocker = MessageTiming {
            blocked_others: Duration::from_millis(430),
            blocking_sends: 4,
            longest_block: Duration::from_millis(120),
            ..MessageTiming::default()
        };
        let per_message = [MessageTiming::default(), blocker];
        let evidence = |missed, interface_erroring, failed, oversubscribed| MissedSendEvidence {
            missed,
            interface_erroring,
            failed,
            serial_oversubscribed: oversubscribed,
            service: None,
        };

        // Nothing skipped: no line at all.
        assert!(missed_send_routing(&evidence(0, false, 0, false), &per_message).is_none());

        // A *live* interface fault outranks everything because it is actionable
        // now, without claiming that it caused the misses.
        let failing = missed_send_routing(&evidence(12, true, 3, true), &per_message).unwrap();
        assert!(failing.text.contains("failing right now"), "{failing:?}");
        assert_eq!(failing.tone, SignalTone::Warning);

        // The same failure count with no current error is uncorrelated history;
        // it must not hide the stronger delay observation below.
        let recovered = missed_send_routing(&evidence(12, false, 3, false), &per_message).unwrap();
        assert!(
            recovered.text.contains("check message #2 first")
                && !recovered.text.contains("failed earlier"),
            "historical failures must not hide measured delay evidence: {recovered:?}"
        );

        // A schedule the wire cannot carry. No settings-vs-running qualifier is
        // needed any more: capacity is calculated from the running schedule, so
        // an unapplied edit cannot reach this line.
        let oversubscribed =
            missed_send_routing(&evidence(12, false, 3, true), &per_message).unwrap();
        assert!(oversubscribed.text.contains("cannot carry this schedule"));

        // The blocking message: routed to, not convicted, and the elapsed hold
        // is stated separately from the combined waiting it caused — the latter
        // sums across victims and can exceed the send itself.
        let blocked = missed_send_routing(&evidence(12, false, 0, false), &per_message).unwrap();
        assert_eq!(
            blocked.text,
            "Missed sends: check message #2 first — its longest send held the channel 120 ms, \
             causing 430 ms of combined waiting across other messages in 4 sends. See Per-message \
             timing."
        );

        // One active message cannot block another, so the blocking branch is
        // structurally unreachable. Reporting its absence as a finding is the
        // non-sequitur this branch exists to avoid.
        let active = MessageTiming {
            interval: Duration::from_millis(50),
            ..MessageTiming::default()
        };
        let alone = missed_send_routing(
            &evidence(12, false, 0, false),
            &[active, MessageTiming::default()],
        )
        .unwrap();
        assert!(
            alone.text.contains("one active message"),
            "a single-message channel must not be told no message stands out: {alone:?}"
        );

        // Two active messages, neither blocking: now the absence really is the
        // finding, and the line says so without naming a message.
        let second = MessageTiming {
            interval: Duration::from_millis(80),
            ..MessageTiming::default()
        };
        let unexplained =
            missed_send_routing(&evidence(12, false, 0, false), &[active, second]).unwrap();
        assert!(
            unexplained
                .text
                .contains("application capacity could not be estimated"),
            "unavailable capacity must not be reported as a passed limit: {unexplained:?}"
        );

        // A non-overloaded estimate is still only an estimate, not proof that
        // no capacity limit was reached.
        let estimated = missed_send_routing(
            &MissedSendEvidence {
                service: Some(ServiceEstimate {
                    samples: 20,
                    summed_p99_upper_bounds: Duration::from_millis(1),
                    capacity_messages_per_second: 1_000.0,
                    utilization: 0.5,
                }),
                ..evidence(12, false, 0, false)
            },
            &[active, second],
        )
        .unwrap();
        assert!(
            estimated
                .text
                .contains("capacity estimate does not point to overload"),
            "an advisory estimate must stay qualified: {estimated:?}"
        );

        // Only after the current and measured leads are exhausted does an old
        // failure total get its own line.
        let recovered_only =
            missed_send_routing(&evidence(12, false, 3, false), &[active, second]).unwrap();
        assert!(
            recovered_only.text.contains("failed earlier in this run"),
            "the remaining historical clue should still be offered: {recovered_only:?}"
        );

        // No active message must not fall through the single-message wording.
        let none_active =
            missed_send_routing(&evidence(12, false, 0, false), &[MessageTiming::default()])
                .unwrap();
        assert!(
            !none_active.text.contains("one active message"),
            "zero active messages must not be described as one: {none_active:?}"
        );
    }

    /// The one branch entitled to state an amount rather than a lead, and the
    /// two things that keep it honest: every attributed miss is accounted for,
    /// and the unmatched remainder is described as unknown rather than as idle
    /// time the measurement never observed.
    #[test]
    fn measured_misses_state_an_amount_without_overclaiming_the_remainder() {
        let culprit = MessageTiming {
            blocked_others: Duration::from_millis(430),
            blocking_sends: 4,
            longest_block: Duration::from_millis(120),
            missed_others: 9,
            ..MessageTiming::default()
        };
        let per_message = [MessageTiming::default(), culprit];
        let evidence = |missed| MissedSendEvidence {
            missed,
            interface_erroring: false,
            failed: 0,
            serial_oversubscribed: false,
            service: None,
        };

        // Every miss accounted for. Note this same input routes to the
        // delay-based lead when `missed_others` is zero, above.
        let all = missed_send_routing(&evidence(9), &per_message).unwrap();
        assert_eq!(
            all.text,
            "Missed sends: for all 9, Talker was waiting for another message's serial or network \
             send to finish; it was message #2 every time. Its longest send held the channel 120 \
             ms. See Per-message timing."
        );

        // A recovered failure is only a run-wide lead. It cannot hide the
        // exact overlap measured for these misses.
        let with_failure_history = missed_send_routing(
            &MissedSendEvidence {
                failed: 3,
                ..evidence(9)
            },
            &per_message,
        )
        .unwrap();
        assert_eq!(with_failure_history.text, all.text);

        // Three the record cannot place. Unmatched is not idle — a late wake
        // and work outside the send call leave the same gap as a free thread,
        // and this line must not pick one of those.
        let partial = missed_send_routing(&evidence(12), &per_message).unwrap();
        assert!(
            partial.text.contains(
                "For the other 3, no serial or network send was recorded as being in progress"
            ),
            "the shortfall must state only what the timing established: {partial:?}"
        );
        assert!(
            partial
                .text
                .contains("does not mean the channel was idle; the available timing cannot identify the cause"),
            "the measurement cannot see an idle channel, so it must state that limit: {partial:?}"
        );
    }

    /// Every attributed message has to survive into the sentence. Naming only
    /// the largest culprit and the shortfall silently dropped the rest: with five
    /// misses on #2, four on #3 and three unmatched, the line accounted for
    /// eight of twelve and read as though it had covered all of them.
    #[test]
    fn the_routing_line_accounts_for_every_attributed_miss() {
        let hold = Duration::from_millis(120);
        let per_message = [
            MessageTiming::default(),
            MessageTiming {
                missed_others: 5,
                longest_block: hold,
                ..MessageTiming::default()
            },
            MessageTiming {
                missed_others: 4,
                longest_block: hold,
                ..MessageTiming::default()
            },
        ];
        let routing = missed_send_routing(
            &MissedSendEvidence {
                missed: 12,
                interface_erroring: false,
                failed: 0,
                serial_oversubscribed: false,
                service: None,
            },
            &per_message,
        )
        .unwrap();

        // The attributed total, the run total, the largest single share, and the
        // part that could not be placed — all four, or the arithmetic does not
        // close for the reader.
        assert!(
            routing
                .text
                .contains("for 9 of 12, Talker was waiting for another message's serial or network send to finish"),
            "{routing:?}"
        );
        assert!(
            routing
                .text
                .contains("message #2 accounts for the largest share (5)"),
            "{routing:?}"
        );
        assert!(routing.text.contains("For the other 3"), "{routing:?}");
    }

    /// The expanded help must keep the exact missed-send count separate from a
    /// delay-based lead while speaking in terms already visible to a technician.
    #[test]
    fn missed_send_limits_distinguish_evidence_without_internal_vocabulary() {
        assert!(MISSED_ROUTING_LIMITS.contains("A count beside a message"));
        assert!(MISSED_ROUTING_LIMITS.contains("“Check message #…”"));
        assert!(MISSED_ROUTING_LIMITS
            .contains("No serial or network send was recorded as being in progress"));
        assert!(MISSED_ROUTING_LIMITS.contains("current send failure"));
        assert!(MISSED_ROUTING_LIMITS.contains("application capacity is an estimate"));

        for internal in [
            "cadence point",
            "charged",
            "interface write",
            "retained history",
            "window",
            "projection",
        ] {
            assert!(
                !MISSED_ROUTING_LIMITS.contains(internal),
                "technician-facing help contains internal term {internal:?}"
            );
        }
    }
}
