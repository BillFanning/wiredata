//! Shared field renderers and small pure helpers for the talker GUI, split
//! out of `mod.rs` in the master–detail restructure (spec §3.2). Everything
//! here is either a stateless `fn(ui, …)` widget or a pure validation /
//! formatting helper — app state stays in [`super::TalkerApp`].

mod byte_preview;
mod display_pane;
mod insert_popups;
mod message_text;
mod serial_fields;
mod start_blockers;
mod status;
mod udp_fields;
mod validation;

use wiredata_ui::serial::LineClick;

// ── Interface field editors (Serial / UDP) ───────────────────────────────────

/// What an interface editor reports for one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::gui) struct InterfaceEdit {
    /// A value was committed. A running channel takes it at once.
    pub changed: bool,
    /// The serial port list should be read again.
    pub refresh_ports: bool,
    /// This computer's network address list should be read again.
    pub refresh_addresses: bool,
    /// An output line's button was clicked, on a serial channel.
    pub click: Option<LineClick>,
}

/// Foreground/background pair for a lossy code-page substitution. The shared
/// palette's amber is a readable foreground in light mode but too dark behind
/// black text, so the light theme uses a pale amber field with dark-brown text.
fn replacement_highlight_colors(ui: &egui::Ui) -> (egui::Color32, egui::Color32) {
    if ui.visuals().dark_mode {
        (
            egui::Color32::BLACK,
            wiredata_ui::palette::active(ui).warning,
        )
    } else {
        (
            egui::Color32::from_rgb(45, 30, 0),
            egui::Color32::from_rgb(255, 225, 150),
        )
    }
}

pub(in crate::gui) use byte_preview::*;
pub(in crate::gui) use display_pane::*;
pub(in crate::gui) use insert_popups::*;
pub(in crate::gui) use message_text::*;
pub(in crate::gui) use serial_fields::*;
pub(in crate::gui) use start_blockers::*;
pub(in crate::gui) use status::*;
pub(in crate::gui) use udp_fields::*;
pub(in crate::gui) use validation::*;

#[cfg(test)]
mod test_support {

    // ── The narrow detail pane ────────────────────────────────────────────────

    /// What a pane of some width shows of an editor drawn into it.
    pub(super) struct Pane {
        /// Rows of controls the editor took.
        pub(super) rows: usize,
        /// The right edge the editor reached.
        pub(super) drawn_right: f32,
        /// The right edge the pane had.
        pub(super) room_right: f32,
    }

    impl Pane {
        pub(super) fn fits(&self) -> bool {
            self.drawn_right <= self.room_right + 0.5
        }
    }

    /// Draw an editor into a `width`-point pane, as the detail pane does.
    pub(super) fn in_pane(width: f32, mut draw: impl FnMut(&mut egui::Ui)) -> Pane {
        let ctx = egui::Context::default();
        let mut pane = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(width);
            // Before drawing: a Ui grows its max rect to take in what overflows it.
            let room_right = ui.max_rect().right();
            draw(ui);
            let used = ui.min_rect();
            let row = ui.spacing().interact_size.y;
            let gap = ui.spacing().item_spacing.y;
            pane = Some(Pane {
                rows: ((used.height() + gap) / (row + gap)).round() as usize,
                drawn_right: used.right(),
                room_right,
            });
        });
        pane.expect("the frame ran")
    }
}
