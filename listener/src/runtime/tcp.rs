//! TCP listener supervision (spec §16, §97.1).
//!
//! A TCP listener does not itself receive application data — it accepts clients
//! and the runtime turns each into an independent TCP connection channel (Model
//! A, §16.4). This supervisor task:
//!
//! - runs the listener acceptor and, per [`NewConnection`],
//!   mints a fresh `ChannelId` (§97.1) and starts a connection channel;
//! - enforces `max_connections` (§16.1): at the limit it closes the incoming
//!   connection and raises a `ConnectionRejected` warning (§93) without faulting
//!   the listener;
//! - emits `TcpClientConnected` / `TcpClientDisconnected` lifecycle events
//!   (§137), routing every connection's events into the same shared event stream;
//! - on stop, stops accepting and then stops every live connection (§110, §13).
//!
//! Connection channels are runtime-only and never persisted (§16.3).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::{self, Sender};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::core::{ChannelId, RuntimeEvent};
use crate::transport::tcp::TcpConnectionTransport;
use crate::transport::{ConnectionAcceptorRunner, NewConnection, TransportOutcome};

use super::channel::{
    report_transport_fault, spawn_channel_tasks, ChannelTasks, MatchSetup, TRANSPORT_NOTICES,
};
use super::pipeline::PipelineCapacities;

/// Per-connection state retained by the supervisor for shutdown and disconnect
/// handling. The transport join handle lives in the monitor `JoinSet`, not here.
struct Connection {
    transport_cancel: CancellationToken,
    pipeline_task: JoinHandle<super::pipeline::ChannelPipeline>,
    /// Kept so a connection fault's CAUSE reaches the pipeline's diagnostics
    /// before the pipeline drains and finishes (the `ChannelFaulted` event
    /// carries only the id).
    notice_tx: mpsc::Sender<crate::transport::TransportNotice>,
}

/// Handle to a running TCP listener and its connection channels.
pub struct TcpListenerHandle {
    listener_id: ChannelId,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl TcpListenerHandle {
    pub fn listener_id(&self) -> ChannelId {
        self.listener_id
    }

    /// Graceful shutdown (§110, §13): stop accepting, then stop all connections.
    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Start a TCP listener and supervise its connection channels (§16).
///
/// All lifecycle events flow into `events`. A spontaneous acceptor fault sets
/// `faulted` (the channel's polled-state flag, ADR-006) and emits
/// `ChannelFaulted(listener_id)` — the listener is then Faulted like any other
/// transport, so auto-reconnect can engage (§162). Existing connection
/// channels live on either way.
///
/// Per-connection **recording** is not wired: each connection would need a
/// distinct destination file, which depends on filename templating (§59,
/// deferred). Accepted connections are therefore unrecorded for now.
pub fn start_tcp_listener(
    listener_id: ChannelId,
    acceptor: impl ConnectionAcceptorRunner,
    caps: PipelineCapacities,
    max_connections: Option<u32>,
    faulted: Arc<AtomicBool>,
    events: Sender<RuntimeEvent>,
) -> TcpListenerHandle {
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();

    // Listener acceptor → supervisor queue of NewConnection. Run before the
    // spawn so the acceptor type itself never crosses into the task.
    let (conn_tx, mut conn_rx) = mpsc::channel::<NewConnection>(caps.ingest);
    let listener_cancel = CancellationToken::new();
    let mut listener_handle = Some(acceptor.run(conn_tx, listener_cancel.clone()));

    let task = tokio::spawn(async move {
        let mut connections: HashMap<ChannelId, Connection> = HashMap::new();
        // Each entry awaits a connection's transport completion and yields its id.
        let mut monitors: JoinSet<(ChannelId, TransportOutcome)> = JoinSet::new();
        let mut listener_open = true;

        loop {
            tokio::select! {
                biased;
                _ = task_cancel.cancelled() => break,

                maybe = conn_rx.recv(), if listener_open => match maybe {
                    Some(new_conn) => {
                        // max_connections (§16.1): reject at the limit without faulting.
                        if let Some(max) = max_connections {
                            if connections.len() as u32 >= max {
                                drop(new_conn.stream); // close the rejected connection
                                let _ = events.try_send(RuntimeEvent::WarningRaised(listener_id));
                                continue;
                            }
                        }

                        let conn_id = ChannelId::new(); // runtime mints (§97.1)
                        let transport = TcpConnectionTransport::new(conn_id, new_conn.stream);
                        // TCP is async and never stalls the reader (§97.1) —
                        // the sender is kept only for fault-cause notices.
                        let (notice_tx, notice_rx) = mpsc::channel(TRANSPORT_NOTICES);
                        let tasks = spawn_channel_tasks(
                            conn_id,
                            transport,
                            // Per-connection recording is deferred (distinct files
                            // per connection need §59 filename templates).
                            None,
                            None,
                            // One default Display View per connection.
                            1,
                            None, // per-connection recording (and its guard) deferred
                            // Per-connection Match Rules are deferred (§50.2): rules
                            // are per-listener config; wiring them per accepted
                            // connection follows the per-connection recording work.
                            MatchSetup::none(),
                            caps,
                            events.clone(),
                            notice_rx,
                            None,
                        );
                        let ChannelTasks {
                            transport: transport_join,
                            transport_cancel,
                            pipeline_task,
                            channel_id: _,
                            pipeline_cancel: _,
                            display_handles: _,
                            // Per-connection snapshots are deferred (no per-conn
                            // handle is retained); drop the request sender.
                            requests: _,
                        } = tasks;

                        // Detect disconnect: the transport task ends on EOF/cancel/fault.
                        monitors.spawn(async move {
                            let outcome = transport_join.join().await;
                            (conn_id, outcome)
                        });
                        connections.insert(
                            conn_id,
                            Connection {
                                transport_cancel,
                                pipeline_task,
                                notice_tx,
                            },
                        );
                        let _ = events.try_send(RuntimeEvent::TcpClientConnected(conn_id));
                    }
                    None => {
                        // Acceptor ended on its own (not our shutdown cancel);
                        // existing connections live on, but the listener no
                        // longer accepts — surface a fault as any transport
                        // would (§162), instead of sitting silently "running".
                        listener_open = false;
                        if let Some(handle) = listener_handle.take() {
                            if let TransportOutcome::Faulted(cause) = handle.join().await {
                                tracing::error!("TCP listener acceptor faulted: {cause}");
                                faulted.store(true, Ordering::Relaxed);
                                let _ = events.try_send(RuntimeEvent::ChannelFaulted(listener_id));
                            }
                        }
                    }
                },

                Some(joined) = monitors.join_next(), if !monitors.is_empty() => {
                    if let Ok((conn_id, outcome)) = joined {
                        if let Some(conn) = connections.remove(&conn_id) {
                            let Connection {
                                transport_cancel: _,
                                pipeline_task,
                                notice_tx,
                            } = conn;
                            // A read fault is reported before the disconnect
                            // (§94/§101). The cause goes to the pipeline as a
                            // notice BEFORE the drain below, so it lands in the
                            // connection's diagnostics rather than dying here.
                            if let TransportOutcome::Faulted(cause) = outcome {
                                report_transport_fault(&notice_tx, conn_id, cause).await;
                                let _ = events.try_send(RuntimeEvent::ChannelFaulted(conn_id));
                            }
                            // The pipeline runs until BOTH ingest and notices
                            // close — drop our sender before awaiting it.
                            drop(notice_tx);
                            // Reception ended; drain the accepted backlog (§110).
                            let _ = pipeline_task.await;
                            let _ = events.try_send(RuntimeEvent::TcpClientDisconnected(conn_id));
                        }
                    }
                }
            }
        }

        // Shutdown (§110, §13): stop accepting, then stop every connection.
        listener_cancel.cancel();
        if let Some(handle) = listener_handle.take() {
            let _ = handle.join().await;
        }
        for (conn_id, conn) in connections.drain() {
            let Connection {
                transport_cancel,
                pipeline_task,
                notice_tx,
            } = conn;
            transport_cancel.cancel();
            // The pipeline runs until BOTH its ingest and notices close —
            // holding this sender across the await would deadlock the drain.
            drop(notice_tx);
            let _ = pipeline_task.await;
            let _ = events.try_send(RuntimeEvent::TcpClientDisconnected(conn_id));
        }
        monitors.shutdown().await;
    });

    TcpListenerHandle {
        listener_id,
        cancel,
        task,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tcp::{BoundTcpListenerTransport, TcpListenerTransport};
    use crate::transport::TransportJoinHandle;
    use std::collections::HashSet;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    async fn bound_listener() -> BoundTcpListenerTransport {
        TcpListenerTransport::new(ChannelId::new(), "127.0.0.1:0".parse().unwrap())
            .bind()
            .await
            .unwrap()
    }

    fn start(
        bound: BoundTcpListenerTransport,
        max_connections: Option<u32>,
        events: Sender<RuntimeEvent>,
    ) -> (TcpListenerHandle, Arc<AtomicBool>) {
        let faulted = Arc::new(AtomicBool::new(false));
        let handle = start_tcp_listener(
            bound.channel_id(),
            bound,
            PipelineCapacities::default(),
            max_connections,
            faulted.clone(),
            events,
        );
        (handle, faulted)
    }

    #[tokio::test]
    async fn connection_lifecycle_emits_connected_and_disconnected() {
        let bound = bound_listener().await;
        let addr = bound.local_addr().unwrap();
        let listener_id = bound.channel_id();
        let (ev_tx, mut ev_rx) = mpsc::channel(64);

        let (handle, _faulted) = start(bound, None, ev_tx);

        let mut client = TcpStream::connect(addr).await.unwrap();

        let conn_id = match ev_rx.recv().await.unwrap() {
            RuntimeEvent::TcpClientConnected(id) => {
                assert_ne!(id, listener_id);
                id
            }
            other => panic!("expected TcpClientConnected, got {other:?}"),
        };

        // Stream-only (ADR-010): bytes are ingested verbatim and do not raise a
        // per-message event. Writing exercises the connection channel's ingest
        // path; the connection stays healthy until the client disconnects.
        client.write_all(b"hello").await.unwrap();

        drop(client); // client disconnects → EOF
        match ev_rx.recv().await.unwrap() {
            RuntimeEvent::TcpClientDisconnected(id) => assert_eq!(id, conn_id),
            other => panic!("expected TcpClientDisconnected, got {other:?}"),
        }

        handle.stop().await;
    }

    #[tokio::test]
    async fn max_connections_rejects_and_warns_without_faulting() {
        let bound = bound_listener().await;
        let addr = bound.local_addr().unwrap();
        let listener_id = bound.channel_id();
        let (ev_tx, mut ev_rx) = mpsc::channel(64);

        let (handle, _faulted) = start(bound, Some(1), ev_tx);

        // First client accepted.
        let _c1 = TcpStream::connect(addr).await.unwrap();
        assert!(matches!(
            ev_rx.recv().await.unwrap(),
            RuntimeEvent::TcpClientConnected(_)
        ));

        // Second client over the limit: rejected with a warning on the listener.
        let _c2 = TcpStream::connect(addr).await.unwrap();
        match ev_rx.recv().await.unwrap() {
            RuntimeEvent::WarningRaised(id) => assert_eq!(id, listener_id),
            other => panic!("expected WarningRaised, got {other:?}"),
        }

        handle.stop().await;
    }

    /// An acceptor that fails immediately, without touching a socket —
    /// injected in place of the bound listener to exercise the fault path.
    struct FailingAcceptor {
        cause: &'static str,
    }

    impl ConnectionAcceptorRunner for FailingAcceptor {
        fn run(
            self,
            out: mpsc::Sender<NewConnection>,
            _cancel: CancellationToken,
        ) -> TransportJoinHandle {
            let cause = self.cause;
            TransportJoinHandle::Task(tokio::spawn(async move {
                drop(out); // acceptor gone → the supervisor's queue closes
                TransportOutcome::Faulted(cause.to_string())
            }))
        }
    }

    #[tokio::test]
    async fn acceptor_fault_sets_faulted_and_emits_channel_faulted() {
        // Regression (§162): a spontaneously dying acceptor used to only flip
        // an internal flag — the listener kept reading as Running, accepted
        // nothing, and auto-reconnect never engaged.
        let listener_id = ChannelId::new();
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let faulted = Arc::new(AtomicBool::new(false));
        let handle = start_tcp_listener(
            listener_id,
            FailingAcceptor {
                cause: "injected accept failure",
            },
            PipelineCapacities::default(),
            None,
            faulted.clone(),
            ev_tx,
        );

        match ev_rx.recv().await.unwrap() {
            RuntimeEvent::ChannelFaulted(id) => assert_eq!(id, listener_id),
            other => panic!("expected ChannelFaulted, got {other:?}"),
        }
        assert!(faulted.load(Ordering::Relaxed), "polled-state flag set");

        handle.stop().await;
    }

    #[tokio::test]
    async fn connections_are_independent_channels() {
        let bound = bound_listener().await;
        let addr = bound.local_addr().unwrap();
        let (ev_tx, mut ev_rx) = mpsc::channel(128);

        let (handle, _faulted) = start(bound, None, ev_tx);

        let mut c1 = TcpStream::connect(addr).await.unwrap();
        let mut c2 = TcpStream::connect(addr).await.unwrap();
        c1.write_all(b"a").await.unwrap();
        c2.write_all(b"b").await.unwrap();

        // Each accepted client becomes its own connection channel (Model A, §16.4):
        // two clients yield two distinct connection ids.
        let mut connected: HashSet<ChannelId> = HashSet::new();
        while connected.len() < 2 {
            if let RuntimeEvent::TcpClientConnected(id) = ev_rx.recv().await.unwrap() {
                connected.insert(id);
            }
        }

        assert_eq!(connected.len(), 2, "two distinct connection channels");

        handle.stop().await;
    }
}
