//! The match rule editor (§50.2): every rule a channel has, each a condition
//! and the actions it fires, edited on the channel's draft. Every condition and
//! action the runtime supports can be made here, so the editor shows every rule
//! a profile holds and drops none.
//!
//! Edits go straight into `ChannelConfig.match_rules`; because `config_needs_restart`
//! counts `match_rules`, committing flips the lifecycle button to **Apply & Restart**,
//! which rebuilds the pipeline with the new rules (no dedicated runtime command).

use crate::config::limits::{MAX_MATCH_ACTIONS, MAX_MATCH_RULES};
use crate::config::{
    ChannelConfig, MarkPosition, MarkTimestamp, MarkTimestampStyle, MatchAction, MatchCondition,
    MatchRule, RecordControl, RecordTarget,
};
use crate::core::{validate_zda_talker_id, TimestampConfig, MAX_ZDA_TALKER_ID_BYTES};
use crate::diagnostics::DiagnosticSeverity;
use crate::display::valid_utf8_prefix;
use wiredata_ui::hover::{Hover, ShowHover};

const RULES_HOVER: Hover = Hover::new(
    "A rule is a trigger and the actions it initiates.\n\n\
     The trigger is a byte pattern arriving, any byte from a set arriving, or nothing \
     arriving for a set time. When \
     it happens, the rule does each of its actions in turn: put a timestamp or a \
     marker line next to the pattern, begin or stop a recording, add a line to the \
     Diagnostics, or pause the view.\n\n\
     Rules never change the received data or the Raw recording. Changes take effect \
     with Apply & Restart, which restarts the channel.",
);
const RULE_ENABLED_HOVER: Hover = Hover::new(
    "Turns this rule on or off.\n\n\
     When unchecked, the rule stays in the list but does nothing. The change takes \
     effect with Apply & Restart.",
);
const RULE_NAME_HOVER: Hover =
    Hover::new("A name to tell this rule apart from the others. It is saved with the profile.");
const CONDITION_HOVER: Hover = Hover::new(
    "The trigger: what makes the rule act.\n\n\
     Pattern acts each time the bytes typed beside it arrive, even when they are \
     split across two reads.\n\n\
     Any byte of acts when a byte from the set beside it arrives: once at the \
     first of each unbroken run of them, so a burst of line noise acts once.\n\n\
     Quiet for acts once when nothing has arrived for the time beside it, and again \
     after data has arrived and stopped again.",
);
const PATTERN_HOVER: Hover = Hover::new(
    "The bytes to look for in the received data.\n\n\
     Type them as text. Write a control byte as two hex digits between angle \
     brackets: GGA<0D><0A> is GGA followed by a carriage return and a line feed. \
     This is the same notation the view shows when View is Raw and ctrl-chars is \
     <0A>.\n\n\
     A rule with an empty pattern never acts.",
);
const BYTE_SET_HOVER: Hover = Hover::new(
    "The bytes that trigger the rule, as hex.\n\n\
     Type each byte as two hex digits, and three or more in a row as a range, \
     separated by spaces: 00-08 0B 0C 0E-1F 7F is every control byte except tab, CR \
     and LF. Presets fills in a common choice.\n\n\
     The rule acts at the first byte of each unbroken run of these bytes, and a \
     timestamp or a recording begins or ends at that one byte.",
);
const IDLE_HOVER: Hover = Hover::new(
    "How long the channel must receive nothing before the rule acts, in seconds.\n\n\
     Type a number from 0.1 to 86400, such as 5 or 1.5, or use − and + to change it \
     by 1 s. On Windows the rule can act up to about 16 ms after this time.",
);
const ACTION_HOVER: Hover = Hover::new(
    "What the rule does when its trigger happens.\n\n\
     Timestamp puts the arrival time, or an NMEA ZDA sentence, next to the pattern \
     in the view and the Display recording.\n\n\
     Marker line writes a ‹MARK› line into the Display recording. The view does not \
     show it.\n\n\
     Record begins or stops the Raw recording, the Display recording or both, \
     using the settings under Record Raw Data and Record Display. The pattern \
     itself is recorded either way.\n\n\
     Notify adds a line to the channel's Diagnostics and to listener's event log \
     file.\n\n\
     Pause display freezes the view. Reception and recording carry on, and Resume \
     brings the view back.",
);
const TIMESTAMP_NEEDS_PATTERN_HOVER: Hover = Hover::new(
    "A timestamp goes next to the pattern that was received. A Quiet for rule has no \
     pattern to put it beside, so this action adds nothing. Use Marker line or \
     Notify to see when the channel went quiet.",
);
const TALKER_HOVER: Hover = Hover::new(
    "The talker ID at the start of the ZDA sentence, such as GP in $GPZDA.\n\n\
     It is one to 32 printable ASCII characters, and letters are changed to capitals as \
     you type. Two characters make a standard NMEA talker ID; a longer value is a \
     custom ID. A space, tab, $, !, comma or * is not allowed.",
);
const SEPARATOR_HOVER: Hover = Hover::new(
    "Text added immediately after the timestamp, to set it apart from the data.\n\n\
     It can be a space, a comma and a space, or a control byte written as two hex \
     digits between angle brackets, such as <0A> for a new line. Leave it empty to add \
     nothing.",
);

/// The two kinds of condition, as the editor offers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConditionKind {
    Pattern,
    AnyByte,
    Quiet,
}

impl ConditionKind {
    const ALL: [Self; 3] = [Self::Pattern, Self::AnyByte, Self::Quiet];

    fn of(condition: &MatchCondition) -> Self {
        match condition {
            MatchCondition::BytePattern { .. } => Self::Pattern,
            MatchCondition::AnyByte { .. } => Self::AnyByte,
            MatchCondition::Idle { .. } => Self::Quiet,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pattern => "Pattern",
            Self::AnyByte => "Any byte of",
            Self::Quiet => "Quiet for",
        }
    }

    /// A new condition of this kind: an empty pattern, to be typed; the
    /// control bytes other than tab, CR and LF; or 5 s of quiet.
    fn new_condition(self) -> MatchCondition {
        match self {
            Self::Pattern => MatchCondition::BytePattern {
                pattern: Vec::new(),
            },
            Self::AnyByte => MatchCondition::AnyByte {
                bytes: CONTROL_EXCEPT_LINE_ENDS.bytes(),
            },
            Self::Quiet => MatchCondition::Idle {
                timeout_ms: DEFAULT_QUIET_MS,
            },
        }
    }
}

/// A byte set the Presets menu fills in, as inclusive ranges of values.
#[derive(Clone, Copy, Debug)]
struct BytePreset {
    label: &'static str,
    ranges: &'static [(u8, u8)],
}

impl BytePreset {
    fn bytes(self) -> Vec<u8> {
        self.ranges
            .iter()
            .flat_map(|&(first, last)| first..=last)
            .collect()
    }
}

/// The control bytes a text line does not normally hold: all but tab, CR
/// and LF. What a new Any byte of trigger starts with.
const CONTROL_EXCEPT_LINE_ENDS: BytePreset = BytePreset {
    label: "Control bytes except tab, CR and LF",
    ranges: &[(0x00, 0x08), (0x0B, 0x0C), (0x0E, 0x1F), (0x7F, 0x7F)],
};
const ALL_CONTROL: BytePreset = BytePreset {
    label: "All control bytes",
    ranges: &[(0x00, 0x1F), (0x7F, 0x7F)],
};
const ABOVE_7F: BytePreset = BytePreset {
    label: "Bytes above 7F",
    ranges: &[(0x80, 0xFF)],
};

/// A byte set as the box shows it: hex bytes in order, three or more in a
/// row as a range, such as `00-08 0B 0C 0E-1F 7F`.
fn byte_set_text(bytes: &[u8]) -> String {
    let set: std::collections::BTreeSet<u8> = bytes.iter().copied().collect();
    let mut runs: Vec<(u8, u8)> = Vec::new();
    for byte in set {
        match runs.last_mut() {
            Some((_, last)) if u16::from(*last) + 1 == u16::from(byte) => *last = byte,
            _ => runs.push((byte, byte)),
        }
    }
    let mut parts = Vec::new();
    for (first, last) in runs {
        if last - first >= 2 {
            parts.push(format!("{first:02X}-{last:02X}"));
        } else {
            parts.extend((first..=last).map(|byte| format!("{byte:02X}")));
        }
    }
    parts.join(" ")
}

/// Typed hex bytes and ranges, split by spaces or commas, as a sorted set;
/// `None` unless every part is two hex digits or a rising range of them.
fn parse_byte_set(text: &str) -> Option<Vec<u8>> {
    let hex = |part: &str| {
        (part.len() == 2)
            .then(|| u8::from_str_radix(part, 16).ok())
            .flatten()
    };
    let mut set = std::collections::BTreeSet::new();
    for part in text.split([' ', ',']).filter(|part| !part.is_empty()) {
        let (first, last) = match part.split_once('-') {
            Some((first, last)) => (hex(first)?, hex(last)?),
            None => (hex(part)?, hex(part)?),
        };
        if first > last {
            return None;
        }
        set.extend(first..=last);
    }
    (!set.is_empty()).then(|| set.into_iter().collect())
}

/// The quiet time a new Quiet for condition starts with.
const DEFAULT_QUIET_MS: u64 = 5_000;
/// The shortest and longest quiet time the field accepts: a tenth of a
/// second, and a day.
const QUIET_MIN_MS: u64 = 100;
const QUIET_MAX_MS: u64 = 86_400_000;
/// How far each − and + button moves the quiet time.
const QUIET_STEP_MS: u64 = 1_000;

/// The action kinds, as the editor offers them. A `Mark` is two of them: with a
/// timestamp, and without one (a marker line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionKind {
    Timestamp,
    MarkerLine,
    Record,
    Notify,
    PauseDisplay,
}

impl ActionKind {
    const ALL: [Self; 5] = [
        Self::Timestamp,
        Self::MarkerLine,
        Self::Record,
        Self::Notify,
        Self::PauseDisplay,
    ];

    fn of(action: &MatchAction) -> Self {
        match action {
            MatchAction::Mark { timestamp: Some(_) } => Self::Timestamp,
            MatchAction::Mark { timestamp: None } => Self::MarkerLine,
            MatchAction::Record { .. } => Self::Record,
            MatchAction::Notify { .. } => Self::Notify,
            MatchAction::PauseDisplay {} => Self::PauseDisplay,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Timestamp => "Timestamp",
            Self::MarkerLine => "Marker line",
            Self::Record => "Record",
            Self::Notify => "Notify",
            Self::PauseDisplay => "Pause display",
        }
    }

    /// A new action of this kind, with the settings it starts with.
    fn new_action(self) -> MatchAction {
        match self {
            Self::Timestamp => MatchAction::Mark {
                timestamp: Some(MarkTimestamp {
                    position: MarkPosition::Before,
                    style: Default::default(),
                    format: TimestampConfig::default(),
                    separator: String::new(),
                }),
            },
            Self::MarkerLine => MatchAction::Mark { timestamp: None },
            Self::Record => MatchAction::Record {
                target: RecordTarget::Raw,
                control: RecordControl::Begin,
            },
            Self::Notify => MatchAction::Notify {
                severity: DiagnosticSeverity::Warning,
            },
            Self::PauseDisplay => MatchAction::PauseDisplay {},
        }
    }
}

/// A new rule: an empty pattern, to be typed, with one timestamp action,
/// named after its place in the list.
fn new_rule(existing: usize) -> MatchRule {
    MatchRule {
        name: format!("rule {}", existing + 1),
        condition: ConditionKind::Pattern.new_condition(),
        actions: vec![ActionKind::Timestamp.new_action()],
        enabled: true,
    }
}

/// The quiet time in seconds, as the field shows it: no trailing zeros.
fn quiet_text(timeout_ms: u64) -> String {
    let text = format!("{}.{:03}", timeout_ms / 1000, timeout_ms % 1000);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Typed seconds as milliseconds, as the profile keeps them, or `None` for
/// text that is not a number in the field's range.
fn parse_quiet(text: &str) -> Option<u64> {
    let secs: f64 = text.trim().parse().ok()?;
    if !secs.is_finite() {
        return None;
    }
    let ms = (secs * 1000.0).round();
    (QUIET_MIN_MS as f64..=QUIET_MAX_MS as f64)
        .contains(&ms)
        .then_some(ms as u64)
}

/// The quiet time one button press later, kept within the field's range.
fn step_quiet(timeout_ms: u64, longer: bool) -> u64 {
    let stepped = if longer {
        timeout_ms.saturating_add(QUIET_STEP_MS)
    } else {
        timeout_ms.saturating_sub(QUIET_STEP_MS)
    };
    stepped.clamp(QUIET_MIN_MS, QUIET_MAX_MS)
}

/// Whether this action adds nothing on this condition: a timestamp on a Quiet
/// for rule has no pattern to sit beside.
fn timestamp_without_pattern(condition: &MatchCondition, action: &MatchAction) -> bool {
    matches!(condition, MatchCondition::Idle { .. })
        && ActionKind::of(action) == ActionKind::Timestamp
}

/// Edit the channel's match rules (§50.2): one group per rule, then a button
/// that adds one. Edits land in `config.match_rules`; the caller's Apply &
/// Restart path picks them up.
pub(crate) fn edit_match_rules(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    ui.label("A rule is a trigger and the actions it initiates.")
        .on_hover(&RULES_HOVER);
    let mut delete = None;
    for (i, rule) in config.match_rules.iter_mut().enumerate() {
        ui.push_id(("rule", i), |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                if edit_rule(ui, rule) {
                    delete = Some(i);
                }
            });
        });
    }
    if let Some(i) = delete {
        config.match_rules.remove(i);
    }
    let room = config.match_rules.len() < MAX_MATCH_RULES;
    let add = ui
        .add_enabled(room, egui::Button::new("+ Add rule"))
        .on_hover_text(
            "Adds a rule that puts a timestamp next to a pattern. Change what it does below it.",
        )
        .on_disabled_hover_text(format!(
            "A channel can have at most {MAX_MATCH_RULES} rules."
        ));
    if add.clicked() {
        config.match_rules.push(new_rule(config.match_rules.len()));
    }
}

/// One rule's group. Returns whether its delete button was clicked.
fn edit_rule(ui: &mut egui::Ui, rule: &mut MatchRule) -> bool {
    let mut delete = false;
    ui.horizontal_wrapped(|ui| {
        // The enabled flag is honored when the channel (re)starts, so the
        // checkbox edits it like any other rule field, committed via
        // Apply & Restart. (A *live* toggle without a restart is the
        // deferred ADR-012 seam; this is not that.)
        ui.checkbox(&mut rule.enabled, "")
            .on_hover(&RULE_ENABLED_HOVER);
        ui.add(egui::TextEdit::singleline(&mut rule.name).desired_width(140.0))
            .on_hover(&RULE_NAME_HOVER);
        if ui.button("🗑").on_hover_text("Deletes this rule.").clicked() {
            delete = true;
        }
    });
    edit_condition(ui, &mut rule.condition);
    edit_actions(ui, &rule.condition, &mut rule.actions);
    delete
}

fn edit_condition(ui: &mut egui::Ui, condition: &mut MatchCondition) {
    ui.horizontal_wrapped(|ui| {
        ui.label("When");
        let current = ConditionKind::of(condition);
        let mut kind = current;
        egui::ComboBox::from_id_salt("condition")
            .selected_text(kind.label())
            .width(90.0)
            .show_ui(ui, |ui| {
                for option in ConditionKind::ALL {
                    ui.selectable_value(&mut kind, option, option.label());
                }
            })
            .response
            .on_hover(&CONDITION_HOVER);
        if kind != current {
            *condition = kind.new_condition();
        }
        match condition {
            MatchCondition::BytePattern { pattern } => pattern_field(ui, pattern),
            MatchCondition::AnyByte { bytes } => byte_set_field(ui, bytes),
            MatchCondition::Idle { timeout_ms } => quiet_field(ui, timeout_ms),
        }
    });
}

/// The byte set as hex bytes and ranges in a box, with a Presets menu. Like
/// the quiet time, the box keeps the text as typed while it has focus, and
/// text that is not a set is flagged and changes nothing.
fn byte_set_field(ui: &mut egui::Ui, bytes: &mut Vec<u8>) {
    let typed = ui.make_persistent_id("set_typed");
    let mut text = ui
        .data(|d| d.get_temp::<String>(typed))
        .unwrap_or_else(|| byte_set_text(bytes));
    let response = ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .id_salt("byte_set")
                .desired_width(160.0)
                .hint_text("00-08 0B 0C 0E-1F 7F"),
        )
        .on_hover(&BYTE_SET_HOVER);
    if response.changed() {
        if let Some(set) = parse_byte_set(&text) {
            *bytes = set;
        }
    }
    let refused = response.has_focus() && parse_byte_set(&text).is_none();
    if response.has_focus() {
        ui.data_mut(|d| d.insert_temp(typed, text));
    } else {
        ui.data_mut(|d| d.remove::<String>(typed));
    }
    ui.menu_button("Presets", |ui| {
        for preset in [CONTROL_EXCEPT_LINE_ENDS, ALL_CONTROL, ABOVE_7F] {
            if ui.button(preset.label).clicked() {
                *bytes = preset.bytes();
                ui.close();
            }
        }
    })
    .response
    .on_hover_text("Fills the set with a common choice. You can change it afterwards.");
    if refused {
        ui.label(egui::RichText::new("⚠ hex bytes such as 1B or 00-1F").weak());
    } else if bytes.is_empty() {
        ui.label(egui::RichText::new("⚠ empty").weak());
    }
}

/// The quiet time as `[−] [seconds] [+] s`, like the port field: a box to type
/// into and buttons that step by a second, with no drag. While the box has
/// focus it keeps the text as typed, so "1." can become "1.5"; text out of
/// range is flagged and changes nothing, and the box shows the kept time again
/// once it loses focus.
fn quiet_field(ui: &mut egui::Ui, timeout_ms: &mut u64) {
    ui.horizontal(|ui| {
        if ui
            .small_button("\u{2212}")
            .on_hover_text("Makes the quiet time 1 s shorter.")
            .clicked()
        {
            *timeout_ms = step_quiet(*timeout_ms, false);
        }
        let typed = ui.make_persistent_id("quiet_typed");
        let mut text = ui
            .data(|d| d.get_temp::<String>(typed))
            .unwrap_or_else(|| quiet_text(*timeout_ms));
        let response = ui
            .add(
                egui::TextEdit::singleline(&mut text)
                    .id_salt("quiet_secs")
                    .desired_width(56.0),
            )
            .on_hover(&IDLE_HOVER);
        if response.changed() {
            if let Some(ms) = parse_quiet(&text) {
                *timeout_ms = ms;
            }
        }
        let refused = response.has_focus() && parse_quiet(&text).is_none();
        if response.has_focus() {
            ui.data_mut(|d| d.insert_temp(typed, text));
        } else {
            ui.data_mut(|d| d.remove::<String>(typed));
        }
        if ui
            .small_button("+")
            .on_hover_text("Makes the quiet time 1 s longer.")
            .clicked()
        {
            *timeout_ms = step_quiet(*timeout_ms, true);
        }
        ui.label("s");
        if refused {
            ui.label(egui::RichText::new("⚠ 0.1 to 86400 s").weak());
        }
    });
}

fn edit_actions(ui: &mut egui::Ui, condition: &MatchCondition, actions: &mut Vec<MatchAction>) {
    let mut remove = None;
    for (i, action) in actions.iter_mut().enumerate() {
        ui.push_id(("action", i), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(if i == 0 { "Then" } else { "and" });
                action_kind_selector(ui, action);
                edit_action_settings(ui, action);
                if ui
                    .button("🗑")
                    .on_hover_text("Removes this action from the rule.")
                    .clicked()
                {
                    remove = Some(i);
                }
            });
            if let MatchAction::Mark {
                timestamp: Some(timestamp),
            } = action
            {
                ui.indent("timestamp_options", |ui| {
                    ui.horizontal_wrapped(|ui| edit_timestamp_options(ui, timestamp));
                });
            }
            if timestamp_without_pattern(condition, action) {
                ui.indent("timestamp_note", |ui| {
                    ui.label(egui::RichText::new("⚠ adds nothing on a Quiet for rule").weak())
                        .on_hover(&TIMESTAMP_NEEDS_PATTERN_HOVER);
                });
            }
        });
    }
    if let Some(i) = remove {
        actions.remove(i);
    }
    ui.horizontal_wrapped(|ui| {
        if actions.is_empty() {
            ui.label(egui::RichText::new("⚠ no actions: this rule does nothing").weak());
        }
        let room = actions.len() < MAX_MATCH_ACTIONS;
        let add = ui
            .add_enabled(room, egui::Button::new("+ Add action"))
            .on_hover_text("Adds another action to this rule. It starts as a Notify.")
            .on_disabled_hover_text(format!(
                "A rule can have at most {MAX_MATCH_ACTIONS} actions."
            ));
        if add.clicked() {
            actions.push(ActionKind::Notify.new_action());
        }
    });
}

fn action_kind_selector(ui: &mut egui::Ui, action: &mut MatchAction) {
    let current = ActionKind::of(action);
    let mut kind = current;
    egui::ComboBox::from_id_salt("action_kind")
        .selected_text(kind.label())
        .width(110.0)
        .show_ui(ui, |ui| {
            for option in ActionKind::ALL {
                ui.selectable_value(&mut kind, option, option.label());
            }
        })
        .response
        .on_hover(&ACTION_HOVER);
    if kind != current {
        *action = kind.new_action();
    }
}

/// The settings on the action's own row; a timestamp's longer settings go on
/// the row under it.
fn edit_action_settings(ui: &mut egui::Ui, action: &mut MatchAction) {
    match action {
        MatchAction::Mark {
            timestamp: Some(timestamp),
        } => {
            mark_position_selector(ui, &mut timestamp.position);
            mark_style_selector(ui, &mut timestamp.style);
        }
        MatchAction::Mark { timestamp: None } | MatchAction::PauseDisplay {} => {}
        MatchAction::Record { target, control } => {
            egui::ComboBox::from_id_salt("record_control")
                .selected_text(match control {
                    RecordControl::Begin => "begin",
                    RecordControl::Stop => "stop",
                })
                .width(64.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(control, RecordControl::Begin, "begin");
                    ui.selectable_value(control, RecordControl::Stop, "stop");
                })
                .response
                .on_hover_text(
                    "Begin starts the recording at the pattern's first byte. Stop ends it \
                     just after the pattern's last byte. Either way the pattern is recorded.",
                );
            egui::ComboBox::from_id_salt("record_target")
                .selected_text(record_target_label(*target))
                .width(80.0)
                .show_ui(ui, |ui| {
                    for option in [RecordTarget::Raw, RecordTarget::Display, RecordTarget::Both] {
                        ui.selectable_value(target, option, record_target_label(option));
                    }
                })
                .response
                .on_hover_text(
                    "Which recording to begin or stop: the Raw recording (.raw), the \
                     Display recording (.disp), or both.",
                );
        }
        MatchAction::Notify { severity } => {
            egui::ComboBox::from_id_salt("notify_severity")
                .selected_text(severity_label(*severity))
                .width(80.0)
                .show_ui(ui, |ui| {
                    for option in [
                        DiagnosticSeverity::Event,
                        DiagnosticSeverity::Warning,
                        DiagnosticSeverity::Error,
                    ] {
                        ui.selectable_value(severity, option, severity_label(option));
                    }
                })
                .response
                .on_hover_text(
                    "The level of the line added to the channel's Diagnostics, named as \
                     its Info, Warn and Error filters name them.",
                );
        }
    }
}

fn record_target_label(target: RecordTarget) -> &'static str {
    match target {
        RecordTarget::Raw => "Raw",
        RecordTarget::Display => "Display",
        RecordTarget::Both => "both",
    }
}

fn severity_label(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Event => "Info",
        DiagnosticSeverity::Warning => "Warn",
        DiagnosticSeverity::Error => "Error",
    }
}

/// A timestamp's format settings and separator, on the row under its action.
fn edit_timestamp_options(ui: &mut egui::Ui, timestamp: &mut MarkTimestamp) {
    match &mut timestamp.style {
        MarkTimestampStyle::Plain {} => mark_format_toggles(ui, &mut timestamp.format),
        MarkTimestampStyle::NmeaZda { talker } => {
            mark_zda_talker_field(ui, talker);
            ui.checkbox(&mut timestamp.format.include_millis, "ms")
                .on_hover_text("Adds milliseconds to the UTC time field of the ZDA sentence.");
        }
    }
    mark_separator_field(ui, &mut timestamp.separator);
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
    let mut rest = bytes;
    while !rest.is_empty() {
        let (valid, invalid) = valid_utf8_prefix(rest);
        push_chars(&mut out, valid);
        let Some((&bad, after)) = invalid.split_first() else {
            break;
        };
        out.push_str(&format!("<{bad:02X}>"));
        rest = after;
    }
    out
}

/// The pattern field: edit the byte pattern as text. Control (and non-UTF-8)
/// bytes read and write as `<XX>` hex escapes — the view's own HexEscape
/// notation — so a CRLF-anchored pattern is typed as e.g. `GGA<0D><0A>`. A
/// non-UTF-8 pattern from a profile round-trips through edits instead of being
/// lossily replaced.
fn pattern_field(ui: &mut egui::Ui, pattern: &mut Vec<u8>) {
    let mut text = format_escaped_bytes(pattern);
    let resp = ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(120.0)
                .hint_text("pattern e.g. $GPGGA"),
        )
        .on_hover(&PATTERN_HOVER);
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
    response.on_hover(&TALKER_HOVER);
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
        .on_hover(&SEPARATOR_HOVER);
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
        .on_hover_text("Starts the timestamp with the date and a T, as in 2026-06-03T14:05:09.");
    ui.checkbox(&mut format.include_millis, "ms")
        .on_hover_text("Adds milliseconds, as in 14:05:09.123.");
    ui.checkbox(&mut format.include_timezone, "tz")
        .on_hover_text("Ends the timestamp with the local offset from UTC, such as -07:00.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_kind_makes_an_action_of_that_kind() {
        for kind in ActionKind::ALL {
            assert_eq!(ActionKind::of(&kind.new_action()), kind, "{kind:?}");
        }
    }

    #[test]
    fn a_new_condition_is_an_empty_pattern_or_five_seconds_of_quiet() {
        assert_eq!(
            ConditionKind::Pattern.new_condition(),
            MatchCondition::BytePattern {
                pattern: Vec::new()
            }
        );
        assert_eq!(
            ConditionKind::Quiet.new_condition(),
            MatchCondition::Idle { timeout_ms: 5_000 }
        );
        assert_eq!(
            ConditionKind::AnyByte.new_condition(),
            MatchCondition::AnyByte {
                bytes: CONTROL_EXCEPT_LINE_ENDS.bytes()
            }
        );
        for kind in ConditionKind::ALL {
            assert_eq!(ConditionKind::of(&kind.new_condition()), kind);
        }
    }

    /// A set shows as hex bytes, three or more in a row as a range.
    #[test]
    fn a_byte_set_shows_as_hex_bytes_and_ranges() {
        assert_eq!(byte_set_text(&[]), "");
        assert_eq!(byte_set_text(&[0x7F, 0x1B, 0x1B]), "1B 7F");
        assert_eq!(byte_set_text(&[0x0B, 0x0C]), "0B 0C");
        assert_eq!(
            byte_set_text(&CONTROL_EXCEPT_LINE_ENDS.bytes()),
            "00-08 0B 0C 0E-1F 7F"
        );
        assert_eq!(byte_set_text(&ABOVE_7F.bytes()), "80-FF");
    }

    /// Typed text is hex bytes and ranges, split by spaces or commas; text
    /// that is not changes nothing.
    #[test]
    fn typed_hex_bytes_and_ranges_make_the_set() {
        assert_eq!(parse_byte_set("1b, 7F"), Some(vec![0x1B, 0x7F]));
        assert_eq!(parse_byte_set(" 00-02 "), Some(vec![0x00, 0x01, 0x02]));
        assert_eq!(parse_byte_set("7F 00 7F"), Some(vec![0x00, 0x7F]));
        for refused in ["", "G0", "123", "1F-00", "00-", "-1F", "0x1B"] {
            assert_eq!(parse_byte_set(refused), None, "{refused:?}");
        }
        for preset in [CONTROL_EXCEPT_LINE_ENDS, ALL_CONTROL, ABOVE_7F] {
            let bytes = preset.bytes();
            assert_eq!(parse_byte_set(&byte_set_text(&bytes)), Some(bytes));
        }
    }

    #[test]
    fn the_presets_hold_the_bytes_they_name() {
        let control_except = CONTROL_EXCEPT_LINE_ENDS.bytes();
        assert!(!control_except.contains(&b'\t'));
        assert!(!control_except.contains(&b'\r'));
        assert!(!control_except.contains(&b'\n'));
        assert!(control_except.contains(&0x00) && control_except.contains(&0x7F));
        assert_eq!(control_except.len(), 30);
        assert_eq!(ALL_CONTROL.bytes().len(), 33);
        assert_eq!(ABOVE_7F.bytes(), (0x80..=0xFF).collect::<Vec<u8>>());
    }

    /// A new rule is what the old Add timestamp mark button made, and is
    /// named after its place in the list.
    #[test]
    fn a_new_rule_timestamps_a_pattern_still_to_be_typed() {
        let rule = new_rule(2);
        assert_eq!(rule.name, "rule 3");
        assert!(rule.enabled);
        assert_eq!(ConditionKind::of(&rule.condition), ConditionKind::Pattern);
        assert_eq!(
            rule.actions.iter().map(ActionKind::of).collect::<Vec<_>>(),
            [ActionKind::Timestamp]
        );
    }

    #[test]
    fn the_quiet_time_shows_in_seconds_without_trailing_zeros() {
        assert_eq!(quiet_text(5_000), "5");
        assert_eq!(quiet_text(1_500), "1.5");
        assert_eq!(quiet_text(100), "0.1");
        assert_eq!(quiet_text(123), "0.123");
    }

    /// Typed seconds are kept as milliseconds; text that is not a number in
    /// the field's range changes nothing.
    #[test]
    fn typed_seconds_in_range_set_the_quiet_time() {
        assert_eq!(parse_quiet(" 2 "), Some(2_000));
        assert_eq!(parse_quiet("1.5"), Some(1_500));
        assert_eq!(parse_quiet("0.1234"), Some(123));
        assert_eq!(parse_quiet("86400"), Some(86_400_000));
        for refused in ["", "abc", "0", "0.05", "86401", "-1", "NaN", "inf"] {
            assert_eq!(parse_quiet(refused), None, "{refused:?}");
        }
        for ms in [100, 1_500, 5_000, 86_400_000] {
            assert_eq!(parse_quiet(&quiet_text(ms)), Some(ms));
        }
    }

    /// The buttons step by a second and stop at the ends of the range.
    #[test]
    fn the_buttons_step_the_quiet_time_by_a_second_within_its_range() {
        assert_eq!(step_quiet(5_000, true), 6_000);
        assert_eq!(step_quiet(5_000, false), 4_000);
        assert_eq!(step_quiet(500, false), 100);
        assert_eq!(step_quiet(100, false), 100);
        assert_eq!(step_quiet(86_400_000, true), 86_400_000);
    }

    #[test]
    fn only_a_timestamp_on_a_quiet_rule_adds_nothing() {
        let quiet = ConditionKind::Quiet.new_condition();
        let pattern = ConditionKind::Pattern.new_condition();
        for kind in ActionKind::ALL {
            let action = kind.new_action();
            assert_eq!(
                timestamp_without_pattern(&quiet, &action),
                kind == ActionKind::Timestamp,
                "{kind:?}"
            );
            assert!(!timestamp_without_pattern(&pattern, &action));
        }
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
}
