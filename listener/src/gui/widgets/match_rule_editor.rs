//! A minimal in-app editor for **Mark timestamp** rules (§50.2): create a
//! `BytePattern → Mark(+timestamp)` rule so a matched pattern gets an inline local
//! time or NMEA ZDA annotation in the display and Display Recording. This is deliberately small
//! — it edits only single-pattern Mark rules; the general match-rule editor
//! (Idle/Record/Notify/PauseDisplay, compound conditions) is a separate TODO. Rules
//! it doesn't understand are listed read-only and never modified.
//!
//! Edits go straight into `ChannelConfig.match_rules`; because `config_needs_restart`
//! counts `match_rules`, committing flips the lifecycle button to **Apply & Restart**,
//! which rebuilds the pipeline with the new rules (no dedicated runtime command).

use crate::config::{
    ChannelConfig, MarkPosition, MarkTimestamp, MarkTimestampStyle, MatchAction, MatchCondition,
    MatchRule,
};
use crate::core::{validate_zda_talker_id, TimestampConfig, MAX_ZDA_TALKER_ID_BYTES};

/// Is this rule one the Mark editor owns — a single `BytePattern` condition whose only
/// action is a timestamped `Mark`? Other rules (Idle, Record, bare Mark, multi-action)
/// are shown read-only so the editor never silently drops profile-authored rules.
fn is_mark_timestamp_rule(rule: &MatchRule) -> bool {
    matches!(rule.condition, MatchCondition::BytePattern { .. })
        && matches!(
            rule.actions.as_slice(),
            [MatchAction::Mark { timestamp: Some(_) }]
        )
}

/// Edit the channel's inline **Mark timestamp** rules (§50.2). Lists the editor-owned
/// rules with pattern / position / format toggles and a delete button, plus an
/// add-row. Returns nothing — edits land in `config.match_rules`; the caller's
/// Apply & Restart path picks them up.
pub(crate) fn edit_mark_rules(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    ui.label("Timestamp marks").on_hover_text(
        "Insert local arrival time or an NMEA ZDA sentence before/after a byte pattern, \
             inline in the view and .disp recording (never .raw). Applying restarts the channel.",
    );

    // Edit existing editor-owned rules in place; collect indices to delete afterwards
    // so we don't mutate the vec while iterating it.
    let mut delete: Option<usize> = None;
    for (i, rule) in config.match_rules.iter_mut().enumerate() {
        if !is_mark_timestamp_rule(rule) {
            continue;
        }
        // Both matches! guards above guarantee these destructures succeed.
        let MatchCondition::BytePattern { pattern } = &mut rule.condition else {
            continue;
        };
        let [MatchAction::Mark {
            timestamp: Some(ts),
        }] = rule.actions.as_mut_slice()
        else {
            continue;
        };
        ui.horizontal(|ui| {
            // The enabled flag is honored when the channel (re)starts, so the
            // checkbox edits it like any other rule field — committed via
            // Apply & Restart. (A *live* toggle without a restart is the
            // deferred ADR-012 seam; this is not that.)
            ui.checkbox(&mut rule.enabled, "").on_hover_text(
                "Evaluate this rule. Uncheck to keep it configured but inactive. \
                 Applying restarts the channel.",
            );
            mark_pattern_field(ui, pattern);
            mark_position_selector(ui, &mut ts.position);
            mark_style_selector(ui, &mut ts.style);
            if ui
                .button("🗑")
                .on_hover_text("Delete this mark rule")
                .clicked()
            {
                delete = Some(i);
            }
        });
        ui.indent(("mark_options", i), |ui| {
            ui.horizontal(|ui| {
                match &mut ts.style {
                    MarkTimestampStyle::Plain {} => mark_format_toggles(ui, &mut ts.format),
                    MarkTimestampStyle::NmeaZda { talker } => {
                        mark_zda_talker_field(ui, talker);
                        ui.checkbox(&mut ts.format.include_millis, "ms")
                            .on_hover_text("Include milliseconds in the ZDA UTC time field.");
                    }
                }
                mark_separator_field(ui, &mut ts.separator);
            });
        });
    }
    if let Some(i) = delete {
        config.match_rules.remove(i);
    }

    // Add-row: an "Add" button appends a new Before/HH:MM:SS mark on an empty pattern.
    // The empty pattern is invalid (validation rejects it) until the user types one, so
    // it can't fire; the user fills it in on the row above after adding.
    if ui
        .button("+ Add timestamp mark")
        .on_hover_text("Add a new byte-pattern → timestamp rule")
        .clicked()
    {
        let n = config
            .match_rules
            .iter()
            .filter(|r| is_mark_timestamp_rule(r))
            .count();
        config.match_rules.push(MatchRule {
            name: format!("mark {}", n + 1),
            condition: MatchCondition::BytePattern {
                pattern: Vec::new(),
            },
            actions: vec![MatchAction::Mark {
                timestamp: Some(MarkTimestamp {
                    position: MarkPosition::Before,
                    style: Default::default(),
                    format: TimestampConfig::default(),
                    separator: String::new(),
                }),
            }],
            enabled: true,
        });
    }
}

/// Parse editor text into bytes, interpreting `<XX>` (exactly two hex digits —
/// the same notation the view's HexEscape mode renders, e.g. `<0D><0A>` for CRLF)
/// as a single byte. Everything else is literal UTF-8. An incomplete or
/// non-hex `<…` stays literal, so the field is stable while an escape is being
/// typed. (Consequence: a *literal* `<XX>` five-character string can't be
/// expressed — it always reads as the byte.)
fn parse_escaped_bytes(text: &str) -> Vec<u8> {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|v| v as u8)
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' && i + 3 < bytes.len() && bytes[i + 3] == b'>' {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Render bytes as editor text — the inverse of [`parse_escaped_bytes`]:
/// printable characters pass through; control bytes (≤ 0x1F, 0x7F) and bytes
/// that aren't valid UTF-8 become `<XX>`, matching the view's HexEscape
/// rendering. `parse_escaped_bytes(format_escaped_bytes(b)) == b`.
fn format_escaped_bytes(bytes: &[u8]) -> String {
    fn push_chars(out: &mut String, s: &str) {
        for c in s.chars() {
            let cp = c as u32;
            if cp <= 0x1F || cp == 0x7F {
                out.push_str(&format!("<{cp:02X}>"));
            } else {
                out.push(c);
            }
        }
    }
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match std::str::from_utf8(&bytes[i..]) {
            Ok(s) => {
                push_chars(&mut out, s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                push_chars(&mut out, std::str::from_utf8(&bytes[i..i + valid]).unwrap());
                i += valid;
                out.push_str(&format!("<{:02X}>", bytes[i]));
                i += 1;
            }
        }
    }
    out
}

/// The pattern field: edit the byte pattern as text. Control (and non-UTF-8)
/// bytes read and write as `<XX>` hex escapes — the view's own HexEscape
/// notation — so a CRLF-anchored pattern is typed as e.g. `GGA<0D><0A>`. A
/// non-UTF-8 pattern from a profile round-trips through edits instead of being
/// lossily replaced.
fn mark_pattern_field(ui: &mut egui::Ui, pattern: &mut Vec<u8>) {
    let mut text = format_escaped_bytes(pattern);
    let resp = ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(120.0)
                .hint_text("pattern e.g. $GPGGA"),
        )
        .on_hover_text(
            "Byte pattern to match. Control bytes as <XX> hex escapes, e.g. \
             $GPGGA or GGA<0D><0A> — the same notation the Hex-escape view shows.",
        );
    if resp.changed() {
        *pattern = parse_escaped_bytes(&text);
    }
    if pattern.is_empty() {
        ui.label(egui::RichText::new("⚠ empty").weak());
    }
}

/// Before/After selector for where the timestamp is spliced relative to the match.
fn mark_position_selector(ui: &mut egui::Ui, position: &mut MarkPosition) {
    egui::ComboBox::from_id_salt(("mark_pos", ui.next_auto_id()))
        .selected_text(match position {
            MarkPosition::Before => "before",
            MarkPosition::After => "after",
        })
        .width(64.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(position, MarkPosition::Before, "before");
            ui.selectable_value(position, MarkPosition::After, "after");
        });
}

fn mark_style_selector(ui: &mut egui::Ui, style: &mut MarkTimestampStyle) {
    let mut zda = matches!(style, MarkTimestampStyle::NmeaZda { .. });
    egui::ComboBox::from_id_salt(("mark_style", ui.next_auto_id()))
        .selected_text(if zda { "NMEA ZDA" } else { "Time" })
        .width(82.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut zda, false, "Time");
            ui.selectable_value(&mut zda, true, "NMEA ZDA");
        });
    if zda && matches!(style, MarkTimestampStyle::Plain {}) {
        *style = MarkTimestampStyle::NmeaZda {
            talker: "GP".to_string(),
        };
    } else if !zda && matches!(style, MarkTimestampStyle::NmeaZda { .. }) {
        *style = MarkTimestampStyle::Plain {};
    }
}

fn mark_zda_talker_field(ui: &mut egui::Ui, talker: &mut String) {
    ui.label("talker");
    let response = ui.add(
        egui::TextEdit::singleline(talker)
            .desired_width(112.0)
            .char_limit(MAX_ZDA_TALKER_ID_BYTES)
            .hint_text("GP"),
    );
    if response.changed() {
        *talker = talker.to_ascii_uppercase();
    }
    response.on_hover_text(
        "One to 32 printable ASCII characters. Two characters form a standard NMEA talker ID; \
         longer values are custom IDs. Whitespace and $ ! , * are not allowed.",
    );
    if let Err(error) = validate_zda_talker_id(talker) {
        ui.colored_label(ui.visuals().error_fg_color, "invalid talker")
            .on_hover_text(error.to_string());
    }
}

/// Free-text separator appended right after the timestamp — a space, `", "`, or
/// a control byte as a `<XX>` hex escape (`<0A>` puts the data on a new line) —
/// so the time stands apart from the adjacent data in the view and `.disp`.
fn mark_separator_field(ui: &mut egui::Ui, separator: &mut String) {
    ui.label("sep");
    let mut text = format_escaped_bytes(separator.as_bytes());
    let resp = ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(100.0)
                .hint_text("␣ ,"),
        )
        .on_hover_text(
            "Text added immediately after the timestamp to separate it from the \
             data — e.g. a space, \", \", or a control byte as a <XX> hex escape \
             (<0A> = newline). Empty = nothing added.",
        );
    if resp.changed() {
        // The separator is spliced into rendered text, so it stays a String;
        // escapes that don't decode to valid UTF-8 become U+FFFD.
        *separator = String::from_utf8_lossy(&parse_escaped_bytes(&text)).into_owned();
    }
}

/// The three independent format toggles (date / millis / timezone). Time-of-day is
/// always present, so there's no toggle for it — matches talker's `TimestampConfig`.
fn mark_format_toggles(ui: &mut egui::Ui, format: &mut TimestampConfig) {
    ui.checkbox(&mut format.include_date, "date")
        .on_hover_text("Prefix the calendar date (YYYY-MM-DD).");
    ui.checkbox(&mut format.include_millis, "ms")
        .on_hover_text("Include milliseconds (.123).");
    ui.checkbox(&mut format.include_timezone, "tz")
        .on_hover_text("Append the local UTC offset (e.g. -07:00).");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::templates;

    #[test]
    fn recognizes_only_single_timestamped_mark_rules() {
        let mut rule = MatchRule {
            name: "m".into(),
            condition: MatchCondition::BytePattern {
                pattern: b"$".to_vec(),
            },
            actions: vec![MatchAction::Mark {
                timestamp: Some(MarkTimestamp {
                    position: MarkPosition::Before,
                    style: Default::default(),
                    format: TimestampConfig::default(),
                    separator: String::new(),
                }),
            }],
            enabled: true,
        };
        assert!(is_mark_timestamp_rule(&rule));

        // A bare Mark (no timestamp) is not owned by this editor.
        rule.actions = vec![MatchAction::Mark { timestamp: None }];
        assert!(!is_mark_timestamp_rule(&rule));

        // An Idle condition is not owned either.
        rule.condition = MatchCondition::Idle { timeout_ms: 1000 };
        assert!(!is_mark_timestamp_rule(&rule));
    }

    #[test]
    fn hex_escapes_parse_to_bytes_and_incomplete_ones_stay_literal() {
        assert_eq!(parse_escaped_bytes("$GPGGA"), b"$GPGGA");
        assert_eq!(parse_escaped_bytes("GGA<0D><0A>"), b"GGA\r\n");
        assert_eq!(parse_escaped_bytes("a<0d>b"), b"a\rb"); // lowercase hex too
        assert_eq!(parse_escaped_bytes("<FF>"), &[0xFF]);
        // Mid-typing stability: an incomplete or non-hex escape is literal.
        assert_eq!(parse_escaped_bytes("<0"), b"<0");
        assert_eq!(parse_escaped_bytes("<ZZ>"), b"<ZZ>");
        assert_eq!(parse_escaped_bytes("a<b"), b"a<b");
    }

    #[test]
    fn format_escapes_controls_and_round_trips() {
        assert_eq!(format_escaped_bytes(b"$GPGGA\r\n"), "$GPGGA<0D><0A>");
        assert_eq!(format_escaped_bytes(&[0x00, 0x7F]), "<00><7F>");
        assert_eq!(format_escaped_bytes(&[0xFF]), "<FF>"); // not valid UTF-8
        assert_eq!(format_escaped_bytes(" ok".as_bytes()), " ok"); // space stays

        // parse(format(bytes)) is identity — controls, non-UTF-8, multi-byte
        // UTF-8, and a partially-typed escape all round-trip.
        for bytes in [
            &b"$GPGGA\r\n"[..],
            &[0x00, 0x1F, 0x7F, 0xFF],
            "aéb".as_bytes(),
            b"$GP<0",
        ] {
            assert_eq!(
                parse_escaped_bytes(&format_escaped_bytes(bytes)),
                bytes,
                "round trip failed for {bytes:?}"
            );
        }
    }

    #[test]
    fn ignores_channels_with_unrelated_rules() {
        // A channel template has no match rules; the editor treats it as empty.
        let channel = templates::udp_template();
        assert!(!channel.match_rules.iter().any(is_mark_timestamp_rule));
    }
}
