//! Serial and UDP transports.
//!
//! This is `listener-transport` (spec §128). It owns interface handles and
//! receive buffers and knows nothing about NMEA, display, or recording formats.
//! This file defines the **push-based transport contract** (§138, ADR-001): the
//! types every transport emits and the runner trait the runtime drives; the
//! concrete Serial/UDP runners live in the submodules. Each data-bearing source
//! emits [`ReceivedData`].
//!
//! There is no pull-based `receive()`. Lifecycle is cancel-then-await: the
//! runtime cancels the [`CancellationToken`], then awaits completion via
//! [`TransportJoinHandle`].

use std::time::Duration;

use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::core::{ArrivalTimestampStatus, ChannelId, ChunkTime};

pub mod serial;
pub mod udp;

pub use serial::{
    OpenSerialTransport, SerialControlCommand, SerialControlHooks, SerialControlLines,
    SerialTransport,
};
pub(crate) use serial::{SerialStallState, STALL_WARNING};
pub use udp::{BoundUdpTransport, UdpMode, UdpTransport};

/// One unit of received data emitted by a data-bearing transport (§104, §138).
///
/// Each chunk carries its payload and the [`ChunkTime`] captured when it was
/// read. In the stream-only design (ADR-010) chunk/datagram boundaries are a
/// reception detail only — the pipeline concatenates payloads into one verbatim
/// stream and never reframes them (§17–18).
#[derive(Clone, Debug)]
pub struct ReceivedData {
    pub channel_id: ChannelId,
    /// The received bytes, appended to the stream verbatim whether the OS delivered
    /// them as a stream chunk (Serial) or one whole datagram (UDP, §15).
    pub payload: Vec<u8>,
    pub received_at: ChunkTime,
}

/// Why a transport's receive loop ended (§94, §101, §111).
#[derive(Debug)]
pub enum TransportOutcome {
    /// Ended because its cancellation token was triggered — a normal stop (§111).
    Cancelled,
    /// Ended with no error of its own: the downstream pipeline went away. Whether that is a fault is the caller's
    /// call: a Channel's monitor faults it unless the Channel was being stopped.
    Completed,
    /// Ended on a fatal read/accept error (§94). The reader may have lost data at
    /// the OS before failing — transport-specific loss the runtime reports (§101).
    Faulted(String),
}

/// A transport condition delivered to its pipeline for retained diagnostics
/// (§94, §95, §101; listener ADR-007 / ADR-020).
///
/// The transport stays free of the diagnostics/event vocabulary: it states what
/// happened (self-describing, including which channel), and the pipeline — owner
/// of the channel's `DiagnosticLog` — records it. Delivery follows the condition:
/// a live [`ReceptionStalled`](Self::ReceptionStalled) warning (and its
/// [`ReceptionResumed`](Self::ReceptionResumed) follow-up) uses `try_send` and
/// may drop rather than block reception, while a terminal
/// [`TransportFaulted`](Self::TransportFaulted) is awaited by the runtime monitor
/// after reception ends, when no reader remains to stall.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportNotice {
    /// The reader stalled on the Transport→Pipeline edge — the only edge that may
    /// backpressure the reader (§97.1, §99) — for `stalled_for`. Long enough to
    /// risk a UART/driver overrun: possible transport-specific loss, unquantifiable
    /// from userland (§101). `stalled_for` is the observable proxy for "how bad".
    ReceptionStalled {
        channel_id: ChannelId,
        stalled_for: Duration,
    },
    /// A stall that had passed the [`ReceptionStalled`](Self::ReceptionStalled)
    /// threshold ended because the blocked send went through. `stalled_for` is the
    /// stall's full length. Not sent when the stall ends by cancellation or a
    /// closed queue, nor for a stall that never passed the threshold. Advisory
    /// like the stall notice: it uses `try_send` and may drop.
    ReceptionResumed {
        channel_id: ChannelId,
        stalled_for: Duration,
    },
    /// Linux's attributable per-socket UDP receive-queue drop count. `None` is an
    /// explicit unsupported state on platforms without an equivalent socket API;
    /// `Some(0)` is a supported counter with no observed drops.
    UdpKernelDrops {
        channel_id: ChannelId,
        dropped: Option<u64>,
    },
    /// Effective UDP arrival-timestamp policy after socket setup. An unavailable
    /// request remains explicit instead of silently looking like ordinary post-read timing.
    UdpArrivalTimestamps {
        channel_id: ChannelId,
        status: ArrivalTimestampStatus,
    },
    /// Whether the OS applied a requested shared port (§15, ADR-047). Sent
    /// only when sharing was requested.
    UdpSharedPort {
        channel_id: ChannelId,
        status: udp::SharedPortStatus,
    },
    /// The receive buffer a UDP socket asked for and what the OS granted
    /// (§75). Sent when a size was requested.
    UdpReceiveBuffer {
        channel_id: ChannelId,
        buffer: udp::ReceiveBuffer,
    },
    /// The transport ended on a spontaneous fault (§94). Sent by the channel's
    /// fault monitor so the **cause** reaches the pipeline's diagnostics log —
    /// the paired `ChannelFaulted` lifecycle event (§137) carries only the id,
    /// and the transport outcome's own string is not read elsewhere. Terminal
    /// delivery waits for bounded queue capacity rather than dropping (ADR-020).
    TransportFaulted {
        channel_id: ChannelId,
        cause: String,
    },
}

/// Unifies a Tokio task handle and a dedicated OS thread so the runtime can
/// await transport completion uniformly (§138). Joining a blocking thread goes
/// through an async-observable `oneshot`, so it never blocks a runtime worker.
pub enum TransportJoinHandle {
    /// A transport that runs as a Tokio task (UDP).
    Task(tokio::task::JoinHandle<TransportOutcome>),
    /// A transport whose body runs on a dedicated OS thread (Serial); the thread
    /// reports its outcome on this `oneshot`.
    Thread(tokio::sync::oneshot::Receiver<TransportOutcome>),
}

impl TransportJoinHandle {
    /// Await transport completion and learn why it ended (§101). Safe to call
    /// from an async context: the blocking-thread variant awaits a `oneshot`
    /// rather than `JoinHandle::join`. A panicked task or a dropped sender is
    /// not a normal end: it reports `Faulted`, because no outcome was produced.
    pub async fn join(self) -> TransportOutcome {
        match self {
            TransportJoinHandle::Task(handle) => handle.await.unwrap_or_else(unexpected_end),
            TransportJoinHandle::Thread(done) => done.await.unwrap_or_else(unexpected_end),
        }
    }
}

/// The outcome for a transport that ended without reporting one.
fn unexpected_end(cause: impl std::fmt::Display) -> TransportOutcome {
    TransportOutcome::Faulted(format!("the receive task ended unexpectedly: {cause}"))
}

/// A data-bearing transport: Serial or UDP (§138).
///
/// Serial runs its body on a dedicated OS thread, preserving a pending block
/// with bounded `try_send` retries while the queue is full; UDP runs as a
/// Tokio task. `run` consumes the runner, so transports are dispatched by
/// value (e.g. via an enum), not as `dyn` trait objects.
pub trait DataTransportRunner {
    fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicked_task_joins_as_faulted() {
        let handle = TransportJoinHandle::Task(tokio::spawn(async { panic!("boom") }));
        match handle.join().await {
            TransportOutcome::Faulted(cause) => {
                assert!(cause.starts_with("the receive task ended unexpectedly"));
            }
            other => panic!("expected Faulted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dropped_thread_sender_joins_as_faulted() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        drop(tx);
        assert!(matches!(
            TransportJoinHandle::Thread(rx).join().await,
            TransportOutcome::Faulted(_)
        ));
    }

    #[tokio::test]
    async fn a_reported_outcome_passes_through() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(TransportOutcome::Completed).unwrap();
        assert!(matches!(
            TransportJoinHandle::Thread(rx).join().await,
            TransportOutcome::Completed
        ));
    }
}
