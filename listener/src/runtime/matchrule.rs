//! Match Rule evaluation (spec §50.2, §165).
//!
//! A [`MatchRuleSet`] is the runtime form of a Channel's configured
//! [`MatchRule`]s: each rule is compiled once (minting a [`MatchRuleId`] so events
//! and commands can name it) and then evaluated against received data. Evaluation
//! is **pure and side-effect-free** — it reports *which* rules fired and *what
//! actions* they carry; the pipeline owns applying those actions (recording,
//! display, diagnostics, events). Rules never modify the received bytes or the
//! recordings — the stream stays verbatim (§40, §103, §116).
//!
//! Two evaluation paths, per §50.2:
//! - **stream** ([`evaluate_stream`](MatchRuleSet::evaluate_stream)) for
//!   `BytePattern`, run against each received chunk;
//! - **timer-based** ([`evaluate_idle`](MatchRuleSet::evaluate_idle)) for `Idle`,
//!   which fires once when the stream has been quiet for the timeout and re-arms
//!   when data resumes ([`note_activity`](MatchRuleSet::note_activity)).
//!
//! Evaluation is bounded (a linear scan of the rule list) and never stalls
//! reception (§100).
//!
//! ## Cross-chunk matching
//!
//! Received bytes arrive in chunks whose boundaries track OS buffering, not
//! content (ADR-009): a pattern can be split across two reads (`"GG"` ends one
//! chunk, `"A"` begins the next), or across several when reads are shorter than
//! the pattern. A naive per-chunk scan would miss it. So the set keeps a small
//! **carry** — the last `max_pattern_len - 1` bytes of the stream so far, which
//! may span several earlier chunks — and runs two scans: a bounded **boundary scan** over `carry` plus just
//! enough of the new chunk for the longest pattern to complete (finding matches
//! that *start* in the carry and *end* in the chunk), and a zero-copy **chunk
//! scan** over the chunk slice itself (matches fully inside the carry were
//! already reported last time). A match found by the boundary scan is a
//! **boundary split**: it would have been missed without the carry. Those are
//! counted and flagged so the pipeline can measure where, why, and how often
//! splitting actually occurs (§50.2).

use std::time::Duration;

use crate::config::{MatchAction, MatchCondition, MatchRule};
use crate::core::MatchRuleId;

/// One configured rule in runtime form: its minted id, condition, actions, the
/// enabled flag (a live toggle is deferred — see [`MatchRuleSet::set_enabled`]),
/// and the small bit of state the `Idle` condition needs to fire exactly once
/// per quiet episode.
struct CompiledRule {
    id: MatchRuleId,
    condition: MatchCondition,
    actions: Vec<MatchAction>,
    enabled: bool,
    /// For an `Idle` rule: whether it has already fired during the current quiet
    /// episode (reset by `note_activity` when data resumes). Unused otherwise.
    idle_fired: bool,
    /// How many of this rule's `BytePattern` matches were **boundary splits** —
    /// found only because the cross-chunk carry was scanned (the pattern straddled
    /// a read boundary). The measurement of how often splitting matters (§50.2).
    boundary_saves: u64,
}

/// A rule that fired, with the actions the pipeline should apply. Actions are
/// cloned (only for the rare matched rule) so the caller can apply them while
/// holding `&mut` to the rest of the pipeline.
#[derive(Clone, Debug, PartialEq)]
pub struct FiredRule {
    pub id: MatchRuleId,
    pub actions: Vec<MatchAction>,
    /// Absolute stream offset of the **first byte** of the match that fired
    /// (§50.2: matches are anchored on byte offsets — the stream-only model has no
    /// message numbers). `None` for an `Idle` firing, not tied to a data position.
    pub match_offset: Option<u64>,
    /// Length of the matched byte pattern. Zero for an `Idle` firing. Keeping
    /// the extent lets an `After` annotation anchor on the match's final byte
    /// while diagnostics continue to report its first byte.
    pub match_len: usize,
    /// Whether this match was a **boundary split** — its first byte fell in an
    /// earlier chunk and it completed in this one, so a per-chunk scan would have
    /// missed it. Always `false` for `Idle`. Drives the where/why/how-often
    /// measurement in the pipeline.
    pub boundary_split: bool,
    /// Timer evaluation lateness for an `Idle` firing. `None` for byte matches.
    pub timer_lateness: Option<Duration>,
}

/// A Channel's compiled Match Rules (§50.2). Owned by the pipeline.
pub struct MatchRuleSet {
    rules: Vec<CompiledRule>,
    /// The newest `max_pattern_len - 1` bytes of the stream, kept so a pattern
    /// split across read boundaries still matches. Empty until the first chunk.
    carry: Vec<u8>,
    /// Absolute stream offset of `carry[0]` — the position of the carry's first
    /// byte, so a match starting in the carry reports its true offset.
    carry_offset: u64,
    /// The largest enabled `BytePattern` length, recomputed when rules change. The
    /// carry never needs more than `max_pattern_len - 1` bytes.
    max_pattern_len: usize,
}

impl MatchRuleSet {
    /// Compile a Channel's configured rules, minting a fresh [`MatchRuleId`] for
    /// each (in config order).
    pub fn compile(rules: &[MatchRule]) -> Self {
        let rules: Vec<CompiledRule> = rules
            .iter()
            .map(|r| CompiledRule {
                id: MatchRuleId::new(),
                condition: r.condition.clone(),
                actions: r.actions.clone(),
                enabled: r.enabled,
                idle_fired: false,
                boundary_saves: 0,
            })
            .collect();
        let mut set = Self {
            rules,
            carry: Vec::new(),
            carry_offset: 0,
            max_pattern_len: 0,
        };
        set.recompute_max_pattern_len();
        set
    }

    /// The longest enabled `BytePattern` determines how much carry we must keep.
    /// Recomputed whenever the enabled set changes.
    fn recompute_max_pattern_len(&mut self) {
        self.max_pattern_len = self
            .rules
            .iter()
            .filter(|r| r.enabled)
            .filter_map(|r| match &r.condition {
                MatchCondition::BytePattern { pattern } => Some(pattern.len()),
                MatchCondition::Idle { .. } => None,
            })
            .max()
            .unwrap_or(0);
    }

    /// True when there are no rules at all (lets the pipeline skip evaluation).
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// True when at least one enabled rule uses the `Idle` condition, so the
    /// pipeline knows whether to run an idle timer at all.
    pub fn has_idle_rule(&self) -> bool {
        self.rules
            .iter()
            .any(|r| r.enabled && matches!(r.condition, MatchCondition::Idle { .. }))
    }

    /// Time until the earliest enabled, not-yet-fired Idle rule becomes due.
    /// `None` means no timer work is pending for the current quiet episode.
    pub fn next_idle_wait(&self, idle_for: Duration) -> Option<Duration> {
        self.rules
            .iter()
            .filter(|rule| rule.enabled && !rule.idle_fired)
            .filter_map(|rule| match rule.condition {
                MatchCondition::Idle { timeout_ms } => {
                    Some(Duration::from_millis(timeout_ms).saturating_sub(idle_for))
                }
                MatchCondition::BytePattern { .. } => None,
            })
            .min()
    }

    /// The ids of every rule, in config order. The name lives in config; the id
    /// is the runtime key. **Not yet wired** to a runtime command — this is the
    /// seam the deferred live rule-toggle will route through (ADR-012: a new
    /// `Listener` method + pipeline command, like `set_recording`).
    pub fn ids(&self) -> Vec<MatchRuleId> {
        self.rules.iter().map(|r| r.id).collect()
    }

    /// Enable or disable a rule by id. Returns whether a rule with that id
    /// existed. **Not yet wired** — the deferred live rule-toggle's seam
    /// (ADR-012); today rules change via the config Apply & Restart path.
    pub fn set_enabled(&mut self, id: MatchRuleId, enabled: bool) -> bool {
        if let Some(rule) = self.rules.iter_mut().find(|r| r.id == id) {
            rule.enabled = enabled;
            self.recompute_max_pattern_len();
            true
        } else {
            false
        }
    }

    /// Total boundary-split saves across all rules — `BytePattern` matches found
    /// only because the cross-chunk carry was scanned. The "how often" of the
    /// where/why/how-often measurement (§50.2). Monotonic since Start.
    pub fn boundary_saves(&self) -> u64 {
        self.rules.iter().map(|r| r.boundary_saves).sum()
    }

    /// Evaluate the `BytePattern` conditions against one received chunk (§50.2),
    /// matching **across the previous chunk's boundary** via the retained carry.
    /// `chunk_offset` is the absolute stream offset of `chunk[0]`. `Idle` rules are
    /// never matched here — they are timer-driven. Returns one firing per
    /// **occurrence** (not per rule): a chunk carrying three `$GPGGA`s fires a GGA
    /// rule three times, each anchored at its own start offset, so per-match
    /// actions (a Mark timestamp next to *each* occurrence) see every one.
    /// Overlapping occurrences all fire (scanning resumes one byte after each
    /// match start). Firings are rule-major, then in stream order within a rule.
    /// The carry is updated for the next chunk.
    pub fn evaluate_stream(&mut self, chunk: &[u8], chunk_offset: u64) -> Vec<FiredRule> {
        let mut fired = Vec::new();
        if chunk.is_empty() {
            return fired;
        }

        let carry_len = self.carry.len();
        // Boundary region: the carry plus just enough of the chunk for the longest
        // enabled pattern to complete (`max_pattern_len − 1` bytes). A match that
        // straddles the boundary — starts in the carry, ends in the chunk — lies
        // entirely inside it, so this small bounded buffer is the only copy made:
        // the chunk itself is scanned in place, never cloned.
        let boundary: Vec<u8> = if carry_len == 0 {
            Vec::new()
        } else {
            let take = self.max_pattern_len.saturating_sub(1).min(chunk.len());
            let mut b = Vec::with_capacity(carry_len + take);
            b.extend_from_slice(&self.carry);
            b.extend_from_slice(&chunk[..take]);
            b
        };

        for rule in &mut self.rules {
            let MatchCondition::BytePattern { pattern } = &rule.condition else {
                continue;
            };
            if !rule.enabled || pattern.is_empty() || pattern.len() > carry_len + chunk.len() {
                continue;
            }
            // 1. Boundary splits: matches that start in the carry and end in the
            // chunk — a per-chunk scan would miss them. Matches fully inside the
            // carry were reported last chunk; a start at/after the carry belongs
            // to the chunk scan below (so nothing is double-reported).
            if carry_len > 0 && pattern.len() >= 2 {
                let mut start = 0;
                while start < carry_len {
                    let Some(rel) = find_subslice(&boundary[start..], pattern) else {
                        break;
                    };
                    let match_start = start + rel;
                    if match_start >= carry_len {
                        break; // starts in the chunk — the chunk scan reports it
                    }
                    if match_start + pattern.len() > carry_len {
                        rule.boundary_saves += 1;
                        fired.push(FiredRule {
                            id: rule.id,
                            actions: rule.actions.clone(),
                            match_offset: Some(self.carry_offset + match_start as u64),
                            match_len: pattern.len(),
                            boundary_split: true,
                            timer_lateness: None,
                        });
                    }
                    start = match_start + 1;
                }
            }
            // 2. In-chunk matches, one firing per occurrence (a chunk holding
            // several occurrences fires the rule once per occurrence, so per-match
            // actions — Mark timestamps, Notify — see each one), scanned on the
            // chunk slice directly.
            let mut start = 0;
            while let Some(rel) = find_subslice(&chunk[start..], pattern) {
                let match_start = start + rel;
                fired.push(FiredRule {
                    id: rule.id,
                    actions: rule.actions.clone(),
                    match_offset: Some(chunk_offset + match_start as u64),
                    match_len: pattern.len(),
                    boundary_split: false,
                    timer_lateness: None,
                });
                start = match_start + 1;
            }
        }

        // The next carry is the newest `max_pattern_len - 1` bytes of the stream:
        // the tail of carry plus chunk, not of the chunk alone. A chunk shorter
        // than that keeps older carry bytes, so a pattern spread over three or
        // more short reads still completes. `carry_offset` is computed from the
        // chunk's true end offset so it stays absolute.
        let want = self.max_pattern_len.saturating_sub(1);
        let chunk_end_offset = chunk_offset + chunk.len() as u64;
        if want == 0 {
            self.carry.clear();
        } else if chunk.len() >= want {
            self.carry.clear();
            self.carry.extend_from_slice(&chunk[chunk.len() - want..]);
        } else {
            self.carry.extend_from_slice(chunk);
            let excess = self.carry.len().saturating_sub(want);
            self.carry.drain(..excess);
        }
        self.carry_offset = chunk_end_offset - self.carry.len() as u64;
        fired
    }

    /// Evaluate the `Idle` condition against the current quiet duration (§50.2). An
    /// idle rule fires once when `idle_for` reaches its timeout and is then latched
    /// until [`note_activity`](Self::note_activity) re-arms it. Returns the rules
    /// that fired this tick.
    pub fn evaluate_idle(&mut self, idle_for: Duration) -> Vec<FiredRule> {
        let mut fired = Vec::new();
        for rule in &mut self.rules {
            let MatchCondition::Idle { timeout_ms } = rule.condition else {
                continue;
            };
            if !rule.enabled || rule.idle_fired {
                continue;
            }
            if idle_for >= Duration::from_millis(timeout_ms) {
                rule.idle_fired = true;
                fired.push(FiredRule {
                    id: rule.id,
                    actions: rule.actions.clone(),
                    match_offset: None,
                    match_len: 0,
                    boundary_split: false,
                    timer_lateness: Some(
                        idle_for.saturating_sub(Duration::from_millis(timeout_ms)),
                    ),
                });
            }
        }
        fired
    }

    /// Data resumed: re-arm every `Idle` rule so it can fire again on the next
    /// quiet episode (§50.2).
    pub fn note_activity(&mut self) {
        for rule in &mut self.rules {
            rule.idle_fired = false;
        }
    }
}

/// Substring search: the index of the first occurrence of `needle` in `haystack`,
/// or `None`. An empty needle is rejected at config time (§71), but treat it as
/// "no match" defensively.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MatchRule;

    fn rule(name: &str, condition: MatchCondition) -> MatchRule {
        MatchRule {
            name: name.to_string(),
            condition,
            actions: vec![MatchAction::Mark { timestamp: None }],
            enabled: true,
        }
    }

    #[test]
    fn byte_pattern_matches_a_substring_within_a_chunk() {
        let mut set = MatchRuleSet::compile(&[rule(
            "gga",
            MatchCondition::BytePattern {
                pattern: b"GGA".to_vec(),
            },
        )]);
        let fired = set.evaluate_stream(b"$GPGGA,...", 0);
        assert_eq!(fired.len(), 1);
        // "GGA" begins at index 3 of the chunk; no carry → absolute offset 3.
        assert_eq!(fired[0].match_offset, Some(3));
        assert!(!fired[0].boundary_split);
        assert!(set.evaluate_stream(b"$GPGLL,...", 10).is_empty());
    }

    #[test]
    fn empty_or_oversized_byte_patterns_never_match() {
        assert_eq!(find_subslice(b"abc", b""), None);
        assert_eq!(find_subslice(b"ab", b"abc"), None);
        assert_eq!(find_subslice(b"abc", b"abc"), Some(0));
        assert_eq!(find_subslice(b"xxabc", b"abc"), Some(2));
    }

    #[test]
    fn disabled_rules_do_not_fire() {
        let mut config = rule(
            "p",
            MatchCondition::BytePattern {
                pattern: b"X".to_vec(),
            },
        );
        config.enabled = false;
        let mut set = MatchRuleSet::compile(&[config]);
        assert!(set.evaluate_stream(b"X", 0).is_empty());

        // Re-enable via the command path and it fires.
        let id = set.ids()[0];
        assert!(set.set_enabled(id, true));
        assert_eq!(set.evaluate_stream(b"X", 1).len(), 1);
    }

    #[test]
    fn byte_pattern_matches_across_a_chunk_boundary_and_is_counted() {
        let mut set = MatchRuleSet::compile(&[rule(
            "gga",
            MatchCondition::BytePattern {
                pattern: b"GGA".to_vec(),
            },
        )]);
        // "GG" ends the first chunk, "A" begins the second: a per-chunk scan misses
        // it, but the carry catches it.
        assert!(set.evaluate_stream(b"$GPGG", 0).is_empty());
        let fired = set.evaluate_stream(b"A,123", 5);
        assert_eq!(fired.len(), 1);
        // The match starts at stream offset 3 ("GGA" begins inside chunk 1).
        assert_eq!(fired[0].match_offset, Some(3));
        assert!(fired[0].boundary_split, "match straddled the boundary");
        // Measurement: exactly one boundary save recorded.
        assert_eq!(set.boundary_saves(), 1);
    }

    #[test]
    fn every_occurrence_in_a_chunk_fires() {
        // §50.2: a chunk carrying several occurrences fires the rule once per
        // occurrence (a large serial read or UDP datagram can hold many sentences;
        // a per-match Mark timestamp must land next to each one).
        let mut set = MatchRuleSet::compile(&[rule(
            "gga",
            MatchCondition::BytePattern {
                pattern: b"GGA".to_vec(),
            },
        )]);
        let fired = set.evaluate_stream(b"$GPGGA,1\r\n$GPGGA,2\r\n$GPGGA,3\r\n", 0);
        assert_eq!(fired.len(), 3, "one firing per occurrence");
        let offsets: Vec<_> = fired.iter().map(|f| f.match_offset).collect();
        assert_eq!(offsets, vec![Some(3), Some(13), Some(23)]);
        assert!(fired.iter().all(|f| !f.boundary_split));
    }

    #[test]
    fn overlapping_occurrences_each_fire() {
        // Scanning resumes one byte after each match start, so overlapping
        // occurrences all fire ("AA" in "AAA" → offsets 0 and 1).
        let mut set = MatchRuleSet::compile(&[rule(
            "aa",
            MatchCondition::BytePattern {
                pattern: b"AA".to_vec(),
            },
        )]);
        let fired = set.evaluate_stream(b"AAA", 0);
        let offsets: Vec<_> = fired.iter().map(|f| f.match_offset).collect();
        assert_eq!(offsets, vec![Some(0), Some(1)]);
    }

    #[test]
    fn a_boundary_split_and_a_later_in_chunk_occurrence_both_fire() {
        // The carry-recovered split and a fresh in-chunk occurrence are distinct
        // firings; only the split increments the boundary-save measurement.
        let mut set = MatchRuleSet::compile(&[rule(
            "gga",
            MatchCondition::BytePattern {
                pattern: b"GGA".to_vec(),
            },
        )]);
        assert!(set.evaluate_stream(b"$GPGG", 0).is_empty());
        let fired = set.evaluate_stream(b"A,1,GGA,2", 5);
        assert_eq!(fired.len(), 2);
        assert_eq!(fired[0].match_offset, Some(3));
        assert!(fired[0].boundary_split);
        assert_eq!(fired[1].match_offset, Some(9));
        assert!(!fired[1].boundary_split);
        assert_eq!(set.boundary_saves(), 1);
    }

    #[test]
    fn a_match_is_not_double_counted_across_the_boundary() {
        // A pattern fully inside chunk 1 ending exactly at the boundary is reported
        // from chunk 1, and must not be re-reported when it reappears in the carry.
        let mut set = MatchRuleSet::compile(&[rule(
            "ab",
            MatchCondition::BytePattern {
                pattern: b"AB".to_vec(),
            },
        )]);
        // chunk 1 ends with "AB"; reported now at offset 2.
        let f1 = set.evaluate_stream(b"xxAB", 0);
        assert_eq!(f1.len(), 1);
        assert_eq!(f1[0].match_offset, Some(2));
        assert!(!f1[0].boundary_split);
        // chunk 2 has no new occurrence ending in it; the carried "B" must not
        // re-fire the earlier "AB".
        let f2 = set.evaluate_stream(b"cd", 4);
        assert!(f2.is_empty());
        assert_eq!(set.boundary_saves(), 0);
    }

    /// Every way of splitting a stream into reads, down to one byte per read,
    /// fires exactly the matches one whole-stream read fires, at the same offsets.
    /// Reads shorter than a pattern spread it over three or more reads, so the
    /// carry must keep the newest bytes across reads, not only the last read's.
    #[test]
    fn every_split_of_a_stream_fires_what_one_read_fires() {
        fn firings(rules: &[MatchRule], reads: &[&[u8]]) -> Vec<(usize, u64)> {
            let mut set = MatchRuleSet::compile(rules);
            let ids = set.ids();
            let mut offset = 0;
            let mut out = Vec::new();
            for read in reads {
                for fired in set.evaluate_stream(read, offset) {
                    let rule = ids.iter().position(|id| *id == fired.id).unwrap();
                    out.push((rule, fired.match_offset.unwrap()));
                }
                offset += read.len() as u64;
            }
            out.sort_unstable();
            out
        }

        let rules = [
            rule(
                "gga",
                MatchCondition::BytePattern {
                    pattern: b"$GPGGA".to_vec(),
                },
            ),
            rule(
                "aa",
                MatchCondition::BytePattern {
                    pattern: b"AA".to_vec(),
                },
            ),
            rule(
                "dollar",
                MatchCondition::BytePattern {
                    pattern: b"$".to_vec(),
                },
            ),
        ];
        let stream: &[u8] = b"x$GPGGAA$GPGGA";
        let whole = firings(&rules, &[stream]);
        assert_eq!(whole, vec![(0, 1), (0, 8), (1, 6), (2, 1), (2, 8)]);

        // Bit i of `cuts` ends a read after byte i.
        for cuts in 0u32..1 << (stream.len() - 1) {
            let mut reads = Vec::new();
            let mut start = 0;
            for end in 1..stream.len() {
                if cuts & (1 << (end - 1)) != 0 {
                    reads.push(&stream[start..end]);
                    start = end;
                }
            }
            reads.push(&stream[start..]);
            assert_eq!(firings(&rules, &reads), whole, "reads: {reads:?}");
        }
    }

    #[test]
    fn an_in_chunk_match_after_the_carry_is_not_a_boundary_split() {
        let mut set = MatchRuleSet::compile(&[rule(
            "zz",
            MatchCondition::BytePattern {
                pattern: b"ZZ".to_vec(),
            },
        )]);
        set.evaluate_stream(b"aaa", 0); // carry = "a"
        let fired = set.evaluate_stream(b"bZZc", 3);
        assert_eq!(fired.len(), 1);
        // "ZZ" begins at chunk index 1 → absolute offset 4; not a split.
        assert_eq!(fired[0].match_offset, Some(4));
        assert!(!fired[0].boundary_split);
        assert_eq!(set.boundary_saves(), 0);
    }

    #[test]
    fn idle_fires_once_per_quiet_episode_and_rearms_on_activity() {
        let mut set =
            MatchRuleSet::compile(&[rule("idle", MatchCondition::Idle { timeout_ms: 500 })]);
        assert!(set.has_idle_rule());

        // Below the timeout: nothing fires.
        assert!(set.evaluate_idle(Duration::from_millis(300)).is_empty());
        assert_eq!(
            set.next_idle_wait(Duration::from_millis(300)),
            Some(Duration::from_millis(200))
        );
        // At/over the timeout: fires once.
        let fired = set.evaluate_idle(Duration::from_millis(600));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].timer_lateness, Some(Duration::from_millis(100)));
        assert_eq!(set.next_idle_wait(Duration::from_millis(600)), None);
        // Still quiet, already fired: latched, no repeat.
        assert!(set.evaluate_idle(Duration::from_millis(900)).is_empty());
        // Data resumes, then quiet again: it can fire again.
        set.note_activity();
        assert_eq!(set.evaluate_idle(Duration::from_millis(600)).len(), 1);
    }

    #[test]
    fn idle_is_never_matched_on_the_stream_path() {
        let mut set =
            MatchRuleSet::compile(&[rule("idle", MatchCondition::Idle { timeout_ms: 1 })]);
        assert!(set.evaluate_stream(b"anything", 0).is_empty());
        // An idle-only set keeps no carry (max_pattern_len is 0).
        assert_eq!(set.boundary_saves(), 0);
    }

    #[test]
    fn fired_rule_carries_its_actions() {
        let mut config = rule(
            "p",
            MatchCondition::BytePattern {
                pattern: b"hit".to_vec(),
            },
        );
        config.actions = vec![
            MatchAction::Notify {
                severity: crate::diagnostics::DiagnosticSeverity::Warning,
            },
            MatchAction::Mark { timestamp: None },
        ];
        let mut set = MatchRuleSet::compile(&[config]);
        let fired = set.evaluate_stream(b"a hit here", 0);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].actions.len(), 2);
        assert_eq!(fired[0].match_offset, Some(2));
        assert_eq!(fired[0].match_len, 3);
    }

    #[test]
    fn multiple_rules_each_evaluate_independently() {
        let mut set = MatchRuleSet::compile(&[
            rule(
                "a",
                MatchCondition::BytePattern {
                    pattern: b"AA".to_vec(),
                },
            ),
            rule(
                "b",
                MatchCondition::BytePattern {
                    pattern: b"ZZ".to_vec(),
                },
            ),
        ]);
        // Only rule "a" matches a chunk containing "AA" but not "ZZ".
        let fired = set.evaluate_stream(b"--AA--", 0);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, set.ids()[0]);
        // Both fire when both patterns are present (offset 6 keeps the streams apart).
        assert_eq!(set.evaluate_stream(b"AA..ZZ", 6).len(), 2);
        // Neither matches when absent.
        assert!(set.evaluate_stream(b"BBBB", 12).is_empty());
    }
}
