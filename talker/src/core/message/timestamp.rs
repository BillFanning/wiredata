//! Per-message ISO 8601 timestamp (spec §5.4).

use std::fmt::Write as _;
use std::time::SystemTime;

use jiff::tz::{Offset, TimeZone};
use jiff::Timestamp;
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
    pub fn format(&self, now: Timestamp) -> String {
        self.format_at(now, TimeZone::system().to_offset(now))
    }

    /// [`Self::format`] with the local offset given, so the zone is not read
    /// from the machine. Written field by field: every field has a fixed
    /// width, which [`Self::wire_len`] relies on, and milliseconds are cut,
    /// never rounded.
    pub(crate) fn format_at(&self, now: Timestamp, local: Offset) -> String {
        let at = if self.utc {
            Offset::UTC.to_datetime(now)
        } else {
            local.to_datetime(now)
        };
        let mut out = String::with_capacity(self.wire_len());
        // Writing to a String cannot fail.
        if self.include_date {
            let _ = write!(out, "{:04}-{:02}-{:02}T", at.year(), at.month(), at.day());
        }
        let _ = write!(
            out,
            "{:02}:{:02}:{:02}",
            at.hour(),
            at.minute(),
            at.second()
        );
        if self.include_millis {
            let _ = write!(out, ".{:03}", at.subsec_nanosecond() / 1_000_000);
        }
        if self.include_timezone {
            if self.utc {
                out.push('Z');
            } else {
                out.push_str(&offset_text(local));
            }
        }
        out
    }
}

/// `offset` as ISO 8601 writes it, `+hh:mm`: `+00:00` for UTC, and any
/// seconds left off, so the width is always six.
pub(crate) fn offset_text(offset: Offset) -> String {
    let seconds = offset.seconds();
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// `at` as a jiff timestamp. A clock reading is always within jiff's range,
/// the years -9999 to 9999; one outside it is held at the nearer end.
pub(crate) fn to_timestamp(at: SystemTime) -> Timestamp {
    Timestamp::try_from(at).unwrap_or(if at < SystemTime::UNIX_EPOCH {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-05-22 14:30:45.123 UTC
    fn sample() -> Timestamp {
        instant(1_779_460_245, 123_000_000)
    }

    fn zone(seconds: i32) -> Offset {
        Offset::from_seconds(seconds).unwrap()
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

    /// `seconds` after 1970 UTC, plus `nanos`.
    fn instant(seconds: i64, nanos: i32) -> Timestamp {
        Timestamp::new(seconds, nanos).unwrap()
    }

    /// Milliseconds are cut, never rounded, and always three digits.
    #[test]
    fn milliseconds_are_truncated_to_three_digits() {
        let c = cfg(false, true, false, true);
        // 2026-05-22 14:30:45 UTC
        assert_eq!(
            c.format_at(instant(1_779_460_245, 999_999_999), zone(0)),
            "14:30:45.999"
        );
        assert_eq!(
            c.format_at(instant(1_779_460_245, 0), zone(0)),
            "14:30:45.000"
        );
        assert_eq!(
            c.format_at(instant(1_779_460_245, 7_000_000), zone(0)),
            "14:30:45.007"
        );
    }

    /// A zero local offset is written `+00:00`, never `Z`, and a zone west
    /// of Greenwich can put the local date on the day before.
    #[test]
    fn a_zero_local_offset_and_a_west_zone_across_midnight() {
        let c = cfg(true, false, true, false);
        assert_eq!(c.format_at(sample(), zone(0)), "2026-05-22T14:30:45+00:00");
        // 2026-05-22 03:00:00 UTC
        assert_eq!(
            c.format_at(instant(1_779_418_800, 0), zone(-5 * 3600)),
            "2026-05-21T22:00:00-05:00"
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
        let local = TimeZone::system().to_offset(sample());
        let c = cfg(true, true, true, false);
        assert_eq!(c.format(sample()), c.format_at(sample(), local));
        assert!(
            c.format(sample()).ends_with(&offset_text(local)),
            "{}",
            c.format(sample())
        );
    }

    #[test]
    fn offsets_are_written_as_iso_8601_hours_and_minutes() {
        assert_eq!(offset_text(zone(0)), "+00:00");
        assert_eq!(offset_text(zone(-4 * 3600)), "-04:00");
        assert_eq!(offset_text(zone(5 * 3600 + 1800)), "+05:30");
        assert_eq!(offset_text(zone(-(3 * 3600 + 1800))), "-03:30");
        assert_eq!(offset_text(zone(14 * 3600)), "+14:00");
    }

    #[test]
    fn a_clock_reading_converts_and_one_out_of_range_is_held_at_the_end() {
        use std::time::Duration;
        let at = SystemTime::UNIX_EPOCH + Duration::new(1_779_460_245, 123_000_000);
        assert_eq!(to_timestamp(at), sample());
        let far = SystemTime::UNIX_EPOCH + Duration::from_secs(400_000_000_000);
        assert_eq!(to_timestamp(far), Timestamp::MAX);
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
