//! The channel detail / message-view panel (`show_detail`), split out of `mod.rs`.
//! The decision logic it dispatches (Apply & Start/Restart sequencing, lifecycle
//! actions) lives — and is unit-tested — in [`super::widgets`]. The densest sub-panel,
//! the stream viewer, lives in [`stream_view`].

mod diagnostics;
mod stream_view;

use crate::core::{ChannelId, RecordingState};
use crate::diagnostics::DiagnosticSeverity;
use crate::runtime::ListenerRunSummary;

use super::bridge::{self, UiCommand};
use super::state::ChannelStatus;
use super::widgets::{
    config_needs_restart, edit_display_recording, edit_interface, edit_raw_recording, human_bytes,
    line_indicator, line_toggle, paint_glyph, recording_facts, recording_glyph_size,
    recording_indicator, start_button, status_color, status_glyph, status_label, stop_enabled,
};
use super::ListenerApp;
use wiredata_ui::fonts::bold;
use wiredata_ui::format::{compact_duration, human_byte_rate};
use wiredata_ui::palette::active as palette;

/// Uniform size for the lifecycle / recording control buttons. Text wider than the
/// min grows the button (so "Apply & Restart" doesn't clip).
const CONTROL_BUTTON_SIZE: egui::Vec2 = egui::vec2(96.0, 32.0);

const THROUGHPUT_TOOLTIP: &str = concat!(
    "Received is the cumulative byte count processed by Listener since Start and is ",
    "retained after Stop. Throughput uses bytes whose post-read arrival times fall in ",
    "the current and previous four one-second buckets, divided by five seconds and ",
    "scaled by SI unit. It is an approximate five-second application receive rate ",
    "from the latest status snapshot, not instantaneous line rate, link utilization, ",
    "or device-buffer occupancy. During the first five seconds it still uses the full ",
    "five-second denominator; after Stop the rate is zero while Received remains."
);
const FINAL_SNAPSHOT_TOOLTIP: &str = concat!(
    "When complete, completed-run values come from the channel's final snapshot. If ",
    "the final snapshot is incomplete, timing, queue, and transport values may be ",
    "missing or defaulted and must not be read as measured zero."
);

/// Which recording tap a shared control targets (ADR-013): the byte-exact Raw
/// `.raw` or the rendered Display `.disp`. The two blocks are deliberately
/// symmetric — one enum keeps the header button and the live-persist fold a
/// single implementation each.
#[derive(Clone, Copy)]
enum RecTap {
    Raw,
    Display,
}

fn show_last_run_summary(ui: &mut egui::Ui, summary: &ListenerRunSummary) {
    let heading = format!(
        "Last completed run · {} · {} · {} chunks",
        compact_duration(summary.elapsed),
        human_bytes(summary.total_bytes),
        summary.chunk_shape.chunk_count(),
    );
    let summary_view = egui::CollapsingHeader::new(heading)
        .id_salt("listener_last_completed_run")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Copy summary").clicked() {
                    ui.ctx().copy_text(summary.to_report_text());
                }
                ui.weak(format!(
                    "Listener {} · {} · {}/{}",
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
                summary.finished_utc()
            ));
            ui.weak(format!(
                "Ingest queue peak {}/{} · {} warnings · {} errors{}",
                summary.ingest_queue.peak,
                summary.ingest_queue.capacity,
                summary.diagnostics_warnings,
                summary.diagnostics_errors,
                if summary.final_snapshot_complete {
                    ""
                } else {
                    " · final snapshot incomplete"
                }
            ));
        });
    summary_view
        .header_response
        .on_hover_text(FINAL_SNAPSHOT_TOOLTIP);
}

impl ListenerApp {
    pub(super) fn show_detail(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected else {
            ui.label("No channel selected. Use “+ Add” by the channel list.");
            return;
        };
        self.sync_edit_draft(id);
        let Some((details, status, bytes_total, bps, last_error, recording, serial_port)) =
            self.state.channel(id).map(|v| {
                (
                    v.details.clone(),
                    v.status,
                    v.bytes_total,
                    v.bytes_per_sec,
                    v.last_error.clone(),
                    v.recording,
                    match &v.config.interface {
                        crate::config::InterfaceConfig::Serial(serial) => Some(serial.port.clone()),
                        _ => None,
                    },
                )
            })
        else {
            ui.label("That channel is no longer present.");
            return;
        };

        // Does the edit draft differ from the committed config in a way that needs a
        // restart? Drives the Start button's "Apply & Restart" label. Live-applied
        // fields (name, raw recording, view settings, scroll buffer) are excluded — see
        // `config_needs_restart` — so editing them doesn't flip the lifecycle button.
        let config_changed = match (&self.edit_draft, self.state.channel(id)) {
            (Some((eid, draft)), Some(view)) if *eid == id => {
                config_needs_restart(draft, &view.config)
            }
            _ => false,
        };
        // Channel block (name, status, stats, lifecycle) on the LEFT, recording block on
        // the RIGHT, as two columns — but the whole columns area is capped to a FIXED
        // width (`set_max_width`), so each column has a constant width and the recording
        // block's left edge stays put when the window's right edge is resized (with a
        // free 50/50 `columns` the right column widened with the pane, dragging the block
        // sideways). The cap is scoped to just the columns; the View config + stream view
        // render after on the full-width `ui` — `columns` is the only side-by-side layout
        // that has reliably kept the stream view (horizontal_top variants collapsed it).
        const CONTROLS_WIDTH: f32 = 670.0;
        ui.scope(|ui| {
            ui.set_max_width(CONTROLS_WIDTH);
            ui.columns(2, |cols| {
                self.show_channel_controls(
                    &mut cols[0],
                    id,
                    status,
                    config_changed,
                    &details,
                    bytes_total,
                    bps,
                );
                self.show_recording_block(&mut cols[1], id, status, recording);
            });
        });
        if let Some(err) = &last_error {
            ui.colored_label(palette(ui).fault, format!("⚠ {err}"));
        }
        // Only the UI knows what is currently enumerated, so only the UI can say
        // whether the port the OS called absent is still in the list.
        //
        // Keyed on Faulted as well as `last_error`: a failed *start* takes the
        // channel to Faulted and leaves its reason to the runtime's diagnostics
        // headline rather than setting `last_error`, so gating on the error
        // alone hid this hint in exactly the case it was written for.
        if status == ChannelStatus::Faulted || last_error.is_some() {
            if let Some(port) = serial_port.filter(|p| !p.is_empty()) {
                let listed = self.serial_ports.contains(&port);
                ui.colored_label(
                    palette(ui).warning,
                    wiredata_ui::format::serial_port_hint(&port, listed),
                );
            }
        }
        if let Some(view) = self.state.channel(id) {
            ui.add_space(6.0);
            diagnostics::show_receive_diagnostics_card(ui, id, status, view);
        }
        if let Some(summary) = self
            .state
            .channel(id)
            .and_then(|view| view.snapshot.as_ref())
            .and_then(|snapshot| snapshot.last_run_summary.as_ref())
        {
            ui.add_space(4.0);
            show_last_run_summary(ui, summary);
        }

        // Configure: edit the full interface config on a working copy, then commit
        // with one click — "Apply & Restart" installs it and brings the channel up
        // (no separate Apply-then-Start step).
        let ports = self.serial_ports.clone();
        // Force the section open for one frame when focus moved to a needy channel
        // (task 1); `None` afterwards so the user can still collapse it.
        let force_open = self.force_config_open.then_some(true);
        let mut refresh = false;
        // Configure is edit-only: there's no Apply button here. Edits commit via the
        // Start / Apply & Restart button at the top, which applies the pending draft.
        if let Some((_, config)) = &mut self.edit_draft {
            // "Configure interface" — the shared section title in both apps
            // (talker's interface editor uses the same words).
            egui::CollapsingHeader::new("Configure interface")
                // A STABLE id (not per-channel) so switching channels doesn't create a
                // "new" header each time — that re-triggered a focus/animation highlight
                // that flashed a rectangle around the label on every channel switch. The
                // open/closed state is now shared across channels (consistent), with a
                // one-frame force-open when a needy channel needs attention.
                .id_salt("configure")
                .open(force_open)
                .default_open(true)
                .show(ui, |ui| {
                    refresh = ui
                        .push_id("edit_iface", |ui| edit_interface(ui, id, config, &ports))
                        .inner;
                    // Display (.disp) recording lives in the recording block on the
                    // right, under Record Raw Data (ADR-013); the inline Mark
                    // timestamp editor lives under Configure display (it shapes what
                    // the view and .disp show).
                });
        }
        self.force_config_open = false;
        if refresh {
            self.refresh_serial_ports();
        }

        // Live serial control/status lines (§161): green = high, grey = low.
        if let Some(lines) = self.state.channel(id).and_then(|v| v.control_lines) {
            ui.horizontal(|ui| {
                // Outputs are clickable toggles (§161): clicking sends Set{Rts,Dtr};
                // the shown state still comes from the live poll, so it reflects what
                // the port actually did, not just what we asked for.
                ui.label("Out:");
                if line_toggle(ui, "RTS", lines.rts).clicked() {
                    self.send(UiCommand::SetRts(id, !lines.rts));
                }
                if line_toggle(ui, "DTR", lines.dtr).clicked() {
                    self.send(UiCommand::SetDtr(id, !lines.dtr));
                }
                ui.separator();
                // Inputs are read-only indicators.
                ui.label("In:");
                line_indicator(ui, "CTS", lines.cts);
                line_indicator(ui, "DSR", lines.dsr);
                line_indicator(ui, "DCD", lines.dcd);
                line_indicator(ui, "RI", lines.ri);
            });
        }

        ui.separator();
        self.show_diagnostics(ui, id);
        ui.separator();
        self.show_stream_view(ui, id);
    }

    /// Diagnostics for the selected channel (snapshot-driven): a color-coded
    /// headline that opens a filterable, ms-timestamped log. Split out of
    /// `show_detail`. (Match-rule activity surfaces through its effects — inline
    /// Mark timestamps, recording state, the diagnostics entries Notify and
    /// boundary-split recoveries write — not a firing list of its own.)
    fn show_diagnostics(&mut self, ui: &mut egui::Ui, id: ChannelId) {
        // Diagnostics — only meaningful once there's a snapshot. Pull the
        // data into owned locals so the filter checkboxes can mutate `self` without a
        // live `self.state` borrow.
        struct DiagView {
            headline_level: &'static str,
            headline: String,
            headline_color: egui::Color32,
            counts: (usize, usize, usize),
            /// The full diagnostics log, chronological (oldest → newest) — the
            /// per-snapshot cache from the view-model (an O(1) `Rc` clone per
            /// frame; the flatten+sort happens once per poll, not per repaint).
            entries: std::rc::Rc<Vec<crate::diagnostics::Diagnostic>>,
        }
        let view = self.state.channel(id);
        // The diagnostics log comes *only* from the snapshot — the GUI never synthesizes
        // entries. The runtime is the single writer: live diagnostics arrive via the 5 Hz
        // poll; stop-time notes via the final snapshot taken at stop; and a start/bind
        // fault (which never ran a pipeline) is retained by the runtime and served via a
        // minimal snapshot for the faulted channel. The snapshot is kept across stop/start
        // so a previous run's messages persist.
        let (entries, counts) = match view {
            Some(v) => {
                let counts = v
                    .snapshot
                    .as_ref()
                    .map(|s| {
                        let d = &s.diagnostics;
                        (d.events.len(), d.warnings.len(), d.errors.len())
                    })
                    .unwrap_or((0, 0, 0));
                (v.sorted_diagnostics.clone(), counts)
            }
            None => (std::rc::Rc::new(Vec::new()), (0, 0, 0)),
        };
        // Always shown — an empty log renders a neutral "no diagnostics yet"
        // headline so the section doesn't pop into existence on the first entry.
        // The headline is the latest entry, so the newest diagnostic leads — a fresh
        // INFO supersedes an older ERROR, and a just-recorded fault (newest) leads.
        let (headline_level, headline, headline_color) = match entries.last() {
            Some(d) => {
                let (level, color) = match d.severity {
                    // The headline already prints its level, so INFO is the
                    // baseline the other two are read against — body text, not
                    // an accent of its own.
                    DiagnosticSeverity::Event => ("INFO", ui.visuals().text_color()),
                    DiagnosticSeverity::Warning => ("WARN", palette(ui).warning),
                    DiagnosticSeverity::Error => ("ERROR", palette(ui).fault),
                };
                (level, d.message.clone(), color)
            }
            None => (
                "",
                "no diagnostics yet".to_string(),
                ui.visuals().weak_text_color(),
            ),
        };
        let dv = DiagView {
            headline_level,
            headline,
            headline_color,
            counts,
            entries,
        };
        {
            // The diagnostics header is a single line: "Diagnostics (counts)  LEVEL phrase"
            // — the live headline (chronologically latest diagnostic) sits to the right of
            // the title and score, shortened to a clean phrase (headline_phrase) and
            // truncated by egui if it still overflows the row. Single-line and a fixed
            // height, so the collapsing header's layout stays stable across egui's two
            // passes (a wrapping/variable-height header caused a repaint spin). The full
            // untruncated text is in the expanded log below.
            let headline = if dv.headline_level.is_empty() {
                headline_phrase(&dv.headline)
            } else {
                format!("{}  {}", dv.headline_level, headline_phrase(&dv.headline))
            };
            let headline_color = dv.headline_color;
            let diag_id = ui.make_persistent_id(("diagnostics", id));
            egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                diag_id,
                false,
            )
            .show_header(ui, |ui| {
                let (e, w, x) = dv.counts;
                ui.label("Diagnostics");
                ui.label(egui::RichText::new(format!("({e} info · {w} warn · {x} err)")).weak());
                ui.add(
                    egui::Label::new(egui::RichText::new(headline).color(headline_color))
                        .truncate(),
                );
            })
            .body(|ui| {
                ui.horizontal(|ui| {
                    let (e, w, x) = dv.counts;
                    ui.label("Show");
                    ui.checkbox(&mut self.show_info, format!("Info ({e})"));
                    ui.checkbox(&mut self.show_warn, format!("Warn ({w})"));
                    ui.checkbox(&mut self.show_error, format!("Error ({x})"));
                });
                egui::ScrollArea::vertical()
                    .id_salt("diag_log")
                    .max_height(200.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Inside the scroll area (fixed height, auto_shrink off), the
                        // available width is the stable wrap target — don't derive it from
                        // `clip_rect`/`cursor`, which vary between egui's two layout passes
                        // and were destabilizing the layout.
                        let log_w = ui.available_width();
                        ui.set_max_width(log_w);
                        let mut shown = 0usize;
                        for d in dv.entries.iter().rev() {
                            let (enabled, color, level) = match d.severity {
                                DiagnosticSeverity::Event => {
                                    (self.show_info, ui.visuals().weak_text_color(), "INFO ")
                                }
                                DiagnosticSeverity::Warning => {
                                    (self.show_warn, palette(ui).warning, "WARN ")
                                }
                                DiagnosticSeverity::Error => {
                                    (self.show_error, palette(ui).fault, "ERROR")
                                }
                            };
                            if !enabled {
                                continue;
                            }
                            let dt: chrono::DateTime<chrono::Local> = d.timestamp.into();
                            let msg = &d.message;
                            shown += 1;
                            // Wrap long entries so the full message stays readable
                            // (a plain colored_label was clipped at the pane edge).
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "{}  {level}  {msg}",
                                        dt.format("%H:%M:%S%.3f")
                                    ))
                                    .color(color),
                                )
                                .wrap(),
                            );
                        }
                        if shown == 0 {
                            ui.label(egui::RichText::new("no diagnostics match the filter").weak());
                        }
                    });
            });
        }
    }

    /// The left channel/control column: status, live rename, byte stats, and
    /// the [Start / Apply & Restart / Retry] [Stop] lifecycle row. Both
    /// buttons are always present; Stop is disabled unless stoppable; the
    /// Start side's label/enabled state comes from the pure `start_button`
    /// decision.
    #[allow(clippy::too_many_arguments)]
    fn show_channel_controls(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        config_changed: bool,
        details: &str,
        bytes_total: u64,
        bps: f64,
    ) {
        // Name row: status glyph + editable name (renames live, §6).
        ui.horizontal(|ui| {
            // Painted into a fixed cell so the glyph never drives the row height.
            let (glyph, scale) = status_glyph(status);
            paint_glyph(ui, glyph, scale, status_color(status, palette(ui)));
            const NAME_HINT: &str = "This channel's display name. When file rotation is \
                on, it's also the base name of the rotated files (<channel>_<time \
                period>), so keep it filesystem-safe.";
            ui.label(bold("Name")).on_hover_text(NAME_HINT);
            let mut renamed = None;
            if let Some((_, config)) = &mut self.edit_draft {
                let mut name = config.name.as_str().to_string();
                if ui
                    .add(egui::TextEdit::singleline(&mut name).desired_width(180.0))
                    .on_hover_text(NAME_HINT)
                    .changed()
                {
                    config.name = crate::core::ChannelName::new(name.clone());
                    renamed = Some(name);
                }
            }
            if let Some(name) = renamed {
                // Names must be unique (§6, ADR-014). Commit only a name not already used
                // by another channel (case-insensitively); a duplicate is kept in the
                // draft (so the user can keep editing toward a unique name) but not sent
                // to the runtime, and an inline warning shows why. This channel itself is
                // excluded (`v.id != id`), so re-typing its own current name is fine.
                let is_duplicate = self
                    .state
                    .channels()
                    .any(|v| v.id != id && v.name.eq_ignore_ascii_case(&name));
                self.name_duplicate = is_duplicate;
                if !is_duplicate {
                    // Tell the runtime AND fold locally — the echoed ChannelRenamed is
                    // advisory/lossy (§99), so the optimistic local apply keeps the list
                    // row and re-seeded draft authoritative (#5).
                    self.send(UiCommand::Rename(
                        id,
                        crate::core::ChannelName::new(name.clone()),
                    ));
                    self.state.apply(bridge::UiUpdate::ChannelRenamed(id, name));
                }
            }
            if self.name_duplicate {
                ui.label(
                    egui::RichText::new("⚠ name already in use — names must be unique")
                        .color(palette(ui).warning),
                );
            }
        });
        ui.horizontal(|ui| {
            ui.label(status_label(status));
            ui.label("·");
            ui.label(egui::RichText::new(details).weak());
        });
        // Byte-based liveness (§18): total received + rolling throughput.
        ui.label(format!(
            "Received: {}    Throughput: {}",
            human_bytes(bytes_total),
            human_byte_rate(bps)
        ))
        .on_hover_text(THROUGHPUT_TOOLTIP);
        ui.add_space(12.0); // a blank line between the readouts and the buttons
        let size = CONTROL_BUTTON_SIZE;
        ui.horizontal(|ui| {
            let (start_label, start_enabled) = start_button(status, config_changed);
            if ui
                .add_enabled(start_enabled, egui::Button::new(start_label).min_size(size))
                .clicked()
            {
                // Start / Apply & Restart / Retry are all the same action: commit the
                // edited config and bring the channel up. `try_start` sends one
                // CommitAndStart; the runtime handles the Running-restart and the
                // Faulted→Stopped→Starting recovery (§8.5) — no per-state client steps.
                self.try_start(id);
            }
            if ui
                .add_enabled(
                    stop_enabled(status),
                    egui::Button::new("Stop Channel").min_size(size),
                )
                .clicked()
            {
                self.send(UiCommand::Stop(id));
            }
        });
    }

    /// Recording block (right column): the **Raw** section (header row with live
    /// state glyph + Start/Stop button, over a collapsible setup) and, right under
    /// it, the **Display** section in the same shape (ADR-013 — two independent
    /// recordings, same options). The Raw live toggle reads the settings from the
    /// editor at click time (ADR-012); both taps' setup edits apply live
    /// (`persist_recording`), never via Apply & Restart.
    ///
    /// Kept in small pieces (the header rows, `record_button`,
    /// `recording_setup_section`) because this block is still evolving — add new
    /// recording controls as their own helpers rather than growing this method.
    fn show_recording_block(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        recording: Option<RecordingState>,
    ) {
        // Raw header row: title + live state indicator + the Start/Stop recording
        // button on the same line (no expander — the setup follows below).
        ui.horizontal(|ui| {
            ui.label("Record Raw Data");
            // Status glyph (same symbol set/colors as channel status). The common
            // words ("recording"/"off"/"faulted") are dropped to keep the row
            // compact — the glyph ■/●/⚠ carries them — but a gap says what it is
            // waiting for.
            let (glyph, color, text) = recording_indicator(recording, palette(ui));
            paint_glyph(ui, glyph, recording_glyph_size(glyph), color);
            if matches!(recording, Some(RecordingState::Gap(_))) {
                ui.label(egui::RichText::new(text).weak());
            }
            self.record_button(ui, id, status, recording, RecTap::Raw);
        });
        let raw_status = self
            .state
            .channel(id)
            .and_then(|v| v.raw_recording_status.clone());
        show_recording_facts(ui, raw_status.as_ref());
        if self.edit_draft.as_ref().map(|(eid, _)| *eid) != Some(id) {
            ui.label(egui::RichText::new("(select the channel to edit)").weak());
            return;
        }
        if let Some((_, config)) = &mut self.edit_draft {
            let summary = {
                let rec = &config.raw_recording;
                record_summary(&rec.destination, rec.file_rotation, rec.overwrite_policy)
            };
            recording_setup_section(ui, ("raw_rec_setup", id), &summary, |ui| {
                edit_raw_recording(ui, config)
            });
        }
        self.persist_recording(id, RecTap::Raw);

        // Display recording (§54): the sibling tap, same options, same layout, same
        // live toggle (ADR-012/-013) — begin/stop mid-run from click-time settings.
        ui.separator();
        let display_recording = self.state.channel(id).and_then(|v| v.display_recording);
        ui.horizontal(|ui| {
            ui.label("Record Display");
            let (glyph, color, text) = recording_indicator(display_recording, palette(ui));
            paint_glyph(ui, glyph, recording_glyph_size(glyph), color);
            if matches!(display_recording, Some(RecordingState::Gap(_))) {
                ui.label(egui::RichText::new(text).weak());
            }
            self.record_button(ui, id, status, display_recording, RecTap::Display);
        });
        let display_status = self
            .state
            .channel(id)
            .and_then(|v| v.display_recording_status.clone());
        show_recording_facts(ui, display_status.as_ref());
        if let Some((_, config)) = &mut self.edit_draft {
            let summary = {
                let rec = &config.display_recording;
                record_summary(&rec.destination, rec.file_rotation, rec.overwrite_policy)
            };
            recording_setup_section(ui, ("disp_rec_setup", id), &summary, |ui| {
                edit_display_recording(ui, config)
            });
        }
        self.persist_recording(id, RecTap::Display);
    }

    /// The recording controls on a tap's header row: a "Record on start" toggle
    /// (begins recording when the channel next starts, §53/§54) and, for a running
    /// channel, the live Record/Stop button (ADR-012). The button reads the
    /// on-screen settings *at click time* (from the edit draft) and sends them with
    /// the command, so recording goes exactly where the controls say — no restart,
    /// no Apply. One implementation for both taps (ADR-013 symmetry).
    fn record_button(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        recording: Option<RecordingState>,
        tap: RecTap,
    ) {
        // "Record on start" — the auto-start flag, editable whether or not the
        // channel is running (it governs the next start).
        if let Some((_, config)) = &mut self.edit_draft {
            let (flag, hover) = match tap {
                RecTap::Raw => (
                    &mut config.raw_recording.enabled,
                    "Begin recording automatically when the channel starts (§53).",
                ),
                RecTap::Display => (
                    &mut config.display_recording.enabled,
                    "Record the rendered view output (.disp) — what the display shows,                      not the raw bytes (§54) — automatically when the channel starts.",
                ),
            };
            ui.checkbox(flag, "Record on start").on_hover_text(hover);
        }
        if status != ChannelStatus::Running {
            return;
        }
        let draft = self
            .edit_draft
            .as_ref()
            .filter(|(eid, _)| *eid == id)
            .map(|(_, cfg)| cfg);
        let has_dest = draft.is_some_and(|cfg| match tap {
            RecTap::Raw => cfg.raw_recording.destination.is_some(),
            RecTap::Display => cfg.display_recording.destination.is_some(),
        });
        // A recording in a gap is still on — it resumes by itself — so it
        // offers Stop, not Record.
        let recording_now = recording.is_some_and(RecordingState::is_on);
        let label = if recording_now { "Stop" } else { "Record" };
        // Match the start-channel button's *width* (96) but keep the default height —
        // a full CONTROL_BUTTON_SIZE min_size plus a long label made it both too wide
        // and too tall. Short labels fit the 96px width.
        let resp = ui.add_enabled(
            has_dest || recording_now,
            egui::Button::new(label).min_size(egui::vec2(CONTROL_BUTTON_SIZE.x, 0.0)),
        );
        let resp = if !has_dest && !recording_now {
            resp.on_hover_text("Set a destination below first")
        } else {
            resp
        };
        if resp.clicked() {
            let begin = !recording_now;
            let cmd = match tap {
                RecTap::Raw => UiCommand::SetRecording(
                    id,
                    begin,
                    Box::new(draft.map(|c| c.raw_recording.clone()).unwrap_or_default()),
                ),
                RecTap::Display => UiCommand::SetDisplayRecording(
                    id,
                    begin,
                    Box::new(
                        draft
                            .map(|c| c.display_recording.clone())
                            .unwrap_or_default(),
                    ),
                ),
            };
            self.send(cmd);
        }
    }

    /// Persist a tap's recording edits into the stored config and runtime. Both
    /// recordings are live fields (ADR-012), so their edits never travel through
    /// the Apply & Restart path — without this, a profile save wouldn't capture
    /// them. When the draft differs from the channel's stored config, fold it in
    /// and sync the runtime.
    fn persist_recording(&mut self, id: ChannelId, tap: RecTap) {
        let Some((eid, cfg)) = self.edit_draft.as_ref() else {
            return;
        };
        if *eid != id {
            return;
        }
        match tap {
            RecTap::Raw => {
                let draft = cfg.raw_recording.clone();
                if let Some(view) = self.state.channel_mut(id) {
                    if view.config.raw_recording != draft {
                        view.config.raw_recording = draft.clone();
                        self.send(UiCommand::SetRawRecordingConfig(id, Box::new(draft)));
                    }
                }
            }
            RecTap::Display => {
                let draft = cfg.display_recording.clone();
                if let Some(view) = self.state.channel_mut(id) {
                    if view.config.display_recording != draft {
                        view.config.display_recording = draft.clone();
                        self.send(UiCommand::SetDisplayRecordingConfig(id, Box::new(draft)));
                    }
                }
            }
        }
    }
}

/// Shorten a full diagnostic message into a headline phrase by cutting at the first
/// natural boundary, so the title-row headline reads as a clean phrase rather than a
/// mid-word truncation. Drops the *detail* tail:
/// - `→` separates a subject from its target — keep the subject ("Raw recording
///   started → C:\…" → "Raw recording started").
/// - ` — ` / `: ` introduce an explanation — keep up to and including the first
///   `<name>:` segment but drop a following explanatory clause ("UDP_Channel3: failed
///   to bind interface: Only one usage… (os error 10048)" → "UDP_Channel3: failed to
///   bind interface").
///
/// Falls back to the whole (trimmed) message when there is no such boundary; the egui
/// label still ellipsizes if even the phrase overflows the row.
fn headline_phrase(message: &str) -> String {
    // First, drop a `→ target` tail (recording destinations etc.).
    let head = message.split('→').next().unwrap_or(message).trim();
    // Then drop an explanatory clause after the *second* `: ` (the first `: ` is the
    // "<name>: <kind>" separator we want to keep) or after a ` — ` dash.
    let mut cut = head.len();
    if let Some(dash) = head.find(" — ") {
        cut = cut.min(dash);
    }
    // Keep the first "<name>: <kind>" but trim a second ": <detail>".
    if let Some(first_colon) = head.find(": ") {
        if let Some(rel) = head[first_colon + 2..].find(": ") {
            cut = cut.min(first_colon + 2 + rel);
        }
    }
    head[..cut].trim_end().to_string()
}

/// The facts line under a running recording's header (§56.2); nothing when it
/// is not running. Hovering shows the full path of the file being written.
fn show_recording_facts(ui: &mut egui::Ui, status: Option<&crate::runtime::RecordingStatus>) {
    let Some(status) = status else {
        return;
    };
    let line = recording_facts(status, std::time::SystemTime::now());
    let label = ui.label(egui::RichText::new(line).weak());
    if let Some(path) = &status.current_file {
        label.on_hover_text(path.display().to_string());
    }
}

/// A collapsible "Setup" section for one recording (collapsed by default): the
/// closed header carries a one-line `path · rotation · on-exists` summary so the
/// configured destination stays visible without the controls; open shows the full
/// editor. Shared by the Raw and Display blocks (ADR-013 — same options).
fn recording_setup_section(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash,
    summary: &str,
    body: impl FnOnce(&mut egui::Ui),
) {
    let setup_id = ui.make_persistent_id(salt);
    let state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), setup_id, false);
    let open = state.is_open();
    state
        .show_header(ui, |ui| {
            ui.label("Setup");
            // The summary rides the (closed) header so it reads as one line; when
            // open, the full editor is in the body below, so keep the header terse.
            if !open {
                ui.label(egui::RichText::new(summary).weak());
            }
        })
        .body(body);
}

/// A one-line summary of a recording setup for the collapsed Setup header:
/// `path · rotation · on-exists` (e.g. `C:\logs\gps.raw · Daily · Append`). The path
/// reads "(no destination)" when unset; rotation/on-exists use short words. Shared
/// by the Raw and Display blocks — their configs carry the same fields (ADR-013).
fn record_summary(
    destination: &Option<std::path::PathBuf>,
    file_rotation: crate::record::FileRotationPolicy,
    overwrite_policy: crate::record::OverwritePolicy,
) -> String {
    use crate::record::{FileRotationPolicy, OverwritePolicy};
    let path = destination
        .as_ref()
        .map(|p| shorten_path(p))
        .unwrap_or_else(|| "(no destination)".to_string());
    let rotation = match file_rotation {
        FileRotationPolicy::None => "no rotation",
        FileRotationPolicy::Hourly => "Hourly",
        FileRotationPolicy::Daily => "Daily",
    };
    let on_exists = match overwrite_policy {
        OverwritePolicy::Refuse => "Refuse",
        OverwritePolicy::Overwrite => "Overwrite",
        OverwritePolicy::AppendIfExists => "Append",
    };
    format!("{path} · {rotation} · {on_exists}")
}

/// Shorten a path for a compact display: collapse the user's home directory to `~`
/// (the OS-idiomatic shorthand — `USERPROFILE` on Windows, `HOME` elsewhere), then, if
/// still long, middle-ellipsize so the start and the filename stay visible
/// (`C:\logs\…\gps.raw`). Display-only — never used for the actual path.
fn shorten_path(path: &std::path::Path) -> String {
    const MAX: usize = 28; // characters before middle-ellipsizing (aggressive)

    // Collapse $HOME / %USERPROFILE% to ~.
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from);
    let s = match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    };

    if s.chars().count() <= MAX {
        return s;
    }
    // Keep the filename whole; ellipsize the directory prefix in the middle.
    let sep = std::path::MAIN_SEPARATOR;
    let (dir, file) = match s.rfind(sep) {
        Some(i) => (&s[..i], &s[i + sep.len_utf8()..]),
        None => return s, // single component longer than MAX — leave it
    };
    // Budget for the directory part after reserving the filename + "…\" markers.
    let keep = MAX.saturating_sub(file.chars().count() + 3);
    let head: String = dir.chars().take(keep).collect();
    format!("{head}…{sep}{file}")
}

#[cfg(test)]
mod tests {
    use super::{headline_phrase, FINAL_SNAPSHOT_TOOLTIP, THROUGHPUT_TOOLTIP};

    #[test]
    fn headline_phrase_cuts_at_natural_boundaries() {
        // `→ target` is dropped (recording destination).
        assert_eq!(
            headline_phrase("Raw recording started → C:\\Users\\me\\Desktop\\poop"),
            "Raw recording started"
        );
        // A second `: detail` (the OS reason) is dropped; the "<name>: <kind>" is kept.
        assert_eq!(
            headline_phrase(
                "UDP_Channel3: failed to bind interface: Only one usage of each socket \
                 address (os error 10048)"
            ),
            "UDP_Channel3: failed to bind interface"
        );
        // A ` — ` explanatory clause is dropped.
        assert_eq!(
            headline_phrase("recording could not start — check the destination"),
            "recording could not start"
        );
        // No boundary → the whole (trimmed) message is kept (egui ellipsizes if needed).
        assert_eq!(headline_phrase("connected"), "connected");
        assert_eq!(headline_phrase("  spaced  "), "spaced");
    }

    #[test]
    fn throughput_and_final_snapshot_help_prevent_false_precision() {
        assert!(THROUGHPUT_TOOLTIP.contains("divided by five seconds"));
        assert!(THROUGHPUT_TOOLTIP.contains("full five-second denominator"));
        assert!(THROUGHPUT_TOOLTIP.contains("after Stop the rate is zero"));
        assert!(FINAL_SNAPSHOT_TOOLTIP.contains("channel's final snapshot"));
        assert!(FINAL_SNAPSHOT_TOOLTIP.contains("must not be read as measured zero"));
    }
}
