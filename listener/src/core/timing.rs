//! Chunk-arrival timing types (spec §138).
//!
//! In the stream-only design (ADR-010) there is no Message model; the only
//! timing the runtime needs is the per-chunk arrival time. Per-byte arrival time
//! is not available from the OS, so all timing is chunk-granular: a chunk's
//! [`ChunkTime`] is captured when the transport reads it and carried through to
//! recording (the `.raw.idx` sidecar, Display-rotation period keys) and Mark
//! timestamps.

use std::time::{Instant, SystemTime};

use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
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
    /// Userland post-read timestamps are in use (the default for every transport).
    #[default]
    PostRead,
    /// Kernel software receive timestamps were requested and activated.
    KernelSoftware,
    /// Kernel timestamps were requested, but this platform/socket could not provide them.
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
        let local: DateTime<Local> = at.into();
        let mut s = String::new();
        if self.include_date {
            s.push_str(&local.format("%Y-%m-%dT").to_string());
        }
        s.push_str(&local.format("%H:%M:%S").to_string());
        if self.include_millis {
            s.push_str(&local.format("%.3f").to_string());
        }
        if self.include_timezone {
            s.push_str(&local.format("%:z").to_string());
        }
        s
    }
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
/// The clock/date fields are UTC. Zone fields carry the local UTC offset when
/// it fits ZDA's conventional signed-hour/minute representation.
pub fn zda_sentence(
    talker: &str,
    at: SystemTime,
    include_millis: bool,
) -> Result<String, ZdaTalkerIdError> {
    let utc: DateTime<Utc> = at.into();
    let local: DateTime<Local> = at.into();
    zda_sentence_at(talker, utc, local.offset().fix(), include_millis)
}

fn zda_sentence_at(
    talker: &str,
    at: DateTime<Utc>,
    local_offset: FixedOffset,
    include_millis: bool,
) -> Result<String, ZdaTalkerIdError> {
    use chrono::{Datelike, Timelike};
    use nmea0183::{format_utc_time, NmeaSentence, SentenceType, TalkerId};

    validate_zda_talker_id(talker)?;
    let time = format_utc_time(
        at.hour(),
        at.minute(),
        at.second(),
        at.timestamp_subsec_millis(),
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
    let talker: TalkerId = talker.parse().expect("TalkerId parse is infallible");
    let wire = NmeaSentence::new(talker, SentenceType::ZDA, fields).to_wire();
    Ok(wire
        .strip_suffix("\r\n")
        .expect("NMEA wire construction always appends CRLF")
        .to_string())
}

fn zda_zone_fields(offset: FixedOffset) -> (String, String) {
    let seconds = offset.local_minus_utc();
    let total_minutes = seconds / 60;
    if seconds % 60 != 0 || total_minutes.unsigned_abs() > 13 * 60 + 59 {
        return (String::new(), String::new());
    }
    let absolute = total_minutes.unsigned_abs();
    let hours = match total_minutes.cmp(&0) {
        std::cmp::Ordering::Less => format!("-{:02}", absolute / 60),
        std::cmp::Ordering::Equal => "00".to_string(),
        std::cmp::Ordering::Greater => format!("+{:02}", absolute / 60),
    };
    (hours, format!("{:02}", absolute % 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed *local* instant (2026-05-22 14:30:45.123 local) round-tripped through
    /// `SystemTime`, so `format` reproduces the same local wall-clock regardless of
    /// the host timezone.
    fn sample() -> SystemTime {
        use chrono::{TimeZone, Timelike};
        let local = Local
            .with_ymd_and_hms(2026, 5, 22, 14, 30, 45)
            .unwrap()
            .with_nanosecond(123_000_000)
            .unwrap();
        SystemTime::from(local)
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
        use chrono::{DateTime, Local};
        let cfg = TimestampConfig {
            include_date: false,
            include_millis: false,
            include_timezone: true,
        };
        // The emitted suffix is the local UTC offset for that instant (e.g. -07:00),
        // never a fixed `Z`. Compare against chrono's own offset for the same instant.
        let local: DateTime<Local> = sample().into();
        let expected = format!("14:30:45{}", local.format("%:z"));
        assert_eq!(cfg.format(sample()), expected);
    }

    fn utc_sample() -> DateTime<Utc> {
        use chrono::{TimeZone, Timelike};
        Utc.with_ymd_and_hms(2026, 5, 22, 0, 30, 45)
            .unwrap()
            .with_nanosecond(123_000_000)
            .unwrap()
    }

    #[test]
    fn zda_accepts_long_custom_talker_and_emits_valid_checksum_without_crlf() {
        let text = zda_sentence_at(
            "RECEIVER_A",
            utc_sample(),
            FixedOffset::west_opt(3 * 3600 + 30 * 60).unwrap(),
            true,
        )
        .unwrap();

        assert_eq!(
            text.split('*').next().unwrap(),
            "$RECEIVER_AZDA,003045.123,22,05,2026,-03,30"
        );
        assert!(!text.contains(['\r', '\n']));
        let parsed = nmea0183::NmeaSentence::parse(&text).unwrap();
        assert_eq!(parsed.talker_id.to_string(), "RECEIVER_A");
        assert_eq!(parsed.sentence_type, nmea0183::SentenceType::ZDA);
    }

    #[test]
    fn zda_uses_utc_calendar_fields_not_local_calendar_date() {
        let text = zda_sentence_at(
            "GP",
            utc_sample(),
            FixedOffset::west_opt(5 * 3600).unwrap(),
            false,
        )
        .unwrap();
        let parsed = nmea0183::NmeaSentence::parse(&text).unwrap();

        assert_eq!(parsed.fields, ["003045", "22", "05", "2026", "-05", "00"]);
    }

    #[test]
    fn zda_helper_formats_a_chrono_leap_second_at_fixed_width() {
        use chrono::{TimeZone, Timelike};

        let leap = Utc
            .with_ymd_and_hms(2016, 12, 31, 23, 59, 59)
            .unwrap()
            .with_nanosecond(1_800_000_000)
            .unwrap();
        let text = zda_sentence_at("GP", leap, FixedOffset::east_opt(0).unwrap(), true).unwrap();
        let parsed = nmea0183::NmeaSentence::parse(&text).unwrap();

        assert_eq!(parsed.field(0), Some("235960.800"));
    }

    #[test]
    fn zda_zone_fields_cover_zero_positive_and_nonstandard_offsets() {
        assert_eq!(
            zda_zone_fields(FixedOffset::east_opt(0).unwrap()),
            ("00".to_string(), "00".to_string())
        );
        assert_eq!(
            zda_zone_fields(FixedOffset::east_opt(13 * 3600 + 45 * 60).unwrap()),
            ("+13".to_string(), "45".to_string())
        );
        assert_eq!(
            zda_zone_fields(FixedOffset::east_opt(14 * 3600).unwrap()),
            (String::new(), String::new())
        );
        assert_eq!(
            zda_zone_fields(FixedOffset::east_opt(3601).unwrap()),
            (String::new(), String::new())
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
