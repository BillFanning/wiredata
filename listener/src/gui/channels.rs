//! The channel-list (tabs) panel and its row snapshot, split out of `mod.rs`.

use crate::core::ChannelId;
use wiredata_ui::format::human_byte_rate;
use wiredata_ui::selection;

use super::bridge::UiCommand;
use super::state::ChannelStatus;
use super::widgets::{human_bytes, status_color, status_glyph, AddKind};
use super::ListenerApp;

/// One channel's row data, snapshotted before rendering so the list isn't borrowing
/// the view-model while a click mutates the selection.
struct ChannelRow {
    id: ChannelId,
    name: String,
    details: String,
    status: ChannelStatus,
    bytes_total: u64,
    bytes_per_sec: f64,
    info: usize,
    warnings: usize,
    errors: usize,
    /// The channel's last error (e.g. a recording fault), shown on the tab so a fault on
    /// a non-selected channel is visible without opening it.
    last_error: Option<String>,
}

impl ListenerApp {
    pub(super) fn show_channel_list(
        &mut self,
        ui: &mut egui::Ui,
    ) -> Option<selection::SelectedTab> {
        // Heading + the "Add" menu (the only place channels are created now), plus
        // bulk Start all / Stop all (#5).
        ui.horizontal(|ui| {
            if ui
                .button("\u{25C0}")
                .on_hover_text("Collapse channel list")
                .clicked()
            {
                self.channels_collapsed = true;
            }
            ui.heading("Channels");
            // Theme toggle — the shared button (same storage key as talker,
            // so the two apps read and behave identically). Both themes'
            // visuals are pre-installed (ADR-019).
            wiredata_ui::style::theme_toggle_button(ui, &mut self.dark_mode);
            ui.menu_button("Profile", |ui| {
                // Fixed width so the menu is the same size in both apps rather
                // than sized by whichever recent-file name happens to be longest.
                ui.set_min_width(selection::PROFILE_MENU_WIDTH);
                // Recent profiles at the top: one click reloads (replaces the
                // workspace, §70). Most-recent-first. The header always shows (with a
                // placeholder when empty) so the section is visibly present.
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
                            self.load_profile_path(path);
                            ui.close();
                        }
                    }
                }
                ui.separator();
                // "Save" writes to the current file (or prompts if there is none);
                // "Save As…" always prompts and re-points the current file.
                if ui.button("Save").clicked() {
                    self.save_profile();
                    ui.close();
                }
                if ui.button("Save As…").clicked() {
                    self.save_profile_as();
                    ui.close();
                }
                if ui.button("Load…").clicked() {
                    self.load_profile_dialog();
                    ui.close();
                }
                ui.separator();
                // Resume on launch (ADR-045): registered in application state,
                // never in the profile, and only for a profile saved or loaded.
                self.resume_menu_item(ui);
                ui.separator();
                if ui.button("New").clicked() {
                    self.new_profile();
                    ui.close();
                }
            });
        });
        // Bulk actions on their own row, as in talker. A `horizontal` does not
        // wrap, so its minimum width is the sum of everything in it, and that
        // minimum is the floor the channel panel can be dragged down to. With
        // these two buttons in the header row the floor exceeded the panel's own
        // default width, so the divider could be dragged right but never back
        // left.
        ui.horizontal(|ui| {
            if ui.button("Start all").clicked() {
                // Only Stopped channels — starting an already-Running one would be an
                // illegal Running→Starting transition. Validate each (unconfigured ones
                // get an inline complaint, #3/#6) and batch the ready ones into a single
                // StartAll, rather than a 2N-command Reconfigure+Start burst that could
                // overflow the bounded command channel (the bug Stop all hit).
                self.start_all();
            }
            if ui.button("Stop all").clicked() {
                // One command, not an N-command burst: a burst could overflow the
                // bounded command channel (and the resulting ChannelStopped events the
                // event channel), leaving some channels stuck Running in the UI. The
                // driver iterates server-side and skips already-Stopped channels.
                self.send(UiCommand::StopAll);
            }
            // "+ Add" rides with the bulk actions rather than the title row, so
            // the header carries only the title, theme toggle and Profile. That
            // also lowers the header row's minimum width, which is the floor the
            // channel panel can be dragged down to.
            ui.menu_button("+ Add", |ui| {
                for kind in AddKind::ADD_MENU {
                    if ui.button(kind.label()).clicked() {
                        self.add_channel(kind);
                        ui.close();
                    }
                }
            });
        });
        // The last Save/Load outcome (e.g. "Saved foo.toml" or an error), if any.
        if let Some(status) = self.state.workspace_status() {
            ui.label(egui::RichText::new(status).weak());
        }
        ui.separator();

        // Snapshot the rows first so the list isn't borrowing `state` while a click
        // mutates `selected`.
        let rows: Vec<ChannelRow> = self
            .state
            .channels()
            .enumerate()
            .map(|(index, v)| ChannelRow {
                id: v.id,
                name: selection::channel_title(index + 1, &v.name),
                details: v.details.clone(),
                status: v.status,
                bytes_total: v.bytes_total,
                bytes_per_sec: v.bytes_per_sec,
                info: v.info,
                warnings: v.warnings,
                errors: v.errors,
                last_error: v.last_error.clone(),
            })
            .collect();

        if rows.is_empty() {
            ui.label("No channels yet — use “+ Add”.");
            return None;
        }

        let base = egui::TextStyle::Body.resolve(ui.style()).size;
        let mut selected_tab_rect = None;
        let scroll = egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(selection::TAB_JOIN_MARGIN);
            for row in rows {
                let id = row.id;
                let selected = self.selected == Some(id);
                // Each box gets its own id scope so widget ids don't collide between
                // channels (that collision was why the 2nd channel couldn't be
                // selected, #4).
                let visible_selected_rect = ui.push_id(id, |ui| {
                    let inner = selection::channel_card(ui, selected, |ui| {
                        // Line 1: status glyph + name.
                        let emphasis = selection::channel_row_emphasis(ui, selected);
                        let mut line1 = egui::text::LayoutJob::default();
                        let (glyph, scale) = status_glyph(row.status);
                        line1.append(
                            glyph,
                            0.0,
                            egui::TextFormat {
                                font_id: egui::FontId::proportional(base * scale),
                                color: status_color(row.status, wiredata_ui::palette::active(ui)),
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
                        ui.label(line1);
                        // Line 2: connection details.
                        ui.label(egui::RichText::new(&row.details).weak());
                        // Line 3: live stats.
                        ui.label(
                            egui::RichText::new(format!(
                                "{}  ·  {}",
                                human_bytes(row.bytes_total),
                                human_byte_rate(row.bytes_per_sec)
                            ))
                            .weak(),
                        );
                        // Line 4: per-severity diagnostic counts, color-coded
                        // (#8) — the shared row line.
                        selection::severity_counts_line(
                            ui,
                            row.info as u64,
                            row.warnings as u64,
                            row.errors as u64,
                            &emphasis,
                            None,
                        );
                        // Line 5: the last error (e.g. a recording fault), so a fault on
                        // this channel is visible on its tab even when it isn't selected.
                        if let Some(err) = &row.last_error {
                            selection::last_error_line(ui, err);
                        }
                    });
                    // Box-select: sense a click on the whole box first.
                    let box_resp = inner.response.interact(egui::Sense::click());
                    // The ✕ remove button is placed ON TOP, in the box's top-right
                    // corner, AFTER the box interaction — so it's last in z-order and
                    // wins the click instead of the box stealing it (the earlier
                    // structure had the box's `interact` swallow the button's click).
                    let btn_size = egui::vec2(20.0, 20.0);
                    let btn_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            box_resp.rect.right() - btn_size.x - 6.0,
                            box_resp.rect.top() + 6.0,
                        ),
                        btn_size,
                    );
                    let remove_resp = ui
                        .put(btn_rect, egui::Button::new("\u{2715}").small())
                        .on_hover_text("Remove channel");
                    if remove_resp.clicked() {
                        self.confirm_remove = Some(id);
                    } else if box_resp.clicked() {
                        // A box click that wasn't the ✕ selects the channel (#8).
                        self.selected = Some(id);
                    }
                    selected.then_some(box_resp.rect)
                });
                if visible_selected_rect.inner.is_some() {
                    selected_tab_rect = visible_selected_rect.inner;
                }
                ui.add_space(selection::TAB_JOIN_MARGIN);
            }
        });
        selected_tab_rect.map(|rect| selection::SelectedTab::new(rect, scroll.inner_rect))
    }

    /// Save to the current profile path silently; if none is set yet, fall through to
    /// "Save As…" so the first save still names a file (§67).
    pub(super) fn save_profile(&mut self) {
        match self.current_profile_path.clone() {
            Some(path) => self.send(UiCommand::SaveProfile(path)),
            None => self.save_profile_as(),
        }
    }

    /// Always prompt for a destination, then save there and remember it as the
    /// current profile path. Defaults to the app's `profiles/` directory (next to the
    /// exe) on first use.
    ///
    /// **The one intentional exception to "the UI thread never blocks / never does
    /// I/O" (AGENTS §5).** A native file picker is inherently modal and synchronous, so
    /// `save_file()`/`pick_file()` (and the best-effort `profiles_dir()` mkdir that
    /// seeds them) run on the UI thread for the moment the dialog is open. That's
    /// acceptable: it's a user-driven modal, not background work, and the only blocking
    /// call. The actual profile *write* still happens off-thread in the driver (§67) —
    /// this method just hands it the chosen path.
    pub(super) fn save_profile_as(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter("TOML profile", &["toml"]);
        // Seed the picker with the current file's name/location if we have one;
        // otherwise default to the app profiles directory.
        dialog = match self.current_profile_path.as_ref() {
            Some(path) => {
                if let Some(dir) = path.parent() {
                    dialog = dialog.set_directory(dir);
                }
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("listener.toml");
                dialog.set_file_name(name)
            }
            None => {
                if let Some(dir) = profiles_dir() {
                    dialog = dialog.set_directory(dir);
                }
                dialog.set_file_name("listener.toml")
            }
        };
        if let Some(path) = dialog.save_file() {
            self.current_profile_path = Some(path.clone());
            self.remember_recent_profile(path.clone());
            self.send(UiCommand::SaveProfile(path));
        }
    }

    /// Open a native "load profile" dialog and, if the user picks a file, load it.
    /// Defaults to the app's `profiles/` directory.
    fn load_profile_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter("TOML profile", &["toml"]);
        // Open in the last-used profile's directory, else the app profiles directory.
        let start = self
            .current_profile_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .or_else(profiles_dir);
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.load_profile_path(path);
        }
    }

    /// Discard the workspace and start empty.
    ///
    /// Always confirms: Listener tracks no dirty flag, so it cannot know whether
    /// anything would be lost — and this removes every channel, running ones
    /// included. Uses the same native dialog as talker's New.
    pub(super) fn new_profile(&mut self) {
        let confirmed = rfd::MessageDialog::new()
            .set_title("New Profile")
            .set_description("Remove all channels and start a new profile?")
            .set_buttons(rfd::MessageButtons::OkCancel)
            .show()
            == rfd::MessageDialogResult::Ok;
        if !confirmed {
            return;
        }
        self.current_profile_path = None;
        self.send(UiCommand::NewProfile);
    }

    /// The Profile menu's "Resume this profile on launch" choice (ADR-045), for
    /// the current profile, and which profile resumes when that is another.
    fn resume_menu_item(&mut self, ui: &mut egui::Ui) {
        const HINT: &str = "When Listener starts, it checks this profile, waits 10 s so \
            you can cancel, then loads it and starts every channel. Recordings set to \
            record on start begin too.";
        match self.current_profile_path.clone() {
            Some(current) => {
                let mut on = self.resume_profile.as_ref() == Some(&current);
                if ui
                    .checkbox(&mut on, "Resume this profile on launch")
                    .on_hover_text(HINT)
                    .changed()
                {
                    self.resume_profile = on.then_some(current);
                }
            }
            None => {
                ui.add_enabled(
                    false,
                    egui::Checkbox::new(&mut false, "Resume this profile on launch"),
                )
                .on_disabled_hover_text("Save or load a profile first.");
            }
        }
        let other = self
            .resume_profile
            .clone()
            .filter(|path| Some(path) != self.current_profile_path.as_ref());
        if let Some(other) = other {
            let name = other.file_name().map_or_else(
                || other.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            ui.label(egui::RichText::new(format!("Resumes on launch: {name}")).weak())
                .on_hover_text(other.display().to_string());
            if ui.button("Stop resuming on launch").clicked() {
                self.resume_profile = None;
            }
        }
    }

    /// Load a profile from a known path (used by both the picker and the recent-files
    /// menu): replace the workspace (§70), remember it as current, and bump it to the
    /// top of the recents list.
    pub(super) fn load_profile_path(&mut self, path: std::path::PathBuf) {
        self.current_profile_path = Some(path.clone());
        self.remember_recent_profile(path.clone());
        self.send(UiCommand::LoadProfile(path));
    }
}

/// The app's profile directory: `profiles/` beside the running executable. Created
/// best-effort if absent (it's just the picker's starting folder). Returns `None`
/// if the exe path can't be resolved or the directory can't be created — e.g. the
/// app was installed somewhere read-only — in which case the picker opens at the
/// OS default rather than failing.
fn profiles_dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join("profiles");
    // Best-effort: if it exists already this is a no-op; if creation fails, fall back.
    if !dir.exists() && std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    Some(dir)
}
