//! Status presentation: lifecycle/recording button decisions, the headline
//! diagnostic, serial control-line indicators, and the shared status-glyph set with
//! its optical sizing. Pure (the button/indicator decisions are unit-tested in the
//! parent's test module); only the line/glyph painters touch egui.

use super::super::state::ChannelStatus;
use crate::transport::SerialControlLines;
use wiredata_ui::hover::{Hover, ShowHover};
use wiredata_ui::palette::Palette;

/// The Start/Apply/Retry button's label and enabled state, from the channel status
/// and whether the edit draft has pending changes (§8.5). Pure decision, unit-tested;
/// the detail pane just renders the result and dispatches on click:
/// - Stopped → "Start channel", enabled (Stopped→Starting is legal)
/// - Running + pending edits → "Apply & Restart", enabled (a coordinated restart)
/// - Running + no edits → "Start channel", **disabled** (nothing to do)
/// - Faulted → "Retry channel", enabled — one `CommitAndStart`; the runtime
///   normalizes Faulted→Stopped before starting (§8.5), no client-side Stop step
/// - Reconnecting → "Start channel", **disabled** — Stop is the only valid action
///   mid-reconnect.
pub(crate) fn start_button(status: ChannelStatus, config_changed: bool) -> (&'static str, bool) {
    match status {
        ChannelStatus::Stopped => ("Start channel", true),
        ChannelStatus::Running if config_changed => ("Apply & Restart", true),
        ChannelStatus::Running => ("Start channel", false),
        ChannelStatus::Faulted => ("Retry channel", true),
        ChannelStatus::Reconnecting => ("Start channel", false),
    }
}

/// Whether the Stop button is enabled: a Stop is legal from Running, Faulted, or
/// Reconnecting (it returns any of those to Stopped, §8.5/§10.2), and illegal from
/// Stopped. Pure, unit-tested.
pub(crate) fn stop_enabled(status: ChannelStatus) -> bool {
    matches!(
        status,
        ChannelStatus::Running | ChannelStatus::Faulted | ChannelStatus::Reconnecting
    )
}

/// The recording-state indicator: glyph, color, and label for a channel's raw
/// recording state (§53). Uses the **same symbol set and colors as channel status**
/// ([`status_glyph`] / [`status_color`]) — `■` off, `●` recording, `◐` in a gap
/// and recovering, `⚠` could not begin — so the two read consistently. Pure,
/// unit-tested; the detail pane renders it as a colored label sized via
/// [`recording_glyph_size`].
pub(crate) fn recording_indicator(
    recording: Option<crate::core::RecordingState>,
    pal: &Palette,
) -> (&'static str, egui::Color32, &'static str) {
    use crate::core::{GapReason, RecordingState};
    // Same colors as channel status (`status_color`): active ● matches a Running
    // channel, a gap ◐ a Reconnecting one (on, but not writing yet), faulted ⚠
    // the fault accent, off ■ the idle grey.
    use wiredata_ui::glyphs;
    match recording {
        Some(RecordingState::Enabled) => (glyphs::RUNNING, pal.running, "recording"),
        Some(RecordingState::Gap(reason)) => (
            glyphs::RECONNECTING,
            pal.warning,
            match reason {
                GapReason::LowDisk => "gap — waiting for disk space",
                GapReason::DestinationMissing => "gap — waiting for the folder",
                GapReason::QueueOverflow | GapReason::WriteFailed | GapReason::OpenFailed => {
                    "gap — retrying"
                }
            },
        ),
        Some(RecordingState::Faulted) => (glyphs::FAULT, pal.fault, "faulted"),
        Some(RecordingState::Disabled) | None => (glyphs::STOPPED, pal.idle, "off"),
    }
}

/// One line of facts about a running recording (§56.2): the file it is writing
/// and how much is in it, the size of all its files, free space, and the time
/// to the next file when it rotates. Pure, unit-tested.
pub(crate) fn recording_facts(
    status: &crate::runtime::RecordingStatus,
    now: std::time::SystemTime,
) -> String {
    use wiredata_ui::format::human_bytes;
    let mut parts = Vec::new();
    match &status.current_file {
        Some(path) => {
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            parts.push(format!("Writing {name}"));
            parts.push(format!(
                "{} in this file",
                human_bytes(status.bytes_written)
            ));
        }
        None => parts.push("No file open".to_owned()),
    }
    if let Some(total) = status.total_size {
        parts.push(format!("{} in all its files", human_bytes(total)));
    }
    if let Some(free) = status.free_space {
        parts.push(format!("{} free", human_bytes(free)));
    }
    if let Some(next) = status.next_rotation {
        let left = next.duration_since(now).unwrap_or_default().as_secs();
        parts.push(match (left / 3600, (left % 3600) / 60) {
            (0, 0) => "next file in under a minute".to_owned(),
            (0, minutes) => format!("next file in {minutes} min"),
            (hours, minutes) => format!("next file in {hours} h {minutes} min"),
        });
    }
    parts.join(" · ")
}

/// The detail pane's status line (§9.1, ADR-045): the status word, or, while
/// the runtime retries, where the retry stands — in words, since the glyph's
/// colour alone must not carry it. Pure, unit-tested.
pub(crate) fn status_words(
    status: ChannelStatus,
    reconnect: Option<crate::runtime::ReconnectProgress>,
) -> String {
    use crate::runtime::ReconnectProgress;
    match (status, reconnect) {
        (
            ChannelStatus::Reconnecting,
            Some(ReconnectProgress::Retrying {
                attempt,
                next_try_in,
            }),
        ) => {
            // Rounded up: "next try in 0 s" would read as stuck.
            let secs = next_try_in.as_secs() + u64::from(next_try_in.subsec_nanos() > 0);
            format!(
                "Reconnecting — attempt {attempt}, next try in {}",
                wiredata_ui::format::wait(std::time::Duration::from_secs(secs))
            )
        }
        _ => status_label(status).to_owned(),
    }
}

/// A short status word for the detail pane.
pub(crate) fn status_label(status: ChannelStatus) -> &'static str {
    match status {
        ChannelStatus::Stopped => "stopped",
        ChannelStatus::Running => "running",
        ChannelStatus::Faulted => "faulted",
        ChannelStatus::Reconnecting => "reconnecting",
    }
}

/// The glyph marking a control line's level (§161).
///
/// Shape carries the level; colour reinforces it. This is the rule the channel
/// status glyphs below already follow, and the control lines are where it was
/// missed: green against grey is the classic red-green collision, so a readout
/// whose entire job is telling high from low was resting on the one cue a
/// colour-blind reader does not get.
pub(crate) fn line_glyph(high: bool) -> &'static str {
    if high {
        wiredata_ui::glyphs::LINE_HIGH
    } else {
        wiredata_ui::glyphs::LINE_LOW
    }
}

/// A serial control-line indicator (§161): the line name prefixed by a filled
/// glyph when the line is high (asserted) and a hollow one when low, coloured to
/// match, with a hover tooltip naming the level.
pub(crate) fn line_indicator(ui: &mut egui::Ui, name: &str, high: bool) {
    let pal = wiredata_ui::palette::active(ui);
    let color = if high { pal.running } else { pal.idle };
    ui.colored_label(color, format!("{} {name}", line_glyph(high)))
        .on_hover_text(if high { "high" } else { "low" });
}

/// A click on an output line's button: the line, and the level asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineClick {
    Rts(bool),
    Dtr(bool),
}

/// The serial control lines (§161) on one row, as Linemon and a Channel's
/// settings both show them: RTS and DTR as buttons that set them, then CTS,
/// DSR, DCD and RI as circles, filled when high. An output cannot be read
/// back, so its button shows what was set: low at open (ADR-073), then what a
/// click set. Under RTS/CTS flow control (`rts_by_flow`) RTS is the flow
/// control's, so it has no button.
///
/// `live` is the last reading while the port runs. With `None` the row is
/// greyed out and a sentence says why: greyed out alone would not, disabled
/// widgets show no hover, and hollow circles would read as lines that are low.
/// Returns the button clicked, if any.
pub(crate) fn serial_lines_row(
    ui: &mut egui::Ui,
    live: Option<SerialControlLines>,
    rts_by_flow: bool,
) -> Option<LineClick> {
    let lines = live.unwrap_or_default();
    let mut click = None;
    ui.add_enabled_ui(live.is_some(), |ui| {
        if rts_by_flow {
            rts_by_flow_control(ui);
        } else if ui
            .radio(lines.rts, "RTS")
            .on_hover(&output_line_hover("RTS (Request To Send)", lines.rts))
            .clicked()
        {
            click = Some(LineClick::Rts(!lines.rts));
        }
        if ui
            .radio(lines.dtr, "DTR")
            .on_hover(&output_line_hover("DTR (Data Terminal Ready)", lines.dtr))
            .clicked()
        {
            click = Some(LineClick::Dtr(!lines.dtr));
        }
        ui.separator();
        for (name, high) in [
            ("CTS", lines.cts),
            ("DSR", lines.dsr),
            ("DCD", lines.dcd),
            ("RI", lines.ri),
        ] {
            line_indicator(ui, name, high);
        }
    });
    if live.is_none() {
        ui.weak("The lines show while the port is open.");
    }
    click
}

/// The hover of an output line's button: what the line is, its level now, and
/// what a click does.
fn output_line_hover(name: &str, high: bool) -> Hover {
    let (now, then) = if high {
        ("high", "low")
    } else {
        ("low", "high")
    };
    Hover::from_paragraphs([
        format!("{name} is an output this computer sets."),
        format!("It is {now} now. Click to set it {then}."),
    ])
}

/// Whether RTS is the flow control's to set rather than the user's: under
/// RTS/CTS flow control (§161, ADR-073). Listener leaves it alone at open
/// then, and there is no button for it. Its level is known only where
/// `RTS_HIGH_UNDER_RTS_CTS` says so.
pub(crate) fn rts_set_by_flow_control(interface: &crate::config::InterfaceConfig) -> bool {
    matches!(
        interface,
        crate::config::InterfaceConfig::Serial(serial)
            if serial.flow_control == crate::config::FlowControl::RtsCts
    )
}

/// What stands in for the RTS button under RTS/CTS flow control, in words.
fn rts_by_flow_control(ui: &mut egui::Ui) {
    let (label, hover) = rts_by_flow_control_text();
    ui.label(label).on_hover_text(hover);
}

/// The words for RTS under RTS/CTS flow control, and their hover: its level
/// where that is known, and otherwise only who sets it.
fn rts_by_flow_control_text() -> (&'static str, &'static str) {
    if crate::transport::serial::RTS_HIGH_UNDER_RTS_CTS {
        (
            "RTS high, set by flow control",
            "With RTS/CTS flow control, the port opens with RTS high and keeps it high, \
             so it is not set from here.",
        )
    } else {
        (
            "RTS set by flow control",
            "With RTS/CTS flow control, the driver raises and lowers RTS to pace the \
             sender, so it is not set from here and its level is not shown.",
        )
    }
}

/// The color for a status glyph. The distinct glyphs `●`/`■`/`⚠` are the primary
/// signal and colour reinforces them, which is why this readout survived the
/// colour-deficiency problem that the control lines above did not.
/// Pure — callers pass the active theme's palette (`wiredata_ui::palette::active`),
/// the same pattern as talker's `lifecycle_indicator`.
pub(crate) fn status_color(status: ChannelStatus, pal: &Palette) -> egui::Color32 {
    match status {
        ChannelStatus::Running => pal.running,
        ChannelStatus::Stopped => pal.idle,
        ChannelStatus::Faulted => pal.fault,
        ChannelStatus::Reconnecting => pal.warning,
    }
}

/// The shared status symbol set + its size (now `wiredata_ui::glyphs` — talker
/// uses the same set), used for BOTH channel-lifecycle and raw-recording state
/// so the two read consistently:
/// - Stopped / recording-off → `■`
/// - Running / recording-on → `●`
/// - Faulted (channel or recording) → `⚠`
/// - Reconnecting → `◐`
///
/// Reconnecting used to share `●` with Running and was told apart by colour
/// alone. That is fine in the detail pane, which prints [`status_label`] beside
/// the glyph — but the channel list shows the glyph and the channel's *name*,
/// so on the one surface built for scanning many channels at once, a
/// reconnecting channel and a healthy one were the same mark in two greens-ish
/// hues. It now has its own.
///
/// Returns the glyph and the body-relative size (base scale × optical correction), so
/// every call site renders the same symbol at the same apparent size. Pair with
/// [`status_color`] (channel) or the recording color from [`recording_indicator`].
pub(crate) fn status_glyph(status: ChannelStatus) -> (&'static str, f32) {
    use wiredata_ui::glyphs;
    let glyph = match status {
        ChannelStatus::Stopped => glyphs::STOPPED,
        ChannelStatus::Faulted => glyphs::FAULT,
        ChannelStatus::Running => glyphs::RUNNING,
        ChannelStatus::Reconnecting => glyphs::RECONNECTING,
    };
    (glyph, glyphs::glyph_size(glyph))
}

/// The size for a recording-indicator glyph, matching [`status_glyph`]'s optical
/// sizing for the same symbol.
pub(crate) use wiredata_ui::glyphs::glyph_size as recording_glyph_size;

/// The fixed-cell, non-interactive glyph painter — see `wiredata_ui::glyphs`.
pub(crate) use wiredata_ui::glyphs::paint_glyph;

#[cfg(test)]
mod tests {
    use super::{line_glyph, rts_set_by_flow_control};

    #[test]
    fn rts_is_the_flow_controls_only_under_rts_cts() {
        use crate::config::{templates, FlowControl, InterfaceConfig};
        let mut serial = templates::serial_template().interface;
        for (flow, theirs) in [
            (FlowControl::None, false),
            (FlowControl::XonXoff, false),
            (FlowControl::RtsCts, true),
        ] {
            if let InterfaceConfig::Serial(config) = &mut serial {
                config.flow_control = flow;
            }
            assert_eq!(rts_set_by_flow_control(&serial), theirs, "{flow:?}");
        }
        assert!(!rts_set_by_flow_control(
            &templates::udp_template().interface
        ));
    }

    /// The two control-line levels must be distinguishable with the colour
    /// removed — that is the whole point of the glyph, and the defect it fixed.
    #[test]
    fn control_line_levels_differ_without_colour() {
        let high = line_glyph(true);
        let low = line_glyph(false);
        assert_ne!(
            high, low,
            "high and low would read identically in greyscale"
        );
        assert!(!high.is_empty() && !low.is_empty());
        // Filled against hollow: a fill difference survives at small sizes,
        // where two similar outlines would not.
        assert_eq!(high, "\u{25CF}");
        assert_eq!(low, "\u{25CB}");
    }
}
