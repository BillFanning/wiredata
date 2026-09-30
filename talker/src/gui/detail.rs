//! The detail pane: everything about the **selected** channel — header
//! (name, summary, actions), the Connection editor, the Messages
//! editor, and the Output display pane. One channel on screen at a time;
//! the channel list in [`super::channels`] picks which.

use egui::{Align, Layout};

use crate::core::{
    capacity::{
        measured_service_estimate, serial_line_estimate, service_sample_count, ChannelDemand,
        MessageDemand, MIN_SERVICE_SAMPLES,
    },
    channel::InterfaceConfig,
    message::NmeaChecksumMode,
    run_summary::RunSummary,
    telemetry::{recent_snapshot_state, RecentSnapshotState},
    timing::{ActiveCadence, CadenceAlignment, TimerReason},
};

use wiredata_ui::{
    diagnostics::{
        attention_callout, decision_card, dismissible_attention_callout, signal_grid, signal_row,
        SignalTone,
    },
    fonts::bold,
    format::{
        compact_duration, human_byte_rate, human_bytes, percent, serial_port_hint, thousands,
    },
    glyphs,
};

use super::draft::{ConnKind, PayloadKind, ScheduleDraft};
use super::widgets::{
    checksum_label, code_page_label, hex_valid, invalid_parse, lifecycle_indicator,
    marker_aware_text_edit, message_editor_max_height, plain_text_edit_with_cursor,
    preview_ascii_layout_job, red_bordered, show_display_pane, show_insert_byte_button,
    show_insert_unit_button, show_interface_summary, show_serial_fields, show_tcp_fields,
    show_udp_fields, start_button, LiveUpdateQueueGauge, UppercaseHex,
};
use super::{MessageAnalysisCache, MessageDraftAnalysis, MessagePreview, TalkerApp};
use wiredata_ui::palette::active as theme_palette;
// The decision layer this panel renders. Imported wholesale: every item in
// `diagnostics` exists to be shown here, and the two stay in step as signals
// are added.
use super::diagnostics::*;

/// The per-message breakdown: one row per message, with what it suffered and
/// what it cost the others side by side (ADR-045).
fn show_per_message_table(ui: &mut egui::Ui, rows: &[MessageRow]) {
    let pal = theme_palette(ui);
    egui::Grid::new("per_message_grid")
        .num_columns(7)
        .spacing(egui::vec2(14.0, 4.0))
        .striped(true)
        .show(ui, |ui| {
            for heading in [
                "Msg",
                "Interval",
                "Sends",
                "Late",
                "Send call",
                "Longest block",
                "Cost to others",
            ] {
                ui.label(bold(heading).size(12.0));
            }
            ui.end_row();

            for row in rows {
                ui.label(egui::RichText::new(&row.label).size(12.0));
                ui.label(egui::RichText::new(&row.interval).size(12.0));
                ui.label(egui::RichText::new(&row.sends).size(12.0));
                ui.label(egui::RichText::new(&row.late).size(12.0));
                ui.label(egui::RichText::new(&row.send_call).size(12.0));
                // Only the culprit column is toned. Lateness is not scored
                // against an invented budget, so a late row stays neutral;
                // having delayed another message is an attributable fact.
                let hold = egui::RichText::new(&row.longest_block).size(12.0);
                ui.label(if row.costs_others {
                    hold.color(pal.warning)
                } else {
                    hold
                });
                let caused = egui::RichText::new(&row.cost_to_others).size(12.0);
                ui.label(if row.costs_others {
                    caused.color(pal.warning)
                } else {
                    caused
                });
                ui.end_row();
            }
        });
}

fn show_last_run_summary(ui: &mut egui::Ui, summary: &RunSummary) {
    let unsent = summary.unsent_sends();
    let heading = format!(
        "Last completed run · {} · {} sent · {unsent} unsent",
        compact_duration(summary.elapsed),
        summary.total_count,
    );
    egui::CollapsingHeader::new(heading)
        .id_salt("last_completed_run")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Copy summary").clicked() {
                    ui.ctx().copy_text(summary.to_report_text());
                }
                ui.weak(format!(
                    "Talker {} · {} · {}/{}",
                    env!("CARGO_PKG_VERSION"),
                    if cfg!(debug_assertions) {
                        "debug"
                    } else {
                        "release"
                    },
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                ));
            });
            ui.weak(format!(
                "Started {} · finished {}",
                summary.started_utc(),
                summary.finished_utc(),
            ));
            // Same shape as the live send-outcomes line: the aggregate, then
            // its parts in parentheses.
            ui.weak(format!(
                "Sent: {} · {} messages · {} unsent ({} failed · {} suppressed · {} missed)",
                human_bytes(summary.total_bytes),
                summary.total_count,
                unsent,
                summary.failed_sends,
                summary.suppressed_sends,
                summary.missed_sends,
            ))
            .on_hover_text(SENT_MEANING_TOOLTIP);
            let (timer_detail, timer_hot) = timer_status_detail(summary.timer);
            let timer = egui::RichText::new(format!("Timer: {timer_detail}")).weak();
            ui.label(if timer_hot {
                timer.color(theme_palette(ui).warning)
            } else {
                timer
            })
            .on_hover_text(TIMER_TOOLTIP);

            // Same timing model as every live readout (ADR-046): the worst
            // value with its sample count, and a percentile only where it
            // differs. A completed run had its own p99-only form, which read as
            // a different measurement of the same thing.
            let timing = summary.timing.cumulative;
            let timing_text = if timing.deadline_lateness.sample_count() == 0 {
                "Timing: no deadline samples".to_owned()
            } else {
                {
                    let samples = timing.send_duration.sample_count();
                    format!(
                        "Timing (run, {} sends): {} · {} · {}",
                        thousands(samples),
                        timing_metric("late", timing.deadline_lateness, samples),
                        longest_metric("render", timing.render_duration, samples),
                        longest_metric("send call", timing.send_duration, samples),
                    )
                }
            };
            ui.weak(timing_text).on_hover_text(TIMING_TOOLTIP);
        });
}

impl TalkerApp {
    /// Render the central detail pane for the selected channel (or a hint
    /// when there is none).
    pub(super) fn show_detail(&mut self, ui: &mut egui::Ui) {
        let Some(i) = self.selected.filter(|&i| i < self.conn_drafts.len()) else {
            ui.centered_and_justified(|ui| {
                ui.weak(if self.conn_drafts.is_empty() {
                    "No channels — use “+ Add” in the channel list to create one."
                } else {
                    "Select a channel on the left."
                });
            });
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.push_id(i, |ui| {
                let running = self.is_connection_running(i);
                self.show_channel_header(ui, i, running);
                ui.separator();
                self.show_channel_body(ui, i, running);
                ui.separator();
                // The sent total proves whether Output payload updates
                // were omitted; retained Output history is a separate concern.
                // Dropped updates are the pane's other completeness limit and
                // are read from the same snapshot.
                let (sent_total, dropped_updates, queue) = self
                    .sup
                    .telemetry_ref(i)
                    .map(|telemetry| {
                        (
                            telemetry.total_count,
                            telemetry.dropped_statuses,
                            LiveUpdateQueueGauge {
                                len: telemetry.queue_len,
                                peak: telemetry.queue_peak,
                                capacity: super::STATUS_QUEUE_CAP,
                            },
                        )
                    })
                    .unwrap_or((
                        0,
                        0,
                        LiveUpdateQueueGauge {
                            len: 0,
                            peak: 0,
                            capacity: super::STATUS_QUEUE_CAP,
                        },
                    ));
                let view = &mut self.views[i];
                show_display_pane(
                    ui,
                    &mut view.display,
                    &mut view.notices.dropped_updates,
                    sent_total,
                    dropped_updates,
                    queue,
                );
            });
        });
    }

    /// The detail header, laid out like listener's channel block so the two
    /// apps read as one product: name row (status glyph · name · label), a
    /// `status · interface` row, sent/unsent totals, throughput, the
    /// performance readouts for high-rate health, and the lifecycle button
    /// pair. Channel removal lives on the channel-list rows (the ✕ overlay),
    /// as in listener.
    fn show_channel_header(&mut self, ui: &mut egui::Ui, i: usize, running: bool) {
        let pal = theme_palette(ui);
        // Owned snapshot of the channel's telemetry (ADR-019): the readouts
        // are rendered across several `&mut self` widget closures.
        let telemetry = self.sup.telemetry(i);
        let recent_snapshot_state = recent_snapshot_state(
            telemetry.recent_timing_captured_at,
            std::time::Instant::now(),
            telemetry.recent_timing_is_final,
        );
        let error: Option<String> = telemetry.banner_error().map(str::to_owned);
        let (glyph, glyph_color, status_word) = lifecycle_indicator(running, error.is_some(), pal);
        let draft_kind = self.conn_drafts[i].kind();
        let draft_interface = self.conn_drafts.get(i).and_then(|draft| draft.to_config());
        let (iface_drift, run_drift) = self.detect_drift(i, draft_interface.as_ref());

        // Name row: status glyph (listener's symbol set/colors, painted into
        // a fixed cell so status changes never shift the row) + editable
        // name + label.
        ui.horizontal(|ui| {
            glyphs::paint_glyph(ui, glyph, glyphs::glyph_size(glyph), glyph_color);
            // Editable display name (cosmetic — channels are positional).
            // The hint shows the positional fallback the list uses when the
            // name is empty.
            let hint = format!("Channel {}", i + 1);
            let name_r = ui.add(
                egui::TextEdit::singleline(&mut self.conn_drafts[i].name)
                    .id_salt("channel_name")
                    .desired_width(140.0)
                    .hint_text(hint),
            );
            if name_r.changed() {
                self.dirty = true;
            }
            ui.label(bold("Name"))
                .on_hover_text("This channel's display name, shown in the channel list.");
            // Duplicate names are allowed (nothing is keyed by them) but
            // worth a nudge — two identical rows in the list are confusing.
            let name = &self.conn_drafts[i].name;
            let duplicate = !name.is_empty()
                && self
                    .conn_drafts
                    .iter()
                    .enumerate()
                    .any(|(j, d)| j != i && d.name == *name);
            ui.push_id("dup_name_hint", |ui| {
                if duplicate {
                    ui.label(
                        egui::RichText::new("duplicate name")
                            .color(pal.warning)
                            .size(11.0),
                    )
                    .on_hover_text("Another channel has the same name — allowed, but confusing.");
                }
            });
        });

        // Status · interface row (listener's `running · details` line).
        // A stable id scopes the summary, whose red `?` pills may come and go
        // between egui's two layout passes.
        ui.horizontal(|ui| {
            ui.label(status_word);
            ui.label("·");
            ui.push_id("iface_summary", |ui| {
                show_interface_summary(ui, &self.conn_drafts[i]);
            });
        });

        // Decision-level diagnostics stay compact; the evidence and caveats
        // remain one click away in the card's details section.
        let detail_line = |ui: &mut egui::Ui, text: String, hot: bool, tip: &str| {
            let rt = egui::RichText::new(text).weak();
            ui.label(if hot { rt.color(pal.warning) } else { rt })
                .on_hover_text(tip);
        };

        let msgs = telemetry.total_count;
        let bytes = telemetry.total_bytes;
        let (mps, bps) = self
            .views
            .get(i)
            .map(|v| (v.rate.per_sec, v.rate.bytes_per_sec))
            .unwrap_or((0.0, 0.0));
        let missed = telemetry.missed_sends;
        let failed = telemetry.failed_sends;
        let suppressed = telemetry.suppressed_sends;
        let outcomes = send_outcomes(msgs, failed, suppressed, missed);
        let outcomes_tip = send_outcomes_tooltip(msgs, failed, suppressed, missed);

        // Capacity describes the configuration that is actually sending. The
        // runner reports each message's wire size and interval, so a running
        // channel's demand is its own; the settings on screen are used only
        // when there is no run to describe, and are labelled a projection.
        let running_demand = (!telemetry.per_message_timing.is_empty()).then(|| {
            ChannelDemand::from_messages(
                telemetry
                    .per_message_timing
                    .iter()
                    .map(|message| MessageDemand::new(message.wire_bytes, message.interval_ms())),
            )
        });
        // Never re-renders payloads in this per-frame header path: the draft
        // path folds already-memoized wire lengths.
        let drafted_demand = self
            .sched_drafts
            .get(i)
            .zip(self.message_analysis.get(i))
            .and_then(|(messages, analyses)| analyzed_channel_demand(messages.len(), analyses));
        let projecting = running_demand.is_none();
        let demand = running_demand.or(drafted_demand);

        // The interface follows the same rule: the one the runner confirmed
        // open, so a serial verdict is about the baud that is carrying bytes.
        let applied_interface = self.sup.applied_interface(i).cloned();
        let capacity_interface = applied_interface.as_ref().or(draft_interface.as_ref());
        let serial = demand.and_then(|demand| {
            if !demand.is_active() {
                return None;
            }
            let InterfaceConfig::Serial(config) = capacity_interface? else {
                return None;
            };
            serial_line_estimate(demand, config)
        });
        let (service_source, service_timing) = select_service_timing(
            recent_snapshot_state,
            telemetry.recent_timing,
            telemetry.timing,
        );
        let service_source_label =
            service_timing_source_label(service_source, recent_snapshot_state);
        let service_estimate = demand
            .filter(|demand| demand.is_active())
            .and_then(|demand| measured_service_estimate(demand, service_timing));
        let service_samples = service_sample_count(service_timing);
        // Demand now comes from the running schedule whenever there is one, so
        // unapplied edits no longer make this figure describe something other
        // than the run. Only a channel with no run to describe is a projection.
        let app_label = if projecting { "app (projected)" } else { "app" };

        let app_capacity = if let Some(estimate) = service_estimate {
            let mut text = format!(
                "{app_label} ~{} headroom",
                compact_factor(estimate.headroom_factor())
            );
            if service_source == ServiceTimingSource::Run
                && matches!(recent_snapshot_state, RecentSnapshotState::Expired(_))
            {
                text.push_str(" · run-wide timing · recent timing unavailable");
            }
            text
        } else if service_samples == 0 {
            format!("{app_label} unmeasured")
        } else if service_samples < MIN_SERVICE_SAMPLES {
            format!("{app_label} warming {service_samples}/{MIN_SERVICE_SAMPLES}")
        } else {
            format!("{app_label} estimate unavailable")
        };
        let capacity = match demand {
            None => DecisionSignal {
                text: format!(
                    "Complete message setup to calculate · {} sent (~5 s)",
                    compact_rate(f64::from(mps), "msg/s")
                ),
                tone: SignalTone::Neutral,
            },
            Some(demand) if !demand.is_active() => DecisionSignal {
                text: format!(
                    "No active messages · {} sent (~5 s)",
                    compact_rate(f64::from(mps), "msg/s")
                ),
                tone: SignalTone::Neutral,
            },
            Some(demand) => {
                let line_capacity = if let Some(line) = serial {
                    format!("serial {}", percent(line.utilization * 100.0))
                } else {
                    unavailable_line_capacity_label(draft_kind).to_owned()
                };
                DecisionSignal {
                    text: format!(
                        "{} requested / {} sent (~5 s) · {line_capacity} · {app_capacity}",
                        compact_rate(demand.messages_per_second, "msg/s"),
                        compact_rate(f64::from(mps), "msg/s"),
                    ),
                    tone: if serial.is_some_and(|line| line.is_oversubscribed()) {
                        SignalTone::Fault
                    } else if serial.is_some_and(|line| line.utilization >= 0.8)
                        || service_estimate.is_some_and(|estimate| estimate.utilization >= 0.8)
                    {
                        SignalTone::Warning
                    } else {
                        SignalTone::Neutral
                    },
                }
            }
        };

        let cumulative_timing = telemetry.timing;
        let timing = telemetry.recent_timing;
        let timing_text = timing_detail_text(timing, cumulative_timing, recent_snapshot_state);
        // Runtime truth first: the running schedule's own active cadences. The
        // draft's shortest interval is only a fallback for a channel that has
        // not run yet, and carries no message count, so a draft-only channel
        // says how often it will send without claiming how many messages did.
        let active_cadence = telemetry.timer.active_cadence.or_else(|| {
            let demand = demand?;
            let shortest = demand.shortest_interval?;
            Some(ActiveCadence {
                messages: demand.active_messages,
                shortest,
            })
        });
        // Interval detail for the schedule phrase, from exactly one source: the
        // running schedule's own per-message intervals once the channel has
        // reported any, the on-screen draft otherwise. Mixing them would let a
        // stale draft interval appear inside a line of measured runtime facts.
        let drafted_intervals = self
            .message_analysis
            .get(i)
            .and_then(|analyses| draft_intervals(analyses));
        let has_runtime_timing = !telemetry.per_message_timing.is_empty();
        let cadence_groups = if has_runtime_timing {
            cadence_groups(
                telemetry
                    .per_message_timing
                    .iter()
                    .map(|message| message.interval),
            )
        } else {
            drafted_intervals
                .clone()
                .map(cadence_groups)
                .unwrap_or_default()
        };
        // An unparseable interval is an unfinished edit, not an idle channel.
        let cadence_setup_incomplete = !has_runtime_timing && drafted_intervals.is_none();
        let cadence_decision = cadence_decision(
            timing,
            cumulative_timing,
            active_cadence,
            &cadence_groups,
            cadence_setup_incomplete,
            recent_snapshot_state,
        );

        let timer_prefix = if running {
            "Timer"
        } else if telemetry.timer.active_cadence.is_some()
            || telemetry.timer.reason != TimerReason::None
        {
            "Timer (last run)"
        } else {
            "Timer"
        };
        let (timer_detail, timer_hot) = timer_status_detail(telemetry.timer);
        let cadence = match telemetry.timer.cadence_alignment {
            CadenceAlignment::Immediate => "immediate start".to_owned(),
            CadenceAlignment::UtcPhase if telemetry.timer.clock_realignments == 0 => {
                "UTC phase".to_owned()
            }
            CadenceAlignment::UtcPhase => format!(
                "UTC phase · {} wall-clock rebases",
                telemetry.timer.clock_realignments
            ),
        };
        // The send-outcome tone still escalates the card badge even though the
        // counts themselves now live above the card: a failing interface must
        // read as ISSUE there, and the adjacent line carries the reason.
        let card_tone = diagnostic_card_tone(outcomes.tone, capacity.tone, timer_hot, running);
        // Read before the card so the closure below can stay free of `self`.
        // `showing` mutates — it re-arms a record left over from a previous run
        // — so this is the one call per frame, and dismissal is applied after.
        let missed_notice_showing = self.views[i].notices.missed_sends.showing(missed);
        let mut missed_acknowledged = false;
        let card_status = match card_tone {
            SignalTone::Fault => "ISSUE",
            SignalTone::Warning => "ATTENTION",
            SignalTone::Healthy => "LIVE",
            SignalTone::Neutral => "IDLE",
        };

        // Counted facts sit directly beneath the status · interface row, in
        // exactly one place: the run's send outcomes, then what was sent.
        // The card below holds only the readouts that need interpretation.
        ui.add(
            egui::Label::new(
                egui::RichText::new(&outcomes.text).color(match outcomes.tone {
                    SignalTone::Fault => pal.fault,
                    SignalTone::Warning => pal.warning,
                    _ => ui.visuals().text_color(),
                }),
            )
            .wrap()
            .sense(egui::Sense::hover()),
        )
        .on_hover_text(&outcomes_tip);
        detail_line(
            ui,
            format!(
                "Sent: {} total · {} · {:.1} msg/s (~5 s)",
                human_bytes(bytes),
                human_byte_rate(f64::from(bps)),
                mps
            ),
            false,
            THROUGHPUT_TOOLTIP,
        );

        // Lifecycle controls sit directly under the basic readouts, matching
        // listener's control block. Below the diagnostics they were reachable
        // only after scrolling past several collapsible sections, so the same
        // pair of buttons lived in visibly different places in the two apps.
        if let Some(err) = &error {
            ui.colored_label(pal.fault, format!("\u{26A0} {err}"));
            // Only the UI knows what is currently enumerated, so only the UI can
            // say whether the port the OS called absent is still in the list.
            if draft_kind == ConnKind::Serial {
                let port = self.conn_drafts[i].serial_port.clone();
                if !port.is_empty() {
                    let listed = self.serial_ports.contains(&port);
                    ui.colored_label(pal.warning, serial_port_hint(&port, listed));
                }
            }
        }
        ui.add_space(12.0); // a blank line between the readouts and the buttons
        self.show_lifecycle_buttons(ui, i, running, iface_drift || run_drift, error.is_some());
        ui.add_space(6.0);

        decision_card(ui, "", card_status, card_tone, |ui| {
            signal_grid(ui, "send_decisions", |ui| {
                signal_row(
                    ui,
                    "Cadence",
                    &cadence_decision.text,
                    cadence_decision.tone,
                    cadence_tooltip(
                        active_cadence,
                        &cadence_groups,
                        timing.deadline_lateness.sample_count(),
                        cumulative_timing.deadline_lateness.sample_count(),
                        recent_snapshot_state,
                    ),
                );
                signal_row(
                        ui,
                        "Capacity",
                        &capacity.text,
                        capacity.tone,
                        "Requested load comes from the running schedule: each message's wire size and interval as the channel is actually sending them. A channel that is not running has none, so it is projected from the settings shown and labelled as such. Sent rate is the rolling five-second average of configured-interface writes that returned success. Serial utilization is a theoretical UART line estimate. Application headroom compares that same requested load with separate render and interface-write p99 bounds. Timing from the last ~10 seconds is preferred after enough observations; timing whose update is ten seconds old is discarded, and a clearly labelled run-wide fallback is used when available. This is an advisory projection, not a hard capacity promise.",
                    );
            });

            // Skipped cadence points name a cause rather than a count: the
            // count is already on the send-outcomes line above, and the message
            // showing the misses is rarely the one causing them (ADR-045).
            //
            // Dismissible because it is advice, not a count: the misses
            // themselves stay on the send-outcomes line above and keep this
            // card's badge raised, so acknowledging the routing hides where to
            // look, never what happened.
            let mut missed_routing_shown = false;
            if missed_notice_showing {
                if let Some(routing) = missed_send_routing(
                    &MissedSendEvidence {
                        missed,
                        // A command failure also appears in the banner, but it
                        // says nothing about the interface or retry backoff.
                        // Only the interface-specific state can support this
                        // branch; `failed` remains a run total that may have
                        // recovered.
                        interface_erroring: telemetry.last_error.is_some(),
                        failed,
                        serial_oversubscribed: serial.is_some_and(|line| line.is_oversubscribed()),
                        service: service_estimate,
                    },
                    &telemetry.per_message_timing,
                ) {
                    ui.add_space(4.0);
                    missed_acknowledged = dismissible_attention_callout(
                        ui,
                        "missed_send_routing",
                        routing.text,
                        routing.tone,
                        MISSED_ROUTING_TOOLTIP,
                    );
                    missed_routing_shown = true;
                }
            }

            // No unsent callout: the send-outcomes line above the card already
            // carries the counts and its own tone, and the badge escalates from
            // the same signal. Repeating it here was the third rendering of one
            // fact.
            if let Some(line) = serial.filter(|line| line.is_oversubscribed()) {
                ui.add_space(4.0);
                attention_callout(
                        ui,
                        "serial_capacity_attention",
                        format!(
                            "Serial demand is {} of line capacity · needs {} (baud {})",
                            percent(line.utilization * 100.0),
                            compact_rate(line.required_bits_per_second, "bit/s"),
                            line.minimum_baud(),
                        ),
                        SignalTone::Fault,
                        "The sustained requested payload cannot physically fit at the configured baud. The warning remains advisory so deliberate overload tests are still possible.",
                    );
            }
            if timer_hot {
                ui.add_space(4.0);
                attention_callout(
                        ui,
                        "timer_request_attention",
                        "Windows 1 ms timer request failed; cadence waits are using the fallback",
                        SignalTone::Warning,
                        "The channel continues with ordinary deadline waits. The failed request does not prove that any send was late; inspect measured deadline lateness and Missed cadence points for the observed effect.",
                    );
            }
            // The dropped-update callout and its one queue gauge are shown
            // together above the Output pane. Do not repeat either here; the
            // tooltip there states that the shared queue can also delay these
            // live readouts.

            ui.add_space(5.0);
            egui::CollapsingHeader::new("Timing & runtime details")
                    .id_salt("timing_runtime_details")
                    .default_open(false)
                    .show(ui, |ui| {
                        // Send outcomes and sent totals/rates are not
                        // repeated here — they are always visible above the
                        // card, so this section carries only what the compact
                        // readouts leave out.
                        //
                        // Shown only while the callout above is: the limits
                        // qualify that routing, and without it they describe
                        // nothing on screen.
                        if missed_routing_shown {
                            detail_line(
                                ui,
                                MISSED_ROUTING_LIMITS.to_owned(),
                                false,
                                "How far the missed-send routing above can be trusted.",
                            );
                        }
                        match demand {
                            None => detail_line(
                                ui,
                                "Capacity: complete all messages to calculate".to_owned(),
                                false,
                                "Capacity uses exact compiled wire lengths and active message intervals. Fix the message validation errors first.",
                            ),
                            Some(demand) if !demand.is_active() => detail_line(
                                ui,
                                "Capacity: no active messages".to_owned(),
                                false,
                                "Messages with interval 0 are dormant and create no scheduled wire demand.",
                            ),
                            Some(demand) => {
                                let requested = format!(
                                    "{} · {}",
                                    compact_rate(demand.messages_per_second, "msg/s"),
                                    compact_rate(demand.bytes_per_second, "B/s")
                                );
                                if let Some(line) = serial {
                                    let percentage = percent(line.utilization * 100.0);
                                    let (text, hot) = if line.is_oversubscribed() {
                                        (
                                            format!(
                                                "Capacity: {requested} · serial OVER CAPACITY {percentage} · needs {} (baud {})",
                                                compact_rate(line.required_bits_per_second, "bit/s"),
                                                line.minimum_baud(),
                                            ),
                                            true,
                                        )
                                    } else if let Some(headroom) = line.headroom_factor() {
                                        (
                                            format!(
                                                "Capacity: {requested} · serial {percentage} · {} line headroom",
                                                compact_factor(headroom)
                                            ),
                                            line.utilization >= 0.8,
                                        )
                                    } else {
                                        (format!("Capacity: {requested} · serial idle"), false)
                                    };
                                    detail_line(
                                        ui,
                                        text,
                                        hot,
                                        "Calculated, not measured, from the running schedule and the baud the channel actually opened. Each wire byte uses one UART frame: 1 start bit plus the configured data, parity, and stop bits. Above 100%, the sustained requested payload cannot physically fit at the configured baud. At or below 100% is not a real-time guarantee: flow control, adapter/driver buffering, operating-system delays, and same-deadline message bursts can reduce effective headroom. This warning is advisory so deliberate overload tests remain possible.",
                                    );
                                } else if draft_kind == ConnKind::Serial {
                                    detail_line(
                                        ui,
                                        format!(
                                            "Capacity: requested {requested} · complete Serial setup"
                                        ),
                                        false,
                                        "Complete the Serial port and line settings before Talker can calculate UART utilization. The requested message and byte rates still come from exact compiled wire lengths and active intervals.",
                                    );
                                } else {
                                    detail_line(
                                        ui,
                                        format!("Capacity: requested {requested}"),
                                        false,
                                        "Calculated, not measured: the exact wire length of each message against its interval, taken from the running schedule where there is one and from the settings shown otherwise. Network link capacity is unknown, so Talker reports requested load without inventing a physical headroom figure.",
                                    );
                                }

                                if let Some(estimate) = service_estimate {
                                    // Whose schedule, and whose timing. A live
                                    // channel is described by its own; a
                                    // stopped one can only be projected.
                                    let estimate_kind = if projecting {
                                        "settings shown, timing from the last run"
                                    } else {
                                        "running configuration"
                                    };
                                    detail_line(
                                        ui,
                                        format!(
                                            "Application headroom ({estimate_kind}; {service_source_label}): {} · summed p99 bounds ≤ {} · capacity ~{}",
                                            compact_factor(estimate.headroom_factor()),
                                            compact_duration(estimate.summed_p99_upper_bounds),
                                            compact_rate(estimate.capacity_messages_per_second, "msg/s"),
                                        ),
                                        estimate.utilization >= 0.8,
                                        "Advisory projection: the separate render and configured-interface write p99 histogram upper bounds are added, then compared with the aggregate message rate the running schedule is asking for. Timing from the last ~10 seconds is preferred after 20 paired observations. Otherwise run-wide timing is used after it has enough observations; recent timing is discarded once its update is ten seconds old. A channel that is not running is projected from the settings shown, using timing retained from its previous run. It is not a joint p99 or hard capacity promise. A write may return after driver/kernel buffering, and coincident due messages still serialize.",
                                    );
                                } else {
                                    let text = if service_samples == 0 {
                                        "Estimated app headroom: run channel to measure".to_owned()
                                    } else if service_samples < MIN_SERVICE_SAMPLES {
                                        format!(
                                            "Estimated app headroom: warming up ({service_samples}/{MIN_SERVICE_SAMPLES} send attempts)"
                                        )
                                    } else {
                                        "Estimated app headroom: unavailable from the observed bounds".to_owned()
                                    };
                                    detail_line(
                                        ui,
                                        text,
                                        false,
                                        "At least 20 paired render/send-call observations are required before estimating application-service headroom. Slow schedules can use the cumulative run once enough samples exist.",
                                    );
                                }
                            }
                        }

                        detail_line(
                            ui,
                            timing_text,
                            false,
                            TIMING_TOOLTIP,
                        );
                        detail_line(
                            ui,
                            format!("{timer_prefix}: {timer_detail}"),
                            timer_hot,
                            TIMER_TOOLTIP,
                        );
                        detail_line(
                            ui,
                            format!("Cadence alignment: {cadence}"),
                            false,
                            ALIGNMENT_TOOLTIP,
                        );
                        // No queue gauge here: its one copy sits beside the
                        // live-update warning above Output. That shared queue
                        // also feeds these readouts, as its tooltip explains.
                    });

            // Per-message breakdown (ADR-045). Collapsed by default: it answers
            // "which message is responsible", which only matters once the
            // channel-wide rows say something is wrong.
            let rows =
                per_message_rows(&telemetry.per_message_counts, &telemetry.per_message_timing);
            if !rows.is_empty() {
                ui.add_space(5.0);
                egui::CollapsingHeader::new("Per-message timing")
                    .id_salt("per_message_timing")
                    .default_open(false)
                    .show(ui, |ui| {
                        show_per_message_table(ui, &rows);
                    })
                    .header_response
                    .on_hover_text(PER_MESSAGE_TOOLTIP);
            }
        });

        if missed_acknowledged {
            self.views[i].notices.missed_sends.dismiss(missed);
        }

        if let Some(summary) = self.sup.last_run_summary(i) {
            ui.add_space(4.0);
            show_last_run_summary(ui, summary);
        }
    }

    /// The lifecycle button pair (listener's control row): [Start Channel /
    /// Apply & Restart / Retry Channel] [Stop Channel], both always present
    /// at the shared control size; the Start side's label/enabled state is
    /// the pure [`start_button`] decision. Start, Retry, and Apply & Restart
    /// are all the same deferred action — `start_connection` stops any
    /// current runner, applies the drafts (interface + messages), and starts.
    fn show_lifecycle_buttons(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        running: bool,
        drift: bool,
        has_error: bool,
    ) {
        // Matches listener's CONTROL_BUTTON_SIZE so the two detail panes
        // read identically; text wider than the min grows the button.
        const SIZE: egui::Vec2 = egui::vec2(96.0, 32.0);
        // A running channel with drift needs the same full draft validation
        // as a stopped channel. Keep the reasons here so the button state and
        // its disabled tooltip are derived from one result.
        let blockers = if running && !drift {
            Vec::new()
        } else {
            super::widgets::start_blockers_analyzed(
                &self.conn_drafts[i],
                &self.sched_drafts[i],
                &self.message_analysis[i],
            )
        };
        let can_start = blockers.is_empty();
        let (label, enabled) = start_button(running, has_error, drift, can_start);
        ui.horizontal(|ui| {
            let mut btn = ui.add_enabled(enabled, egui::Button::new(label).min_size(SIZE));
            if !enabled && (!running || drift) {
                // The disabled hover must chain off the same Response as the
                // add, or egui won't show it.
                let tip = blockers.join("\n");
                btn = btn.on_disabled_hover_text(if tip.is_empty() {
                    "Add a valid message first".to_string()
                } else {
                    tip
                });
            }
            if label == "Apply & Restart" && enabled {
                btn = btn.on_hover_text(
                    "Stops the current send loop, applies the edited interface \
                     and messages, and starts again. Interface-only edits can \
                     also be applied live by pressing Enter in the edited field.",
                );
            }
            if btn.clicked() {
                self.deferred.start = Some(i);
            }
            if ui
                .add_enabled(running, egui::Button::new("Stop Channel").min_size(SIZE))
                .clicked()
            {
                self.deferred.stop = Some(i);
            }
        });
    }

    fn show_channel_body(&mut self, ui: &mut egui::Ui, i: usize, running: bool) {
        // "Configure interface" — the shared section title in both apps
        // (listener's Configure section uses the same words). Stays a plain
        // collapsing section — it does NOT auto-collapse on run (you often
        // want the interface params visible while a channel is live).
        // Default open; the user's expand/collapse choice persists via the
        // stable id_salt.
        let (changed, refresh) = egui::CollapsingHeader::new("Configure interface")
            .id_salt(("conn_section", i))
            .default_open(true)
            .show(ui, |ui| {
                let interface_result = match self.conn_drafts[i].kind() {
                    // Each kind gets its own push_id namespace so the very
                    // different widget trees produced by Serial / UDP / TCP can't
                    // shift each other's auto-ids across egui's two layout passes.
                    ConnKind::Serial => {
                        ui.push_id("serial_body", |ui| {
                            show_serial_fields(ui, &mut self.conn_drafts[i], &self.serial_ports)
                        })
                        .inner
                    }
                    ConnKind::Udp => {
                        ui.push_id("udp_body", |ui| {
                            (show_udp_fields(ui, &mut self.conn_drafts[i]), false)
                        })
                        .inner
                    }
                    ConnKind::Tcp => {
                        ui.push_id("tcp_body", |ui| {
                            (show_tcp_fields(ui, &mut self.conn_drafts[i]), false)
                        })
                        .inner
                    }
                };

                // No timer control and no timer preview here (ADR-047). The
                // policy is derived from the shortest active interval and its
                // outcome is the same either way, so a line stating it would
                // read the same on every look. The diagnostics card reports
                // which policy a *running* channel actually got.
                ui.add_space(6.0);
                let mut utc_aligned =
                    self.conn_drafts[i].cadence_alignment == CadenceAlignment::UtcPhase;
                if ui
                    .checkbox(&mut utc_aligned, "Align sends to UTC interval boundaries")
                    .on_hover_text(format!("{ALIGNMENT_TOOLTIP} Requires Apply & Restart."))
                    .changed()
                {
                    self.conn_drafts[i].cadence_alignment = if utc_aligned {
                        CadenceAlignment::UtcPhase
                    } else {
                        CadenceAlignment::Immediate
                    };
                    self.dirty = true;
                }
                interface_result
            })
            .body_returned
            .unwrap_or((false, false));
        if changed {
            self.deferred.apply.push(i);
        }
        if refresh {
            self.deferred.refresh_ports = true;
        }

        ui.separator();
        // Owned (the schedule section takes `&mut self` state alongside it),
        // but clone only the counts Vec, not the whole telemetry struct.
        let per_message_counts = self
            .sup
            .telemetry_ref(i)
            .map(|t| t.per_message_counts.clone())
            .unwrap_or_default();
        let interval_changes = show_schedule_section(
            ui,
            &mut self.sched_drafts[i],
            &mut self.message_analysis[i],
            &mut self.dirty,
            &per_message_counts,
            running,
        );
        for (msg_index, interval_ms) in interval_changes {
            if self.sup.is_running(i) {
                // Undeliverable changes surface in the channel telemetry.
                let _ = self.sup.set_interval(i, msg_index, interval_ms);
            }
        }
    }
}

// ── Inline message editor ─────────────────────────────────────────────────────

fn show_schedule_section(
    ui: &mut egui::Ui,
    entries: &mut Vec<ScheduleDraft>,
    analyses: &mut Vec<MessageAnalysisCache>,
    dirty: &mut bool,
    per_message_counts: &[u64],
    channel_running: bool,
) -> Vec<(usize, u64)> {
    let mut to_remove: Option<usize> = None;
    let mut add_one = false;
    // Message indices whose interval was committed this frame, with the new value.
    let mut interval_changes: Vec<(usize, u64)> = Vec::new();

    // Sent totals and drop counts live in the detail header now; this
    // header is just the section title. `id_salt` keeps the persistent
    // open/closed state stable when the message count changes the label.
    // (The old stacked-card layout auto-collapsed this section on
    // Start; in the detail pane there's room, so the section just
    // honours whatever the user last chose.)
    analyses.resize_with(entries.len(), MessageAnalysisCache::default);
    let n = entries.len();
    let header = if n == 0 {
        "Configure messages — (none)".to_string()
    } else {
        format!(
            "Configure messages — {n} message{}",
            if n == 1 { "" } else { "s" }
        )
    };
    egui::CollapsingHeader::new(header)
        .id_salt("messages_section")
        .default_open(true)
        .show(ui, |ui| {
            for (i, entry) in entries.iter_mut().enumerate() {
                let analysis_cache = &mut analyses[i];
                ui.push_id(i, |ui| {
                    ui.group(|ui| {
                        let mut content_changed = false;
                        ui.horizontal(|ui| {
                            ui.strong(format!("Message {}", i + 1));
                            ui.separator();
                            let before_kind = entry.payload_kind;
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Nmea, "NMEA");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Ascii, "ASCII");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Utf8, "UTF-8");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Utf16, "UTF-16");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Hex, "Hex");
                            content_changed |= entry.payload_kind != before_kind;
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if entry.pending_remove {
                                    // Confirm step: ✓ commits, ✕ cancels. The
                                    // confirm button is tinted red so the
                                    // destructive choice is the visually heavy
                                    // one rather than the bare-X-shape default.
                                    if ui
                                        .small_button("Cancel")
                                        .on_hover_text("Keep this message")
                                        .clicked()
                                    {
                                        entry.pending_remove = false;
                                    }
                                    // The one red both apps use for destructive
                                    // and faulted, so "this discards something"
                                    // never arrives in a second shade.
                                    let palette = wiredata_ui::palette::active(ui);
                                    let confirm = egui::Button::new(
                                        egui::RichText::new("Remove")
                                            .color(egui::Color32::WHITE)
                                            .strong(),
                                    )
                                    .fill(palette.fault);
                                    if ui
                                        .add(confirm)
                                        .on_hover_text("Permanently remove this message")
                                        .clicked()
                                    {
                                        to_remove = Some(i);
                                    }
                                    ui.label(
                                        egui::RichText::new("Remove this message?")
                                            .color(palette.warning),
                                    );
                                } else if ui
                                    .button(egui::RichText::new("\u{00D7}").size(18.0).strong())
                                    .on_hover_text("Remove this message")
                                    .clicked()
                                {
                                    entry.pending_remove = true;
                                }
                            });
                        });

                        // Per-kind Grid id so each payload variant lives in its
                        // own egui id namespace. Without this, switching kinds
                        // makes the layout's widget set change shape inside the
                        // same Grid — and any auto-derived id whose position
                        // shifts triggers "id changed between passes" warnings
                        // on the next layout pass.
                        let grid_id = match entry.payload_kind {
                            PayloadKind::Hex => "message_grid_hex",
                            PayloadKind::Utf8 => "message_grid_utf8",
                            PayloadKind::Utf16 => "message_grid_utf16",
                            PayloadKind::Ascii => "message_grid_ascii",
                            PayloadKind::Nmea => "message_grid_nmea",
                        };
                        egui::Grid::new(grid_id)
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                content_changed |= show_payload_fields(
                                    ui,
                                    entry,
                                    analysis_cache.analysis.as_ref(),
                                );

                                let bad_interval = invalid_parse::<u64>(&entry.interval_ms);
                                ui.label("Interval (ms)");
                                let interval_resp = red_bordered(
                                    ui,
                                    bad_interval,
                                    "must be a whole number",
                                    |ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut entry.interval_ms)
                                                .id_salt("interval_ms")
                                                .desired_width(80.0),
                                        )
                                    },
                                );
                                content_changed |= interval_resp.changed();
                                ui.end_row();
                                if interval_resp.lost_focus() {
                                    if let Ok(ms) = entry.interval_ms.parse::<u64>() {
                                        interval_changes.push((i, ms));
                                    }
                                }
                            });

                        ui.horizontal(|ui| {
                            content_changed |= show_timestamp_editor(ui, entry);
                            ui.separator();
                            content_changed |= show_checksum_editor(ui, entry);
                        });

                        if content_changed {
                            entry.mark_changed();
                            *dirty = true;
                        }
                        let analysis = analysis_cache.refresh(entry);
                        show_message_preview(ui, analysis);

                        let sent = per_message_counts.get(i).copied().unwrap_or(0);
                        show_message_status(ui, channel_running, sent);
                    });
                });
                ui.add_space(4.0);
            }
            if ui.small_button("+ Add Message").clicked() {
                add_one = true;
            }
        });

    if let Some(i) = to_remove {
        entries.remove(i);
        analyses.remove(i);
        *dirty = true;
    }
    if add_one {
        entries.push(ScheduleDraft::default());
        analyses.push(MessageAnalysisCache::default());
        *dirty = true;
    }

    interval_changes
}

/// Render the payload-format fields for one message into the surrounding grid.
/// Each `PayloadKind` arm has its own renderer below.
fn show_payload_fields(
    ui: &mut egui::Ui,
    entry: &mut ScheduleDraft,
    analysis: Option<&MessageDraftAnalysis>,
) -> bool {
    match entry.payload_kind {
        PayloadKind::Hex => show_hex_payload(ui, entry),
        PayloadKind::Utf8 => show_utf8_payload(ui, entry),
        PayloadKind::Utf16 => show_utf16_payload(ui, entry),
        PayloadKind::Ascii => show_ascii_payload(ui, entry, analysis),
        PayloadKind::Nmea => show_nmea_payload(ui, entry),
    }
}

/// Fill a grid cell's known row height while placing its contents at the top.
/// egui grids otherwise center every cell vertically, which is undesirable for
/// the multiline text rows.
fn top_aligned_grid_cell<R>(
    ui: &mut egui::Ui,
    height: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.scope_builder(
        egui::UiBuilder::new().layout(egui::Layout::left_to_right(egui::Align::Min)),
        |ui| {
            ui.set_min_height(height);
            add_contents(ui)
        },
    )
    .inner
}

fn show_hex_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let bad_hex = !entry.hex_data.is_empty() && !hex_valid(&entry.hex_data);
    ui.label("Data (hex)");
    let response = red_bordered(
        ui,
        bad_hex,
        "invalid hex — use byte pairs like DE AD BE EF",
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut UppercaseHex(&mut entry.hex_data))
                    .id_salt("payload_hex")
                    .desired_width(360.0)
                    .hint_text("DE AD BE EF"),
            )
        },
    );
    ui.end_row();
    response.changed()
}

fn show_utf8_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let row_height = message_editor_max_height(ui, &entry.utf8_text);
    top_aligned_grid_cell(ui, row_height, |ui| ui.label("Text"));
    let changed = top_aligned_grid_cell(ui, row_height, |ui| {
        let edited = marker_aware_text_edit(
            ui,
            &mut entry.utf8_text,
            "payload_utf8",
            None,
            300.0,
            "Unicode text",
        )
        .changed();
        let inserted = show_insert_byte_button(
            ui,
            &mut entry.utf8_text,
            &mut entry.insert_byte_hex,
            "payload_utf8",
        );
        edited || inserted
    });
    ui.end_row();
    changed
}

fn show_utf16_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let row_height = message_editor_max_height(ui, &entry.utf16_text);
    top_aligned_grid_cell(ui, row_height, |ui| ui.label("Text"));
    let text_changed = top_aligned_grid_cell(ui, row_height, |ui| {
        // Two editor modes, chosen by `Allow raw bytes`:
        //   off — plain Unicode editor (what you see is what gets
        //         encoded). Insert Code Unit inserts the decoded
        //         glyph (4 hex → one char).
        //   on  — marker-aware editor + Insert Byte button. Insert
        //         Code Unit inserts marker pairs with byte order
        //         applied.
        let mut changed = if entry.utf16_allow_raw_bytes {
            let edited = marker_aware_text_edit(
                ui,
                &mut entry.utf16_text,
                "payload_utf16",
                None,
                300.0,
                "Unicode text",
            )
            .changed();
            let inserted = show_insert_byte_button(
                ui,
                &mut entry.utf16_text,
                &mut entry.insert_byte_hex,
                "payload_utf16",
            );
            edited || inserted
        } else {
            plain_text_edit_with_cursor(
                ui,
                &mut entry.utf16_text,
                "payload_utf16",
                300.0,
                "Unicode text",
            )
            .changed()
        };
        changed |= show_insert_unit_button(
            ui,
            &mut entry.utf16_text,
            &mut entry.insert_byte_hex,
            "payload_utf16",
            entry.utf16_big_endian,
            entry.utf16_allow_raw_bytes,
        );
        changed
    });
    ui.end_row();
    let before_options = (
        entry.utf16_big_endian,
        entry.utf16_bom,
        entry.utf16_allow_raw_bytes,
    );
    ui.label("Byte order");
    ui.horizontal(|ui| {
        ui.radio_value(&mut entry.utf16_big_endian, true, "Big-endian");
        ui.radio_value(&mut entry.utf16_big_endian, false, "Little-endian");
        ui.separator();
        ui.checkbox(&mut entry.utf16_bom, "BOM");
        ui.separator();
        ui.checkbox(&mut entry.utf16_allow_raw_bytes, "Allow raw bytes")
            .on_hover_text(
                "Treat ‹XX› in the text as raw bytes (fuzzing escape \
                 hatch). When off, ‹ and › are literal Unicode chars.",
            );
    });
    ui.end_row();
    text_changed
        || before_options
            != (
                entry.utf16_big_endian,
                entry.utf16_bom,
                entry.utf16_allow_raw_bytes,
            )
}

fn show_ascii_payload(
    ui: &mut egui::Ui,
    entry: &mut ScheduleDraft,
    analysis: Option<&MessageDraftAnalysis>,
) -> bool {
    let row_height = message_editor_max_height(ui, &entry.ascii_text);
    top_aligned_grid_cell(ui, row_height, |ui| ui.label("Text"));
    let text_changed = top_aligned_grid_cell(ui, row_height, |ui| {
        let edited = marker_aware_text_edit(
            ui,
            &mut entry.ascii_text,
            "payload_ascii",
            Some(entry.ascii_code_page),
            300.0,
            "text",
        )
        .changed();
        let inserted = show_insert_byte_button(
            ui,
            &mut entry.ascii_text,
            &mut entry.insert_byte_hex,
            "payload_ascii",
        );
        edited || inserted
    });
    ui.end_row();
    let code_page_before = entry.ascii_code_page;
    ui.label("Code page");
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("code_page")
            .selected_text(code_page_label(entry.ascii_code_page))
            .show_ui(ui, |ui| {
                for cp in [
                    crate::core::message::CodePage::Iso8859_1,
                    crate::core::message::CodePage::Windows1252,
                    crate::core::message::CodePage::Cp437,
                    crate::core::message::CodePage::MacRoman,
                ] {
                    ui.selectable_value(&mut entry.ascii_code_page, cp, code_page_label(cp));
                }
            });
        if let Some(summary) = analysis.and_then(|analysis| analysis.replacements.as_ref()) {
            let characters = summary
                .characters
                .iter()
                .map(|c| format!("'{c}' (U+{:04X})", *c as u32))
                .collect::<Vec<_>>()
                .join(", ");
            ui.colored_label(
                theme_palette(ui).warning,
                format!("{} replaced with ?; use UTF-8", summary.count),
            )
            .on_hover_text(format!(
                "{} cannot represent: {characters}. Each occurrence will be sent as '?' \
                 (0x3F). Switch the message Format to UTF-8 (recommended) or UTF-16, \
                 or insert exact bytes when substitution is not appropriate.",
                code_page_label(entry.ascii_code_page)
            ));
        }
    });
    ui.end_row();
    text_changed || entry.ascii_code_page != code_page_before
}

fn show_nmea_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let mut changed = false;
    ui.label("Talker / Sentence");
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut entry.nmea_talker)
                .id_salt("payload_nmea_talker")
                .desired_width(40.0)
                .hint_text("GP"),
        );
        if r.changed() {
            entry.nmea_talker = entry.nmea_talker.to_ascii_uppercase();
            changed = true;
        }
        ui.menu_button("v", |ui| {
            changed |= show_filtered_picker(
                ui,
                "filter by code or description",
                &mut entry.nmea_talker_filter,
                nmea0183::talker_id::ALL_WITH_DESC,
                &mut entry.nmea_talker,
            );
        });
        ui.separator();
        let r = ui.add(
            egui::TextEdit::singleline(&mut entry.nmea_sentence_type)
                .id_salt("payload_nmea_sentence")
                .desired_width(50.0)
                .hint_text("GGA"),
        );
        if r.changed() {
            entry.nmea_sentence_type = entry.nmea_sentence_type.to_ascii_uppercase();
            prefill_nmea_fields(entry);
            changed = true;
        }
        ui.menu_button("v", |ui| {
            if show_filtered_picker(
                ui,
                "filter by code or description",
                &mut entry.nmea_sentence_filter,
                nmea0183::sentence_type::ALL_WITH_DESC,
                &mut entry.nmea_sentence_type,
            ) {
                prefill_nmea_fields(entry);
                changed = true;
            }
        });
        ui.separator();
        ui.label("NMEA checksum:").on_hover_text(
            "The protocol-internal `*XX` byte at the end of an NMEA \
             sentence. Distinct from the `Message checksum` row below, \
             which is an outer checksum wrapped around the complete \
             rendered message (timestamp + payload + NMEA `*XX`).",
        );
        let checksum_before = entry.nmea_checksum_mode;
        ui.radio_value(
            &mut entry.nmea_checksum_mode,
            NmeaChecksumMode::Correct,
            "include",
        );
        ui.radio_value(
            &mut entry.nmea_checksum_mode,
            NmeaChecksumMode::Omit,
            "omit",
        );
        ui.radio_value(
            &mut entry.nmea_checksum_mode,
            NmeaChecksumMode::Wrong,
            "wrong",
        );
        changed |= entry.nmea_checksum_mode != checksum_before;
    });
    ui.end_row();

    let sentence_type: nmea0183::SentenceType = entry
        .nmea_sentence_type
        .parse()
        .expect("SentenceType parse is infallible");
    let time_fields = sentence_type.time_fields();
    let supports_live_time = !time_fields.is_empty();
    ui.label("Time fields");
    ui.horizontal(|ui| {
        let live_before = entry.nmea_live_time;
        let live_response = ui.add_enabled(
            supports_live_time || entry.nmea_live_time,
            egui::Checkbox::new(&mut entry.nmea_live_time, "Live time (UTC)"),
        );
        if supports_live_time {
            let fields = time_fields
                .iter()
                .map(|(index, kind)| format!("field {}: {}", index + 1, kind.label()))
                .collect::<Vec<_>>()
                .join(", ");
            live_response.on_hover_text(format!(
                "Refreshed at every send: {fields}. Typed values are retained in the profile but \
                 overridden on the wire. Missing fields are added through the last live field."
            ));
        } else {
            live_response
                .on_hover_text("No live UTC field positions are defined for this sentence type.");
        }
        changed |= entry.nmea_live_time != live_before;

        let millis_before = entry.nmea_live_millis;
        ui.add_enabled(
            supports_live_time && entry.nmea_live_time,
            egui::Checkbox::new(&mut entry.nmea_live_millis, "Milliseconds"),
        )
        .on_hover_text("Use hhmmss.sss instead of hhmmss for live UTC time fields.");
        changed |= entry.nmea_live_millis != millis_before;

        if entry.nmea_live_time && !supports_live_time {
            ui.colored_label(theme_palette(ui).fault, "Unsupported sentence type")
                .on_hover_text("Turn Live time off or choose a sentence with defined UTC fields.");
        }
    });
    ui.end_row();

    ui.label("Fields");
    let fields_r = ui.add(
        egui::TextEdit::singleline(&mut entry.nmea_fields)
            .id_salt("payload_nmea_fields")
            .desired_width(360.0)
            .hint_text("comma-separated, e.g. 123519,4807.038,N,01131.000,E"),
    );
    if fields_r.changed() {
        // User edited by hand — protect Fields from being overwritten
        // by future auto-fills on sentence-type changes.
        entry.nmea_fields_autofilled = false;
        changed = true;
    }
    ui.end_row();
    changed
}

/// Example comma-separated field values for common NMEA sentence types.
/// Returned with no trailing `*XX` (the checksum is added downstream).
/// Used to auto-fill the Fields box when the user picks a sentence type
/// and the Fields box is currently empty — so brand-new messages start
/// from a realistic sample rather than a blank.
fn nmea_example_fields(sentence: &str) -> Option<&'static str> {
    match sentence {
        "GGA" => Some("123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,"),
        "RMC" => Some("220516,A,5133.82,N,00042.24,W,173.8,231.8,130694,004.2,W"),
        "VTG" => Some("054.7,T,034.4,M,005.5,N,010.2,K"),
        "GLL" => Some("4916.45,N,12311.12,W,225444,A"),
        "GSA" => Some("A,3,19,28,14,18,27,22,31,39,,,,,1.7,1.0,1.3"),
        "GSV" => Some("2,1,08,01,40,083,46,02,17,308,41,12,07,344,39,14,22,228,45"),
        "GNS" => Some("122310.2,3722.425671,N,12258.856215,W,DA,15,0.9,1005.543,6.5,5.2,23"),
        "HDT" => Some("123.4,T"),
        "HDM" => Some("123.4,M"),
        "HDG" => Some("123.4,1.2,E,2.0,W"),
        "THS" => Some("123.4,A"),
        "ROT" => Some("35.6,A"),
        "ZDA" => Some("201530.00,04,07,2002,00,00"),
        "VHW" => Some("123.4,T,123.4,M,1.0,N,1.852,K"),
        "VBW" => Some("11.0,01.0,A,12.0,02.0,A"),
        "VLW" => Some("12345.6,N,123.4,N"),
        "DBT" => Some("5.0,f,1.5,M,0.8,F"),
        "DBK" => Some("5.0,f,1.5,M,0.8,F"),
        "DBS" => Some("5.0,f,1.5,M,0.8,F"),
        "DPT" => Some("3.4,0.5"),
        "MTW" => Some("17.9,C"),
        "MWV" => Some("019.0,R,15.5,N,A"),
        "MWD" => Some("019.0,T,021.0,M,015.5,N,007.97,M"),
        "MDA" => Some("30.12,I,1.02,B,17.9,C,,,53,,,,019.0,T,021.0,M,15.5,N,007.97,M"),
        "XDR" => Some("C,17.9,C,TEMP1"),
        "RSA" => Some("0.5,A,,V"),
        "RPM" => Some("S,1,1000.0,5.0,A"),
        "APB" => Some("A,A,0.10,R,N,V,V,011.0,T,DEST,011.0,T,011.0,T"),
        "BOD" => Some("097.0,T,103.2,M,POINTB,POINTA"),
        "XTE" => Some("A,A,0.10,R,N"),
        "GBS" => Some("125027,1.2,1.3,3.2,12,0.04,-0.3,7.5"),
        "GST" => Some("172814.0,0.006,0.023,0.020,273.6,0.023,0.020,0.031"),
        // Proprietary — pair with talker P. PASHR (Ashtech attitude):
        // hhmmss.ss,heading,T,roll,pitch,heave,roll_acc,pitch_acc,heading_acc,quality
        "ASHR" => Some("123519.00,123.45,T,1.23,-0.50,0.10,0.020,0.020,0.025,1"),
        // PRDID (Teledyne RDI): pitch,roll,heading — has no checksum.
        "RDID" => Some("-1.23,2.34,123.45"),
        _ => None,
    }
}

/// Pre-fill `entry.nmea_fields` with a sample for the current sentence
/// type when it's safe to do so:
///
/// - The Fields box is empty, OR
/// - The Fields box was previously auto-filled and the user hasn't edited
///   it since (`nmea_fields_autofilled == true`).
///
/// Anything the user has typed by hand is left alone.
fn prefill_nmea_fields(entry: &mut ScheduleDraft) {
    let safe_to_overwrite = entry.nmea_fields.is_empty() || entry.nmea_fields_autofilled;
    if !safe_to_overwrite {
        return;
    }
    if let Some(example) = nmea_example_fields(&entry.nmea_sentence_type) {
        entry.nmea_fields = example.to_string();
        entry.nmea_fields_autofilled = true;
    } else if entry.nmea_fields_autofilled {
        // No example for this new sentence type. Clear any stale auto-fill
        // from the previous sentence type — keeping it would confuse the
        // user. (Leave user-typed content alone, which is why we only do
        // this when the autofilled flag is set.)
        entry.nmea_fields.clear();
        entry.nmea_fields_autofilled = false;
    }
}

/// Filterable, scrollable popup body used for the NMEA Talker and Sentence
/// pickers. Renders a small TextEdit at the top, then a scrollable list of
/// `(code, description)` rows. The filter is case-insensitive and matches
/// against BOTH the code and the description, so typing "depth" narrows the
/// sentence list to DBK/DBS/DBT/DPT etc. Clicking a row commits the code
/// into `selected` and closes the popup.
fn show_filtered_picker(
    ui: &mut egui::Ui,
    hint: &str,
    filter: &mut String,
    options: &[(&'static str, &'static str)],
    selected: &mut String,
) -> bool {
    // Pin the popup so the Talker and Sentence pickers look the same and
    // so the (often long) descriptions don't keep widening it.
    ui.set_min_width(360.0);
    let r = ui.add(
        egui::TextEdit::singleline(filter)
            .desired_width(340.0)
            .hint_text(hint),
    );
    r.request_focus();
    let needle = filter.to_ascii_lowercase();
    let mut changed = false;
    egui::ScrollArea::vertical()
        .min_scrolled_height(300.0)
        .max_height(300.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Empty-selection row, always at the top — lets the user
            // clear a previously-picked value without retyping or
            // closing the popup. Skipped when the filter is active so
            // it doesn't visually compete with real matches.
            if needle.is_empty()
                && ui
                    .button(egui::RichText::new("(empty — clear selection)").italics())
                    .clicked()
            {
                selected.clear();
                filter.clear();
                changed = true;
                ui.close();
            }
            for (code, desc) in options {
                let matches = needle.is_empty()
                    || code.to_ascii_lowercase().contains(&needle)
                    || desc.to_ascii_lowercase().contains(&needle);
                if matches && ui.button(format!("{code}  —  {desc}")).clicked() {
                    *selected = (*code).to_string();
                    filter.clear();
                    changed = true;
                    ui.close();
                }
            }
        });
    changed
}

/// Render the read-only "this is what would be sent" preview row.
///
/// The revision-keyed [`MessageDraftAnalysis`] owns conversion, compilation,
/// and fixed-time rendering. This widget only applies theme-dependent styling,
/// so unchanged long messages do no wire-format work during repaints.
///
/// Bytes are shown as text (lossy UTF-8) for payload types that are text
/// at heart (Utf8 / Ascii / NMEA) and as space-separated hex for the
/// binary types (Hex / Utf16), to avoid the U+FFFD-tofu we'd otherwise
/// get for non-UTF-8 bytes.
fn show_message_preview(ui: &mut egui::Ui, analysis: &MessageDraftAnalysis) {
    ui.horizontal(|ui| {
        ui.label("Wire bytes:").on_hover_text(
            "Literal bytes that would be sent on the wire, rendered \
                 in a payload-appropriate view. Timestamps use a fixed \
                 reference instant so the value doesn't tick — the \
                 actual send uses the wall clock.",
        );
        let preview: egui::WidgetText = match &analysis.preview {
            MessagePreview::Ascii {
                bytes,
                code_page,
                replacement_wire_offsets,
            } => preview_ascii_layout_job(ui, bytes, *code_page, replacement_wire_offsets).into(),
            MessagePreview::Text(text) | MessagePreview::Hex(text) => {
                egui::RichText::new(text).monospace().into()
            }
            MessagePreview::Invalid(error) => egui::RichText::new(format!("Invalid: {error}"))
                .color(theme_palette(ui).fault)
                .monospace()
                .into(),
            MessagePreview::Incomplete => egui::RichText::new("(message is incomplete)")
                .monospace()
                .into(),
        };
        ui.label(preview);
    });
}

/// Per-message status line at the bottom of each message group:
/// a coloured state dot plus the message's running local-acceptance count.
///
/// State follows the channel — messages aren't independently scheduled
/// from the user's perspective. "Active" = channel is running and this
/// message will fire on its interval. "Idle" = channel is stopped, so
/// the count is the last value seen.
/// How strongly the message status strip is tinted by its state accent. Enough
/// to read as a strip rather than another row of widgets, light enough that the
/// theme's body text stays the most legible thing on it.
pub(super) const STATUS_STRIP_TINT_ALPHA: u8 = 46;

fn show_message_status(ui: &mut egui::Ui, channel_running: bool, sent: u64) {
    // Footer bar: separator above to split it from the message body, then
    // a tinted Frame so the "Active / Sent: N" line reads as a status
    // strip rather than just another row of widgets. Inner margin
    // matches the channel-summary chrome so all the framed bits in the
    // GUI feel like the same component.
    ui.add_space(2.0);
    ui.separator();
    let palette = wiredata_ui::palette::active(ui);
    let (dot_color, state) = if channel_running {
        (palette.running, "Active")
    } else {
        (palette.idle, "Idle")
    };
    // Tinted strip behind the status line. Blending the same accent into the
    // panel gives a deep green on dark and a pale one on light without naming
    // four background colors that would drift the moment the accent changed —
    // and the label text keeps the theme's body colour, so it stays legible.
    let bg = wiredata_ui::palette::tint(ui, dot_color, STATUS_STRIP_TINT_ALPHA);
    egui::Frame::default()
        .fill(bg)
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(dot_color, egui::RichText::new("\u{2022}").size(16.0));
                ui.label(egui::RichText::new(state).strong());
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("Sent: {sent}"))
                        .strong()
                        .monospace(),
                )
                .on_hover_text(SENT_MEANING_TOOLTIP);
            });
        });
}

/// Render the per-message timestamp toggles.
///
/// No inner separator between the `Timestamp` checkbox and its
/// sub-toggles — visual grouping comes from the parent horizontal. The
/// only `ui.separator()` at this nesting level is the one *between* the
/// timestamp group and the message-checksum group, so the hierarchy reads
/// "groups are separated; within a group is just spacing".
fn show_timestamp_editor(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let before = (
        entry.timestamp_enabled,
        entry.ts_date,
        entry.ts_millis,
        entry.ts_timezone,
    );
    ui.horizontal(|ui| {
        ui.checkbox(&mut entry.timestamp_enabled, "Timestamp");
        if entry.timestamp_enabled {
            ui.checkbox(&mut entry.ts_date, "Date");
            ui.checkbox(&mut entry.ts_millis, "Milliseconds");
            ui.checkbox(&mut entry.ts_timezone, "Z (UTC)");
        }
    });
    before
        != (
            entry.timestamp_enabled,
            entry.ts_date,
            entry.ts_millis,
            entry.ts_timezone,
        )
}

/// Render the per-message checksum controls. See [`show_timestamp_editor`]
/// for the separator hierarchy rationale.
fn show_checksum_editor(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    use crate::core::message::ChecksumAlgorithm;
    let before = (
        entry.checksum_enabled,
        entry.checksum_algorithm,
        entry.checksum_wrong,
    );
    ui.horizontal(|ui| {
        ui.checkbox(&mut entry.checksum_enabled, "Message checksum")
            .on_hover_text(
                "Outer checksum appended to the complete rendered message \
                 (timestamp + payload). Independent of any protocol-internal \
                 checksum like NMEA's `*XX` — that one is still emitted.",
            );
        if entry.checksum_enabled {
            egui::ComboBox::from_id_salt("checksum_algorithm")
                .selected_text(checksum_label(entry.checksum_algorithm))
                .show_ui(ui, |ui| {
                    for algo in [
                        ChecksumAlgorithm::Xor,
                        ChecksumAlgorithm::Crc8,
                        ChecksumAlgorithm::Crc16Kermit,
                        ChecksumAlgorithm::Crc16Modbus,
                        ChecksumAlgorithm::Crc32,
                    ] {
                        ui.selectable_value(
                            &mut entry.checksum_algorithm,
                            algo,
                            checksum_label(algo),
                        );
                    }
                });
            ui.checkbox(&mut entry.checksum_wrong, "Intentionally wrong");
        }
    });
    before
        != (
            entry.checksum_enabled,
            entry.checksum_algorithm,
            entry.checksum_wrong,
        )
}

#[cfg(test)]
mod tests {
    use super::top_aligned_grid_cell;

    #[test]
    fn multiline_grid_cells_share_the_same_top_edge() {
        let mut measured_tops = None;
        egui::__run_test_ui(|ui| {
            egui::Grid::new("top_aligned_grid_test")
                .num_columns(2)
                .show(ui, |ui| {
                    let label = top_aligned_grid_cell(ui, 80.0, |ui| ui.label("Text"));
                    let editor = top_aligned_grid_cell(ui, 80.0, |ui| {
                        ui.allocate_response(egui::vec2(300.0, 60.0), egui::Sense::hover())
                    });
                    ui.end_row();
                    measured_tops = Some((label.rect.top(), editor.rect.top()));
                });
        });

        let (label_top, editor_top) = measured_tops.expect("grid contents should be measured");
        assert!(
            (label_top - editor_top).abs() <= 0.5,
            "label top {label_top} did not align with editor top {editor_top}"
        );
    }
}
