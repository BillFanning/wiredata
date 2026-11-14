//! TCP listener and TCP connection transports (spec §16).
//!
//! These split along the §138 contract:
//! - [`BoundTcpListenerTransport`] is a [`ConnectionAcceptorRunner`]: it accepts
//!   clients and emits a [`NewConnection`] per accept. It is **not** a data
//!   source and does not receive application data itself (§16.1).
//! - [`TcpConnectionTransport`] is a [`DataTransportRunner`]: one per accepted
//!   client, reading that client's independent byte stream (§16.2, Model A).
//!
//! The runtime mints a fresh `ChannelId` for each accepted connection (§97.1)
//! and supervises the connection channels (see `runtime::tcp`). Connection
//! channels are runtime-only and never persisted (§16.3).
//!
//! Both run as Tokio tasks (async-native socket I/O, ADR-001). Binding is split
//! from running so a bind failure surfaces at Channel Start (§71).

use std::io;
use std::net::SocketAddr;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::core::{ChannelId, ChunkTime};

use super::{
    ConnectionAcceptorRunner, DataTransportRunner, NewConnection, ReceivedData,
    TransportJoinHandle, TransportOutcome,
};

/// Read buffer size for one stream read. The chunk boundary tracks OS buffering,
/// not content; the stream is appended verbatim and never reframed (ADR-010).
const READ_BUFFER: usize = 8192;

/// An unbound TCP listener description (§16.1, §76). Call [`bind`](Self::bind)
/// at Channel Start.
#[derive(Clone, Debug)]
pub struct TcpListenerTransport {
    channel_id: ChannelId,
    bind_addr: SocketAddr,
}

impl TcpListenerTransport {
    pub fn new(channel_id: ChannelId, bind_addr: SocketAddr) -> Self {
        Self {
            channel_id,
            bind_addr,
        }
    }

    /// Create the listening socket (§8.2). Fallible so the runtime can take
    /// Starting → Faulted on a bind failure (§9, §71).
    pub async fn bind(self) -> io::Result<BoundTcpListenerTransport> {
        let listener = TcpListener::bind(self.bind_addr).await?;
        Ok(BoundTcpListenerTransport {
            channel_id: self.channel_id,
            listener,
        })
    }
}

/// A bound TCP listening socket. Implements [`ConnectionAcceptorRunner`].
#[derive(Debug)]
pub struct BoundTcpListenerTransport {
    channel_id: ChannelId,
    listener: TcpListener,
}

impl BoundTcpListenerTransport {
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// The actual bound address (useful with an ephemeral port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

impl ConnectionAcceptorRunner for BoundTcpListenerTransport {
    fn run(self, out: Sender<NewConnection>, cancel: CancellationToken) -> TransportJoinHandle {
        let BoundTcpListenerTransport {
            channel_id,
            listener,
        } = self;
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return TransportOutcome::Cancelled,
                    res = listener.accept() => match res {
                        Ok((stream, remote_addr)) => {
                            let conn = NewConnection {
                                listener_channel_id: channel_id,
                                remote_addr,
                                accepted_at: ChunkTime::now(),
                                stream,
                            };
                            // The supervisor mints the ChannelId and decides
                            // accept/reject (max_connections, §16.1). An `Err`
                            // means the supervisor is gone.
                            if out.send(conn).await.is_err() {
                                return TransportOutcome::Completed;
                            }
                        }
                        Err(e) => {
                            return TransportOutcome::Faulted(format!("TCP accept failed: {e}"))
                        }
                    },
                }
            }
        });
        TransportJoinHandle::Task(handle)
    }
}

/// One accepted TCP client connection, read as an independent byte stream
/// (§16.2). Created by the runtime from a [`NewConnection`].
#[derive(Debug)]
pub struct TcpConnectionTransport {
    channel_id: ChannelId,
    stream: TcpStream,
}

impl TcpConnectionTransport {
    pub fn new(channel_id: ChannelId, stream: TcpStream) -> Self {
        Self { channel_id, stream }
    }

    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }
}

impl DataTransportRunner for TcpConnectionTransport {
    fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle {
        let TcpConnectionTransport {
            channel_id,
            mut stream,
        } = self;
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; READ_BUFFER];
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return TransportOutcome::Cancelled,
                    res = stream.read(&mut buf) => match res {
                        // EOF: the client closed the connection (§16, disconnect).
                        Ok(0) => return TransportOutcome::Completed,
                        Ok(n) => {
                            let received_at = ChunkTime::now();
                            let data = ReceivedData {
                                channel_id,
                                payload: buf[..n].to_vec(),
                                received_at,
                            };
                            // Awaiting `send` is the one place this transport may
                            // stall (§97.1); an `Err` means the pipeline is gone.
                            if out.send(data).await.is_err() {
                                return TransportOutcome::Completed;
                            }
                        }
                        Err(e) => return TransportOutcome::Faulted(format!("TCP read failed: {e}")),
                    },
                }
            }
        });
        TransportJoinHandle::Task(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn listener_emits_a_new_connection_per_client() {
        let bound = TcpListenerTransport::new(ChannelId::new(), "127.0.0.1:0".parse().unwrap())
            .bind()
            .await
            .unwrap();
        let addr = bound.local_addr().unwrap();
        let listener_id = bound.channel_id();

        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let handle = bound.run(tx, cancel.clone());

        let client = TcpStream::connect(addr).await.unwrap();
        let client_addr = client.local_addr().unwrap();

        let conn = rx.recv().await.unwrap();
        assert_eq!(conn.listener_channel_id, listener_id);
        assert_eq!(conn.remote_addr, client_addr);

        cancel.cancel();
        assert!(matches!(handle.join().await, TransportOutcome::Cancelled));
    }

    #[tokio::test]
    async fn connection_reads_stream_bytes_and_ends_on_eof() {
        // A real loopback pair gives us a server-side TcpStream to read.
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        let (server_stream, _) = server.accept().await.unwrap();

        let transport = TcpConnectionTransport::new(ChannelId::new(), server_stream);
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let handle = transport.run(tx, cancel.clone());

        client.write_all(b"chunk-one").await.unwrap();
        let data = rx.recv().await.unwrap();
        assert_eq!(data.payload, b"chunk-one");

        // Closing the client ends the connection transport (EOF), so the join
        // handle completes on its own — reported as a normal `Completed` outcome.
        drop(client);
        assert!(matches!(handle.join().await, TransportOutcome::Completed));
    }
}
