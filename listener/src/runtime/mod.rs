//! Channel lifecycle orchestration, queue wiring, shutdown, fan-out, and backpressure policy.
//!
//! This is `listener-runtime` (spec §128): it owns orchestration — Channel
//! lifecycle, task spawning, queue wiring, fan-out, backpressure policy, and
//! shutdown. Per ADR-001 it is a Tokio hybrid: async tasks for orchestration
//! and network I/O, dedicated OS threads for blocking serial reads.
//!
//! Layers:
//! - [`queue`] — the §99 bounded-queue backpressure policies.
//! - [`pipeline`] — the §102 per-Channel stream pipeline + async ingest loop
//!   (raw recorder, scrollback, display recording, find/triggers, diagnostics).
//! - [`activity`] — the §166 per-Channel liveness/throughput meter.
//! - [`matchrule`] — §50.2 Find & Triggers evaluation (BytePattern / Idle).
//! - [`channel`]/[`tcp`] — per-Channel and TCP-listener task orchestration.
//! - [`build`] — maps validated config to live transports.
//! - [`listener`] — the [`Listener`] orchestrator: registry, §9 state machine,
//!   start/stop/apply-pending exposed as async methods (the command surface; ADR-012).
//! - [`snapshot`] — on-demand, pull-side readout: the small [`snapshot::ChannelSnapshot`]
//!   plus incremental [`snapshot::StreamDelta`] scrollback reads (ADR-011).
//! - [`telemetry`] — fixed-size cumulative timing summaries (ADR-026).

pub mod activity;
pub mod build;
pub mod channel;
pub mod listener;
pub mod matchrule;
pub mod pipeline;
pub mod queue;
pub mod run_summary;
pub mod snapshot;
pub mod tcp;
pub mod telemetry;

pub use crate::diagnostics::{Diagnostic, DiagnosticSeverity};
pub use activity::{ActivityMeter, ChannelActivity};
pub use build::BuildError;
pub use listener::{
    DrainedStop, Listener, OpenedStart, OrchestratorError, ReconnectAttempt, ShutdownOutcome,
    StartTicket, StopTicket, RUNTIME_SHUTDOWN_LIMIT,
};
pub use matchrule::{FiredRule, MatchRuleSet};
pub use pipeline::{run_channel, CapacityRequest, ChannelPipeline, PipelineCapacities};
pub use queue::DropOldestQueue;
pub use run_summary::{ListenerRunSummary, RunEndReason, RunId};
pub use snapshot::{
    ChannelSnapshot, ChannelStats, DiagnosticsSnapshot, DisplayViewSnapshot, PipelineRequest,
    QueueDepth, ReconnectProgress, RecordingStatus, StreamDelta, TriggeredMatch,
};
pub use tcp::{start_tcp_listener, TcpListenerHandle};
pub use telemetry::{
    ArrivalTimestampSummary, ByteHistogram, ChunkShape, CounterAvailability, DurationHistogram,
    IdleDeadlineTimerMode, IdleDeadlineTimerSummary, SerialStallSummary, TransportHealth,
};
