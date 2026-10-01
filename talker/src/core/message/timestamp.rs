//! Per-message ISO 8601 timestamp (spec §5.4).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Configuration for the timestamp prepended to a message's wire output.
///
/// Time-of-day (`HH:MM:SS`) is always present; the date, milliseconds, and
/// timezone designator are independently toggleable. A message carries a
/// timestamp only when [`MessageConfig::timestamp`] is `Some`.
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
    #[serde(default)]
    pub include_timezone: bool,
}

impl TimestampConfig {
    /// Number of ASCII bytes produced by [`Self::format`]. All timestamp
    /// components are fixed-width, so this is independent of the instant.
    pub(crate) const fn wire_len(&self) -> usize {
        8 + if self.include_date { 11 } else { 0 }
            + if self.include_millis { 4 } else { 0 }
            + if self.include_timezone { 1 } else { 0 }
    }

    /// Format `now` (UTC) as an ISO 8601 timestamp per this configuration.
    pub fn format(&self, now: DateTime<Utc>) -> String {
        let mut s = String::new();
        if self.include_date {
            s.push_str(&now.format("%Y-%m-%dT").to_string());
        }
        s.push_str(&now.format("%H:%M:%S").to_string());
        if self.include_millis {
            s.push_str(&now.format("%.3f").to_string());
        }
        if self.include_timezone {
            s.push('Z');
        }
        s
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

    #[test]
    fn full_timestamp() {
        let cfg = TimestampConfig {
            include_date: true,
            include_millis: true,
            include_timezone: true,
        };
        assert_eq!(cfg.format(sample()), "2026-05-22T14:30:45.123Z");
    }

    #[test]
    fn date_time_timezone_no_millis() {
        let cfg = TimestampConfig {
            include_date: true,
            include_millis: false,
            include_timezone: true,
        };
        assert_eq!(cfg.format(sample()), "2026-05-22T14:30:45Z");
    }

    #[test]
    fn time_and_millis_only() {
        let cfg = TimestampConfig {
            include_date: false,
            include_millis: true,
            include_timezone: false,
        };
        assert_eq!(cfg.format(sample()), "14:30:45.123");
    }

    #[test]
    fn time_only_is_the_default() {
        assert_eq!(TimestampConfig::default().format(sample()), "14:30:45");
    }

    #[test]
    fn wire_len_matches_every_formatted_shape() {
        for include_date in [false, true] {
            for include_millis in [false, true] {
                for include_timezone in [false, true] {
                    let cfg = TimestampConfig {
                        include_date,
                        include_millis,
                        include_timezone,
                    };
                    assert_eq!(cfg.wire_len(), cfg.format(sample()).len());
                }
            }
        }
    }
}
