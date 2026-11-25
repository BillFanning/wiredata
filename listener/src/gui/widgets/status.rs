//! Status presentation: lifecycle/recording button decisions, the headline
//! diagnostic, and the shared status-glyph set with its optical sizing. Pure
//! (the button/indicator decisions are unit-tested in the parent's test module);
//! only the glyph painter touches egui.

use super::super::state::ChannelStatus;
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

/// The color for a status glyph. The distinct glyphs `●`/`■`/`⚠` are the primary
/// signal and colour reinforces them.
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
    use super::rts_set_by_flow_control;

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
}
