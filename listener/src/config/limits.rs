//! The limits a profile and the runtime both enforce (ADR-048, §71).
//!
//! They live in one place so validation and the runtime's constructors cannot
//! disagree. Together they give each Channel a stated worst case of about
//! 50 MiB: a 16 MiB receive queue, two 8 MiB recording queues, 16 MiB of
//! scrollback and about 2 MiB of diagnostics (ADR-048).

use super::{
    DiskGuard, DiskThreshold, MatchAction, MatchCondition, MatchRule, ReconnectPolicy,
    RecordControl, RecordTarget, RetentionConfig, MIN_SIZE_CAP,
};

/// Stream scrollback per Channel (§71, §88).
pub const MAX_SCROLLBACK_BYTES: usize = 16 * 1024 * 1024;
/// Retained diagnostics per severity (§71, §88).
pub const MAX_DIAGNOSTICS_PER_SEVERITY: usize = 2_000;
/// One match pattern (§71).
pub const MAX_MATCH_PATTERN_BYTES: usize = 256;
/// Match rules per Channel (§71).
pub const MAX_MATCH_RULES: usize = 64;
/// Actions on one match rule (§71). With the limit on Mark text, this bounds
/// what one firing can cost, so the firing limit bounds one read (ADR-065).
pub const MAX_MATCH_ACTIONS: usize = 16;
/// The text a timestamped Mark adds after its timestamp (§71).
pub const MAX_MARK_SEPARATOR_BYTES: usize = 256;
/// The reconnect backoff multiplier's range (§71).
pub const RECONNECT_MULTIPLIER: std::ops::RangeInclusive<f64> = 1.0..=10.0;
/// The receive queue, in chunks of up to 64 KiB: 16 MiB at most (ADR-048).
pub const MAX_INGEST_CHUNKS: usize = 256;
/// Each recording's queue, in bytes (ADR-043, ADR-048).
pub const MAX_RECORDING_QUEUE_BYTES: usize = 8 * 1024 * 1024;
/// The runtime-to-UI event queue, shared by every Channel. An event is small,
/// so this bounds how large a burst may queue rather than memory.
pub const MAX_UI_EVENTS: usize = 4_096;

/// A setting outside its limit (ADR-048). Each names the setting, the value
/// found and the limit, so the message says what to change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LimitError {
    #[error(
        "scrollback of {found} bytes is over the limit of 16 MiB ({MAX_SCROLLBACK_BYTES} bytes)"
    )]
    Scrollback { found: usize },
    #[error("{severity} retention of {found} is over the limit of {MAX_DIAGNOSTICS_PER_SEVERITY}")]
    Diagnostics {
        severity: &'static str,
        found: usize,
    },
    #[error(
        "match rule \"{rule}\" has a {found}-byte pattern; the limit is {MAX_MATCH_PATTERN_BYTES} bytes"
    )]
    MatchPattern { rule: String, found: usize },
    #[error("{found} match rules; the limit is {MAX_MATCH_RULES} per Channel")]
    MatchRules { found: usize },
    #[error("match rule \"{rule}\" has {found} actions; the limit is {MAX_MATCH_ACTIONS}")]
    MatchActions { rule: String, found: usize },
    #[error(
        "match rule \"{rule}\" has a {found}-byte Mark separator; the limit is \
         {MAX_MARK_SEPARATOR_BYTES} bytes"
    )]
    MarkSeparator { rule: String, found: usize },
    #[error(
        "match rules \"{stop}\" and \"{begin}\" overlap. A match of the Stop pattern can end \
         inside a match of the Begin pattern, so what is recorded would depend on how the \
         data arrived. Change one of the two patterns so that they cannot overlap that way."
    )]
    TriggerOverlap { stop: String, begin: String },
    #[error("reconnect initial backoff of {initial} ms is longer than its maximum of {max} ms")]
    BackoffOrder { initial: u64, max: u64 },
    /// The multiplier as written, since a float has no `Eq`.
    #[error("reconnect multiplier {found} is outside 1.0–10")]
    BackoffMultiplier { found: String },
    #[error(
        "{recording} recording size cap of {found} bytes is under the minimum of 64 MiB \
         ({MIN_SIZE_CAP} bytes)"
    )]
    SizeCap { recording: &'static str, found: u64 },
    #[error(
        "{recording} recording's low-disk threshold of {found} percent is outside 1–90; \
         recording resumes only at 10% above it, so a higher one never resumes"
    )]
    DiskGuardPercent { recording: &'static str, found: u8 },
    #[error("{recording} recording's low-disk threshold must be more than 0 bytes")]
    DiskGuardBytes { recording: &'static str },
    #[error("{what} of {found} is outside 1–{max}")]
    Capacity {
        what: &'static str,
        found: usize,
        max: usize,
    },
}

impl RetentionConfig {
    /// The limits this retention breaks (§71, §88).
    pub fn limit_errors(&self) -> Vec<LimitError> {
        let mut errors = Vec::new();
        if let Some(found) = self
            .byte_limit
            .filter(|&bytes| bytes > MAX_SCROLLBACK_BYTES)
        {
            errors.push(LimitError::Scrollback { found });
        }
        for (severity, limit) in [
            ("event", self.event_limit),
            ("warning", self.warning_limit),
            ("error", self.error_limit),
        ] {
            if let Some(found) = limit.filter(|&n| n > MAX_DIAGNOSTICS_PER_SEVERITY) {
                errors.push(LimitError::Diagnostics { severity, found });
            }
        }
        errors
    }
}

impl ReconnectPolicy {
    /// The limits this policy breaks (§71).
    pub fn limit_errors(&self) -> Vec<LimitError> {
        let mut errors = Vec::new();
        if self.initial_backoff_ms > self.max_backoff_ms {
            errors.push(LimitError::BackoffOrder {
                initial: self.initial_backoff_ms,
                max: self.max_backoff_ms,
            });
        }
        // `contains` is false for NaN, so it is refused too.
        if !RECONNECT_MULTIPLIER.contains(&self.multiplier) {
            errors.push(LimitError::BackoffMultiplier {
                found: self.multiplier.to_string(),
            });
        }
        errors
    }
}

/// The limits a Channel's match rules break (§71).
pub fn match_rule_errors(rules: &[MatchRule]) -> Vec<LimitError> {
    let mut errors = Vec::new();
    if rules.len() > MAX_MATCH_RULES {
        errors.push(LimitError::MatchRules { found: rules.len() });
    }
    for rule in rules {
        if let MatchCondition::BytePattern { pattern } = &rule.condition {
            if pattern.len() > MAX_MATCH_PATTERN_BYTES {
                errors.push(LimitError::MatchPattern {
                    rule: rule.name.clone(),
                    found: pattern.len(),
                });
            }
        }
        if rule.actions.len() > MAX_MATCH_ACTIONS {
            errors.push(LimitError::MatchActions {
                rule: rule.name.clone(),
                found: rule.actions.len(),
            });
        }
        for action in &rule.actions {
            if let MatchAction::Mark {
                timestamp: Some(timestamp),
            } = action
            {
                if timestamp.separator.len() > MAX_MARK_SEPARATOR_BYTES {
                    errors.push(LimitError::MarkSeparator {
                        rule: rule.name.clone(),
                        found: timestamp.separator.len(),
                    });
                }
            }
        }
    }
    errors.extend(trigger_overlaps(rules));
    errors
}

/// Pairs of rules whose Record actions would record different bytes depending
/// on how the data arrived (ADR-064).
///
/// A Record action takes effect at its match: a Begin at the match's first
/// byte, a Stop after its last. The two are put in stream order only among the
/// matches of one read, and a match fires in the read where it completes. A
/// Stop that ends inside a Begin's match, for the same recording, completes
/// first; whether both land in one read is then up to the transport, and the
/// recording differs. Every other arrangement gives one result.
fn trigger_overlaps(rules: &[MatchRule]) -> Vec<LimitError> {
    let triggers = |control: RecordControl| {
        rules.iter().filter_map(move |rule| {
            let MatchCondition::BytePattern { pattern } = &rule.condition else {
                return None;
            };
            let targets: Vec<RecordTarget> = rule
                .actions
                .iter()
                .filter_map(|action| match action {
                    MatchAction::Record { target, control: c } if *c == control => Some(*target),
                    _ => None,
                })
                .collect();
            (!targets.is_empty()).then_some((rule, pattern.as_slice(), targets))
        })
    };
    let shares_a_recording = |a: RecordTarget, b: RecordTarget| {
        let raw = |t| matches!(t, RecordTarget::Raw | RecordTarget::Both);
        let display = |t| matches!(t, RecordTarget::Display | RecordTarget::Both);
        (raw(a) && raw(b)) || (display(a) && display(b))
    };
    let mut errors = Vec::new();
    for (stop_rule, stop, stop_targets) in triggers(RecordControl::Stop) {
        for (begin_rule, begin, begin_targets) in triggers(RecordControl::Begin) {
            let same_recording = stop_targets
                .iter()
                .any(|&s| begin_targets.iter().any(|&b| shares_a_recording(s, b)));
            if same_recording && stop_can_end_inside_begin(stop, begin) {
                errors.push(LimitError::TriggerOverlap {
                    stop: stop_rule.name.clone(),
                    begin: begin_rule.name.clone(),
                });
            }
        }
    }
    errors
}

/// Whether some stream holds a match of `stop` that ends strictly inside a
/// match of `begin`: after its first byte and before its last.
///
/// Take the Stop ending `end` bytes into the Begin, for each `end` from 1 to
/// one short of the Begin's length. The bytes the two would share are the
/// last `min(end, stop.len())` of the Stop and the Begin's bytes just before
/// `end`; the arrangement exists when those agree.
fn stop_can_end_inside_begin(stop: &[u8], begin: &[u8]) -> bool {
    if stop.is_empty() || begin.len() < 2 {
        return false;
    }
    (1..begin.len()).any(|end| {
        let shared = end.min(stop.len());
        begin[end - shared..end] == stop[stop.len() - shared..]
    })
}

/// A configured recording size cap under the minimum (§59). The runtime never
/// uses one, since it raises the cap to the minimum; this reports it.
pub fn size_cap_error(recording: &'static str, size_cap: Option<u64>) -> Option<LimitError> {
    size_cap
        .filter(|&found| found < MIN_SIZE_CAP)
        .map(|found| LimitError::SizeCap { recording, found })
}

/// A recording's low-disk threshold that cannot work (§56.2, §71). Resuming
/// takes 1.1 times the threshold free, so a percentage over 90 could never
/// resume and 0 bytes could never count as low.
pub fn disk_guard_error(recording: &'static str, guard: Option<DiskGuard>) -> Option<LimitError> {
    match guard?.min_free {
        DiskThreshold::Percent { percent } if !(1..=90).contains(&percent) => {
            Some(LimitError::DiskGuardPercent {
                recording,
                found: percent,
            })
        }
        DiskThreshold::Bytes { bytes: 0 } => Some(LimitError::DiskGuardBytes { recording }),
        _ => None,
    }
}

/// A queue capacity must hold something and stay within its bound.
pub fn capacity_error(what: &'static str, found: usize, max: usize) -> Option<LimitError> {
    (found == 0 || found > max).then_some(LimitError::Capacity { what, found, max })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_is_held_to_its_limits() {
        let at_limits = RetentionConfig {
            byte_limit: Some(MAX_SCROLLBACK_BYTES),
            event_limit: Some(2_000),
            warning_limit: Some(2_000),
            error_limit: Some(2_000),
        };
        assert_eq!(at_limits.limit_errors(), []);

        let over = RetentionConfig {
            byte_limit: Some(MAX_SCROLLBACK_BYTES + 1),
            event_limit: Some(2_001),
            warning_limit: None,
            error_limit: Some(10_000),
        };
        assert_eq!(
            over.limit_errors(),
            [
                LimitError::Scrollback {
                    found: MAX_SCROLLBACK_BYTES + 1
                },
                LimitError::Diagnostics {
                    severity: "event",
                    found: 2_001
                },
                LimitError::Diagnostics {
                    severity: "error",
                    found: 10_000
                },
            ]
        );
        assert_eq!(
            over.limit_errors()[1].to_string(),
            "event retention of 2001 is over the limit of 2000"
        );
    }

    #[test]
    fn reconnect_backoff_is_ordered_and_its_multiplier_bounded() {
        let mut policy = ReconnectPolicy::default();
        assert_eq!(policy.limit_errors(), []);
        for fine in [1.0, 10.0] {
            policy.multiplier = fine;
            assert_eq!(policy.limit_errors(), [], "{fine}");
        }

        policy.initial_backoff_ms = 5_000;
        policy.max_backoff_ms = 1_000;
        policy.multiplier = 0.5;
        assert_eq!(
            policy.limit_errors(),
            [
                LimitError::BackoffOrder {
                    initial: 5_000,
                    max: 1_000
                },
                LimitError::BackoffMultiplier {
                    found: "0.5".to_owned()
                },
            ]
        );
        for bad in [10.5, f64::NAN, f64::INFINITY] {
            policy.multiplier = bad;
            assert!(
                policy
                    .limit_errors()
                    .iter()
                    .any(|e| matches!(e, LimitError::BackoffMultiplier { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn match_rules_are_counted_and_their_patterns_measured() {
        let rule = |name: &str, len: usize| MatchRule {
            name: name.to_owned(),
            condition: MatchCondition::BytePattern {
                pattern: vec![b'x'; len],
            },
            actions: Vec::new(),
            enabled: true,
        };
        assert_eq!(
            match_rule_errors(&[rule("ok", MAX_MATCH_PATTERN_BYTES)]),
            []
        );
        assert_eq!(
            match_rule_errors(&[rule("long", MAX_MATCH_PATTERN_BYTES + 1)]),
            [LimitError::MatchPattern {
                rule: "long".to_owned(),
                found: MAX_MATCH_PATTERN_BYTES + 1
            }]
        );
        let many: Vec<MatchRule> = (0..=MAX_MATCH_RULES).map(|_| rule("r", 1)).collect();
        assert_eq!(
            match_rule_errors(&many),
            [LimitError::MatchRules {
                found: MAX_MATCH_RULES + 1
            }]
        );
    }

    fn trigger(
        name: &str,
        pattern: &[u8],
        target: RecordTarget,
        control: RecordControl,
    ) -> MatchRule {
        MatchRule {
            name: name.to_owned(),
            condition: MatchCondition::BytePattern {
                pattern: pattern.to_vec(),
            },
            actions: vec![MatchAction::Record { target, control }],
            enabled: true,
        }
    }

    fn overlaps(stop: &[u8], begin: &[u8], stop_on: RecordTarget, begin_on: RecordTarget) -> bool {
        let rules = [
            trigger("stop", stop, stop_on, RecordControl::Stop),
            trigger("begin", begin, begin_on, RecordControl::Begin),
        ];
        match_rule_errors(&rules)
            .iter()
            .any(|e| matches!(e, LimitError::TriggerOverlap { .. }))
    }

    #[test]
    fn a_stop_that_can_end_inside_a_begin_is_refused() {
        use RecordTarget::{Both, Display, Raw};
        // Inside: the review's case. One read recorded ABC, two reads ABCDE.
        assert!(overlaps(b"BC", b"ABCDE", Raw, Raw));
        // Its end overlapping the Begin's start: "XAB" then "ABC" over "XABC".
        assert!(overlaps(b"XAB", b"ABC", Raw, Raw));
        // Both, against either recording.
        assert!(overlaps(b"BC", b"ABCDE", Both, Display));
        assert!(overlaps(b"BC", b"ABCDE", Raw, Both));
    }

    #[test]
    fn triggers_that_give_one_result_however_the_data_arrives_are_accepted() {
        use RecordTarget::{Display, Raw};
        // Ordinary sentence triggers never overlap.
        assert!(!overlaps(b"$GPRMC", b"$GPGGA", Raw, Raw));
        // Ending with the Begin: both complete on the same byte, in one read.
        assert!(!overlaps(b"CDE", b"ABCDE", Raw, Raw));
        // A Stop that ends after the Begin completes later, whatever the reads.
        assert!(!overlaps(b"DEF", b"ABCDE", Raw, Raw));
        // A Begin inside a Stop is in order either way.
        assert!(!overlaps(b"ABCDE", b"BC", Raw, Raw));
        // Different recordings do not interact.
        assert!(!overlaps(b"BC", b"ABCDE", Raw, Display));
    }

    #[test]
    fn a_rules_actions_and_mark_text_are_measured() {
        use crate::config::{MarkTimestamp, MarkTimestampStyle};
        let mut rule = MatchRule {
            name: "busy".to_owned(),
            condition: MatchCondition::BytePattern {
                pattern: b"x".to_vec(),
            },
            actions: vec![MatchAction::PauseDisplay {}; MAX_MATCH_ACTIONS],
            enabled: true,
        };
        assert_eq!(match_rule_errors(std::slice::from_ref(&rule)), []);
        rule.actions.push(MatchAction::PauseDisplay {});
        assert_eq!(
            match_rule_errors(std::slice::from_ref(&rule)),
            [LimitError::MatchActions {
                rule: "busy".to_owned(),
                found: MAX_MATCH_ACTIONS + 1
            }]
        );

        rule.actions = vec![MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: Default::default(),
                style: MarkTimestampStyle::Plain {},
                format: Default::default(),
                separator: " ".repeat(MAX_MARK_SEPARATOR_BYTES + 1),
            }),
        }];
        assert_eq!(
            match_rule_errors(std::slice::from_ref(&rule)),
            [LimitError::MarkSeparator {
                rule: "busy".to_owned(),
                found: MAX_MARK_SEPARATOR_BYTES + 1
            }]
        );
    }

    #[test]
    fn a_size_cap_under_the_minimum_is_reported() {
        assert_eq!(size_cap_error("raw", None), None);
        assert_eq!(size_cap_error("raw", Some(MIN_SIZE_CAP)), None);
        assert_eq!(
            size_cap_error("display", Some(MIN_SIZE_CAP - 1)),
            Some(LimitError::SizeCap {
                recording: "display",
                found: MIN_SIZE_CAP - 1
            })
        );
    }

    #[test]
    fn a_disk_guard_threshold_must_be_one_the_guard_can_act_on() {
        use super::super::LowDiskAction;
        let guard = |min_free| {
            Some(DiskGuard {
                min_free,
                on_low: LowDiskAction::Warn,
            })
        };
        let percent = |percent| guard(DiskThreshold::Percent { percent });
        assert_eq!(disk_guard_error("raw", None), None);
        for ok in [1, 50, 90] {
            assert_eq!(disk_guard_error("raw", percent(ok)), None, "{ok}");
        }
        for bad in [0, 91, 100, 101, 255] {
            assert_eq!(
                disk_guard_error("display", percent(bad)),
                Some(LimitError::DiskGuardPercent {
                    recording: "display",
                    found: bad
                }),
                "{bad}"
            );
        }
        assert_eq!(
            disk_guard_error("raw", guard(DiskThreshold::Bytes { bytes: 1 })),
            None
        );
        assert_eq!(
            disk_guard_error("raw", guard(DiskThreshold::Bytes { bytes: u64::MAX })),
            None
        );
        assert_eq!(
            disk_guard_error("raw", guard(DiskThreshold::Bytes { bytes: 0 })),
            Some(LimitError::DiskGuardBytes { recording: "raw" })
        );
        assert_eq!(
            disk_guard_error("raw", percent(95)).unwrap().to_string(),
            "raw recording's low-disk threshold of 95 percent is outside 1–90; \
             recording resumes only at 10% above it, so a higher one never resumes"
        );
    }

    #[test]
    fn a_queue_capacity_is_non_zero_and_bounded() {
        assert_eq!(capacity_error("ingest", 1, 256), None);
        assert_eq!(capacity_error("ingest", 256, 256), None);
        assert!(capacity_error("ingest", 0, 256).is_some());
        assert_eq!(
            capacity_error("ingest", 257, 256).unwrap().to_string(),
            "ingest of 257 is outside 1–256"
        );
    }
}
