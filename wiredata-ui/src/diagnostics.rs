//! Shared presentation primitives for compact, decision-oriented diagnostics.
//!
//! The applications decide what each signal means and when it deserves attention;
//! this module owns only the identical egui chrome: a quiet card, aligned
//! signal rows, the ⚠ that marks a row wanting attention, and a restrained
//! attention callout.
//!
//! # Shared readout vocabulary
//!
//! A technician may read both applications' panels in one session, so a
//! measurement in the same *state* should be described the same way in both. The
//! domain noun differs — Talker counts scheduled sends, Listener counts read
//! chunks and rule firings — but the states, and how each one is worded, do not.
//! The columns are real renderings, so a change on one side that this table
//! cannot describe is a divergence, not a variation:
//!
//! | State | When | Talker | Listener |
//! |---|---|---|---|
//! | Awaiting first sample | nothing measured yet this run | `awaiting the first scheduled send` | `awaiting the first chunk` |
//! | Measured | the recent window has samples | `worst was 4 ms behind schedule · 812 scheduled sends reached` | `Handoff worst 4 ms · 812 chunks in ~last 10 s` |
//! | No recent samples | the run has samples, the recent window does not | `nothing sent in ~last 10 s · worst this run 9 ms` | `no chunks in ~last 10 s · Handoff worst 9 ms this run` |
//! | Expired | Talker only — see below | `recent snapshot expired · as of 11.0 s ago` | — |
//! | Final | Talker only — the exact-at-stop snapshot | `final snapshot` | — |
//!
//! Conventions that go with it:
//!
//! - **`≤`, not `<=`.** Bucketed histograms yield an upper bound, never an exact
//!   percentile, and the glyph is what says so.
//! - **Two windows, named.** A rolling window (`~last 10 s` while running,
//!   `~final 10 s` once stopped) sits beside a cumulative `run max`. Any readout
//!   showing both must label which is which.
//! - **No sample-count gate.** State the largest value with its sample count —
//!   exact, and true at one sample — and add the percentile only when
//!   `p99 < max`. There is no warming-up state in either application. (Why the
//!   twenty-sample gate was retired: talker ADR-046, listener ADR-037.)
//! - **Name the largest value for what it is.** A late send and a slow handoff
//!   are the *worst* ones, but the longest gap between two reads is just the
//!   longest — a quiet source is not a fault, and the superlative should not
//!   imply one.
//! - **No invented health thresholds.** Neither application scores latency
//!   against a budget it does not have; escalation comes from evidence that
//!   something is actually wrong (a confirmed drop, a queue at half capacity),
//!   not from a timing number being large.
//!
//! # How certain a reading is
//!
//! Both applications report things they measured directly, things they can only
//! point at, and things they cannot see at all. Each readout says which in its
//! own words — "worst", "check", "unavailable", "not charged to any send" — and
//! the wording is reviewed per readout. There is deliberately no shared
//! certainty vocabulary here; if one is ever wanted, add it with its first two
//! call sites and not before.
//!
//! The rule lives in both applications rather than here: it reads a
//! `wiredata-telemetry` histogram, and this crate depends on `egui` alone. See
//! `timing_figures` in `talker/src/gui/diagnostics.rs` and in
//! `listener/src/gui/detail/diagnostics.rs` — they are deliberately the same
//! function, and a change to one is a change to the vocabulary above.
//!
//! *Expired* and *Final* are Talker-only, and deliberately so: Talker pushes
//! collapsed snapshots that age between emissions, while Listener collapses each
//! window when a request is served. A Listener answer can still be old, because
//! when a Channel does not answer a request in time Listener serves its last
//! answer; each answer says whether it was measured for the request or is an
//! older one, with its age, and Listener's panel says so in words. That
//! asymmetry follows from the two runtime models and is recorded in talker
//! ADR-043 and listener ADR-035 — it is not a gap to be filled on either side.

use std::hash::Hash;

use egui::{Color32, Response, RichText, Ui, WidgetText};

use crate::palette::{active, tint};

/// Visual emphasis for a diagnostic signal. Semantics and thresholds remain local
/// to the application that owns the measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SignalTone {
    /// Informational or not yet measured.
    #[default]
    Neutral,
    /// The signal confirms the requested work is proceeding normally.
    Healthy,
    /// The signal needs attention but does not prove a hard failure or loss.
    Warning,
    /// The signal proves a failed operation, impossible demand, or data loss.
    Fault,
}

impl SignalTone {
    fn accent(self, ui: &Ui) -> Color32 {
        let palette = active(ui);
        match self {
            Self::Neutral => palette.idle,
            Self::Healthy => palette.running,
            Self::Warning => palette.warning,
            Self::Fault => palette.fault,
        }
    }

    fn text_color(self, ui: &Ui) -> Color32 {
        match self {
            // The shared running green is deliberately bright because it was
            // chosen for status glyphs. On the light card it is too pale for
            // small text, so calm states use the theme's foreground color.
            Self::Neutral | Self::Healthy => ui.visuals().text_color(),
            Self::Warning | Self::Fault => self.accent(ui),
        }
    }
}

fn translucent(accent: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha)
}

fn card_frame(ui: &Ui) -> egui::Frame {
    let visuals = ui.visuals();
    let neutral = visuals.widgets.noninteractive.bg_stroke.color;
    let stroke = tint(ui, neutral, 150);
    let fill = tint(ui, visuals.widgets.noninteractive.weak_bg_fill, 90);
    egui::Frame::group(ui.style())
        .fill(fill)
        .stroke(egui::Stroke::new(1.0_f32, stroke))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(11))
}

/// The diagnostic card: a framed box for the caller's rows and callouts, with
/// no header row.
///
/// It had one, a title and a status badge summing up the card in one word.
/// Every reason that badge rose for was already marked on its own row or
/// callout, so it said *that* something was wrong but not *what*, and took a
/// row to do it (talker ADR-066, listener ADR-057).
pub fn diagnostic_card<R>(
    ui: &mut Ui,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    card_frame(ui).show(ui, add_contents)
}

/// Draw a two-column signal grid inside [`diagnostic_card`]. Call
/// [`signal_row`] from `add_rows` for each decision-level signal.
pub fn signal_grid<R>(
    ui: &mut Ui,
    id_source: impl Hash,
    add_rows: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Grid::new(id_source)
        .num_columns(2)
        .spacing(egui::vec2(14.0, 5.0))
        .show(ui, add_rows)
}

/// Draw one aligned decision signal. Warning and fault tones color only the value;
/// calm values use the normal foreground so labels keep a stable hierarchy, healthy
/// text remains legible in the light theme, and the card does not become a wall of
/// status colors.
///
/// The same tooltip is attached to the label and the value. A reader who does
/// not yet know what a row measures hovers its *name* first, so leaving the
/// label inert hides the explanation exactly when it is most wanted.
pub fn signal_row(
    ui: &mut Ui,
    label: impl Into<WidgetText>,
    value: impl Into<WidgetText>,
    tone: SignalTone,
    tooltip: impl Into<WidgetText>,
) {
    let tooltip = tooltip.into();
    ui.add(
        egui::Label::new(RichText::new(label.into().text()).strong()).sense(egui::Sense::hover()),
    )
    .on_hover_text(tooltip.clone());
    let text_color = tone.text_color(ui);
    let value = format!("{}{}", tone_marker(tone), value.into().text());
    ui.add(
        egui::Label::new(RichText::new(value).color(text_color))
            .wrap()
            .sense(egui::Sense::hover()),
    )
    .on_hover_text(tooltip);
    ui.end_row();
}

/// The mark a row wears when it wants attention, or nothing when it does not.
///
/// Rows used to differ by colour alone: a warning row and a calm one were the
/// same text in two hues, which is the failure mode this module's own rule
/// forbids — colour reinforces a state, it never carries one alone.
///
/// Warning and Fault share `⚠` deliberately. What must survive without colour
/// is the binary *does this row want attention*, and both answer yes; which of
/// the two it is stays in the row's own text, where words already say it.
/// Adding a second glyph would split a distinction the reader does not have to
/// make at a glance.
pub fn tone_marker(tone: SignalTone) -> &'static str {
    match tone {
        SignalTone::Warning | SignalTone::Fault => "\u{26A0} ",
        SignalTone::Neutral | SignalTone::Healthy => "",
    }
}

/// The tinted, accented surround both callout forms are drawn in.
fn callout_frame(ui: &Ui, tone: SignalTone) -> egui::Frame {
    let accent = tone.accent(ui);
    egui::Frame::new()
        .fill(tint(ui, accent, 24))
        .stroke(egui::Stroke::new(1.0_f32, translucent(accent, 115)))
        .corner_radius(egui::CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(8, 5))
}

/// The callout's own text, marked and accented.
fn callout_label(ui: &mut Ui, text: WidgetText, tone: SignalTone) -> Response {
    ui.add(
        egui::Label::new(RichText::new(format!("\u{26A0} {}", text.text())).color(tone.accent(ui)))
            .wrap(),
    )
}

/// Draw an exceptional condition beneath the summary rows. Healthy information
/// belongs in the grid; this callout is deliberately reserved for warning/fault
/// conditions so it remains noticeable.
pub fn attention_callout(
    ui: &mut Ui,
    id_source: impl Hash,
    text: impl Into<WidgetText>,
    tone: SignalTone,
    tooltip: impl Into<WidgetText>,
) -> Response {
    debug_assert!(matches!(tone, SignalTone::Warning | SignalTone::Fault));
    let text = text.into();
    let response = ui
        .push_id(id_source, |ui| {
            callout_frame(ui, tone)
                .show(ui, |ui| callout_label(ui, text, tone))
                .inner
        })
        .inner;
    response.on_hover_text(tooltip)
}

/// [`attention_callout`] with a Dismiss button on its trailing edge. Returns
/// `true` on the frame the button is clicked; the caller owns what dismissal
/// then means, since only it knows what would make the condition worth saying
/// again.
///
/// The button is added *before* the text under a right-to-left layout, so the
/// text wraps into the width the button leaves rather than pushing it out of
/// the frame. A long routing sentence is the normal case here, not the edge one.
pub fn dismissible_attention_callout(
    ui: &mut Ui,
    id_source: impl Hash,
    text: impl Into<WidgetText>,
    tone: SignalTone,
    tooltip: impl Into<WidgetText>,
) -> bool {
    debug_assert!(matches!(tone, SignalTone::Warning | SignalTone::Fault));
    let text = text.into();
    let tooltip = tooltip.into();
    ui.push_id(id_source, |ui| {
        callout_frame(ui, tone)
            .show(ui, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    let dismissed = ui
                        .small_button("Dismiss")
                        .on_hover_text("Hide this until the condition gets worse.")
                        .clicked();
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                        callout_label(ui, text, tone).on_hover_text(tooltip);
                    });
                    dismissed
                })
                .inner
            })
            .inner
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row that wants attention says so without its colour.
    ///
    /// This is the module's own rule applied to itself: tone used to reach the
    /// reader through colour alone, so a warning row and a calm one were one
    /// string in two hues.
    #[test]
    fn only_the_tones_that_want_attention_are_marked() {
        for calm in [SignalTone::Neutral, SignalTone::Healthy] {
            assert_eq!(tone_marker(calm), "", "a calm row carries no mark");
        }
        for wants_attention in [SignalTone::Warning, SignalTone::Fault] {
            let marker = tone_marker(wants_attention);
            assert!(!marker.is_empty(), "{wants_attention:?} is colour-only");
            assert!(
                marker.starts_with('\u{26A0}'),
                "attention uses the shared ⚠, not a private symbol"
            );
        }
    }

    /// The dismissible form is the plain one plus a button, not a second
    /// visual language for the same kind of statement.
    #[test]
    fn both_callout_forms_share_one_frame() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            for tone in [SignalTone::Warning, SignalTone::Fault] {
                let frame = callout_frame(ui, tone);
                assert_eq!(frame.corner_radius, egui::CornerRadius::same(5));
                assert_eq!(frame.stroke.width, 1.0);
                assert_ne!(
                    frame.fill,
                    tone.accent(ui),
                    "the fill is a tint of the accent, never the accent itself"
                );
            }
        });
    }

    /// The Dismiss button is added before the text, under a right-to-left
    /// layout, so the text wraps into the width the button leaves. Added the
    /// other way round a long sentence pushes the button past the frame edge,
    /// and the control the reader is looking for is the part that vanishes.
    #[test]
    fn a_dismissible_callout_wraps_its_text_rather_than_displacing_the_button() {
        const LONG: &str = "Missed sends: check message #2 first — its longest send held the \
                            channel 120 ms, causing 430 ms of combined waiting across other \
                            messages in 4 sends. See Per-message timing.";

        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(320.0);
            let available = ui.available_width();

            let short = ui
                .scope(|ui| {
                    dismissible_attention_callout(
                        ui,
                        "short",
                        "Missed sends.",
                        SignalTone::Warning,
                        "",
                    );
                })
                .response
                .rect;
            let long = ui
                .scope(|ui| {
                    dismissible_attention_callout(ui, "long", LONG, SignalTone::Warning, "");
                })
                .response
                .rect;

            assert!(
                long.width() <= available + 0.5,
                "the callout grew past its width ({} > {available}), so the button is outside it",
                long.width()
            );
            assert!(
                long.height() > short.height(),
                "the long text did not wrap: {} vs {}",
                long.height(),
                short.height()
            );
        });
    }

    #[test]
    fn card_chrome_is_rounded_and_quiet() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let frame = card_frame(ui);
            assert_eq!(frame.corner_radius, egui::CornerRadius::same(8));
            assert_eq!(frame.inner_margin, egui::Margin::same(11));
            assert_eq!(frame.stroke.width, 1.0);
            assert_ne!(frame.fill, SignalTone::Warning.accent(ui));
        });
    }

    #[test]
    fn every_semantic_tone_has_a_distinct_theme_color() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let colors = [
                    SignalTone::Neutral.accent(ui),
                    SignalTone::Healthy.accent(ui),
                    SignalTone::Warning.accent(ui),
                    SignalTone::Fault.accent(ui),
                ];
                for (index, color) in colors.iter().enumerate() {
                    assert!(!colors[..index].contains(color));
                }
            });
        }
    }

    #[test]
    fn calm_text_uses_the_theme_foreground_in_both_themes() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert_eq!(
                    SignalTone::Neutral.text_color(ui),
                    ui.visuals().text_color()
                );
                assert_eq!(
                    SignalTone::Healthy.text_color(ui),
                    ui.visuals().text_color()
                );
                assert_eq!(
                    SignalTone::Warning.text_color(ui),
                    SignalTone::Warning.accent(ui)
                );
                assert_eq!(
                    SignalTone::Fault.text_color(ui),
                    SignalTone::Fault.accent(ui)
                );
            });
        }
    }
}
