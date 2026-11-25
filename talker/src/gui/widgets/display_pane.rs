//! The Output display pane (spec §5.7).

use egui::{Align, Layout};
use wiredata_ui::diagnostics::{dismissible_attention_callout, SignalTone};

use crate::gui::diagnostics::LIVE_UPDATE_QUEUE_TOOLTIP;
use crate::gui::display::{ChannelDisplay, ControlStyle, DisplayMode};
use wiredata_ui::hover::{Hover, ShowHover};

use crate::gui::notice::DismissedNotice;

use super::*;

// ── Display pane ──────────────────────────────────────────────────────────────

pub(super) fn output_layout_job(
    ui: &egui::Ui,
    text: &str,
    replacement_ranges: &[std::ops::Range<usize>],
) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let normal = ui.visuals().text_color();
    let (replacement_text, replacement_background) = replacement_highlight_colors(ui);
    let mut job = egui::text::LayoutJob::default();
    let mut cursor = 0;
    for range in replacement_ranges {
        debug_assert!(range.start >= cursor && range.end <= text.len());
        if cursor < range.start {
            job.append(
                &text[cursor..range.start],
                0.0,
                egui::TextFormat {
                    font_id: font.clone(),
                    color: normal,
                    ..Default::default()
                },
            );
        }
        job.append(
            &text[range.clone()],
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: replacement_text,
                background: replacement_background,
                ..Default::default()
            },
        );
        cursor = range.end;
    }
    if cursor < text.len() || text.is_empty() {
        job.append(
            &text[cursor..],
            0.0,
            egui::TextFormat {
                font_id: font,
                color: normal,
                ..Default::default()
            },
        );
    }
    job
}

/// Render a channel's real-time outbound display pane (spec §5.7).
///
/// Three lines can head the pane. The warning and gauge describe the shared
/// live-update queue, which can affect Output and diagnostic readouts; the
/// sampling note qualifies Output alone. The shared queue has one visible home
/// here rather than being repeated in the diagnostics card. Its warning is
/// dismissible because it is a run total that keeps its last value long after
/// the pressure that caused it passed; `dismissed` carries the count the reader
/// has already acknowledged (see `super::notice`).
///
/// An unacknowledged warning also marks the section's own header. The pane is
/// collapsed by default, and moving this warning here took away the diagnostics
/// badge that used to raise it, so stating it only inside would have put the
/// condition somewhere a reader can sit in front of and never see. The header
/// carries a stable `id_salt`, so a heading that changes with the count does not
/// read as a different section and collapse itself.
pub(in crate::gui) fn show_display_pane(
    ui: &mut egui::Ui,
    display: &mut ChannelDisplay,
    dismissed: &mut DismissedNotice,
    accepted_total: u64,
    dropped_updates: u64,
    queue: LiveUpdateQueueGauge,
) {
    let unacknowledged = dropped_updates > 0 && dismissed.showing(dropped_updates);
    let heading = if unacknowledged {
        egui::RichText::new(format!("Output  \u{26A0} {dropped_updates} dropped"))
            .color(wiredata_ui::palette::active(ui).warning)
    } else {
        egui::RichText::new("Output")
    };
    egui::CollapsingHeader::new(heading)
        .id_salt("output_pane")
        .show(ui, |ui| {
            // Ordered by how much each notice qualifies the pane: dropped updates
            // can cost any kind of update, sampling only costs payload lines.
            // `showing` was consulted once above, for the header: it re-arms a
            // record left from a previous run, so calling it again here would be a
            // second mutation in one frame.
            if unacknowledged {
                let hover = LIVE_UPDATE_QUEUE_TOOLTIP.layout(ui.style());
                let acknowledged = dismissible_attention_callout(
                    ui,
                    "live_update_drop_attention",
                    format!(
                        "{dropped_updates} live updates dropped; Output may omit lines and live \
                         readouts may lag"
                    ),
                    SignalTone::Warning,
                    hover,
                );
                if acknowledged {
                    dismissed.dismiss(dropped_updates);
                }
                ui.add_space(4.0);
            }
            // The payload observer is rate-limited independently of cumulative
            // counters. Drive the badge from proven accepted-vs-sampled omission
            // so a throughput estimate can neither conceal nor invent sampling.
            let sample_hz = 1.0
                / crate::core::runner::ObserverPolicy::sampled()
                    .sample_interval
                    .as_secs_f32();
            // The gauge sits with the warning it explains. This shared queue
            // also feeds live diagnostic readouts (as the tooltip says); its
            // placement does not make it an Output-only measurement.
            ui.label(
                egui::RichText::new(format!(
                    // "of", not a slash: "/" on this screen means "per".
                    "live update queue at last screen check: {} of {} · peak {} · {} dropped",
                    queue.len, queue.capacity, queue.peak, dropped_updates
                ))
                .small()
                .color(ui.visuals().weak_text_color()),
            )
            .on_hover(&LIVE_UPDATE_QUEUE_TOOLTIP);
            if display.payload_samples_omitted(accepted_total) {
                // A standing note about how the pane works, not a state to act
                // on, so it recedes with the theme: no accent, no ⚠, and the
                // same quiet weight as the queue gauge above it. The callout is
                // what carries urgency when there is any.
                ui.label(
                    egui::RichText::new(format!(
                        "sampled output · not every sent payload is shown · limit ~{sample_hz:.0}/s"
                    ))
                    .small()
                    .color(ui.visuals().weak_text_color()),
                )
                .on_hover(&Hover::from(format!(
                    "Above about {sample_hz:.0} messages a second, Output shows a sample of the \
                     messages, not every one.\n\n\
                     That keeps the pane's drawing cost the same at any send rate. This line \
                     appears once the count of sent messages shows that at least one was left \
                     out, and stays for the rest of the run. A full live update queue can also \
                     leave one out.\n\n\
                     The run's counts are exact; live readouts can lag until a later update. \
                     None of these figures shows that the data reached the wire or a receiver."
                )));
                ui.separator();
            }
            ui.horizontal(|ui| {
                ui.label("View:").on_hover(&Hover::new(
                    "The view changes only how Output shows the bytes.\n\n\
                     What is sent is the same in every view.",
                ));
                ui.radio_value(&mut display.mode, DisplayMode::Hex, "Hex");
                ui.radio_value(&mut display.mode, DisplayMode::Rendered, "Rendered");
                // Raw comes last so the ctrl-chars sub-options below sit
                // immediately next to the radio they modify.
                ui.radio_value(&mut display.mode, DisplayMode::Raw, "Raw");
                // Wrap the conditional ctrl-chars block in a stable id scope so
                // its appearance / disappearance can't shift the auto-derived
                // ids of the surrounding widgets (Clear button, etc.) and trip
                // egui's "duplicate widget id" warnings on view-mode changes.
                ui.push_id("ctrl_chars_block", |ui| {
                    if display.mode == DisplayMode::Raw {
                        ui.separator();
                        ui.label("ctrl-chars:");
                        ui.radio_value(
                            &mut display.control_style,
                            ControlStyle::Pictures,
                            "Pictures (\u{240A})",
                        );
                        ui.radio_value(&mut display.control_style, ControlStyle::Brackets, "[LF]");
                        ui.radio_value(
                            &mut display.control_style,
                            ControlStyle::HexEscapes,
                            "<0A>",
                        );
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Clear").clicked() {
                        display.clear();
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(150.0)
                .stick_to_bottom(true)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    // One logical label preserves exact selection and copying:
                    // egui's multi-widget selection inserts separators between
                    // labels and cannot retain virtualized-offscreen endpoints.
                    let (text, replacement_ranges) = display.rendered();
                    let job = output_layout_job(ui, text, replacement_ranges);
                    ui.add(egui::Label::new(job).wrap().selectable(true));
                });
        });
}
