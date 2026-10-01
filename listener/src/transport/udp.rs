//! UDP transport: unicast, broadcast, and multicast (spec §15).
//!
//! A UDP transport runs as a Tokio task (ADR-001 — async-native socket I/O, no
//! dedicated thread). Each received datagram is delivered whole by the OS (§15):
//! the transport emits `ReceivedPayload::Datagram`, which the pipeline appends to
//! the verbatim stream like any other bytes (ADR-010 — no extraction, the datagram
//! boundary is a reception detail, not stream structure).
//!
//! Binding is separated from the receive loop so resource errors surface at
//! Channel Start (§71, §8.2): [`UdpTransport::bind`] performs the fallible
//! socket setup and returns a [`BoundUdpTransport`] whose infallible
//! [`DataTransportRunner::run`] spawns the loop (§138).

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::time::SystemTime;

use tokio::net::UdpSocket;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::core::{ArrivalTimestampStatus, ChannelId, ChunkTime};

use super::{
    DataTransportRunner, ReceivedData, ReceivedPayload, TransportJoinHandle, TransportNotice,
    TransportOutcome,
};

/// Maximum size of a single UDP datagram payload (IPv4 theoretical max). The
/// receive buffer is sized to this so no datagram is ever truncated.
const MAX_DATAGRAM: usize = 65_535;

/// How a UDP Channel receives (§15, mirrors `UdpMode` in the profile schema §75).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UdpMode {
    Unicast,
    Broadcast,
    Multicast,
}

/// Whether a UDP Channel's port is shared with other programs (§15, ADR-047).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SharedPortStatus {
    /// Not requested.
    #[default]
    NotRequested,
    /// Requested, and the OS applied address reuse before the bind.
    Applied,
    /// Requested, but the OS did not apply address reuse: the port is not shared.
    NotApplied,
}

/// The receive buffer a UDP socket asked for and what the OS granted (§75).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveBuffer {
    pub requested: usize,
    /// In the request's terms. Linux reports twice the size it set, to cover
    /// its bookkeeping (socket(7)), so its figure is halved.
    pub granted: usize,
}

impl ReceiveBuffer {
    /// The OS granted less than was asked for. On Linux the cap is
    /// `net.core.rmem_max`.
    pub fn is_short(&self) -> bool {
        self.granted < self.requested
    }
}

/// An unbound UDP transport description (§15, §75). Call [`bind`](Self::bind) at
/// Channel Start to acquire the socket.
#[derive(Clone, Debug)]
pub struct UdpTransport {
    channel_id: ChannelId,
    bind_addr: SocketAddr,
    mode: UdpMode,
    multicast_group: Option<IpAddr>,
    multicast_interface: Option<Ipv4Addr>,
    recv_buffer: Option<usize>,
    kernel_timestamps: bool,
    shared_port: bool,
}

impl UdpTransport {
    pub fn new(channel_id: ChannelId, bind_addr: SocketAddr, mode: UdpMode) -> Self {
        Self {
            channel_id,
            bind_addr,
            mode,
            multicast_group: None,
            multicast_interface: None,
            recv_buffer: None,
            kernel_timestamps: false,
            shared_port: false,
        }
    }

    /// Set the multicast group to join (required for [`UdpMode::Multicast`]).
    pub fn with_multicast_group(mut self, group: IpAddr) -> Self {
        self.multicast_group = Some(group);
        self
    }

    /// Select the local IPv4 interface to join the multicast group on (§167), for
    /// multi-homed hosts. Without it, the OS default interface is used.
    pub fn with_multicast_interface(mut self, interface: Ipv4Addr) -> Self {
        self.multicast_interface = Some(interface);
        self
    }

    /// Ask for SO_RCVBUF in bytes (§75) — the lever against kernel-dropped UDP
    /// (§101). The OS may grant less; the bound transport reports what it got.
    pub fn with_recv_buffer(mut self, bytes: usize) -> Self {
        self.recv_buffer = Some(bytes);
        self
    }

    /// Request OS receive timestamps for UDP datagrams. Unsupported systems retain
    /// the post-read timestamp and report that fallback through transport health.
    pub fn with_kernel_timestamps(mut self) -> Self {
        self.kernel_timestamps = true;
        self
    }

    /// Request a shared port (§15, ADR-047): set address reuse before the bind,
    /// so another program can bind the same port. Meant for broadcast and
    /// multicast; profile validation refuses it for unicast, where the OS
    /// delivers each datagram to only one of the sharing sockets.
    pub fn with_shared_port(mut self) -> Self {
        self.shared_port = true;
        self
    }

    /// Bind the socket and apply mode-specific setup (§8.2). Fallible so the
    /// runtime can take Starting → Faulted on failure (§9, §71).
    pub async fn bind(self) -> io::Result<BoundUdpTransport> {
        let BuiltSocket {
            socket: std_socket,
            drop_counter_supported,
            kernel_timestamps_active,
            shared_port,
            receive_buffer,
        } = build_udp_socket(&self)?;
        std_socket.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_socket)?;
        match self.mode {
            UdpMode::Unicast => {}
            UdpMode::Broadcast => socket.set_broadcast(true)?,
            UdpMode::Multicast => {
                let group = self.multicast_group.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "multicast mode requires a multicast group address",
                    )
                })?;
                match group {
                    IpAddr::V4(group) => {
                        let iface = self.multicast_interface.unwrap_or(Ipv4Addr::UNSPECIFIED);
                        socket.join_multicast_v4(group, iface)?;
                    }
                    IpAddr::V6(group) => socket.join_multicast_v6(&group, 0)?,
                }
            }
        }
        Ok(BoundUdpTransport {
            channel_id: self.channel_id,
            socket,
            notices: None,
            drop_counter_supported,
            kernel_timestamps_requested: self.kernel_timestamps,
            kernel_timestamps_active,
            shared_port,
            receive_buffer,
        })
    }
}

/// A bound std socket and what the OS applied while building it.
struct BuiltSocket {
    socket: std::net::UdpSocket,
    drop_counter_supported: bool,
    kernel_timestamps_active: bool,
    shared_port: SharedPortStatus,
    receive_buffer: Option<ReceiveBuffer>,
}

/// Build and bind a std UDP socket, setting SO_RCVBUF (§167) and address reuse
/// (§15) *before* bind — the point most platforms honor. Built via `socket2` so
/// the options can be set on the raw socket before it is handed to Tokio.
fn build_udp_socket(transport: &UdpTransport) -> io::Result<BuiltSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let addr = transport.bind_addr;
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    let receive_buffer = transport
        .recv_buffer
        .map(|requested| request_receive_buffer(&socket, requested));
    let drop_counter_supported = enable_udp_drop_counter(&socket);
    let kernel_timestamps_active =
        enable_udp_kernel_timestamps(&socket, transport.kernel_timestamps);
    let shared_port = if transport.shared_port {
        share_port(&socket)
    } else {
        SharedPortStatus::NotRequested
    };
    socket.bind(&addr.into())?;
    Ok(BuiltSocket {
        socket: socket.into(),
        drop_counter_supported,
        kernel_timestamps_active,
        shared_port,
        receive_buffer,
    })
}

/// Ask for `requested` bytes of receive buffer (§75) and read back what the
/// OS granted. A refusal is not fatal: macOS refuses a size over its limit
/// outright, and the socket then keeps its default, which status reports.
fn request_receive_buffer(socket: &socket2::Socket, requested: usize) -> ReceiveBuffer {
    let _ = socket.set_recv_buffer_size(requested);
    let reported = socket.recv_buffer_size().unwrap_or(0);
    let granted = if cfg!(target_os = "linux") {
        reported / 2
    } else {
        reported
    };
    ReceiveBuffer { requested, granted }
}

/// Set address reuse before the bind (§15, ADR-047): SO_REUSEADDR, and on Unix
/// SO_REUSEPORT as well. macOS needs it to share a port bound to an address
/// that is not multicast, and with both set Linux shares the port with a
/// program that set either. Each option is read back, so the status reports
/// what the OS applied rather than what was asked.
fn share_port(socket: &socket2::Socket) -> SharedPortStatus {
    let reuse_address =
        socket.set_reuse_address(true).is_ok() && socket.reuse_address().unwrap_or(false);
    #[cfg(unix)]
    let reuse_port = socket.set_reuse_port(true).is_ok() && socket.reuse_port().unwrap_or(false);
    #[cfg(not(unix))]
    let reuse_port = true;
    if reuse_address && reuse_port {
        SharedPortStatus::Applied
    } else {
        SharedPortStatus::NotApplied
    }
}

#[cfg(target_os = "linux")]
fn enable_udp_drop_counter(socket: &socket2::Socket) -> bool {
    use std::os::fd::AsRawFd;

    let enabled: libc::c_int = 1;
    // SAFETY: `socket` owns a valid descriptor; `enabled` points to an initialized
    // integer for exactly the size passed to `setsockopt`.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RXQ_OVFL,
            (&raw const enabled).cast(),
            std::mem::size_of_val(&enabled) as libc::socklen_t,
        ) == 0
    }
}

#[cfg(not(target_os = "linux"))]
fn enable_udp_drop_counter(_socket: &socket2::Socket) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn enable_udp_kernel_timestamps(socket: &socket2::Socket, requested: bool) -> bool {
    use std::os::fd::AsRawFd;

    if !requested {
        return false;
    }
    let enabled: libc::c_int = 1;
    // SAFETY: `socket` owns a valid descriptor; `enabled` is a live integer with
    // the exact byte length passed to `setsockopt`.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TIMESTAMPNS,
            (&raw const enabled).cast(),
            std::mem::size_of_val(&enabled) as libc::socklen_t,
        ) == 0
    }
}

#[cfg(not(target_os = "linux"))]
fn enable_udp_kernel_timestamps(_socket: &socket2::Socket, _requested: bool) -> bool {
    false
}

/// A bound UDP socket ready to receive. Implements [`DataTransportRunner`].
#[derive(Debug)]
pub struct BoundUdpTransport {
    channel_id: ChannelId,
    socket: UdpSocket,
    notices: Option<Sender<TransportNotice>>,
    drop_counter_supported: bool,
    kernel_timestamps_requested: bool,
    kernel_timestamps_active: bool,
    shared_port: SharedPortStatus,
    receive_buffer: Option<ReceiveBuffer>,
}

impl BoundUdpTransport {
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// The actual bound address (useful when binding to an ephemeral port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn with_notice_sender(mut self, notices: Sender<TransportNotice>) -> Self {
        self.notices = Some(notices);
        self
    }
}

impl DataTransportRunner for BoundUdpTransport {
    fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle {
        let BoundUdpTransport {
            channel_id,
            socket,
            notices,
            drop_counter_supported,
            kernel_timestamps_requested,
            kernel_timestamps_active,
            shared_port,
            receive_buffer,
        } = self;
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            let mut last_kernel_drops = 0u32;
            let mut cumulative_kernel_drops = 0u64;
            if let Some(notices) = &notices {
                let _ = notices.try_send(TransportNotice::UdpKernelDrops {
                    channel_id,
                    dropped: drop_counter_supported.then_some(0),
                });
                let status = if kernel_timestamps_active {
                    ArrivalTimestampStatus::KernelSoftware
                } else if kernel_timestamps_requested {
                    ArrivalTimestampStatus::KernelRequestedUnavailable
                } else {
                    ArrivalTimestampStatus::PostRead
                };
                let _ =
                    notices.try_send(TransportNotice::UdpArrivalTimestamps { channel_id, status });
                if shared_port != SharedPortStatus::NotRequested {
                    let _ = notices.try_send(TransportNotice::UdpSharedPort {
                        channel_id,
                        status: shared_port,
                    });
                }
                if let Some(buffer) = receive_buffer {
                    let _ =
                        notices.try_send(TransportNotice::UdpReceiveBuffer { channel_id, buffer });
                }
            }
            loop {
                tokio::select! {
                    biased;
                    // Cooperative cancellation, checked first (§111).
                    _ = cancel.cancelled() => return TransportOutcome::Cancelled,
                    res = receive_datagram(&socket, &mut buf) => match res {
                        Ok((n, kernel_drops, kernel_wall_clock)) => {
                            let received_at = kernel_wall_clock.map_or_else(
                                ChunkTime::now,
                                |wall_clock| ChunkTime::now().with_kernel_wall_clock(wall_clock),
                            );
                            if drop_counter_supported {
                                if let Some(kernel_drops) = kernel_drops {
                                    let newly_dropped = kernel_drops.wrapping_sub(last_kernel_drops);
                                    last_kernel_drops = kernel_drops;
                                    if newly_dropped > 0 {
                                        cumulative_kernel_drops = cumulative_kernel_drops
                                            .saturating_add(u64::from(newly_dropped));
                                        if let Some(notices) = &notices {
                                            let _ = notices.try_send(
                                                TransportNotice::UdpKernelDrops {
                                                    channel_id,
                                                    dropped: Some(cumulative_kernel_drops),
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                            let data = ReceivedData {
                                channel_id,
                                payload: ReceivedPayload::Datagram(buf[..n].to_vec()),
                                received_at,
                            };
                            // Awaiting `send` is the one place this transport may
                            // stall: a full Transport→Pipeline queue backpressures the
                            // recv loop, which can drop datagrams at the kernel (§97.1,
                            // §101). An `Err` means the pipeline is gone.
                            if out.send(data).await.is_err() {
                                return TransportOutcome::Completed;
                            }
                        }
                        // A receive error ends the transport as a fault (§94). Some
                        // loss (e.g. kernel-dropped datagrams) is not observable
                        // from userland; we report the failure we can see (§101).
                        Err(e) => return TransportOutcome::Faulted(format!("UDP receive failed: {e}")),
                    },
                }
            }
        });
        TransportJoinHandle::Task(handle)
    }
}

#[cfg(not(target_os = "linux"))]
async fn receive_datagram(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, Option<u32>, Option<SystemTime>)> {
    socket
        .recv_from(buf)
        .await
        .map(|(bytes, _)| (bytes, None, None))
}

#[cfg(target_os = "linux")]
async fn receive_datagram(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, Option<u32>, Option<SystemTime>)> {
    use std::os::fd::AsRawFd;

    socket
        .async_io(tokio::io::Interest::READABLE, || {
            receive_datagram_with_metadata(socket.as_raw_fd(), buf)
        })
        .await
}

#[cfg(target_os = "linux")]
fn receive_datagram_with_metadata(
    socket: std::os::fd::RawFd,
    buf: &mut [u8],
) -> io::Result<(usize, Option<u32>, Option<SystemTime>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // u64 storage gives the control buffer cmsghdr alignment as well as ample room.
    let mut control = [0u64; 16];
    // SAFETY: every pointer in `message` names initialized, live storage for the
    // duration of this nonblocking recvmsg call. The socket is owned by `UdpSocket`.
    let (received, message) = unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control);
        let received = libc::recvmsg(socket, &raw mut message, 0);
        (received, message)
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut dropped = None;
    let mut kernel_wall_clock = None;
    // SAFETY: libc's CMSG helpers walk only the control range initialized by
    // recvmsg. We verify the level/type and payload length before reading u32.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET
                && (*header).cmsg_type == libc::SO_RXQ_OVFL
                && (*header).cmsg_len >= libc::CMSG_LEN(std::mem::size_of::<u32>() as _) as usize
            {
                dropped = Some((libc::CMSG_DATA(header).cast::<u32>()).read_unaligned());
            } else if (*header).cmsg_level == libc::SOL_SOCKET
                && (*header).cmsg_type == libc::SCM_TIMESTAMPNS
                && (*header).cmsg_len
                    >= libc::CMSG_LEN(std::mem::size_of::<libc::timespec>() as _) as usize
            {
                let timestamp = (libc::CMSG_DATA(header).cast::<libc::timespec>()).read_unaligned();
                if timestamp.tv_sec >= 0 && (0..1_000_000_000).contains(&timestamp.tv_nsec) {
                    kernel_wall_clock = SystemTime::UNIX_EPOCH.checked_add(Duration::new(
                        timestamp.tv_sec as u64,
                        timestamp.tv_nsec as u32,
                    ));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    Ok((received as usize, dropped, kernel_wall_clock))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §75: the receive buffer is asked for before the bind, and what the OS
    /// granted is what the socket has, in the request's terms.
    #[test]
    fn the_granted_receive_buffer_is_what_the_socket_has() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let requested = 512 * 1024;
        let transport =
            UdpTransport::new(ChannelId::new(), addr, UdpMode::Unicast).with_recv_buffer(requested);
        let built = build_udp_socket(&transport).unwrap();
        let buffer = built.receive_buffer.expect("a size was requested");
        assert_eq!(buffer.requested, requested);
        let reported = socket2::SockRef::from(&built.socket)
            .recv_buffer_size()
            .unwrap();

        #[cfg(target_os = "linux")]
        {
            // Linux sets min(requested, rmem_max) and reports twice that.
            let rmem_max: usize = std::fs::read_to_string("/proc/sys/net/core/rmem_max")
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(buffer.granted, requested.min(rmem_max));
            assert_eq!(buffer.granted, reported / 2);
            assert_eq!(buffer.is_short(), rmem_max < requested);
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(buffer.granted, reported);
            assert!(!buffer.is_short(), "{buffer:?}");
        }
    }

    #[test]
    fn a_receive_buffer_is_short_only_when_less_was_granted() {
        let buffer = |granted| ReceiveBuffer {
            requested: 4 << 20,
            granted,
        };
        assert!(!buffer(4 << 20).is_short());
        assert!(!buffer(8 << 20).is_short());
        assert!(buffer(208 * 1024).is_short());
    }

    #[tokio::test]
    async fn the_receive_buffer_is_reported_when_one_was_asked_for() {
        let cid = ChannelId::new();
        let bound = UdpTransport::new(cid, "127.0.0.1:0".parse().unwrap(), UdpMode::Unicast)
            .with_recv_buffer(256 * 1024)
            .bind()
            .await
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(2);
        let (notice_tx, mut notice_rx) = tokio::sync::mpsc::channel(4);
        let cancel = CancellationToken::new();
        let handle = bound.with_notice_sender(notice_tx).run(tx, cancel.clone());

        let _drop_status = notice_rx.recv().await.unwrap();
        let _arrival_status = notice_rx.recv().await.unwrap();
        match notice_rx.recv().await.unwrap() {
            TransportNotice::UdpReceiveBuffer { channel_id, buffer } => {
                assert_eq!(channel_id, cid);
                assert_eq!(buffer.requested, 256 * 1024);
                assert!(buffer.granted > 0, "{buffer:?}");
            }
            other => panic!("expected the receive buffer, got {other:?}"),
        }

        cancel.cancel();
        assert!(matches!(handle.join().await, TransportOutcome::Cancelled));
    }

    fn broadcast(addr: SocketAddr) -> UdpTransport {
        UdpTransport::new(ChannelId::new(), addr, UdpMode::Broadcast)
    }

    #[tokio::test]
    async fn sockets_that_request_a_shared_port_bind_the_same_port() {
        // §15, ADR-047: another program can bind the port too. Both request
        // sharing, as the other program must on Linux and macOS.
        let first = broadcast("127.0.0.1:0".parse().unwrap())
            .with_shared_port()
            .bind()
            .await
            .unwrap();
        let addr = first.local_addr().unwrap();
        let second = broadcast(addr)
            .with_shared_port()
            .bind()
            .await
            .expect("the shared port binds a second time");
        assert_eq!(second.local_addr().unwrap(), addr);
        assert_eq!(first.shared_port, SharedPortStatus::Applied);
        assert_eq!(second.shared_port, SharedPortStatus::Applied);
    }

    #[tokio::test]
    async fn without_sharing_a_second_bind_of_the_port_is_refused() {
        let first = broadcast("127.0.0.1:0".parse().unwrap())
            .bind()
            .await
            .unwrap();
        let err = broadcast(first.local_addr().unwrap())
            .bind()
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(first.shared_port, SharedPortStatus::NotRequested);
    }

    #[tokio::test]
    async fn a_shared_port_reports_whether_the_os_applied_it() {
        let cid = ChannelId::new();
        let bound = UdpTransport::new(cid, "127.0.0.1:0".parse().unwrap(), UdpMode::Broadcast)
            .with_shared_port()
            .bind()
            .await
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(2);
        let (notice_tx, mut notice_rx) = tokio::sync::mpsc::channel(4);
        let cancel = CancellationToken::new();
        let handle = bound.with_notice_sender(notice_tx).run(tx, cancel.clone());

        let _drop_status = notice_rx.recv().await.unwrap();
        let _arrival_status = notice_rx.recv().await.unwrap();
        assert_eq!(
            notice_rx.recv().await.unwrap(),
            TransportNotice::UdpSharedPort {
                channel_id: cid,
                status: SharedPortStatus::Applied,
            }
        );

        cancel.cancel();
        assert!(matches!(handle.join().await, TransportOutcome::Cancelled));
    }

    #[tokio::test]
    async fn unicast_receives_each_datagram_as_a_chunk() {
        let cid = ChannelId::new();
        let transport = UdpTransport::new(cid, "127.0.0.1:0".parse().unwrap(), UdpMode::Unicast);
        let bound = transport.bind().await.unwrap();
        let server_addr = bound.local_addr().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let (notice_tx, mut notice_rx) = tokio::sync::mpsc::channel(4);
        let cancel = CancellationToken::new();
        let handle = bound.with_notice_sender(notice_tx).run(tx, cancel.clone());

        assert_eq!(
            notice_rx.recv().await.unwrap(),
            TransportNotice::UdpKernelDrops {
                channel_id: cid,
                dropped: cfg!(target_os = "linux").then_some(0),
            }
        );
        assert_eq!(
            notice_rx.recv().await.unwrap(),
            TransportNotice::UdpArrivalTimestamps {
                channel_id: cid,
                status: ArrivalTimestampStatus::PostRead,
            }
        );

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"$GPGGA,first", server_addr).await.unwrap();
        client.send_to(b"second", server_addr).await.unwrap();

        // `recv().await` blocks until the transport emits — deterministic, no sleep.
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert_eq!(first.payload.bytes(), b"$GPGGA,first");
        assert_eq!(second.payload.bytes(), b"second");
        assert!(matches!(first.payload, ReceivedPayload::Datagram(_)));
        assert_eq!(
            first.received_at.wall_clock_source,
            crate::core::ArrivalTimestampSource::PostRead
        );

        cancel.cancel();
        assert!(matches!(handle.join().await, TransportOutcome::Cancelled));
    }

    #[tokio::test]
    async fn requested_kernel_timestamps_report_active_or_explicit_fallback() {
        let cid = ChannelId::new();
        let transport = UdpTransport::new(cid, "127.0.0.1:0".parse().unwrap(), UdpMode::Unicast)
            .with_kernel_timestamps();
        let bound = transport.bind().await.unwrap();
        let server_addr = bound.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let (notice_tx, mut notice_rx) = tokio::sync::mpsc::channel(4);
        let cancel = CancellationToken::new();
        let handle = bound.with_notice_sender(notice_tx).run(tx, cancel.clone());

        let _drop_status = notice_rx.recv().await.unwrap();
        let expected_status = if cfg!(target_os = "linux") {
            ArrivalTimestampStatus::KernelSoftware
        } else {
            ArrivalTimestampStatus::KernelRequestedUnavailable
        };
        assert_eq!(
            notice_rx.recv().await.unwrap(),
            TransportNotice::UdpArrivalTimestamps {
                channel_id: cid,
                status: expected_status,
            }
        );

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"timestamped", server_addr).await.unwrap();
        let received = rx.recv().await.unwrap();
        let expected_source = if cfg!(target_os = "linux") {
            crate::core::ArrivalTimestampSource::KernelSoftware
        } else {
            crate::core::ArrivalTimestampSource::PostRead
        };
        assert_eq!(received.received_at.wall_clock_source, expected_source);

        #[cfg(target_os = "linux")]
        {
            // This deliberately checks only gross plausibility. SO_TIMESTAMPNS is
            // software, datagram-granular wall-clock metadata, not an accuracy
            // promise; a wide bound still catches an epoch/size/alignment decode
            // error without making normal scheduler delay part of the contract.
            let now = SystemTime::now();
            let distance = received
                .received_at
                .wall_clock
                .duration_since(now)
                .unwrap_or_else(|before| before.duration());
            assert!(
                distance <= Duration::from_secs(60),
                "kernel receive timestamp was implausibly far from now: {distance:?}"
            );
        }

        cancel.cancel();
        assert!(matches!(handle.join().await, TransportOutcome::Cancelled));
    }

    #[tokio::test]
    async fn broadcast_mode_binds() {
        let transport = UdpTransport::new(
            ChannelId::new(),
            "0.0.0.0:0".parse().unwrap(),
            UdpMode::Broadcast,
        );
        assert!(transport.bind().await.is_ok());
    }

    #[tokio::test]
    async fn multicast_mode_joins_the_group() {
        let transport = UdpTransport::new(
            ChannelId::new(),
            "0.0.0.0:0".parse().unwrap(),
            UdpMode::Multicast,
        )
        .with_multicast_group("239.0.0.1".parse().unwrap());
        assert!(transport.bind().await.is_ok());
    }

    #[tokio::test]
    async fn multicast_without_group_is_rejected() {
        let transport = UdpTransport::new(
            ChannelId::new(),
            "0.0.0.0:0".parse().unwrap(),
            UdpMode::Multicast,
        );
        let err = transport.bind().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
