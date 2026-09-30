//! Time-based recording file rotation (spec §59).
//!
//! A [`RotatingRawRecorder`] / [`RotatingDisplayRecorder`] wraps a per-period
//! file recorder ([`file`](super::file)). It writes to a **directory** and opens
//! a new file at each calendar period (Hourly/Daily), named for the period start:
//! `<channel>_<start-time><ext>` (§59). Rotation is **data-driven** — the period
//! is taken from each item's wall-clock arrival time, so a quiet period produces
//! no file and a rotation happens when the first item of the next period arrives.
//! Period keys are in **local time** so filenames match the operator's wall clock
//! (`<channel>_2026-06-03_08` is the local 08:00 hour). The trade-off is the DST edge
//! (a repeated/skipped local hour around the change) — see `period_key`. Each file
//! stays contiguous and byte-exact for the data it holds; a rotation is a clean file
//! boundary, never a gap (§56).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local};

use crate::core::RecordError;
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

use super::file::{DisplayFileRecorder, RawFileRecorder};
use super::{
    DisplayRecorder, FileRotationPolicy, OverwritePolicy, RawRecorder, RecordingStopReason,
};

/// Ensure the rotation **directory** exists (§59), with a clear error when the path is
/// an existing **file**. `create_dir_all` on a path that is already a file fails with an
/// opaque OS message ("cannot create a file when that file already exists", error 183 on
/// Windows); catch that case explicitly so the user is told the real problem — rotation
/// needs a folder, not a file.
async fn ensure_rotation_dir(dir: &Path) -> Result<(), RecordError> {
    if dir.is_file() {
        return Err(RecordError::RotationDestinationIsFile(
            dir.display().to_string(),
        ));
    }
    tokio::fs::create_dir_all(dir).await?;
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
fn rotation_filename(channel: &str, key: &str, ext: &str) -> String {
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

/// Raw Recording with time-based rotation (§59). Writes `.raw` files into `dir`,
/// one per calendar period.
pub struct RotatingRawRecorder {
    dir: PathBuf,
    channel: String,
    ext: String,
    policy: OverwritePolicy,
    timestamps: bool,
    rotation: FileRotationPolicy,
    current_key: String,
    inner: RawFileRecorder,
}

impl RotatingRawRecorder {
    /// Create the recorder and eagerly open the current period's file, so an
    /// enable-time failure (missing directory, refused overwrite) surfaces now
    /// (§55) rather than on first write. `rotation` must not be `None`.
    pub async fn create(
        dir: &Path,
        channel: &str,
        ext: &str,
        policy: OverwritePolicy,
        timestamps: bool,
        rotation: FileRotationPolicy,
    ) -> Result<Self, RecordError> {
        ensure_rotation_dir(dir).await?;
        let key = period_key(rotation, SystemTime::now())
            .expect("RotatingRawRecorder requires a rotation period");
        let path = dir.join(rotation_filename(channel, &key, ext));
        let inner = RawFileRecorder::create(&path, policy, timestamps).await?;
        Ok(Self {
            dir: dir.to_owned(),
            channel: channel.to_owned(),
            ext: ext.to_owned(),
            policy,
            timestamps,
            rotation,
            current_key: key,
            inner,
        })
    }

    async fn rotate_to(&mut self, key: String) -> Result<(), RecordError> {
        // Flush + close the current file (a clean boundary, §56), then open the
        // next. Replacing `inner` drops the old recorder, closing its file.
        self.inner
            .finalize(RecordingStopReason::ChannelStopped)
            .await?;
        let path = self
            .dir
            .join(rotation_filename(&self.channel, &key, &self.ext));
        self.inner = RawFileRecorder::create(&path, self.policy, self.timestamps).await?;
        self.current_key = key;
        Ok(())
    }
}

#[async_trait::async_trait]
impl RawRecorder for RotatingRawRecorder {
    async fn write_chunk(&mut self, chunk: &ReceivedData) -> Result<(), RecordError> {
        if let Some(key) = period_key(self.rotation, chunk.received_at.wall_clock) {
            if key != self.current_key {
                self.rotate_to(key).await?;
            }
        }
        self.inner.write_chunk(chunk).await
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.inner.flush().await
    }

    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        self.inner.finalize(reason).await
    }
}

/// Display Recording with time-based rotation (§59). Writes `.disp` files into
/// `dir`, one per calendar period.
pub struct RotatingDisplayRecorder {
    dir: PathBuf,
    channel: String,
    ext: String,
    policy: OverwritePolicy,
    rotation: FileRotationPolicy,
    current_key: String,
    inner: DisplayFileRecorder,
}

impl RotatingDisplayRecorder {
    pub async fn create(
        dir: &Path,
        channel: &str,
        ext: &str,
        policy: OverwritePolicy,
        rotation: FileRotationPolicy,
    ) -> Result<Self, RecordError> {
        ensure_rotation_dir(dir).await?;
        let key = period_key(rotation, SystemTime::now())
            .expect("RotatingDisplayRecorder requires a rotation period");
        let path = dir.join(rotation_filename(channel, &key, ext));
        let inner = DisplayFileRecorder::create(&path, policy).await?;
        Ok(Self {
            dir: dir.to_owned(),
            channel: channel.to_owned(),
            ext: ext.to_owned(),
            policy,
            rotation,
            current_key: key,
            inner,
        })
    }

    async fn rotate_to(&mut self, key: String) -> Result<(), RecordError> {
        self.inner
            .finalize(RecordingStopReason::ChannelStopped)
            .await?;
        let path = self
            .dir
            .join(rotation_filename(&self.channel, &key, &self.ext));
        self.inner = DisplayFileRecorder::create(&path, self.policy).await?;
        self.current_key = key;
        Ok(())
    }
}

#[async_trait::async_trait]
impl DisplayRecorder for RotatingDisplayRecorder {
    async fn write_rendered(&mut self, output: &RenderedOutput) -> Result<(), RecordError> {
        // A rendered chunk carries its own timestamp; fall back to now() if the
        // view emits none, so rotation still advances.
        let at = output
            .timestamp
            .map(|t| t.wall_clock)
            .unwrap_or_else(SystemTime::now);
        if let Some(key) = period_key(self.rotation, at) {
            if key != self.current_key {
                self.rotate_to(key).await?;
            }
        }
        self.inner.write_rendered(output).await
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.inner.flush().await
    }

    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        self.inner.finalize(reason).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime};
    use crate::transport::ReceivedPayload;
    use chrono::TimeZone;
    use std::time::Instant;

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

    fn chunk_at(bytes: &[u8], at: SystemTime) -> ReceivedData {
        ReceivedData {
            channel_id: ChannelId::new(),
            payload: ReceivedPayload::Bytes(bytes.to_vec()),
            received_at: ChunkTime {
                monotonic: Instant::now(),
                wall_clock: at,
                wall_clock_source: crate::core::ArrivalTimestampSource::PostRead,
            },
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("listener-rot-{tag}-{}", uuid::Uuid::new_v4()));
        p
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

    #[tokio::test]
    async fn rotating_to_a_path_that_is_an_existing_file_gives_a_clear_error() {
        // §59: with rotation the destination is a folder. If the path is an existing
        // *file*, the user gets a clear RotationDestinationIsFile error, not the opaque
        // OS "cannot create a file when that file already exists" from create_dir_all.
        let mut path = std::env::temp_dir();
        path.push(format!("listener-rot-file-{}.raw", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, b"i am a file, not a folder")
            .await
            .unwrap();

        let result = RotatingRawRecorder::create(
            &path,
            "GPS",
            ".raw",
            OverwritePolicy::AppendIfExists,
            false,
            FileRotationPolicy::Hourly,
        )
        .await;
        assert!(
            matches!(result, Err(RecordError::RotationDestinationIsFile(_))),
            "an existing file as a rotation destination must give the clear error"
        );
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn raw_rotates_on_the_hour_with_correct_names_and_contiguous_files() {
        let dir = temp_dir("raw");
        let mut rec = RotatingRawRecorder::create(
            &dir,
            "GPS",
            ".dat",
            OverwritePolicy::Overwrite,
            false,
            FileRotationPolicy::Hourly,
        )
        .await
        .unwrap();

        // Two chunks in the 08:00 hour, one in 09:00 → two files, no gap, no backfill.
        rec.write_chunk(&chunk_at(b"A", local(2026, 6, 3, 8, 30)))
            .await
            .unwrap();
        rec.write_chunk(&chunk_at(b"B", local(2026, 6, 3, 8, 45)))
            .await
            .unwrap();
        rec.write_chunk(&chunk_at(b"C", local(2026, 6, 3, 9, 5)))
            .await
            .unwrap();
        rec.finalize(RecordingStopReason::ChannelStopped)
            .await
            .unwrap();
        drop(rec); // close the final file

        let f08 = dir.join("GPS_2026-06-03_08.dat");
        let f09 = dir.join("GPS_2026-06-03_09.dat");
        assert_eq!(tokio::fs::read(&f08).await.unwrap(), b"AB");
        assert_eq!(tokio::fs::read(&f09).await.unwrap(), b"C");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Each period's sidecar indexes its own file, and a restart inside a period
    /// continues that file's offsets rather than restarting them.
    ///
    /// This is the default configuration, not a corner: rotation defaults to
    /// hourly and coerces Refuse to Append (§59), so stopping and starting a
    /// recording within the hour reopens the current period file. Counting from
    /// zero there indexed the second run's chunks onto the first run's bytes.
    #[tokio::test]
    async fn rotated_sidecars_index_their_own_period_file_across_a_restart() {
        let dir = temp_dir("raw-idx");
        let offsets = |text: String| -> Vec<String> {
            text.lines()
                .map(|line| line.split(',').next().unwrap().to_owned())
                .collect()
        };

        let mut rec = RotatingRawRecorder::create(
            &dir,
            "GPS",
            ".raw",
            OverwritePolicy::AppendIfExists,
            true,
            FileRotationPolicy::Hourly,
        )
        .await
        .unwrap();
        rec.write_chunk(&chunk_at(b"AB", local(2026, 6, 3, 8, 30)))
            .await
            .unwrap();
        rec.write_chunk(&chunk_at(b"CDE", local(2026, 6, 3, 8, 45)))
            .await
            .unwrap();
        // Crossing the hour opens a fresh file, so its index starts over — that
        // restart of offsets is correct, and is what makes the other one wrong.
        rec.write_chunk(&chunk_at(b"FG", local(2026, 6, 3, 9, 5)))
            .await
            .unwrap();
        rec.finalize(RecordingStopReason::ChannelStopped)
            .await
            .unwrap();
        drop(rec);

        // Stop and start again inside the 09:00 hour: the same file is reopened.
        let mut rec = RotatingRawRecorder::create(
            &dir,
            "GPS",
            ".raw",
            OverwritePolicy::AppendIfExists,
            true,
            FileRotationPolicy::Hourly,
        )
        .await
        .unwrap();
        rec.write_chunk(&chunk_at(b"HIJ", local(2026, 6, 3, 9, 20)))
            .await
            .unwrap();
        rec.finalize(RecordingStopReason::ChannelStopped)
            .await
            .unwrap();
        drop(rec);

        let f08 = dir.join("GPS_2026-06-03_08.raw");
        let f09 = dir.join("GPS_2026-06-03_09.raw");
        assert_eq!(tokio::fs::read(&f08).await.unwrap(), b"ABCDE");
        assert_eq!(tokio::fs::read(&f09).await.unwrap(), b"FGHIJ");
        assert_eq!(
            offsets(
                tokio::fs::read_to_string(f08.with_extension("raw.idx"))
                    .await
                    .unwrap()
            ),
            vec!["0", "2"],
        );
        assert_eq!(
            offsets(
                tokio::fs::read_to_string(f09.with_extension("raw.idx"))
                    .await
                    .unwrap()
            ),
            vec!["0", "2"],
            "the restart's chunk is at offset 2 of its own file, not 0"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn display_rotates_on_the_day_with_correct_names() {
        let dir = temp_dir("disp");
        let mut rec = RotatingDisplayRecorder::create(
            &dir,
            "AIS",
            ".disp",
            OverwritePolicy::Overwrite,
            FileRotationPolicy::Daily,
        )
        .await
        .unwrap();

        let render = |text: &str, at: SystemTime| RenderedOutput {
            channel_id: ChannelId::new(),
            text: text.to_string(),
            timestamp: Some(ChunkTime {
                monotonic: Instant::now(),
                wall_clock: at,
                wall_clock_source: crate::core::ArrivalTimestampSource::PostRead,
            }),
        };
        rec.write_rendered(&render("day1", local(2026, 6, 3, 23, 50)))
            .await
            .unwrap();
        rec.write_rendered(&render("day2", local(2026, 6, 4, 0, 10)))
            .await
            .unwrap();
        rec.finalize(RecordingStopReason::ChannelStopped)
            .await
            .unwrap();
        drop(rec);

        assert_eq!(
            tokio::fs::read_to_string(dir.join("AIS_2026-06-03.disp"))
                .await
                .unwrap(),
            "day1"
        );
        assert_eq!(
            tokio::fs::read_to_string(dir.join("AIS_2026-06-04.disp"))
                .await
                .unwrap(),
            "day2"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
