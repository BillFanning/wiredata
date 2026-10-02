use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::Context;

use super::config::{InterfaceConfig, TcpClientConfig};
use super::{write_counted, Interface, MissingRetryConfiguration, WriteFailure};

/// Cap on connect. Without it the OS default applies (~20s on Windows),
/// which is far too long for an interactive tool to sit unresponsive.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on each blocking write. Without it, a peer that stops reading
/// (dead device, full window) wedges the owning talker thread inside
/// `write_all` forever — it can then never see its Stop command.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The most one drain reads before a write (ADR-059, §4.5). A peer that keeps
/// sending could otherwise hold up the schedule; what is left over is not
/// lost, it waits for the next send's drain.
const DRAIN_LIMIT: usize = 64 * 1024;

/// A drain read end-of-stream: the peer closed the connection. The send fails
/// with nothing written, rather than writing into a connection whose peer
/// has gone and counting the message as sent.
#[derive(Debug)]
struct PeerClosed;

impl std::fmt::Display for PeerClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the peer closed the connection")
    }
}

impl std::error::Error for PeerClosed {}

/// A write the peer took nothing of for [`WRITE_TIMEOUT`]. Said in talker's
/// words, since Windows describes this timeout as a failed connection attempt.
#[derive(Debug)]
struct WriteTimedOut;

impl std::fmt::Display for WriteTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the peer took no data for {} s (write timed out)",
            WRITE_TIMEOUT.as_secs()
        )
    }
}

impl std::error::Error for WriteTimedOut {}

/// The error for a failed write (§4.4, §4.5). Where `timeouts_hide_sent_bytes`
/// — on Windows, which reports a timed-out write as sending nothing but
/// leaves the connection undetermined — a write that timed out is possibly
/// partial even with no byte counted.
fn write_error(failure: WriteFailure, timeouts_hide_sent_bytes: bool) -> anyhow::Error {
    // Unix reports a write timeout as would-block, Windows as timed out. The
    // stream blocks outside a drain, so would-block means nothing else here.
    if matches!(
        failure.io.kind(),
        ErrorKind::TimedOut | ErrorKind::WouldBlock
    ) {
        failure.into_error_as(
            anyhow::Error::new(WriteTimedOut),
            "writing to TCP stream",
            timeouts_hide_sent_bytes,
        )
    } else {
        failure.into_error("writing to TCP stream")
    }
}

pub(super) struct TcpClientInterface {
    // `None` only between a failed stream being dropped and a later retry
    // connecting again.
    stream: Option<TcpStream>,
    /// The last write failed, so this stream is not used again (ADR-059): the
    /// peer may be gone, or may hold part of a message the next one would be
    /// appended to. The next retry point connects afresh instead.
    reconnect_required: bool,
    /// Bytes the peer sent, read and discarded, that the runner has not taken.
    peer_bytes: u64,
}

impl TcpClientInterface {
    pub(super) fn open(config: &TcpClientConfig) -> anyhow::Result<Self> {
        Ok(Self {
            stream: Some(connect(config)?),
            reconnect_required: false,
            peer_bytes: 0,
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

/// Read and discard whatever the peer has sent, without blocking, up to
/// [`DRAIN_LIMIT`] (ADR-059, §4.5). Unread replies would fill the receive
/// buffer, and closing a socket with unread data makes most stacks reset the
/// connection, which can discard the peer's own in-flight data. Returns the
/// bytes read, and the error that ends this connection, if one did.
fn drain(stream: &mut TcpStream) -> (u64, Option<anyhow::Error>) {
    if let Err(error) = stream.set_nonblocking(true) {
        return (
            0,
            Some(anyhow::Error::new(error).context("reading the peer's replies")),
        );
    }
    let mut buf = [0u8; 8 * 1024];
    let mut drained = 0;
    let mut ended = None;
    while drained < DRAIN_LIMIT {
        match stream.read(&mut buf) {
            Ok(0) => {
                ended = Some(anyhow::Error::new(PeerClosed));
                break;
            }
            Ok(n) => drained += n,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                ended = Some(anyhow::Error::new(error).context("reading the peer's replies"));
                break;
            }
        }
    }
    if let Err(error) = stream.set_nonblocking(false) {
        ended
            .get_or_insert_with(|| anyhow::Error::new(error).context("reading the peer's replies"));
    }
    (drained as u64, ended)
}

impl Interface for TcpClientInterface {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let stream = self.stream.as_mut().context("not connected")?;
        let (drained, ended) = drain(stream);
        self.peer_bytes += drained;
        if let Some(error) = ended {
            self.reconnect_required = true;
            return Err(error);
        }
        if let Err(failure) = write_counted(stream, data) {
            self.reconnect_required = true;
            return Err(write_error(failure, cfg!(windows)));
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

    fn take_peer_bytes(&mut self) -> u64 {
        std::mem::take(&mut self.peer_bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::super::PartialWrite;
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
    fn replies_are_drained_and_counted_before_each_write() {
        use std::io::Write;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut conn =
            TcpClientInterface::open(&TcpClientConfig::new(listener.local_addr().unwrap()))
                .unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.write_all(b"ACK\r\n").unwrap();

        // The reply reaches talker's socket in its own time: send until a
        // drain has counted it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut counted = 0;
        while counted < 5 {
            assert!(std::time::Instant::now() < deadline, "reply never drained");
            conn.send(b"x").unwrap();
            counted += conn.take_peer_bytes();
        }
        assert_eq!(counted, 5);
        assert_eq!(conn.take_peer_bytes(), 0, "taken once");
    }

    #[test]
    fn a_peer_that_closed_fails_the_send_with_nothing_written() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut conn =
            TcpClientInterface::open(&TcpClientConfig::new(listener.local_addr().unwrap()))
                .unwrap();
        // The peer closes with nothing unread, so it sends an orderly close;
        // give that a moment to arrive before the first send.
        drop(listener.accept().unwrap());
        std::thread::sleep(Duration::from_millis(200));

        let error = conn.send(b"x").unwrap_err();
        assert!(error.is::<PeerClosed>(), "{error:#}");
        assert_eq!(error.to_string(), "the peer closed the connection");
        assert!(conn.reconnect_required, "the next retry point reconnects");
    }

    fn failure(kind: std::io::ErrorKind, written: usize, total: usize) -> WriteFailure {
        WriteFailure {
            io: kind.into(),
            written,
            total,
        }
    }

    #[test]
    fn a_write_timeout_is_said_in_talkers_words() {
        // Unix reports a write timeout as would-block, Windows as timed out,
        // and Windows words it as a failed connection attempt.
        for kind in [std::io::ErrorKind::TimedOut, std::io::ErrorKind::WouldBlock] {
            let error = write_error(failure(kind, 0, 40), false);
            assert_eq!(
                format!("{error:#}"),
                "writing to TCP stream: the peer took no data for 5 s (write timed out)"
            );
            assert!(error.downcast_ref::<PartialWrite>().is_none(), "{error:#}");
        }
    }

    #[test]
    fn a_timeout_that_hides_what_was_sent_is_possibly_partial() {
        // Windows reports nothing sent, but part of the message may have gone.
        let error = write_error(failure(std::io::ErrorKind::TimedOut, 0, 40), true);
        let partial = error
            .downcast_ref::<PartialWrite>()
            .expect("possibly partial");
        assert_eq!(
            partial.written, 0,
            "no byte is counted that was not reported"
        );
        assert_eq!(
            format!("{error:#}"),
            "the OS cannot say how much of the 40 bytes went before the write failed: \
             writing to TCP stream: the peer took no data for 5 s (write timed out)"
        );
    }

    #[test]
    fn a_timeout_after_part_was_accepted_is_possibly_partial_everywhere() {
        let error = write_error(failure(std::io::ErrorKind::WouldBlock, 12, 40), false);
        let partial = error
            .downcast_ref::<PartialWrite>()
            .expect("possibly partial");
        assert_eq!(partial.written, 12);
    }

    #[test]
    fn other_write_errors_keep_the_os_words() {
        let error = write_error(failure(std::io::ErrorKind::ConnectionReset, 0, 40), true);
        assert!(error.downcast_ref::<PartialWrite>().is_none(), "{error:#}");
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::ConnectionReset)
        );
        assert!(format!("{error:#}").starts_with("writing to TCP stream: "));
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
