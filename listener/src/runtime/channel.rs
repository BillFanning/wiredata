//! Channel orchestration: wiring a transport to its pipeline and driving the
//! Channel lifecycle (spec §10, §97, §110, §111).
//!
//! `spawn_monitored_channel` is the runtime's per-Channel setup for a single
//! data-bearing transport (Serial, UDP, or an accepted TCP connection): it
//! creates the bounded Transport→Pipeline queue (§97.2), spawns the pipeline
//! task, hands the transport its `out` sender, and adds the fault monitor. The
//! returned `MonitoredChannel` owns the cancellation tokens and join handles
//! for shutdown.
//!
//! `spawn_channel_tasks` is the lower-level primitive: it takes an externally
//! supplied event sender (so many channels can share one event stream, as the
//! TCP listener supervisor does) and returns the raw `ChannelTasks`. A
//! test-only `start_data_channel`/`RunningChannel` wrapper runs one standalone
//! channel with its own event receiver, so the spawn/monitor/stop paths are
//! testable without a `Listener` registry.
//!
//! Two shutdown paths:
//! - graceful (§110): stop reception first, then let the pipeline drain the
//!   already-queued accepted data before finishing.
//! - forced (§111, §113): cancel both halves at once without guaranteeing the
//!   backlog is drained.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::MatchRule;
use crate::core::{ChannelId, RuntimeEvent};
use crate::display::{DisplayView, RenderedOutput};
use crate::record::Recording;
use crate::transport::{
    DataTransportRunner, ReceivedData, SerialStallState, TransportJoinHandle, TransportNotice,
    TransportOutcome,
};

use super::pipeline::{
    run_channel, ChannelPipeline, DisplayRecordingSettings, DisplayViewHandle, PipelineCapacities,
    RawRecordingSettings,
};
use super::snapshot::{ChannelSnapshot, ChannelStats, PipelineRequest, StreamDelta};

/// Match Rule wiring for a Channel (§50.2, §165): the compiled rules plus the
/// optional Raw recording settings a `Record` action needs to lazily create a
/// recording. Bundled so the long `spawn_*` signatures gain one parameter, not two.
pub(crate) struct MatchSetup {
    pub(crate) rules: Vec<MatchRule>,
    pub(crate) recording_settings: Option<RawRecordingSettings>,
    /// Display sibling of `recording_settings` (§50.2/§54): what a match-triggered
    /// `Record { target: Display | Both }` needs to lazily create the `.disp`.
    pub(crate) display_recording_settings: Option<DisplayRecordingSettings>,
    /// "Record on start" (§53): the pipeline begins recording at startup. Set when the
    /// channel's `raw_recording.enabled` is true and a destination is configured.
    pub(crate) auto_begin_recording: bool,
    /// "Record on start" for the Display recording (§54): set from the
    /// channel's `display_recording.enabled`.
    pub(crate) auto_begin_display_recording: bool,
    /// The previous run's diagnostics, so a restarted Channel keeps its log across a
    /// stop/start within a session (§88). Empty for a first start.
    pub(crate) prior_diagnostics: Vec<crate::diagnostics::Diagnostic>,
    /// The configured Channel name, for diagnostics and the event log (§118).
    /// `None` names the Channel by its UUID (a TCP connection has no name).
    pub(crate) channel_name: Option<String>,
}

impl MatchSetup {
    /// No Match Rules (standalone and per-connection channels for now).
    pub(crate) fn none() -> Self {
        Self {
            rules: Vec::new(),
            recording_settings: None,
            display_recording_settings: None,
            auto_begin_recording: false,
            auto_begin_display_recording: false,
            prior_diagnostics: Vec::new(),
            channel_name: None,
        }
    }
}

/// Bounded request channel for on-demand snapshots (§137, ADR-006). Tiny: a
/// requester sends a oneshot reply and awaits; the pipeline answers between reads.
const SNAPSHOT_REQUESTS: usize = 8;

/// Bounded capacity for the transport-notice channel (§95, §101, ADR-007).
///
/// Advisory stall notices are bounded and use `try_send` (§99): blocking the live
/// reader to announce a reader stall would cause the very stall being reported.
/// The capacity is large enough for a short burst; a dropped-advisory counter can
/// be added if the rate ever matters. Every data-transport monitor also retains a
/// sender for its one terminal fault cause; that post-reception path awaits capacity
/// through [`report_transport_fault`] instead of dropping (ADR-020).
pub(crate) const TRANSPORT_NOTICES: usize = 16;

/// Deliver a terminal transport fault to the pipeline after reception has ended.
///
/// Unlike advisory notices emitted from a live receive loop, this path may await
/// bounded capacity: no reader remains to stall, and losing the terminal cause
/// would leave the retained diagnostics unable to explain the faulted state.
pub(crate) async fn report_transport_fault(
    notices: &Sender<TransportNotice>,
    channel_id: ChannelId,
    cause: String,
) {
    let _ = notices
        .send(TransportNotice::TransportFaulted { channel_id, cause })
        .await;
}

/// The transport + pipeline tasks for one Channel. A plain holder, destructured
/// by [`spawn_monitored_channel`] and the TCP listener supervisor (which each do
/// their own transport-outcome monitoring).
pub(crate) struct ChannelTasks {
    pub(crate) channel_id: ChannelId,
    pub(crate) transport_cancel: CancellationToken,
    pub(crate) pipeline_cancel: CancellationToken,
    pub(crate) transport: TransportJoinHandle,
    pub(crate) pipeline_task: JoinHandle<ChannelPipeline>,
    pub(crate) display_handles: Vec<DisplayViewHandle>,
    /// Sender for snapshot/stats requests served by the pipeline task while it runs.
    pub(crate) requests: Sender<PipelineRequest>,
}

/// Wire a bound data-bearing transport to a fresh pipeline and start both tasks,
/// emitting events into the supplied sender (§97.2, §102, §137).
// Internal wiring with distinct, meaningful per-channel inputs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_channel_tasks<R: DataTransportRunner>(
    channel_id: ChannelId,
    runner: R,
    // A pre-built byte-exact Raw recorder (§53); production channels leave this
    // `None` and begin recording lazily via the recording settings (§55).
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
    display_recorder: Option<(DisplayView, Recording<RenderedOutput>)>,
    view_count: usize,
    match_setup: MatchSetup,
    caps: PipelineCapacities,
    events: Sender<RuntimeEvent>,
    notices_rx: Receiver<TransportNotice>,
    serial_stall_state: Option<SerialStallState>,
) -> ChannelTasks {
    // The bounded Transport→Pipeline queue — the only edge that may stall the
    // reader (§97.1, §99). No unbounded intermediate queue is introduced (§97.2).
    let (ingest_tx, ingest_rx) = mpsc::channel(caps.ingest);

    let mut pipeline = ChannelPipeline::new(channel_id, caps).with_event_sender(events);
    if let Some(name) = match_setup.channel_name {
        pipeline = pipeline.with_channel_name(name);
    }
    if let Some(state) = serial_stall_state {
        pipeline = pipeline.with_serial_stall_state(state);
    }
    // Carry the previous run's diagnostics forward (§88) so a restart keeps its log,
    // before the pipeline records anything new.
    if !match_setup.prior_diagnostics.is_empty() {
        pipeline = pipeline.with_prior_diagnostics(match_setup.prior_diagnostics);
    }
    // Match Rules (§50.2, §165): compile the rules and supply any Raw recording
    // settings a match-triggered `Record` needs before the pipeline starts.
    pipeline = pipeline.with_match_rules(&match_setup.rules);
    if let Some(settings) = match_setup.recording_settings {
        pipeline = pipeline.with_recording_settings(settings);
    }
    if let Some(settings) = match_setup.display_recording_settings {
        pipeline = pipeline.with_display_recording_settings(settings);
    }
    // "Record on start": the pipeline begins recording at startup (via the same path
    // as the live toggle), so it never pre-builds the recorder for this case.
    if match_setup.auto_begin_recording {
        pipeline = pipeline.with_auto_begin_recording();
    }
    if match_setup.auto_begin_display_recording {
        pipeline = pipeline.with_auto_begin_display_recording();
    }
    if let Some(r) = raw_recorder {
        pipeline = pipeline.with_raw_recorder(r);
    }
    // `new` created the default Display View; add the rest to reach the count (§48).
    for _ in 1..view_count.max(1) {
        pipeline.add_display_view();
    }
    if let Some((renderer, recording)) = display_recorder {
        pipeline.set_display_recorder(renderer, recording);
    }
    let display_handles = pipeline.display_view_handles();

    let transport_cancel = CancellationToken::new();
    let pipeline_cancel = CancellationToken::new();
    let (requests, request_rx) = mpsc::channel(SNAPSHOT_REQUESTS);

    let transport = runner.run(ingest_tx, transport_cancel.clone());
    let pipeline_task = tokio::spawn(run_channel(
        ingest_rx,
        request_rx,
        notices_rx,
        pipeline,
        pipeline_cancel.clone(),
    ));

    ChannelTasks {
        channel_id,
        transport_cancel,
        pipeline_cancel,
        transport,
        pipeline_task,
        display_handles,
        requests,
    }
}

/// A data Channel's controls plus a fault monitor. The transport's join handle
/// lives in the monitor task — which emits `ChannelFaulted` if the transport
/// ends on a spontaneous fault (§94/§101) — so `stop`/`abort` synchronize on the
/// pipeline draining rather than joining the transport directly.
pub(crate) struct MonitoredChannel {
    /// Read only by the test-only standalone wrapper (`RunningChannel`).
    #[cfg_attr(not(test), allow(dead_code))]
    channel_id: ChannelId,
    transport_cancel: CancellationToken,
    /// Cancels the pipeline half without draining — the §111 forced-stop path,
    /// reached only through the test-only [`abort`](Self::abort) today
    /// (production shutdown bounds the graceful drain instead, §113).
    #[cfg_attr(not(test), allow(dead_code))]
    pipeline_cancel: CancellationToken,
    pipeline_task: JoinHandle<ChannelPipeline>,
    monitor: JoinHandle<()>,
    display_handles: Vec<DisplayViewHandle>,
    requests: Sender<PipelineRequest>,
}

impl MonitoredChannel {
    /// Pause/resume handles for this Channel's Display Views (§11, §48).
    pub(crate) fn display_handles(&self) -> &[DisplayViewHandle] {
        &self.display_handles
    }

    /// Request an on-demand snapshot of the running pipeline (§137, ADR-006).
    /// Returns `None` if the pipeline task has already ended (e.g. after a fault
    /// or stop) — the event stream remains the authoritative liveness signal.
    pub(crate) async fn snapshot(&self) -> Option<ChannelSnapshot> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.requests
            .send(PipelineRequest::Snapshot(reply_tx))
            .await
            .ok()?;
        reply_rx.await.ok()
    }

    /// Cheap O(1) liveness stats for a multi-channel overview (§91.1, ADR-006) —
    /// no stream-buffer copy. `None` if the pipeline task has already ended.
    pub(crate) async fn stats(&self) -> Option<ChannelStats> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.requests
            .send(PipelineRequest::Stats(reply_tx))
            .await
            .ok()?;
        reply_rx.await.ok()
    }

    /// Incremental stream bytes since `since` (§87, ADR-009): only what is new, so
    /// a live viewer never re-ships the whole scrollback. `None` if the pipeline
    /// task has already ended.
    pub(crate) async fn stream_delta(&self, since: u64) -> Option<StreamDelta> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.requests
            .send(PipelineRequest::StreamDelta {
                since,
                reply: reply_tx,
            })
            .await
            .ok()?;
        reply_rx.await.ok()
    }

    /// Begin/stop Raw recording live, without a restart (§50.2, ADR-012). Fire-and-
    /// forget: returns `true` if the command reached the pipeline, `false` if the
    /// task has already ended. The outcome is observed via the next snapshot's
    /// recording state (and a `RecordingFaulted` event on a begin failure, §55).
    pub(crate) async fn set_recording(
        &self,
        enabled: bool,
        settings: Option<RawRecordingSettings>,
    ) -> bool {
        self.requests
            .send(PipelineRequest::SetRecording { enabled, settings })
            .await
            .is_ok()
    }

    /// Begin/stop **Display** recording live (§54, ADR-012) — the Raw variant's
    /// sibling; same fire-and-forget contract.
    pub(crate) async fn set_display_recording(
        &self,
        enabled: bool,
        settings: Option<DisplayRecordingSettings>,
    ) -> bool {
        self.requests
            .send(PipelineRequest::SetDisplayRecording { enabled, settings })
            .await
            .is_ok()
    }

    /// Graceful stop (§110): stop reception; the transport's sender drops, the
    /// pipeline drains the accepted backlog and returns, and the monitor ends.
    /// `None` if the pipeline task panicked — its final state (diagnostics,
    /// snapshot) is unrecoverable, but the stop itself still completes rather
    /// than propagating the panic into the orchestrator.
    pub(crate) async fn stop(self) -> Option<ChannelPipeline> {
        self.transport_cancel.cancel();
        let pipeline = match self.pipeline_task.await {
            Ok(pipeline) => Some(pipeline),
            Err(join_err) => {
                tracing::error!("channel pipeline task failed during stop: {join_err}");
                None
            }
        };
        let _ = self.monitor.await;
        pipeline
    }

    /// Forced stop (§111, §113): cancel both halves; the backlog may be abandoned.
    /// Test-only today — production shutdown prefers the graceful [`stop`](Self::stop)
    /// under a timeout (`Listener::shutdown`), never an outright abort.
    #[cfg(test)]
    pub(crate) async fn abort(self) -> ChannelPipeline {
        self.transport_cancel.cancel();
        self.pipeline_cancel.cancel();
        let pipeline = self
            .pipeline_task
            .await
            .expect("pipeline task should not panic");
        let _ = self.monitor.await;
        pipeline
    }
}

/// Like [`spawn_channel_tasks`] but adds a fault monitor that awaits the
/// transport outcome and emits `ChannelFaulted` if it ended on a fault (§94,
/// §101). Used for standalone and orchestrated data channels; the TCP supervisor
/// does its own per-connection monitoring instead.
///
/// `faulted` is the shared per-channel fault flag (listener ADR-006): the
/// detached monitor cannot mutate the method-based orchestrator's state, so it
/// flips this flag on a spontaneous fault. The orchestrator reads it to keep
/// `state()` and command validation honest; observers learn of the fault through
/// the `ChannelFaulted` event, which is authoritative for presentation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_monitored_channel<R: DataTransportRunner>(
    channel_id: ChannelId,
    runner: R,
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
    display_recorder: Option<(DisplayView, Recording<RenderedOutput>)>,
    view_count: usize,
    match_setup: MatchSetup,
    caps: PipelineCapacities,
    events: Sender<RuntimeEvent>,
    faulted: Arc<AtomicBool>,
    serial_stall_state: Option<SerialStallState>,
    // Both ends: the receiver feeds the pipeline; the monitor keeps the
    // sender so a spontaneous fault's CAUSE reaches the diagnostics log
    // (serial transports hold their own clone for stall notices).
    notices: (Sender<TransportNotice>, Receiver<TransportNotice>),
) -> MonitoredChannel {
    let (notices_tx, notices_rx) = notices;
    let monitor_events = events.clone();
    let ChannelTasks {
        channel_id,
        transport_cancel,
        pipeline_cancel,
        transport,
        pipeline_task,
        display_handles,
        requests,
    } = spawn_channel_tasks(
        channel_id,
        runner,
        raw_recorder,
        display_recorder,
        view_count,
        match_setup,
        caps,
        events,
        notices_rx,
        serial_stall_state,
    );

    let monitor = tokio::spawn(async move {
        // A spontaneous fault surfaces as ChannelFaulted; cancel/EOF/pipeline-gone
        // outcomes are normal ends and emit nothing. The flag reconciles the
        // orchestrator's internal state (ADR-006); the event informs observers;
        // the CAUSE goes to the pipeline as a notice so it lands in the
        // channel's diagnostics (and survives stop via the retained log) —
        // the string used to die unread here.
        if let TransportOutcome::Faulted(cause) = transport.join().await {
            faulted.store(true, Ordering::Relaxed);
            report_transport_fault(&notices_tx, channel_id, cause).await;
            let _ = monitor_events.try_send(RuntimeEvent::ChannelFaulted(channel_id));
        }
    });

    MonitoredChannel {
        channel_id,
        transport_cancel,
        pipeline_cancel,
        pipeline_task,
        monitor,
        display_handles,
        requests,
    }
}

/// A live, standalone Channel: its tasks plus its own event receiver. Returned
/// by [`start_data_channel`].
///
/// Test-only: production channels are orchestrated through
/// [`Listener`](super::Listener) (the CLI and the GUI driver both go through
/// it); this standalone wrapper exists so the spawn/monitor/stop paths can be
/// exercised directly, without a registry.
#[cfg(test)]
pub struct RunningChannel {
    tasks: MonitoredChannel,
    events: mpsc::Receiver<RuntimeEvent>,
    faulted: Arc<AtomicBool>,
}

#[cfg(test)]
impl RunningChannel {
    pub fn channel_id(&self) -> ChannelId {
        self.tasks.channel_id
    }

    /// Whether the transport has ended on a spontaneous fault (§94/§101). Set by
    /// the fault monitor; the `ChannelFaulted` event fires at the same time.
    pub fn is_faulted(&self) -> bool {
        self.faulted.load(Ordering::Relaxed)
    }

    /// Request an on-demand snapshot of this Channel's pipeline state (§137,
    /// ADR-006). `None` once the pipeline has ended.
    pub async fn snapshot(&self) -> Option<ChannelSnapshot> {
        self.tasks.snapshot().await
    }

    /// Cheap liveness stats (§91.1, ADR-006). `None` once the pipeline has ended.
    pub async fn stats(&self) -> Option<ChannelStats> {
        self.tasks.stats().await
    }

    /// Incremental stream bytes since `since` (§87, ADR-009). `None` once the
    /// pipeline has ended.
    pub async fn stream_delta(&self, since: u64) -> Option<StreamDelta> {
        self.tasks.stream_delta(since).await
    }

    /// Begin/stop Raw recording live, without a restart (§50.2, ADR-012); `settings`
    /// apply the on-screen recording config first. `false` once the pipeline has ended.
    pub async fn set_recording(
        &self,
        enabled: bool,
        settings: Option<RawRecordingSettings>,
    ) -> bool {
        self.tasks.set_recording(enabled, settings).await
    }

    /// The runtime→UI event stream for this Channel (§137).
    pub fn events(&mut self) -> &mut mpsc::Receiver<RuntimeEvent> {
        &mut self.events
    }

    /// Graceful stop (§110). `None` if the pipeline task panicked (its final
    /// state is unrecoverable, but the stop still completes).
    pub async fn stop(self) -> Option<ChannelPipeline> {
        self.tasks.stop().await
    }

    /// Forced stop (§111, §113).
    pub async fn abort(self) -> ChannelPipeline {
        self.tasks.abort().await
    }
}

/// Start a standalone data channel (test-only — see [`RunningChannel`]).
#[cfg(test)]
pub fn start_data_channel<R: DataTransportRunner>(
    channel_id: ChannelId,
    runner: R,
    caps: PipelineCapacities,
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
) -> RunningChannel {
    let (event_tx, event_rx) = mpsc::channel(caps.events);
    let faulted = Arc::new(AtomicBool::new(false));
    // A standalone channel has no serial loss reporter; the monitor still
    // holds the sender for fault-cause notices.
    let (notice_tx, notice_rx) = mpsc::channel(TRANSPORT_NOTICES);
    let tasks = spawn_monitored_channel(
        channel_id,
        runner,
        raw_recorder,
        None,               // no display recording on a standalone channel
        1,                  // a single default Display View
        MatchSetup::none(), // no Match Rules on a standalone channel
        caps,
        event_tx,
        faulted.clone(),
        None,
        (notice_tx, notice_rx),
    );
    RunningChannel {
        tasks,
        events: event_rx,
        faulted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChunkTime, RuntimeEvent};
    use crate::transport::udp::{UdpMode, UdpTransport};
    use crate::transport::{ReceivedData, ReceivedPayload, TransportOutcome};
    use std::time::Duration;
    use tokio::net::UdpSocket;

    /// A test transport that emits a fixed script of datagrams, then stays alive
    /// until cancelled. Lets shutdown tests control the accepted backlog.
    struct ScriptedTransport {
        channel_id: ChannelId,
        chunks: Vec<Vec<u8>>,
    }

    impl DataTransportRunner for ScriptedTransport {
        fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle {
            let ScriptedTransport { channel_id, chunks } = self;
            let handle = tokio::spawn(async move {
                for chunk in chunks {
                    let data = ReceivedData {
                        channel_id,
                        payload: ReceivedPayload::Datagram(chunk),
                        received_at: ChunkTime::now(),
                    };
                    // Stops early if the pipeline is gone (forced shutdown).
                    if out.send(data).await.is_err() {
                        return TransportOutcome::Completed;
                    }
                }
                cancel.cancelled().await;
                TransportOutcome::Cancelled
            });
            TransportJoinHandle::Task(handle)
        }
    }

    #[tokio::test]
    async fn udp_channel_end_to_end_through_the_pipeline() {
        // Bind a UDP transport and run it through the orchestrated pipeline.
        let transport = UdpTransport::new(
            ChannelId::new(),
            "127.0.0.1:0".parse().unwrap(),
            UdpMode::Unicast,
        );
        let bound = transport.bind().await.unwrap();
        let server_addr = bound.local_addr().unwrap();
        let channel_id = bound.channel_id();

        let running = start_data_channel(channel_id, bound, PipelineCapacities::default(), None);

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"alpha", server_addr).await.unwrap();
        client.send_to(b"bravo", server_addr).await.unwrap();

        // Poll until both datagrams land in the stream scrollback, then fetch them
        // verbatim via the incremental stream path — concatenated in receive order
        // (no reframing, §18).
        let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(s) = running.snapshot().await {
                    if s.stream_end_offset >= 10 {
                        break s;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("datagrams did not arrive");
        assert_eq!(snapshot.channel_id, channel_id);
        assert_eq!(snapshot.activity.total_bytes, 10);
        let delta = running.stream_delta(0).await.unwrap();
        assert_eq!(&*delta.bytes, b"alphabravo");
        assert_eq!(delta.end_offset, 10);

        let _ = running.stop().await;
    }

    #[tokio::test]
    async fn graceful_stop_drains_the_accepted_backlog() {
        // §110: graceful stop must finish processing already-accepted data.
        let cid = ChannelId::new();
        let transport = ScriptedTransport {
            channel_id: cid,
            chunks: (0u8..5).map(|i| vec![i]).collect(),
        };
        let running = start_data_channel(cid, transport, PipelineCapacities::default(), None);

        // Wait for the five 1-byte datagrams to be received, then stop.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(s) = running.snapshot().await {
                    if s.activity.total_bytes >= 5 {
                        return;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("datagrams did not arrive");
        let mut pipeline = tokio::time::timeout(Duration::from_secs(5), running.stop())
            .await
            .expect("graceful stop hung")
            .expect("pipeline task panicked");
        // All five datagrams' bytes were received before finalizing.
        assert_eq!(pipeline.snapshot().activity.total_bytes, 5);
    }

    #[tokio::test]
    async fn forced_abort_completes_without_hanging() {
        // §111: forced shutdown may abandon the backlog, but must terminate
        // promptly and cleanly rather than draining it.
        let cid = ChannelId::new();
        let transport = ScriptedTransport {
            channel_id: cid,
            chunks: (0..1000).map(|i| vec![(i % 256) as u8]).collect(),
        };
        let running = start_data_channel(cid, transport, PipelineCapacities::default(), None);

        let mut pipeline = tokio::time::timeout(Duration::from_secs(5), running.abort())
            .await
            .expect("forced abort hung");
        // It terminated; it cannot have received more than was produced.
        assert!(pipeline.snapshot().activity.total_bytes <= 1000);
    }

    #[tokio::test]
    async fn spontaneous_transport_fault_emits_channel_faulted() {
        // §94/§101: a transport that ends on a fault (not a stop) surfaces a
        // ChannelFaulted event via the channel's fault monitor.
        struct FaultingTransport;
        impl DataTransportRunner for FaultingTransport {
            fn run(
                self,
                _out: Sender<ReceivedData>,
                _cancel: CancellationToken,
            ) -> TransportJoinHandle {
                TransportJoinHandle::Task(tokio::spawn(async move {
                    TransportOutcome::Faulted("device error".to_string())
                }))
            }
        }

        let cid = ChannelId::new();
        let mut running =
            start_data_channel(cid, FaultingTransport, PipelineCapacities::default(), None);
        assert_eq!(
            running.events().recv().await.unwrap(),
            RuntimeEvent::ChannelFaulted(cid)
        );
        // The same fault flips the shared state flag (ADR-006), so an orchestrator
        // reading it reconciles to Faulted without waiting for a command.
        assert!(running.is_faulted());
        // The fault's CAUSE reaches the diagnostics (review round 2: the
        // outcome string used to die unread in the monitor).
        let mut pipeline = running.stop().await.expect("pipeline returns");
        let snap = pipeline.snapshot();
        assert!(
            snap.diagnostics
                .errors
                .iter()
                .any(|d| d.message.contains("device error")),
            "diagnostics carry the transport fault cause: {:?}",
            snap.diagnostics.errors
        );
    }

    #[tokio::test]
    async fn terminal_fault_waits_for_space_in_a_full_notice_queue() {
        // Advisory stall notices may drop on saturation because they are emitted
        // from the receive hot path. A terminal fault is different: reception has
        // already ended, so its cause must wait for capacity and remain observable.
        let cid = ChannelId::new();
        let (notice_tx, mut notice_rx) = mpsc::channel(1);
        notice_tx
            .try_send(TransportNotice::ReceptionStalled {
                channel_id: cid,
                stalled_for: Duration::from_secs(1),
            })
            .unwrap();

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let delivery = tokio::spawn(async move {
            let _ = started_tx.send(());
            report_transport_fault(&notice_tx, cid, "device error".to_string()).await;
        });
        started_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert!(
            !delivery.is_finished(),
            "terminal delivery must wait rather than drop on a full queue"
        );

        assert!(matches!(
            notice_rx.recv().await,
            Some(TransportNotice::ReceptionStalled { .. })
        ));
        tokio::time::timeout(Duration::from_secs(1), delivery)
            .await
            .expect("terminal delivery stayed blocked after capacity opened")
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), notice_rx.recv())
                .await
                .expect("terminal fault was dropped")
                .unwrap(),
            TransportNotice::TransportFaulted {
                channel_id: cid,
                cause: "device error".to_string(),
            }
        );
    }
}
