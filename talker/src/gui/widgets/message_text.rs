//! Marker-aware text editing for message payloads (spec §5.3): layout jobs,
//! the bounded editor, its right-click menu and cursor helpers.

use std::sync::Arc;

use crate::core::message::{code_page_encodes, repair_after_edit, segments, CodePage, Segment};

use super::*;

// ── Marker-aware text editing ─────────────────────────────────────────────────

/// `egui::TextBuffer` wrapper around a `&mut String` that **uppercases every
/// character on insert**, so a hex field's value can never momentarily contain
/// a lowercase letter (no one-frame flash between keystroke and post-hoc
/// `to_ascii_uppercase`). Used for the message editor's Hex data field.
pub(in crate::gui) struct UppercaseHex<'a>(pub(in crate::gui) &'a mut String);

impl egui::TextBuffer for UppercaseHex<'_> {
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
    fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        let upper = text.to_ascii_uppercase();
        let byte_idx = self
            .0
            .char_indices()
            .nth(char_index)
            .map_or(self.0.len(), |(i, _)| i);
        self.0.insert_str(byte_idx, &upper);
        upper.chars().count()
    }
    fn delete_char_range(&mut self, char_range: std::ops::Range<usize>) {
        let start = self
            .0
            .char_indices()
            .nth(char_range.start)
            .map_or(self.0.len(), |(i, _)| i);
        let end = self
            .0
            .char_indices()
            .nth(char_range.end)
            .map_or(self.0.len(), |(i, _)| i);
        self.0.replace_range(start..end, "");
    }
    fn type_id(&self) -> std::any::TypeId {
        // `UppercaseHex<'a>` isn't `'static`, so we can't use `TypeId::of::<Self>()`.
        // Use a `'static` marker — egui only needs *some* stable TypeId.
        struct UppercaseHexMarker;
        std::any::TypeId::of::<UppercaseHexMarker>()
    }
}

/// Build the editor layout, including marker color and optional code-page
/// replacement backgrounds.
fn marker_layout_job(
    ui: &egui::Ui,
    text: &str,
    code_page: Option<CodePage>,
) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let normal = ui.visuals().text_color();
    let marker = egui::Color32::from_rgb(110, 170, 255);
    let (replacement_text, replacement_background) = replacement_highlight_colors(ui);
    let mut job = egui::text::LayoutJob::default();
    // Soft wrapping would make long pasted lines grow vertically and displace
    // unrelated controls. The surrounding two-axis ScrollArea handles long
    // lines, while LayoutJob still honors explicit newline characters.
    job.wrap.max_width = f32::INFINITY;
    for (range, segment) in segments(text) {
        match (segment, code_page) {
            (Segment::Byte(_), _) => {
                job.append(
                    &text[range],
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color: marker,
                        ..Default::default()
                    },
                );
            }
            (Segment::Text, None) => {
                job.append(
                    &text[range],
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color: normal,
                        ..Default::default()
                    },
                );
            }
            (Segment::Text, Some(code_page)) => {
                let chunk = &text[range];
                let mut run_start = 0;
                let mut run_replaced = None;
                for (offset, character) in chunk.char_indices() {
                    let replaced = !matches!(character, '\u{2039}' | '\u{203A}')
                        && !code_page_encodes(character, code_page);
                    if run_replaced.is_some_and(|current| current != replaced) {
                        append_code_page_run(
                            &mut job,
                            &chunk[run_start..offset],
                            run_replaced.unwrap_or(false),
                            &font,
                            normal,
                            replacement_text,
                            replacement_background,
                        );
                        run_start = offset;
                    }
                    run_replaced = Some(replaced);
                }
                append_code_page_run(
                    &mut job,
                    &chunk[run_start..],
                    run_replaced.unwrap_or(false),
                    &font,
                    normal,
                    replacement_text,
                    replacement_background,
                );
            }
        }
    }
    job
}

fn append_code_page_run(
    job: &mut egui::text::LayoutJob,
    text: &str,
    replaced: bool,
    font: &egui::FontId,
    normal: egui::Color32,
    replacement_text: egui::Color32,
    replacement_background: egui::Color32,
) {
    if text.is_empty() {
        return;
    }
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: font.clone(),
            color: if replaced { replacement_text } else { normal },
            background: if replaced {
                replacement_background
            } else {
                egui::Color32::TRANSPARENT
            },
            ..Default::default()
        },
    );
}

#[derive(Clone, Copy)]
enum MessageEditorLayout {
    MarkerAware(Option<CodePage>),
    Plain,
}

fn plain_editor_layout_job(ui: &egui::Ui, text: &str) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: egui::TextStyle::Body.resolve(ui.style()),
            color: ui.visuals().text_color(),
            ..Default::default()
        },
    );
    job
}

/// Lay out one editor value without soft wrapping. The resulting galley is
/// both the source of truth for horizontal extent and the layout TextEdit
/// consumes, avoiding a separate glyph-by-glyph width traversal.
fn message_editor_galley(
    ui: &egui::Ui,
    text: &str,
    layout: MessageEditorLayout,
) -> Arc<egui::Galley> {
    let job = match layout {
        MessageEditorLayout::MarkerAware(code_page) => marker_layout_job(ui, text, code_page),
        MessageEditorLayout::Plain => plain_editor_layout_job(ui, text),
    };
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

/// Supplies the precomputed galley to TextEdit's first layout request. If an
/// input event changes the text, TextEdit asks again and receives a fresh
/// galley for the new value.
struct MessageEditorLayouter {
    layout: MessageEditorLayout,
    initial: Option<Arc<egui::Galley>>,
}

impl MessageEditorLayouter {
    fn new(layout: MessageEditorLayout, initial: Arc<egui::Galley>) -> Self {
        Self {
            layout,
            initial: Some(initial),
        }
    }

    fn layout(&mut self, ui: &egui::Ui, text: &str) -> Arc<egui::Galley> {
        self.initial
            .take()
            .unwrap_or_else(|| message_editor_galley(ui, text, self.layout))
    }
}

const MESSAGE_EDITOR_MIN_ROWS: usize = 3;
const MESSAGE_EDITOR_MAX_ROWS: usize = 8;

fn message_editor_visible_rows(text: &str) -> usize {
    text.split('\n')
        .count()
        .clamp(MESSAGE_EDITOR_MIN_ROWS, MESSAGE_EDITOR_MAX_ROWS)
}

fn message_editor_content_width(
    ui: &egui::Ui,
    text: &str,
    galley: &egui::Galley,
    viewport_width: f32,
) -> f32 {
    let natural_width = if galley.size().x > 0.0 {
        galley.size().x
    } else {
        // Headless tests and a temporarily unavailable fallback font can
        // report a zero-width galley. Use a conservative byte-count estimate
        // only in that exceptional path; normal frames never rescan text.
        let longest_line = text.split('\n').map(str::len).max().unwrap_or(0);
        longest_line as f32 * egui::TextStyle::Body.resolve(ui.style()).size
    };
    // TextEdit's horizontal frame margin needs a little room beyond glyphs.
    (natural_width + 12.0).max(viewport_width)
}

pub(in crate::gui) fn message_editor_max_height(ui: &egui::Ui, text: &str) -> f32 {
    message_editor_height_for_rows(ui, message_editor_visible_rows(text))
}

fn message_editor_height_for_rows(ui: &egui::Ui, rows: usize) -> f32 {
    let rows = rows as f32;
    let text_height = rows * ui.text_style_height(&egui::TextStyle::Body);
    // Frame margins plus room for a horizontal scrollbar when a line is long.
    text_height + 8.0 + ui.spacing().scroll.allocated_width()
}

fn message_editor_content<R>(
    ui: &mut egui::Ui,
    salt: &'static str,
    width: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut content_rect = ui.available_rect_before_wrap();
    content_rect.max.x = content_rect.min.x + width;
    ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("message_editor_content", salt))
            .max_rect(content_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        add_contents,
    )
    .inner
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextEditContextMenuState {
    has_selection: bool,
    has_text: bool,
}

impl TextEditContextMenuState {
    fn new(text: &str, cursor_range: Option<egui::text::CCursorRange>) -> Self {
        Self {
            has_selection: cursor_range.is_some_and(|range| !range.is_empty()),
            has_text: !text.is_empty(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextEditContextAction {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl TextEditContextAction {
    fn viewport_command(self) -> Option<egui::ViewportCommand> {
        match self {
            Self::Cut => Some(egui::ViewportCommand::RequestCut),
            Self::Copy => Some(egui::ViewportCommand::RequestCopy),
            Self::Paste => Some(egui::ViewportCommand::RequestPaste),
            Self::SelectAll => None,
        }
    }
}

fn text_edit_context_button(
    ui: &mut egui::Ui,
    label: &str,
    key: egui::Key,
    enabled: bool,
) -> egui::Response {
    let shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, key);
    let shortcut_text = ui.ctx().format_shortcut(&shortcut);
    ui.add_enabled(
        enabled,
        egui::Button::new(label).shortcut_text(shortcut_text),
    )
}

fn apply_text_edit_context_action(
    ui: &mut egui::Ui,
    output: &mut egui::text_edit::TextEditOutput,
    action: TextEditContextAction,
) {
    let response = &output.response.response;
    response.request_focus();

    if let Some(command) = action.viewport_command() {
        // Let TextEdit process the native clipboard event on the next frame.
        // This preserves its undo history and marker-aware post-edit repair.
        ui.ctx().send_viewport_cmd(command);
        return;
    }

    let range = egui::text::CCursorRange::select_all(&output.galley);
    output.cursor_range = Some(range);
    output.state.cursor.set_char_range(Some(range));
    output.state.clone().store(ui.ctx(), response.id);
}

fn show_text_edit_context_menu(
    ui: &mut egui::Ui,
    text: &str,
    output: &mut egui::text_edit::TextEditOutput,
) {
    let state = TextEditContextMenuState::new(text, output.cursor_range);
    let response = output.response.response.clone();
    let mut action = None;

    response.context_menu(|ui| {
        if text_edit_context_button(ui, "Cut", egui::Key::X, state.has_selection).clicked() {
            action = Some(TextEditContextAction::Cut);
            ui.close();
        }
        if text_edit_context_button(ui, "Copy", egui::Key::C, state.has_selection).clicked() {
            action = Some(TextEditContextAction::Copy);
            ui.close();
        }
        if text_edit_context_button(ui, "Paste", egui::Key::V, true).clicked() {
            action = Some(TextEditContextAction::Paste);
            ui.close();
        }
        ui.separator();
        if text_edit_context_button(ui, "Select All", egui::Key::A, state.has_text).clicked() {
            action = Some(TextEditContextAction::SelectAll);
            ui.close();
        }
    });

    // A secondary click does not inherently focus TextEdit. Focus it when the
    // menu opens and again when applying an action so Paste targets this field.
    if response.secondary_clicked() {
        response.request_focus();
    }
    if let Some(action) = action {
        apply_text_edit_context_action(ui, output, action);
    }
}

fn bounded_multiline_text_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    salt: &'static str,
    layout: MessageEditorLayout,
    width: f32,
    hint: &str,
) -> egui::text_edit::TextEditOutput {
    let viewport_width = (width - ui.spacing().scroll.allocated_width()).max(64.0);
    let rows = message_editor_visible_rows(text);
    let max_height = message_editor_height_for_rows(ui, rows);

    // This is the layout TextEdit needs anyway. Read its natural width before
    // constructing the scroll content, then reuse the same Arc on TextEdit's
    // first layout request instead of walking every glyph a second time.
    let initial_galley = message_editor_galley(ui, text, layout);
    let content_width = message_editor_content_width(ui, text, &initial_galley, viewport_width);
    let mut editor_layouter = MessageEditorLayouter::new(layout, initial_galley);
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap_width: f32| {
        editor_layouter.layout(ui, buf.as_str())
    };

    let mut output = egui::ScrollArea::both()
        .id_salt(("message_editor_scroll", salt))
        .max_width(width)
        .max_height(max_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            message_editor_content(ui, salt, content_width, |ui| {
                egui::TextEdit::multiline(text)
                    .id_salt(salt)
                    .desired_width(content_width)
                    .desired_rows(rows)
                    .hint_text(hint)
                    .layouter(&mut layouter)
                    .show(ui)
            })
        })
        .inner;

    show_text_edit_context_menu(ui, text, &mut output);
    output
}

#[derive(Clone, Default)]
struct MarkerEditSnapshot {
    text: Arc<str>,
}

impl MarkerEditSnapshot {
    /// Synchronize programmatic changes (profile/channel switches and Insert
    /// popup actions) before TextEdit can mutate the value. Returns whether a
    /// copy was needed; an unchanged repaint only performs a byte comparison.
    fn sync_external(&mut self, current: &str) -> bool {
        if self.text.as_ref() == current {
            false
        } else {
            self.text = Arc::from(current);
            true
        }
    }

    fn repair_and_commit(&mut self, current: &mut String, changed: bool) -> bool {
        if changed {
            repair_after_edit(&self.text, current);
            self.text = Arc::from(current.as_str());
            true
        } else {
            false
        }
    }
}

/// Bounded multiline TextEdit for text that may contain `‹XX›` byte markers.
///
/// Three marker-aware behaviours layered on a plain `TextEdit`:
///
///  1. Coloured-marker highlighting via [`message_editor_galley`].
///  2. *Atomic* marker deletion via [`repair_after_edit`]: a single
///     keystroke that disturbs a complete marker removes the whole
///     4-character unit rather than leaving an orphan `‹` / `›`.
///  3. *Cursor jump*: if the caret lands strictly inside a marker
///     (typed-through, clicked-into, etc.), it snaps to the marker's
///     near edge — direction of movement when known, closer edge on a
///     fresh click. Markers behave as single atoms for navigation.
///
/// The pre-edit text, previous cursor position, and the widget id are
/// stashed in `egui::Memory` so [`show_insert_byte_button`] (rendered
/// from a different ui parent — the popup) can read the target's
/// cursor and write a new one after inserting markers.
pub(in crate::gui) fn marker_aware_text_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    salt: &'static str,
    code_page: Option<CodePage>,
    width: f32,
    hint: &str,
) -> egui::Response {
    let stash_prev_text = ui.id().with("marker_prev").with(salt);
    let stash_prev_cursor = ui.id().with("marker_prev_cursor").with(salt);
    // Shared (parent-independent) ids the insert-byte popup uses.
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(salt);

    // Cloning the snapshot clones only an Arc. Leave the stored value untouched
    // on unchanged frames; actual or programmatic edits replace it below.
    let mut snapshot = ui
        .memory(|m| m.data.get_temp::<MarkerEditSnapshot>(stash_prev_text))
        .unwrap_or_default();
    let mut store_snapshot = snapshot.sync_external(text);
    let prev_cursor: Option<usize> = ui.memory(|m| m.data.get_temp(stash_prev_cursor));

    let output = bounded_multiline_text_edit(
        ui,
        text,
        salt,
        MessageEditorLayout::MarkerAware(code_page),
        width,
        hint,
    );

    // TextEdit::show returns AtomLayoutResponse wrapping the actual
    // Response — unwrap once here so the rest reads naturally.
    let resp = output.response.response;
    store_snapshot |= snapshot.repair_and_commit(text, resp.changed());
    if store_snapshot {
        ui.memory_mut(|m| m.data.insert_temp(stash_prev_text, snapshot));
    }

    let widget_id = resp.id;
    ui.memory_mut(|m| m.data.insert_temp(shared_widget_id, widget_id));

    if let Some(range) = output.cursor_range {
        // Only snap when there's no active selection — otherwise we'd
        // yank the user's shift-arrow selection sideways.
        let has_selection = range.primary != range.secondary;
        let cursor_char = range.primary.index;
        let cursor_byte = char_to_byte(text, cursor_char);
        let mut effective_cursor_char = cursor_char;
        if !has_selection {
            for (mrange, seg) in segments(text) {
                if !matches!(seg, Segment::Byte(_)) {
                    continue;
                }
                if mrange.start < cursor_byte && cursor_byte < mrange.end {
                    let target_byte = match prev_cursor.map(|p| char_to_byte(text, p)) {
                        Some(p) if p < cursor_byte => mrange.end,
                        Some(p) if p > cursor_byte => mrange.start,
                        // Fresh click or stationary — closer edge, end on tie.
                        _ => {
                            if cursor_byte - mrange.start < mrange.end - cursor_byte {
                                mrange.start
                            } else {
                                mrange.end
                            }
                        }
                    };
                    let target_char = byte_to_char(text, target_byte);
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), widget_id) {
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(target_char),
                            )));
                        state.store(ui.ctx(), widget_id);
                    }
                    effective_cursor_char = target_char;
                    break;
                }
            }
        }
        ui.memory_mut(|m| {
            m.data.insert_temp(stash_prev_cursor, effective_cursor_char);
            m.data.insert_temp(shared_cursor_id, effective_cursor_char);
        });
    }

    resp
}

/// Plain bounded multiline TextEdit that stashes its cursor + widget id
/// under the same shared ids [`marker_aware_text_edit`] uses, so the
/// matching `Insert …` popup can find them. Use this for fields
/// that don't recognise `‹XX›` markers (UTF-16 in its default
/// Unicode mode).
pub(in crate::gui) fn plain_text_edit_with_cursor(
    ui: &mut egui::Ui,
    text: &mut String,
    salt: &'static str,
    width: f32,
    hint: &str,
) -> egui::Response {
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(salt);
    let output =
        bounded_multiline_text_edit(ui, text, salt, MessageEditorLayout::Plain, width, hint);
    let resp = output.response.response;
    ui.memory_mut(|m| m.data.insert_temp(shared_widget_id, resp.id));
    if let Some(range) = output.cursor_range {
        ui.memory_mut(|m| m.data.insert_temp(shared_cursor_id, range.primary.index));
    }
    resp
}

/// Byte position of the character at `char_idx` in `text`. Saturates to
/// `text.len()` for indices past the end (treat as the after-last position).
pub(super) fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// Character position of the byte at `byte_idx`. Clamps `byte_idx` to
/// `text.len()` first so callers don't have to.
pub(super) fn byte_to_char(text: &str, byte_idx: usize) -> usize {
    let byte_idx = byte_idx.min(text.len());
    text[..byte_idx].chars().count()
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn editor_galleys_preserve_newlines_without_soft_wrapping() {
        egui::__run_test_ui(|ui| {
            let text = format!("{}\nsecond line", "long ‹0D› ".repeat(500));
            for layout in [
                MessageEditorLayout::MarkerAware(None),
                MessageEditorLayout::Plain,
            ] {
                let galley = message_editor_galley(ui, &text, layout);
                assert_eq!(galley.rows.len(), 2, "long lines must not soft-wrap");
            }
        });
    }

    #[test]
    fn editor_layouter_reuses_the_galley_that_supplied_its_width() {
        egui::__run_test_ui(|ui| {
            let text = "a long editor line";
            let initial = message_editor_galley(ui, text, MessageEditorLayout::MarkerAware(None));
            let mut layouter = MessageEditorLayouter::new(
                MessageEditorLayout::MarkerAware(None),
                Arc::clone(&initial),
            );

            let first = layouter.layout(ui, text);
            assert!(
                Arc::ptr_eq(&first, &initial),
                "TextEdit's first request must reuse the width galley"
            );
            let second = layouter.layout(ui, "changed");
            assert_eq!(second.text(), "changed");
        });
    }

    #[test]
    fn text_edit_context_menu_enables_actions_from_editor_state() {
        let caret = egui::text::CCursorRange::one(egui::text::CCursor::new(2));
        let selection =
            egui::text::CCursorRange::two(egui::text::CCursor::new(1), egui::text::CCursor::new(3));

        assert_eq!(
            TextEditContextMenuState::new("", None),
            TextEditContextMenuState {
                has_selection: false,
                has_text: false,
            }
        );
        assert_eq!(
            TextEditContextMenuState::new("text", Some(caret)),
            TextEditContextMenuState {
                has_selection: false,
                has_text: true,
            }
        );
        assert_eq!(
            TextEditContextMenuState::new("text", Some(selection)),
            TextEditContextMenuState {
                has_selection: true,
                has_text: true,
            }
        );
    }

    #[test]
    fn text_edit_clipboard_actions_use_native_viewport_requests() {
        assert_eq!(
            TextEditContextAction::Cut.viewport_command(),
            Some(egui::ViewportCommand::RequestCut)
        );
        assert_eq!(
            TextEditContextAction::Copy.viewport_command(),
            Some(egui::ViewportCommand::RequestCopy)
        );
        assert_eq!(
            TextEditContextAction::Paste.viewport_command(),
            Some(egui::ViewportCommand::RequestPaste)
        );
        assert_eq!(TextEditContextAction::SelectAll.viewport_command(), None);
    }

    #[test]
    fn text_edit_select_all_uses_and_persists_galley_character_bounds() {
        egui::__run_test_ui(|ui| {
            let mut text = "Aé‹1B›\nZ".to_owned();
            let mut output = egui::TextEdit::multiline(&mut text).show(ui);
            let widget_id = output.response.response.id;
            let expected = egui::text::CCursorRange::select_all(&output.galley);

            apply_text_edit_context_action(ui, &mut output, TextEditContextAction::SelectAll);

            assert_eq!(output.cursor_range, Some(expected));
            let stored = egui::TextEdit::load_state(ui.ctx(), widget_id)
                .expect("Select All must persist the TextEdit cursor state");
            assert_eq!(stored.cursor.char_range(), Some(expected));
            assert!(ui.memory(|memory| memory.has_focus(widget_id)));
        });
    }

    #[test]
    fn marker_snapshot_copies_only_when_text_changes() {
        let mut snapshot = MarkerEditSnapshot::default();
        assert!(snapshot.sync_external("A‹1B›B"));
        let unchanged = Arc::clone(&snapshot.text);

        assert!(!snapshot.sync_external("A‹1B›B"));
        assert!(Arc::ptr_eq(&snapshot.text, &unchanged));

        let mut edited = "A‹X1B›B".to_string();
        assert!(snapshot.repair_and_commit(&mut edited, true));
        assert_eq!(edited, "AB");
        assert_eq!(snapshot.text.as_ref(), "AB");

        assert!(snapshot.sync_external("profile replacement"));
        assert!(!snapshot.sync_external("profile replacement"));
    }

    #[test]
    fn replacement_background_distinguishes_fallback_from_literal_question_mark() {
        egui::__run_test_ui(|ui| {
            let editor = marker_layout_job(ui, "?—", Some(CodePage::Iso8859_1));
            assert_eq!(&editor.text[editor.sections[0].byte_range.clone()], "?");
            assert_eq!(
                editor.sections[0].format.background,
                egui::Color32::TRANSPARENT
            );
            assert_eq!(&editor.text[editor.sections[1].byte_range.clone()], "—");
            assert_ne!(
                editor.sections[1].format.background,
                egui::Color32::TRANSPARENT
            );

            let preview = preview_ascii_layout_job(ui, b"??", CodePage::Iso8859_1, &[1]);
            assert_eq!(
                preview.sections[0].format.background,
                egui::Color32::TRANSPARENT
            );
            assert_ne!(
                preview.sections[1].format.background,
                egui::Color32::TRANSPARENT
            );
        });
    }

    #[test]
    fn replacement_background_has_contrast_in_both_themes_and_output() {
        egui::__run_test_ui(|ui| {
            ui.visuals_mut().dark_mode = false;
            let (light_text, light_background) = replacement_highlight_colors(ui);
            assert_eq!(light_text, egui::Color32::from_rgb(45, 30, 0));
            assert_eq!(light_background, egui::Color32::from_rgb(255, 225, 150));

            let replacement_range = 1..2;
            let output = output_layout_job(ui, "??", std::slice::from_ref(&replacement_range));
            assert_eq!(
                output.sections[0].format.background,
                egui::Color32::TRANSPARENT
            );
            assert_eq!(output.sections[1].format.color, light_text);
            assert_eq!(output.sections[1].format.background, light_background);

            ui.visuals_mut().dark_mode = true;
            let (dark_text, dark_background) = replacement_highlight_colors(ui);
            assert_eq!(dark_text, egui::Color32::BLACK);
            assert_eq!(dark_background, wiredata_ui::palette::DARK.warning);
        });
    }

    #[test]
    fn message_editor_grows_for_newlines_then_caps_its_viewport() {
        assert_eq!(message_editor_visible_rows("one line"), 3);
        assert_eq!(message_editor_visible_rows("1\n2\n3\n4\n5"), 5);
        assert_eq!(message_editor_visible_rows(&"line\n".repeat(20)), 8);

        egui::__run_test_ui(|ui| {
            ui.set_width(500.0);
            let mut text = format!("{}\n{}", "wide ".repeat(500), "line\n".repeat(20));
            let before = ui.min_rect().bottom();
            let max_height = message_editor_max_height(ui, &text);
            let galley = message_editor_galley(ui, &text, MessageEditorLayout::MarkerAware(None));
            let content_width = message_editor_content_width(ui, &text, &galley, 300.0);
            let response =
                marker_aware_text_edit(ui, &mut text, "bounded_editor_test", None, 300.0, "text");
            let consumed = ui.min_rect().bottom() - before;
            assert!(
                consumed <= max_height + 1.0,
                "editor consumed {consumed} px despite a {max_height} px cap"
            );
            assert!(
                response.rect.width() > 300.0,
                "long lines must scroll: response={} content={content_width}",
                response.rect.width()
            );
            assert!(
                response.rect.height() > 8.0 * ui.text_style_height(&egui::TextStyle::Body),
                "all explicit lines should remain in scrollable content"
            );
        });
    }

    #[test]
    fn plain_message_editor_uses_the_same_bounded_scroll_geometry() {
        egui::__run_test_ui(|ui| {
            ui.set_width(500.0);
            let mut text = format!("{}\n{}", "wide ".repeat(500), "line\n".repeat(20));
            let before = ui.min_rect().bottom();
            let max_height = message_editor_max_height(ui, &text);
            let response =
                plain_text_edit_with_cursor(ui, &mut text, "plain_editor_test", 300.0, "text");
            let consumed = ui.min_rect().bottom() - before;

            assert!(consumed <= max_height + 1.0);
            assert!(response.rect.width() > 300.0);
            assert!(response.rect.height() > 8.0 * ui.text_style_height(&egui::TextStyle::Body));
        });
    }

    // ── char_to_byte / byte_to_char ───────────────────────────────────────────

    #[test]
    fn char_byte_round_trip_ascii() {
        let s = "hello";
        for i in 0..=s.len() {
            assert_eq!(byte_to_char(s, char_to_byte(s, i)), i.min(5));
        }
    }

    #[test]
    fn char_byte_handles_multibyte() {
        // 'A' (1 byte/char) + '‹' (3 bytes/1 char) + 'B' (1 byte/char)
        let s = "A\u{2039}B";
        assert_eq!(char_to_byte(s, 0), 0);
        assert_eq!(char_to_byte(s, 1), 1);
        assert_eq!(char_to_byte(s, 2), 4);
        assert_eq!(char_to_byte(s, 3), 5); // saturates to len
        assert_eq!(byte_to_char(s, 0), 0);
        assert_eq!(byte_to_char(s, 1), 1);
        assert_eq!(byte_to_char(s, 4), 2);
        assert_eq!(byte_to_char(s, 5), 3);
        assert_eq!(byte_to_char(s, 99), 3); // clamps past end
    }
}
