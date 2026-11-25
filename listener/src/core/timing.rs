//! Chunk-arrival timing types (spec §138).
//!
//! In the stream-only design (ADR-010) there is no Message model; the only
//! timing the runtime needs is the per-chunk arrival time. Per-byte arrival time
//! is not available from the OS, so all timing is chunk-granular: a chunk's
//! [`ChunkTime`] is captured when the transport reads it and carried through to
//! recording (the `.raw.idx` sidecar, Display-rotation period keys) and Mark
//! timestamps.

use std::time::{Instant, SystemTime};

use std::fmt::Write as _;

use jiff::tz::{Offset, TimeZone};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Where a chunk's wall-clock arrival timestamp was captured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArrivalTimestampSource {
    /// Captured in userland immediately after the transport read completed.
    #[default]
    PostRead,
    /// Captured by the kernel when a UDP datagram entered the socket receive path.
    KernelSoftware,
}

/// Effective timestamping policy for a running transport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArrivalTimestampStatus {
    /// Userland post-read timestamps are in use: serial, and UDP on Windows
    /// and macOS.
    #[default]
    PostRead,
    /// Kernel software receive timestamps are in use: UDP on Linux.
    KernelSoftware,
    /// Linux could not turn kernel timestamps on for this socket, so post-read
    /// timestamps are used.
    KernelRequestedUnavailable,
}

/// A single timing capture taken when a transport chunk is read (§138).
///
/// `monotonic` orders chunks, measures durations, and breaks timestamp ties
/// (§125); `wall_clock` renders Local/UTC display times (§26). Precision is not
/// accuracy — userland arrival times carry OS scheduling jitter (§26, §133).
#[derive(Clone, Copy, Debug)]
pub struct ChunkTime {
    /// Always captured immediately after the OS read returns. Runtime duration
    /// telemetry uses this monotonic point even when `wall_clock` came from the kernel.
    pub monotonic: Instant,
    pub wall_clock: SystemTime,
    pub wall_clock_source: ArrivalTimestampSource,
}

impl ChunkTime {
    /// Capture the current monotonic and wall-clock time together.
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall_clock: SystemTime::now(),
            wall_clock_source: ArrivalTimestampSource::PostRead,
        }
    }

    /// Replace only the display/recording wall clock with an OS receive timestamp.
    /// The monotonic post-read capture remains unchanged for queue-delay telemetry.
    pub fn with_kernel_wall_clock(mut self, wall_clock: SystemTime) -> Self {
        self.wall_clock = wall_clock;
        self.wall_clock_source = ArrivalTimestampSource::KernelSoftware;
        self
    }
}

/// Configuration for an inline timestamp rendered next to a matched byte pattern
/// (§50.2 Mark). Mirrors talker's `TimestampConfig` field-for-field, but formats in
/// **Local** time (talker formats UTC) — the listener shows a local wall-clock time
/// because it is a field troubleshooting/logging tool (§26).
///
/// Time-of-day (`HH:MM:SS`) is always present; the date, milliseconds, and timezone
/// offset are independently toggleable. Default = `HH:MM:SS`. NMEA ZDA Mark
/// style reuses `include_millis` and ignores the other two toggles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
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
    /// Format a wall-clock instant as a **local** timestamp per this configuration.
    /// The timezone toggle emits the local UTC offset (e.g. `-07:00`), not `Z`.
    pub fn format(&self, at: SystemTime) -> String {
        self.format_at(at, TimeZone::system().to_offset(to_timestamp(at)))
    }

    /// [`Self::format`] in the zone `offset`, so a test does not read the
    /// machine's zone. Milliseconds are cut, never rounded.
    fn format_at(&self, at: SystemTime, offset: Offset) -> String {
        let local = offset.to_datetime(to_timestamp(at));
        let mut s = String::new();
        // Writing to a String cannot fail.
        if self.include_date {
            let _ = write!(
                s,
                "{:04}-{:02}-{:02}T",
                local.year(),
                local.month(),
                local.day()
            );
        }
        let _ = write!(
            s,
            "{:02}:{:02}:{:02}",
            local.hour(),
            local.minute(),
            local.second()
        );
        if self.include_millis {
            let _ = write!(s, ".{:03}", local.subsec_nanosecond() / 1_000_000);
        }
        if self.include_timezone {
            s.push_str(&offset_text(offset));
        }
        s
    }
}

/// `at` as ISO 8601 local time in the zone `offset`, with milliseconds cut
/// rather than rounded: `2026-11-13T14:02:04.000-05:00`.
pub(crate) fn iso_local_at(at: SystemTime, offset: Offset) -> String {
    TimestampConfig {
        include_date: true,
        include_millis: true,
        include_timezone: true,
    }
    .format_at(at, offset)
}

/// `offset` as ISO 8601 writes it, `+hh:mm`: `+00:00` for UTC, and any
/// seconds left off.
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

/// Safety cap for custom NMEA ZDA talker IDs used by Mark annotations.
pub const MAX_ZDA_TALKER_ID_BYTES: usize = 32;

/// Why a custom talker ID cannot be emitted safely inside an NMEA ZDA Mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ZdaTalkerIdError {
    #[error("the ID is empty")]
    Empty,
    #[error("the ID is longer than {MAX_ZDA_TALKER_ID_BYTES} bytes")]
    TooLong,
    #[error("the ID contains {0:?}; use printable ASCII without whitespace or $ ! , *")]
    InvalidCharacter(char),
}

/// Validate an NMEA ZDA Mark talker ID. Standard two-character IDs and longer
/// custom IDs share this framing policy.
pub fn validate_zda_talker_id(talker: &str) -> Result<(), ZdaTalkerIdError> {
    if talker.is_empty() {
        return Err(ZdaTalkerIdError::Empty);
    }
    if talker.len() > MAX_ZDA_TALKER_ID_BYTES {
        return Err(ZdaTalkerIdError::TooLong);
    }
    if let Some(c) = talker
        .chars()
        .find(|c| !c.is_ascii_graphic() || matches!(c, '$' | '!' | ',' | '*'))
    {
        return Err(ZdaTalkerIdError::InvalidCharacter(c));
    }
    Ok(())
}

/// Construct an inline `$--ZDA` annotation for `at`, without trailing CR/LF.
/// The clock/date fields are UTC. The zone fields are NMEA's local zone (the
/// time added to local time to get UTC; see `zda_zone_fields`) when it fits
/// ZDA's signed-hour/minute range.
pub fn zda_sentence(
    talker: &str,
    at: SystemTime,
    include_millis: bool,
) -> Result<String, ZdaTalkerIdError> {
    let at = to_timestamp(at);
    zda_sentence_at(talker, at, TimeZone::system().to_offset(at), include_millis)
}

fn zda_sentence_at(
    talker: &str,
    at: Timestamp,
    local_offset: Offset,
    include_millis: bool,
) -> Result<String, ZdaTalkerIdError> {
    use nmea0183::{format_utc_time, NmeaSentence, SentenceType, TalkerId};

    validate_zda_talker_id(talker)?;
    let at = Offset::UTC.to_datetime(at);
    let field = |value: i8| u32::from(value.unsigned_abs());
    let time = format_utc_time(
        field(at.hour()),
        field(at.minute()),
        field(at.second()),
        at.subsec_nanosecond().unsigned_abs() / 1_000_000,
        include_millis,
    );
    let (zone_hours, zone_minutes) = zda_zone_fields(local_offset);
    let fields = vec![
        time,
        format!("{:02}", at.day()),
        format!("{:02}", at.month()),
        format!("{:04}", at.year()),
        zone_hours,
        zone_minutes,
    ];
    let talker: TalkerId = TalkerId::from(talker);
    let wire = NmeaSentence::new(talker, SentenceType::ZDA, fields).to_wire();
    Ok(wire
        .strip_suffix("\r\n")
        .expect("NMEA wire construction always appends CRLF")
        .to_string())
}

/// ZDA's local-zone hours and minutes for `offset`, in NMEA 0183's sense: the
/// time added to local time to get UTC. That is UTC minus local, the opposite
/// sign to ISO 8601's offset, so a zone east of Greenwich is negative
/// (§50.2). The minutes take the hours' sign. Both are empty for an offset
/// that is not a whole number of minutes or is beyond ±13:59.
fn zda_zone_fields(offset: Offset) -> (String, String) {
    let seconds = -offset.seconds();
    let total_minutes = seconds / 60;
    if seconds % 60 != 0 || total_minutes.unsigned_abs() > 13 * 60 + 59 {
        return (String::new(), String::new());
    }
    let absolute = total_minutes.unsigned_abs();
    let (hours, minutes) = (absolute / 60, absolute % 60);
    match total_minutes.cmp(&0) {
        std::cmp::Ordering::Equal => ("00".to_string(), "00".to_string()),
        std::cmp::Ordering::Greater => (format!("+{hours:02}"), format!("{minutes:02}")),
        std::cmp::Ordering::Less => {
            let minutes = if minutes == 0 {
                "00".to_string()
            } else {
                format!("-{minutes:02}")
            };
            (format!("-{hours:02}"), minutes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed *local* instant (2026-05-22 14:30:45.123 local) round-tripped through
    /// `SystemTime`, so `format` reproduces the same local wall-clock regardless of
    /// the host timezone.
    fn sample() -> SystemTime {
        let local = jiff::civil::date(2026, 5, 22)
            .at(14, 30, 45, 123_000_000)
            .to_zoned_datetime(TimeZone::system())
            .unwrap();
        SystemTime::try_from(local.timestamp()).unwrap()
    }

    #[test]
    fn time_only_is_the_default() {
        assert_eq!(TimestampConfig::default().format(sample()), "14:30:45");
    }

    #[test]
    fn kernel_wall_clock_does_not_replace_the_post_read_monotonic_capture() {
        let post_read = ChunkTime::now();
        let kernel_wall_clock = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(42);
        let stamped = post_read.with_kernel_wall_clock(kernel_wall_clock);

        assert_eq!(stamped.monotonic, post_read.monotonic);
        assert_eq!(stamped.wall_clock, kernel_wall_clock);
        assert_eq!(
            stamped.wall_clock_source,
            ArrivalTimestampSource::KernelSoftware
        );
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
    fn date_and_time_no_millis() {
        let cfg = TimestampConfig {
            include_date: true,
            include_millis: false,
            include_timezone: false,
        };
        assert_eq!(cfg.format(sample()), "2026-05-22T14:30:45");
    }

    #[test]
    fn timezone_offset_matches_the_local_offset() {
        let cfg = TimestampConfig {
            include_date: false,
            include_millis: false,
            include_timezone: true,
        };
        // The emitted suffix is the local UTC offset for that instant (e.g. -07:00),
        // never a fixed `Z`. Compare against the zone's offset for the same instant.
        let offset = TimeZone::system().to_offset(to_timestamp(sample()));
        let expected = format!("14:30:45{}", offset_text(offset));
        assert_eq!(cfg.format(sample()), expected);
    }

    /// 2026-05-22 00:30:45.123 UTC
    fn utc_sample() -> Timestamp {
        Timestamp::new(1_779_409_845, 123_000_000).unwrap()
    }

    #[test]
    fn zda_accepts_long_custom_talker_and_emits_valid_checksum_without_crlf() {
        let text = zda_sentence_at(
            "RECEIVER_A",
            utc_sample(),
            zone(-(3 * 3600 + 30 * 60)),
            true,
        )
        .unwrap();

        assert_eq!(
            text.split('*').next().unwrap(),
            "$RECEIVER_AZDA,003045.123,22,05,2026,+03,30"
        );
        assert!(!text.contains(['\r', '\n']));
        let parsed = nmea0183::NmeaSentence::parse(&text).unwrap();
        assert_eq!(parsed.talker_id.to_string(), "RECEIVER_A");
        assert_eq!(parsed.sentence_type, nmea0183::SentenceType::ZDA);
    }

    #[test]
    fn zda_uses_utc_calendar_fields_not_local_calendar_date() {
        let text = zda_sentence_at("GP", utc_sample(), zone(-(5 * 3600)), false).unwrap();
        let parsed = nmea0183::NmeaSentence::parse(&text).unwrap();

        assert_eq!(parsed.fields, ["003045", "22", "05", "2026", "+05", "00"]);
    }

    /// NMEA's zone is the hours added to local time to get UTC, the opposite
    /// sign to ISO 8601's offset: a zone east of Greenwich is negative, and the
    /// minutes take the hours' sign.
    #[test]
    fn zda_zone_fields_follow_the_nmea_sign() {
        let fields = |hours: &str, minutes: &str| (hours.to_string(), minutes.to_string());
        assert_eq!(zda_zone_fields(zone(0)), fields("00", "00"));
        // US Eastern, UTC-5: add 5 hours to local time to get UTC.
        assert_eq!(zda_zone_fields(zone(-(5 * 3600))), fields("+05", "00"));
        // Central Europe, UTC+1.
        assert_eq!(zda_zone_fields(zone(3600)), fields("-01", "00"));
        // India, UTC+5:30, and the edge of the range, UTC+13:45.
        assert_eq!(
            zda_zone_fields(zone(5 * 3600 + 30 * 60)),
            fields("-05", "-30")
        );
        assert_eq!(
            zda_zone_fields(zone(13 * 3600 + 45 * 60)),
            fields("-13", "-45")
        );
        // Newfoundland, UTC-3:30.
        assert_eq!(
            zda_zone_fields(zone(-(3 * 3600 + 30 * 60))),
            fields("+03", "30")
        );
        assert_eq!(
            zda_zone_fields(zone(14 * 3600)),
            (String::new(), String::new())
        );
        assert_eq!(zda_zone_fields(zone(3601)), (String::new(), String::new()));
    }

    fn zone(seconds: i32) -> Offset {
        Offset::from_seconds(seconds).unwrap()
    }

    /// 2026-05-22 14:30:45 UTC plus `nanos`.
    fn wall(nanos: u32) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::new(1_779_460_245, nanos)
    }

    /// Every toggle, in fixed zones, with literal results: milliseconds cut
    /// rather than rounded, `+00:00` for a zero offset, and a local date that
    /// differs from UTC's.
    #[test]
    fn mark_timestamps_in_fixed_zones() {
        let cfg = |include_date, include_millis, include_timezone| TimestampConfig {
            include_date,
            include_millis,
            include_timezone,
        };
        let at = wall(123_999_999);
        assert_eq!(cfg(false, false, false).format_at(at, zone(0)), "14:30:45");
        assert_eq!(
            cfg(false, true, false).format_at(at, zone(0)),
            "14:30:45.123"
        );
        assert_eq!(
            cfg(false, false, true).format_at(at, zone(0)),
            "14:30:45+00:00"
        );
        assert_eq!(
            cfg(true, true, true).format_at(at, zone(-4 * 3600)),
            "2026-05-22T10:30:45.123-04:00"
        );
        assert_eq!(
            cfg(true, false, true).format_at(at, zone(9 * 3600 + 1800)),
            "2026-05-23T00:00:45+09:30"
        );
        assert_eq!(
            cfg(false, true, false).format_at(wall(0), zone(0)),
            "14:30:45.000"
        );
    }

    /// ZDA's milliseconds are cut, never rounded up into the next second.
    #[test]
    fn zda_milliseconds_are_truncated() {
        let at = to_timestamp(wall(999_999_999));
        let text = zda_sentence_at("GP", at, zone(0), true).unwrap();
        let parsed = nmea0183::NmeaSentence::parse(&text).unwrap();
        assert_eq!(
            parsed.fields,
            ["143045.999", "22", "05", "2026", "00", "00"]
        );
    }

    #[test]
    fn zda_talker_validation_rejects_framing_controls_and_oversize_ids() {
        assert!(validate_zda_talker_id("A").is_ok());
        assert!(validate_zda_talker_id("CUSTOM_RECEIVER_123").is_ok());
        assert_eq!(validate_zda_talker_id(""), Err(ZdaTalkerIdError::Empty));
        assert_eq!(
            validate_zda_talker_id("G,P"),
            Err(ZdaTalkerIdError::InvalidCharacter(','))
        );
        assert_eq!(
            validate_zda_talker_id("GP\n"),
            Err(ZdaTalkerIdError::InvalidCharacter('\n'))
        );
        assert_eq!(
            validate_zda_talker_id(&"X".repeat(MAX_ZDA_TALKER_ID_BYTES + 1)),
            Err(ZdaTalkerIdError::TooLong)
        );
    }
}
