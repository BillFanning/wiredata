//! Status presentation: lifecycle/recording button decisions, the headline
//! diagnostic, serial control-line indicators, and the shared status-glyph set with
//! its optical sizing. Pure (the button/indicator decisions are unit-tested in the
//! parent's test module); only the line/glyph painters touch egui.

use super::super::state::ChannelStatus;
use wiredata_ui::palette::Palette;

/// The Start/Apply/Retry button's label and enabled state, from the channel status
/// and whether the edit draft has pending changes (§8.5). Pure decision, unit-tested;
/// the detail pane just renders the result and dispatches on click:
/// - Stopped → "Start Channel", enabled (Stopped→Starting is legal)
/// - Running + pending edits → "Apply & Restart", enabled (a coordinated restart)
/// - Running + no edits → "Start Channel", **disabled** (nothing to do)
/// - Faulted → "Retry Channel", enabled — one `CommitAndStart`; the runtime
///   normalizes Faulted→Stopped before starting (§8.5), no client-side Stop step
/// - Reconnecting → "Start Channel", **disabled** — Stop is the only valid action
///   mid-reconnect.
pub(crate) fn start_button(status: ChannelStatus, config_changed: bool) -> (&'static str, bool) {
    match status {
        ChannelStatus::Stopped => ("Start Channel", true),
        ChannelStatus::Running if config_changed => ("Apply & Restart", true),
        ChannelStatus::Running => ("Start Channel", false),
        ChannelStatus::Faulted => ("Retry Channel", true),
        ChannelStatus::Reconnecting => ("Start Channel", false),
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

/// A clickable serial output-line toggle (RTS/DTR, §161): a selectable chip
/// carrying the same level glyph as [`line_indicator`], highlighted when the
/// line is asserted. Returns the click response so the caller can send the
/// matching Set command.
pub(crate) fn line_toggle(ui: &mut egui::Ui, name: &str, high: bool) -> egui::Response {
    let color = if high {
        wiredata_ui::palette::active(ui).running
    } else {
        ui.visuals().weak_text_color()
    };
    let label = format!("{} {name}", line_glyph(high));
    ui.selectable_label(high, egui::RichText::new(label).color(color))
        .on_hover_text(format!(
            "{name} output is {} — click to set it {}",
            if high { "high" } else { "low" },
            if high { "low" } else { "high" },
        ))
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
    use super::line_glyph;

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
