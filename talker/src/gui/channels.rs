//! The channel-list panel (master side of the master–detail layout) and its
//! collapsed mini-strip. Rows are snapshotted before rendering so the list
//! isn't borrowing the app state while a click mutates the selection.

use wiredata_ui::{glyphs, palette::Palette, selection};

use super::draft::ConnKind;
use super::widgets::{interface_summary, lifecycle_indicator};
use super::TalkerApp;
use wiredata_ui::hover::{Hover, ShowHover};
use wiredata_ui::palette::active as theme_palette;

/// One channel's row data, snapshotted before rendering.
struct ChannelRow {
    /// Display name, already resolved to the positional fallback.
    name: String,
    /// One-line interface summary (may contain `?` for unfilled fields).
    summary: String,
    running: bool,
    /// The channel's current error, if any — shown on the row so a fault
    /// on a non-selected channel is visible without opening it.
    error: Option<String>,
    sent: u64,
    /// Messages per second over the last sampling window (0 when idle).
    per_sec: f32,
    /// Log events attributed to this channel since its last start.
    info: u32,
    warnings: u32,
    errors: u32,
}

impl TalkerApp {
    /// Resolve channel `i`'s display name: the user's name, or the
    /// positional fallback.
    pub(super) fn channel_name(&self, i: usize) -> String {
        match self.conn_drafts.get(i) {
            Some(d) if !d.name.is_empty() => d.name.clone(),
            _ => format!("Channel {}", i + 1),
        }
    }

    /// The label frozen into a run's log text at start (ADR-020): the custom
    /// name in quotes ("channel 'GPS' running"), else the 1-based position at
    /// start time ("channel 3 running" — same as the pre-ADR-020 lines).
    /// Attribution never rides this text; it rides the slot's stable id.
    pub(super) fn channel_label(&self, i: usize) -> String {
        match self.conn_drafts.get(i) {
            Some(d) if !d.name.is_empty() => format!("'{}'", d.name),
            _ => (i + 1).to_string(),
        }
    }

    pub(super) fn show_channel_list(
        &mut self,
        ui: &mut egui::Ui,
    ) -> Option<selection::SelectedTab> {
        let pal = theme_palette(ui);

        // Header: collapse, title, + Add and Profile menus (listener's header
        // shape — the Profile menu sits next to "+ Add").
        ui.horizontal(|ui| {
            if ui
                .button("\u{25C0}")
                .on_hover_text("Collapse channel list")
                .clicked()
            {
                self.channels_collapsed = true;
            }
            ui.heading("Channels");
            self.show_profile_menu(ui);
        });
        // Bulk actions on their own row so the header doesn't crowd. Plain
        // labels and an always-present Stop all, as in listener (stopping
        // with nothing running is a no-op).
        ui.horizontal(|ui| {
            let can_start_any = self.can_start_any();
            let start_all = ui.add_enabled(can_start_any, egui::Button::new("Start all"));
            if start_all.clicked() {
                self.deferred.start_all = true;
            }
            if !can_start_any {
                start_all.on_disabled_hover_text("Add at least one valid channel and one message");
            }
            if ui.button("Stop all").clicked() {
                self.deferred.stop_all = true;
            }
            // "+ Add" rides with the bulk actions rather than the title row, so
            // the header carries only the title and Profile. That also lowers
            // the header row's minimum width, which is the floor the channel
            // panel can be dragged down to.
            ui.menu_button("+ Add", |ui| {
                for kind in ConnKind::ADD_MENU {
                    if ui.button(kind.label()).clicked() {
                        self.deferred.add_channel = Some(kind);
                        ui.close();
                    }
                }
            });
        });
        // The current profile + dirty marker (listener's status line under
        // its header). Renaming happens via Save As…, as in listener.
        let name = if self.profile.name.is_empty() {
            "(unsaved profile)"
        } else {
            self.profile.name.as_str()
        };
        let marker = if self.dirty { " *" } else { "" };
        ui.label(egui::RichText::new(format!("{name}{marker}")).weak())
            .on_hover(&Hover::new(
                "The loaded profile. A * after its name means unsaved changes.\n\n\
                 Ctrl+S saves. To rename the profile, save it to a new file with Profile → Save \
                 As….",
            ));
        ui.separator();

        // Snapshot the rows, then render — the click handler mutates
        // `selected` (via deferred), which must not alias the borrow.
        let rows: Vec<ChannelRow> = (0..self.conn_drafts.len())
            .map(|i| {
                // Borrowed telemetry: rows read two fields, so don't clone
                // the whole struct per channel per frame.
                let telemetry = self.sup.telemetry_ref(i);
                // Row → tally via the slot's stable id (ADR-020): the id
                // travels with the slot, so this stays right after removals.
                let counts = self
                    .sup
                    .channel_id(i)
                    .and_then(|id| self.log_counts.get(&id).copied())
                    .unwrap_or_default();
                ChannelRow {
                    name: selection::channel_title(i + 1, &self.conn_drafts[i].name),
                    summary: interface_summary(&self.conn_drafts[i]),
                    running: self.is_connection_running(i),
                    error: telemetry.and_then(|t| t.banner_error()).map(str::to_owned),
                    sent: telemetry.map(|t| t.total_count).unwrap_or_default(),
                    per_sec: self.views.get(i).map(|v| v.rate.per_sec).unwrap_or(0.0),
                    info: counts.info,
                    warnings: counts.warn,
                    errors: counts.error,
                }
            })
            .collect();

        let mut selected_tab_rect = None;
        let scroll = egui::ScrollArea::vertical().show(ui, |ui| {
            if rows.is_empty() {
                ui.weak("No channels yet — “+ Add” above.");
            } else {
                ui.add_space(selection::TAB_JOIN_MARGIN);
                for (i, row) in rows.iter().enumerate() {
                    if let Some(rect) = self.show_channel_row(ui, i, row, pal) {
                        selected_tab_rect = Some(rect);
                    }
                    ui.add_space(selection::TAB_JOIN_MARGIN);
                }
            }
        });
        selected_tab_rect.map(|rect| selection::SelectedTab::new(rect, scroll.inner_rect))
    }

    /// The Profile menu (listener's structure): the Recent section, then
    /// Save / Save As… / Load…, plus talker's New at the bottom. Lives in the
    /// channel-list header next to "+ Add", as in listener.
    fn show_profile_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Profile", |ui| {
            // Fixed width so the menu is the same size in both apps rather than
            // sized by whichever recent-file name happens to be longest.
            ui.set_min_width(selection::PROFILE_MENU_WIDTH);
            // Recent profiles at the top: one click reloads. Most-recent-
            // first. The header always shows (with a placeholder when empty)
            // so the section is visibly present.
            ui.label(egui::RichText::new("Recent").weak());
            if self.recent_profiles.is_empty() {
                ui.add_enabled(false, egui::Button::new("(none yet)"));
            } else {
                let recents = self.recent_profiles.clone();
                for path in recents {
                    let label = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_else(|| path.to_str().unwrap_or("profile"));
                    if ui
                        .button(label)
                        .on_hover_text(path.display().to_string())
                        .clicked()
                    {
                        if self.confirm_discard() {
                            self.load_profile_from_path(&path);
                        }
                        ui.close();
                    }
                }
            }
            ui.separator();
            if ui.button("Save").on_hover_text("Ctrl+S").clicked() {
                self.save_profile();
                ui.close();
            }
            if ui
                .button("Save As\u{2026}")
                .on_hover_text("Ctrl+Shift+S — write to a new file")
                .clicked()
            {
                self.save_profile_as();
                ui.close();
            }
            if ui.button("Load\u{2026}").on_hover_text("Ctrl+O").clicked() {
                self.load_profile_dialog();
                ui.close();
            }
            ui.separator();
            if ui.button("New").on_hover_text("Ctrl+N").clicked() {
                self.new_profile();
                ui.close();
            }
        });
    }

    fn show_channel_row(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        row: &ChannelRow,
        pal: &Palette,
    ) -> Option<egui::Rect> {
        let selected = self.selected == Some(i);
        // The shared connector opens the selected card's right edge into the
        // detail page after both panels have rendered.
        let (box_resp, remove_clicked) = ui
            .push_id(i, |ui| {
                let inner = selection::channel_card(ui, selected, |ui| {
                    // Line 1: status glyph + name, as one LayoutJob so the
                    // enlarged glyph centers on the text line instead of
                    // stretching the row (listener's line-1 pattern; same
                    // shared symbol set as the detail header).
                    let base = egui::TextStyle::Body.resolve(ui.style()).size;
                    let (glyph, color, word) =
                        lifecycle_indicator(row.running, row.error.is_some(), pal);
                    let emphasis = selection::channel_row_emphasis(ui, selected);
                    let mut line1 = egui::text::LayoutJob::default();
                    line1.append(
                        glyph,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(base * glyphs::glyph_size(glyph)),
                            color,
                            valign: egui::Align::Center,
                            ..Default::default()
                        },
                    );
                    line1.append(
                        &format!("  {}", row.name),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(base * 1.1),
                            color: emphasis.name,
                            valign: egui::Align::Center,
                            ..Default::default()
                        },
                    );
                    ui.label(line1)
                        .on_hover_text(row.error.as_deref().unwrap_or(word));
                    // Line 2: connection details.
                    ui.weak(&row.summary);
                    // Line 3: live local-acceptance stats,
                    // weak like listener's stats line.
                    if row.running {
                        let rate = if row.per_sec > 0.05 {
                            format!(" \u{00B7} {:.1} msg/s", row.per_sec)
                        } else {
                            String::new()
                        };
                        ui.label(
                            egui::RichText::new(format!("Sent: {}{rate}", row.sent)).weak(),
                        )
                        .on_hover(&Hover::new(
                            "The messages the interface accepted since this channel started.\n\n\
                             That does not confirm they reached the wire or a receiver. The rate \
                             is the average over the last five seconds.",
                        ));
                    }
                    // Line 4: per-severity log counts (since the channel's
                    // last start) — the shared row line (info · warn · err;
                    // historical counts recede on background tabs).
                    selection::severity_counts_line(
                        ui,
                        row.info.into(),
                        row.warnings.into(),
                        row.errors.into(),
                        &emphasis,
                        Some(
                            "Log events attributed to this channel \
                             since its last start",
                        ),
                    );
                    // Line 5: the live fault — saturated on every row.
                    if let Some(err) = &row.error {
                        selection::last_error_line(ui, err);
                    }
                });
                // Box-select: sense a click on the whole box first.
                let box_resp = inner.response.interact(egui::Sense::click());
                // The ✕ remove button is placed ON TOP, in the box's
                // top-right corner, AFTER the box interaction — so it's last
                // in z-order and wins the click instead of the box stealing
                // it (listener's row pattern).
                let btn_size = egui::vec2(20.0, 20.0);
                let btn_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        box_resp.rect.right() - btn_size.x - 6.0,
                        box_resp.rect.top() + 6.0,
                    ),
                    btn_size,
                );
                let remove_clicked = ui
                    .put(btn_rect, egui::Button::new("\u{2715}").small())
                    .on_hover_text("Remove channel")
                    .clicked();
                (box_resp, remove_clicked)
            })
            .inner;
        let selected_tab_rect = selected.then_some(box_resp.rect);
        // Removal confirms through the shared modal; a box click that
        // wasn't the ✕ selects the channel.
        if remove_clicked {
            self.confirm_remove = Some(i);
        } else if box_resp.clicked() {
            self.deferred.select = Some(i);
        }
        selected_tab_rect
    }

    /// The collapsed variant: a thin strip — an expand button plus one
    /// status dot per channel (click to select, name on hover).
    pub(super) fn show_channel_strip(
        &mut self,
        ui: &mut egui::Ui,
    ) -> Option<selection::SelectedTab> {
        let pal = theme_palette(ui);
        if ui
            .button("\u{25B6}")
            .on_hover_text("Show channels")
            .clicked()
        {
            self.channels_collapsed = false;
        }
        ui.separator();
        let clip_rect = ui.clip_rect();
        ui.add_space(selection::TAB_JOIN_MARGIN);
        let base = egui::TextStyle::Body.resolve(ui.style()).size;
        let mut selected_tab_rect = None;
        for i in 0..self.conn_drafts.len() {
            let running = self.is_connection_running(i);
            let error = self
                .sup
                .telemetry_ref(i)
                .is_some_and(|t| t.banner_error().is_some());
            let (glyph, color, _) = lifecycle_indicator(running, error, pal);
            let selected = self.selected == Some(i);
            let text = egui::RichText::new(glyph)
                .color(color)
                .size(base * glyphs::glyph_size(glyph));
            let title = selection::channel_title(i + 1, &self.conn_drafts[i].name);
            let response = selection::mini_tab(ui, selected, text).on_hover_text(title);
            if selected {
                selected_tab_rect = Some(response.rect);
            }
            if response.clicked() {
                self.deferred.select = Some(i);
            }
            ui.add_space(selection::TAB_JOIN_MARGIN);
        }
        selected_tab_rect.map(|rect| selection::SelectedTab::new(rect, clip_rect))
    }
}
