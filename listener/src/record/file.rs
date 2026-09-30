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
use tokio::io::{AsyncWriteExt, BufWriter};

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
    // `std::fs::File::try_lock` (stable since Rust 1.89) — no fs4 needed
    // for the lock; fs4 stays for the disk-space free functions (§168).
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(path))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(RecordError::DestinationInUse),
        Err(std::fs::TryLockError::Error(e)) => Err(RecordError::Io(e)),
    }
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".idx");
    PathBuf::from(name)
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
    /// lifetime, released deterministically when this std handle drops.
    _lock: std::fs::File,
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
        // Sidecar BEFORE the main destination: opening can create/truncate,
        // so the fallible pair must touch the derived artifact first — a
        // failed begin must never have modified the main recording (the
        // precious one). The inverse edge (main open fails after the sidecar
        // truncated) only costs the derived `.idx`.
        let sidecar = if timestamps {
            Some(BufWriter::new(
                open_recording_file(&sidecar_path(path), policy).await?,
            ))
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
            _lock: lock,
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
        if let Some(sidecar) = &mut self.sidecar {
            // Key the timestamp by the offset of this chunk's first byte (§57).
            let line = format!(
                "{},{}\n",
                self.stream_offset,
                wall_clock_nanos(chunk.received_at.wall_clock)
            );
            sidecar.write_all(line.as_bytes()).await?;
        }
        self.file.write_all(bytes).await?;
        self.stream_offset += bytes.len() as u64;
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
}

/// Display Recording to a file: appends a view's rendered text exactly as
/// produced (§54) — the pipeline's `StreamRenderer` guarantees the concatenation
/// is the exact rendered stream, so this recorder adds **nothing** (no per-chunk
/// newline; read boundaries leave no trace). Not byte-exact. Any inline Mark
/// timestamps are already spliced into `output.text`; `‹MARK …›` marker lines
/// arrive pre-framed with their own newlines.
pub struct DisplayFileRecorder {
    file: BufWriter<File>,
    /// Advisory lock on the destination (§121, ADR-014); see `RawFileRecorder._lock`.
    _lock: std::fs::File,
}

impl DisplayFileRecorder {
    pub async fn create(path: &Path, policy: OverwritePolicy) -> Result<Self, RecordError> {
        // Lock the destination first (§121, ADR-014) — same as Raw, so a `.disp` cannot
        // be shared by two recordings either.
        let lock = lock_recording_destination(path)?;
        let file = BufWriter::new(open_recording_file(path, policy).await?);
        Ok(Self { file, _lock: lock })
    }
}

#[async_trait::async_trait]
impl DisplayRecorder for DisplayFileRecorder {
    async fn write_rendered(&mut self, output: &RenderedOutput) -> Result<(), RecordError> {
        self.file.write_all(output.text.as_bytes()).await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.file.flush().await?;
        Ok(())
    }

    async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
        self.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime};
    use crate::record::{start_display_recording, start_raw_recording};
    use crate::transport::ReceivedPayload;
    use std::sync::Arc;

    fn temp_path(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("listener-rec-{tag}-{}.bin", uuid::Uuid::new_v4()));
        path
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
        let mut recording = start_raw_recording(recorder, 16);

        // Chunk boundaries must not appear in the byte stream (§53).
        recording.try_record(chunk(b"$GPGGA,"));
        recording.try_record(chunk(b"123.4*7F\r\n"));
        recording.try_record(chunk(b"\x00\x01\x02"));
        recording.finalize(RecordingStopReason::Disabled).await;

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
        let mut recording = start_raw_recording(recorder, 16);
        recording.try_record(chunk(b"AB"));
        recording.try_record(chunk(b"CDE"));
        recording.finalize(RecordingStopReason::Disabled).await;

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
            let mut recording = start_raw_recording(recorder, 16);
            recording.try_record(chunk(b"ABCDE"));
            recording.finalize(RecordingStopReason::Disabled).await;
        }
        {
            let recorder = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, true)
                .await
                .unwrap();
            let mut recording = start_raw_recording(recorder, 16);
            recording.try_record(chunk(b"FG"));
            recording.try_record(chunk(b"HIJ"));
            recording.finalize(RecordingStopReason::Disabled).await;
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
            let mut recording = start_raw_recording(recorder, 16);
            recording.try_record(chunk(b"early-bytes"));
            recording.finalize(RecordingStopReason::Disabled).await;
        }
        assert!(
            tokio::fs::metadata(&sidecar_path(&path)).await.is_err(),
            "no sidecar while timestamps were off"
        );
        {
            let recorder = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, true)
                .await
                .unwrap();
            let mut recording = start_raw_recording(recorder, 16);
            recording.try_record(chunk(b"late"));
            recording.finalize(RecordingStopReason::Disabled).await;
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
        let mut recording = start_display_recording(recorder, 16);
        let cid = ChannelId::new();
        recording.try_record(RenderedOutput {
            channel_id: cid,
            text: "first".to_string(),
            timestamp: None,
        });
        recording.try_record(RenderedOutput {
            channel_id: cid,
            text: "second".to_string(),
            timestamp: None,
        });
        recording.finalize(RecordingStopReason::Disabled).await;

        // The recorder appends verbatim — no injected separators (ADR-018): the
        // rendered stream's own text is the file, byte for byte.
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "firstsecond"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }
}
