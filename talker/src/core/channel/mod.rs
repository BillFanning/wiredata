mod config;
mod serial;
mod tcp;
mod udp;

pub use config::{
    ChannelConfig, DataBits, FlowControl, InterfaceConfig, Parity, SerialConfig, StopBits,
    TcpClientConfig, UdpConfig, UdpMode,
};

/// Stable identity of one configured channel, minted when the channel is
/// created and unchanged for its whole life (ADR-020).
///
/// Channel *slots* are positional — they shift down when a channel above is
/// removed — but a running runner thread keeps stamping the identity it was
/// started with. When that stamp was the slot index, every status and log
/// line from a runner below a removed channel was attributed to the wrong
/// row. Everything that attributes across time (the structured `channel`
/// tracing field, `TalkerStatus`) therefore carries this id; positional
/// indices remain only for "which row right now" (rendering, command
/// routing through the supervisor's slots).
///
/// Runtime-only: never persisted (profiles identify channels by position and
/// name), never reused within a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(u64);

impl ChannelId {
    /// Mint the next process-unique id (1-based, monotonic).
    pub fn mint() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// The raw id value — what the structured `channel` tracing field carries.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Rebuild an id from a raw `channel` tracing-field value (the log layer's
    /// inverse of [`as_u64`](Self::as_u64)).
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A live interface that can send raw bytes.
///
/// An interface is owned by a single talker thread; `Send` is required so it
/// can be moved into that thread.
pub trait Interface: Send {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()>;

    /// Prepare the live handle for a scheduled retry after a send failure.
    ///
    /// Most transient send errors can be retried on the existing handle, so
    /// the default does nothing. A transport whose failed OS handle cannot
    /// become usable again may reopen it from `current`; the runner calls this
    /// only when retry backoff permits another attempt. Returns `true` only
    /// when this call replaced the operating-system handle. An error withholds
    /// that due fire without calling [`send`](Self::send).
    fn prepare_retry(&mut self, _current: Option<&InterfaceConfig>) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Bytes the peer sent since the last call, read and discarded (ADR-059,
    /// §4.5). Only a transport that reads its peer reports any.
    fn take_peer_bytes(&mut self) -> u64 {
        0
    }

    /// Apply `next` to the existing handle when reopening would conflict with
    /// the resource it already owns. Returns `true` when applied in place;
    /// `false` asks the runner to open a replacement and swap on success.
    fn reconfigure(
        &mut self,
        _current: &InterfaceConfig,
        _next: &InterfaceConfig,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// A write that failed after the interface accepted part of the message
/// (§4.4, ADR-059). The receiver may hold that fragment, so the runner counts
/// the send as **possibly partial** rather than failed, and never resends it.
/// Attached as context to the write error, so the message reads
/// "wrote 12 of 40 bytes before the write failed: …".
#[derive(Debug)]
pub struct PartialWrite {
    /// Bytes the interface reported accepting before the error. 0 when the OS
    /// cannot say: a Windows TCP write that timed out reports nothing sent,
    /// though part of the message may have gone.
    pub written: usize,
    /// The message's length.
    pub total: usize,
}

impl std::fmt::Display for PartialWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.written == 0 {
            write!(
                f,
                "the OS cannot say how much of the {} bytes went before the write failed",
                self.total
            )
        } else {
            write!(
                f,
                "wrote {} of {} bytes before the write failed",
                self.written, self.total
            )
        }
    }
}

/// Write all of `data`, keeping count of what the writer accepted (§4.4).
pub(super) fn write_counted<W: std::io::Write + ?Sized>(
    writer: &mut W,
    data: &[u8],
) -> Result<(), WriteFailure> {
    let mut written = 0;
    while written < data.len() {
        match writer.write(&data[written..]) {
            Ok(0) => {
                return Err(WriteFailure {
                    io: std::io::ErrorKind::WriteZero.into(),
                    written,
                    total: data.len(),
                })
            }
            Ok(n) => written += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(WriteFailure {
                    io: error,
                    written,
                    total: data.len(),
                })
            }
        }
    }
    Ok(())
}

/// A failed [`write_counted`]: the OS error, for a transport that classifies
/// it, and how much of the message the writer accepted first.
pub(super) struct WriteFailure {
    pub io: std::io::Error,
    written: usize,
    total: usize,
}

impl WriteFailure {
    /// The error to report. A failure after part of the message was accepted
    /// carries [`PartialWrite`]; one before any byte was accepted is the bare
    /// error.
    pub fn into_error(self, context: &'static str) -> anyhow::Error {
        let Self { io, written, total } = self;
        possibly_partial(
            anyhow::Error::new(io).context(context),
            written,
            total,
            false,
        )
    }

    /// [`into_error`](Self::into_error) with `cause` in place of the OS error.
    /// `sent_unknown` marks a write the OS reports nothing of although part of
    /// it may have gone, which is possibly partial too (§4.4).
    pub fn into_error_as(
        self,
        cause: anyhow::Error,
        context: &'static str,
        sent_unknown: bool,
    ) -> anyhow::Error {
        possibly_partial(
            cause.context(context),
            self.written,
            self.total,
            sent_unknown,
        )
    }
}

fn possibly_partial(
    error: anyhow::Error,
    written: usize,
    total: usize,
    sent_unknown: bool,
) -> anyhow::Error {
    if written > 0 || sent_unknown {
        error.context(PartialWrite { written, total })
    } else {
        error
    }
}

/// A built-in interface needed its confirmed configuration to replace a failed
/// handle, but the runner did not have one. This is talker's state disagreeing
/// with itself, not an error reported by the configured link (ADR-054).
#[derive(Debug)]
pub(crate) struct MissingRetryConfiguration;

impl std::fmt::Display for MissingRetryConfiguration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("retry has no current interface configuration")
    }
}

impl std::error::Error for MissingRetryConfiguration {}

impl InterfaceConfig {
    /// Open the live interface described by this config.
    pub fn open(&self) -> anyhow::Result<Box<dyn Interface>> {
        match self {
            Self::Serial(c) => Ok(Box::new(serial::SerialInterface::open(c)?)),
            Self::Udp(c) => Ok(Box::new(udp::UdpInterface::open(c)?)),
            Self::TcpClient(c) => Ok(Box::new(tcp::TcpClientInterface::open(c)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::UdpSocket;

    use super::*;

    #[test]
    fn open_udp_unicast_succeeds() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let cfg = InterfaceConfig::Udp(UdpConfig::unicast(receiver.local_addr().unwrap()));
        assert!(cfg.open().is_ok());
    }

    /// Accepts `accept` bytes, then fails every write.
    struct Stalls {
        accept: usize,
        written: Vec<u8>,
    }

    impl std::io::Write for Stalls {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let room = self.accept - self.written.len();
            if room == 0 {
                return Err(std::io::ErrorKind::ConnectionReset.into());
            }
            // One byte at a time, as a slow link may.
            let n = data.len().min(room).min(1);
            self.written.extend_from_slice(&data[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_write_that_fails_after_some_bytes_is_possibly_partial() {
        let mut link = Stalls {
            accept: 3,
            written: Vec::new(),
        };
        let failure = write_counted(&mut link, b"$GPGGA").unwrap_err();
        assert_eq!(link.written, b"$GP");
        assert_eq!(failure.io.kind(), std::io::ErrorKind::ConnectionReset);
        let error = failure.into_error("writing");
        let partial = error.downcast_ref::<PartialWrite>().unwrap();
        assert_eq!((partial.written, partial.total), (3, 6));
        assert!(
            format!("{error:#}")
                .starts_with("wrote 3 of 6 bytes before the write failed: writing: "),
            "{error:#}"
        );
    }

    #[test]
    fn a_write_that_fails_before_any_byte_is_failed() {
        let mut link = Stalls {
            accept: 0,
            written: Vec::new(),
        };
        let failure = write_counted(&mut link, b"$GPGGA").unwrap_err();
        assert!(failure
            .into_error("writing")
            .downcast_ref::<PartialWrite>()
            .is_none());

        let mut link = Stalls {
            accept: 6,
            written: Vec::new(),
        };
        assert!(write_counted(&mut link, b"$GPGGA").is_ok());
        assert_eq!(link.written, b"$GPGGA");
    }

    #[test]
    fn open_serial_bad_port_returns_error() {
        let cfg = InterfaceConfig::Serial(SerialConfig::new("/dev/does_not_exist_xyz"));
        assert!(cfg.open().is_err());
    }
}
