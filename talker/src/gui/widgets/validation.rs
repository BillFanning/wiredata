//! Field validation and the red-border chrome for invalid values.

use crate::core::message::{ChecksumAlgorithm, CodePage};

// ── Validation / chrome helpers ───────────────────────────────────────────────

/// "Broken value" red-box for any text-input field. Calls `add` to
/// render the field, and when `invalid` is true tints the fill pink,
/// paints a 2-px red outline just outside the widget rect, and
/// attaches `msg` as the hover tooltip. Returns the field's
/// [`egui::Response`] unchanged so callers can still chain `lost_focus()`,
/// `changed()`, etc.
///
/// Replaces an earlier "red error row below the field" pattern that
/// pushed surrounding controls around as the user typed.
pub(in crate::gui) fn red_bordered<F>(
    ui: &mut egui::Ui,
    invalid: bool,
    msg: &str,
    add: F,
) -> egui::Response
where
    F: FnOnce(&mut egui::Ui) -> egui::Response,
{
    /// How strongly an invalid field is washed with the fault red. Low enough
    /// that the text the user is fixing stays the most legible thing in it.
    const TINT_ALPHA: u8 = 36;

    // The shared fault red, so an invalid field, a faulted channel and a failed
    // recording are all the same red — this used to be its own `220,80,80`,
    // which its own comment described as "the rest of the GUI's warning red".
    let red = wiredata_ui::palette::active(ui).fault;
    // Derived from the same red rather than named, so the wash cannot drift
    // away from the outline it sits inside, and it lands pale on the light
    // theme and deep on the dark one without a second constant.
    let tint = wiredata_ui::palette::tint(ui, red, TINT_ALPHA);

    // No `ui.scope`: a scope inherits a wrapping layout, and a field inside
    // it then shrinks to the room left on the row instead of wrapping to the
    // next one. The fill is set on the caller's own `Ui` and put back, so the
    // field's id never depends on whether it is invalid either.
    let resp = if invalid {
        let v = ui.visuals_mut();
        let saved = (v.text_edit_bg_color, v.extreme_bg_color);
        // Pink fill via the two fields TextEdit might read:
        //  - `text_edit_bg_color` is the explicit override
        //  - `extreme_bg_color` is the fallback when the former is `None`
        v.text_edit_bg_color = Some(tint);
        v.extreme_bg_color = tint;
        let resp = add(ui);
        let v = ui.visuals_mut();
        (v.text_edit_bg_color, v.extreme_bg_color) = saved;
        resp
    } else {
        add(ui)
    };
    if invalid {
        // Explicit outline outside the rect — guarantees a visible
        // 2-px red box regardless of which `Visuals` field a given
        // egui version's TextEdit uses for its border.
        ui.painter().rect_stroke(
            resp.rect,
            egui::CornerRadius::same(2),
            egui::Stroke::new(2.0_f32, red),
            egui::StrokeKind::Outside,
        );
        resp.on_hover_text(msg)
    } else {
        resp
    }
}

/// True when the user "committed" the contents of a TextEdit by pressing
/// Enter on the way out — the pattern we use to apply interface-field
/// changes to the running talker thread.
pub(in crate::gui) fn enter_committed(r: &egui::Response, ui: &egui::Ui) -> bool {
    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

/// True when `s` is a non-empty string that fails to parse as `T`.
/// Used to drive the red-border / start-blocker validation: empty means
/// "user hasn't typed anything here yet" (not an error to surface),
/// whereas non-empty + parse fail means the user typed something wrong.
pub(in crate::gui) fn invalid_parse<T>(s: &str) -> bool
where
    T: std::str::FromStr,
{
    !s.is_empty() && s.parse::<T>().is_err()
}

/// "Broken value" check for an IPv4-address text field, tolerant of
/// partial typing.
///
/// Empty and not-yet-complete inputs are considered OK so the field
/// doesn't flash red while the user is mid-type. Only flags red once
/// the string is unambiguously garbage:
///
///  - any character that isn't a digit or `.`
///  - more than four dot-separated parts
///  - exactly four parts with none empty, but the whole string still
///    fails to parse as [`Ipv4Addr`] (e.g. `192.168.1.999`)
///
/// In particular the LAN-prefix default `192.168.1.` (4 parts, last
/// empty) is considered "still being typed" — no red.
pub(super) fn invalid_ipv4(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.chars().any(|c| !c.is_ascii_digit() && c != '.') {
        return true;
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 4 {
        return true;
    }
    parts.len() == 4
        && parts.iter().all(|p| !p.is_empty())
        && s.parse::<std::net::Ipv4Addr>().is_err()
}

pub(in crate::gui) fn hex_valid(s: &str) -> bool {
    let stripped: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    !stripped.is_empty()
        && stripped.len().is_multiple_of(2)
        && stripped.chars().all(|c| c.is_ascii_hexdigit())
}

pub(in crate::gui) fn code_page_label(code_page: CodePage) -> &'static str {
    match code_page {
        CodePage::Iso8859_1 => "ISO-8859-1",
        CodePage::Windows1252 => "Windows-1252",
        CodePage::Cp437 => "CP437",
        CodePage::MacRoman => "Mac OS Roman",
    }
}

pub(in crate::gui) fn checksum_label(algorithm: ChecksumAlgorithm) -> &'static str {
    match algorithm {
        ChecksumAlgorithm::Xor => "XOR",
        ChecksumAlgorithm::Crc8 => "CRC-8",
        ChecksumAlgorithm::Crc16Kermit => "CRC-16/KERMIT",
        ChecksumAlgorithm::Crc16Modbus => "CRC-16/MODBUS",
        ChecksumAlgorithm::Crc32 => "CRC-32",
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    // ── invalid_ipv4 ──────────────────────────────────────────────────────────

    #[test]
    fn ipv4_empty_is_not_invalid() {
        assert!(!invalid_ipv4(""));
    }

    #[test]
    fn ipv4_partial_typing_is_not_invalid() {
        // The user is mid-typing; don't flash red yet.
        for s in [
            "1",
            "19",
            "192",
            "192.",
            "192.168",
            "192.168.1",
            "192.168.1.",
        ] {
            assert!(!invalid_ipv4(s), "{s:?} should be treated as partial");
        }
    }

    #[test]
    fn ipv4_complete_valid_is_not_invalid() {
        for s in ["0.0.0.0", "192.168.1.5", "255.255.255.255"] {
            assert!(!invalid_ipv4(s), "{s:?} parses as Ipv4Addr");
        }
    }

    #[test]
    fn ipv4_garbage_chars_are_invalid() {
        for s in ["abc", "192.168.1.a", "192-168-1-5", "192.168.1.5 "] {
            assert!(invalid_ipv4(s), "{s:?} contains non-IPv4 characters");
        }
    }

    #[test]
    fn ipv4_too_many_parts_or_out_of_range_is_invalid() {
        for s in ["192.168.1.5.6", "192.168.1.300", "1..2.3.4"] {
            assert!(invalid_ipv4(s), "{s:?} can never be a valid Ipv4Addr");
        }
    }

    /// The red-border tint is on the field and only the field: the style is
    /// put back, so the fields after it are not tinted.
    #[test]
    fn an_invalid_field_leaves_the_style_as_it_found_it() {
        egui::__run_test_ui(|ui| {
            let before = (
                ui.visuals().text_edit_bg_color,
                ui.visuals().extreme_bg_color,
            );
            let mut text = String::new();
            let mut during = None;
            red_bordered(ui, true, "bad", |ui| {
                during = Some((
                    ui.visuals().text_edit_bg_color,
                    ui.visuals().extreme_bg_color,
                ));
                ui.add(egui::TextEdit::singleline(&mut text))
            });
            assert_ne!(
                during,
                Some(before),
                "the field is tinted while it is drawn"
            );
            assert_eq!(
                (
                    ui.visuals().text_edit_bg_color,
                    ui.visuals().extreme_bg_color
                ),
                before,
                "and the tint is taken off after it"
            );
        });
    }
}
