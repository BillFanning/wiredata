//! Where a recording's files go and what they are called (listener ADR-043,
//! spec §59): the file-backed [`SegmentSource`]s.
//!
//! A recording is a series of **segments**. The first file of an enable follows
//! the overwrite policy (§55). Every later file — a new rotation period, the
//! size cap, recovery after a gap — is a new numbered segment, always created
//! new: `GPS_2026-06-03_08.raw`, then `GPS_2026-06-03_08_2.raw`, `_3`; a
//! single-file destination `run.raw` continues as `run_2.raw`. Numbering restarts
//! each period, and numbers are allocated under one lock the recording holds for
//! its whole life (§121), by scanning what exists.
//!
//! The first successful open writes a `.wiredata-destination` marker in the
//! recording folder. Recovery never creates that folder and resumes only when
//! it exists and holds the marker: on Linux an unplugged drive can leave its
//! empty mount point on the system disk, and recording must not resume there.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use super::controller::{OpenError, OpenKind, OpenRequest, SegmentSource};
use super::file::{lock_file, DisplayFileRecorder, RawFileRecorder};
use super::file_rotation::{ensure_rotation_dir, period_key, rotation_filename};
use super::{FileRotationPolicy, OverwritePolicy, RecorderWriter};
use crate::core::{GapReason, RecordError};
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

/// The marker a recording folder carries once a recording has started in it.
pub const DESTINATION_MARKER: &str = ".wiredata-destination";

const MARKER_TEXT: &str = "This folder holds wiredata recordings. Listener resumes a \
recording here after a fault only while this file is present, so it never writes \
to a different disk that happens to appear at the same path.\n";

/// Where a recording's segments go and how they are named (§59).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentPlan {
    /// A file when `rotation` is `None`; otherwise the folder of period files.
    pub destination: PathBuf,
    /// The Channel name, which names rotated files.
    pub channel: String,
    /// The extension of rotated files: `.raw` or `.disp`.
    pub ext: String,
    pub rotation: FileRotationPolicy,
    pub overwrite: OverwritePolicy,
    /// The soft size cap per segment, in bytes (§59).
    pub size_cap: Option<u64>,
}

impl SegmentPlan {
    /// The folder the segments are written in.
    pub fn folder(&self) -> PathBuf {
        match self.rotation {
            FileRotationPolicy::None => self
                .destination
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
            FileRotationPolicy::Hourly | FileRotationPolicy::Daily => self.destination.clone(),
        }
    }

    /// The one lock the recording holds for its whole life (§121).
    fn lock_path(&self) -> PathBuf {
        match self.rotation {
            FileRotationPolicy::None => {
                let mut name = self.destination.as_os_str().to_owned();
                name.push(".lock");
                PathBuf::from(name)
            }
            FileRotationPolicy::Hourly | FileRotationPolicy::Daily => self
                .destination
                .join(format!("{}{}.lock", self.channel, self.ext)),
        }
    }

    /// The period an item arriving at `at` belongs to.
    fn period_of(&self, at: SystemTime) -> Option<String> {
        period_key(self.rotation, at)
    }

    /// Segment 1's path: the destination itself, or the period's base file.
    fn base(&self, period: Option<&str>) -> PathBuf {
        match (self.rotation, period) {
            (FileRotationPolicy::None, _) | (_, None) => self.destination.clone(),
            (_, Some(period)) => {
                self.destination
                    .join(rotation_filename(&self.channel, period, &self.ext))
            }
        }
    }
}

/// Segment `n`'s path: `n == 1` is the base itself; later segments add `_n`
/// before the extension.
fn numbered(base: &Path, n: u32) -> PathBuf {
    if n <= 1 {
        return base.to_path_buf();
    }
    let (stem, ext) = split_name(base);
    base.with_file_name(format!("{stem}_{n}{ext}"))
}

/// A base file's name as (stem, extension-with-dot).
fn split_name(base: &Path) -> (String, String) {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = base
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (stem, ext)
}

/// The segment numbers that exist for `base`: 1 for the base itself, `n` for
/// each `<stem>_<n><ext>`.
fn existing_numbers(base: &Path) -> std::io::Result<Vec<u32>> {
    let (stem, ext) = split_name(base);
    let folder = base.parent().unwrap_or_else(|| Path::new("."));
    let base_name = base.file_name().map(|n| n.to_string_lossy().into_owned());
    let prefix = format!("{stem}_");
    let mut numbers = Vec::new();
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(numbers),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if Some(&name) == base_name.as_ref() {
            numbers.push(1);
        } else if let Some(n) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(ext.as_str()))
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|n| *n >= 2)
        {
            numbers.push(n);
        }
    }
    Ok(numbers)
}

/// What every file source shares: the plan, the recording's lock, and the
/// choice of the next file.
struct Planner {
    plan: SegmentPlan,
    lock: Option<std::fs::File>,
}

impl Planner {
    /// Check the destination, take the lock, write the marker, and choose the
    /// next file and how to open it. Blocking — run it off the async runtime.
    fn prepare(&mut self, request: &OpenRequest) -> Result<(PathBuf, OverwritePolicy), OpenError> {
        let folder = self.plan.folder();
        let first = request.kind == OpenKind::First;
        let retry = |reason, error| OpenError::Retry { reason, error };
        let missing = || {
            retry(
                GapReason::DestinationMissing,
                RecordError::DestinationMissing(folder.display().to_string()),
            )
        };
        if first {
            if self.plan.rotation == FileRotationPolicy::None {
                if !folder.is_dir() {
                    return Err(missing());
                }
            } else {
                ensure_rotation_dir(&folder).map_err(|error| match error {
                    RecordError::RotationDestinationIsFile(_) => OpenError::Terminal(error),
                    other => retry(GapReason::DestinationMissing, other),
                })?;
            }
        } else if !folder.is_dir() || !folder.join(DESTINATION_MARKER).is_file() {
            return Err(missing());
        }
        if self.lock.is_none() {
            let lock = lock_file(&self.plan.lock_path()).map_err(|error| match error {
                RecordError::DestinationInUse if first => OpenError::Terminal(error),
                other => retry(GapReason::OpenFailed, other),
            })?;
            self.lock = Some(lock);
        }
        let marker = folder.join(DESTINATION_MARKER);
        if !marker.is_file() {
            std::fs::write(&marker, MARKER_TEXT)
                .map_err(|error| retry(GapReason::OpenFailed, error.into()))?;
        }
        let base = self.plan.base(request.period.as_deref());
        let numbers =
            existing_numbers(&base).map_err(|error| retry(GapReason::OpenFailed, error.into()))?;
        let highest = numbers.iter().copied().max();
        let next = |n: Option<u32>| match n {
            None => (base.clone(), OverwritePolicy::Refuse),
            Some(n) => (numbered(&base, n + 1), OverwritePolicy::Refuse),
        };
        Ok(match (request.kind, self.plan.overwrite) {
            (OpenKind::First, OverwritePolicy::Refuse) => (base.clone(), OverwritePolicy::Refuse),
            (OpenKind::First, OverwritePolicy::Overwrite) => {
                (base.clone(), OverwritePolicy::Overwrite)
            }
            (OpenKind::First, OverwritePolicy::AppendIfExists) => match highest {
                None => (base.clone(), OverwritePolicy::AppendIfExists),
                Some(n) => {
                    let current = numbered(&base, n);
                    let len = std::fs::metadata(&current).map(|m| m.len()).unwrap_or(0);
                    if self.plan.size_cap.is_none_or(|cap| len < cap) {
                        (current, OverwritePolicy::AppendIfExists)
                    } else {
                        next(Some(n))
                    }
                }
            },
            _ => next(highest),
        })
    }

    /// Classify a failed open of the chosen file.
    fn open_failed(kind: OpenKind, mode: OverwritePolicy, error: RecordError) -> OpenError {
        let refused =
            matches!(&error, RecordError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists);
        if kind == OpenKind::First && mode == OverwritePolicy::Refuse && refused {
            OpenError::Terminal(error)
        } else {
            OpenError::Retry {
                reason: GapReason::OpenFailed,
                error,
            }
        }
    }
}

/// Run [`Planner::prepare`] on the blocking pool: its checks are filesystem
/// calls, and a slow drive must not hold an async worker.
async fn prepare(
    planner: &mut Option<Planner>,
    request: &OpenRequest,
) -> Result<(PathBuf, OverwritePolicy), OpenError> {
    let mut taken = planner.take().expect("the planner is always returned");
    let request = request.clone();
    let (taken, result) = tokio::task::spawn_blocking(move || {
        let result = taken.prepare(&request);
        (taken, result)
    })
    .await
    .map_err(|join| OpenError::Retry {
        reason: GapReason::OpenFailed,
        error: RecordError::Io(std::io::Error::other(format!(
            "preparing the next file failed: {join}"
        ))),
    })?;
    *planner = Some(taken);
    result
}

/// Raw recording segments (`.raw`, with an optional `.raw.idx` sidecar, §57).
pub struct RawSegments {
    planner: Option<Planner>,
    size_cap: Option<u64>,
    timestamps: bool,
}

impl RawSegments {
    pub fn new(plan: SegmentPlan, timestamps: bool) -> Self {
        Self {
            size_cap: plan.size_cap,
            planner: Some(Planner { plan, lock: None }),
            timestamps,
        }
    }
}

#[async_trait::async_trait]
impl SegmentSource<Arc<ReceivedData>> for RawSegments {
    fn period_of(&self, at: SystemTime) -> Option<String> {
        self.planner.as_ref().and_then(|p| p.plan.period_of(at))
    }

    fn size_cap(&self) -> Option<u64> {
        self.size_cap
    }

    async fn open(
        &mut self,
        request: OpenRequest,
    ) -> Result<Box<dyn RecorderWriter<Arc<ReceivedData>>>, OpenError> {
        let (path, mode) = prepare(&mut self.planner, &request).await?;
        match RawFileRecorder::open(&path, mode, self.timestamps).await {
            Ok(recorder) => Ok(Box::new(recorder)),
            Err(error) => Err(Planner::open_failed(request.kind, mode, error)),
        }
    }
}

/// Display recording segments (`.disp`, §54).
pub struct DisplaySegments {
    planner: Option<Planner>,
    size_cap: Option<u64>,
}

impl DisplaySegments {
    pub fn new(plan: SegmentPlan) -> Self {
        Self {
            size_cap: plan.size_cap,
            planner: Some(Planner { plan, lock: None }),
        }
    }
}

#[async_trait::async_trait]
impl SegmentSource<RenderedOutput> for DisplaySegments {
    fn period_of(&self, at: SystemTime) -> Option<String> {
        self.planner.as_ref().and_then(|p| p.plan.period_of(at))
    }

    fn size_cap(&self) -> Option<u64> {
        self.size_cap
    }

    async fn open(
        &mut self,
        request: OpenRequest,
    ) -> Result<Box<dyn RecorderWriter<RenderedOutput>>, OpenError> {
        let (path, mode) = prepare(&mut self.planner, &request).await?;
        match DisplayFileRecorder::open(&path, mode).await {
            Ok(recorder) => Ok(Box::new(recorder)),
            Err(error) => Err(Planner::open_failed(request.kind, mode, error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime, RecordingState};
    use crate::record::controller::{start_recording, StreamPos, Timings};
    use crate::record::RecordingStopReason;
    use crate::transport::ReceivedPayload;
    use chrono::{Local, TimeZone};
    use std::time::{Duration, Instant};

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> SystemTime {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("unambiguous local time")
            .into()
    }

    fn chunk_at(bytes: &[u8], at: SystemTime) -> (Arc<ReceivedData>, StreamPos) {
        let item = Arc::new(ReceivedData {
            channel_id: ChannelId::new(),
            payload: ReceivedPayload::Bytes(bytes.to_vec()),
            received_at: ChunkTime {
                monotonic: Instant::now(),
                wall_clock: at,
                wall_clock_source: crate::core::ArrivalTimestampSource::PostRead,
            },
        });
        (item, StreamPos { offset: 0, at })
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("listener-seg-{tag}-{}", uuid::Uuid::new_v4()));
        p
    }

    fn plan(destination: &Path, rotation: FileRotationPolicy) -> SegmentPlan {
        SegmentPlan {
            destination: destination.to_path_buf(),
            channel: "GPS".to_owned(),
            ext: ".raw".to_owned(),
            rotation,
            overwrite: OverwritePolicy::AppendIfExists,
            size_cap: None,
        }
    }

    fn fast() -> Timings {
        Timings {
            first_retry: Duration::from_millis(10),
            max_retry: Duration::from_millis(40),
            flush: Duration::from_secs(3600),
            ..Timings::default()
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn numbered_segments_add_their_number_before_the_extension() {
        let base = Path::new("rec").join("GPS_2026-06-03_08.raw");
        assert_eq!(numbered(&base, 1), base);
        assert_eq!(
            numbered(&base, 3),
            Path::new("rec").join("GPS_2026-06-03_08_3.raw")
        );
        assert_eq!(
            numbered(Path::new("run.raw"), 2),
            PathBuf::from("run_2.raw")
        );
    }

    #[tokio::test]
    async fn hourly_rotation_writes_one_file_per_period_with_a_marker_and_one_lock() {
        let dir = temp_dir("hourly");
        let mut recording = start_recording(
            RawSegments::new(plan(&dir, FileRotationPolicy::Hourly), false),
            crate::record::DEFAULT_QUEUE_BUDGET,
            fast(),
        );
        // Periods after now: the first file is for the hour the recording
        // started in, and rotation only moves forward from there.
        for (bytes, at) in [
            (b"A".as_slice(), local(2099, 6, 3, 8, 30)),
            (b"B", local(2099, 6, 3, 8, 45)),
            (b"C", local(2099, 6, 3, 9, 5)),
        ] {
            let (item, pos) = chunk_at(bytes, at);
            recording.try_record(item, pos);
        }
        assert!(recording
            .finalize(RecordingStopReason::Disabled)
            .await
            .fault
            .is_none());

        assert_eq!(
            std::fs::read(dir.join("GPS_2099-06-03_08.raw")).unwrap(),
            b"AB"
        );
        assert_eq!(
            std::fs::read(dir.join("GPS_2099-06-03_09.raw")).unwrap(),
            b"C"
        );
        let names = names(&dir);
        assert!(names.contains(&DESTINATION_MARKER.to_owned()), "{names:?}");
        assert!(names.contains(&"GPS.raw.lock".to_owned()), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n.ends_with(".raw.lock") && n != "GPS.raw.lock"),
            "one lock per recording, not per file: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_clock_stepped_back_keeps_writing_the_current_file() {
        // §59: rotation only moves forward, so an earlier period never reopens
        // (and, under Overwrite, never truncates) a file already written.
        let dir = temp_dir("backwards");
        let mut recording = start_recording(
            RawSegments::new(plan(&dir, FileRotationPolicy::Hourly), false),
            crate::record::DEFAULT_QUEUE_BUDGET,
            fast(),
        );
        let future = |h| local(2099, 1, 1, h, 0);
        for (bytes, at) in [(b"A".as_slice(), future(10)), (b"B", future(9))] {
            let (item, pos) = chunk_at(bytes, at);
            recording.try_record(item, pos);
        }
        assert!(recording
            .finalize(RecordingStopReason::Disabled)
            .await
            .fault
            .is_none());
        assert_eq!(
            std::fs::read(dir.join("GPS_2099-01-01_10.raw")).unwrap(),
            b"AB"
        );
        assert!(!dir.join("GPS_2099-01-01_09.raw").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_size_cap_numbers_segments_and_derives_each_index_from_its_raw() {
        let dir = temp_dir("cap");
        std::fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("run.raw");
        let mut p = plan(&destination, FileRotationPolicy::None);
        p.size_cap = Some(4);
        let mut recording = start_recording(
            RawSegments::new(p, true),
            crate::record::DEFAULT_QUEUE_BUDGET,
            fast(),
        );
        for bytes in [b"AB".as_slice(), b"CD", b"EF"] {
            let (item, pos) = chunk_at(bytes, SystemTime::now());
            recording.try_record(item, pos);
        }
        assert!(recording
            .finalize(RecordingStopReason::Disabled)
            .await
            .fault
            .is_none());
        assert_eq!(std::fs::read(dir.join("run.raw")).unwrap(), b"ABCD");
        assert_eq!(std::fs::read(dir.join("run_2.raw")).unwrap(), b"EF");
        assert!(dir.join("run.raw.idx").exists());
        assert!(dir.join("run_2.raw.idx").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rotated_sidecars_index_their_own_period_file_across_a_restart() {
        // ADR-039: a period file reopened by a restart continues its index from
        // the file's end, through the same path as a fresh one.
        let dir = temp_dir("sidecar-restart");
        let period = |at| period_key(FileRotationPolicy::Daily, at);
        let started = period(SystemTime::now());
        for bytes in [b"ABCDE".as_slice(), b"FG"] {
            let mut recording = start_recording(
                RawSegments::new(plan(&dir, FileRotationPolicy::Daily), true),
                crate::record::DEFAULT_QUEUE_BUDGET,
                fast(),
            );
            let (item, pos) = chunk_at(bytes, SystemTime::now());
            recording.try_record(item, pos);
            assert!(recording
                .finalize(RecordingStopReason::Disabled)
                .await
                .fault
                .is_none());
        }
        // Across local midnight the runs belong to two periods — not this case.
        if period(SystemTime::now()) == started {
            let raw = dir.join(format!("GPS_{}.raw", started.unwrap()));
            assert_eq!(std::fs::read(&raw).unwrap(), b"ABCDEFG");
            let index = std::fs::read_to_string(super::super::file::sidecar_path(&raw)).unwrap();
            let offsets: Vec<&str> = index
                .lines()
                .map(|line| line.split(',').next().unwrap())
                .collect();
            assert_eq!(offsets, ["0", "5"]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_restart_appends_to_the_highest_segment_under_the_cap() {
        let dir = temp_dir("restart");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.raw"), b"full").unwrap();
        std::fs::write(dir.join("run_2.raw"), b"X").unwrap();
        let destination = dir.join("run.raw");
        let mut p = plan(&destination, FileRotationPolicy::None);
        p.size_cap = Some(4);
        let mut recording = start_recording(
            RawSegments::new(p, false),
            crate::record::DEFAULT_QUEUE_BUDGET,
            fast(),
        );
        let (item, pos) = chunk_at(b"Y", SystemTime::now());
        recording.try_record(item, pos);
        assert!(recording
            .finalize(RecordingStopReason::Disabled)
            .await
            .fault
            .is_none());
        assert_eq!(std::fs::read(dir.join("run.raw")).unwrap(), b"full");
        assert_eq!(std::fs::read(dir.join("run_2.raw")).unwrap(), b"XY");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refuse_over_an_existing_file_cannot_begin() {
        let dir = temp_dir("refuse");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.raw"), b"precious").unwrap();
        let mut p = plan(&dir.join("run.raw"), FileRotationPolicy::None);
        p.overwrite = OverwritePolicy::Refuse;
        let recording = start_recording(
            RawSegments::new(p, false),
            crate::record::DEFAULT_QUEUE_BUDGET,
            fast(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while recording.state() != RecordingState::Faulted {
            assert!(Instant::now() < deadline, "never faulted");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(std::fs::read(dir.join("run.raw")).unwrap(), b"precious");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovery_never_recreates_a_vanished_folder_and_needs_its_marker() {
        // §59: after the first open, a missing folder — or one without the
        // marker, like a mount point left behind by an unplugged drive — is
        // waited for, never recreated.
        let dir = temp_dir("vanish");
        let mut planner = Planner {
            plan: plan(&dir, FileRotationPolicy::Daily),
            lock: None,
        };
        let recovery = OpenRequest {
            kind: OpenKind::Recovery,
            period: Some("2026-06-03".to_owned()),
        };
        assert!(matches!(
            planner.prepare(&recovery),
            Err(OpenError::Retry {
                reason: GapReason::DestinationMissing,
                ..
            })
        ));
        assert!(!dir.exists(), "recovery must not create the folder");

        std::fs::create_dir_all(&dir).unwrap(); // the mount point, empty
        assert!(matches!(
            planner.prepare(&recovery),
            Err(OpenError::Retry {
                reason: GapReason::DestinationMissing,
                ..
            })
        ));

        std::fs::write(dir.join(DESTINATION_MARKER), MARKER_TEXT).unwrap(); // the drive is back
        let (path, mode) = planner.prepare(&recovery).unwrap();
        assert_eq!(path, dir.join("GPS_2026-06-03.raw"));
        assert_eq!(
            mode,
            OverwritePolicy::Refuse,
            "segments are always created new"
        );
        drop(planner);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
