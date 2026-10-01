//! The limits a profile and the runtime both enforce (ADR-048, §71).
//!
//! They live in one place so validation and the runtime's constructors cannot
//! disagree. Together they give each Channel a stated worst case of about
//! 50 MiB: a 16 MiB receive queue, two 8 MiB recording queues, 16 MiB of
//! scrollback and about 2 MiB of diagnostics (ADR-048).

use super::{MatchCondition, MatchRule, ReconnectPolicy, RetentionConfig, MIN_SIZE_CAP};

/// Stream scrollback per Channel (§71, §88).
pub const MAX_SCROLLBACK_BYTES: usize = 16 * 1024 * 1024;
/// Retained diagnostics per severity (§71, §88).
pub const MAX_DIAGNOSTICS_PER_SEVERITY: usize = 2_000;
/// One match pattern (§71).
pub const MAX_MATCH_PATTERN_BYTES: usize = 256;
/// Match rules per Channel (§71).
pub const MAX_MATCH_RULES: usize = 64;
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
    }
    errors
}

/// A configured recording size cap under the minimum (§59). The runtime never
/// uses one, since it raises the cap to the minimum; this reports it.
pub fn size_cap_error(recording: &'static str, size_cap: Option<u64>) -> Option<LimitError> {
    size_cap
        .filter(|&found| found < MIN_SIZE_CAP)
        .map(|found| LimitError::SizeCap { recording, found })
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
