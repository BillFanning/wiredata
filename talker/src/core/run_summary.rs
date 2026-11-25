//! Retained, self-contained summaries of completed channel runs.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use jiff::tz::{Offset, TimeZone};

use super::{
    channel::ChannelId,
    message::to_timestamp,
    telemetry::{DurationHistogram, MessageTiming, SendTimingReport},
    timing::{CadenceAlignment, TimerIntent, TimerMode, TimerStatus},
};

/// Process-unique run identity. A channel can restart before its predecessor's
/// completion is polled; ordering summaries by this id prevents an older tail
/// from replacing the newer completed run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(u64);

impl RunId {
    pub fn mint() -> Self {
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

/// Why the runner's command loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEndReason {
    StopCommand,
    OwnerDisconnected,
}

/// The cumulative outcomes of one run: what the runner's final
/// `TalkerStatus::Counters` and the completed run's [`RunSummary`] both state.
/// The runner builds it once at the end of a run and hands the same value to
/// both, so the two cannot disagree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunTotals {
    /// Complete messages sent, across all messages in the channel.
    pub total_count: u64,
    /// Wire bytes the interface accepted, including the accepted part of each
    /// possibly-partial send (§4.4).
    pub total_bytes: u64,
    /// Per-message send counts, indexed by position in the compiled schedule.
    pub per_message_counts: Vec<u64>,
    /// Per-message cumulative timing on the same index basis, including the
    /// delay each message's sends imposed on the others (ADR-045).
    pub per_message_timing: Vec<MessageTiming>,
    /// Status updates discarded because the receiver was full.
    pub dropped_statuses: u64,
    /// Sends missed under the scheduler's stall policy.
    pub missed_sends: u64,
    /// Send calls that reached the interface and failed before it accepted any
    /// byte.
    pub failed_sends: u64,
    /// Sends that failed after the interface accepted part of the message
    /// (§4.4); their accepted bytes are in `total_bytes`.
    pub possibly_partial_sends: u64,
    /// Due fires withheld by the bounded-backoff gate.
    pub withheld_sends: u64,
    /// Bounded cumulative measurements of deadline handling, payload rendering
    /// and the interface send call.
    pub timing: SendTimingReport,
    /// Platform deadline-wait policy and the interval that selected it.
    pub timer: TimerStatus,
}

impl RunTotals {
    pub fn unsent_sends(&self) -> u64 {
        self.missed_sends
            .saturating_add(self.failed_sends)
            .saturating_add(self.possibly_partial_sends)
            .saturating_add(self.withheld_sends)
    }

    pub fn scheduled_sends(&self) -> u64 {
        self.total_count.saturating_add(self.unsent_sends())
    }
}

/// Exact final outcomes plus bounded timing and environment facts for one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: RunId,
    pub channel: ChannelId,
    pub label: String,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    pub elapsed: Duration,
    pub end_reason: RunEndReason,
    pub totals: RunTotals,
}

impl RunSummary {
    pub fn started_utc(&self) -> String {
        format_utc(self.started_at)
    }

    pub fn finished_utc(&self) -> String {
        format_utc(self.finished_at)
    }

    /// Start time as the computer's local time, for the screen: no offset.
    pub fn started_local(&self) -> String {
        format_local(self.started_at)
    }

    /// Finish time as the computer's local time, for the screen: no offset.
    pub fn finished_local(&self) -> String {
        format_local(self.finished_at)
    }

    /// Stable multiline text intended for clipboard export and issue reports.
    pub fn to_report_text(&self) -> String {
        let mut out = String::with_capacity(1_024);
        let build_profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let _ = writeln!(out, "wiredata_talker_run_summary=1");
        let _ = writeln!(out, "app_version={}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(out, "build_profile={build_profile}");
        let _ = writeln!(
            out,
            "platform={}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let _ = writeln!(out, "channel_id={}", self.channel.as_u64());
        let _ = writeln!(out, "channel_label={:?}", self.label);
        let _ = writeln!(out, "run_id={}", self.run_id.as_u64());
        let _ = writeln!(out, "started_utc={}", self.started_utc());
        let _ = writeln!(out, "finished_utc={}", self.finished_utc());
        let _ = writeln!(out, "elapsed_us={}", self.elapsed.as_micros());
        let _ = writeln!(out, "end_reason={}", end_reason_name(self.end_reason));
        let _ = writeln!(out, "sent_messages={}", self.totals.total_count);
        let _ = writeln!(out, "sent_bytes={}", self.totals.total_bytes);
        let _ = writeln!(out, "failed_sends={}", self.totals.failed_sends);
        let _ = writeln!(
            out,
            "possibly_partial_sends={}",
            self.totals.possibly_partial_sends
        );
        let _ = writeln!(out, "withheld_sends={}", self.totals.withheld_sends);
        let _ = writeln!(out, "missed_sends={}", self.totals.missed_sends);
        let _ = writeln!(out, "scheduled_sends={}", self.totals.scheduled_sends());
        let counts = self
            .totals
            .per_message_counts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(out, "per_message_sent={counts}");
        // Per-message lanes stay flat and comma-separated, positionally aligned
        // with `per_message_sent`, so a reader can diff one line per fact
        // instead of re-assembling a block per message.
        write_per_message(
            &mut out,
            "interval_us",
            &self.totals.per_message_timing,
            |m| m.interval.as_micros().to_string(),
        );
        write_per_message(
            &mut out,
            "interval_changed",
            &self.totals.per_message_timing,
            |m| u8::from(m.interval_changed).to_string(),
        );
        // Each boundary carries its own sample count and maximum, so a lane can
        // be read without assuming the counts match `per_message_sent` — the
        // lateness population includes sends withheld by retry backoff.
        write_per_message(
            &mut out,
            "late_samples",
            &self.totals.per_message_timing,
            |m| m.deadline_lateness.sample_count().to_string(),
        );
        write_per_message(
            &mut out,
            "late_p99_us",
            &self.totals.per_message_timing,
            |m| optional_us(m.deadline_lateness.percentile_upper_bound(99)),
        );
        write_per_message(
            &mut out,
            "late_max_us",
            &self.totals.per_message_timing,
            |m| optional_us(m.deadline_lateness.max()),
        );
        write_per_message(
            &mut out,
            "render_samples",
            &self.totals.per_message_timing,
            |m| m.render_duration.sample_count().to_string(),
        );
        write_per_message(
            &mut out,
            "render_p99_us",
            &self.totals.per_message_timing,
            |m| optional_us(m.render_duration.percentile_upper_bound(99)),
        );
        write_per_message(
            &mut out,
            "render_max_us",
            &self.totals.per_message_timing,
            |m| optional_us(m.render_duration.max()),
        );
        write_per_message(
            &mut out,
            "send_samples",
            &self.totals.per_message_timing,
            |m| m.send_duration.sample_count().to_string(),
        );
        write_per_message(
            &mut out,
            "send_p99_us",
            &self.totals.per_message_timing,
            |m| optional_us(m.send_duration.percentile_upper_bound(99)),
        );
        write_per_message(
            &mut out,
            "send_max_us",
            &self.totals.per_message_timing,
            |m| optional_us(m.send_duration.max()),
        );
        // The culprit lane: what each message's own sends cost the others.
        write_per_message(
            &mut out,
            "blocked_others_us",
            &self.totals.per_message_timing,
            |m| m.blocked_others.as_micros().to_string(),
        );
        write_per_message(
            &mut out,
            "blocking_sends",
            &self.totals.per_message_timing,
            |m| m.blocking_sends.to_string(),
        );
        // The elapsed hold, which `blocked_others_us` is not: that one sums
        // every delayed message's wait and can exceed the send causing it.
        write_per_message(
            &mut out,
            "longest_block_us",
            &self.totals.per_message_timing,
            |m| m.longest_block.as_micros().to_string(),
        );
        // Sends lost rather than delayed. This lane sums to at most
        // `missed_sends`; the shortfall is what could not be charged to a
        // retained send, which is weaker than saying the thread was idle.
        write_per_message(
            &mut out,
            "missed_others",
            &self.totals.per_message_timing,
            |m| m.missed_others.to_string(),
        );
        let _ = writeln!(
            out,
            "observer_updates_dropped={}",
            self.totals.dropped_statuses
        );
        let _ = writeln!(
            out,
            "timer_policy={}",
            timer_mode_name(self.totals.timer.mode)
        );
        let _ = writeln!(
            out,
            "timer_reason={}",
            timer_intent_name(self.totals.timer.intent)
        );
        let shortest_us = self
            .totals
            .timer
            .shortest_active_interval()
            .map(|duration| duration.as_micros().to_string())
            .unwrap_or_else(|| "none".to_owned());
        let _ = writeln!(out, "shortest_active_interval_us={shortest_us}");
        let _ = writeln!(
            out,
            "cadence_alignment={}",
            cadence_alignment_name(self.totals.timer.cadence_alignment)
        );
        let _ = writeln!(
            out,
            "wall_clock_realignments={}",
            self.totals.timer.clock_realignments
        );
        write_histogram(
            &mut out,
            "deadline_lateness",
            self.totals.timing.cumulative.deadline_lateness,
        );
        write_histogram(
            &mut out,
            "render_duration",
            self.totals.timing.cumulative.render_duration,
        );
        write_histogram(
            &mut out,
            "send_call_duration",
            self.totals.timing.cumulative.send_duration,
        );
        write_histogram(
            &mut out,
            "recent_deadline_lateness",
            self.totals.timing.recent.deadline_lateness,
        );
        write_histogram(
            &mut out,
            "recent_render_duration",
            self.totals.timing.recent.render_duration,
        );
        write_histogram(
            &mut out,
            "recent_send_call_duration",
            self.totals.timing.recent.send_duration,
        );
        out
    }
}

/// `time` in UTC as RFC 3339 with milliseconds, which are cut, never rounded.
fn format_utc(time: SystemTime) -> String {
    let t = Offset::UTC.to_datetime(to_timestamp(time));
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.subsec_nanosecond() / 1_000_000
    )
}

fn format_local(time: SystemTime) -> String {
    format_local_at(time, TimeZone::system().to_offset(to_timestamp(time)))
}

/// [`format_local`] in the zone `offset`, so a test does not read the
/// machine's zone.
fn format_local_at(time: SystemTime, offset: Offset) -> String {
    let t = offset.to_datetime(to_timestamp(time));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn optional_us(duration: Option<Duration>) -> String {
    duration
        .map(|value| value.as_micros().to_string())
        .unwrap_or_else(|| "none".to_owned())
}

/// One `per_message_<name>=` line, positionally aligned with the others.
fn write_per_message(
    out: &mut String,
    name: &str,
    messages: &[MessageTiming],
    field: impl Fn(&MessageTiming) -> String,
) {
    let values = messages.iter().map(field).collect::<Vec<_>>().join(",");
    let _ = writeln!(out, "per_message_{name}={values}");
}

fn write_histogram(out: &mut String, name: &str, histogram: DurationHistogram) {
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

const fn end_reason_name(reason: RunEndReason) -> &'static str {
    match reason {
        RunEndReason::StopCommand => "stop_command",
        RunEndReason::OwnerDisconnected => "owner_disconnected",
    }
}

const fn timer_mode_name(mode: TimerMode) -> &'static str {
    match mode {
        TimerMode::NoRequest => "no_request",
        TimerMode::WindowsOneMillisecond => "windows_1_ms",
        TimerMode::WindowsRequestFailed => "windows_request_failed",
        TimerMode::NativeDeadlineWaits => "native_deadline_waits",
    }
}

const fn timer_intent_name(intent: TimerIntent) -> &'static str {
    match intent {
        TimerIntent::None => "none",
        TimerIntent::ContinuousHighRate => "high_rate",
        TimerIntent::PrecisionWindow => "precision_window",
    }
}

const fn cadence_alignment_name(alignment: CadenceAlignment) -> &'static str {
    match alignment {
        CadenceAlignment::Immediate => "immediate",
        CadenceAlignment::UtcPhase => "utc_phase",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::telemetry::SendTimingTelemetry;
    use crate::core::timing::ActiveCadence;

    fn zone(seconds: i32) -> Offset {
        Offset::from_seconds(seconds).unwrap()
    }

    /// `seconds` after 1970 UTC, plus `millis`.
    fn wall(seconds: u64, millis: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis)
    }

    /// A summary's UTC time has milliseconds, cut rather than rounded, and `Z`.
    #[test]
    fn utc_times_are_rfc3339_with_milliseconds() {
        // 2026-05-22 14:30:45 UTC
        assert_eq!(
            format_utc(wall(1_779_460_245, 123)),
            "2026-05-22T14:30:45.123Z"
        );
        assert_eq!(
            format_utc(wall(1_779_460_245, 0)),
            "2026-05-22T14:30:45.000Z"
        );
        let almost = wall(1_779_460_245, 0) + Duration::from_nanos(999_999_999);
        assert_eq!(format_utc(almost), "2026-05-22T14:30:45.999Z");
    }

    /// A summary's local time is to the second, in the zone given.
    #[test]
    fn local_times_are_to_the_second_in_the_local_zone() {
        let at = wall(1_779_460_245, 999);
        assert_eq!(format_local_at(at, zone(0)), "2026-05-22 14:30:45");
        assert_eq!(format_local_at(at, zone(-4 * 3600)), "2026-05-22 10:30:45");
        assert_eq!(
            format_local_at(at, zone(9 * 3600 + 1800)),
            "2026-05-23 00:00:45"
        );
    }

    fn histogram(sample: Duration) -> DurationHistogram {
        let mut histogram = DurationHistogram::default();
        histogram.record(sample);
        histogram
    }

    fn summary() -> RunSummary {
        let mut cumulative = SendTimingTelemetry::default();
        cumulative
            .deadline_lateness
            .record(Duration::from_micros(120));
        cumulative.render_duration.record(Duration::from_micros(60));
        cumulative.send_duration.record(Duration::from_micros(700));
        RunSummary {
            run_id: RunId::from_raw(9),
            channel: ChannelId::from_raw(3),
            label: "GPS\nfeed".to_owned(),
            started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            finished_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_012),
            elapsed: Duration::from_millis(12_345),
            end_reason: RunEndReason::StopCommand,
            totals: RunTotals {
                total_count: 100,
                total_bytes: 7_000,
                per_message_counts: vec![60, 40],
                per_message_timing: vec![
                    // #0: the victim — fast cadence, late, blames nobody.
                    MessageTiming {
                        interval: Duration::from_millis(50),
                        wire_bytes: 80,
                        interval_changed: false,
                        deadline_lateness: histogram(Duration::from_millis(9)),
                        render_duration: histogram(Duration::from_micros(40)),
                        send_duration: histogram(Duration::from_micros(300)),
                        blocked_others: Duration::ZERO,
                        blocking_sends: 0,
                        longest_block: Duration::ZERO,
                        missed_others: 0,
                    },
                    // #1: the culprit — slow cadence, slow write, charged for it.
                    MessageTiming {
                        interval: Duration::from_secs(2),
                        wire_bytes: 512,
                        interval_changed: true,
                        deadline_lateness: histogram(Duration::from_micros(80)),
                        render_duration: histogram(Duration::from_micros(90)),
                        send_duration: histogram(Duration::from_millis(120)),
                        blocked_others: Duration::from_millis(430),
                        blocking_sends: 4,
                        longest_block: Duration::from_millis(120),
                        // Two of the run's three misses are its doing; the third
                        // passed with the thread free and belongs to no message.
                        missed_others: 2,
                    },
                ],
                dropped_statuses: 2,
                missed_sends: 3,
                failed_sends: 1,
                possibly_partial_sends: 1,
                withheld_sends: 2,
                timing: SendTimingReport {
                    cumulative,
                    recent: cumulative,
                },
                timer: TimerStatus {
                    mode: TimerMode::WindowsOneMillisecond,
                    intent: TimerIntent::ContinuousHighRate,
                    active_cadence: Some(ActiveCadence {
                        messages: 1,
                        shortest: Duration::from_millis(10),
                    }),
                    cadence_alignment: CadenceAlignment::UtcPhase,
                    clock_realignments: 1,
                },
            },
        }
    }

    #[test]
    fn local_times_are_for_the_screen_and_carry_no_zone() {
        let summary = summary();
        for text in [summary.started_local(), summary.finished_local()] {
            assert_eq!(text.len(), 19, "{text}");
            assert_eq!(&text[4..5], "-");
            assert_eq!(&text[10..11], " ");
            assert!(!text.contains('Z') && !text.contains('+'), "{text}");
        }
        assert!(summary.started_utc().ends_with('Z'));
    }

    #[test]
    fn outcome_totals_are_saturating_and_complete() {
        let summary = summary();
        assert_eq!(summary.totals.unsent_sends(), 7);
        assert_eq!(summary.totals.scheduled_sends(), 107);
    }

    #[test]
    fn report_is_stable_escaped_and_self_describing() {
        let report = summary().to_report_text();
        assert!(report.starts_with("wiredata_talker_run_summary=1\n"));
        assert!(report.contains("channel_label=\"GPS\\nfeed\"\n"));
        assert!(report.contains("run_id=9\n"));
        assert!(report.contains("elapsed_us=12345000\n"));
        assert!(report.contains("scheduled_sends=107\n"));
        assert!(report.contains("possibly_partial_sends=1\n"));
        assert!(report.contains("per_message_sent=60,40\n"));
        // Every per-message lane is positionally aligned with per_message_sent,
        // so column N of each line describes the same message.
        assert!(report.contains("per_message_interval_us=50000,2000000\n"));
        // Bucket upper bounds, not raw samples: 300 µs lands in the 500 µs
        // bucket and 120 ms in the 128 ms one.
        assert!(report.contains("per_message_send_p99_us=500,128000\n"));
        // The pair that separates victim from culprit: #0 carries the lateness,
        // #1 carries the blame for causing it.
        assert!(report.contains("per_message_late_max_us=9000,80\n"));
        assert!(report.contains("per_message_blocked_others_us=0,430000\n"));
        assert!(report.contains("per_message_blocking_sends=0,4\n"));
        // The miss lane sums to at most the run's `missed_sends`; the gap is
        // what no retained send accounts for, and a reader can take that
        // difference straight off these two lines.
        assert!(report.contains("per_message_missed_others=0,2\n"));
        assert!(report.contains("missed_sends=3\n"));
        // `timing_mode` is gone from the report: the policy is derived from the
        // schedule, and `timer_policy` below already states which one applied.
        assert!(report.contains("timer_policy=windows_1_ms\n"));
        assert!(report.contains("cadence_alignment=utc_phase\n"));
        assert!(report.contains("wall_clock_realignments=1\n"));
        assert!(report.contains("deadline_lateness_samples=1\n"));
        assert!(report.contains("deadline_lateness_p99_upper_us=250\n"));
        assert!(report.contains("recent_deadline_lateness_samples=1\n"));
        assert!(report.contains("recent_send_call_duration_p99_upper_us=1000\n"));
    }
}
