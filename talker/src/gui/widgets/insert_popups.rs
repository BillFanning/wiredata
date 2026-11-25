//! The Insert Byte and Insert Code Unit popups.

use super::*;

// ── Insert Byte / Insert Code Unit popups ─────────────────────────────────────

/// Parse the Insert Byte popup's hex input — single byte (`1B`) or a
/// space- and/or comma-separated list (`1B 0D 0A`, `1B,0D,0A`,
/// `1B, 0D 0A`). On failure the `Err` is the disabled-button hover text.
fn parse_hex_bytes(input: &str) -> Result<Vec<u8>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(
            "Type 1–2 hex digits — single byte (1B) or several separated by \
             spaces / commas (1B 0D 0A)"
                .to_string(),
        );
    }
    let pieces: Vec<&str> = trimmed
        .split([' ', ',', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if pieces.is_empty() {
        // All separators, no values — e.g. " , , ".
        return Err("No hex digits found between separators".to_string());
    }
    let mut out = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        if piece.len() > 2 {
            return Err(format!(
                "`{piece}` is more than 2 hex digits — split bytes with a \
                 space or comma (1B 0D 0A)"
            ));
        }
        match u8::from_str_radix(piece, 16) {
            Ok(b) => out.push(b),
            Err(_) => {
                return Err(format!(
                    "`{piece}` is not a valid hex byte — use 1–2 digits 0–9 / A–F"
                ))
            }
        }
    }
    Ok(out)
}

/// Parse the UTF-16 Insert popup's input — one or more 4-hex-digit
/// code units, optionally space/comma separated (`0E16`,
/// `0E16 1F62`, `0E16, 1F62`). Each piece must be exactly 4 hex
/// digits; the active byte order is applied by the caller, not here.
fn parse_hex_units(input: &str) -> Result<Vec<u16>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(
            "Type 4 hex digits — single code unit (0E16) or several separated \
             by spaces / commas (0E16 1F62)"
                .to_string(),
        );
    }
    let pieces: Vec<&str> = trimmed
        .split([' ', ',', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if pieces.is_empty() {
        return Err("No hex digits found between separators".to_string());
    }
    let mut out = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        if piece.len() != 4 {
            return Err(format!(
                "`{piece}` must be exactly 4 hex digits (one UTF-16 code unit) \
                 — use spaces or commas to separate units (0E16 1F62)"
            ));
        }
        match u16::from_str_radix(piece, 16) {
            Ok(u) => out.push(u),
            Err(_) => {
                return Err(format!(
                    "`{piece}` is not a valid hex code unit — use digits 0–9 / A–F"
                ))
            }
        }
    }
    Ok(out)
}

/// Chrome strings for the insert popup — bundled so
/// [`show_insert_popup`] doesn't take a parade of `&'static str`s.
struct InsertChrome {
    button_label: &'static str,
    field_label: &'static str,
    hint: &'static str,
}

/// "Insert Byte" button — popup inserts one or more raw bytes (each
/// wrapped in a `‹XX›` marker) at the target field's cursor. Used by
/// UTF-8, ASCII, and UTF-16 (with raw-bytes mode on) payloads.
pub(in crate::gui) fn show_insert_byte_button(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
) -> bool {
    show_insert_popup(
        ui,
        text,
        hex,
        target_salt,
        InsertChrome {
            button_label: "Insert Byte",
            field_label: "Byte value(s) (hex):",
            hint: "1B  or  1B 0D 0A",
        },
        |s| Ok(bytes_to_markers(&parse_hex_bytes(s)?)),
    )
}

/// "Insert Code Unit" button — UTF-16 variant. Each unit is 4 hex
/// digits (one `u16`). What gets inserted depends on `allow_raw_bytes`:
///
///  - `false` (default): the units decode as UTF-16 to actual
///    Unicode characters and are inserted verbatim. `0E16` inserts
///    `ฃ`, surrogate pairs are recognised, lone surrogates error.
///  - `true`: each unit splits into two raw bytes per `big_endian`
///    and is inserted as a pair of `‹XX›` markers.
pub(in crate::gui) fn show_insert_unit_button(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
    big_endian: bool,
    allow_raw_bytes: bool,
) -> bool {
    show_insert_popup(
        ui,
        text,
        hex,
        target_salt,
        InsertChrome {
            // `0E16` is Thai `ฃ` — renders because the shared font stack
            // bundles Noto Sans Thai as a fallback. CJK is *not* bundled,
            // so codepoints in U+4E00–9FFF still show as tofu.
            button_label: "Insert Code Unit",
            field_label: "Code unit(s) (4 hex):",
            hint: "0E16  or  0E16 1F62",
        },
        move |s| {
            let units = parse_hex_units(s)?;
            if allow_raw_bytes {
                let mut bytes = Vec::with_capacity(units.len() * 2);
                for u in &units {
                    if big_endian {
                        bytes.extend_from_slice(&u.to_be_bytes());
                    } else {
                        bytes.extend_from_slice(&u.to_le_bytes());
                    }
                }
                Ok(bytes_to_markers(&bytes))
            } else {
                String::from_utf16(&units).map_err(|_| {
                    "lone surrogate — pair high (D800–DBFF) and low (DC00–DFFF) \
                     surrogates together (e.g. D83D DE00 for 😀)"
                        .to_string()
                })
            }
        },
    )
}

/// Wrap each byte in a `‹XX›` marker (uppercase hex). The string is
/// what gets inserted into a marker-aware text field.
fn bytes_to_markers(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("\u{2039}{b:02X}\u{203A}"))
        .collect()
}

/// Shared MenuButton + popup chrome that the byte / code-unit insert
/// buttons hang off. `parse` turns the popup's text into the literal
/// string to splice into the target field at the cursor — bytes get
/// marker-wrapped by the caller's parser, glyph mode produces real
/// Unicode characters. Labels / hint live in [`InsertChrome`]; the
/// cursor bookkeeping is common to every caller.
///
/// `target_salt` must match the salt passed to
/// [`marker_aware_text_edit`] for the field this drives — that's how the
/// popup (rendered under a different ui parent) finds the target's
/// cursor and widget id in egui memory.
fn show_insert_popup<F>(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
    chrome: InsertChrome,
    parse: F,
) -> bool
where
    F: Fn(&str) -> Result<String, String>,
{
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(target_salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(target_salt);

    // Default menu close behavior is `CloseOnClick`, which closes the
    // menu the moment the user clicks anywhere inside — including the
    // TextEdit (which has to be clicked to gain focus). Switch to
    // `CloseOnClickOutside` so the popup stays open while the user
    // types the hex value.
    let mut changed = false;
    egui::containers::menu::MenuButton::new(chrome.button_label)
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            // Three rows — label, hex entry, Insert button — in a fixed
            // 200-px wide child UI with a non-justified top-down layout.
            // The fixed width keeps the popup compact enough to fit
            // below the trigger button (egui's auto-placement flips
            // popups above when they'd be too wide for the space below).
            // A bare `ui.vertical` would inherit the menu's
            // `top_down_justified` layout, which stretches each row to
            // the full layout width — re-introducing the same flip.
            ui.allocate_ui_with_layout(
                egui::vec2(200.0, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.label(chrome.field_label);
                    let resp = ui.add(
                        egui::TextEdit::singleline(hex)
                            .desired_width(180.0)
                            .hint_text(chrome.hint),
                    );
                    // Auto-focus the hex field so the user can start typing
                    // right away. Idempotent — egui doesn't keep resetting
                    // the caret if the field already has focus.
                    resp.request_focus();
                    let parse_result = parse(hex);
                    let ok = parse_result.is_ok();
                    // Enter while the popup is open commits — gated on the
                    // input parsing, not on `resp.lost_focus()` (which
                    // doesn't always fire for popup-hosted TextEdits, so
                    // Enter would otherwise feel dead). Only consume Enter
                    // when the value parses.
                    let entered =
                        resp.has_focus() && ok && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let mut insert = ui.add_enabled(ok, egui::Button::new("Insert"));
                    if let Err(why) = &parse_result {
                        insert = insert.on_disabled_hover_text(why.clone());
                    }
                    if let Ok(insertion) = &parse_result {
                        if insert.clicked() || entered {
                            // Cursor byte position from memory; fall back
                            // to end-of-text if the field was never focused.
                            let cursor_char: Option<usize> =
                                ui.memory(|m| m.data.get_temp(shared_cursor_id));
                            let insert_byte = cursor_char
                                .map(|c| char_to_byte(text, c))
                                .unwrap_or(text.len());
                            text.insert_str(insert_byte, insertion);
                            changed = true;
                            // New cursor sits right after the inserted
                            // text. Update both the shared stash (so a
                            // subsequent Insert lands in the right place
                            // even if the field isn't re-focused first)
                            // and the actual TextEditState.
                            let new_cursor_char = byte_to_char(text, insert_byte + insertion.len());
                            ui.memory_mut(|m| {
                                m.data.insert_temp(shared_cursor_id, new_cursor_char);
                            });
                            let widget_id: Option<egui::Id> =
                                ui.memory(|m| m.data.get_temp(shared_widget_id));
                            if let Some(wid) = widget_id {
                                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), wid) {
                                    state.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::one(egui::text::CCursor::new(
                                            new_cursor_char,
                                        )),
                                    ));
                                    state.store(ui.ctx(), wid);
                                }
                            }
                            hex.clear();
                            ui.close();
                        }
                    }
                },
            );
        });
    changed
}

#[cfg(test)]
mod tests {

    use super::*;

    // ── parse_hex_bytes ───────────────────────────────────────────────────────

    #[test]
    fn parse_single_byte() {
        assert_eq!(parse_hex_bytes("1B").unwrap(), vec![0x1B]);
        assert_eq!(parse_hex_bytes("ff").unwrap(), vec![0xFF]);
        assert_eq!(parse_hex_bytes("0").unwrap(), vec![0x00]);
    }

    #[test]
    fn parse_space_separated() {
        assert_eq!(parse_hex_bytes("1B 0D 0A").unwrap(), vec![0x1B, 0x0D, 0x0A]);
    }

    #[test]
    fn parse_comma_separated() {
        assert_eq!(parse_hex_bytes("1B,0D,0A").unwrap(), vec![0x1B, 0x0D, 0x0A]);
    }

    #[test]
    fn parse_mixed_separators_and_extra_whitespace() {
        assert_eq!(
            parse_hex_bytes("  1B,  0D 0A,   FF  ").unwrap(),
            vec![0x1B, 0x0D, 0x0A, 0xFF]
        );
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_hex_bytes("").is_err());
        assert!(parse_hex_bytes("   ").is_err());
        assert!(parse_hex_bytes(" , , ").is_err());
    }

    #[test]
    fn parse_rejects_non_hex() {
        let err = parse_hex_bytes("1B XY 0D").unwrap_err();
        assert!(err.contains("XY"), "{err}");
    }

    #[test]
    fn parse_rejects_too_long_piece() {
        let err = parse_hex_bytes("1B0D").unwrap_err();
        assert!(err.contains("more than 2"), "{err}");
    }

    // ── parse_hex_units ───────────────────────────────────────────────────────

    #[test]
    fn parse_units_single_and_multiple() {
        assert_eq!(parse_hex_units("0E16").unwrap(), vec![0x0E16]);
        assert_eq!(parse_hex_units("0E16 1F62").unwrap(), vec![0x0E16, 0x1F62]);
        assert_eq!(parse_hex_units("0E16,1F62").unwrap(), vec![0x0E16, 0x1F62]);
        assert_eq!(
            parse_hex_units("  0E16,  1F62  ").unwrap(),
            vec![0x0E16, 0x1F62]
        );
    }

    #[test]
    fn parse_units_rejects_wrong_length() {
        // 3 digits, 5 digits, and a missing space between two units.
        for input in ["E16", "01F62", "0E161F62"] {
            let err = parse_hex_units(input).unwrap_err();
            assert!(err.contains("exactly 4 hex digits"), "{input}: {err}");
        }
    }

    #[test]
    fn parse_units_rejects_non_hex() {
        let err = parse_hex_units("XYZW").unwrap_err();
        assert!(err.contains("XYZW"), "{err}");
    }

    #[test]
    fn parse_units_rejects_empty() {
        assert!(parse_hex_units("").is_err());
        assert!(parse_hex_units("   ").is_err());
        assert!(parse_hex_units(", ,").is_err());
    }
}
