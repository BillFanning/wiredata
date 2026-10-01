//! Serial, UDP, TCP listener, and TCP connection transports.
//!
//! This is `listener-transport` (spec §128). It owns interface handles and
//! receive buffers and knows nothing about NMEA, display, or recording formats.
//! This file defines the **push-based transport contract** (§138, ADR-001): the
//! types every transport emits and the runner traits the runtime drives; the
//! concrete Serial/UDP/TCP runners live in the submodules.
//!
//! Two output shapes (§138):
//! - data-bearing sources (Serial, UDP, TCP connection) emit [`ReceivedData`];
//! - the connection acceptor (TCP listener) emits [`NewConnection`].
//!
//! There is no pull-based `receive()`. Lifecycle is cancel-then-await: the
//! runtime cancels the [`CancellationToken`], then awaits completion via
//! [`TransportJoinHandle`].

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::core::{ArrivalTimestampStatus, ChannelId, ChunkTime};

pub mod serial;
pub mod tcp;
pub mod udp;

pub(crate) use serial::SerialStallState;
pub use serial::{
    OpenSerialTransport, SerialControlCommand, SerialControlHooks, SerialControlLines,
    SerialTransport,
};
pub use tcp::{BoundTcpListenerTransport, TcpConnectionTransport, TcpListenerTransport};
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
    pub payload: ReceivedPayload,
    pub received_at: ChunkTime,
}

/// The two payload shapes a transport can deliver (§138). Both are appended to the
/// stream verbatim; the distinction only reflects how the OS delivered the bytes.
#[derive(Clone, Debug)]
pub enum ReceivedPayload {
    /// A stream chunk whose boundaries track OS buffering, not content (Serial, TCP).
    Bytes(Vec<u8>),
    /// One datagram delivered whole by the OS (UDP, §15); appended to the stream
    /// like any other bytes — the boundary is not preserved as structure.
    Datagram(Vec<u8>),
}

impl ReceivedPayload {
    /// The raw bytes of this payload, regardless of shape.
    pub fn bytes(&self) -> &[u8] {
        match self {
            ReceivedPayload::Bytes(b) | ReceivedPayload::Datagram(b) => b,
        }
    }
}

/// Emitted by a TCP listener when it accepts a client (§16, §138). The runtime
/// mints the new connection's [`ChannelId`] on receipt — this event carries the
/// *listener's* id plus the accepted stream and its metadata.
#[derive(Debug)]
pub struct NewConnection {
    pub listener_channel_id: ChannelId,
    pub remote_addr: SocketAddr,
    pub accepted_at: ChunkTime,
    /// The accepted connection (the reason this event exists). `remote_addr` and
    /// `accepted_at` are metadata, not identity.
    pub stream: tokio::net::TcpStream,
}

/// Why a transport's receive/accept loop ended (§94, §101, §111).
#[derive(Debug)]
pub enum TransportOutcome {
    /// Ended because its cancellation token was triggered — a normal stop (§111).
    Cancelled,
    /// Ended normally: TCP EOF (the client closed), or the downstream pipeline
    /// went away. Not a fault.
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
/// a live [`ReceptionStalled`](Self::ReceptionStalled) warning uses `try_send` and
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
    /// The transport ended on a spontaneous fault (§94). Sent by the channel's
    /// fault monitor so the **cause** reaches the pipeline's diagnostics log —
    /// the paired `ChannelFaulted` lifecycle event (§137) carries only the id,
    /// and this string previously died unread with the transport outcome. Terminal
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
    /// A transport that runs as a Tokio task (UDP, TCP).
    Task(tokio::task::JoinHandle<TransportOutcome>),
    /// A transport whose body runs on a dedicated OS thread (Serial); the thread
    /// reports its outcome on this `oneshot`.
    Thread(tokio::sync::oneshot::Receiver<TransportOutcome>),
}

impl TransportJoinHandle {
    /// Await transport completion and learn why it ended (§101). Safe to call
    /// from an async context: the blocking-thread variant awaits a `oneshot`
    /// rather than `JoinHandle::join`. A panicked task / dropped sender reports
    /// `Completed`.
    pub async fn join(self) -> TransportOutcome {
        match self {
            TransportJoinHandle::Task(handle) => {
                handle.await.unwrap_or(TransportOutcome::Completed)
            }
            TransportJoinHandle::Thread(done) => done.await.unwrap_or(TransportOutcome::Completed),
        }
    }
}

/// A data-bearing transport: Serial, UDP, or TCP connection (§138).
///
/// Serial runs its body on a dedicated OS thread, preserving a pending block
/// with bounded `try_send` retries while the queue is full; UDP/TCP run as
/// Tokio tasks. `run` consumes the runner, so transports are dispatched by
/// value (e.g. via an enum), not as `dyn` trait objects.
pub trait DataTransportRunner {
    fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle;
}

/// A connection-accepting transport: the TCP listener (§138). Not a data source
/// — it emits [`NewConnection`], and the runtime turns each into a TCP
/// connection channel.
pub trait ConnectionAcceptorRunner {
    fn run(self, out: Sender<NewConnection>, cancel: CancellationToken) -> TransportJoinHandle;
}
