//! Shared GUI **chrome** for the wiredata apps (talker ADR-016 / listener
//! ADR-019): the bundled font stack, the severity/status color palette, the
//! base widget style (both themes), compact diagnostic-card chrome, shared
//! modal dialogs, the hover-text format, and small pure formatting helpers.
//!
//! Scope is deliberately narrow — the pieces that make the two apps *look and
//! feel* like one product. App-specific widgets, layouts, and view-models stay
//! in each app. This crate depends only on `egui`; it must never depend on
//! `talker`, `listener`, `eframe`, or any runtime crate.

pub mod diagnostics;
pub mod dialog;
pub mod fonts;
pub mod format;
pub mod glyphs;
pub mod hover;
pub mod palette;
pub mod repaint;
pub mod selection;
pub mod style;

/// One-call chrome install both apps run at creation, in the required order:
/// fonts, both themes' widget visuals, then the shared style tweaks. The
/// caller still picks the startup theme with `ctx.set_theme(...)`.
pub fn install_chrome(ctx: &egui::Context) {
    fonts::install_fonts(ctx);
    style::install_visuals(ctx);
    style::apply_style_tweaks(ctx);
}
