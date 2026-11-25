//! One-line previews of the bytes a message sends.

use crate::core::message::{decode_codepage_byte, CodePage};

use super::*;

// ── Byte previews ─────────────────────────────────────────────────────────────

/// Render `bytes` as a single-line preview string.
///
/// Every printable ASCII byte (`0x20..=0x7E`) is emitted as-is; **every
/// other byte** — control characters, CR/LF, anything ≥ 0x80, and the
/// individual bytes of any multi-byte UTF-8 sequence — becomes a `‹XX›`
/// marker. This guarantees that the bundled fonts can render every glyph the
/// preview emits, so nothing tofus. The tradeoff: pretty Unicode display
/// is lost in the preview — `café` shows as `caf‹C3›‹A9›` — but the user
/// can see the exact bytes that will go on the wire, which matters more
/// for a tool like this.
///
/// Embedded `\r` and `\n` therefore appear as `‹0D›‹0A›` (visible, no
/// real line break), so the preview always renders on a single line and
/// no separate trim step is needed.
pub(in crate::gui) fn preview_text(bytes: &[u8]) -> String {
    // Printable ASCII passes through; anything else (control bytes
    // and high bytes alike) is opaque to this preview, so render it
    // as the familiar `‹XX›` byte marker.
    preview_with(bytes, |b| (0x20..=0x7E).contains(&b).then_some(b as char))
}

/// Code-page preview with provenance: fallback `?` bytes receive an amber
/// background, while literal question marks remain ordinary text.
pub(in crate::gui) fn preview_ascii_layout_job(
    ui: &egui::Ui,
    bytes: &[u8],
    code_page: CodePage,
    replacement_wire_offsets: &[usize],
) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let normal = ui.visuals().text_color();
    let (replacement_text, replacement_background) = replacement_highlight_colors(ui);
    let mut job = egui::text::LayoutJob::default();

    for (offset, &byte) in bytes.iter().enumerate() {
        let text = match byte {
            0x00..=0x1F | 0x7F => format!("\u{2039}{byte:02X}\u{203A}"),
            _ => decode_codepage_byte(byte, code_page).to_string(),
        };
        let replaced = replacement_wire_offsets.binary_search(&offset).is_ok();
        job.append(
            &text,
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: if replaced { replacement_text } else { normal },
                background: if replaced {
                    replacement_background
                } else {
                    egui::Color32::TRANSPARENT
                },
                ..Default::default()
            },
        );
    }
    job
}

/// Shared byte-preview loop: emit either the caller-supplied glyph or a
/// visible `‹XX›` marker when the caller returns `None`.
fn preview_with<F: Fn(u8) -> Option<char>>(bytes: &[u8], decode: F) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match decode(b) {
            Some(c) => out.push(c),
            None => out.push_str(&format!("\u{2039}{b:02X}\u{203A}")),
        }
    }
    out
}
