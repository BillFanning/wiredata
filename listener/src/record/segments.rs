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
use super::file::{lock_file, sidecar_path, DisplayFileRecorder, RawFileRecorder};
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

/// The folder a recording writes its files in: the destination's folder for a
/// single file, the destination itself when it rotates (§59). The destination
/// marker lives here.
pub fn recording_folder(destination: &Path, rotation: FileRotationPolicy) -> PathBuf {
    match rotation {
        FileRotationPolicy::None => destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        FileRotationPolicy::Hourly | FileRotationPolicy::Daily => destination.to_path_buf(),
    }
}

impl SegmentPlan {
    /// The folder the segments are written in.
    pub fn folder(&self) -> PathBuf {
        recording_folder(&self.destination, self.rotation)
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

    /// The total size of this recording's files — its segments and their
    /// indexes — for status (§56.2). Other channels' and other taps' files in the
    /// same folder are not counted. Blocking — run it off the async runtime.
    pub fn files_size(&self) -> std::io::Result<u64> {
        self.scan_size(None)
    }

    /// [`files_size`](Self::files_size) without the segment at `current` and
    /// its index — the files that have stopped growing once `current` is the
    /// segment being written. Add [`segment_size`](Self::segment_size) for the
    /// rest.
    pub fn files_size_except(&self, current: &Path) -> std::io::Result<u64> {
        self.scan_size(Some(current))
    }

    /// The size of the segment at `path` and its index, by two stats — not a
    /// scan of the folder. A missing file counts as nothing.
    pub fn segment_size(&self, path: &Path) -> u64 {
        let len = |p: &Path| std::fs::metadata(p).map_or(0, |meta| meta.len());
        len(path) + len(&sidecar_path(path))
    }

    fn scan_size(&self, except: Option<&Path>) -> std::io::Result<u64> {
        let except = except.and_then(|path| path.file_name().map(|n| n.to_owned()));
        let mut total = 0;
        for entry in std::fs::read_dir(self.folder())?.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(except) = &except {
                let base = name_str.strip_suffix(".idx").unwrap_or(&name_str);
                if base == except.to_string_lossy() {
                    continue;
                }
            }
            if self.is_recording_file(&name_str) {
                // A file removed mid-scan is simply not counted.
                total += entry.metadata().map_or(0, |meta| meta.len());
            }
        }
        Ok(total)
    }

    /// Whether `name` is one of this recording's segments or their indexes.
    fn is_recording_file(&self, name: &str) -> bool {
        let name = name.strip_suffix(".idx").unwrap_or(name);
        match self.rotation {
            FileRotationPolicy::None => {
                let (stem, ext) = split_name(&self.destination);
                let Some(rest) = name.strip_suffix(ext.as_str()) else {
                    return false;
                };
                rest == stem
                    || rest
                        .strip_prefix(&format!("{stem}_"))
                        .and_then(|n| n.parse::<u32>().ok())
                        .is_some_and(|n| n >= 2)
            }
            FileRotationPolicy::Hourly | FileRotationPolicy::Daily => name
                .strip_suffix(self.ext.as_str())
                .and_then(|rest| rest.strip_prefix(&format!("{}_", self.channel)))
                .is_some_and(is_period_and_segment),
        }
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

/// A period key and optional segment number: `YYYY-MM-DD`, then at most two
/// `_<digits>` groups (an hour, a segment number). The date's fixed shape is what
/// keeps channel `GPS_2`'s files out of channel `GPS`'s.
fn is_period_and_segment(rest: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let mut parts = rest.split('_');
    let date: Vec<&str> = parts.next().unwrap_or_default().split('-').collect();
    let date_ok = date.len() == 3
        && date
            .iter()
            .zip([4, 2, 2])
            .all(|(part, len)| part.len() == len && digits(part));
    let tail: Vec<&str> = parts.collect();
    date_ok && tail.len() <= 2 && tail.iter().all(|part| digits(part))
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
///
/// The planner travels into the blocking task and back. A task that fails to
/// join does not give it back, so the next call builds another from `plan`;
/// the recording's lock went with the lost one and is taken again.
async fn prepare(
    planner: &mut Option<Planner>,
    plan: &SegmentPlan,
    request: &OpenRequest,
) -> Result<(PathBuf, OverwritePolicy), OpenError> {
    let mut taken = planner.take().unwrap_or_else(|| Planner {
        plan: plan.clone(),
        lock: None,
    });
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
    plan: SegmentPlan,
    /// `None` only while a blocking task has it, or after one lost it.
    planner: Option<Planner>,
    timestamps: bool,
}

impl RawSegments {
    pub fn new(plan: SegmentPlan, timestamps: bool) -> Self {
        Self {
            planner: Some(Planner {
                plan: plan.clone(),
                lock: None,
            }),
            plan,
            timestamps,
        }
    }
}

#[async_trait::async_trait]
impl SegmentSource<Arc<ReceivedData>> for RawSegments {
    fn period_of(&self, at: SystemTime) -> Option<String> {
        self.plan.period_of(at)
    }

    fn size_cap(&self) -> Option<u64> {
        self.plan.size_cap
    }

    async fn open(
        &mut self,
        request: OpenRequest,
    ) -> Result<Box<dyn RecorderWriter<Arc<ReceivedData>>>, OpenError> {
        let (path, mode) = prepare(&mut self.planner, &self.plan, &request).await?;
        match RawFileRecorder::open(&path, mode, self.timestamps).await {
            Ok(recorder) => Ok(Box::new(recorder)),
            Err(error) => Err(Planner::open_failed(request.kind, mode, error)),
        }
    }
}

/// Display recording segments (`.disp`, §54).
pub struct DisplaySegments {
    plan: SegmentPlan,
    /// `None` only while a blocking task has it, or after one lost it.
    planner: Option<Planner>,
    /// Whether each file carries `‹GAP …›` lines (see [`DisplayFileRecorder`]).
    mark_gaps: bool,
}

impl DisplaySegments {
    /// Mark gaps in the files this source opens; off by default.
    pub fn with_gap_marks(mut self, on: bool) -> Self {
        self.mark_gaps = on;
        self
    }

    pub fn new(plan: SegmentPlan) -> Self {
        Self {
            mark_gaps: false,
            planner: Some(Planner {
                plan: plan.clone(),
                lock: None,
            }),
            plan,
        }
    }
}

#[async_trait::async_trait]
impl SegmentSource<RenderedOutput> for DisplaySegments {
    fn period_of(&self, at: SystemTime) -> Option<String> {
        self.plan.period_of(at)
    }

    fn size_cap(&self) -> Option<u64> {
        self.plan.size_cap
    }

    async fn open(
        &mut self,
        request: OpenRequest,
    ) -> Result<Box<dyn RecorderWriter<RenderedOutput>>, OpenError> {
        let (path, mode) = prepare(&mut self.planner, &self.plan, &request).await?;
        match DisplayFileRecorder::open(&path, mode).await {
            Ok(recorder) => Ok(Box::new(recorder.with_gap_marks(self.mark_gaps))),
            Err(error) => Err(Planner::open_failed(request.kind, mode, error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime, RecordingState};
    use crate::record::controller::{start_recording, StreamPos, Timings};
    use std::time::{Duration, Instant};

    fn local(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> SystemTime {
        let zoned = jiff::civil::date(y, mo, d)
            .at(h, mi, 0, 0)
            .to_zoned_datetime(jiff::tz::TimeZone::system())
            .expect("a local time jiff can represent");
        SystemTime::try_from(zoned.timestamp()).expect("a time the clock can hold")
    }

    fn chunk_at(bytes: &[u8], at: SystemTime) -> (Arc<ReceivedData>, StreamPos) {
        let item = Arc::new(ReceivedData {
            channel_id: ChannelId::new(),
            payload: bytes.to_vec(),
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
            retry: crate::retry::Backoff {
                first: Duration::from_millis(10),
                max: Duration::from_millis(40),
            },
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

    #[tokio::test]
    async fn a_source_whose_planner_was_lost_still_opens_the_next_file() {
        // A blocking task that fails to join takes the planner with it. The
        // next open must build another, not panic the recorder task.
        let dir = temp_dir("lost-planner");
        std::fs::create_dir_all(&dir).unwrap();
        let mut source =
            RawSegments::new(plan(&dir.join("run.raw"), FileRotationPolicy::None), false);
        source.planner = None;
        let opened = source
            .open(OpenRequest {
                kind: OpenKind::First,
                period: None,
            })
            .await;
        assert!(opened.is_ok(), "{:?}", opened.err());
        assert!(dir.join("run.raw").is_file());
        drop(opened);
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn a_recording_s_size_counts_only_its_own_segments_and_indexes() {
        let dir = temp_dir("size");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, len) in [
            ("GPS_2026-06-03_08.raw", 5),
            ("GPS_2026-06-03_08.raw.idx", 3),
            ("GPS_2026-06-03_08_2.raw", 4),
            ("GPS_2_2026-06-03_08.raw", 100), // another channel
            ("GPS_2026-06-03_08.disp", 50),   // the other tap
            ("GPS.raw.lock", 7),
            (DESTINATION_MARKER, 9),
            ("notes.txt", 11),
        ] {
            std::fs::write(dir.join(name), vec![b'x'; len]).unwrap();
        }
        assert_eq!(
            plan(&dir, FileRotationPolicy::Hourly).files_size().unwrap(),
            12
        );

        let single = dir.join("run.raw");
        for (name, len) in [
            ("run.raw", 2),
            ("run_2.raw", 3),
            ("run_2.raw.idx", 1),
            ("run_x.raw", 13),
            ("runner.raw", 17),
        ] {
            std::fs::write(dir.join(name), vec![b'x'; len]).unwrap();
        }
        assert_eq!(
            plan(&single, FileRotationPolicy::None)
                .files_size()
                .unwrap(),
            6
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_size_without_a_segment_plus_that_segment_is_the_whole_size() {
        let dir = temp_dir("size-split");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, len) in [
            ("GPS_2026-06-03_08.raw", 5),
            ("GPS_2026-06-03_08.raw.idx", 3),
            ("GPS_2026-06-03_08_2.raw", 4),
            ("GPS_2026-06-03_08_2.raw.idx", 2),
            ("GPS_2026-06-03_08.disp", 50),
        ] {
            std::fs::write(dir.join(name), vec![b'x'; len]).unwrap();
        }
        let plan = plan(&dir, FileRotationPolicy::Hourly);
        let current = dir.join("GPS_2026-06-03_08_2.raw");
        assert_eq!(plan.files_size_except(&current).unwrap(), 8);
        assert_eq!(plan.segment_size(&current), 6);
        assert_eq!(plan.files_size().unwrap(), 14);
        assert_eq!(plan.segment_size(&dir.join("missing.raw")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "benchmark: prints the cost of one size scan over 20,000 files"]
    fn files_size_over_twenty_thousand_files() {
        let dir = temp_dir("size-bench");
        std::fs::create_dir_all(&dir).unwrap();
        for n in 2..20_002 {
            std::fs::File::create(dir.join(format!("GPS_2026-06-03_08_{n}.raw"))).unwrap();
        }
        let plan = plan(&dir, FileRotationPolicy::Hourly);
        let started = std::time::Instant::now();
        let size = plan.files_size().unwrap();
        println!("files_size over 20000 files: {:?}", started.elapsed());
        assert_eq!(size, 0);
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(recording.finalize().await.fault.is_none());

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
        assert!(recording.finalize().await.fault.is_none());
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
        assert!(recording.finalize().await.fault.is_none());
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
            assert!(recording.finalize().await.fault.is_none());
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
        assert!(recording.finalize().await.fault.is_none());
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

    // ---- gap marks in a Display recording (§56.1) ----

    use crate::core::GapReason;
    use crate::record::controller::{OpenKind, RecordItem, RecorderReport, PER_ITEM_OVERHEAD};

    fn at(second: u32) -> StreamPos {
        StreamPos {
            offset: u64::from(second),
            at: local(2026, 11, 13, 14, 2) + Duration::from_secs(u64::from(second)),
        }
    }

    fn rendered(text: &str) -> RenderedOutput {
        RenderedOutput {
            channel_id: ChannelId::new(),
            text: text.to_owned(),
            timestamp: None,
        }
    }

    fn raw_item(text: &str) -> Arc<ReceivedData> {
        chunk_at(text.as_bytes(), SystemTime::now()).0
    }

    /// Offer "AB", overflow the queue so "EF" is lost, wait for recording to
    /// resume, offer "GH" at second 12, and stop. Returns the reports and the contents of the files
    /// the recording wrote, in order.
    async fn record_across_a_gap<I: RecordItem, S: SegmentSource<I>>(
        source: S,
        make: impl Fn(&str) -> I,
        dir: &Path,
        ext: &str,
    ) -> (Vec<RecorderReport>, Vec<Vec<u8>>) {
        let budget = 2 * (2 + PER_ITEM_OVERHEAD); // room for two 2-byte items
        let mut recording = start_recording(source, budget, fast());
        recording.try_record(make("AB"), at(0));
        // A queue overflow: "EF" is offered at second 4 and lost.
        recording.try_record(make("CD"), at(2));
        recording.try_record(make("EF"), at(4)); // does not fit
        let mut reports = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reports.iter().any(|r| {
            matches!(
                r,
                RecorderReport::SegmentOpened {
                    kind: OpenKind::Recovery,
                    ..
                }
            )
        }) {
            assert!(Instant::now() < deadline, "recording never resumed");
            reports.extend(recording.take_reports());
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        recording.try_record(make("GH"), at(12));
        let finalized = recording.finalize().await;
        reports.extend(finalized.reports);
        let files = names(dir)
            .into_iter()
            .filter(|name| name.ends_with(ext))
            .map(|name| std::fs::read(dir.join(name)).unwrap())
            .collect();
        (reports, files)
    }

    fn display_source(dir: &Path, mark_gaps: bool) -> DisplaySegments {
        let mut plan = plan(&dir.join("run.disp"), FileRotationPolicy::None);
        plan.ext = ".disp".to_owned();
        DisplaySegments::new(plan).with_gap_marks(mark_gaps)
    }

    #[tokio::test]
    async fn a_display_recording_resumes_with_a_gap_line_before_the_first_item() {
        let dir = temp_dir("gap-mark");
        std::fs::create_dir_all(&dir).unwrap();
        let (reports, files) =
            record_across_a_gap(display_source(&dir, true), rendered, &dir, ".disp").await;
        assert!(reports
            .iter()
            .any(|r| matches!(r, RecorderReport::GapClosed { start: Some(_), .. })));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], b"ABCD", "what was recorded before the gap");
        assert_eq!(
            String::from_utf8(files[1].clone()).unwrap(),
            format!(
                "\u{2039}GAP the recording queue overflowed \u{b7} {} to {} \u{b7} not \
                 recorded\u{203a}\nGH",
                crate::record::local_time(at(4).at),
                crate::record::local_time(at(12).at)
            ),
            "the line goes first in the new file, then the item recording resumed with"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_display_recording_with_marks_off_resumes_with_just_the_item() {
        let dir = temp_dir("gap-mark-off");
        std::fs::create_dir_all(&dir).unwrap();
        let (_, files) =
            record_across_a_gap(display_source(&dir, false), rendered, &dir, ".disp").await;
        assert_eq!(files, vec![b"ABCD".to_vec(), b"GH".to_vec()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_raw_recording_never_gets_a_gap_line() {
        let dir = temp_dir("gap-mark-raw");
        std::fs::create_dir_all(&dir).unwrap();
        let source = RawSegments::new(plan(&dir.join("run.raw"), FileRotationPolicy::None), false);
        let (reports, files) = record_across_a_gap(source, raw_item, &dir, ".raw").await;
        assert!(reports
            .iter()
            .any(|r| matches!(r, RecorderReport::GapClosed { start: Some(_), .. })));
        assert_eq!(
            files,
            vec![b"ABCD".to_vec(), b"GH".to_vec()],
            "a .raw file is exactly the bytes received"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
