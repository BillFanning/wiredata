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
    ByteHistogram, CounterAvailability, DurationHistogram, Provenance, QueueDepth, TransportHealth,
};
use crate::transport::udp::{ReceiveBuffer, SharedPortStatus};

use super::super::state::{ChannelStatus, ChannelView};
use super::super::widgets::human_bytes;
use wiredata_ui::diagnostics::{
    attention_callout, diagnostic_card, signal_grid, signal_row, SignalTone,
};
use wiredata_ui::format::{compact_duration, thousands};
use wiredata_ui::hover::{Hover, ShowHover};
use wiredata_ui::palette::active as palette;

// Diagnostic help follows the same technician-facing sequence throughout:
// the point first, then where the measurement sits in the receive path, how to
// read it, and the boundary of what it can prove (talker ADR-065).
const TRANSPORT_TOOLTIP: Hover = Hover::new(
    "What this channel's input itself can report about trouble.\n\n\
     On a Linux UDP channel, a reported drop is a datagram that the operating system discarded \
     because this socket's receive queue was full. The count shown is a lower bound, because \
     the updates that carry it can be missed.\n\n\
     On a Serial channel, a stall is a time when Listener's Serial reader stopped reading \
     because a block was waiting for room in the Ingest queue. The reader counts these itself, \
     so the Serial figures are exact, not estimated. The completed figures cover stalls that \
     have ended. Active is how long the stall now in progress has lasted so far.\n\n\
     A reported zero does not rule out loss in the network, the device, the adapter, the driver \
     or an upstream buffer.",
);
const PRESSURE_TOOLTIP: Hover = Hover::new(
    "How full Listener's own queues have been while it handled received data.\n\n\
     Ingest is the queue that holds received data on its way to being processed. Its figure is \
     the highest count of blocks waiting that Listener has sampled this run. It is not a live \
     reading of the queue now.\n\n\
     Raw is the queue that holds data for the background recorder, measured in bytes. While \
     recording, the latest sample and the highest observed amount are shown. After recording \
     stops, the highest observed amount stays on screen for this run. The recorder may be \
     writing one block at the same time.\n\n\
     These are queues inside Listener, not device or driver buffers.\n\n\
     Half capacity is the early-warning point. While Raw is recording, a latest sample at or \
     above half of its capacity raises Attention. A highest observed amount stays visible but \
     does not raise Attention by itself. A Raw gap or a failed recording raises Issue.\n\n\
     A brief burst can set a high figure, so a high figure is not proof of data loss or of \
     sustained pressure.",
);
const PIPELINE_TOOLTIP: Hover = Hover::new(
    "How long received data waited for Listener, and how long Listener took to handle it.\n\n\
     Handoff is the time from the moment the operating system finished a read until Listener \
     started processing that block.\n\n\
     Processing is the time Listener then spent handling the block. The background recorder's \
     disk work may run at the same time as that handling, and it is not part of Processing.\n\n\
     Recent figures cover about the last ten seconds while the channel runs, and about the \
     final ten seconds once it has stopped. If the channel stops answering, the figures shown are \
     its last answer and the card says how old they are. The window is approximate. The chunk \
     count on the same line says how many blocks the recent figures are based on.\n\n\
     The worst value is exact, however few blocks there are. \"99% ≤\" appears beside it only \
     when at least 99 of every 100 blocks were within a time that is lower than the worst. It is \
     an upper bound: the true value is at or below it.\n\n\
     There is no universal good or bad limit for these times. Compare a change with the \
     pressure figures and with how the channel behaves normally.",
);

const OLDER_FIGURES_TOOLTIP: Hover = Hover::new(
    "These figures are not from the latest request.\n\n\
     Listener asked the Channel for fresh figures and did not get an answer in time, so it is \
     showing the last answer it has. A Channel can be slow to answer while it closes a \
     recording on a disk that is not responding.\n\n\
     The figures are shown as they were measured, not adjusted for the time since. A \
     throughput or an active stall length shown here may no longer be true.",
);

const UDP_DROP_CALLOUT_TOOLTIP: Hover = Hover::new(
    "Linux confirms that at least this many datagrams were discarded because this socket's \
     receive queue was full.\n\n\
     The count shown comes from the latest update that reached Listener. The true total for the \
     run may be higher. Packets lost before they reached this socket, in the network or the \
     device, are not counted.\n\n\
     Check the load on this computer, the rate at which data arrives, and the size of the \
     receive buffer.",
);
const SERIAL_BACKPRESSURE_TOOLTIP: Hover = Hover::new(
    "Listener's Serial reader had to wait because the Ingest queue was full.\n\n\
     After reading a block, the reader holds it until the Ingest queue has room, trying again \
     about every 5 ms. It does not read again while it waits. Each such wait is one episode.\n\n\
     Completed episodes counts the episodes that have ended. The completed total and the \
     completed maximum are the combined and the longest duration of those same episodes. \
     Active is how long the episode now in progress has lasted so far, and is not included in \
     the completed figures.\n\n\
     The warning lines about a stall are advisory and can be lost. Losing one does not change \
     these figures.\n\n\
     While the channel keeps running, the waiting block is tried again. If the channel stops or \
     its processing path closes, the block is discarded.\n\n\
     Anything the adapter or driver lost while the reader waited cannot be seen here.",
);

const RECEIVE_DETAILS_TOOLTIP: Hover = Hover::new(
    "The measurements behind the Transport, Pressure and Pipeline rows.\n\n\
     Use them to find where data is held up: at the input, in Listener's processing, or in \
     recording.\n\n\
     Recent and final figures cover about the last ten seconds, or the final ten seconds after \
     the channel has stopped. The window is approximate. Figures marked \"run\" cover \
     everything since Start.\n\n\
     Each row gives its worst value and the number of samples behind it. \"99% ≤\" is an upper \
     bound that at least 99 of every 100 samples stayed within, and it appears only where it is \
     lower than the worst.",
);
const HANDOFF_TOOLTIP: Hover = Hover::new(
    "How long each received block waited before Listener began processing it.\n\n\
     The time starts just after the operating system's read returns. It includes copying the \
     data, waiting in the Ingest queue, and waiting for the operating system to run Listener. It \
     leaves out any time spent in the device, adapter, driver or kernel before the read \
     completed.\n\n\
     If Handoff rises together with Ingest pressure, data arrived faster than Listener took it \
     in. Compare Processing to tell a heavy Listener workload from a scheduling delay or a burst \
     of input.",
);
const PROCESSING_TOOLTIP: Hover = Hover::new(
    "How long Listener spent handling each received block.\n\n\
     The time covers counting the data, the scrollback, the match rules, preparing the display, \
     and handing the block to the recorder along with the recorder's checks. It leaves out \
     Handoff. It also leaves out the background recorder's disk work, which may run at the same \
     time.\n\n\
     If Processing rises together with Ingest pressure, Listener's own workload is the \
     bottleneck.",
);
const CHUNK_SHAPE_TOOLTIP: Hover = Hover::new(
    "The size of each read and the time between reads, over the whole run.\n\n\
     One chunk is the block returned by a single operating-system read. It is not necessarily a \
     whole line or a whole protocol message.\n\n\
     Size is in bytes per read. A median or a \"99% ≤\" figure appears only when the reads \
     actually varied in size, so an input that returns same-sized blocks shows only the \
     largest.\n\n\
     Read gap is the time between the end of one read and the end of the next. It can reflect \
     how the source sends, buffering, and the computer's own scheduling. It is not the timing of \
     individual bytes on the wire.\n\n\
     These figures cover the full channel run, not the recent ten-second window, so a long run \
     can hide a brief change.",
);
const ARRIVAL_TIMESTAMP_TOOLTIP: Hover = Hover::new(
    "Where the time of day on each received block comes from.\n\n\
     Timestamped Mark annotations and the recording's metadata use this time.\n\n\
     Post-read is the computer's clock read just after the operating system's read returns. \
     Linux kernel software time is captured earlier, as the data enters the socket. When kernel \
     timing is in use, a post-read fallback is a datagram that arrived without a usable kernel \
     timestamp. Kernel unavailable means kernel timing was asked for but could not be used, so \
     post-read is used instead.\n\n\
     The kernel's software timestamp carries digits down to the nanosecond. That does not mean it \
     is accurate to a nanosecond.\n\n\
     Handoff and Read gap always use the post-read clock. Neither kind of timestamp is device \
     time, hardware time, or the arrival time of individual bytes.",
);
const UDP_NOT_APPLICABLE_TOOLTIP: Hover = Hover::new(
    "This channel's input has no UDP receive-overflow counter.\n\n\
     That is expected for this kind of channel. It says nothing about whether data was lost.",
);
const UDP_UNAVAILABLE_TOOLTIP: Hover = Hover::new(
    "Listener has no drop counter for this UDP socket.\n\n\
     The operating system may not support one, or turning it on may have failed. Treat \
     unavailable as unknown, not as zero.\n\n\
     If you need an independent measure of loss, use a packet capture or the operating system's \
     and network's own counters.",
);
const UDP_AVAILABLE_TOOLTIP: Hover = Hover::new(
    "The latest total of datagrams discarded because this Linux UDP socket's receive queue was \
     full.\n\n\
     A nonzero value confirms that at least that many datagrams were discarded. A later update \
     can be missed, so the actual total for the run may be higher.\n\n\
     Zero means that no nonzero update has reached this display. It does not mean that no \
     packets were lost here or elsewhere.",
);
const INGEST_QUEUE_TOOLTIP: Hover = Hover::new(
    "How many received blocks were waiting in the Ingest queue.\n\n\
     Last sample is the number of blocks in the queue, counting the one just taken, at the \
     moment Listener most recently took a block for processing. It is not a live reading. Peak \
     is the highest such sample this run, and the second number is the queue's capacity.\n\n\
     A brief burst can set the peak, so when it is high, compare it with Handoff timing and \
     with transport stalls.",
);
const RAW_QUEUE_ACTIVE_TOOLTIP: Hover = Hover::new(
    "Bytes of received data waiting for the background Raw recorder.\n\n\
     Each queued block counts as its length plus a small fixed allowance. Latest sample is the \
     amount queued in the most recent status update. Highest observed is the largest sample \
     this run, and the second figure is the queue's capacity. The recorder may be writing one \
     block at the same time. A brief burst can set the highest observed figure.\n\n\
     If a block would not fit, Raw recording enters a gap. Those bytes are not recorded, the gap \
     is logged, and recording resumes in a new file. Reception continues throughout.",
);
const RAW_QUEUE_GAP_TOOLTIP: Hover = Hover::new(
    "Raw recording is in a gap, so received bytes are not being recorded.\n\n\
     Recording resumes when the next file opens. The start, the end and the reason for the gap \
     are logged in the diagnostics and in the event log. Reception continues.\n\n\
     The highest observed queue amount stays on screen for this run.",
);
const RAW_QUEUE_RETAINED_TOOLTIP: Hover = Hover::new(
    "The highest Raw queue amount observed earlier in this run.\n\n\
     Raw recording is no longer active. The figure is kept so that earlier pressure on the \
     recorder stays visible. It does not describe how busy the storage is now.",
);
const RAW_QUEUE_FAULTED_TOOLTIP: Hover = Hover::new(
    "Raw recording could not begin, and trying again on its own will not fix it.\n\n\
     For example, the file already exists and the recording is set to Refuse, or another \
     recording holds the destination. Reception continues.\n\n\
     Check the diagnostics for the specific cause, then press Record again. The highest observed \
     queue amount stays on screen for this run.",
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

const SHARED_PORT_TOOLTIP: Hover = Hover::new(
    "Whether the operating system applied the request to share this port.\n\n\
     Request shared port sets the operating system's address-reuse option before Listener opens \
     the port. Another program that also asks for sharing can then open this port too and \
     receive the same broadcast or multicast datagrams.\n\n\
     This line says whether the operating system applied the option. It cannot say whether \
     another program is actually bound.",
);

const RECEIVE_BUFFER_TOOLTIP: Hover = Hover::new(
    "How much received data the operating system will hold for this socket while Listener reads \
     it.\n\n\
     When the buffer fills, the operating system drops datagrams.\n\n\
     Listener asks for 4 MiB unless the profile sets recv_buffer_bytes. On Linux the most that \
     can be granted is net.core.rmem_max, and raising it with sysctl allows more.\n\n\
     Linux reports twice the size it actually set aside. Listener halves that report, so the \
     figure shown can be compared directly with the size requested.",
);

/// The receive-buffer line (§75): what was granted against what was asked.
fn receive_buffer_words(buffer: ReceiveBuffer) -> String {
    if buffer.is_short() {
        format!(
            "Receive buffer: {} granted, less than the {} requested",
            human_bytes(buffer.granted as u64),
            human_bytes(buffer.requested as u64)
        )
    } else {
        format!(
            "Receive buffer: {} granted",
            human_bytes(buffer.granted as u64)
        )
    }
}

/// The shared-port line (§15, ADR-047), or `None` when sharing was not requested.
fn shared_port_words(status: SharedPortStatus) -> Option<&'static str> {
    match status {
        SharedPortStatus::NotRequested => None,
        SharedPortStatus::Applied => Some("Shared port: applied · other programs can bind it"),
        SharedPortStatus::NotApplied => {
            Some("Shared port: requested · the OS did not apply it · not shared")
        }
    }
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
        format!(
            "Ingest highest sampled {} of {}",
            ingest.peak, ingest.capacity
        )
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
    let window = timing_window(status, view.figures);
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

/// The label for the window the recent figures cover. "Final" is claimed only
/// for a stopped or faulted Channel whose figures were measured for the latest
/// request; an older answer cannot say it was taken at the end.
fn timing_window(status: ChannelStatus, figures: Provenance) -> &'static str {
    if matches!(status, ChannelStatus::Running | ChannelStatus::Reconnecting) {
        "~last 10 s"
    } else if figures == Provenance::Fresh {
        "~final 10 s"
    } else {
        "~last 10 s measured"
    }
}

/// The sentence that says the figures shown are not from the latest request,
/// or `None` when they are.
fn older_figures_sentence(figures: Provenance) -> Option<String> {
    const CAUSE: &str = "The Channel is not answering status requests.";
    match figures {
        Provenance::Fresh => None,
        Provenance::Cached { age } => Some(format!(
            "These figures were measured {} ago. {CAUSE}",
            compact_duration(age)
        )),
        Provenance::Retained => Some(
            "These figures are from an earlier run, or are empty. \
             The latest run has no status answer to show."
                .to_owned(),
        ),
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

    // No header row (ADR-057): the badge rose only from Transport and
    // Pressure, which mark themselves with ⚠ and have their own callouts. Its
    // calm words are said elsewhere: running or stopped by the status at the
    // top of the pane, last-run figures by Pipeline's "~final 10 s", and no
    // data yet by the rows' own "awaiting".
    diagnostic_card(ui, |ui| {
        if let Some(sentence) = older_figures_sentence(view.figures) {
            ui.label(sentence).on_hover(&OLDER_FIGURES_TOOLTIP);
            ui.add_space(6.0);
        }
        signal_grid(ui, ("listener_receive_signals", id), |ui| {
            let help = TRANSPORT_TOOLTIP.layout(ui.style());
            signal_row(ui, "Transport", transport.value, transport.tone, help);
            let help = PRESSURE_TOOLTIP.layout(ui.style());
            signal_row(ui, "Pressure", pressure.value, pressure.tone, help);
            let help = PIPELINE_TOOLTIP.layout(ui.style());
            signal_row(ui, "Pipeline", pipeline.value, pipeline.tone, help);
        });

        if let CounterAvailability::Available(dropped) = view.transport_health.udp_kernel_drops {
            if dropped > 0 {
                ui.add_space(6.0);
                let help = UDP_DROP_CALLOUT_TOOLTIP.layout(ui.style());
                let _ = attention_callout(
                    ui,
                    ("listener_udp_kernel_drops", id),
                    format!("Kernel receive queue reported at least {dropped} dropped datagrams."),
                    SignalTone::Fault,
                    help,
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
                let help = SERIAL_BACKPRESSURE_TOOLTIP.layout(ui.style());
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
                    help,
                );
            }
        }

        ui.add_space(5.0);
        let details = egui::CollapsingHeader::new("Receive & transport details")
            .id_salt(("listener_receive_transport_details", id))
            .default_open(false)
            .show(ui, |ui| show_receive_transport_details(ui, status, view));
        details.header_response.on_hover(&RECEIVE_DETAILS_TOOLTIP);
    });
}

fn show_receive_transport_details(ui: &mut egui::Ui, status: ChannelStatus, view: &ChannelView) {
    let window = timing_window(status, view.figures);
    let timing_text = detail_timing(
        "Handoff timing",
        "chunk",
        view.ingest_delay,
        view.recent_ingest_delay,
        window,
    );
    ui.label(egui::RichText::new(timing_text).weak())
        .on_hover(&HANDOFF_TOOLTIP);

    let processing_text = detail_timing(
        "Processing timing",
        "chunk",
        view.ingest_processing,
        view.recent_ingest_processing,
        window,
    );
    ui.label(egui::RichText::new(processing_text).weak())
        .on_hover(&PROCESSING_TOOLTIP);

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
        .on_hover(&CHUNK_SHAPE_TOOLTIP);

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
        .on_hover(&SERIAL_BACKPRESSURE_TOOLTIP);
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
        .on_hover(&ARRIVAL_TIMESTAMP_TOOLTIP);

    let shared_port = view.transport_health.udp_shared_port;
    if let Some(words) = shared_port_words(shared_port) {
        let text = egui::RichText::new(words);
        ui.label(if shared_port == SharedPortStatus::NotApplied {
            text.color(palette(ui).warning)
        } else {
            text.weak()
        })
        .on_hover(&SHARED_PORT_TOOLTIP);
    }

    if let Some(buffer) = view.transport_health.udp_receive_buffer {
        let text = egui::RichText::new(receive_buffer_words(buffer));
        ui.label(if buffer.is_short() {
            text.color(palette(ui).warning)
        } else {
            text.weak()
        })
        .on_hover(&RECEIVE_BUFFER_TOOLTIP);
    }

    match view.transport_health.udp_kernel_drops {
        CounterAvailability::NotApplicable => {
            ui.label(egui::RichText::new("UDP kernel drops: not applicable").weak())
                .on_hover(&UDP_NOT_APPLICABLE_TOOLTIP);
        }
        CounterAvailability::Unsupported => {
            ui.label(egui::RichText::new("UDP kernel drops: counter unavailable").weak())
                .on_hover(&UDP_UNAVAILABLE_TOOLTIP);
        }
        CounterAvailability::Available(dropped) => {
            let reported = reported_udp_drops(dropped);
            let text = egui::RichText::new(format!("UDP kernel receive-queue drops: {reported}"));
            ui.label(if dropped > 0 {
                text.color(palette(ui).warning)
            } else {
                text.weak()
            })
            .on_hover(&UDP_AVAILABLE_TOOLTIP);
        }
    }

    let ingest = view.ingest_queue;
    let ingest_text = if ingest.capacity == 0 {
        "Ingest pressure: awaiting first block".to_owned()
    } else {
        format!(
            "Ingest pressure (run): peak {} of {} · last sample {}",
            ingest.peak, ingest.capacity, ingest.current
        )
    };
    ui.label(egui::RichText::new(ingest_text).weak())
        .on_hover(&INGEST_QUEUE_TOOLTIP);

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
            .on_hover(&RAW_QUEUE_GAP_TOOLTIP);
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
            .on_hover(&RAW_QUEUE_FAULTED_TOOLTIP);
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
            .on_hover(&RAW_QUEUE_ACTIVE_TOOLTIP);
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
            .on_hover(&RAW_QUEUE_RETAINED_TOOLTIP);
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
        compact_timing, counted, detail_timing, older_figures_sentence, pressure_signal,
        queue_level_reaches_half, receive_buffer_words, shared_port_words, size_figures,
        timing_figures, timing_window, transport_signal, ChannelStatus, ReceiveBuffer,
        SharedPortStatus, ARRIVAL_TIMESTAMP_TOOLTIP, CHUNK_SHAPE_TOOLTIP, HANDOFF_TOOLTIP,
        INGEST_QUEUE_TOOLTIP, PIPELINE_TOOLTIP, PRESSURE_TOOLTIP, PROCESSING_TOOLTIP,
        RAW_QUEUE_ACTIVE_TOOLTIP, RAW_QUEUE_FAULTED_TOOLTIP, RAW_QUEUE_GAP_TOOLTIP,
        RAW_QUEUE_RETAINED_TOOLTIP, RECEIVE_BUFFER_TOOLTIP, RECEIVE_DETAILS_TOOLTIP,
        SERIAL_BACKPRESSURE_TOOLTIP, SHARED_PORT_TOOLTIP, TRANSPORT_TOOLTIP, UDP_AVAILABLE_TOOLTIP,
        UDP_DROP_CALLOUT_TOOLTIP, UDP_NOT_APPLICABLE_TOOLTIP, UDP_UNAVAILABLE_TOOLTIP,
    };
    use crate::core::RecordingState;
    use crate::runtime::{
        ByteHistogram, CounterAvailability, DurationHistogram, Provenance, QueueDepth,
        SerialStallSummary, TransportHealth,
    };
    use wiredata_ui::diagnostics::SignalTone;
    use wiredata_ui::format::compact_duration;

    /// The window labels the card puts on every recent measurement.
    #[test]
    fn timing_window_names_the_live_and_completed_windows() {
        let fresh = Provenance::Fresh;
        assert_eq!(timing_window(ChannelStatus::Running, fresh), "~last 10 s");
        assert_eq!(timing_window(ChannelStatus::Stopped, fresh), "~final 10 s");
        assert_eq!(timing_window(ChannelStatus::Faulted, fresh), "~final 10 s");
    }

    /// A window is called the final one only when it was measured at the end.
    #[test]
    fn a_window_from_an_older_answer_is_not_called_the_final_one() {
        let cached = Provenance::Cached {
            age: Duration::from_secs(12),
        };
        for figures in [cached, Provenance::Retained] {
            assert_eq!(
                timing_window(ChannelStatus::Faulted, figures),
                "~last 10 s measured"
            );
            assert_eq!(
                timing_window(ChannelStatus::Stopped, figures),
                "~last 10 s measured"
            );
            assert_eq!(timing_window(ChannelStatus::Running, figures), "~last 10 s");
        }
    }

    #[test]
    fn only_figures_from_an_older_answer_get_a_sentence_saying_so() {
        assert_eq!(older_figures_sentence(Provenance::Fresh), None);
        assert_eq!(
            older_figures_sentence(Provenance::Cached {
                age: Duration::from_secs(12)
            })
            .as_deref(),
            Some(
                "These figures were measured 12.0 s ago. \
                 The Channel is not answering status requests."
            )
        );
        assert_eq!(
            older_figures_sentence(Provenance::Retained).as_deref(),
            Some(
                "These figures are from an earlier run, or are empty. \
                 The latest run has no status answer to show."
            )
        );
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
            "Ingest highest sampled 9 of 10 · Raw latest sample 2 B · highest observed 7 B of 8 B"
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
            "Ingest highest sampled 9 of 10 · Raw retained highest observed 7 B of 8 B"
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
        let hover = PRESSURE_TOOLTIP;
        let text = hover.text();
        assert!(text.contains("queue that holds received data on its way to being processed"));
        assert!(text.contains("queue that holds data for the background recorder"));
        assert!(text.contains("queues inside Listener, not device or driver buffers"));
        assert!(text.contains("It is not a live reading of the queue now"));
        assert!(text.contains("does not raise Attention by itself"));
        assert!(text.contains("not proof of data loss"));
    }

    #[test]
    fn technician_help_preserves_measurement_boundaries() {
        assert!(TRANSPORT_TOOLTIP
            .text()
            .contains("so the Serial figures are exact, not estimated"));
        assert!(TRANSPORT_TOOLTIP
            .text()
            .contains("how long the stall now in progress has lasted so far"));
        assert!(UDP_UNAVAILABLE_TOOLTIP
            .text()
            .contains("unknown, not as zero"));
        assert!(UDP_AVAILABLE_TOOLTIP
            .text()
            .contains("actual total for the run may be higher"));
        assert!(UDP_AVAILABLE_TOOLTIP
            .text()
            .contains("It does not mean that no packets were lost here or elsewhere"));
        assert!(PIPELINE_TOOLTIP
            .text()
            .contains("The window is approximate"));
        assert!(PIPELINE_TOOLTIP
            .text()
            .contains("background recorder's disk work may run at the same time"));
        assert!(PIPELINE_TOOLTIP
            .text()
            .contains("no universal good or bad limit"));
    }

    #[test]
    fn technician_help_states_timing_timestamp_and_retention_limits() {
        assert!(SERIAL_BACKPRESSURE_TOOLTIP
            .text()
            .contains("not included in the completed figures"));
        assert!(SERIAL_BACKPRESSURE_TOOLTIP
            .text()
            .contains("advisory and can be lost"));
        assert!(CHUNK_SHAPE_TOOLTIP.text().contains("full channel run"));
        assert!(CHUNK_SHAPE_TOOLTIP
            .text()
            .contains("not the recent ten-second window"));
        assert!(ARRIVAL_TIMESTAMP_TOOLTIP
            .text()
            .contains("does not mean it is accurate to a nanosecond"));
        assert!(ARRIVAL_TIMESTAMP_TOOLTIP
            .text()
            .contains("Handoff and Read gap always use the post-read clock"));
        assert!(RAW_QUEUE_ACTIVE_TOOLTIP
            .text()
            .contains("most recent status update"));
        assert!(RAW_QUEUE_ACTIVE_TOOLTIP
            .text()
            .contains("Reception continues throughout"));
        assert!(RAW_QUEUE_FAULTED_TOOLTIP
            .heading()
            .contains("could not begin"));
        assert!(RAW_QUEUE_GAP_TOOLTIP
            .heading()
            .contains("received bytes are not being recorded"));
        assert!(RAW_QUEUE_GAP_TOOLTIP.text().contains("Reception continues"));
    }

    /// Every explanatory hover opens with a one-sentence heading, and no
    /// paragraph break falls inside the heading.
    #[test]
    fn every_explanatory_hover_opens_with_a_single_sentence_heading() {
        for hover in [
            &TRANSPORT_TOOLTIP,
            &PRESSURE_TOOLTIP,
            &PIPELINE_TOOLTIP,
            &UDP_DROP_CALLOUT_TOOLTIP,
            &SERIAL_BACKPRESSURE_TOOLTIP,
            &RECEIVE_DETAILS_TOOLTIP,
            &HANDOFF_TOOLTIP,
            &PROCESSING_TOOLTIP,
            &CHUNK_SHAPE_TOOLTIP,
            &ARRIVAL_TIMESTAMP_TOOLTIP,
            &UDP_NOT_APPLICABLE_TOOLTIP,
            &UDP_UNAVAILABLE_TOOLTIP,
            &UDP_AVAILABLE_TOOLTIP,
            &INGEST_QUEUE_TOOLTIP,
            &RAW_QUEUE_ACTIVE_TOOLTIP,
            &RAW_QUEUE_GAP_TOOLTIP,
            &RAW_QUEUE_RETAINED_TOOLTIP,
            &RAW_QUEUE_FAULTED_TOOLTIP,
            &SHARED_PORT_TOOLTIP,
            &RECEIVE_BUFFER_TOOLTIP,
        ] {
            let heading = hover.heading();
            assert!(heading.ends_with('.'), "{heading}");
            assert!(hover.text().len() > heading.len(), "{heading}");
        }
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

    #[test]
    fn the_receive_buffer_line_says_when_less_was_granted() {
        assert_eq!(
            receive_buffer_words(ReceiveBuffer {
                requested: 4_194_304,
                granted: 4_194_304,
            }),
            "Receive buffer: 4.194 MB granted"
        );
        assert_eq!(
            receive_buffer_words(ReceiveBuffer {
                requested: 4_194_304,
                granted: 212_992,
            }),
            "Receive buffer: 212.992 kB granted, less than the 4.194 MB requested"
        );
    }

    #[test]
    fn the_shared_port_line_says_what_the_os_applied() {
        assert_eq!(shared_port_words(SharedPortStatus::NotRequested), None);
        assert_eq!(
            shared_port_words(SharedPortStatus::Applied),
            Some("Shared port: applied · other programs can bind it")
        );
        assert_eq!(
            shared_port_words(SharedPortStatus::NotApplied),
            Some("Shared port: requested · the OS did not apply it · not shared")
        );
        // It reports the option, not whether anyone else is bound.
        assert!(SHARED_PORT_TOOLTIP
            .text()
            .contains("cannot say whether another program is actually bound"));
    }

    /// Nothing in the panel may teach the state Listener no longer has.
    #[test]
    fn no_readout_teaches_the_retired_warm_up_state() {
        for hover in [
            PIPELINE_TOOLTIP,
            RECEIVE_DETAILS_TOOLTIP,
            CHUNK_SHAPE_TOOLTIP,
            HANDOFF_TOOLTIP,
            PROCESSING_TOOLTIP,
        ] {
            let tooltip = hover.text();
            assert!(
                !tooltip.to_ascii_lowercase().contains("warm"),
                "tooltip still teaches a warm-up: {tooltip}"
            );
            assert!(!tooltip.contains("20 samples"), "{tooltip}");
        }
    }
}
