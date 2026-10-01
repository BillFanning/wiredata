use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use anyhow::Context;

use super::config::{InterfaceConfig, TcpClientConfig};
use super::{Interface, MissingRetryConfiguration};

/// Cap on connect. Without it the OS default applies (~20s on Windows),
/// which is far too long for an interactive tool to sit unresponsive.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on each blocking write. Without it, a peer that stops reading
/// (dead device, full window) wedges the owning talker thread inside
/// `write_all` forever — it can then never see its Stop command.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct TcpClientInterface {
    // `None` only between a failed stream being dropped and a later retry
    // connecting again.
    stream: Option<TcpStream>,
    /// The last write failed, so this stream is not used again (ADR-059): the
    /// peer may be gone, or may hold part of a message the next one would be
    /// appended to. The next retry point connects afresh instead.
    reconnect_required: bool,
}

impl TcpClientInterface {
    pub(super) fn open(config: &TcpClientConfig) -> anyhow::Result<Self> {
        Ok(Self {
            stream: Some(connect(config)?),
            reconnect_required: false,
        })
    }
}

fn connect(config: &TcpClientConfig) -> anyhow::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&config.address, CONNECT_TIMEOUT)
        .with_context(|| format!("connecting to {}", config.address))?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .context("setting TCP write timeout")?;
    // Talker is a timing-oriented test tool: each scheduled message
    // should hit the wire when it fires, not when Nagle decides to
    // coalesce it with the next one.
    stream.set_nodelay(true).context("setting TCP_NODELAY")?;
    Ok(stream)
}

impl Interface for TcpClientInterface {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let stream = self.stream.as_mut().context("not connected")?;
        if let Err(error) = stream.write_all(data) {
            self.reconnect_required = true;
            return Err(error).context("writing to TCP stream");
        }
        Ok(())
    }

    /// Connect again at a retry point after a failed write (ADR-059, §4.5).
    /// The failed message is never resent: the runner sends the next due one
    /// on the new connection. A failed connect leaves no stream, so every
    /// later retry point tries again.
    fn prepare_retry(&mut self, current: Option<&InterfaceConfig>) -> anyhow::Result<bool> {
        if !self.reconnect_required {
            return Ok(false);
        }
        let Some(InterfaceConfig::TcpClient(config)) = current else {
            return Err(MissingRetryConfiguration.into());
        };
        drop(self.stream.take());
        self.stream = Some(connect(config)?);
        self.reconnect_required = false;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn connect_to_nonexistent_port_returns_error() {
        let config = TcpClientConfig::new("127.0.0.1:1".parse().unwrap());
        let result = TcpClientInterface::open(&config);
        let msg = result
            .err()
            .expect("expected error connecting to port 1")
            .to_string();
        assert!(msg.contains("127.0.0.1:1"));
    }

    #[test]
    fn send_to_local_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let config = TcpClientConfig::new(addr);
        let mut conn = TcpClientInterface::open(&config).unwrap();

        let (mut server_stream, _) = listener.accept().unwrap();

        conn.send(b"ping").unwrap();

        let mut buf = [0u8; 16];
        use std::io::Read;
        server_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let n = server_stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
    }

    #[test]
    fn open_sets_nodelay_and_write_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let conn = TcpClientInterface::open(&TcpClientConfig::new(addr)).unwrap();
        let stream = conn.stream.as_ref().unwrap();
        assert!(stream.nodelay().unwrap());
        assert_eq!(stream.write_timeout().unwrap(), Some(WRITE_TIMEOUT));
    }

    #[test]
    fn a_retry_reconnects_only_after_a_failed_write() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = TcpClientConfig::new(listener.local_addr().unwrap());
        let current = InterfaceConfig::TcpClient(config.clone());
        let mut conn = TcpClientInterface::open(&config).unwrap();
        let _first = listener.accept().unwrap();
        assert!(
            !conn.prepare_retry(Some(&current)).unwrap(),
            "a healthy stream is kept"
        );

        conn.reconnect_required = true;
        assert!(
            conn.prepare_retry(Some(&current)).unwrap(),
            "connected again"
        );
        let (mut second, _) = listener.accept().unwrap();
        conn.send(b"next").unwrap();
        let mut buf = [0u8; 4];
        use std::io::Read;
        second
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        second.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"next", "the next message goes on the new stream");
    }

    #[test]
    fn a_retry_without_its_address_is_talker_disagreeing_with_itself() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut conn =
            TcpClientInterface::open(&TcpClientConfig::new(listener.local_addr().unwrap()))
                .unwrap();
        conn.reconnect_required = true;
        let error = conn.prepare_retry(None).unwrap_err();
        assert!(error.is::<MissingRetryConfiguration>());
    }

    #[test]
    fn send_large_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let config = TcpClientConfig::new(addr);
        let mut conn = TcpClientInterface::open(&config).unwrap();

        let (mut server_stream, _) = listener.accept().unwrap();
        let payload = vec![0xABu8; 4096];

        conn.send(&payload).unwrap();

        let mut received = Vec::new();
        server_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut server_stream, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
            if received.len() >= payload.len() {
                break;
            }
        }
        assert_eq!(received, payload);
    }
}
