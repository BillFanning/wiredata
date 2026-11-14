//! The stream viewer: the toolbar (Pause/Resume, View mode, ctrl-chars, font/
//! colors/mono) above a virtualized, soft-wrapped byte view, plus its row cache and
//! scrollbar styling. This is the densest layout in the detail pane, so it lives on
//! its own — see the long comments inside for the layout invariants (row-pitch vs
//! `show_rows`, scrollbar handle visibility, viewer height/stick-to-bottom).

use crate::core::ChannelId;
use crate::display::{
    AnnotationPlacement, CharacterRendering, DisplayEncoding, DisplayMode, DisplayView,
    RenderAnnotation, StreamRenderer, WrappingMode,
};

use super::super::bridge::UiCommand;
use super::super::view_prefs::{
    hex_line_label, scroll_buffer_label, ViewPrefs, HEX_GROUP_SIZES, HEX_LINE_GROUPS,
    MAX_SCROLL_BUFFER_BYTES, MIN_SCROLL_BUFFER_BYTES, SCROLL_BUFFER_PRESETS_KB,
};
use super::super::widgets::{edit_mark_rules, human_bytes, ColorScheme, MSG_FONT_SIZES};
use super::super::ListenerApp;
use crate::config::schema::HexGrouping;
use wiredata_ui::fonts::MonoFont;
use wiredata_ui::hover::{Hover, ShowHover};

const PAUSE_HOVER: Hover = Hover::new(
    "Freezes the view so you can read it or scroll back.\n\n\
     The channel keeps receiving and any recording keeps running. Bytes that arrive \
     while the view is paused are not added to it, so they do not appear when you \
     press Resume. If a recording is running, they are in the recording.",
);

const GROUP_HOVER: Hover = Hover::new(
    "How many bytes are written together before a space in the Hex view.\n\n\
     With 1, every byte stands alone, as in a conventional hex dump (41 42 43 44). \
     With 4, four bytes run together as one word (41424344).\n\n\
     The grouping is part of the rendering, so a Display recording (.disp) has the \
     same spacing as the view.",
);

const LINE_HOVER: Hover = Hover::new(
    "How many groups fit on one line of the Hex view.\n\n\
     The choice \u{201C}fit to width\u{201D} lets the view decide, so a wider window shows more on each line. \
     A fixed number keeps the columns where they are while you resize the window. \
     If the window is too narrow for that many groups, fewer are shown. A byte is \
     never split across two lines; if not even one whole group fits, the line \
     breaks between bytes.\n\n\
     This changes only how the view is laid out. A Display recording (.disp) is \
     not broken into lines of a fixed length either way.",
);

const SCROLL_BUFFER_HOVER: Hover = Hover::new(
    "How far back you can scroll in this channel's view.\n\n\
     A larger buffer uses more memory and can make scrolling and drawing slower, so \
     choose the smallest size that covers what you need.\n\n\
     The buffer limits only what the view keeps. If a Raw recording is running, it \
     holds every byte received, whatever the buffer size.",
);

impl ListenerApp {
    /// The stream viewer (§41): the toolbar (Pause/Resume, View mode, ctrl-chars,
    /// font/colors/mono) above a virtualized, soft-wrapped byte view. Split out of
    /// `show_detail` — this is the densest layout in the pane (scrollbar styling,
    /// selectable-label visuals, row-pitch/virtualization), so it lives on its own.
    pub(super) fn show_stream_view(&mut self, ui: &mut egui::Ui, id: ChannelId) {
        // Per-channel view settings (mode, ctrl-chars, font, colors) live on the
        // channel's ViewPrefs — each channel renders independently. Edit a clone, then
        // (if it changed) write it back, fold it into the channel's config, and tell the
        // runtime so a profile save captures it (SetViewConfig — no restart, §78).
        let Some(mut prefs) = self.state.channel(id).map(|v| v.view_prefs.clone()) else {
            return;
        };

        let stream_len = self
            .state
            .channel(id)
            .map(|v| v.stream_bytes.len())
            .unwrap_or(0);

        // Header: a "View configuration" dropdown holding the controls, with the
        // Pause/Resume button kept on the title row whether the dropdown is open or
        // closed. A plain CollapsingHeader lays its body *and* the sibling button in
        // document order, so opening it pushed the button down onto the first control
        // row; CollapsingState lets us render the title row (title + button) ourselves
        // and the body separately, so the button stays put.
        let display_paused = self
            .state
            .channel(id)
            .and_then(|v| v.snapshot.as_ref())
            .and_then(|s| s.display_paused);
        let header_id = ui.make_persistent_id(("view_config", id));
        egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            header_id,
            false,
        )
        .show_header(ui, |ui| {
            // The collapse arrow is drawn by show_header; add the title + the
            // Pause/Resume button on the same row.
            ui.label("Configure display");
            if let Some(is_paused) = display_paused {
                if is_paused {
                    if ui.button("Resume").clicked() {
                        self.send(UiCommand::ResumeDisplay(id));
                        // Jump the view back onto the newest bytes: pausing is
                        // for scrolling back, which disengaged stick_to_bottom.
                        self.resume_scroll_bottom = Some(id);
                    }
                    ui.label("view paused — reception continues");
                } else if ui.button("Pause").on_hover(&PAUSE_HOVER).clicked() {
                    self.send(UiCommand::PauseDisplay(id));
                }
            }
        })
        .body(|ui| {
            self.show_view_controls(ui, &mut prefs, stream_len);
            // Inline Mark timestamps (§50.2) live here — they shape what the view
            // (and `.disp`) show. They edit the draft config; committing goes
            // through Apply & Restart (config_needs_restart counts match_rules).
            ui.separator();
            if let Some((eid, config)) = &mut self.edit_draft {
                if *eid == id {
                    edit_mark_rules(ui, config);
                }
            }
        });

        // Persist any edit: update this channel's prefs + config and sync the runtime
        // (display + scroll-buffer retention; no restart — see SetViewConfig).
        if let Some(view) = self.state.channel_mut(id) {
            if !view.view_prefs.eq_settings(&prefs) {
                view.view_prefs = prefs.clone();
                prefs.apply_to_config(&mut view.config);
                let display = view.config.display.clone();
                let retention = view.config.retention.clone();
                self.send(UiCommand::SetViewConfig(
                    id,
                    Box::new(display),
                    Box::new(retention),
                ));
            }
        }

        let msg_mode = prefs.mode;
        let msg_chars = prefs.chars;
        let font_size = prefs.font_size;
        let mono_family = prefs.mono.family();
        let fg = prefs.colors.fg();
        let bg = prefs.colors.bg();

        let has_snapshot = self
            .state
            .channel(id)
            .map(|v| v.snapshot.is_some())
            .unwrap_or(false);
        // The stream viewer (§41): there is one source — the verbatim byte stream.
        // Hex line length is filled in below, once the pane's width is known, so
        // that every Hex byte wrap happens in the renderer — the only place a
        // cell boundary is known. `split_stream_rows` is still character-based
        // and still wraps the text modes; a Hex *byte* row never reaches it with
        // anything left to cut (see `hex_line_bytes`). A Hex *Mark* row can, and
        // that is deliberate: a Mark wider than the pane is cut as text, which
        // is the one overrun ADR-042 accepts.
        let hex_grouping = prefs.hex_grouping;
        let mut renderer = DisplayView {
            mode: msg_mode,
            encoding: DisplayEncoding::Utf8,
            character_rendering: msg_chars,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_group: usize::from(hex_grouping.bytes_per_group.max(1)),
            hex_bytes_per_line: 0,
        };
        // Verbatim received bytes (§17–18, §41): line breaks come only from the
        // data — Rendered honors real CR/LF (§44), Raw shows control pictures, Hex
        // is a byte run. Serial and UDP render identically (no reframing).
        //
        // Performance (§100): the scrollback can reach the 256 KB scroll cap. The
        // bytes arrive incrementally (StreamDelta) so the driver never re-ships the
        // whole buffer; here we (a) memoize the split rows, re-rendering only when
        // data arrives or the view mode changes — keyed on the stream cursor — and
        // (b) virtualize the layout with `show_rows`, laying out only visible rows.
        // Both matter: a non-virtualized selectable Label over the full buffer
        // stalled the UI.
        let font = egui::FontId::new(font_size, mono_family.clone());
        // Monospace metrics: row height and the width of one glyph ('0' as a stand-in),
        // so we can convert the available pixel width into a column count for wrapping.
        let (row_h, char_w) =
            ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0').max(1.0)));
        // Width of the (non-floating) vertical scrollbar. Used both to size the bar
        // below and to reserve its space when wrapping, so a full row ends just before
        // the bar instead of under it. One const so the two uses can't drift apart.
        const SCROLLBAR_WIDTH: f32 = 12.0;
        // Pin the viewer to exactly the height left in the pane, so the ScrollArea owns
        // the scrolling (and `stick_to_bottom` keeps the newest bytes pinned to the
        // bottom edge) instead of the content overflowing and scrolling the whole pane —
        // which left the latest data stranded below the window fold ("never reaches the
        // bottom"). The Frame's 4px inner margin top+bottom is subtracted.
        let viewer_height = (ui.available_height() - 8.0).max(0.0);
        egui::Frame::new()
            .fill(bg)
            .inner_margin(4.0)
            .show(ui, |ui| {
                // Pre-wrap each cached row to the columns that fit the viewer's inner
                // width (less the scrollbar), so every row is exactly one visual line —
                // uniform height, which `show_rows` needs to virtualize. Re-wraps only
                // when the column count changes (the cache key includes it).
                let avail_w = (ui.available_width() - SCROLLBAR_WIDTH).max(char_w);
                let wrap_cols = (avail_w / char_w).floor().max(8.0) as usize;
                // Resolve Hex line length against the pane now that its width is
                // known, so the renderer wraps at a whole group and the row
                // splitter below never has a Hex byte line left to cut. The
                // column bound that goes with it is attached by `rebuild_rows`,
                // which receives `wrap_cols` anyway and so cannot be given one
                // setting without the other.
                renderer.hex_bytes_per_line = hex_line_bytes(hex_grouping, wrap_cols);
                self.refresh_stream_rows(id, &renderer, wrap_cols);
                let rows: &[String] = self
                    .stream_cache
                    .as_ref()
                    .filter(|c| c.channel == id)
                    .map(|c| c.rows.as_slice())
                    .unwrap_or(&[]);
                // The viewer fills its full height whether or not data is flowing — the
                // ScrollArea (`auto_shrink([false,false])`) reserves the space, so the
                // window doesn't pop open when a channel starts. Empty state shows a
                // weak note *inside* the scroll area rather than collapsing the frame.
                // Text selection across the (non-interactive) row labels.
                ui.style_mut().interaction.selectable_labels = true;
                // Give the scrollbar a visible track + handle, distinct from the text
                // background, so the strip on the right edge reads as the scrollbar (not
                // mysterious empty space). Both contrast with `bg`, the handle more
                // strongly (see `scrollbar_colors`). Set before the per-widget overrides.
                let (track, handle) = scrollbar_colors(bg);
                ui.visuals_mut().extreme_bg_color = track;
                // A solid, constant-width scrollbar. egui's default is "floating" — thin
                // until hovered, which read as the track widening on hover; `floating =
                // false` plus a fixed `bar_width` (and zero margins) pins it to a steady
                // strip the width we reserved above.
                {
                    let s = &mut ui.style_mut().spacing.scroll;
                    s.floating = false;
                    s.bar_width = SCROLLBAR_WIDTH;
                    s.bar_inner_margin = 0.0;
                    s.bar_outer_margin = 0.0;
                }
                // Suppress the per-widget hover/active visuals egui paints on selectable
                // labels (they drew a flickering box around the rows near the pointer).
                {
                    let w = &mut ui.visuals_mut().widgets;
                    for s in [
                        &mut w.hovered,
                        &mut w.active,
                        &mut w.inactive,
                        &mut w.noninteractive,
                    ] {
                        s.bg_stroke = egui::Stroke::NONE;
                        s.weak_bg_fill = egui::Color32::TRANSPARENT;
                        s.bg_fill = egui::Color32::TRANSPARENT;
                    }
                }
                // The scroll handle is drawn from the *idle* widget visuals (egui maps an
                // un-hovered scrollbar to `widgets.inactive`), using `bg_fill` when
                // `scroll_style.foreground_color` is false (the default) or `fg_stroke`
                // when true. We just cleared those fills for the labels, which is why the
                // handle vanished. Set `foreground_color = true` and drive the handle via
                // `fg_stroke.color` on every interaction state — explicit and immune to
                // the bg_fill clearing above. Hover/drag brighten so it stays visible on
                // a dark scheme.
                ui.style_mut().spacing.scroll.foreground_color = true;
                {
                    let w = &mut ui.visuals_mut().widgets;
                    w.inactive.fg_stroke.color = handle;
                    w.noninteractive.fg_stroke.color = handle;
                    w.hovered.fg_stroke.color = handle.gamma_multiply(1.2);
                    w.active.fg_stroke.color = handle.gamma_multiply(1.4);
                }
                // Zero the inter-row spacing *here*, before `show_rows`, so the row pitch
                // it uses to size the virtual content (and thus where `stick_to_bottom`
                // scrolls) matches what the rows actually render at. Setting it only
                // inside the closure left `show_rows` reserving `row_h + default_spacing`
                // per row while rows drew at `row_h`, so the computed bottom overshot the
                // real last row and the newest data never came into view.
                ui.spacing_mut().item_spacing.y = 0.0;
                // Resume's one-shot jump to the newest bytes: force the offset
                // to the exact virtual content height (rows × row pitch — the
                // spacing zeroed above is what makes this arithmetic hold;
                // egui clamps overshoot). With the view at the bottom,
                // `stick_to_bottom` re-latches and follows new data on its own.
                let jump_to_bottom = self
                    .resume_scroll_bottom
                    .take_if(|channel| *channel == id)
                    .is_some();
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt("stream")
                    .stick_to_bottom(true)
                    .max_height(viewer_height)
                    .auto_shrink([false, false]);
                if jump_to_bottom {
                    scroll = scroll.vertical_scroll_offset(rows.len() as f32 * row_h);
                }
                if stream_len == 0 {
                    // No data yet: still occupy the full viewer height, with a note.
                    scroll.show(ui, |ui| {
                        let note = if has_snapshot {
                            "no data received yet"
                        } else {
                            "waiting for data — Start the channel"
                        };
                        ui.label(egui::RichText::new(note).weak());
                    });
                    return;
                }
                // Soft-wrapped AND virtualized: rows are pre-wrapped to `wrap_cols`
                // (above) so each is one uniform-height visual line, which lets
                // `show_rows` lay out only the visible rows. This is what keeps the UI
                // responsive — the earlier "render everything" approaches (N labels or
                // one giant galley) re-laid-out the whole buffer every frame and made
                // resizing/the whole UI sluggish. Rows don't wrap again here (they're
                // already wrapped); they just extend if anything slipped through.
                scroll.show_rows(ui, row_h, rows.len().max(1), |ui, range| {
                    for row in &rows[range] {
                        ui.add(
                            egui::Label::new(egui::RichText::new(row).font(font.clone()).color(fg))
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                });
            });
    }

    /// The view-configuration controls (inside the "View configuration" dropdown):
    /// View mode + ctrl-chars on one row, font size / colors / mono on the next, then
    /// the scroll-buffer size readout. Edits the passed-in [`ViewPrefs`] (a clone of
    /// the selected channel's); the caller persists any change. `stream_len` is the
    /// channel's current scrollback byte count. Takes `&self` — touches no app state
    /// beyond rendering.
    fn show_view_controls(&self, ui: &mut egui::Ui, prefs: &mut ViewPrefs, stream_len: usize) {
        let base = egui::TextStyle::Body.resolve(ui.style()).size;
        ui.horizontal(|ui| {
            ui.label("View")
                .on_hover_text("How the received bytes are displayed in the viewer.");
            ui.radio_value(&mut prefs.mode, DisplayMode::Hex, "Hex")
                .on_hover_text("Each byte as two-digit hex (e.g. 0A 0D 41).");
            ui.radio_value(&mut prefs.mode, DisplayMode::Rendered, "Rendered")
                .on_hover_text("As text, where line break bytes (CR and LF) start a new line.");
            ui.radio_value(&mut prefs.mode, DisplayMode::Raw, "Raw")
                .on_hover_text(
                    "As text, but control characters are shown as the marks chosen under \
                     ctrl-chars instead of moving the text around.",
                );
            ui.separator();
            // Control-character rendering (§46) — enabled only in Raw mode. The
            // oversized ␊ glyph makes the row taller than the text, so the whole
            // ctrl-chars cluster is bottom-aligned to sit on the View radios' baseline.
            ui.add_enabled_ui(prefs.mode == DisplayMode::Raw, |ui| {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("ctrl-chars");
                        ui.radio_value(
                            &mut prefs.chars,
                            CharacterRendering::Glyph,
                            egui::RichText::new("␊").size(base * 1.6),
                        )
                        .on_hover_text("Control pictures (␊ ␍ ␉ …)");
                        ui.radio_value(&mut prefs.chars, CharacterRendering::Token, "[LF]")
                            .on_hover_text("Bracketed names ([LF] [CR] [TAB] …)");
                        ui.radio_value(&mut prefs.chars, CharacterRendering::HexEscape, "<0A>")
                            .on_hover_text("Hex escapes (<0A> <0D> <09> …)");
                    });
                });
            });
        });
        // Hex grouping (§45) — enabled only in Hex mode, but the setting persists
        // either way, so switching modes and back does not lose it.
        ui.add_enabled_ui(prefs.mode == DisplayMode::Hex, |ui| {
            ui.horizontal(|ui| {
                ui.label("Group").on_hover(&GROUP_HOVER);
                egui::ComboBox::from_id_salt("hex_bytes_per_group")
                    .selected_text(format!(
                        "{} bytes",
                        prefs.hex_grouping.bytes_per_group.max(1)
                    ))
                    .show_ui(ui, |ui| {
                        for &size in HEX_GROUP_SIZES {
                            ui.selectable_value(
                                &mut prefs.hex_grouping.bytes_per_group,
                                size,
                                format!("{size} bytes"),
                            );
                        }
                    });
                ui.label("Line").on_hover(&LINE_HOVER);
                egui::ComboBox::from_id_salt("hex_groups_per_line")
                    .selected_text(hex_line_label(prefs.hex_grouping))
                    .show_ui(ui, |ui| {
                        for &groups in HEX_LINE_GROUPS {
                            let candidate = HexGrouping {
                                groups_per_line: groups,
                                ..prefs.hex_grouping
                            };
                            ui.selectable_value(
                                &mut prefs.hex_grouping.groups_per_line,
                                groups,
                                hex_line_label(candidate),
                            );
                        }
                    });
            });
        });
        ui.horizontal(|ui| {
            ui.label("Size");
            // An editable "combo": type any size into the field, or pick a preset from
            // the ▾ menu; the text is the source of truth while editing.
            let resp = ui.add(egui::TextEdit::singleline(&mut prefs.font_text).desired_width(40.0));
            if resp.changed() {
                if let Ok(v) = prefs.font_text.trim().parse::<f32>() {
                    prefs.font_size = v.clamp(6.0, 72.0);
                }
            }
            ui.menu_button("\u{25BC}", |ui| {
                for &size in MSG_FONT_SIZES {
                    if ui.button(format!("{size:.0}")).clicked() {
                        prefs.font_size = size;
                        prefs.font_text = format!("{size:.0}");
                        ui.close();
                    }
                }
            });
            ui.separator();
            ui.label("Colors");
            egui::ComboBox::from_id_salt("msg_colors")
                .selected_text(prefs.colors.label())
                .show_ui(ui, |ui| {
                    for scheme in [
                        ColorScheme::BlackOnWhite,
                        ColorScheme::GreenOnBlack,
                        ColorScheme::GreenOnBlackDim,
                        ColorScheme::AmberOnBlack,
                        ColorScheme::WhiteOnBlack,
                    ] {
                        ui.selectable_value(&mut prefs.colors, scheme, scheme.label());
                    }
                });
            ui.separator();
            ui.label("Mono");
            egui::ComboBox::from_id_salt("msg_font")
                .selected_text(prefs.mono.label())
                .show_ui(ui, |ui| {
                    for &font in MonoFont::ALL {
                        ui.selectable_value(&mut prefs.mono, font, font.label());
                    }
                });
        });
        // Scroll-buffer cap (§87): how far back the viewer scrolls, chosen from presets
        // (2 … 256 kB) — no free text, so it's always a known value. The current fill is
        // shown alongside. Persisted via retention (the runtime adopts it next start; the
        // GUI viewer caps live).
        ui.horizontal(|ui| {
            ui.label("Scroll buffer").on_hover(&SCROLL_BUFFER_HOVER);
            egui::ComboBox::from_id_salt("scroll_buffer")
                .selected_text(scroll_buffer_label(prefs.scroll_buffer_bytes))
                // Tall enough for every preset: the default popup max height sat
                // right at the content height, so its (floating) scrollbar
                // flashed in and out on hover. With room to spare the popup
                // never scrolls and no scrollbar can appear.
                .height(280.0)
                .show_ui(ui, |ui| {
                    for &kb in SCROLL_BUFFER_PRESETS_KB {
                        let bytes =
                            (kb * 1024).clamp(MIN_SCROLL_BUFFER_BYTES, MAX_SCROLL_BUFFER_BYTES);
                        ui.selectable_value(
                            &mut prefs.scroll_buffer_bytes,
                            bytes,
                            scroll_buffer_label(bytes),
                        );
                    }
                })
                .response
                .on_hover(&SCROLL_BUFFER_HOVER);
            ui.separator();
            // The current fill (how much is buffered right now, ≤ the cap).
            ui.label(
                egui::RichText::new(format!("now: {}", human_bytes(stream_len as u64))).weak(),
            )
            .on_hover_text("The bytes in the scroll buffer right now, never more than its size.");
        });
    }

    /// Refresh the incrementally maintained stream-view rows for `id` (see
    /// [`super::super::StreamRenderCache`]): render only the bytes new since
    /// the last refresh, rebuilding from scratch only on a settings / marks /
    /// reset change. No-ops entirely when nothing changed.
    fn refresh_stream_rows(&mut self, id: ChannelId, renderer: &DisplayView, wrap_cols: usize) {
        let Some(view) = self.state.channel_mut(id) else {
            self.stream_cache = None;
            return;
        };
        // Make the window contiguous *first*, then take an immutable slice via
        // `as_slices` — `stream_contiguous()`'s returned slice would hold the
        // mutable borrow and conflict with reading `view.marks` alongside it.
        view.stream_bytes.make_contiguous();
        let (window, rest) = view.stream_bytes.as_slices();
        debug_assert!(rest.is_empty(), "make_contiguous left a split deque");
        refresh_rows(
            &mut self.stream_cache,
            StreamRefresh {
                channel: id,
                window,
                window_start: view.stream_base_offset,
                cursor: view.stream_cursor,
                marks: &view.marks,
                marks_version: view.marks_version,
                view: renderer,
                wrap_cols,
            },
        );
    }
}

/// Narrowest row width the viewer will lay out to, in monospace columns.
///
/// Shared by the row splitter and the Hex line-length resolver: if the two used
/// different floors, a very narrow pane would let the renderer emit a line the
/// splitter then cut, which is exactly the case Hex wrapping exists to avoid.
const MIN_WRAP_COLS: usize = 8;

/// Bytes per rendered Hex line, resolved against the pane (§45).
///
/// Fit-to-width has to become a *byte* count before rendering, never a
/// character count after it. [`split_stream_rows`] cuts at a raw column, and on
/// a Hex run that column can land between the two digits of one byte or strand
/// a group separator at the head of the next row. Resolving the width here puts
/// every Hex wrap in the renderer, which is the only place a cell boundary is
/// known.
///
/// `G` groups of `B` bytes occupy `G × B × 2` digits plus the `G - 1`
/// separators between them, so the largest `G` fitting `cols` columns is
/// `(cols + 1) / (2B + 1)`.
///
/// A configured group count is **clamped** to what fits rather than allowed to
/// overflow. Overflow would hand the excess back to the character splitter and
/// reintroduce the split byte, so the choice is between showing fewer groups
/// than asked and showing broken ones.
///
/// When not even one whole group fits, the fallback is whole *bytes* — not one
/// group anyway. Returning an oversized line is what re-engages the splitter,
/// which was the original defect; a byte is the smallest unit that can be
/// wrapped without lying about the data.
fn hex_line_bytes(grouping: HexGrouping, wrap_cols: usize) -> usize {
    // Share the splitter's own floor, so the width the renderer wraps to and
    // the width the splitter would cut at can never disagree.
    let cols = wrap_cols.max(MIN_WRAP_COLS);
    let bytes_per_group = usize::from(grouping.bytes_per_group.max(1));
    // A unit's footprint: its digits plus the separator that follows it.
    let stride = bytes_per_group * 2 + 1;
    let fitting_groups = (cols + 1) / stride;
    if fitting_groups == 0 {
        return ((cols + 1) / 3).max(1);
    }
    let groups = match grouping.groups_per_line {
        0 => fitting_groups,
        requested => usize::from(requested).min(fitting_groups),
    };
    groups * bytes_per_group
}

/// Inputs for one refresh of the incremental row cache.
struct StreamRefresh<'a> {
    channel: ChannelId,
    /// The accumulated byte window, contiguous. Its first byte sits at absolute
    /// stream offset `window_start`.
    window: &'a [u8],
    /// Absolute stream offset of `window[0]`, maintained alongside the byte deque.
    window_start: u64,
    /// Absolute stream offset one past the window's last byte.
    cursor: u64,
    /// The channel's pinned inline-Mark timestamps (§50.2), **offset-sorted**
    /// (view space) — the renderer's forward-only walker requires sorted input.
    marks: &'a [crate::gui::state::StreamMark],
    /// The channel's mark-list change counter (see `ChannelView::marks_version`);
    /// gates the history-signature hash to frames where marks actually changed.
    marks_version: u64,
    view: &'a DisplayView,
    wrap_cols: usize,
}

/// Hard cap on trim-accounting batches — a backstop only. The real bound
/// comes from byte-quantum coalescing in `append_text`: batches absorb
/// appends until they span [`row_batch_quantum`] bytes and then their end
/// offset **freezes**, so the advancing window inevitably passes every batch
/// and `trim_evicted` reclaims it. (Merging on *count* alone was a leak: each
/// merge advanced the oldest batch's end in lockstep with the window start,
/// so with more than this many deltas per retained window the front batch
/// never became evictable and rows grew for the channel's lifetime.)
const MAX_ROW_BATCHES: usize = 512;

/// Byte span a trim-accounting batch grows to before its end freezes: the
/// row cache lags eviction by at most this many bytes' worth of rows, and
/// the batch count stays ≤ window/quantum + O(1) (≈128, under the backstop).
fn row_batch_quantum(window_len: usize) -> u64 {
    (window_len as u64 / 128).max(256)
}

/// Refresh `cache` from `p`: incremental append when only new bytes arrived,
/// full rebuild when a setting / the mark history / the stream base changed.
fn refresh_rows(cache: &mut Option<super::super::StreamRenderCache>, p: StreamRefresh) {
    let window_start = p.window_start;
    let needs_rebuild = match cache.as_ref() {
        None => true,
        Some(c) => {
            c.channel != p.channel
                || c.mode != p.view.mode
                || c.chars != p.view.character_rendering
                || c.wrap_cols != p.wrap_cols
                // Grouping changes the text of rows already rendered, so it
                // cannot ride the append path the way new bytes do.
                || c.hex_bytes_per_group != p.view.hex_bytes_per_group
                || c.hex_bytes_per_line != p.view.hex_bytes_per_line
                // The stream restarted / reset behind us…
                || p.cursor < c.rendered_cursor
                // …or eviction ran past the rendered point (a gap we can't append over).
                // A mark appeared for (or was pruned from) an already-rendered
                // offset. The version gate keeps the common idle frame from
                // re-hashing the whole mark history: the signature is only
                // recomputed when the channel's mark list actually changed.
                || window_start > c.rendered_cursor
                || (c.marks_version != p.marks_version
                    && marks_signature_below(p.marks, c.rendered_cursor) != c.history_marks_sig)
        }
    };
    if needs_rebuild {
        *cache = Some(rebuild_rows(&p, window_start));
        return;
    }
    let c = cache.as_mut().expect("checked Some above");
    // The version moved but the history signature didn't (the change was at or
    // past the rendered cursor — it rides the incremental path): adopt the new
    // version so later idle frames skip the hash again.
    c.marks_version = p.marks_version;
    if p.cursor == c.rendered_cursor {
        return; // nothing new — the common per-frame case
    }
    // Append path: render only the suffix past the rendered cursor, through the
    // persistent renderer (so carry / tab column / hex separators continue
    // exactly as a one-shot render would — ADR-018 chunking invariance).
    let from = (c.rendered_cursor - window_start) as usize;
    let delta = &p.window[from..];
    let annotations = delta_annotations(p.marks, c.rendered_cursor, delta.len());
    let text = c.renderer.render_chunk(delta, &annotations);
    c.append_text(&text, p.cursor, row_batch_quantum(p.window.len()));
    c.rendered_cursor = p.cursor;
    c.history_marks_sig = marks_signature_below(p.marks, p.cursor);
    c.trim_evicted(window_start);
}

/// Build the cache fresh: render the whole current window in one chunk (the
/// renderer keeps its state, so subsequent appends continue seamlessly).
fn rebuild_rows(p: &StreamRefresh, window_start: u64) -> super::super::StreamRenderCache {
    // Rebase each pinned mark's view-space offset onto the window and splice
    // its text before/after the annotated byte — exactly as the Display
    // Recording does. The source is the channel's persistent `marks` (folded
    // from snapshot firings, lifetime tied to the bytes), NOT the snapshot's
    // bounded rolling `matches` window — deriving from that made timestamps
    // vanish from a paused view as firings churned.
    //
    // The bound is half-open, matching `delta_annotations`. A mark at exactly
    // one past the window's end is a `Before` whose byte has not arrived yet —
    // including it rendered the mark ahead of its byte and then spliced it a
    // second time when that byte turned up in the next delta, so a Mark landing
    // on a delta boundary appeared twice. (`After` marks anchor on their own
    // byte, so they are always inside the window once that byte is.)
    let annotations: Vec<RenderAnnotation> = p
        .marks
        .iter()
        .filter_map(|m| {
            let within = m.offset.checked_sub(window_start)? as usize;
            (within < p.window.len()).then(|| RenderAnnotation {
                offset: within,
                placement: if m.before {
                    AnnotationPlacement::Before
                } else {
                    AnnotationPlacement::After
                },
                text: m.text.clone(),
            })
        })
        .collect();
    // The pane width belongs to the renderer, and it is attached here rather
    // than by the caller so the two cannot come apart. Since ADR-042 put Marks
    // on their own rows the column bound is a byte-row safety invariant — the
    // renderer's own guarantee that it never returns a row wider than the pane —
    // rather than the thing standing between a Mark and a split byte. It is
    // still attached here because `wrap_cols` arrives with the view, so there is
    // nothing for a caller to forget.
    let mut view = p.view.clone();
    view.wrap_width = Some(p.wrap_cols);
    let mut renderer = StreamRenderer::new(view);
    let text = renderer.render_chunk(p.window, &annotations);
    let rows = split_stream_rows(&text, p.wrap_cols);
    let row_count = rows.len();
    super::super::StreamRenderCache {
        channel: p.channel,
        mode: p.view.mode,
        chars: p.view.character_rendering,
        wrap_cols: p.wrap_cols,
        hex_bytes_per_group: p.view.hex_bytes_per_group,
        hex_bytes_per_line: p.view.hex_bytes_per_line,
        history_marks_sig: marks_signature_below(p.marks, p.cursor),
        marks_version: p.marks_version,
        rendered_cursor: p.cursor,
        renderer,
        rows,
        row_batches: std::collections::VecDeque::from([(p.cursor, row_count)]),
    }
}

/// The marks that fall inside a delta starting at absolute `delta_start`,
/// rebased to chunk-relative offsets for [`StreamRenderer::render_chunk`]. A
/// mark exactly at the delta's end is left for the next delta (where it lands
/// at relative offset 0) — the renderer would otherwise splice an `After`
/// mark ahead of its not-yet-arrived byte.
fn delta_annotations(
    marks: &[crate::gui::state::StreamMark],
    delta_start: u64,
    delta_len: usize,
) -> Vec<RenderAnnotation> {
    marks
        .iter()
        .filter_map(|m| {
            let within = m.offset.checked_sub(delta_start)? as usize;
            (within < delta_len).then(|| RenderAnnotation {
                offset: within,
                placement: if m.before {
                    AnnotationPlacement::Before
                } else {
                    AnnotationPlacement::After
                },
                text: m.text.clone(),
            })
        })
        .collect()
}

impl super::super::StreamRenderCache {
    /// Append newly rendered `text`, re-splitting only the open last row (the
    /// text after the stream's last `\n`, or its last partial wrap chunk) plus
    /// the new text — O(delta + one row), never O(buffer).
    ///
    /// `split_stream_rows` guarantees the invariant this leans on: the last
    /// row is always the open line's most recent wrap chunk (an empty row
    /// when the text ends in `\n`), and wrap-chunk boundaries fall at fixed
    /// multiples of the column count — so popping the open row and
    /// re-splitting `open + new` yields exactly what a one-shot split of the
    /// whole text would.
    fn append_text(&mut self, text: &str, end_offset: u64, quantum: u64) {
        let seed = match self.rows.pop() {
            Some(open) => {
                // The open row was owned by the previous batch — debit it so
                // batch ownership keeps summing to rows.len().
                if let Some((_, owned)) = self.row_batches.back_mut() {
                    *owned = owned.saturating_sub(1);
                }
                open
            }
            None => String::new(),
        };
        let mut combined = seed;
        combined.push_str(text);
        let before = self.rows.len();
        self.rows
            .extend(split_stream_rows(&combined, self.wrap_cols));
        let added = self.rows.len() - before;
        // Coalesce into the back batch while it spans under `quantum` bytes;
        // at the quantum its end freezes, which is what guarantees eviction
        // eventually reclaims it (see `MAX_ROW_BATCHES`). Batches are
        // contiguous, so the back batch's start is its predecessor's end; a
        // lone batch (fresh rebuild) has no known start — freeze it.
        let back_span_under_quantum = match self.row_batches.len() {
            0 | 1 => false,
            len => {
                let prev_end = self.row_batches[len - 2].0;
                self.row_batches[len - 1].0.saturating_sub(prev_end) < quantum
            }
        };
        if back_span_under_quantum {
            let back = self.row_batches.back_mut().expect("len ≥ 2 above");
            back.0 = end_offset;
            back.1 += added;
        } else {
            self.row_batches.push_back((end_offset, added));
        }
        // Backstop only — unreachable with quantum coalescing (≈128 batches
        // per window), kept so a logic slip degrades to lazy trimming
        // instead of unbounded bookkeeping.
        while self.row_batches.len() > MAX_ROW_BATCHES {
            let (_, n1) = self.row_batches.pop_front().expect("len checked");
            let (e2, n2) = self.row_batches.pop_front().expect("len > 1");
            self.row_batches.push_front((e2, n1 + n2));
        }
    }

    /// Drop front rows whose bytes have been evicted from the window: whole
    /// batches only, so the cost is O(evicted rows) amortized. A batch's seam
    /// row can contain a few bytes of its successor; trimming it with the
    /// batch discards those a hair early — invisible in a bottom-anchored
    /// view (the window's own front eviction already cuts mid-line).
    fn trim_evicted(&mut self, window_start: u64) {
        while let Some(&(end, n)) = self.row_batches.front() {
            if window_start >= end && self.row_batches.len() > 1 {
                self.rows.drain(..n.min(self.rows.len()));
                self.row_batches.pop_front();
            } else {
                break;
            }
        }
    }
}

/// A cheap order-sensitive hash of the pinned marks **below** an absolute
/// offset — the already-rendered history. The rows cache invalidates (full
/// rebuild) when this changes even though the raw bytes didn't: a late mark
/// arriving for an already-rendered byte, or front-pruning of the mark list.
/// Marks are offset-sorted, so the prefix is found by partition point; no
/// per-frame allocation.
fn marks_signature_below(marks: &[crate::gui::state::StreamMark], below: u64) -> u64 {
    let end = marks.partition_point(|m| m.offset < below);
    marks_signature(&marks[..end])
}

/// A cheap order-sensitive hash of a mark list.
fn marks_signature(marks: &[crate::gui::state::StreamMark]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    marks.len().hash(&mut h);
    for m in marks {
        m.offset.hash(&mut h);
        m.before.hash(&mut h);
        m.text.hash(&mut h);
    }
    h.finish()
}

/// Scrollbar (track, handle) colors for a viewer whose text background is `bg`. Both
/// are nudged off `bg` toward its opposite end so the scrollbar reads as a distinct
/// strip against the text background — the track subtly, the handle more strongly —
/// and the choice works for both a light (black-on-white) and dark (white-on-black)
/// scheme. Returns colors, not a mutation, so the caller controls when they apply.
fn scrollbar_colors(bg: egui::Color32) -> (egui::Color32, egui::Color32) {
    // Perceived lightness of the background; pick the contrast direction from it.
    let light = (bg.r() as u32 + bg.g() as u32 + bg.b() as u32) / 3 > 128;
    if light {
        // Light bg: a light-grey track, a mid-grey handle.
        (egui::Color32::from_gray(225), egui::Color32::from_gray(150))
    } else {
        // Dark bg: a clearly-lifted track (so it reads against near-black text bg) and
        // a much brighter handle on top of it (gray-95 track vs gray-200 handle reads
        // clearly even when idle).
        (egui::Color32::from_gray(95), egui::Color32::from_gray(200))
    }
}

/// Split rendered stream text into virtualization rows, soft-wrapping each data line
/// to `wrap_cols` monospace columns so every row is exactly one visual line (uniform
/// height — required by `show_rows`). A data line shorter than `wrap_cols` is one row;
/// a longer one (or a line with no LF, e.g. raw binary / UDP) is wrapped into several.
fn split_stream_rows(text: &str, wrap_cols: usize) -> Vec<String> {
    let cols = wrap_cols.max(MIN_WRAP_COLS);
    let mut rows: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        // Wrap by character count (monospace, so columns == chars). Char-based, not
        // byte-based, so multi-byte UTF-8 isn't split mid-codepoint.
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(cols) {
            rows.push(chunk.iter().collect());
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::MatchRuleId;
    use crate::gui::state::StreamMark;

    fn view(mode: DisplayMode) -> DisplayView {
        DisplayView {
            mode,
            encoding: DisplayEncoding::Utf8,
            character_rendering: CharacterRendering::Native,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_group: 1,
            hex_bytes_per_line: 16,
        }
    }

    fn mark(offset: u64, before: bool, text: &str) -> StreamMark {
        StreamMark {
            offset,
            rule_id: MatchRuleId::new(),
            before,
            text: text.to_string(),
        }
    }

    /// Feed `data` to a fresh cache in `chunk`-sized deltas (no eviction) and
    /// return the resulting rows.
    fn incremental_rows(
        data: &[u8],
        chunk: usize,
        marks: &[StreamMark],
        v: &DisplayView,
        cols: usize,
    ) -> Vec<String> {
        let id = ChannelId::new();
        let mut cache = None;
        let mut fed = 0usize;
        while fed < data.len() {
            let end = (fed + chunk).min(data.len());
            refresh_rows(
                &mut cache,
                StreamRefresh {
                    channel: id,
                    window: &data[..end],
                    window_start: 0,
                    cursor: end as u64,
                    marks,
                    marks_version: 0,
                    view: v,
                    wrap_cols: cols,
                },
            );
            fed = end;
        }
        cache.expect("fed at least one delta").rows
    }

    /// Reference: the same input rendered in one shot through a fresh
    /// renderer (identical carry semantics), then split.
    ///
    /// Configured exactly as `rebuild_rows` configures its renderer, pane width
    /// included. A reference that models a *different* renderer would make this
    /// comparison agree by both sides being wrong — which is how a one-shot
    /// render that cut bytes in half once served as the expected value.
    fn batch_rows(data: &[u8], marks: &[StreamMark], v: &DisplayView, cols: usize) -> Vec<String> {
        let annotations = delta_annotations(marks, 0, data.len());
        let mut view = v.clone();
        view.wrap_width = Some(cols);
        let mut r = StreamRenderer::new(view);
        split_stream_rows(&r.render_chunk(data, &annotations), cols)
    }

    #[test]
    fn incremental_rows_equal_one_shot_rows() {
        // The defining invariant of the incremental cache: feeding the stream
        // delta-by-delta produces exactly the rows of a one-shot render+split.
        // Exercises newlines, blank lines, wrap-length lines, multi-byte
        // UTF-8 split across deltas, and all three modes, at several chunk
        // sizes (1 = every boundary possible).
        let data = "alpha\n\nbravo-charlie delta echo\nfoxtrot golf hotel\r\nindia".as_bytes();
        for mode in [DisplayMode::Rendered, DisplayMode::Raw, DisplayMode::Hex] {
            let v = view(mode);
            for cols in [8usize, 10, 80] {
                let want = batch_rows(data, &[], &v, cols);
                for chunk in [1usize, 3, 7, 64] {
                    let got = incremental_rows(data, chunk, &[], &v, cols);
                    assert_eq!(got, want, "mode {mode:?} cols {cols} chunk {chunk}");
                }
            }
        }
    }

    #[test]
    fn split_character_across_deltas_never_shows_a_replacement() {
        // A UTF-8 character split across two deltas is held in the renderer
        // carry, not rendered as U+FFFD-then-fixed.
        let data = "ab \u{e9} cd".as_bytes();
        let v = view(DisplayMode::Rendered);
        let got = incremental_rows(data, 1, &[], &v, 80);
        assert_eq!(got, vec!["ab \u{e9} cd".to_string()]);
    }

    #[test]
    fn hex_separators_continue_across_deltas() {
        let v = view(DisplayMode::Hex);
        let got = incremental_rows(b"ABCD", 2, &[], &v, 80);
        assert_eq!(got, vec!["41 42 43 44".to_string()]);
    }

    /// A group must not break at a read boundary: the transport's chunk sizes
    /// are not a property of the data being read.
    #[test]
    fn hex_groups_survive_delta_boundaries() {
        let mut v = view(DisplayMode::Hex);
        v.hex_bytes_per_group = 4;
        for chunk in [1usize, 2, 3, 5, 8] {
            let got = incremental_rows(b"ABCDEFGH", chunk, &[], &v, 80);
            assert_eq!(
                got,
                vec!["41424344 45464748".to_string()],
                "grouping changed with the read size (chunk {chunk})"
            );
        }
    }

    /// The invariant the whole two-stage wrap exists to hold: a rendered Hex
    /// row never cuts a byte in half, at any pane width or grouping.
    ///
    /// Fit-to-width used to hand the line to the character splitter, which cuts
    /// at a raw column — so a byte could arrive as `4` on one row and `1` on the
    /// next, and a separator could open a row. Every row here must be whole
    /// groups joined by single spaces.
    #[test]
    fn no_pane_width_or_grouping_ever_splits_a_hex_byte() {
        let data: Vec<u8> = (0u8..=255).collect();
        for bytes_per_group in [1u8, 2, 4, 8] {
            // 0 is fit-to-width; the rest must clamp rather than overflow.
            for groups_per_line in [0u8, 1, 4, 16] {
                let grouping = HexGrouping {
                    bytes_per_group,
                    groups_per_line,
                };
                for cols in [8usize, 9, 13, 20, 33, 47, 80, 200] {
                    let mut v = view(DisplayMode::Hex);
                    v.hex_bytes_per_group = usize::from(bytes_per_group);
                    v.hex_bytes_per_line = hex_line_bytes(grouping, cols);
                    let rows = incremental_rows(&data, 7, &[], &v, cols);
                    for row in &rows {
                        let context =
                            format!("B={bytes_per_group} G={groups_per_line} cols={cols}");
                        assert!(
                            !row.starts_with(' ') && !row.ends_with(' '),
                            "row has a stranded separator ({context}): {row:?}"
                        );
                        for group in row.split(' ') {
                            assert!(
                                !group.is_empty() && group.len() % 2 == 0,
                                "row splits a byte ({context}): {row:?}"
                            );
                            assert!(
                                group.chars().all(|c| c.is_ascii_hexdigit()),
                                "row is not whole hex ({context}): {row:?}"
                            );
                        }
                    }
                    // Whole rows still reconstruct the stream exactly.
                    let flat: String = rows.join(" ").split(' ').collect::<Vec<_>>().join("");
                    let expected: String = data.iter().map(|b| format!("{b:02X}")).collect();
                    assert_eq!(flat, expected, "bytes lost or reordered (cols={cols})");
                }
            }
        }
    }

    /// The same invariant with Marks in the line — the case the version above
    /// does *not* cover, because it supplies no annotations.
    ///
    /// A Mark spends one renderer cell but as many columns as its text, so a
    /// line correct by cell count could still overrun the pane and be cut by
    /// the character splitter: at eight columns, one byte per group, a `[T]`
    /// before the second byte produced `41 [T] 42`, which came back as
    /// `41 [T] 4` and `2`. Widths, placements and grouping are crossed here
    /// because the defect only appears where two of them interact.
    #[test]
    fn a_mark_in_the_line_never_pushes_a_hex_byte_across_the_wrap() {
        let data: Vec<u8> = (0x41u8..=0x7A).collect();
        let expected: String = data.iter().map(|b| format!("{b:02X}")).collect();

        for bytes_per_group in [1u8, 2, 4] {
            for groups_per_line in [0u8, 1, 4] {
                let grouping = HexGrouping {
                    bytes_per_group,
                    groups_per_line,
                };
                for cols in [8usize, 9, 13, 20, 47] {
                    for text in ["[T]", "‹12:00:00.000›", "[AB]"] {
                        for before in [true, false] {
                            // Marks early, mid-group and on a group boundary.
                            let marks: Vec<StreamMark> = [1u64, 4, 8, 17]
                                .iter()
                                .map(|&o| mark(o, before, text))
                                .collect();
                            let mut v = view(DisplayMode::Hex);
                            v.hex_bytes_per_group = usize::from(bytes_per_group);
                            v.hex_bytes_per_line = hex_line_bytes(grouping, cols);
                            let rows = incremental_rows(&data, 5, &marks, &v, cols);
                            let context = format!(
                                "B={bytes_per_group} G={groups_per_line} cols={cols} \
                                 text={text:?} before={before}"
                            );

                            let mut seen = String::new();
                            for row in &rows {
                                // A Mark row, or a fragment of one too wide for
                                // the pane — that overrun cuts text, which is
                                // the one this design accepts. Byte rows are
                                // hex digits and single spaces and nothing else,
                                // so anything carrying another character is not
                                // one.
                                if !is_byte_row(row) {
                                    continue;
                                }
                                assert!(
                                    row.chars().count() <= cols,
                                    "byte row overruns the pane ({context}): {row:?}"
                                );
                                for token in row.split(' ') {
                                    // Since Marks take a row of their own, a
                                    // byte row is *only* bytes — a stronger
                                    // claim than "its hex tokens are whole".
                                    assert!(
                                        !token.is_empty()
                                            && token.len() % 2 == 0
                                            && is_hex_token(token),
                                        "a byte row is not whole bytes ({context}): {row:?}"
                                    );
                                    seen.push_str(token);
                                }
                            }
                            // Nothing was lost or reordered around the Marks.
                            assert_eq!(seen, expected, "bytes disturbed by a Mark ({context})");
                        }
                    }
                }
            }
        }
    }

    /// A token is byte data rather than Mark text. Marks here are bracketed or
    /// guillemet-wrapped, so anything all-hex is data — and a Mark that *were*
    /// all hex digits would only make the assertion above stricter.
    fn is_hex_token(token: &str) -> bool {
        token.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// A row of bytes rather than a Mark row: hex digits and separators only.
    fn is_byte_row(row: &str) -> bool {
        !row.is_empty() && row.chars().all(|c| c.is_ascii_hexdigit() || c == ' ')
    }

    #[test]
    fn hex_line_bytes_fills_the_pane_and_clamps_what_cannot_fit() {
        let fit = |b: u8, cols: usize| {
            hex_line_bytes(
                HexGrouping {
                    bytes_per_group: b,
                    groups_per_line: 0,
                },
                cols,
            )
        };
        // Single bytes: "41 42 43" is 3 chars per group less the final space.
        assert_eq!(fit(1, 8), 3, "8 columns hold 3 single-byte groups");
        assert_eq!(fit(1, 47), 16, "the classic 16-byte hexdump line");
        // Four-byte groups occupy 9 columns each.
        assert_eq!(fit(4, 35), 16, "4 groups of 4");
        assert_eq!(fit(4, 8), 4, "a pane too narrow still renders one group");

        // A configured count is honored when it fits and clamped when it does not.
        let fixed = |b: u8, g: u8, cols: usize| {
            hex_line_bytes(
                HexGrouping {
                    bytes_per_group: b,
                    groups_per_line: g,
                },
                cols,
            )
        };
        assert_eq!(fixed(4, 2, 80), 8, "asked for 2 groups, pane is wide");
        assert_eq!(
            fixed(4, 16, 35),
            16,
            "asked for 16 groups in a pane holding 4 — clamped, not overflowed"
        );
    }

    /// A fixed line length wraps where a cell ends, so no separator is stranded
    /// at the head of the next line — the reason the renderer owns this and the
    /// row splitter does not.
    #[test]
    fn hex_lines_wrap_between_groups_not_inside_them() {
        let mut v = view(DisplayMode::Hex);
        v.hex_bytes_per_group = 4;
        v.hex_bytes_per_line = 8; // two groups of four
        for chunk in [1usize, 3, 5, 12] {
            let got = incremental_rows(b"ABCDEFGHIJKL", chunk, &[], &v, 80);
            assert_eq!(
                got,
                vec!["41424344 45464748".to_string(), "494A4B4C".to_string()],
                "chunk {chunk}"
            );
        }
    }

    /// A Mark takes a row of its own, and the bytes after it start a fresh
    /// group — otherwise one annotation shifts every column below it.
    #[test]
    fn hex_annotations_take_a_row_of_their_own() {
        let mut v = view(DisplayMode::Hex);
        v.hex_bytes_per_group = 4;
        let marks = vec![mark(2, true, "[T]")];
        let got = incremental_rows(b"ABCDEF", 2, &[], &v, 80);
        assert_eq!(got, vec!["41424344 4546".to_string()], "no mark, no break");
        // The byte run breaks at the marked byte, so the row above is short and
        // the mark below explains why.
        let got = incremental_rows(b"ABCDEF", 3, &marks, &v, 80);
        assert_eq!(
            got,
            vec![
                "4142".to_string(),
                "[T]".to_string(),
                "43444546".to_string()
            ]
        );
    }

    /// A Mark whose byte lands exactly on a delta boundary is spliced once.
    ///
    /// The rebuild used to include a mark at one *past* the window end — a
    /// `Before` whose byte had not arrived — so it rendered ahead of its byte
    /// and again when that byte turned up. Every mode is checked because the
    /// splice point is shared, and the read size must not be observable.
    #[test]
    fn a_mark_on_a_delta_boundary_splices_once() {
        let marks = vec![mark(2, true, "[T]")];
        for mode in [DisplayMode::Rendered, DisplayMode::Raw, DisplayMode::Hex] {
            let v = view(mode);
            let want = batch_rows(b"ABCDEF", &marks, &v, 80);
            for chunk in [1usize, 2, 3, 6] {
                assert_eq!(
                    incremental_rows(b"ABCDEF", chunk, &marks, &v, 80),
                    want,
                    "mode {mode:?} chunk {chunk}"
                );
            }
        }
    }

    #[test]
    fn marks_for_new_bytes_ride_the_incremental_path() {
        // A mark whose byte arrives in the second delta splices without a
        // rebuild (same rows as one-shot).
        let data = b"hello world";
        let marks = vec![mark(6, true, "[T]")];
        let v = view(DisplayMode::Rendered);
        let got = incremental_rows(data, 4, &marks, &v, 80);
        assert_eq!(got, vec!["hello [T]world".to_string()]);
    }

    #[test]
    fn late_mark_for_rendered_bytes_forces_a_rebuild() {
        let id = ChannelId::new();
        let v = view(DisplayMode::Rendered);
        let mut cache = None;
        let data = b"hello world";
        refresh_rows(
            &mut cache,
            StreamRefresh {
                channel: id,
                window: data,
                window_start: 0,
                cursor: data.len() as u64,
                marks: &[],
                marks_version: 0,
                view: &v,
                wrap_cols: 80,
            },
        );
        assert_eq!(
            cache.as_ref().unwrap().rows,
            vec!["hello world".to_string()]
        );
        // The mark targets offset 0 - long since rendered. The state bumps
        // `marks_version` whenever the mark list changes, which admits the
        // history-signature check; the signature differs, so the cache
        // rebuilds with the splice.
        let marks = vec![mark(0, true, "[LATE]")];
        refresh_rows(
            &mut cache,
            StreamRefresh {
                channel: id,
                window: data,
                window_start: 0,
                cursor: data.len() as u64,
                marks: &marks,
                marks_version: 1,
                view: &v,
                wrap_cols: 80,
            },
        );
        assert_eq!(
            cache.as_ref().unwrap().rows,
            vec!["[LATE]hello world".to_string()]
        );
    }

    #[test]
    fn unchanged_marks_version_skips_the_history_signature_check() {
        // The version gate is the contract: with an unchanged `marks_version`
        // the cache never re-hashes the mark history, so a mark list that
        // mutated *without* a bump goes unnoticed. The state upholds its half
        // (`marks_version_moves_with_the_mark_list_not_the_bytes`); this pins
        // the cache's half — the idle-frame fast path.
        let id = ChannelId::new();
        let v = view(DisplayMode::Rendered);
        let mut cache = None;
        let data = b"hello world";
        let refresh = |cache: &mut _, marks: &[StreamMark], version: u64| {
            refresh_rows(
                cache,
                StreamRefresh {
                    channel: id,
                    window: data,
                    window_start: 0,
                    cursor: data.len() as u64,
                    marks,
                    marks_version: version,
                    view: &v,
                    wrap_cols: 80,
                },
            );
        };
        refresh(&mut cache, &[], 0);
        // Same version: the changed mark history is (by contract) not noticed.
        let marks = vec![mark(0, true, "[LATE]")];
        refresh(&mut cache, &marks, 0);
        assert_eq!(
            cache.as_ref().unwrap().rows,
            vec!["hello world".to_string()]
        );
        // Bumped version: noticed, rebuilt with the splice.
        refresh(&mut cache, &marks, 1);
        assert_eq!(
            cache.as_ref().unwrap().rows,
            vec!["[LATE]hello world".to_string()]
        );
    }

    #[test]
    fn eviction_trims_front_rows_and_keeps_the_tail_exact() {
        // Simulate the state's byte cap: after each delta the window keeps
        // only the last `cap` bytes. Front rows must be trimmed (bounded
        // memory) and the tail rows must match a one-shot render's tail.
        let id = ChannelId::new();
        let v = view(DisplayMode::Rendered);
        let mut cache = None;
        let cap = 32usize;
        let mut all: Vec<u8> = Vec::new();
        for i in 0..40 {
            all.extend_from_slice(format!("line {i:02}\n").as_bytes());
            let start = all.len().saturating_sub(cap);
            refresh_rows(
                &mut cache,
                StreamRefresh {
                    channel: id,
                    window: &all[start..],
                    window_start: start as u64,
                    cursor: all.len() as u64,
                    marks: &[],
                    marks_version: 0,
                    view: &v,
                    wrap_cols: 80,
                },
            );
        }
        let c = cache.unwrap();
        // Bounded: the window holds 4 lines (32 / 8 bytes each); trimming is
        // per-batch (a hair lazy), so allow a small constant slack - the
        // point is it is not ~40 rows.
        assert!(
            c.rows.len() <= 8,
            "front rows not trimmed: {} rows",
            c.rows.len()
        );
        // The tail is exact: the last rows equal the one-shot render's tail.
        let want = batch_rows(&all, &[], &v, 80);
        let tail = 3;
        assert_eq!(
            &c.rows[c.rows.len() - tail..],
            &want[want.len() - tail..],
            "tail rows diverged from one-shot render"
        );
    }

    #[test]
    fn cursor_regression_rebuilds() {
        // A restart resets the stream offset to 0 - the cache must rebuild,
        // not append backwards.
        let id = ChannelId::new();
        let v = view(DisplayMode::Rendered);
        let mut cache = None;
        refresh_rows(
            &mut cache,
            StreamRefresh {
                channel: id,
                window: b"old data",
                window_start: 0,
                cursor: 8,
                marks: &[],
                marks_version: 0,
                view: &v,
                wrap_cols: 80,
            },
        );
        refresh_rows(
            &mut cache,
            StreamRefresh {
                channel: id,
                window: b"new",
                window_start: 0,
                cursor: 3,
                marks: &[],
                marks_version: 0,
                view: &v,
                wrap_cols: 80,
            },
        );
        assert_eq!(cache.unwrap().rows, vec!["new".to_string()]);
    }

    #[test]
    fn wrap_change_rebuilds_with_new_columns() {
        let id = ChannelId::new();
        let v = view(DisplayMode::Rendered);
        let mut cache = None;
        let data = b"abcdefghijklmnop"; // 16 chars, no newline
        for cols in [8usize, 16] {
            refresh_rows(
                &mut cache,
                StreamRefresh {
                    channel: id,
                    window: data,
                    window_start: 0,
                    cursor: data.len() as u64,
                    marks: &[],
                    marks_version: 0,
                    view: &v,
                    wrap_cols: cols,
                },
            );
        }
        assert_eq!(cache.unwrap().rows, vec!["abcdefghijklmnop".to_string()]);
    }

    #[test]
    fn tiny_deltas_with_a_sliding_window_keep_rows_bounded() {
        // Regression: with more deltas per retained window than the batch cap
        // (tiny reads on a capped scrollback — the weeks-long-logging shape),
        // the old count-triggered merge advanced the front batch's end in
        // lockstep with the window start, so no batch ever became evictable
        // and `rows` grew for the channel's lifetime. Quantum coalescing
        // freezes batch ends, so rows must track the window, not the total
        // bytes ever received.
        let id = ChannelId::new();
        let v = view(DisplayMode::Rendered);
        let mut cache = None;
        let cap = 2048usize; // window spans ~2048 one-byte deltas >> MAX_ROW_BATCHES
        let mut all: Vec<u8> = Vec::new();
        for i in 0..6000 {
            all.push(if i % 8 == 7 { b'\n' } else { b'x' });
            let start = all.len().saturating_sub(cap);
            refresh_rows(
                &mut cache,
                StreamRefresh {
                    channel: id,
                    window: &all[start..],
                    window_start: start as u64,
                    cursor: all.len() as u64,
                    marks: &[],
                    marks_version: 0,
                    view: &v,
                    wrap_cols: 80,
                },
            );
        }
        let c = cache.unwrap();
        // The window holds 2048 bytes ≈ 256 nine-byte lines; the cache may
        // lag by roughly a quantum's worth of rows, never by the stream's
        // full 6000-byte history (the old code retained ~660 rows here and
        // kept growing with every further delta).
        assert!(
            c.rows.len() <= 320,
            "rows not bounded by the window: {} rows",
            c.rows.len()
        );
        assert!(c.row_batches.len() <= MAX_ROW_BATCHES);
        // Ownership accounting still covers every row exactly.
        let owned: usize = c.row_batches.iter().map(|&(_, n)| n).sum();
        assert_eq!(owned, c.rows.len());
        // And the tail still equals a one-shot render of the window.
        let want = batch_rows(&all[all.len() - cap..], &[], &v, 80);
        let tail = 3;
        assert_eq!(
            &c.rows[c.rows.len() - tail..],
            &want[want.len() - tail..],
            "tail rows diverged from one-shot render"
        );
    }

    #[test]
    fn batch_bookkeeping_stays_bounded() {
        // Thousands of tiny deltas with no eviction: the trim-accounting
        // queue merges instead of growing without bound.
        let id = ChannelId::new();
        let v = view(DisplayMode::Rendered);
        let mut cache = None;
        let mut all = Vec::new();
        for i in 0..(MAX_ROW_BATCHES * 3) {
            all.push(if i % 10 == 9 { b'\n' } else { b'x' });
            refresh_rows(
                &mut cache,
                StreamRefresh {
                    channel: id,
                    window: &all,
                    window_start: 0,
                    cursor: all.len() as u64,
                    marks: &[],
                    marks_version: 0,
                    view: &v,
                    wrap_cols: 80,
                },
            );
        }
        let c = cache.unwrap();
        assert!(c.row_batches.len() <= MAX_ROW_BATCHES);
        // Ownership accounting must still cover every row exactly.
        let owned: usize = c.row_batches.iter().map(|&(_, n)| n).sum();
        assert_eq!(owned, c.rows.len());
    }
}
