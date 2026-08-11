//! The shared status-glyph set and its painter (talker ADR-016 / listener
//! ADR-019 — extracted once both apps needed the same symbols, like
//! `repaint`). Both apps mark channel state with the same symbols so the two
//! read as one product:
//!
//! - `●` running / active recording
//! - `■` stopped / idle
//! - `⚠` faulted / needs attention
//! - `◐` reconnecting — half-filled, so it is not `●` with a different colour
//!
//! Serial control lines reuse the same pair of circles, filled for high and
//! hollow for low, so the distinction survives without colour.
//!
//! What each *state* maps to stays app-specific. Talker represents its internal
//! retry/reopen work as running-with-error; only listener exposes Reconnecting
//! as a lifecycle state. Only the symbols, their optical sizing, and the
//! fixed-cell painter live here. Colors come from [`crate::palette`] at the call
//! site.

/// Running / active: `●`.
pub const RUNNING: &str = "\u{25CF}";
/// Stopped / idle: `■`.
pub const STOPPED: &str = "\u{25A0}";
/// Faulted / needs attention: `⚠`.
pub const FAULT: &str = "\u{26A0}";
/// Reconnecting — running, but not yet: `◐`.
///
/// Half-filled between [`RUNNING`]'s solid dot and an empty one, which is what
/// the state is. It exists because Reconnecting used to share `●` with Running
/// and was told apart by colour alone — green against amber, the pair this
/// palette is least able to rely on.
pub const RECONNECTING: &str = "\u{25D0}";

/// A serial control line that is **high** (asserted): `●`.
///
/// Deliberately the same symbol as [`RUNNING`] — an asserted line and a running
/// channel mean the same thing to a reader, and one product should not spell
/// "active" two ways. Paired with [`LINE_LOW`] it is filled against hollow,
/// which is the distinction that survives when the colour does not.
pub const LINE_HIGH: &str = "\u{25CF}";
/// A serial control line that is **low**: `○`.
///
/// Hollow, so high and low differ in *shape* and not only in colour. Green
/// against grey is the classic red-green collision, and a control-line readout
/// whose whole job is telling the two levels apart cannot rest on it.
pub const LINE_LOW: &str = "\u{25CB}";

/// The base size multiplier for status glyphs (relative to body size). The
/// square (`■`) is the reference at this size; the dot and triangle are
/// enlarged by the optical correction in [`glyph_size`] to match the
/// square's apparent size.
const STATUS_GLYPH_SCALE: f32 = 1.5;

/// Per-glyph optical correction: `●`/`○`/`◐`/`■`/`⚠` have different bounding
/// boxes, so at one font size they look different sizes. The square is the
/// reference (1.0); the circles and the triangle are enlarged so they all read
/// the same size.
///
/// Every glyph in the set needs an arm here. Falling through to the default
/// leaves a symbol at the square's scale while the circles beside it are a
/// third larger — which is how `○` shipped visibly smaller than the `●` it sits
/// next to in the same control-line readout.
fn optical_scale(glyph: &str) -> f32 {
    match glyph {
        STOPPED => 1.0,       // ■ square — the reference
        RUNNING => 1.34,      // ● dot — enlarge up to the square
        RECONNECTING => 1.34, // ◐ same circle bounding box as ●
        LINE_LOW => 1.34,     // ○ likewise: a circle is a circle, filled or not
        FAULT => 1.30,        // ⚠ triangle — enlarge up to the square
        _ => 1.0,
    }
}

/// The body-relative size for a status glyph (base scale × optical
/// correction), so every call site renders the same symbol at the same
/// apparent size.
pub fn glyph_size(glyph: &str) -> f32 {
    STATUS_GLYPH_SCALE * optical_scale(glyph)
}

/// Paint a status `glyph` into a **fixed-size, non-interactive cell**,
/// centered. Painting (rather than adding a sized label) keeps the glyph
/// from driving the row height — a taller glyph otherwise shifts the line
/// beside it. `allocate_space` reserves only layout space with no widget id,
/// so there's no stray hover/focus rectangle. `scale` is the body-relative
/// glyph size (from [`glyph_size`]); the cell is sized to the largest glyph.
pub fn paint_glyph(ui: &mut egui::Ui, glyph: &str, scale: f32, color: egui::Color32) {
    let base = egui::TextStyle::Body.resolve(ui.style()).size;
    let (_id, rect) = ui.allocate_space(egui::vec2(base * 1.5, base));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(base * scale),
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status glyph, not a fixed few — a symbol added without an optical
    /// correction falls through to the square's scale and reads visibly smaller
    /// beside the circles, which is how `○` shipped.
    ///
    /// Named for what it checks: that each glyph has a deliberate scale in a
    /// plausible band. It does **not** measure rendered bounds, so it cannot
    /// prove apparent size — that would need a font-dependent galley
    /// measurement, which is a flakier test than the eyes it would replace.
    #[test]
    fn every_status_glyph_has_an_explicit_optical_scale() {
        // The optical corrections put every glyph's effective size in a
        // narrow band around the square's reference size.
        let square = glyph_size(STOPPED);
        for g in [RUNNING, RECONNECTING, FAULT, LINE_HIGH, LINE_LOW] {
            let s = glyph_size(g);
            assert!(s > square && s < square * 1.4, "{g} out of band: {s}");
        }
    }

    #[test]
    fn unknown_glyphs_fall_back_to_the_base_scale() {
        assert_eq!(glyph_size("x"), STATUS_GLYPH_SCALE);
    }
}
