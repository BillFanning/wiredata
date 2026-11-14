//! Per-message ISO 8601 timestamp (spec §5.4).

use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use serde::{Deserialize, Serialize};

/// Configuration for the timestamp prepended to a message's wire output.
///
/// Time-of-day (`HH:MM:SS`) is always present; the date, milliseconds, and
/// zone designator are independently toggleable. The time is the computer's
/// local time unless [`Self::utc`] is set. A message carries a timestamp only
/// when [`MessageConfig::timestamp`] is `Some`.
///
/// [`MessageConfig::timestamp`]: super::MessageConfig
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimestampConfig {
    #[serde(default)]
    pub include_date: bool,
    #[serde(default)]
    pub include_millis: bool,
    /// Append the zone designator: `Z` when [`Self::utc`], else the local
    /// offset (`-05:00`).
    #[serde(default)]
    pub include_timezone: bool,
    /// Write UTC instead of the computer's local time.
    #[serde(default)]
    pub utc: bool,
}

impl TimestampConfig {
    /// Number of ASCII bytes produced by [`Self::format`]. All timestamp
    /// components are fixed-width, so this is independent of the instant.
    pub(crate) const fn wire_len(&self) -> usize {
        let zone = if !self.include_timezone {
            0
        } else if self.utc {
            1
        } else {
            6
        };
        8 + if self.include_date { 11 } else { 0 } + if self.include_millis { 4 } else { 0 } + zone
    }

    /// Format `now` as an ISO 8601 timestamp per this configuration: UTC when
    /// [`Self::utc`], otherwise the computer's local time at that instant.
    pub fn format(&self, now: DateTime<Utc>) -> String {
        let local = now.with_timezone(&Local).offset().fix();
        self.format_at(now, local)
    }

    /// [`Self::format`] with the local offset given, so the zone is not read
    /// from the machine.
    pub(crate) fn format_at(&self, now: DateTime<Utc>, local: FixedOffset) -> String {
        let mut pattern = String::with_capacity(20);
        if self.include_date {
            pattern.push_str("%Y-%m-%dT");
        }
        pattern.push_str("%H:%M:%S");
        if self.include_millis {
            pattern.push_str("%.3f");
        }
        if self.include_timezone {
            pattern.push_str(if self.utc { "Z" } else { "%:z" });
        }
        if self.utc {
            now.format(&pattern).to_string()
        } else {
            now.with_timezone(&local).format(&pattern).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    /// 2026-05-22 14:30:45.123 UTC
    fn sample() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 22, 14, 30, 45)
            .unwrap()
            .with_nanosecond(123_000_000)
            .unwrap()
    }

    fn zone(seconds: i32) -> FixedOffset {
        FixedOffset::east_opt(seconds).unwrap()
    }

    fn cfg(date: bool, millis: bool, tz: bool, utc: bool) -> TimestampConfig {
        TimestampConfig {
            include_date: date,
            include_millis: millis,
            include_timezone: tz,
            utc,
        }
    }

    #[test]
    fn utc_full_timestamp() {
        assert_eq!(
            cfg(true, true, true, true).format_at(sample(), zone(-4 * 3600)),
            "2026-05-22T14:30:45.123Z"
        );
    }

    #[test]
    fn utc_without_designator_ignores_the_local_zone() {
        assert_eq!(
            cfg(false, true, false, true).format_at(sample(), zone(5 * 3600)),
            "14:30:45.123"
        );
        assert_eq!(
            cfg(true, false, true, true).format_at(sample(), zone(5 * 3600)),
            "2026-05-22T14:30:45Z"
        );
    }

    #[test]
    fn local_negative_offset_with_designator() {
        assert_eq!(
            cfg(true, true, true, false).format_at(sample(), zone(-4 * 3600)),
            "2026-05-22T10:30:45.123-04:00"
        );
    }

    #[test]
    fn local_half_hour_offset_can_change_the_date() {
        assert_eq!(
            cfg(true, false, true, false).format_at(sample(), zone(9 * 3600 + 1800)),
            "2026-05-23T00:00:45+09:30"
        );
        assert_eq!(
            cfg(false, false, true, false).format_at(sample(), zone(5 * 3600 + 1800)),
            "20:00:45+05:30"
        );
    }

    #[test]
    fn local_without_designator() {
        assert_eq!(
            cfg(false, false, false, false).format_at(sample(), zone(-4 * 3600)),
            "10:30:45"
        );
        assert_eq!(
            cfg(true, true, false, false).format_at(sample(), zone(-4 * 3600)),
            "2026-05-22T10:30:45.123"
        );
    }

    #[test]
    fn time_only_local_is_the_default() {
        assert_eq!(
            TimestampConfig::default().format_at(sample(), zone(-4 * 3600)),
            "10:30:45"
        );
    }

    #[test]
    fn format_agrees_with_the_machine_local_offset() {
        let local = sample().with_timezone(&Local).offset().fix();
        let c = cfg(true, true, true, false);
        assert_eq!(c.format(sample()), c.format_at(sample(), local));
        assert!(
            c.format(sample()).ends_with(&local.to_string()),
            "{}",
            c.format(sample())
        );
    }

    #[test]
    fn wire_len_matches_every_formatted_shape() {
        for date in [false, true] {
            for millis in [false, true] {
                for tz in [false, true] {
                    for utc in [false, true] {
                        for offset in [0, -4 * 3600, 5 * 3600 + 1800] {
                            let c = cfg(date, millis, tz, utc);
                            assert_eq!(
                                c.wire_len(),
                                c.format_at(sample(), zone(offset)).len(),
                                "{c:?} at {offset}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn utc_defaults_off_and_round_trips_through_toml() {
        let c: TimestampConfig = toml::from_str("include_timezone = true").unwrap();
        assert!(!c.utc);
        let c = cfg(true, false, true, true);
        let text = toml::to_string(&c).unwrap();
        assert!(text.contains("utc = true"), "{text}");
        assert_eq!(toml::from_str::<TimestampConfig>(&text).unwrap(), c);
    }
}
