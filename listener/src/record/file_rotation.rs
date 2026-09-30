//! Time-based recording file rotation: period keys and file names (spec §59).
//!
//! A rotating recording writes to a **directory** and opens a new file at each
//! calendar period (Hourly/Daily), named for the period start:
//! `<channel>_<start-time><ext>` (§59). Rotation is **data-driven** — the period
//! is taken from each item's wall-clock arrival time, so a quiet period produces
//! no file and a rotation happens when the first item of the next period arrives.
//! Period keys are in **local time** so filenames match the operator's wall clock
//! (`<channel>_2026-06-03_08` is the local 08:00 hour). The trade-off is the DST edge
//! (a repeated/skipped local hour around the change) — see `period_key`. Each file
//! stays contiguous and byte-exact for the data it holds; a rotation is a clean file
//! boundary, never a gap (§56).

use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Local};

use crate::core::RecordError;

use super::FileRotationPolicy;

/// Ensure the rotation **directory** exists (§59), with a clear error when the path is
/// an existing **file**. `create_dir_all` on a path that is already a file fails with an
/// opaque OS message ("cannot create a file when that file already exists", error 183 on
/// Windows); catch that case explicitly so the user is told the real problem — rotation
/// needs a folder, not a file.
///
/// Called for the first file of an enable only: recovery never creates the folder
/// (§59), because an unplugged drive can leave its mount point on the system disk.
/// Blocking — run it off the async runtime.
pub(crate) fn ensure_rotation_dir(dir: &Path) -> Result<(), RecordError> {
    if dir.is_file() {
        return Err(RecordError::RotationDestinationIsFile(
            dir.display().to_string(),
        ));
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// The period key for `at` under `policy` — the rotation trigger *and* the
/// filename time component (§59), in **local time**. `None` policy has no period.
///
/// Local time so rotated filenames match the operator's wall clock (a `…_08` hourly
/// file is the local 08:00 hour, not a UTC hour they must mentally offset). The
/// trade-off is the DST edge UTC avoided: on a fall-back the local "01:00" hour repeats,
/// so two consecutive periods can produce the same key and append to one file; on a
/// spring-forward an hour's key is simply skipped. Both are rare and harmless to the
/// byte stream (still contiguous), and the local-time readability is the deliberate
/// choice here.
pub(crate) fn period_key(policy: FileRotationPolicy, at: SystemTime) -> Option<String> {
    let dt: DateTime<Local> = at.into();
    match policy {
        FileRotationPolicy::None => None,
        FileRotationPolicy::Hourly => Some(dt.format("%Y-%m-%d_%H").to_string()),
        FileRotationPolicy::Daily => Some(dt.format("%Y-%m-%d").to_string()),
    }
}

/// `<channel>_<key><ext>` (§59), e.g. `GPS_2026-06-03_08.raw`.
pub(crate) fn rotation_filename(channel: &str, key: &str, ext: &str) -> String {
    format!("{channel}_{key}{ext}")
}

/// Whether `name` is safe to embed in a recording filename (§59, §71): non-empty,
/// bounded length, no path separators or reserved characters, no control bytes,
/// no trailing dot/space, and not a Windows reserved device name. Validated at
/// config time when rotation is enabled; rejected, never silently sanitized.
pub fn is_filesystem_safe(name: &str) -> bool {
    const MAX_LEN: usize = 64;
    if name.is_empty() || name.len() > MAX_LEN {
        return false;
    }
    if name.chars().any(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return false;
    }
    // Windows rejects names with a trailing space or dot.
    if name.ends_with(' ') || name.ends_with('.') {
        return false;
    }
    // Windows reserved device names (case-insensitive, ignoring any extension).
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    !RESERVED.contains(&stem.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A `SystemTime` at a fixed **local** instant, so period keys are deterministic
    /// regardless of the test machine's timezone (period keys are local time now).
    /// Avoids the DST-ambiguity edge by picking ordinary mid-day/mid-night times.
    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> SystemTime {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("unambiguous local time")
            .into()
    }

    #[test]
    fn a_rotation_folder_that_is_an_existing_file_gives_a_clear_error() {
        // §59: with rotation the destination is a folder. If the path is an existing
        // *file*, the user gets a clear RotationDestinationIsFile error, not the opaque
        // OS "cannot create a file when that file already exists" from create_dir_all.
        let mut path = std::env::temp_dir();
        path.push(format!("listener-rot-file-{}.raw", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"i am a file, not a folder").unwrap();
        let result = ensure_rotation_dir(&path);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            result,
            Err(RecordError::RotationDestinationIsFile(_))
        ));
    }

    #[test]
    fn period_keys_use_local_time_at_the_right_resolution() {
        let at = local(2026, 6, 3, 8, 30);
        assert_eq!(
            period_key(FileRotationPolicy::Hourly, at).as_deref(),
            Some("2026-06-03_08")
        );
        assert_eq!(
            period_key(FileRotationPolicy::Daily, at).as_deref(),
            Some("2026-06-03")
        );
        assert_eq!(period_key(FileRotationPolicy::None, at), None);
    }

    #[test]
    fn filesystem_safety_accepts_plain_names_and_rejects_unsafe_ones() {
        for ok in ["GPS", "GPS-1", "ais_receiver", "Bow Antenna"] {
            assert!(is_filesystem_safe(ok), "{ok} should be safe");
        }
        for bad in [
            "",
            "a/b",
            "a\\b",
            "c:name",
            "star*",
            "q?",
            "CON",
            "com1",
            "nul.dat",
            "trailing.",
            "trailing ",
            "ctrl\u{0007}",
        ] {
            assert!(!is_filesystem_safe(bad), "{bad:?} should be rejected");
        }
        assert!(!is_filesystem_safe(&"x".repeat(65)));
    }
}
