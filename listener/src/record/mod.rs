//! Raw and display recording (spec §51–§59, §142).
//!
//! This is `listener-record` (§128). There are **two independent recording
//! systems** with different inputs, pipeline positions, and guarantees (§51):
//!
//! - **Raw Recording** ([`file::RawFileRecorder`]) — byte-oriented; taps the
//!   received-chunk stream verbatim, exactly as received (§53). Each segment is
//!   byte-exact and contiguous up to a known end (§5.6, §56.1).
//! - **Display Recording** ([`file::DisplayFileRecorder`]) — consumes a display
//!   view's rendered output *after* rendering (§54). Not byte-exact.
//!
//! Each runs as its own [`controller`] task draining a queue bounded in bytes
//! (§142). The producer's enqueue is non-blocking. A fault — a full queue, a
//! failed write or open, a missing destination, low disk — opens a **gap**
//! rather than ending the recording: bytes are omitted, the gap is recorded,
//! and recording continues in a new numbered segment (ADR-043, §56.1).

pub mod controller;
pub mod file;
pub mod file_rotation;
pub mod segments;

use std::path::Path;
use std::sync::Arc;

use crate::core::{GapReason, RecordError};
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

pub use controller::{
    start_recording, Finalized, OpenError, OpenKind, OpenRequest, RecordItem, RecorderReport,
    Recording, SegmentSource, StreamPos, Timings, DEFAULT_QUEUE_BUDGET,
};
pub use file::{DisplayFileRecorder, RawFileRecorder};
pub use file_rotation::is_filesystem_safe;
pub use segments::{
    recording_folder, DisplaySegments, RawSegments, SegmentPlan, DESTINATION_MARKER,
};

/// Time-based recording file rotation (§59). `None` writes a single file; the
/// others write a new file per calendar period, named for the period start (§59).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum FileRotationPolicy {
    #[default]
    None,
    Hourly,
    Daily,
}

/// What to do when the destination file already exists (§80.1). Enforced when
/// recording is enabled (§55, §121); `Refuse` is the default and never clobbers.
/// It governs the first file of an enable only: every later file is a new
/// numbered segment (§59).
///
/// Defined here (record owns file lifecycle, §128); referenced by the profile
/// schema (`config::schema`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum OverwritePolicy {
    #[default]
    Refuse,
    Overwrite,
    AppendIfExists,
}

/// The §59 on-exists rule under rotation, in one place: `Refuse` is meaningless
/// when each period opens a fresh file — and re-opening the current period's
/// file (e.g. after a restart) must append, not fail — so it coerces to
/// `AppendIfExists`. Every other combination passes through. Shared by the
/// runtime settings builders and the GUI editor so the rule cannot drift.
pub fn effective_overwrite(
    policy: OverwritePolicy,
    rotation: FileRotationPolicy,
) -> OverwritePolicy {
    if rotation != FileRotationPolicy::None && policy == OverwritePolicy::Refuse {
        OverwritePolicy::AppendIfExists
    } else {
        policy
    }
}

/// Why a segment is being finalized (§56).
#[derive(Debug)]
pub enum RecordingStopReason {
    /// The user disabled recording.
    Disabled,
    /// The Channel left the Running state, or the segment reached a boundary.
    ChannelStopped,
    /// A fault occurred; the segment ends at its truncation point (§56.1).
    Faulted(RecordError),
}

/// Writes received chunks verbatim, exactly as received (§53, §142).
#[async_trait::async_trait]
pub trait RawRecorder: Send {
    /// Append one received chunk verbatim, exactly as received.
    async fn write_chunk(&mut self, chunk: &ReceivedData) -> Result<(), RecordError>;
    async fn flush(&mut self) -> Result<(), RecordError>;
    /// Flush, close, and (for faults) note the truncation point (§56.1).
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
    /// Bytes in the file now, for the size cap (§59). Zero when unknown.
    fn written_len(&self) -> u64 {
        0
    }
    /// The file being written, for reports.
    fn file_path(&self) -> Option<&Path> {
        None
    }
    /// What opening the file repaired, taken once (§57).
    fn take_open_note(&mut self) -> Option<String> {
        None
    }
}

/// Writes a display view's rendered output, after rendering (§54, §142).
#[async_trait::async_trait]
pub trait DisplayRecorder: Send {
    async fn write_rendered(&mut self, output: &RenderedOutput) -> Result<(), RecordError>;
    async fn flush(&mut self) -> Result<(), RecordError>;
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
    /// Bytes in the file now, for the size cap (§59). Zero when unknown.
    fn written_len(&self) -> u64 {
        0
    }
    /// The file being written, for reports.
    fn file_path(&self) -> Option<&Path> {
        None
    }
}

/// One segment's writer, as the recording controller uses it: the two §142
/// recorders unified so the controller is written once. Each bridges to this.
#[async_trait::async_trait]
pub trait RecorderWriter<I: Send>: Send {
    async fn write(&mut self, item: &I) -> Result<(), RecordError>;
    async fn flush(&mut self) -> Result<(), RecordError>;
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
    /// Bytes in the file now, for the size cap (§59).
    fn bytes_written(&self) -> u64 {
        0
    }
    /// The file being written, for reports.
    fn path(&self) -> Option<&Path> {
        None
    }
    /// What opening the file repaired, taken once (§57).
    fn take_open_note(&mut self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl<R: RawRecorder> RecorderWriter<Arc<ReceivedData>> for R {
    async fn write(&mut self, item: &Arc<ReceivedData>) -> Result<(), RecordError> {
        self.write_chunk(item).await
    }
    async fn flush(&mut self) -> Result<(), RecordError> {
        RawRecorder::flush(self).await
    }
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        RawRecorder::finalize(self, reason).await
    }
    fn bytes_written(&self) -> u64 {
        self.written_len()
    }
    fn path(&self) -> Option<&Path> {
        self.file_path()
    }
    fn take_open_note(&mut self) -> Option<String> {
        RawRecorder::take_open_note(self)
    }
}

#[async_trait::async_trait]
impl<R: DisplayRecorder> RecorderWriter<RenderedOutput> for R {
    async fn write(&mut self, item: &RenderedOutput) -> Result<(), RecordError> {
        self.write_rendered(item).await
    }
    async fn flush(&mut self) -> Result<(), RecordError> {
        DisplayRecorder::flush(self).await
    }
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        DisplayRecorder::finalize(self, reason).await
    }
    fn bytes_written(&self) -> u64 {
        self.written_len()
    }
    fn path(&self) -> Option<&Path> {
        self.file_path()
    }
}

/// A source that yields one ready-made writer, once. A recording built from a
/// pre-opened writer has no way to open another file, so after a fault it
/// waits in its gap until it is stopped.
struct SingleSegment<W> {
    writer: Option<W>,
}

#[async_trait::async_trait]
impl<I, W> SegmentSource<I> for SingleSegment<W>
where
    I: RecordItem,
    W: RecorderWriter<I> + 'static,
{
    async fn open(
        &mut self,
        _request: OpenRequest,
    ) -> Result<Box<dyn RecorderWriter<I>>, OpenError> {
        match self.writer.take() {
            Some(writer) => Ok(Box::new(writer)),
            None => Err(OpenError::Retry {
                reason: GapReason::OpenFailed,
                error: RecordError::Io(std::io::Error::other(
                    "this recording was given one file and cannot continue in another",
                )),
            }),
        }
    }
}

/// Start a Raw Recording on a ready-made writer and return its producer
/// handle (§53, §142). `budget` bounds the queue in bytes.
pub fn start_raw_recording<R: RawRecorder + 'static>(
    recorder: R,
    budget: usize,
) -> Recording<Arc<ReceivedData>> {
    start_recording(
        SingleSegment {
            writer: Some(recorder),
        },
        budget,
        Timings::default(),
    )
}

/// Start a Display Recording on a ready-made writer and return its producer
/// handle (§54, §142). `budget` bounds the queue in bytes.
pub fn start_display_recording<R: DisplayRecorder + 'static>(
    recorder: R,
    budget: usize,
) -> Recording<RenderedOutput> {
    start_recording(
        SingleSegment {
            writer: Some(recorder),
        },
        budget,
        Timings::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime, RecordingState};
    use crate::transport::ReceivedPayload;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    fn chunk(bytes: &[u8]) -> Arc<ReceivedData> {
        Arc::new(ReceivedData {
            channel_id: ChannelId::new(),
            payload: ReceivedPayload::Bytes(bytes.to_vec()),
            received_at: ChunkTime::now(),
        })
    }

    fn pos(offset: u64) -> StreamPos {
        StreamPos {
            offset,
            at: SystemTime::now(),
        }
    }

    /// A recorder whose write fails with a disk-style I/O error.
    struct DiskFailRecorder;

    #[async_trait::async_trait]
    impl RawRecorder for DiskFailRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            Err(RecordError::Io(std::io::Error::other("disk full")))
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    /// §56.1: after a write fault the handle reads as in a gap with no further
    /// enqueues, so a stream that goes quiet right after the disk fails does
    /// not show a healthy recording.
    #[tokio::test]
    async fn a_write_fault_is_visible_on_the_handle_without_further_enqueues() {
        let mut recording = start_raw_recording(DiskFailRecorder, DEFAULT_QUEUE_BUDGET);
        recording.try_record(chunk(b"data"), pos(0)); // accepted; the write fails
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !matches!(recording.state(), RecordingState::Gap(_)) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the handle never observed the gap"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            recording.state(),
            RecordingState::Gap(GapReason::WriteFailed)
        );
        let reports = recording.take_reports();
        assert!(
            reports.iter().any(|r| matches!(
                r,
                RecorderReport::GapOpened { detail, .. } if detail.contains("disk full")
            )),
            "the gap carries its cause: {reports:?}"
        );
    }

    /// A recorder whose first write succeeds and second fails.
    #[derive(Default)]
    struct SecondWriteFails {
        writes: usize,
    }

    #[async_trait::async_trait]
    impl RawRecorder for SecondWriteFails {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            self.writes += 1;
            if self.writes >= 2 {
                Err(RecordError::Io(std::io::Error::other("disk full")))
            } else {
                Ok(())
            }
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    /// §56.1: a stop that truncates the accepted backlog must not read as
    /// clean. On the current-thread test runtime the controller first runs
    /// inside `finalize`, so both items are written during the stop's drain.
    #[tokio::test]
    async fn a_stop_that_truncates_the_backlog_reports_a_fault() {
        let mut recording = start_raw_recording(SecondWriteFails::default(), DEFAULT_QUEUE_BUDGET);
        recording.try_record(chunk(b"one"), pos(0));
        recording.try_record(chunk(b"two"), pos(3)); // this write fails
        let finalized = recording.finalize(RecordingStopReason::Disabled).await;
        assert!(
            finalized
                .fault
                .expect("a dirty stop must surface")
                .contains("disk full"),
            "the fault carries the cause"
        );
    }

    /// A recorder that counts the chunks it accepts.
    #[derive(Clone, Default)]
    struct CountingRecorder {
        count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl RawRecorder for CountingRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_clean_stop_reports_no_fault() {
        let mut recording = start_raw_recording(CountingRecorder::default(), DEFAULT_QUEUE_BUDGET);
        recording.try_record(chunk(b"data"), pos(0));
        let finalized = recording.finalize(RecordingStopReason::Disabled).await;
        assert!(finalized.fault.is_none());
    }

    /// A recorder whose writes succeed but whose periodic flush fails.
    struct FlushFailRecorder;

    #[async_trait::async_trait]
    impl RawRecorder for FlushFailRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Err(RecordError::Io(std::io::Error::other("flush: device gone")))
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    /// A failed periodic flush gaps the recording with no traffic at all; the
    /// paused clock drives the flush tick without real waits.
    #[tokio::test(start_paused = true)]
    async fn a_flush_failure_gaps_the_recording() {
        let recording = start_raw_recording(FlushFailRecorder, DEFAULT_QUEUE_BUDGET);
        let flush = Timings::default().flush;
        let mut waited = Duration::ZERO;
        while !matches!(recording.state(), RecordingState::Gap(_)) {
            assert!(
                waited < flush * 10,
                "the handle never observed the flush fault"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += Duration::from_millis(100);
        }
        let reports = recording.take_reports();
        assert!(
            reports.iter().any(|r| matches!(
                r,
                RecorderReport::GapOpened { detail, .. } if detail.contains("device gone")
            )),
            "unexpected reports: {reports:?}"
        );
    }

    /// A recorder whose first write blocks until released, so items pile up
    /// in the queue without deadlocking the drain on finalize.
    struct BlockFirstRecorder {
        release: Arc<tokio::sync::Notify>,
        blocked_once: bool,
    }

    #[async_trait::async_trait]
    impl RawRecorder for BlockFirstRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            if !self.blocked_once {
                self.blocked_once = true;
                self.release.notified().await;
            }
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    /// The queue is measured in bytes, each item costing its length plus a
    /// fixed allowance (ADR-043).
    #[tokio::test]
    async fn queue_depth_counts_bytes_and_keeps_the_peak() {
        let release = Arc::new(tokio::sync::Notify::new());
        let recorder = BlockFirstRecorder {
            release: release.clone(),
            blocked_once: false,
        };
        let mut recording = start_raw_recording(recorder, 1024);
        assert_eq!(recording.queue_depth(), (0, 0, 1024));

        for offset in 0..5 {
            recording.try_record(chunk(b"x"), pos(offset));
        }
        let per_item = 1 + controller::PER_ITEM_OVERHEAD;
        assert_eq!(recording.queue_depth(), (5 * per_item, 5 * per_item, 1024));
        tokio::task::yield_now().await; // the controller takes the first item and blocks

        let (current, peak, budget) = recording.queue_depth();
        assert_eq!((peak, budget), (5 * per_item, 1024));
        assert!(current < peak, "the controller took an item off the queue");

        release.notify_one();
        let _ = recording.finalize(RecordingStopReason::Disabled).await;
    }

    /// A recorder that counts flush calls.
    #[derive(Clone, Default)]
    struct FlushCountingRecorder {
        flushes: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl RawRecorder for FlushCountingRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_recorder_flushes_on_a_timer() {
        // §56: buffered output reaches the OS each flush interval while the
        // recording runs, not only at finalize.
        let recorder = FlushCountingRecorder::default();
        let flushes = recorder.flushes.clone();
        let mut recording = start_raw_recording(recorder, DEFAULT_QUEUE_BUDGET);

        recording.try_record(chunk(b"x"), pos(0));
        tokio::task::yield_now().await;
        let before = flushes.load(Ordering::SeqCst);

        tokio::time::advance(Timings::default().flush + Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        assert!(
            flushes.load(Ordering::SeqCst) > before,
            "a running recorder flushes on the timer, not only at finalize"
        );
        let _ = recording.finalize(RecordingStopReason::Disabled).await;
    }

    #[tokio::test]
    async fn a_graceful_stop_writes_the_backlog() {
        let recorder = CountingRecorder::default();
        let count = recorder.count.clone();
        let mut recording = start_raw_recording(recorder, DEFAULT_QUEUE_BUDGET);
        for offset in 0..3 {
            recording.try_record(chunk(b"x"), pos(offset));
        }
        assert_eq!(recording.state(), RecordingState::Enabled);
        let _ = recording.finalize(RecordingStopReason::Disabled).await;
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }
}
