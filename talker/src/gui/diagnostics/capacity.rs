//! Capacity: what the channel is being asked to carry, and whether the measured
//! work can service it.
//!
//! Demand comes from the running schedule where there is one and from the
//! on-screen draft otherwise, never mixed in one render (ADR-035). The service
//! estimate stays a projection and says so.

use super::*;

/// Hover text for the Capacity row, one paragraph per figure on it, in order.
pub(in crate::gui) const CAPACITY_TOOLTIP: Hover = Hover::new(
    "Capacity compares what the schedule asks for with what the channel can manage.\n\n\
     Requested is the message rate the running schedule asks for, worked out from each \
     message's size on the wire and its interval. A channel that is not running has no running \
     schedule, so the settings shown are used instead, and app headroom is marked \
     \"projected\".\n\n\
     Sent is the average over the last five seconds of the messages the interface accepted.\n\n\
     Serial is how much of the line's capacity at this baud rate the schedule needs. It is \
     calculated, not measured. A network line's capacity is unknown, so none is shown.\n\n\
     App headroom estimates how many times faster Talker could build and send messages than \
     the schedule needs; below 1x, it cannot keep up. It uses timing from the last ~10 s once \
     there are enough sends, and the whole run otherwise, and says when it does. It is an \
     estimate, not a promise.",
);

pub(in crate::gui) fn analyzed_channel_demand(
    message_count: usize,
    analyses: &[MessageAnalysisCache],
) -> Option<ChannelDemand> {
    if analyses.len() != message_count {
        return None;
    }
    let mut demand = ChannelDemand::default();
    for cache in analyses {
        let analysis = cache.analysis.as_ref()?;
        let config = analysis.config.as_ref()?;
        demand.include(MessageDemand::new(analysis.wire_len?, config.interval_ms));
    }
    Some(demand)
}

/// Each drafted message's interval, for a channel that has not reported any
/// runtime timing yet.
///
/// `None` when any message fails to parse, matching [`analyzed_channel_demand`]:
/// a partial schedule would understate the count. That is distinct from
/// `Some(empty)`, which means the schedule is valid and every message dormant —
/// the caller must not collapse the two, because one is an unfinished edit the
/// user can act on and the other is a deliberate state.
pub(in crate::gui) fn draft_intervals(
    analyses: &[MessageAnalysisCache],
) -> Option<Vec<std::time::Duration>> {
    analyses
        .iter()
        .map(|cache| {
            let config = cache.analysis.as_ref()?.config.as_ref()?;
            Some(std::time::Duration::from_millis(config.interval_ms))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) enum ServiceTimingSource {
    Recent,
    Run,
}

pub(in crate::gui) fn select_service_timing(
    state: RecentSnapshotState,
    recent: SendTimingTelemetry,
    cumulative: SendTimingTelemetry,
) -> (ServiceTimingSource, SendTimingTelemetry) {
    let recent_is_live = matches!(
        state,
        RecentSnapshotState::Current(_) | RecentSnapshotState::Final
    );
    if recent_is_live && service_sample_count(recent) >= MIN_SERVICE_SAMPLES {
        (ServiceTimingSource::Recent, recent)
    } else {
        (ServiceTimingSource::Run, cumulative)
    }
}

pub(in crate::gui) fn service_timing_source_label(
    source: ServiceTimingSource,
    state: RecentSnapshotState,
) -> String {
    match source {
        ServiceTimingSource::Recent => recent_snapshot_label(state),
        ServiceTimingSource::Run if matches!(state, RecentSnapshotState::Expired(_)) => {
            format!("run-wide fallback; {}", recent_snapshot_label(state))
        }
        ServiceTimingSource::Run => "run-wide".to_owned(),
    }
}

pub(in crate::gui) fn unavailable_line_capacity_label(kind: ConnKind) -> &'static str {
    match kind {
        ConnKind::Serial => "complete Serial setup",
        ConnKind::Udp | ConnKind::Tcp => "network line unmeasured",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::telemetry::{RecentSnapshotState, SendTimingTelemetry};

    use crate::gui::draft::{ConnKind, PayloadKind, ScheduleDraft};
    use crate::gui::MessageAnalysisCache;

    #[test]
    fn service_timing_uses_recent_only_while_current_or_final_and_warmed() {
        let mut recent = SendTimingTelemetry::default();
        for _ in 0..20 {
            recent.render_duration.record(Duration::from_micros(100));
            recent.send_duration.record(Duration::from_micros(200));
        }
        let mut cumulative = recent;
        cumulative.render_duration.record(Duration::from_millis(10));
        cumulative.send_duration.record(Duration::from_millis(20));

        assert_eq!(
            select_service_timing(
                RecentSnapshotState::Current(Duration::from_secs(9)),
                recent,
                cumulative,
            )
            .0,
            ServiceTimingSource::Recent
        );
        assert_eq!(
            select_service_timing(
                RecentSnapshotState::Expired(Duration::from_secs(10)),
                recent,
                cumulative,
            )
            .0,
            ServiceTimingSource::Run
        );
        assert_eq!(
            select_service_timing(RecentSnapshotState::Final, recent, cumulative).0,
            ServiceTimingSource::Recent
        );
    }

    #[test]
    fn incomplete_serial_setup_is_not_described_as_network_capacity() {
        assert_eq!(
            unavailable_line_capacity_label(ConnKind::Serial),
            "complete Serial setup"
        );
        assert_eq!(
            unavailable_line_capacity_label(ConnKind::Udp),
            "network line unmeasured"
        );
    }

    #[test]
    fn capacity_demand_reuses_exact_memoized_wire_lengths() {
        let drafts = [
            ScheduleDraft {
                payload_kind: PayloadKind::Utf8,
                utf8_text: "hello".to_owned(),
                interval_ms: "1000".to_owned(),
                ..ScheduleDraft::default()
            },
            ScheduleDraft {
                payload_kind: PayloadKind::Hex,
                hex_data: "DE AD".to_owned(),
                interval_ms: "500".to_owned(),
                ..ScheduleDraft::default()
            },
        ];
        let analyses: Vec<_> = drafts
            .iter()
            .map(|draft| {
                let mut cache = MessageAnalysisCache::default();
                cache.refresh(draft);
                cache
            })
            .collect();

        assert_eq!(analyses[0].analysis.as_ref().unwrap().wire_len, Some(5));
        assert_eq!(analyses[1].analysis.as_ref().unwrap().wire_len, Some(2));
        let demand = analyzed_channel_demand(drafts.len(), &analyses).unwrap();
        assert_eq!(demand.messages_per_second, 3.0);
        assert_eq!(demand.bytes_per_second, 9.0);
    }

    #[test]
    fn incomplete_message_withholds_capacity_instead_of_understating_it() {
        let drafts = [ScheduleDraft {
            payload_kind: PayloadKind::Utf8,
            utf8_text: "hello".to_owned(),
            interval_ms: "not-a-number".to_owned(),
            ..ScheduleDraft::default()
        }];
        let mut cache = MessageAnalysisCache::default();
        cache.refresh(&drafts[0]);

        assert_eq!(analyzed_channel_demand(drafts.len(), &[cache]), None);
    }
}
