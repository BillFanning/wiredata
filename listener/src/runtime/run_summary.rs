//! Retained, self-contained summaries of completed Listener channel runs.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::core::{ArrivalTimestampStatus, ChannelId};
use crate::transport::udp::SharedPortStatus;

use super::snapshot::{ChannelSnapshot, QueueDepth};
use super::telemetry::{
    ByteHistogram, ChunkShape, CounterAvailability, DurationHistogram, IdleDeadlineTimerSummary,
    TransportHealth,
};

/// Process-unique identity for one successfully started Channel run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(u64);

impl RunId {
    pub(crate) fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Why a successfully started Listener run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEndReason {
    StopRequested,
    TransportFault,
}

/// Exact final counters plus bounded timing, queue, loss, and environment facts.
#[derive(Clone, Debug)]
pub struct ListenerRunSummary {
    pub run_id: RunId,
    pub channel: ChannelId,
    pub label: String,
    pub transport: String,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    pub elapsed: Duration,
    pub end_reason: RunEndReason,
    /// False only when a task panicked or shutdown exceeded its grace window.
    pub final_snapshot_complete: bool,
    pub total_bytes: u64,
    pub chunk_shape: ChunkShape,
    pub match_boundary_saves: u64,
    pub diagnostics_events: usize,
    pub diagnostics_warnings: usize,
    pub diagnostics_errors: usize,
    pub ingest_delay: DurationHistogram,
    pub recent_ingest_delay: DurationHistogram,
    pub ingest_processing: DurationHistogram,
    pub recent_ingest_processing: DurationHistogram,
    pub rule_timer_lateness: DurationHistogram,
    pub recent_rule_timer_lateness: DurationHistogram,
    pub idle_deadline_timer: IdleDeadlineTimerSummary,
    pub transport_health: TransportHealth,
    pub ingest_queue: QueueDepth,
    pub raw_recording_queue: Option<QueueDepth>,
}

impl ListenerRunSummary {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn completed(
        run_id: RunId,
        channel: ChannelId,
        label: String,
        transport: String,
        started_at: SystemTime,
        finished_at: SystemTime,
        elapsed: Duration,
        end_reason: RunEndReason,
        snapshot: Option<&ChannelSnapshot>,
    ) -> Self {
        Self {
            run_id,
            channel,
            label,
            transport,
            started_at,
            finished_at,
            elapsed,
            end_reason,
            final_snapshot_complete: snapshot.is_some(),
            total_bytes: snapshot.map_or(0, |snap| snap.activity.total_bytes),
            chunk_shape: snapshot.map_or_else(ChunkShape::default, |snap| snap.chunk_shape),
            match_boundary_saves: snapshot.map_or(0, |snap| snap.match_boundary_saves),
            diagnostics_events: snapshot.map_or(0, |snap| snap.diagnostics.events.len()),
            diagnostics_warnings: snapshot.map_or(0, |snap| snap.diagnostics.warnings.len()),
            diagnostics_errors: snapshot.map_or(0, |snap| snap.diagnostics.errors.len()),
            ingest_delay: snapshot
                .map_or_else(DurationHistogram::default, |snap| snap.ingest_delay),
            recent_ingest_delay: snapshot
                .map_or_else(DurationHistogram::default, |snap| snap.recent_ingest_delay),
            ingest_processing: snapshot
                .map_or_else(DurationHistogram::default, |snap| snap.ingest_processing),
            recent_ingest_processing: snapshot.map_or_else(DurationHistogram::default, |snap| {
                snap.recent_ingest_processing
            }),
            rule_timer_lateness: snapshot
                .map_or_else(DurationHistogram::default, |snap| snap.rule_timer_lateness),
            recent_rule_timer_lateness: snapshot.map_or_else(DurationHistogram::default, |snap| {
                snap.recent_rule_timer_lateness
            }),
            idle_deadline_timer: snapshot.map_or_else(IdleDeadlineTimerSummary::default, |snap| {
                snap.idle_deadline_timer
            }),
            transport_health: snapshot
                .map_or_else(TransportHealth::default, |snap| snap.transport_health),
            ingest_queue: snapshot.map_or_else(QueueDepth::default, |snap| snap.ingest_queue),
            raw_recording_queue: snapshot.and_then(|snap| snap.raw_recording_queue),
        }
    }

    pub fn started_utc(&self) -> String {
        format_utc(self.started_at)
    }

    pub fn finished_utc(&self) -> String {
        format_utc(self.finished_at)
    }

    /// Stable multiline text intended for clipboard export and issue reports.
    pub fn to_report_text(&self) -> String {
        let mut out = String::with_capacity(2_048);
        let _ = writeln!(out, "wiredata_listener_run_summary=1");
        let _ = writeln!(out, "app_version={}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(
            out,
            "build_profile={}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
        let _ = writeln!(
            out,
            "platform={}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let _ = writeln!(out, "channel_id={}", self.channel);
        let _ = writeln!(out, "channel_label={:?}", self.label);
        let _ = writeln!(out, "transport={}", self.transport);
        let _ = writeln!(out, "run_id={}", self.run_id.as_u64());
        let _ = writeln!(out, "started_utc={}", self.started_utc());
        let _ = writeln!(out, "finished_utc={}", self.finished_utc());
        let _ = writeln!(out, "elapsed_us={}", self.elapsed.as_micros());
        let _ = writeln!(out, "end_reason={}", end_reason_name(self.end_reason));
        let _ = writeln!(
            out,
            "final_snapshot_complete={}",
            self.final_snapshot_complete
        );
        let _ = writeln!(out, "received_bytes={}", self.total_bytes);
        let _ = writeln!(out, "received_chunks={}", self.chunk_shape.chunk_count());
        write_byte_histogram(&mut out, "chunk_size", self.chunk_shape.sizes);
        write_duration_histogram(&mut out, "inter_read_gap", self.chunk_shape.inter_read_gaps);
        let _ = writeln!(out, "match_boundary_saves={}", self.match_boundary_saves);
        let _ = writeln!(out, "diagnostic_events={}", self.diagnostics_events);
        let _ = writeln!(out, "diagnostic_warnings={}", self.diagnostics_warnings);
        let _ = writeln!(out, "diagnostic_errors={}", self.diagnostics_errors);
        write_duration_histogram(&mut out, "ingest_handoff", self.ingest_delay);
        write_duration_histogram(&mut out, "recent_ingest_handoff", self.recent_ingest_delay);
        write_duration_histogram(&mut out, "ingest_processing", self.ingest_processing);
        write_duration_histogram(
            &mut out,
            "recent_ingest_processing",
            self.recent_ingest_processing,
        );
        write_duration_histogram(&mut out, "idle_rule_lateness", self.rule_timer_lateness);
        write_duration_histogram(
            &mut out,
            "recent_idle_rule_lateness",
            self.recent_rule_timer_lateness,
        );
        let _ = writeln!(
            out,
            "idle_timer_native_waits={}",
            self.idle_deadline_timer.native_waits
        );
        let _ = writeln!(
            out,
            "idle_timer_windows_1ms_waits={}",
            self.idle_deadline_timer.windows_one_millisecond_waits
        );
        let _ = writeln!(
            out,
            "idle_timer_windows_request_failures={}",
            self.idle_deadline_timer.windows_request_failures
        );
        write_queue(&mut out, "ingest_queue", Some(self.ingest_queue));
        write_queue(
            &mut out,
            "raw_recording_queue_bytes",
            self.raw_recording_queue,
        );
        write_transport_health(&mut out, self.transport_health);
        out
    }
}

fn format_utc(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn write_duration_histogram(out: &mut String, name: &str, histogram: DurationHistogram) {
    let duration_us = |duration: Option<Duration>| {
        duration
            .map(|value| value.as_micros().to_string())
            .unwrap_or_else(|| "none".to_owned())
    };
    let _ = writeln!(out, "{name}_samples={}", histogram.sample_count());
    let _ = writeln!(out, "{name}_mean_us={}", duration_us(histogram.mean()));
    let _ = writeln!(
        out,
        "{name}_p99_upper_us={}",
        duration_us(histogram.percentile_upper_bound(99))
    );
    let _ = writeln!(out, "{name}_max_us={}", duration_us(histogram.max()));
}

fn write_byte_histogram(out: &mut String, name: &str, histogram: ByteHistogram) {
    let bytes = |value: Option<u64>| {
        value
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_owned())
    };
    let _ = writeln!(out, "{name}_samples={}", histogram.sample_count());
    let _ = writeln!(out, "{name}_mean_bytes={}", bytes(histogram.mean()));
    let _ = writeln!(
        out,
        "{name}_p99_upper_bytes={}",
        bytes(histogram.percentile_upper_bound(99))
    );
    let _ = writeln!(out, "{name}_max_bytes={}", bytes(histogram.max()));
}

fn write_queue(out: &mut String, name: &str, queue: Option<QueueDepth>) {
    if let Some(queue) = queue {
        let _ = writeln!(out, "{name}_peak={}", queue.peak);
        let _ = writeln!(out, "{name}_capacity={}", queue.capacity);
    } else {
        let _ = writeln!(out, "{name}_peak=none");
        let _ = writeln!(out, "{name}_capacity=none");
    }
}

fn write_transport_health(out: &mut String, health: TransportHealth) {
    let arrival = health.arrival_timestamps;
    let _ = writeln!(
        out,
        "arrival_timestamp_status={}",
        arrival_status_name(arrival.status)
    );
    let _ = writeln!(out, "arrival_kernel_samples={}", arrival.kernel_samples);
    let _ = writeln!(
        out,
        "arrival_post_read_samples={}",
        arrival.post_read_samples
    );
    let shared_port = match health.udp_shared_port {
        SharedPortStatus::NotRequested => "not_requested",
        SharedPortStatus::Applied => "applied",
        SharedPortStatus::NotApplied => "not_applied",
    };
    let _ = writeln!(out, "udp_shared_port={shared_port}");
    match health.udp_receive_buffer {
        Some(buffer) => {
            let _ = writeln!(
                out,
                "udp_receive_buffer_requested_bytes={}",
                buffer.requested
            );
            let _ = writeln!(out, "udp_receive_buffer_granted_bytes={}", buffer.granted);
        }
        None => {
            let _ = writeln!(out, "udp_receive_buffer_requested_bytes=not_applicable");
            let _ = writeln!(out, "udp_receive_buffer_granted_bytes=not_applicable");
        }
    }
    match health.udp_kernel_drops {
        CounterAvailability::NotApplicable => {
            let _ = writeln!(out, "udp_kernel_drops=not_applicable");
        }
        CounterAvailability::Unsupported => {
            let _ = writeln!(out, "udp_kernel_drops=unsupported");
        }
        CounterAvailability::Available(dropped) => {
            let _ = writeln!(out, "udp_kernel_drops={dropped}");
        }
    }
    if let Some(stalls) = health.serial_stalls {
        let _ = writeln!(out, "serial_stall_episodes={}", stalls.episodes);
        let _ = writeln!(out, "serial_stall_total_us={}", stalls.total.as_micros());
        let _ = writeln!(out, "serial_stall_max_us={}", stalls.max.as_micros());
        let _ = writeln!(out, "serial_stall_active={}", stalls.active_for.is_some());
    } else {
        let _ = writeln!(out, "serial_stall_episodes=not_applicable");
    }
}

const fn end_reason_name(reason: RunEndReason) -> &'static str {
    match reason {
        RunEndReason::StopRequested => "stop_requested",
        RunEndReason::TransportFault => "transport_fault",
    }
}

const fn arrival_status_name(status: ArrivalTimestampStatus) -> &'static str {
    match status {
        ArrivalTimestampStatus::PostRead => "post_read",
        ArrivalTimestampStatus::KernelSoftware => "kernel_software",
        ArrivalTimestampStatus::KernelRequestedUnavailable => "kernel_requested_unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::super::telemetry::{ArrivalTimestampSummary, SerialStallSummary};
    use super::*;
    use crate::core::ArrivalTimestampSource;

    fn summary() -> ListenerRunSummary {
        let mut chunk_shape = ChunkShape::default();
        chunk_shape.sizes.record(120);
        chunk_shape.inter_read_gaps.record(Duration::from_millis(2));
        let mut ingest_delay = DurationHistogram::default();
        ingest_delay.record(Duration::from_micros(120));
        let mut arrivals = ArrivalTimestampSummary {
            status: ArrivalTimestampStatus::KernelSoftware,
            ..ArrivalTimestampSummary::default()
        };
        arrivals.record(ArrivalTimestampSource::KernelSoftware);
        ListenerRunSummary {
            run_id: RunId::from_raw(7),
            channel: ChannelId::new(),
            label: "GPS\nfeed".to_owned(),
            transport: "udp".to_owned(),
            started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            finished_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_012),
            elapsed: Duration::from_millis(12_345),
            end_reason: RunEndReason::StopRequested,
            final_snapshot_complete: true,
            total_bytes: 1_024,
            chunk_shape,
            match_boundary_saves: 2,
            diagnostics_events: 3,
            diagnostics_warnings: 1,
            diagnostics_errors: 0,
            ingest_delay,
            recent_ingest_delay: ingest_delay,
            ingest_processing: DurationHistogram::default(),
            recent_ingest_processing: DurationHistogram::default(),
            rule_timer_lateness: DurationHistogram::default(),
            recent_rule_timer_lateness: DurationHistogram::default(),
            idle_deadline_timer: IdleDeadlineTimerSummary {
                native_waits: 1,
                ..IdleDeadlineTimerSummary::default()
            },
            transport_health: TransportHealth {
                serial_stalls: Some(SerialStallSummary {
                    episodes: 1,
                    total: Duration::from_millis(5),
                    max: Duration::from_millis(5),
                    active_for: None,
                }),
                udp_kernel_drops: CounterAvailability::Available(4),
                arrival_timestamps: arrivals,
                udp_shared_port: SharedPortStatus::Applied,
                udp_receive_buffer: Some(crate::transport::udp::ReceiveBuffer {
                    requested: 4_194_304,
                    granted: 212_992,
                }),
            },
            ingest_queue: QueueDepth {
                current: 0,
                peak: 8,
                capacity: 256,
            },
            raw_recording_queue: None,
        }
    }

    #[test]
    fn report_is_stable_escaped_and_self_describing() {
        let report = summary().to_report_text();
        assert!(report.starts_with("wiredata_listener_run_summary=1\n"));
        assert!(report.contains("channel_label=\"GPS\\nfeed\"\n"));
        assert!(report.contains("run_id=7\n"));
        assert!(report.contains("elapsed_us=12345000\n"));
        assert!(report.contains("received_chunks=1\n"));
        assert!(report.contains("ingest_handoff_p99_upper_us=250\n"));
        assert!(report.contains("arrival_timestamp_status=kernel_software\n"));
        assert!(report.contains("udp_shared_port=applied\n"));
        assert!(report.contains("udp_receive_buffer_requested_bytes=4194304\n"));
        assert!(report.contains("udp_receive_buffer_granted_bytes=212992\n"));
        assert!(report.contains("udp_kernel_drops=4\n"));
        assert!(report.contains("ingest_queue_peak=8\n"));
    }
}
