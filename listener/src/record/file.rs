//! File-backed implementations of the two recorders (spec §53, §54, §55, §57).
//!
//! Both use async file I/O (`tokio::fs` + buffered `tokio::io`) so the recorder
//! task never blocks a runtime worker (§142). [`open_recording_file`] enforces
//! the [`OverwritePolicy`] atomically at enable time
//! (§55, §121): `Refuse` uses `create_new`, so an existing file is never
//! clobbered.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom};

use crate::core::RecordError;
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

use super::{DisplayRecorder, OverwritePolicy, RawRecorder, RecordingStopReason};

/// Open a recording destination, enforcing the overwrite policy (§55, §121).
///
/// `Refuse` opens with `create_new`, which fails atomically with
/// `AlreadyExists` if the file is present — so enabling fails and no existing
/// file is touched. `Overwrite` truncates; `AppendIfExists` appends.
pub async fn open_recording_file(
    path: &Path,
    policy: OverwritePolicy,
) -> Result<File, RecordError> {
    let mut opts = OpenOptions::new();
    opts.write(true);
    match policy {
        OverwritePolicy::Refuse => {
            opts.create_new(true);
        }
        OverwritePolicy::Overwrite => {
            opts.create(true).truncate(true);
        }
        OverwritePolicy::AppendIfExists => {
            opts.create(true).append(true);
        }
    }
    Ok(opts.open(path).await?)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Take a cross-platform advisory exclusive lock guarding a recording destination
/// (§121, ADR-014), so two recordings can never write the same file — whether two
/// channels here or a second `listener` process.
///
/// The lock is taken on a companion `<path>.lock` file, **not** the data file itself:
/// that keeps locking independent of the destination's overwrite policy (Refuse must
/// still fail atomically via the data open; Overwrite/Append must not be clobbered by
/// the lock handle), and it is taken *before* the data file is opened, so a lock
/// conflict never touches the destination. The returned **synchronous**
/// [`std::fs::File`] is the lock holder: a `std::fs::File` closes *deterministically*
/// on drop, releasing the lock the instant a recorder is dropped (so a Stop→Start can
/// immediately re-lock). A `tokio::fs::File` is unsuitable — it closes the OS handle
/// asynchronously, so its lock would linger past drop. Returns
/// [`RecordError::DestinationInUse`] if the destination is already locked.
fn lock_recording_destination(path: &Path) -> Result<std::fs::File, RecordError> {
    lock_file(&lock_path(path))
}

/// Take the advisory exclusive lock on `lock` itself (see
/// [`lock_recording_destination`]). A recording made of segments holds one such
/// lock for its whole life, so its numbered files are allocated under it (§59).
///
/// The lock file is never deleted, so it stays beside the recording after
/// the lock is released. Deleting it on release would let a second recorder
/// create and lock a fresh file while a third still held the old one (on
/// Unix a deleted file can stay locked), so two would write at once.
pub(crate) fn lock_file(lock: &Path) -> Result<std::fs::File, RecordError> {
    // `std::fs::File::try_lock` (stable since Rust 1.89) — no fs4 needed
    // for the lock; fs4 stays for the disk-space free functions (§168).
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(RecordError::DestinationInUse),
        Err(std::fs::TryLockError::Error(e)) => Err(RecordError::Io(e)),
    }
}

pub(crate) fn sidecar_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".idx");
    PathBuf::from(name)
}

/// What reopening an index to append found and removed (§57).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct IndexRepair {
    /// Bytes of the index to keep: its whole, increasing entries that point
    /// inside the `.raw`.
    pub keep: usize,
    /// Whole entries removed: past the end of the `.raw`, or not increasing.
    pub dropped_entries: usize,
    /// A half-written last line was removed.
    pub partial_line: bool,
    /// Bytes at the end of the `.raw` known to have no timestamp: a removed
    /// line showed where their block began.
    pub unindexed_tail: u64,
}

impl IndexRepair {
    /// What was repaired, in words; `None` when the index was whole.
    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        match self.dropped_entries {
            0 => {}
            1 => parts.push("removed 1 entry past the end of the .raw or out of order".to_owned()),
            n => parts.push(format!(
                "removed {n} entries past the end of the .raw or out of order"
            )),
        }
        if self.partial_line {
            parts.push("removed a half-written last line".to_owned());
        }
        if self.unindexed_tail > 0 {
            parts.push(format!(
                "{} bytes at the end have no timestamp",
                self.unindexed_tail
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Plan the repair of an index's lines against a `.raw` of `raw_len` bytes
/// (§57). The `.raw` is authoritative: the index keeps its leading run of whole
/// `<offset>,<nanos>` lines whose offsets increase and lie inside the `.raw`,
/// and everything from the first line that does not is removed.
pub(crate) fn plan_index_repair(index: &[u8], raw_len: u64) -> IndexRepair {
    let mut repair = IndexRepair::default();
    let mut last: Option<u64> = None;
    let mut pos = 0;
    let mut cut = false;
    while pos < index.len() {
        let rest = &index[pos..];
        let (line, complete) = match rest.iter().position(|&b| b == b'\n') {
            Some(end) => (&rest[..end], true),
            None => (rest, false),
        };
        let text = std::str::from_utf8(line).unwrap_or_default();
        // An offset counts only once its comma was written, so a line cut
        // inside its offset is not mistaken for a smaller one.
        let (offset, nanos) = match text.split_once(',') {
            Some((offset, nanos)) => (offset.parse::<u64>().ok(), nanos.parse::<u128>().ok()),
            None => (None, None),
        };
        let increases = offset.is_some_and(|o| o < raw_len && last.is_none_or(|l| o > l));
        if !cut && complete && nanos.is_some() && increases {
            last = offset;
            pos += line.len() + 1;
            repair.keep = pos;
            continue;
        }
        if !cut {
            cut = true;
            // The block this line timed began inside the `.raw` and lost its
            // timestamp, so the bytes from there on have none.
            if let Some(offset) = offset.filter(|_| increases) {
                repair.unindexed_tail = raw_len - offset;
            }
        }
        if complete {
            repair.dropped_entries += 1;
            pos += line.len() + 1;
        } else {
            repair.partial_line = true;
            pos = index.len();
        }
    }
    repair
}

/// How much of an index's end the repair reads first. Damage is only ever at
/// the end, so it reads backwards, quadrupling the window until it reaches a
/// whole entry it keeps — an index can be far larger than memory should hold.
const INDEX_REPAIR_WINDOW: u64 = 64 * 1024;

/// Repair the index at `index` before appending to it (§57), against the `.raw`
/// at `raw`. Returns what was repaired, in words.
async fn repair_index(
    index: &Path,
    raw: &Path,
    window: u64,
) -> Result<Option<String>, RecordError> {
    let mut file = match OpenOptions::new().read(true).write(true).open(index).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let raw_len = match tokio::fs::metadata(raw).await {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    let len = file.metadata().await?.len();
    let mut window = window.max(1).min(len);
    loop {
        let start = len - window;
        file.seek(SeekFrom::Start(start)).await?;
        let mut tail = vec![0; window as usize];
        file.read_exact(&mut tail).await?;
        // A window that starts mid-file may start mid-line: begin after the
        // first line break.
        let skip = if start == 0 {
            0
        } else {
            tail.iter()
                .position(|&b| b == b'\n')
                .map_or(tail.len(), |n| n + 1)
        };
        let repair = plan_index_repair(&tail[skip..], raw_len);
        if repair.keep > 0 || start == 0 {
            let keep = start + (skip + repair.keep) as u64;
            if keep < len {
                file.set_len(keep).await?;
            }
            return Ok(repair.describe());
        }
        window = window.saturating_mul(4).min(len);
    }
}

fn wall_clock_nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Raw Recording to a file: byte-exact, contiguous, with an optional timestamp
/// sidecar keyed by byte offset (§53, §57). The byte stream contains received
/// payload bytes only — timestamps never interleave it (§53).
pub struct RawFileRecorder {
    file: BufWriter<File>,
    /// Timestamp index: one `offset,wall_clock_nanos` line per chunk (§57).
    sidecar: Option<BufWriter<File>>,
    /// Absolute offset in the destination file where the next byte lands.
    ///
    /// Absolute, not "bytes this recorder wrote", because that is what the
    /// sidecar keys on: an index into a `.raw` has to count from the start of
    /// the file the bytes actually joined. Under `AppendIfExists` those differ,
    /// and they differ silently — the `.raw` stays perfectly correct while its
    /// index points into the wrong part of it.
    stream_offset: u64,
    /// Advisory lock on the destination (§121, ADR-014); held for the recorder's
    /// lifetime, released deterministically when this std handle drops. `None`
    /// for a segment, whose recording holds one lock for all its files.
    _lock: Option<std::fs::File>,
    path: PathBuf,
    /// What reopening the index repaired, until the controller reports it.
    open_note: Option<String>,
}

impl RawFileRecorder {
    /// Create the recording (and, if `timestamps`, its sidecar), failing per the
    /// overwrite policy if the destination exists (§55).
    pub async fn create(
        path: &Path,
        policy: OverwritePolicy,
        timestamps: bool,
    ) -> Result<Self, RecordError> {
        // Lock the main destination first (§121, ADR-014): if it is already in use, fail
        // before touching it. Only the main destination is locked — the `.idx` sidecar
        // is derived from it (`<path>.idx`) and shares the recording's lifetime, so two
        // recordings collide on the main path (caught here) before their sidecars could.
        // The one uncovered edge — a user pointing one channel's *main* destination at
        // another's sidecar path — is left unguarded as vanishingly unlikely.
        let lock = lock_recording_destination(path)?;
        let mut recorder = Self::open(path, policy, timestamps).await?;
        recorder._lock = Some(lock);
        Ok(recorder)
    }

    /// Open one segment of a recording whose lock is held elsewhere (§59).
    /// The overwrite policy applies as in [`create`](Self::create).
    pub async fn open(
        path: &Path,
        policy: OverwritePolicy,
        timestamps: bool,
    ) -> Result<Self, RecordError> {
        // Sidecar BEFORE the main destination: opening can create/truncate,
        // so the fallible pair must touch the derived artifact first — a
        // failed begin must never have modified the main recording (the
        // precious one). The inverse edge (main open fails after the sidecar
        // truncated) only costs the derived `.idx`.
        let mut open_note = None;
        let sidecar = if timestamps {
            let index = sidecar_path(path);
            // Appending continues an index an interrupted run may have left
            // damaged; repair it against the `.raw` first (§57).
            if policy == OverwritePolicy::AppendIfExists {
                open_note = repair_index(&index, path, INDEX_REPAIR_WINDOW).await?;
            }
            Some(BufWriter::new(open_recording_file(&index, policy).await?))
        } else {
            None
        };
        let file = open_recording_file(path, policy).await?;
        // Start counting from wherever this recording's first byte will land,
        // which under `AppendIfExists` (§55) is the end of what is already
        // there. Taken from the opened file rather than branched on the policy:
        // a truncated or freshly created destination reports zero, so one read
        // covers all three policies and cannot disagree with the open above.
        //
        // This matters more than it looks. Append is the default, rotation
        // coerces Refuse to Append (`effective_overwrite`, §59), and rotation
        // is also the default — so restarting a recording inside its current
        // period is the ordinary path, not a corner. Counting from zero there
        // left every new index entry pointing at bytes from the previous run.
        let stream_offset = file.metadata().await?.len();
        Ok(Self {
            file: BufWriter::new(file),
            sidecar,
            stream_offset,
            _lock: None,
            path: path.to_path_buf(),
            open_note,
        })
    }

    /// Absolute offset one past the last byte durably offered to the file — the
    /// sidecar's key for the next chunk, and the truncation point on fault
    /// (§56.1). Both want a position in the file, not a count for this run.
    pub fn stream_offset(&self) -> u64 {
        self.stream_offset
    }
}

#[async_trait::async_trait]
impl RawRecorder for RawFileRecorder {
    async fn write_chunk(&mut self, chunk: &ReceivedData) -> Result<(), RecordError> {
        let bytes = chunk.payload.bytes();
        let offset = self.stream_offset;
        // Bytes before their index line (§57): an interrupted write then leaves
        // bytes the index does not describe, never an index entry pointing past
        // the end of the `.raw`.
        self.file.write_all(bytes).await?;
        self.stream_offset += bytes.len() as u64;
        if let Some(sidecar) = &mut self.sidecar {
            // Key the timestamp by the offset of this chunk's first byte (§57).
            let line = format!(
                "{},{}\n",
                offset,
                wall_clock_nanos(chunk.received_at.wall_clock)
            );
            sidecar.write_all(line.as_bytes()).await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.file.flush().await?;
        if let Some(sidecar) = &mut self.sidecar {
            sidecar.flush().await?;
        }
        Ok(())
    }

    async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
        // Flush buffered data to disk before close (truncation point is recorded
        // by the runtime via RecordingFaulted, §56.1). The OS closes the file
        // when the recorder is dropped after the task ends.
        self.flush().await
    }

    fn written_len(&self) -> u64 {
        self.stream_offset
    }

    fn file_path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn take_open_note(&mut self) -> Option<String> {
        self.open_note.take()
    }
}

/// Display Recording to a file: appends a view's rendered text exactly as
/// produced (§54) — the pipeline's `StreamRenderer` guarantees the concatenation
/// is the exact rendered stream, so this recorder adds **nothing** (no per-chunk
/// newline; read boundaries leave no trace). Not byte-exact. Any inline Mark
/// timestamps are already spliced into `output.text`; `‹MARK …›` marker lines
/// arrive pre-framed with their own newlines.
pub struct DisplayFileRecorder {
    file: BufWriter<File>,
    /// Bytes in the file, for the size cap (§59).
    len: u64,
    /// Advisory lock on the destination (§121, ADR-014); see `RawFileRecorder._lock`.
    _lock: Option<std::fs::File>,
    path: PathBuf,
}

impl DisplayFileRecorder {
    pub async fn create(path: &Path, policy: OverwritePolicy) -> Result<Self, RecordError> {
        // Lock the destination first (§121, ADR-014) — same as Raw, so a `.disp` cannot
        // be shared by two recordings either.
        let lock = lock_recording_destination(path)?;
        let mut recorder = Self::open(path, policy).await?;
        recorder._lock = Some(lock);
        Ok(recorder)
    }

    /// Open one segment of a recording whose lock is held elsewhere (§59).
    pub async fn open(path: &Path, policy: OverwritePolicy) -> Result<Self, RecordError> {
        let file = open_recording_file(path, policy).await?;
        let len = file.metadata().await?.len();
        Ok(Self {
            file: BufWriter::new(file),
            len,
            _lock: None,
            path: path.to_path_buf(),
        })
    }
}

#[async_trait::async_trait]
impl DisplayRecorder for DisplayFileRecorder {
    async fn write_rendered(&mut self, output: &RenderedOutput) -> Result<(), RecordError> {
        self.file.write_all(output.text.as_bytes()).await?;
        self.len += output.text.len() as u64;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.file.flush().await?;
        Ok(())
    }

    async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
        self.flush().await
    }

    fn written_len(&self) -> u64 {
        self.len
    }

    fn file_path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime};
    use crate::record::{
        start_display_recording, start_raw_recording, StreamPos, DEFAULT_QUEUE_BUDGET,
    };
    use crate::transport::ReceivedPayload;
    use std::sync::Arc;

    fn temp_path(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("listener-rec-{tag}-{}.bin", uuid::Uuid::new_v4()));
        path
    }

    /// Where an item sits in the stream does not matter to these tests.
    fn pos() -> StreamPos {
        StreamPos {
            offset: 0,
            at: std::time::SystemTime::now(),
        }
    }

    fn chunk(bytes: &[u8]) -> Arc<ReceivedData> {
        Arc::new(ReceivedData {
            channel_id: ChannelId::new(),
            payload: ReceivedPayload::Bytes(bytes.to_vec()),
            received_at: ChunkTime::now(),
        })
    }

    #[tokio::test]
    async fn refuse_policy_does_not_clobber_an_existing_file() {
        let path = temp_path("refuse");
        tokio::fs::write(&path, b"original").await.unwrap();

        let result = RawFileRecorder::create(&path, OverwritePolicy::Refuse, false).await;
        assert!(result.is_err());
        // The existing file is untouched.
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"original");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn a_second_recorder_on_the_same_file_is_refused_while_the_first_is_open() {
        // §121 / ADR-014: an advisory lock stops two live recordings sharing one file.
        let path = temp_path("locked");
        let _first = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .expect("first recorder takes the lock");

        // While the first holds the file, a second open is refused as in-use.
        let second = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, false).await;
        assert!(
            matches!(second, Err(RecordError::DestinationInUse)),
            "a second recorder must be refused while the first is open"
        );

        // After the first is dropped (lock released), the destination is free again.
        drop(_first);
        let third = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, false).await;
        assert!(third.is_ok(), "the lock releases on drop");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn overwrite_policy_truncates_then_appends_policy_keeps() {
        // Overwrite replaces existing content.
        let path = temp_path("overwrite");
        tokio::fs::write(&path, b"stale-and-longer").await.unwrap();
        {
            let mut rec = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
                .await
                .unwrap();
            rec.write_chunk(&chunk(b"new")).await.unwrap();
            rec.finalize(RecordingStopReason::Disabled).await.unwrap();
        }
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"new");

        // AppendIfExists keeps existing content and adds to it.
        {
            let mut rec = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, false)
                .await
                .unwrap();
            rec.write_chunk(&chunk(b"-more")).await.unwrap();
            rec.finalize(RecordingStopReason::Disabled).await.unwrap();
        }
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"new-more");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn raw_recording_is_byte_exact_end_to_end() {
        let path = temp_path("byte-exact");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();
        let mut recording = start_raw_recording(recorder, DEFAULT_QUEUE_BUDGET);

        // Chunk boundaries must not appear in the byte stream (§53).
        recording.try_record(chunk(b"$GPGGA,"), pos());
        recording.try_record(chunk(b"123.4*7F\r\n"), pos());
        recording.try_record(chunk(b"\x00\x01\x02"), pos());
        let _ = recording.finalize(RecordingStopReason::Disabled).await;

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"$GPGGA,123.4*7F\r\n\x00\x01\x02");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn raw_timestamp_sidecar_keeps_the_byte_stream_pure() {
        let path = temp_path("sidecar");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, true)
            .await
            .unwrap();
        let mut recording = start_raw_recording(recorder, DEFAULT_QUEUE_BUDGET);
        recording.try_record(chunk(b"AB"), pos());
        recording.try_record(chunk(b"CDE"), pos());
        let _ = recording.finalize(RecordingStopReason::Disabled).await;

        // Byte stream holds only payload bytes — no timestamps interleaved (§53).
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"ABCDE");
        // Sidecar has one offset-keyed line per chunk (offsets 0 and 2).
        let sidecar = tokio::fs::read_to_string(&sidecar_path(&path))
            .await
            .unwrap();
        let offsets: Vec<&str> = sidecar
            .lines()
            .map(|l| l.split(',').next().unwrap())
            .collect();
        assert_eq!(offsets, vec!["0", "2"]);

        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_file(&sidecar_path(&path)).await;
    }

    /// An appended run indexes where its bytes actually landed (§55, §57).
    ///
    /// The failure this pins is silent and asymmetric: the `.raw` stays
    /// perfectly byte-exact while its index points into the previous run's
    /// bytes, so nothing looks wrong until someone trusts a timestamp. It is
    /// also the ordinary path — append is the default, and rotation (also the
    /// default) coerces Refuse to Append, so any restart inside the current
    /// period lands here.
    #[tokio::test]
    async fn an_appended_sidecar_indexes_from_the_end_of_the_existing_file() {
        let path = temp_path("sidecar-append");
        {
            let recorder = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, true)
                .await
                .unwrap();
            let mut recording = start_raw_recording(recorder, DEFAULT_QUEUE_BUDGET);
            recording.try_record(chunk(b"ABCDE"), pos());
            let _ = recording.finalize(RecordingStopReason::Disabled).await;
        }
        {
            let recorder = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, true)
                .await
                .unwrap();
            let mut recording = start_raw_recording(recorder, DEFAULT_QUEUE_BUDGET);
            recording.try_record(chunk(b"FG"), pos());
            recording.try_record(chunk(b"HIJ"), pos());
            let _ = recording.finalize(RecordingStopReason::Disabled).await;
        }

        let raw = tokio::fs::read(&path).await.unwrap();
        assert_eq!(raw, b"ABCDEFGHIJ", "the byte stream is still exact");
        let sidecar = tokio::fs::read_to_string(&sidecar_path(&path))
            .await
            .unwrap();
        let offsets: Vec<&str> = sidecar
            .lines()
            .map(|line| line.split(',').next().unwrap())
            .collect();
        assert_eq!(
            offsets,
            vec!["0", "5", "7"],
            "the second run's entries must continue from the existing length, not restart"
        );
        // Every offset names the first byte of the chunk it timed.
        for (offset, expected) in offsets.iter().zip(["A", "F", "H"]) {
            let at: usize = offset.parse().unwrap();
            assert_eq!(&raw[at..at + 1], expected.as_bytes(), "offset {offset}");
        }

        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_file(&sidecar_path(&path)).await;
    }

    /// A sidecar enabled on a later run indexes the file it is joining, and
    /// says nothing about the bytes recorded before it existed.
    #[tokio::test]
    async fn a_sidecar_added_to_an_existing_recording_starts_at_that_file_s_end() {
        let path = temp_path("sidecar-late");
        {
            let recorder = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
                .await
                .unwrap();
            let mut recording = start_raw_recording(recorder, DEFAULT_QUEUE_BUDGET);
            recording.try_record(chunk(b"early-bytes"), pos());
            let _ = recording.finalize(RecordingStopReason::Disabled).await;
        }
        assert!(
            tokio::fs::metadata(&sidecar_path(&path)).await.is_err(),
            "no sidecar while timestamps were off"
        );
        {
            let recorder = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, true)
                .await
                .unwrap();
            let mut recording = start_raw_recording(recorder, DEFAULT_QUEUE_BUDGET);
            recording.try_record(chunk(b"late"), pos());
            let _ = recording.finalize(RecordingStopReason::Disabled).await;
        }

        let sidecar = tokio::fs::read_to_string(&sidecar_path(&path))
            .await
            .unwrap();
        let offsets: Vec<&str> = sidecar
            .lines()
            .map(|line| line.split(',').next().unwrap())
            .collect();
        assert_eq!(
            offsets,
            vec!["11"],
            "the first timed chunk sits after the untimed bytes it followed"
        );

        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_file(&sidecar_path(&path)).await;
    }

    #[tokio::test]
    async fn display_recording_writes_rendered_lines() {
        let path = temp_path("display");
        let recorder = DisplayFileRecorder::create(&path, OverwritePolicy::Overwrite)
            .await
            .unwrap();
        let mut recording = start_display_recording(recorder, DEFAULT_QUEUE_BUDGET);
        let cid = ChannelId::new();
        recording.try_record(
            RenderedOutput {
                channel_id: cid,
                text: "first".to_string(),
                timestamp: None,
            },
            pos(),
        );
        recording.try_record(
            RenderedOutput {
                channel_id: cid,
                text: "second".to_string(),
                timestamp: None,
            },
            pos(),
        );
        let _ = recording.finalize(RecordingStopReason::Disabled).await;

        // The recorder appends verbatim — no injected separators (ADR-018): the
        // rendered stream's own text is the file, byte for byte.
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "firstsecond"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[test]
    fn a_whole_index_needs_no_repair() {
        let index = b"0,10\n3,20\n";
        let repair = plan_index_repair(index, 5);
        assert_eq!(
            repair,
            IndexRepair {
                keep: index.len(),
                ..IndexRepair::default()
            }
        );
        assert_eq!(repair.describe(), None);
    }

    #[test]
    fn index_entries_past_the_end_of_the_raw_are_removed() {
        // The index reached the disk ahead of the bytes it timed (§57).
        let repair = plan_index_repair(b"0,10\n3,20\n5,30\n9,40\n", 5);
        assert_eq!(repair.keep, 10);
        assert_eq!(repair.dropped_entries, 2);
        assert_eq!(
            repair.unindexed_tail, 0,
            "those blocks never reached the .raw"
        );
    }

    #[test]
    fn a_half_written_line_is_removed_and_its_block_has_no_timestamp() {
        let repair = plan_index_repair(b"0,10\n3,2", 8);
        assert_eq!(repair.keep, 5);
        assert!(repair.partial_line);
        assert_eq!(repair.unindexed_tail, 5, "the block began at offset 3 of 8");
        assert_eq!(
            repair.describe().as_deref(),
            Some("removed a half-written last line; 5 bytes at the end have no timestamp")
        );
        // Cut inside its offset, a line does not say where its block began.
        let cut = plan_index_repair(b"0,10\n3", 8);
        assert!(cut.partial_line);
        assert_eq!(cut.unindexed_tail, 0);
    }

    #[test]
    fn index_offsets_that_do_not_increase_are_removed_with_what_follows() {
        let repair = plan_index_repair(b"0,10\n3,20\n3,30\n4,40\n", 8);
        assert_eq!(repair.keep, 10);
        assert_eq!(repair.dropped_entries, 2);
    }

    #[tokio::test]
    async fn reopening_to_append_repairs_the_index_before_adding_to_it() {
        // §57: the .raw is authoritative, so the index is cut back to it and
        // the new entries continue from its end.
        let path = temp_path("repair");
        tokio::fs::write(&path, b"ABCDE").await.unwrap();
        tokio::fs::write(sidecar_path(&path), b"0,1\n3,2\n9,3\n12,4")
            .await
            .unwrap();
        let mut recorder = RawFileRecorder::open(&path, OverwritePolicy::AppendIfExists, true)
            .await
            .unwrap();
        assert_eq!(
            recorder.take_open_note().as_deref(),
            Some(
                "removed 1 entry past the end of the .raw or out of order; removed a \
                 half-written last line"
            )
        );
        assert_eq!(recorder.take_open_note(), None, "reported once");
        recorder.write_chunk(&chunk(b"FG")).await.unwrap();
        recorder
            .finalize(RecordingStopReason::Disabled)
            .await
            .unwrap();
        drop(recorder);

        let index = tokio::fs::read_to_string(sidecar_path(&path))
            .await
            .unwrap();
        let offsets: Vec<&str> = index
            .lines()
            .map(|line| line.split(',').next().unwrap())
            .collect();
        assert_eq!(offsets, ["0", "3", "5"]);
        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_file(sidecar_path(&path)).await;
    }

    #[tokio::test]
    async fn the_repair_reads_back_from_the_end_until_it_finds_an_entry_to_keep() {
        // A long run of damage, read through a tiny window, still cuts the index
        // back to its last good entry without reading it all at once.
        let raw = temp_path("repair-window");
        tokio::fs::write(&raw, b"ABCDE").await.unwrap();
        let index = sidecar_path(&raw);
        let mut damaged = b"0,1\n".to_vec();
        for _ in 0..50 {
            damaged.extend_from_slice(b"9,2\n");
        }
        damaged.push(b'1');
        tokio::fs::write(&index, &damaged).await.unwrap();

        let note = repair_index(&index, &raw, 8).await.unwrap();
        assert_eq!(tokio::fs::read(&index).await.unwrap(), b"0,1\n");
        assert_eq!(
            note.as_deref(),
            Some(
                "removed 50 entries past the end of the .raw or out of order; removed a \
                 half-written last line"
            )
        );
        let _ = tokio::fs::remove_file(&raw).await;
        let _ = tokio::fs::remove_file(&index).await;
    }
}
