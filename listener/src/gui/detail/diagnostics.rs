//! The Receive diagnostics card (§137): the compact decision surface and its
//! expanded per-measurement detail.
//!
//! The card answers three questions in order — what the transport itself can
//! report, whether any queue is under pressure, and how long the receive path is
//! taking — then offers the measurement detail behind them. Two boundaries are
//! held throughout: a zero drop count never implies no loss, and no universal
//! good/bad latency threshold is invented, because none exists for this tool.
//!
//! The card chrome is shared with Talker (`wiredata_ui::diagnostics`); what each
//! signal *means*, and when it deserves attention, stays here.

use crate::core::{ArrivalTimestampStatus, ChannelId, RecordingState};
use crate::runtime::{
    ByteHistogram, CounterAvailability, DurationHistogram, QueueDepth, TransportHealth,
};

use super::super::state::{ChannelStatus, ChannelView};
use super::super::widgets::human_bytes;
use wiredata_ui::diagnostics::{
    attention_callout, decision_card, signal_grid, signal_row, SignalTone,
};
use wiredata_ui::format::{compact_duration, thousands};
use wiredata_ui::palette::active as palette;

// Diagnostic help follows the same technician-facing sequence throughout:
// locate the measurement in the receive path, explain how to read it, then
// state the operational implication and the boundary of what it can prove.
const TRANSPORT_TOOLTIP: &str = concat!(
    "Shows transport-specific evidence available from this channel run. ",
    "For Linux UDP, a reported drop is a datagram discarded because this socket's ",
    "kernel receive queue filled; the displayed cumulative count is a lower bound ",
    "because status updates are best-effort. For Serial, a stall means the receive ",
    "loop stopped issuing reads while a block waited for Ingest space. Serial ",
    "snapshots read the reader's authoritative state: completed totals cover finished ",
    "stalls, while Active is the elapsed time of the current unfinished stall. A ",
    "reported zero does not rule out loss in the network, device, adapter, driver, ",
    "or upstream buffer."
);
const PRESSURE_TOOLTIP: &str = concat!(
    "Ingest is received data moving through Listener's in-memory processing queue; ",
    "its summary shows the highest sampled in-flight count this run, not a live ",
    "current depth. Raw is the background recorder's in-memory queue, measured in bytes; ",
    "while recording, its ",
    "latest status sample and highest observed occupancy are shown, and afterward ",
    "the highest observed occupancy is retained for the channel run. The writer may ",
    "be handling one block concurrently. These are application queues, not device or ",
    "driver buffers. Half capacity is an early-warning reference: active Raw pressure ",
    "at or above it raises Attention, while a historical peak remains visible but does ",
    "not escalate the card by itself. A brief burst can set a peak, and occupancy is ",
    "not proof of data loss or sustained pressure."
);
const PIPELINE_TOOLTIP: &str = concat!(
    "Handoff is the time from a completed operating-system read until Listener ",
    "starts processing that block. Processing is the time Listener spends handling ",
    "it synchronously; background recorder I/O may run concurrently and is outside this ",
    "boundary. Recent values use an approximate latest or final ten-second segmented ",
    "window, and the chunk count on the same line says how many samples that is. The ",
    "worst value is exact at any count; 99% ≤ X appears beside it only when at least ",
    "99% of samples fell within a figure below that worst one. Compare changes with ",
    "queue pressure and normal operation; there is no universal good/bad latency limit."
);

const UDP_DROP_CALLOUT_TOOLTIP: &str = concat!(
    "Linux confirms at least this many datagrams were discarded because this ",
    "socket's kernel receive queue filled. The displayed cumulative count is the ",
    "latest update that reached Listener's pipeline; the actual run total may be ",
    "higher. It excludes packets lost before reaching this socket. Check host load, ",
    "input rate, and receive-buffer capacity."
);
const SERIAL_BACKPRESSURE_TOOLTIP: &str = concat!(
    "Shows authoritative Serial backpressure state for this channel run. A stall ",
    "starts after a block is read when the Ingest queue is full; the reader retries ",
    "for space about every 5 ms and issues no new read meanwhile. Episodes is the ",
    "number of completed stalls; Total and Max also cover completed stalls only. ",
    "Active is measured separately from the reader's current stall start. Advisory ",
    "warnings may be dropped without changing these values. While the channel ",
    "continues, the pending block is retried, but it is not retained if the channel ",
    "stops or its processing path closes. Upstream adapter or driver loss is not ",
    "observable here."
);

const RECEIVE_DETAILS_TOOLTIP: &str = concat!(
    "Use these measurements to locate pressure between the input, Listener ",
    "processing, and storage. Recent or final values use an approximate ten-second ",
    "segmented window; run values cover the time since Start. Each row states its worst ",
    "value with the number of samples behind it; 99% ≤ X is an upper-bound estimate for ",
    "at least 99% of those samples, and appears only where it differs from the worst."
);
const HANDOFF_TOOLTIP: &str = concat!(
    "Measures each received block from immediately after the operating-system read ",
    "returns until Listener begins processing it. This includes copying, waiting in ",
    "the Ingest queue, and thread scheduling. It excludes time spent in the device, ",
    "adapter, driver, or kernel before the read completed. Rising Handoff together ",
    "with Ingest pressure means arrivals were not drained promptly; compare Processing ",
    "to distinguish Listener workload from scheduling delay or input bursts."
);
const PROCESSING_TOOLTIP: &str = concat!(
    "Measures how long Listener handles each received block: accounting, scrollback, ",
    "match rules, display preparation, and recorder handoff and checks. It excludes ",
    "Handoff and background recorder I/O, which may run concurrently. Rising Processing ",
    "together with Ingest pressure points to a Listener workload bottleneck."
);
const CHUNK_SHAPE_TOOLTIP: &str = concat!(
    "Cumulative across the full channel run, not the recent ten-second window. One ",
    "chunk is the block returned by a single operating-system read; it is not ",
    "necessarily a complete line or protocol message. Size shows bytes per read; a ",
    "median or percentile appears only when reads actually varied, so a transport ",
    "returning uniform blocks states just the largest. ",
    "Read gap is between consecutive post-read monotonic captures and can reflect ",
    "source cadence, buffering, and host scheduling—not per-byte wire timing. Because ",
    "these values accumulate, a long run can dilute a brief change."
);
const ARRIVAL_TIMESTAMP_TOOLTIP: &str = concat!(
    "Wall-clock source for timestamped Mark annotations and recording metadata. ",
    "Post-read is host time captured when the operating-system read returns; Linux ",
    "kernel software time is captured earlier in the socket receive path. When kernel ",
    "timing is active, a post-read fallback is a datagram whose ancillary metadata ",
    "did not contain a usable timestamp. SO_TIMESTAMPNS returns a software timestamp ",
    "with nanosecond fields; that representation does not imply nanosecond accuracy. ",
    "Handoff and Read gap always use post-read monotonic captures. Neither source is ",
    "device time, hardware time, or per-byte arrival time."
);
const UDP_NOT_APPLICABLE_TOOLTIP: &str = concat!(
    "This channel does not use an input with a per-socket UDP receive-overflow ",
    "counter. This is expected for this transport and says nothing about data loss."
);
const UDP_UNAVAILABLE_TOOLTIP: &str = concat!(
    "Listener does not have an active drop counter attributable to this UDP socket. ",
    "The operating system may not support it, or enabling it may have failed. Treat ",
    "unavailable as unknown, not zero. Use packet capture or operating-system and ",
    "network counters if an independent loss measurement is required."
);
const UDP_AVAILABLE_TOOLTIP: &str = concat!(
    "Latest cumulative drop total reported for this Linux UDP socket. ",
    "A nonzero value confirms at least that many datagrams were discarded because ",
    "the kernel receive queue filled; a later update can be missed, so the actual run ",
    "total may be higher. Zero means no nonzero update reached this display, not that ",
    "no packets were lost here or elsewhere."
);
const IDLE_RULE_TIMING_TOOLTIP: &str = concat!(
    "Measures how late Listener evaluates and fires each Idle rule after its ",
    "configured no-data deadline. The worst firing is stated with the number of ",
    "firings behind it; where it differs, 99% ≤ 2 ms means at least 99% of measured ",
    "firings were no more than about 2 ms late. Lateness can include ",
    "operating-system wake delay, processor contention, and time Listener spent ",
    "servicing requests, processing input, or handling recording before evaluation. ",
    "It is not wall-clock or receive-time accuracy."
);
const IDLE_TIMER_TOOLTIP: &str = concat!(
    "Counts completed Idle-deadline wakes, not every wait that was started. On ",
    "Windows, Listener requests 1 ms timer resolution only during the final 32 ms; ",
    "1 ms unavailable counts completed wakes for which that request was ineffective, ",
    "not necessarily distinct Windows API calls. Linux and macOS use native deadline ",
    "waits. One completed wake can fire several rules. A wait restarted by ",
    "new input or other channel work is not counted. Compare unavailable wakes ",
    "with measured lateness; unavailable alone is not proof that a rule fired late."
);
const INGEST_QUEUE_TOOLTIP: &str = concat!(
    "Last sample is the number of blocks in flight when Listener most recently took ",
    "one for processing, including that block; it is not a live queue reading. Peak ",
    "is the highest such sample this run and capacity is the queue limit. A brief ",
    "burst can set the peak, so compare a high value with Handoff timing and ",
    "transport stalls."
);
const RAW_QUEUE_ACTIVE_TOOLTIP: &str = concat!(
    "Bytes of received blocks queued for the background Raw recorder, each block ",
    "counted with a small fixed allowance. Latest sample is the queue occupancy in the ",
    "most recent status snapshot, highest observed is the largest sample this run, and ",
    "capacity is the queue limit. The writer may be handling one block concurrently. A ",
    "brief burst can set the observed high. If a block would not fit, Raw recording ",
    "enters a gap: those bytes are not recorded, the gap is logged, and recording ",
    "resumes in a new file while reception continues."
);
const RAW_QUEUE_GAP_TOOLTIP: &str = concat!(
    "Raw recording is in a gap: received bytes are not being recorded until the next ",
    "file opens. The gap's start, end and reason are logged in the diagnostics and the ",
    "event log. Reception continues. The highest observed queue occupancy remains ",
    "visible for this channel run."
);
const RAW_QUEUE_RETAINED_TOOLTIP: &str = concat!(
    "Raw recording is no longer active. This preserves the highest observed queue ",
    "occupancy from the current channel run so earlier writer pressure remains ",
    "visible. It does not describe current storage load."
);
const RAW_QUEUE_FAULTED_TOOLTIP: &str = concat!(
    "Raw recording could not begin — for example, the file exists and on-exists is ",
    "Refuse, or another recording holds the destination — and retrying on its own ",
    "cannot fix it. Reception continues. Check the diagnostics for the specific cause, ",
    "then press Record again. The highest observed queue occupancy remains visible for ",
    "this channel run."
);
const RAW_QUEUE_INACTIVE_TOOLTIP: &str =
    "Raw recording is not active, so there is no Raw recorder queue to measure.";
#[derive(Debug, PartialEq, Eq)]
struct DecisionSignal {
    value: String,
    tone: SignalTone,
}

fn queue_level_reaches_half(level: usize, capacity: usize) -> bool {
    // Keep the established >= 50% reference without multiplying `level`
    // (and therefore without a theoretical usize overflow).
    capacity > 0 && level >= capacity.div_ceil(2)
}

fn reported_udp_drops(dropped: u64) -> String {
    if dropped == 0 {
        "reported 0".to_owned()
    } else {
        format!("reported ≥{dropped}")
    }
}

fn transport_signal(health: TransportHealth) -> DecisionSignal {
    if let Some(stalls) = health.serial_stalls {
        let active = stalls
            .active_for
            .map(|elapsed| format!(" · active {}", compact_duration(elapsed)))
            .unwrap_or_default();
        return DecisionSignal {
            value: format!(
                "Serial: {} completed episodes · total {} · max {}{active}",
                stalls.episodes,
                compact_duration(stalls.total),
                compact_duration(stalls.max),
            ),
            tone: if stalls.episodes > 0 || stalls.active_for.is_some() {
                SignalTone::Warning
            } else {
                SignalTone::Neutral
            },
        };
    }

    match health.udp_kernel_drops {
        CounterAvailability::Available(dropped) => DecisionSignal {
            value: format!("Kernel drops {}", reported_udp_drops(dropped)),
            tone: if dropped > 0 {
                SignalTone::Fault
            } else {
                SignalTone::Neutral
            },
        },
        CounterAvailability::Unsupported => DecisionSignal {
            value: "Kernel drops unavailable".to_owned(),
            tone: SignalTone::Neutral,
        },
        CounterAvailability::NotApplicable => DecisionSignal {
            value: "Kernel drops not applicable".to_owned(),
            tone: SignalTone::Neutral,
        },
    }
}

fn pressure_signal(
    ingest: QueueDepth,
    raw: Option<QueueDepth>,
    raw_recording: Option<RecordingState>,
) -> DecisionSignal {
    let ingest_text = if ingest.capacity == 0 {
        "Ingest awaiting data".to_owned()
    } else {
        format!("Ingest highest sampled {}/{}", ingest.peak, ingest.capacity)
    };
    // The Raw queue is measured in bytes (ADR-043).
    let bytes = |n: usize| human_bytes(n as u64);
    let raw_text = match (raw, raw_recording) {
        (Some(queue), Some(RecordingState::Gap(reason))) => format!(
            "Raw gap, {} · highest observed {} of {}",
            reason.describe(),
            bytes(queue.peak),
            bytes(queue.capacity)
        ),
        (None, Some(RecordingState::Gap(reason))) => format!("Raw gap, {}", reason.describe()),
        (Some(queue), Some(RecordingState::Faulted)) => format!(
            "Raw faulted · highest observed {} of {}",
            bytes(queue.peak),
            bytes(queue.capacity)
        ),
        (None, Some(RecordingState::Faulted)) => "Raw faulted".to_owned(),
        (Some(queue), Some(RecordingState::Enabled)) => format!(
            "Raw latest sample {} · highest observed {} of {}",
            bytes(queue.current),
            bytes(queue.peak),
            bytes(queue.capacity)
        ),
        (Some(queue), _) => format!(
            "Raw retained highest observed {} of {}",
            bytes(queue.peak),
            bytes(queue.capacity)
        ),
        (None, _) => "Raw not recording".to_owned(),
    };
    let tone = match (raw, raw_recording) {
        // A gap is bytes going unrecorded now.
        (_, Some(RecordingState::Faulted | RecordingState::Gap(_))) => SignalTone::Fault,
        (Some(queue), Some(RecordingState::Enabled))
            if queue_level_reaches_half(queue.current, queue.capacity) =>
        {
            SignalTone::Warning
        }
        _ => SignalTone::Neutral,
    };
    DecisionSignal {
        value: format!("{ingest_text} · {raw_text}"),
        tone,
    }
}

/// A sample population, named in the units the reader is looking at.
fn counted(samples: u64, noun: &str) -> String {
    if samples == 1 {
        format!("1 {noun}")
    } else {
        format!("{} {noun}s", thousands(samples))
    }
}

/// Summarize a duration histogram without a sample-count gate.
///
/// Listener had one: below twenty samples a readout showed its maximum and
/// called itself warming up. Talker retired the same gate in ADR-046 and the
/// arithmetic is why — the percentile rank is `ceil(samples × 99 / 100)`, which
/// equals `samples` for any count up to 99, so below a hundred samples the p99
/// bucket *is* the maximum's bucket. The gate relabelled one number as two
/// states, and did it at 20, while the two statistics only separate at 100.
///
/// So state the largest value, which is exact and true at one sample, and add
/// the percentile only where `p99 < max` proves it is a different figure:
/// within one bucket the bound is `>=` the maximum, so that comparison is false
/// until the p99 bucket sits strictly below the maximum's. The sample count,
/// which every readout now carries, does the work the label only implied.
///
/// `superlative` names the largest value. A slow handoff is the *worst* one,
/// but the largest gap between two reads is just the *longest* — nothing about
/// a quiet source is bad.
fn timing_figures(histogram: DurationHistogram, superlative: &str) -> Option<String> {
    let max = histogram.max()?;
    let p99 = histogram.percentile_upper_bound(99)?;
    Some(if p99 < max {
        format!(
            "99% ≤ {}, {superlative} {}",
            compact_duration(p99),
            compact_duration(max)
        )
    } else {
        format!("{superlative} {}", compact_duration(max))
    })
}

/// The same no-gate rule for read-chunk sizes, whose figures are byte counts.
///
/// A median and a percentile each earn their place by differing from the
/// largest, so a transport returning uniform blocks — a datagram socket
/// delivering one fixed-size payload per read — says so by having nothing to
/// add.
fn size_figures(sizes: ByteHistogram) -> Option<String> {
    let max = sizes.max()?;
    let median = sizes.percentile_upper_bound(50)?;
    let p99 = sizes.percentile_upper_bound(99)?;
    let mut parts = Vec::new();
    if median < max {
        parts.push(format!("median ≤ {}", human_bytes(median)));
    }
    if p99 < max {
        parts.push(format!("99% ≤ {}", human_bytes(p99)));
    }
    parts.push(format!("largest {}", human_bytes(max)));
    Some(format!("size {}", parts.join(", ")))
}

/// One timing boundary's contribution to the compact Pipeline row.
///
/// The row states one chunk count and one window at its end, so a boundary
/// appends its own count only when the populations differ. Handoff and
/// Processing are recorded once each per ingest, but their recent windows are
/// stamped at the start and the end of that work, so a segment boundary can
/// leave them one sample apart.
fn compact_timing(
    label: &str,
    noun: &str,
    cumulative: DurationHistogram,
    recent: DurationHistogram,
    line_samples: u64,
) -> String {
    match timing_figures(recent, "worst") {
        Some(figures) if recent.sample_count() == line_samples => format!("{label} {figures}"),
        Some(figures) => format!(
            "{label} {figures} of {}",
            counted(recent.sample_count(), noun)
        ),
        // No recent samples: the run's own worst is the only thing measured, and
        // saying which window it came from is the whole point of showing it.
        None => match timing_figures(cumulative, "worst") {
            Some(run) => format!("{label} {run} this run"),
            None => format!("{label} awaiting the first {noun}"),
        },
    }
}

/// One timing boundary's expanded detail row: its own window, its own counts,
/// and the run-wide figure beside the recent one.
fn detail_timing(
    label: &str,
    noun: &str,
    cumulative: DurationHistogram,
    recent: DurationHistogram,
    window: &str,
) -> String {
    let Some(run) = timing_figures(cumulative, "worst") else {
        return format!("{label}: awaiting the first {noun}");
    };
    let run = format!(
        "{run} of {} this run",
        counted(cumulative.sample_count(), noun)
    );
    match timing_figures(recent, "worst") {
        Some(figures) => format!(
            "{label} ({window}): {figures} of {} · {run}",
            counted(recent.sample_count(), noun)
        ),
        None => format!("{label}: no {noun}s in {window} · {run}"),
    }
}

fn pipeline_signal(view: &ChannelView, status: ChannelStatus) -> DecisionSignal {
    let window = timing_window(status);
    let recent_chunks = view.recent_ingest_delay.sample_count();
    let value = if view.ingest_delay.sample_count() == 0 {
        // Both boundaries are recorded inside one ingest, so neither has
        // anything to say — and repeating that twice beside an empty window
        // says less than stating it once.
        "Awaiting the first chunk".to_owned()
    } else {
        let tail = if recent_chunks == 0 {
            format!("no chunks in {window}")
        } else {
            format!("{} in {window}", counted(recent_chunks, "chunk"))
        };
        format!(
            "{} · {} · {tail}",
            compact_timing(
                "Handoff",
                "chunk",
                view.ingest_delay,
                view.recent_ingest_delay,
                recent_chunks,
            ),
            compact_timing(
                "Processing",
                "chunk",
                view.ingest_processing,
                view.recent_ingest_processing,
                recent_chunks,
            ),
        )
    };
    DecisionSignal {
        value,
        // No application latency budget exists, so these measurements are facts,
        // not a fabricated healthy/warning judgment.
        tone: SignalTone::Neutral,
    }
}

fn timing_window(status: ChannelStatus) -> &'static str {
    if matches!(status, ChannelStatus::Running | ChannelStatus::Reconnecting) {
        "~last 10 s"
    } else {
        "~final 10 s"
    }
}

fn card_tone(transport: SignalTone, pressure: SignalTone) -> SignalTone {
    if transport == SignalTone::Fault || pressure == SignalTone::Fault {
        SignalTone::Fault
    } else if transport == SignalTone::Warning || pressure == SignalTone::Warning {
        SignalTone::Warning
    } else {
        SignalTone::Neutral
    }
}

fn diagnostics_badge(
    tone: SignalTone,
    status: ChannelStatus,
    has_completed_run: bool,
) -> &'static str {
    match tone {
        SignalTone::Fault => "ISSUE",
        SignalTone::Warning => "ATTENTION",
        SignalTone::Neutral | SignalTone::Healthy
            if matches!(status, ChannelStatus::Running | ChannelStatus::Reconnecting) =>
        {
            "MONITORING"
        }
        SignalTone::Neutral | SignalTone::Healthy if has_completed_run => "LAST RUN",
        SignalTone::Neutral | SignalTone::Healthy => "AWAITING DATA",
    }
}

pub(super) fn show_receive_diagnostics_card(
    ui: &mut egui::Ui,
    id: ChannelId,
    status: ChannelStatus,
    view: &ChannelView,
) {
    let transport = transport_signal(view.transport_health);
    let pressure = pressure_signal(view.ingest_queue, view.raw_recording_queue, view.recording);
    let pipeline = pipeline_signal(view, status);
    let tone = card_tone(transport.tone, pressure.tone);
    let has_completed_run = view
        .snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.last_run_summary.is_some());
    let badge = diagnostics_badge(tone, status, has_completed_run);

    decision_card(ui, "Receive diagnostics", badge, tone, |ui| {
        signal_grid(ui, ("listener_receive_signals", id), |ui| {
            signal_row(
                ui,
                "Transport",
                transport.value,
                transport.tone,
                TRANSPORT_TOOLTIP,
            );
            signal_row(
                ui,
                "Pressure",
                pressure.value,
                pressure.tone,
                PRESSURE_TOOLTIP,
            );
            signal_row(
                ui,
                "Pipeline",
                pipeline.value,
                pipeline.tone,
                PIPELINE_TOOLTIP,
            );
        });

        if let CounterAvailability::Available(dropped) = view.transport_health.udp_kernel_drops {
            if dropped > 0 {
                ui.add_space(6.0);
                let _ = attention_callout(
                    ui,
                    ("listener_udp_kernel_drops", id),
                    format!("Kernel receive queue reported at least {dropped} dropped datagrams."),
                    SignalTone::Fault,
                    UDP_DROP_CALLOUT_TOOLTIP,
                );
            }
        }
        if let Some(stalls) = view.transport_health.serial_stalls {
            if stalls.episodes > 0 || stalls.active_for.is_some() {
                ui.add_space(6.0);
                let active = stalls
                    .active_for
                    .map(|elapsed| format!(", active for {}", compact_duration(elapsed)))
                    .unwrap_or_default();
                let _ = attention_callout(
                    ui,
                    ("listener_serial_stalls", id),
                    format!(
                        "Serial backpressure: {} completed episodes, {} completed total, {} completed maximum{active}.",
                        stalls.episodes,
                        compact_duration(stalls.total),
                        compact_duration(stalls.max),
                    ),
                    SignalTone::Warning,
                    SERIAL_BACKPRESSURE_TOOLTIP,
                );
            }
        }

        ui.add_space(5.0);
        let details = egui::CollapsingHeader::new("Receive & transport details")
            .id_salt(("listener_receive_transport_details", id))
            .default_open(false)
            .show(ui, |ui| show_receive_transport_details(ui, status, view));
        details
            .header_response
            .on_hover_text(RECEIVE_DETAILS_TOOLTIP);
    });
}

fn show_receive_transport_details(ui: &mut egui::Ui, status: ChannelStatus, view: &ChannelView) {
    let window = timing_window(status);
    let timing_text = detail_timing(
        "Handoff timing",
        "chunk",
        view.ingest_delay,
        view.recent_ingest_delay,
        window,
    );
    ui.label(egui::RichText::new(timing_text).weak())
        .on_hover_text(HANDOFF_TOOLTIP);

    let processing_text = detail_timing(
        "Processing timing",
        "chunk",
        view.ingest_processing,
        view.recent_ingest_processing,
        window,
    );
    ui.label(egui::RichText::new(processing_text).weak())
        .on_hover_text(PROCESSING_TOOLTIP);

    let chunks = view.chunk_shape;
    let chunk_count = chunks.chunk_count();
    let chunk_text = if chunk_count == 0 {
        "Chunk shape: awaiting the first chunk".to_owned()
    } else {
        let size_text = size_figures(chunks.sizes).unwrap_or_else(|| "size n/a".to_owned());
        // A gap needs two reads to exist, so it is short one sample by
        // definition rather than by anything having gone wrong.
        let gap_text = match timing_figures(chunks.inter_read_gaps, "longest") {
            Some(figures) => format!("read gap {figures}"),
            None => "read gap awaiting a second chunk".to_owned(),
        };
        format!(
            "Chunks (run): {} · {size_text} · {gap_text}",
            counted(chunk_count, "chunk")
        )
    };
    ui.label(egui::RichText::new(chunk_text).weak())
        .on_hover_text(CHUNK_SHAPE_TOOLTIP);

    if let Some(stalls) = view.transport_health.serial_stalls {
        let active = stalls
            .active_for
            .map(|elapsed| format!(" · active {}", compact_duration(elapsed)))
            .unwrap_or_default();
        ui.label(
            egui::RichText::new(format!(
                "Serial backpressure (run): {} completed episodes · completed total {} · completed max {}{active}",
                stalls.episodes,
                compact_duration(stalls.total),
                compact_duration(stalls.max),
            ))
            .weak(),
        )
        .on_hover_text(SERIAL_BACKPRESSURE_TOOLTIP);
    }

    let arrival = view.transport_health.arrival_timestamps;
    let arrival_text = match arrival.status {
        ArrivalTimestampStatus::PostRead => format!(
            "Arrival timestamps: post-read · {} chunks",
            arrival.post_read_samples
        ),
        ArrivalTimestampStatus::KernelSoftware => {
            let fallback = (arrival.post_read_samples > 0)
                .then(|| format!(" · {} post-read fallbacks", arrival.post_read_samples));
            format!(
                "Arrival timestamps: Linux kernel software · {} chunks{}",
                arrival.kernel_samples,
                fallback.as_deref().unwrap_or_default()
            )
        }
        ArrivalTimestampStatus::KernelRequestedUnavailable => format!(
            "Arrival timestamps: kernel unavailable · post-read fallback · {} chunks",
            arrival.post_read_samples
        ),
    };
    ui.label(egui::RichText::new(arrival_text).weak())
        .on_hover_text(ARRIVAL_TIMESTAMP_TOOLTIP);

    match view.transport_health.udp_kernel_drops {
        CounterAvailability::NotApplicable => {
            ui.label(egui::RichText::new("UDP kernel drops: not applicable").weak())
                .on_hover_text(UDP_NOT_APPLICABLE_TOOLTIP);
        }
        CounterAvailability::Unsupported => {
            ui.label(egui::RichText::new("UDP kernel drops: counter unavailable").weak())
                .on_hover_text(UDP_UNAVAILABLE_TOOLTIP);
        }
        CounterAvailability::Available(dropped) => {
            let reported = reported_udp_drops(dropped);
            let text = egui::RichText::new(format!("UDP kernel receive-queue drops: {reported}"));
            ui.label(if dropped > 0 {
                text.color(palette(ui).warning)
            } else {
                text.weak()
            })
            .on_hover_text(UDP_AVAILABLE_TOOLTIP);
        }
    }

    let has_idle_rule = view.config.match_rules.iter().any(|rule| {
        rule.enabled && matches!(rule.condition, crate::config::MatchCondition::Idle { .. })
    });
    let cumulative_rule_timing = view.rule_timer_lateness;
    let recent_rule_timing = view.recent_rule_timer_lateness;
    if has_idle_rule || cumulative_rule_timing.sample_count() > 0 {
        let text = detail_timing(
            "Idle rule lateness",
            "firing",
            cumulative_rule_timing,
            recent_rule_timing,
            window,
        );
        ui.label(egui::RichText::new(text).weak())
            .on_hover_text(IDLE_RULE_TIMING_TOOLTIP);
        let timer = view.idle_deadline_timer;
        let timer_text = if cfg!(windows) {
            format!(
                "Idle timer: completed wakes {} · 1 ms effective {} · 1 ms unavailable {}",
                timer
                    .windows_one_millisecond_waits
                    .saturating_add(timer.windows_request_failures),
                timer.windows_one_millisecond_waits,
                timer.windows_request_failures,
            )
        } else {
            format!(
                "Idle timer: completed native deadline wakes {}",
                timer.native_waits
            )
        };
        ui.label(egui::RichText::new(timer_text).weak())
            .on_hover_text(IDLE_TIMER_TOOLTIP);
    }

    let ingest = view.ingest_queue;
    let ingest_text = if ingest.capacity == 0 {
        "Ingest pressure: awaiting first block".to_owned()
    } else {
        format!(
            "Ingest pressure (run): peak {}/{} · last sample {}",
            ingest.peak, ingest.capacity, ingest.current
        )
    };
    ui.label(egui::RichText::new(ingest_text).weak())
        .on_hover_text(INGEST_QUEUE_TOOLTIP);

    let bytes = |n: usize| human_bytes(n as u64);
    match view.raw_recording_queue {
        Some(queue) if matches!(view.recording, Some(RecordingState::Gap(_))) => {
            let reason = match view.recording {
                Some(RecordingState::Gap(reason)) => reason.describe(),
                _ => "",
            };
            ui.label(
                egui::RichText::new(format!(
                    "Raw record queue (gap — {reason}): highest observed {} of {}",
                    bytes(queue.peak),
                    bytes(queue.capacity)
                ))
                .color(palette(ui).fault),
            )
            .on_hover_text(RAW_QUEUE_GAP_TOOLTIP);
        }
        Some(queue) if view.recording == Some(RecordingState::Faulted) => {
            ui.label(
                egui::RichText::new(format!(
                    "Raw record queue (faulted): highest observed {} of {}",
                    bytes(queue.peak),
                    bytes(queue.capacity)
                ))
                .color(palette(ui).fault),
            )
            .on_hover_text(RAW_QUEUE_FAULTED_TOOLTIP);
        }
        Some(queue) if view.recording == Some(RecordingState::Enabled) => {
            // The half-capacity judgement is named, not just coloured. The
            // numbers were always here, but "this crossed the reference" was a
            // conclusion the app had drawn and was stating in amber alone.
            let pressured = queue_level_reaches_half(queue.current, queue.capacity);
            let text = egui::RichText::new(format!(
                "Raw record queue: latest sample {} of {}{} · highest observed {}",
                bytes(queue.current),
                bytes(queue.capacity),
                if pressured { ", at half capacity" } else { "" },
                bytes(queue.peak)
            ))
            .weak();
            ui.label(if pressured {
                text.color(palette(ui).warning)
            } else {
                text
            })
            .on_hover_text(RAW_QUEUE_ACTIVE_TOOLTIP);
        }
        Some(queue) => {
            ui.label(
                egui::RichText::new(format!(
                    "Raw record queue (retained): highest observed {} of {}",
                    bytes(queue.peak),
                    bytes(queue.capacity)
                ))
                .weak(),
            )
            .on_hover_text(RAW_QUEUE_RETAINED_TOOLTIP);
        }
        None => {
            ui.label(egui::RichText::new("Raw record queue: (not recording)").weak())
                .on_hover_text(RAW_QUEUE_INACTIVE_TOOLTIP);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        card_tone, compact_timing, counted, detail_timing, diagnostics_badge, pressure_signal,
        queue_level_reaches_half, size_figures, timing_figures, timing_window, transport_signal,
        ChannelStatus, ARRIVAL_TIMESTAMP_TOOLTIP, CHUNK_SHAPE_TOOLTIP, HANDOFF_TOOLTIP,
        IDLE_RULE_TIMING_TOOLTIP, IDLE_TIMER_TOOLTIP, PIPELINE_TOOLTIP, PRESSURE_TOOLTIP,
        PROCESSING_TOOLTIP, RAW_QUEUE_ACTIVE_TOOLTIP, RAW_QUEUE_FAULTED_TOOLTIP,
        RAW_QUEUE_GAP_TOOLTIP, RECEIVE_DETAILS_TOOLTIP, SERIAL_BACKPRESSURE_TOOLTIP,
        TRANSPORT_TOOLTIP, UDP_AVAILABLE_TOOLTIP, UDP_UNAVAILABLE_TOOLTIP,
    };
    use crate::core::RecordingState;
    use crate::runtime::{
        ByteHistogram, CounterAvailability, DurationHistogram, QueueDepth, SerialStallSummary,
        TransportHealth,
    };
    use wiredata_ui::diagnostics::SignalTone;
    use wiredata_ui::format::compact_duration;

    /// The window labels the card puts on every recent measurement.
    #[test]
    fn timing_window_names_the_live_and_completed_windows() {
        assert_eq!(timing_window(ChannelStatus::Running), "~last 10 s");
        assert_eq!(timing_window(ChannelStatus::Stopped), "~final 10 s");
    }
    #[test]
    fn active_queue_pressure_preserves_the_existing_half_capacity_reference() {
        assert!(!queue_level_reaches_half(4, 10));
        assert!(queue_level_reaches_half(5, 10));
        assert!(!queue_level_reaches_half(1, 3));
        assert!(queue_level_reaches_half(2, 3));
        assert!(!queue_level_reaches_half(0, 0));
    }

    #[test]
    fn transport_signal_names_kernel_counter_scope_without_claiming_no_loss() {
        let cases = [
            (
                CounterAvailability::Available(0),
                "Kernel drops reported 0",
                SignalTone::Neutral,
            ),
            (
                CounterAvailability::Unsupported,
                "Kernel drops unavailable",
                SignalTone::Neutral,
            ),
            (
                CounterAvailability::NotApplicable,
                "Kernel drops not applicable",
                SignalTone::Neutral,
            ),
            (
                CounterAvailability::Available(3),
                "Kernel drops reported ≥3",
                SignalTone::Fault,
            ),
        ];

        for (availability, expected, tone) in cases {
            let signal = transport_signal(TransportHealth {
                udp_kernel_drops: availability,
                ..TransportHealth::default()
            });
            assert_eq!(signal.value, expected);
            assert_eq!(signal.tone, tone);
            assert!(!signal.value.to_ascii_lowercase().contains("no loss"));
        }
    }

    #[test]
    fn serial_stalls_separate_completed_and_active_measurements() {
        let signal = transport_signal(TransportHealth {
            serial_stalls: Some(SerialStallSummary {
                episodes: 2,
                total: Duration::from_millis(7),
                max: Duration::from_millis(5),
                active_for: Some(Duration::from_millis(3)),
            }),
            ..TransportHealth::default()
        });

        assert_eq!(signal.tone, SignalTone::Warning);
        assert!(signal.value.contains("2 completed episodes"));
        assert!(signal.value.contains(&format!(
            "active {}",
            compact_duration(Duration::from_millis(3))
        )));
        assert!(!signal.value.contains("estimated"));
    }

    #[test]
    fn pressure_signal_escalates_current_raw_pressure_but_not_historical_peaks() {
        let ingest = QueueDepth {
            current: 1,
            peak: 9,
            capacity: 10,
        };
        let raw = QueueDepth {
            current: 2,
            peak: 7,
            capacity: 8,
        };
        let quiet = pressure_signal(ingest, Some(raw), Some(RecordingState::Enabled));
        assert_eq!(quiet.tone, SignalTone::Neutral);
        assert_eq!(
            quiet.value,
            "Ingest highest sampled 9/10 · Raw latest sample 2 B · highest observed 7 B of 8 B"
        );

        let pressured = pressure_signal(
            ingest,
            Some(QueueDepth { current: 4, ..raw }),
            Some(RecordingState::Enabled),
        );
        assert_eq!(pressured.tone, SignalTone::Warning);

        let retained = pressure_signal(ingest, Some(raw), Some(RecordingState::Disabled));
        assert_eq!(retained.tone, SignalTone::Neutral);
        assert_eq!(
            retained.value,
            "Ingest highest sampled 9/10 · Raw retained highest observed 7 B of 8 B"
        );

        let faulted = pressure_signal(ingest, Some(raw), Some(RecordingState::Faulted));
        assert_eq!(faulted.tone, SignalTone::Fault);
        assert!(faulted.value.contains("Raw faulted"));

        // A gap names itself and its reason in words, not only by tone.
        let gap = pressure_signal(
            ingest,
            Some(raw),
            Some(RecordingState::Gap(crate::core::GapReason::QueueOverflow)),
        );
        assert_eq!(gap.tone, SignalTone::Fault);
        assert!(
            gap.value
                .contains("Raw gap, the recording queue overflowed"),
            "{}",
            gap.value
        );

        let awaiting = pressure_signal(QueueDepth::default(), None, None);
        assert_eq!(awaiting.value, "Ingest awaiting data · Raw not recording");
    }

    #[test]
    fn pressure_help_locates_both_queues_inside_listener() {
        assert!(PRESSURE_TOOLTIP.contains("Listener's in-memory processing queue"));
        assert!(PRESSURE_TOOLTIP.contains("recorder's in-memory queue"));
        assert!(PRESSURE_TOOLTIP.contains("not device or driver buffers"));
        assert!(PRESSURE_TOOLTIP.contains("not a live current depth"));
        assert!(PRESSURE_TOOLTIP.contains("does not escalate the card by itself"));
        assert!(PRESSURE_TOOLTIP.contains("not proof of data loss"));
    }

    #[test]
    fn technician_help_preserves_measurement_boundaries() {
        assert!(TRANSPORT_TOOLTIP.contains("reader's authoritative state"));
        assert!(TRANSPORT_TOOLTIP.contains("current unfinished stall"));
        assert!(UDP_UNAVAILABLE_TOOLTIP.contains("unknown, not zero"));
        assert!(UDP_AVAILABLE_TOOLTIP.contains("actual run total may be higher"));
        assert!(UDP_AVAILABLE_TOOLTIP.contains("not that no packets were lost"));
        assert!(PIPELINE_TOOLTIP.contains("approximate"));
        assert!(PIPELINE_TOOLTIP.contains("background recorder I/O may run concurrently"));
        assert!(PIPELINE_TOOLTIP.contains("no universal good/bad latency limit"));
    }

    #[test]
    fn technician_help_states_timing_timestamp_and_retention_limits() {
        assert!(SERIAL_BACKPRESSURE_TOOLTIP.contains("authoritative"));
        assert!(SERIAL_BACKPRESSURE_TOOLTIP.contains("completed stalls only"));
        assert!(SERIAL_BACKPRESSURE_TOOLTIP.contains("warnings may be dropped"));
        assert!(IDLE_RULE_TIMING_TOOLTIP.contains("processor contention"));
        assert!(IDLE_RULE_TIMING_TOOLTIP.contains("handling recording before evaluation"));
        assert!(IDLE_TIMER_TOOLTIP.contains("completed Idle-deadline wakes"));
        assert!(IDLE_TIMER_TOOLTIP.contains("One completed wake can fire several rules"));
        assert!(CHUNK_SHAPE_TOOLTIP.contains("full channel run"));
        assert!(CHUNK_SHAPE_TOOLTIP.contains("not the recent ten-second window"));
        assert!(ARRIVAL_TIMESTAMP_TOOLTIP.contains("does not imply nanosecond accuracy"));
        assert!(ARRIVAL_TIMESTAMP_TOOLTIP.contains("Handoff and Read gap always use post-read"));
        assert!(RAW_QUEUE_ACTIVE_TOOLTIP.contains("most recent status snapshot"));
        assert!(RAW_QUEUE_ACTIVE_TOOLTIP.contains("reception continues"));
        assert!(RAW_QUEUE_FAULTED_TOOLTIP.contains("could not begin"));
        assert!(RAW_QUEUE_GAP_TOOLTIP.contains("not being recorded"));
        assert!(RAW_QUEUE_GAP_TOOLTIP.contains("Reception continues"));
    }

    #[test]
    fn card_badge_escalates_only_from_actionable_transport_or_pressure_evidence() {
        assert_eq!(
            card_tone(SignalTone::Fault, SignalTone::Neutral),
            SignalTone::Fault
        );
        assert_eq!(
            card_tone(SignalTone::Neutral, SignalTone::Warning),
            SignalTone::Warning
        );
        assert_eq!(
            card_tone(SignalTone::Neutral, SignalTone::Neutral),
            SignalTone::Neutral
        );
    }

    #[test]
    fn neutral_badge_distinguishes_live_completed_and_empty_states() {
        assert_eq!(
            diagnostics_badge(SignalTone::Neutral, ChannelStatus::Running, false),
            "MONITORING"
        );
        assert_eq!(
            diagnostics_badge(SignalTone::Neutral, ChannelStatus::Reconnecting, true),
            "MONITORING"
        );
        assert_eq!(
            diagnostics_badge(SignalTone::Neutral, ChannelStatus::Stopped, true),
            "LAST RUN"
        );
        assert_eq!(
            diagnostics_badge(SignalTone::Neutral, ChannelStatus::Stopped, false),
            "AWAITING DATA"
        );
        assert_eq!(
            diagnostics_badge(SignalTone::Warning, ChannelStatus::Stopped, true),
            "ATTENTION"
        );
    }

    /// One sample recorded `count` times, for exercising the state ladder.
    fn histogram(count: u64, sample: Duration) -> DurationHistogram {
        let mut histogram = DurationHistogram::default();
        for _ in 0..count {
            histogram.record(sample);
        }
        histogram
    }

    /// The rule that replaced the twenty-sample warm-up gate (ADR-046): the
    /// worst value is reportable at any count, and the percentile earns its
    /// place only by being a different figure.
    #[test]
    fn timing_figures_state_the_worst_value_without_a_warm_up_gate() {
        assert_eq!(
            timing_figures(histogram(1, Duration::from_millis(3)), "worst").unwrap(),
            format!("worst {}", compact_duration(Duration::from_millis(3))),
            "one sample is reportable; the retired gate called this warming up"
        );

        let uniform = timing_figures(histogram(500, Duration::from_millis(1)), "worst").unwrap();
        assert!(
            !uniform.contains("99%"),
            "a percentile inside the maximum's own bucket is not a second figure"
        );

        let mut spread = histogram(999, Duration::from_micros(50));
        spread.record(Duration::from_millis(40));
        let figures = timing_figures(spread, "worst").unwrap();
        assert!(
            figures.starts_with("99% ≤ "),
            "a p99 strictly below the maximum is worth stating: {figures}"
        );
        assert!(figures.ends_with(&format!(
            "worst {}",
            compact_duration(Duration::from_millis(40))
        )));

        assert_eq!(timing_figures(DurationHistogram::default(), "worst"), None);
    }

    /// A quiet source is not a fault, so its gaps are the longest, not the worst.
    #[test]
    fn read_gaps_are_named_longest_rather_than_worst() {
        let figures = timing_figures(histogram(3, Duration::from_secs(2)), "longest").unwrap();
        assert!(figures.starts_with("longest "));
        assert!(!figures.contains("worst"));
    }

    #[test]
    fn compact_timing_names_every_measurement_state() {
        let empty = DurationHistogram::default();
        let one = histogram(1, Duration::from_millis(2));
        let worst = compact_duration(Duration::from_millis(2));

        assert_eq!(
            compact_timing("Handoff", "chunk", empty, empty, 0),
            "Handoff awaiting the first chunk"
        );
        assert_eq!(
            compact_timing("Handoff", "chunk", one, empty, 0),
            format!("Handoff worst {worst} this run"),
            "with nothing recent, the run's own figure is named as such"
        );
        assert_eq!(
            compact_timing("Handoff", "chunk", one, one, 1),
            format!("Handoff worst {worst}"),
            "the row already states this count once"
        );
        assert_eq!(
            compact_timing("Handoff", "chunk", one, one, 2),
            format!("Handoff worst {worst} of 1 chunk"),
            "a boundary whose population differs from the row states its own"
        );
    }

    #[test]
    fn detail_timing_pairs_the_recent_window_with_the_whole_run() {
        let empty = DurationHistogram::default();
        let run = histogram(40, Duration::from_millis(5));
        let recent = histogram(7, Duration::from_millis(5));
        let worst = compact_duration(Duration::from_millis(5));

        assert_eq!(
            detail_timing("Handoff timing", "chunk", empty, empty, "~last 10 s"),
            "Handoff timing: awaiting the first chunk"
        );
        assert_eq!(
            detail_timing("Handoff timing", "chunk", run, empty, "~final 10 s"),
            format!(
                "Handoff timing: no chunks in ~final 10 s · worst {worst} of 40 chunks this run"
            )
        );
        assert_eq!(
            detail_timing("Idle rule lateness", "firing", run, recent, "~last 10 s"),
            format!(
                "Idle rule lateness (~last 10 s): worst {worst} of 7 firings · \
                 worst {worst} of 40 firings this run"
            ),
            "the noun follows the measurement, not the transport"
        );
    }

    #[test]
    fn chunk_sizes_add_a_median_only_when_reads_actually_varied() {
        assert_eq!(size_figures(ByteHistogram::default()), None);

        let mut uniform = ByteHistogram::default();
        for _ in 0..200 {
            uniform.record(64);
        }
        let figures = size_figures(uniform).unwrap();
        assert!(
            !figures.contains("median") && !figures.contains("99%"),
            "fixed-size datagram reads have nothing to add: {figures}"
        );
        assert!(figures.starts_with("size largest "));

        let mut varied = ByteHistogram::default();
        for _ in 0..500 {
            varied.record(8);
        }
        varied.record(64_000);
        let figures = size_figures(varied).unwrap();
        assert!(figures.contains("median ≤ "), "{figures}");
        assert!(figures.contains("largest "), "{figures}");
    }

    #[test]
    fn sample_counts_agree_with_their_noun() {
        assert_eq!(counted(1, "chunk"), "1 chunk");
        assert_eq!(counted(0, "chunk"), "0 chunks");
        assert_eq!(counted(12_500, "firing"), "12,500 firings");
    }

    /// Nothing in the panel may teach the state Listener no longer has.
    #[test]
    fn no_readout_teaches_the_retired_warm_up_state() {
        for tooltip in [
            PIPELINE_TOOLTIP,
            RECEIVE_DETAILS_TOOLTIP,
            IDLE_RULE_TIMING_TOOLTIP,
            CHUNK_SHAPE_TOOLTIP,
            HANDOFF_TOOLTIP,
            PROCESSING_TOOLTIP,
        ] {
            assert!(
                !tooltip.to_ascii_lowercase().contains("warm"),
                "tooltip still teaches a warm-up: {tooltip}"
            );
            assert!(!tooltip.contains("20 samples"), "{tooltip}");
        }
    }
}
