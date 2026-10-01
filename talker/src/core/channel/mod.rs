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

    #[test]
    fn open_serial_bad_port_returns_error() {
        let cfg = InterfaceConfig::Serial(SerialConfig::new("/dev/does_not_exist_xyz"));
        assert!(cfg.open().is_err());
    }
}
