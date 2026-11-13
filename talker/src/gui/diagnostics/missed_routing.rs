//! Where to look when scheduled sends are being skipped.
//!
//! Mostly a router. One branch may state an amount, because misses matched to a
//! send as they happen are evidence about the misses themselves (ADR-051); the
//! verbs carry which kind of claim is being made.

use super::*;

/// The hover's heading, the same on every branch.
const MISSED_SEND_HEADING: &str = "A missed send is a scheduled send that never happened.";

/// What a missed send is. Every branch's hover opens with it, because the
/// callout is often the first place a reader meets the term.
const WHAT_A_MISS_IS: &str = "When a message falls more than one interval behind its schedule, \
     the channel sends it once, late, and skips the scheduled times it has already passed \
     instead of sending them in a burst to catch up. Each skipped time is one missed send. \
     The count is also on the Send outcomes line.";

/// Unmatched is not idle: a late wake, work outside the send call and a free
/// thread leave the same gap, so the hover names the likely two and claims
/// neither.
const NO_SEND_IN_PROGRESS: &str = "No message was being sent when these sends came due. That does \
     not mean the channel was idle: it may have woken late from its timer, or been busy \
     preparing a message to send.";

/// The missed-send callout: what was found, and a hover that says how far this
/// particular finding can be trusted.
///
/// The hover follows the branch rather than covering every branch at once. One
/// shared text explained phrases the reader could not see on the line above
/// it, which is how it came to read as a list of riddles.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::gui) struct MissedSendRouting {
    pub(in crate::gui) text: String,
    pub(in crate::gui) hover: Hover,
}

impl MissedSendRouting {
    fn new(text: impl Into<String>, explanation: &[&str]) -> Self {
        Self {
            text: text.into(),
            hover: Hover::from_paragraphs(
                [MISSED_SEND_HEADING, WHAT_A_MISS_IS]
                    .into_iter()
                    .chain(explanation.iter().copied()),
            ),
        }
    }
}

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

/// "1 send" or "12 sends", with a thousands separator on the count.
fn sends(count: u64) -> String {
    let noun = if count == 1 { "send" } else { "sends" };
    format!("{} {noun}", thousands(count))
}

/// The amount-stating branch: how many sends were skipped while some message
/// was being sent, which message accounts for most of them, and how many
/// remain with no known cause, so the numbers add up.
fn measured_routing(
    missed: u64,
    index: usize,
    message: &MessageTiming,
    attributed: u64,
) -> MissedSendRouting {
    let number = index + 1;
    // ", whose longest send took 120 ms", or nothing when no send was seen to
    // hold the channel.
    let longest = if message.longest_block.is_zero() {
        String::new()
    } else {
        format!(
            ", whose longest send took {}",
            compact_duration(message.longest_block)
        )
    };
    let total = if missed == 1 {
        "1 scheduled send was missed".to_owned()
    } else {
        format!("{} scheduled sends were missed", thousands(missed))
    };

    // Who the channel was busy with, phrased to follow "while".
    let only_one_message = message.missed_others == attributed;
    let busy = if only_one_message {
        format!("the channel was busy sending message #{number}{longest}")
    } else {
        format!(
            "the channel was busy sending another message, most of them ({}) while it was \
             sending message #{number}{longest}",
            thousands(message.missed_others)
        )
    };

    // With a remainder, one line each for the total, the measured part and the
    // remainder, so the arithmetic can be read down the callout.
    let text = if attributed >= missed {
        let all = if missed == 1 { "" } else { "all " };
        format!("{total}, {all}while {busy}.")
    } else {
        format!(
            "{total}.\n{} came due while {busy}.\nNo cause is known for the remaining {}.",
            thousands(attributed),
            thousands(missed - attributed)
        )
    };

    let counted = if only_one_message {
        "These sends came due, and were skipped, while the channel was busy sending the message \
         named. That is measured as each one happens, not inferred afterwards."
    } else {
        "A count beside a message is measured: those sends came due, and were skipped, while the \
         channel was busy sending that message. It is recorded as each one happens, not \
         inferred afterwards."
    };
    let table = "Per-message timing lists every message's figures; the longest send is its \
                 Longest block column.";
    let mut explanation = vec![counted, table];
    if attributed < missed {
        explanation.push(
            "\"No cause is known\" means no message was being sent when those sends came due. \
             That does not mean the channel was idle: it may have woken late from its timer, or \
             been busy preparing a message to send.",
        );
    }
    MissedSendRouting::new(text, &explanation)
}

/// Where to look when scheduled sends are being skipped.
///
/// Mostly a router, occasionally a verdict, and the wording says which. One
/// input *is* measured at the instant a send was skipped —
/// [`MessageTiming::missed_others`], attributed to whichever send held the
/// thread as each point passed (ADR-051) — and where that lands on a message the
/// text states an amount rather than a place to look. Everything else here is a
/// run total or evidence from deadlines that were reached, so those branches
/// keep the hedged verbs: "check", "may".
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
) -> Option<MissedSendRouting> {
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
    let measured = per_message
        .iter()
        .enumerate()
        .filter(|(_, message)| message.missed_others > 0)
        .max_by_key(|(_, message)| message.missed_others);
    let active_messages = per_message
        .iter()
        .filter(|message| !message.interval.is_zero())
        .count();

    let routing =
        if interface_erroring {
            MissedSendRouting::new(
            "Sends are being missed, and the interface is failing right now. Fix the interface \
             first; its failures are counted on the Send outcomes line.",
            &["The interface comes first because it can be fixed now. That does not prove it \
               caused the missed sends."],
        )
        } else if serial_oversubscribed {
            MissedSendRouting::new(
            "Sends are being missed because the serial line cannot carry this schedule: it asks \
             for more bytes per second than the baud rate allows. See Capacity.",
            &["No change elsewhere can fix this. Send less often, send shorter messages, or \
               raise the baud rate if the device allows it."],
        )
        } else if let Some((index, message)) = measured {
            // The one branch entitled to state an amount rather than a lead: every
            // point counted here was attributed while some message's send held the
            // thread.
            let attributed: u64 = per_message
                .iter()
                .map(|message| message.missed_others)
                .sum();
            measured_routing(missed, index, message, attributed)
        } else if let Some((index, message)) = blocker {
            let number = index + 1;
            MissedSendRouting::new(
                format!(
                    "Sends are being missed. Check message #{number} first: its sends held up the \
                 other messages, and its longest took {}.",
                    compact_duration(message.longest_block),
                ),
                &[&format!(
                    "Message #{number} is named because its sends were seen holding up the other \
                 messages. That makes it a good place to start, but it is not proof that it \
                 caused these missed sends."
                )],
            )
        } else if service.is_some_and(|estimate| estimate.headroom_factor() < 1.0) {
            // Still a projection even with the running schedule as its input: it
            // divides summed p99 bounds into a requested rate, so "may" stays the
            // strongest honest verb.
            MissedSendRouting::new(
            "Sends are being missed. Rendering and sending each message may take longer than \
             this rate allows. See Capacity.",
            &["This is an estimate from recent render and send times, not a measurement of the \
               missed sends themselves."],
        )
        } else if active_messages == 1 {
            // With one active message there is nothing else to hold the channel, so
            // the blocking branch above can never fire. What is ruled out is only
            // *another message* being the cause; the two candidates that remain are
            // the useful half of that.
            MissedSendRouting::new(
            "Sends are being missed. This message's send may take longer than its interval, or \
             the channel may be waking late. Compare its Send call time with its interval under \
             Per-message timing.",
            &["With only one message sending, no other message can be holding it up."],
        )
        } else if failed > 0 {
            // Cumulative, so this fault may have recovered long ago, and failure
            // timing is not correlated with miss timing. It remains a useful place
            // to check only after stronger current and measured evidence has been
            // exhausted.
            MissedSendRouting::new(
                format!(
                "Sends are being missed, and the timing does not show why. {} failed earlier in \
                 this run; see Send outcomes.",
                sends(failed)
            ),
                &[
                    "Those failures may be from a problem that has since cleared, so they are a \
                 clue rather than a cause.",
                    NO_SEND_IN_PROGRESS,
                ],
            )
        } else {
            MissedSendRouting::new(
            "Sends are being missed, and the timing does not show why. Compare render and send \
             times under Timing & runtime details.",
            &[NO_SEND_IN_PROGRESS],
        )
        };

    Some(routing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::telemetry::MessageTiming;

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
        assert_eq!(
            failing.text,
            "Sends are being missed, and the interface is failing right now. Fix the interface \
             first; its failures are counted on the Send outcomes line."
        );
        assert!(
            failing
                .hover
                .text()
                .contains("That does not prove it caused"),
            "{failing:?}"
        );

        // The same failure count with no current error is uncorrelated history;
        // it must not hide the stronger delay observation below.
        let recovered = missed_send_routing(&evidence(12, false, 3, false), &per_message).unwrap();
        assert!(
            recovered.text.contains("Check message #2 first")
                && !recovered.text.contains("failed earlier"),
            "historical failures must not hide measured delay evidence: {recovered:?}"
        );

        // A schedule the wire cannot carry. Capacity is calculated from the
        // running schedule, so an unapplied edit cannot reach this line.
        let oversubscribed =
            missed_send_routing(&evidence(12, false, 3, true), &per_message).unwrap();
        assert_eq!(
            oversubscribed.text,
            "Sends are being missed because the serial line cannot carry this schedule: it asks \
             for more bytes per second than the baud rate allows. See Capacity."
        );

        // The blocking message: routed to, not blamed, and only the elapsed
        // hold is stated.
        let blocked = missed_send_routing(&evidence(12, false, 0, false), &per_message).unwrap();
        assert_eq!(
            blocked.text,
            "Sends are being missed. Check message #2 first: its sends held up the other \
             messages, and its longest took 120 ms."
        );
        assert!(
            blocked.hover.text().contains("it is not proof"),
            "a delay-based lead says it is a lead: {blocked:?}"
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
        assert_eq!(
            alone.text,
            "Sends are being missed. This message's send may take longer than its interval, or \
             the channel may be waking late. Compare its Send call time with its interval under \
             Per-message timing."
        );

        // Two active messages, neither blocking: now the absence really is the
        // finding, and the line says so without naming a message.
        let second = MessageTiming {
            interval: Duration::from_millis(80),
            ..MessageTiming::default()
        };
        let unexplained =
            missed_send_routing(&evidence(12, false, 0, false), &[active, second]).unwrap();
        assert_eq!(
            unexplained.text,
            "Sends are being missed, and the timing does not show why. Compare render and send \
             times under Timing & runtime details."
        );
        assert!(
            unexplained
                .hover
                .text()
                .contains("That does not mean the channel was idle"),
            "{unexplained:?}"
        );

        // A passing estimate is not a finding the line may state.
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
        assert_eq!(
            estimated, unexplained,
            "a passing capacity estimate must not change the line"
        );

        // An estimate that says the rate is too slow is a lead, and says "may".
        let too_slow = missed_send_routing(
            &MissedSendEvidence {
                service: Some(ServiceEstimate {
                    samples: 20,
                    summed_p99_upper_bounds: Duration::from_millis(100),
                    capacity_messages_per_second: 10.0,
                    utilization: 2.0,
                }),
                ..evidence(12, false, 0, false)
            },
            &[active, second],
        )
        .unwrap();
        assert_eq!(
            too_slow.text,
            "Sends are being missed. Rendering and sending each message may take longer than \
             this rate allows. See Capacity."
        );

        // Only after the current and measured leads are exhausted does an old
        // failure total get its own line.
        let recovered_only =
            missed_send_routing(&evidence(12, false, 3, false), &[active, second]).unwrap();
        assert_eq!(
            recovered_only.text,
            "Sends are being missed, and the timing does not show why. 3 sends failed earlier \
             in this run; see Send outcomes."
        );
        let one_failure =
            missed_send_routing(&evidence(12, false, 1, false), &[active, second]).unwrap();
        assert!(
            one_failure
                .text
                .contains(" 1 send failed earlier in this run"),
            "{one_failure:?}"
        );

        // No active message must not fall through the single-message wording.
        let none_active =
            missed_send_routing(&evidence(12, false, 0, false), &[MessageTiming::default()])
                .unwrap();
        assert!(
            !none_active.text.contains("This message's send"),
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
            "9 scheduled sends were missed, all while the channel was busy sending message #2, \
             whose longest send took 120 ms."
        );
        assert!(
            !all.hover.text().contains("No cause is known"),
            "nothing is left unplaced, so the hover does not explain a remainder: {all:?}"
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
        assert_eq!(with_failure_history, all);

        // Three the record cannot place. Unmatched is not idle: the line says
        // only that no cause is known, and the hover says what that means.
        let partial = missed_send_routing(&evidence(12), &per_message).unwrap();
        assert_eq!(
            partial.text,
            "12 scheduled sends were missed.\n\
             9 came due while the channel was busy sending message #2, whose longest send took \
             120 ms.\n\
             No cause is known for the remaining 3."
        );
        assert!(
            partial
                .hover
                .text()
                .contains("That does not mean the channel was idle"),
            "{partial:?}"
        );

        // A single missed send reads as one.
        let single = missed_send_routing(
            &evidence(1),
            &[MessageTiming {
                missed_others: 1,
                longest_block: Duration::from_millis(120),
                ..MessageTiming::default()
            }],
        )
        .unwrap();
        assert_eq!(
            single.text,
            "1 scheduled send was missed, while the channel was busy sending message #1, whose \
             longest send took 120 ms."
        );

        // A message whose send never held the channel has no longest to state.
        let no_hold = missed_send_routing(
            &evidence(9),
            &[MessageTiming {
                missed_others: 9,
                ..MessageTiming::default()
            }],
        )
        .unwrap();
        assert_eq!(
            no_hold.text,
            "9 scheduled sends were missed, all while the channel was busy sending message #1."
        );

        // Thousands separators on counts.
        let big = missed_send_routing(
            &evidence(12_345),
            &[MessageTiming {
                missed_others: 12_345,
                ..MessageTiming::default()
            }],
        )
        .unwrap();
        assert!(
            big.text
                .starts_with("12,345 scheduled sends were missed, all while"),
            "{big:?}"
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
        let evidence = |missed| MissedSendEvidence {
            missed,
            interface_erroring: false,
            failed: 0,
            serial_oversubscribed: false,
            service: None,
        };

        // The run total, the attributed total, the largest single share, and
        // the part that could not be placed — all four, each on its own line
        // where there is a remainder, or the arithmetic does not close for the
        // reader.
        let routing = missed_send_routing(&evidence(12), &per_message).unwrap();
        assert_eq!(
            routing.text,
            "12 scheduled sends were missed.\n\
             9 came due while the channel was busy sending another message, most of them (5) \
             while it was sending message #2, whose longest send took 120 ms.\n\
             No cause is known for the remaining 3."
        );

        // Every miss placed: nothing is left for "no known cause".
        let placed = missed_send_routing(&evidence(9), &per_message).unwrap();
        assert_eq!(
            placed.text,
            "9 scheduled sends were missed, all while the channel was busy sending another \
             message, most of them (5) while it was sending message #2, whose longest send took \
             120 ms."
        );
    }

    /// Every branch's hover opens with what a missed send is, and keeps to
    /// terms already visible to a technician.
    #[test]
    fn every_missed_send_hover_explains_the_term_without_internal_vocabulary() {
        let active = |ms| MessageTiming {
            interval: Duration::from_millis(ms),
            ..MessageTiming::default()
        };
        let evidence =
            |interface_erroring, failed, serial_oversubscribed, service| MissedSendEvidence {
                missed: 12,
                interface_erroring,
                failed,
                serial_oversubscribed,
                service,
            };
        let too_slow = Some(ServiceEstimate {
            samples: 20,
            summed_p99_upper_bounds: Duration::from_millis(100),
            capacity_messages_per_second: 10.0,
            utilization: 2.0,
        });
        let measured = MessageTiming {
            missed_others: 9,
            longest_block: Duration::from_millis(120),
            ..active(50)
        };
        let blocking = MessageTiming {
            blocked_others: Duration::from_millis(430),
            longest_block: Duration::from_millis(120),
            ..active(50)
        };
        let routings = [
            (evidence(true, 0, false, None), vec![active(50), active(80)]),
            (evidence(false, 0, true, None), vec![active(50), active(80)]),
            (evidence(false, 0, false, None), vec![measured, active(80)]),
            (evidence(false, 0, false, None), vec![blocking, active(80)]),
            (
                evidence(false, 0, false, too_slow),
                vec![active(50), active(80)],
            ),
            (evidence(false, 0, false, None), vec![active(50)]),
            (
                evidence(false, 3, false, None),
                vec![active(50), active(80)],
            ),
            (
                evidence(false, 0, false, None),
                vec![active(50), active(80)],
            ),
        ]
        .map(|(evidence, per_message)| missed_send_routing(&evidence, &per_message).unwrap());

        // Eight inputs, eight different lines: every branch is covered.
        let mut texts: Vec<&str> = routings
            .iter()
            .map(|routing| routing.text.as_str())
            .collect();
        texts.sort_unstable();
        texts.dedup();
        assert_eq!(texts.len(), 8, "{texts:#?}");

        for routing in &routings {
            let paragraphs: Vec<&str> = routing.hover.text().split("\n\n").collect();
            assert_eq!(paragraphs[0], MISSED_SEND_HEADING);
            assert_eq!(paragraphs[1], WHAT_A_MISS_IS);
            assert!(
                paragraphs.len() >= 3,
                "each branch explains its own line: {routing:?}"
            );

            let lowered = routing.hover.text().to_lowercase();
            let words: Vec<&str> = lowered
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .collect();
            for internal in [
                "queue",
                "thread",
                "handle",
                "ring",
                "charged",
                "window",
                "projection",
                "attributed",
            ] {
                assert!(
                    !words.contains(&internal),
                    "technician-facing help contains internal term {internal:?}: {routing:?}"
                );
            }
            for internal in ["cadence point", "interface write", "retained history"] {
                assert!(
                    !lowered.contains(internal),
                    "technician-facing help contains internal term {internal:?}: {routing:?}"
                );
            }
        }
    }
}
