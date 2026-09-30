//! Font stack for the GUIs: the bundled UI/monospace faces, their fallback
//! chains, and the helpers the apps use to render bold titles and pick a
//! message-view typeface. See `assets/fonts/README.md` for the bundled files.
//!
//! Moved verbatim from `listener/src/gui/fonts.rs` (talker ADR-016 / listener
//! ADR-019) so both apps install the identical stack.

/// Selectable monospace faces for the message view: (display label, family key,
/// font bytes). Each becomes an egui `FontFamily::Name` so the view can switch faces
/// per render. `Hack` (egui's built-in monospace) is offered separately, no entry
/// here. `mono_dejavu` doubles as the wide-coverage fallback for the others.
const MONO_FONTS: &[(&str, &str, &[u8])] = &[
    (
        "Cascadia Mono",
        "mono_cascadia",
        include_bytes!("../assets/fonts/CascadiaMono.ttf"),
    ),
    (
        "JetBrains Mono",
        "mono_jetbrains",
        include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    ),
    (
        "DejaVu Sans Mono",
        "mono_dejavu",
        include_bytes!("../assets/fonts/DejaVuSansMono.ttf"),
    ),
];

/// Build the whole font stack in one place (clearer and less error-prone than
/// incremental `add_font` with cross-family append ordering). Each family's fallback
/// chain is spelled out explicitly:
/// - **Proportional** (the UI): Noto Sans, then script + symbol + control fallbacks.
/// - **Monospace** (built-in Hack) and every selectable mono face: the face → DejaVu
///   (widest mono, so missing glyphs stay *monospace* and columns stay aligned) →
///   Cascadia (its control pictures, ␊ ␍ …) → Noto Sans (last resort; proportional,
///   may misalign) → symbols.
/// - **`ui_bold`**: Noto Sans Bold for genuinely bold titles, with regular fallbacks.
///
/// CJK is not bundled. See `assets/fonts/README.md`.
pub fn install_fonts(ctx: &egui::Context) {
    use egui::FontFamily::{Monospace, Name, Proportional};
    use std::sync::Arc;

    let mut f = egui::FontDefinitions::default();
    fn reg(f: &mut egui::FontDefinitions, name: &str, bytes: &'static [u8]) {
        f.font_data.insert(
            name.to_owned(),
            Arc::new(egui::FontData::from_static(bytes)),
        );
    }
    reg(
        &mut f,
        "noto_sans",
        include_bytes!("../assets/fonts/NotoSans-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_bold",
        include_bytes!("../assets/fonts/NotoSans-Bold.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_symbols2",
        include_bytes!("../assets/fonts/NotoSansSymbols2-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_thai",
        include_bytes!("../assets/fonts/NotoSansThai-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_arabic",
        include_bytes!("../assets/fonts/NotoSansArabic-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_hebrew",
        include_bytes!("../assets/fonts/NotoSansHebrew-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_devanagari",
        include_bytes!("../assets/fonts/NotoSansDevanagari-Regular.ttf"),
    );
    for &(_label, key, bytes) in MONO_FONTS {
        reg(&mut f, key, bytes);
    }

    let scripts = [
        "noto_sans_symbols2",
        "noto_sans_thai",
        "noto_sans_arabic",
        "noto_sans_hebrew",
        "noto_sans_devanagari",
    ];

    // Proportional: Noto Sans wins, then Cascadia (for control pictures) + scripts.
    let prop = f.families.entry(Proportional).or_default();
    prop.insert(0, "noto_sans".to_owned());
    prop.push("mono_cascadia".to_owned());
    prop.extend(scripts.iter().map(|s| s.to_string()));

    // Mono fallback tail shared by the built-in Monospace and the named faces.
    // Cascadia supplies the control pictures (␊ ␍ …); a full face, so no separate
    // subset is bundled. Neither it nor DejaVu is listed under its own family.
    let mono_tail = |face_self: Option<&str>| {
        let mut v: Vec<String> = Vec::new();
        if face_self != Some("mono_dejavu") {
            v.push("mono_dejavu".to_owned()); // widest-coverage mono fallback
        }
        if face_self != Some("mono_cascadia") {
            v.push("mono_cascadia".to_owned()); // control pictures
        }
        v.push("noto_sans".to_owned());
        v.extend(scripts.iter().map(|s| s.to_string()));
        v
    };

    // Built-in Monospace (Hack) keeps its defaults, then the shared tail.
    let mono = f.families.entry(Monospace).or_default();
    mono.extend(mono_tail(None));

    // Each selectable mono face: face first, then the shared tail.
    for &(_label, key, _bytes) in MONO_FONTS {
        let mut chain = vec![key.to_owned()];
        chain.extend(mono_tail(Some(key)));
        f.families.insert(Name(key.into()), chain);
    }

    // Bold UI titles: real bold weight, with regular + symbol fallbacks so a stray
    // glyph never tofus.
    f.families.insert(
        Name("ui_bold".into()),
        vec![
            "noto_sans_bold".to_owned(),
            "noto_sans".to_owned(),
            "noto_sans_symbols2".to_owned(),
            "mono_cascadia".to_owned(),
        ],
    );

    ctx.set_fonts(f);
}

/// A label in a genuine bold weight (the `ui_bold` family). egui `.strong()` only
/// recolors, so titles use this for real boldface.
pub fn bold(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).family(egui::FontFamily::Name("ui_bold".into()))
}

/// A selectable message-view monospace face. `Hack` is egui's built-in monospace;
/// the rest are bundled (see `MONO_FONTS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonoFont {
    Hack,
    Cascadia,
    JetBrains,
    DejaVu,
}

impl MonoFont {
    pub const ALL: &'static [MonoFont] = &[
        MonoFont::Hack,
        MonoFont::Cascadia,
        MonoFont::JetBrains,
        MonoFont::DejaVu,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MonoFont::Hack => "Hack",
            MonoFont::Cascadia => "Cascadia Mono",
            MonoFont::JetBrains => "JetBrains Mono",
            MonoFont::DejaVu => "DejaVu Sans Mono",
        }
    }

    pub fn family(self) -> egui::FontFamily {
        match self {
            MonoFont::Hack => egui::FontFamily::Monospace,
            MonoFont::Cascadia => egui::FontFamily::Name("mono_cascadia".into()),
            MonoFont::JetBrains => egui::FontFamily::Name("mono_jetbrains".into()),
            MonoFont::DejaVu => egui::FontFamily::Name("mono_dejavu".into()),
        }
    }

    /// A stable token for persisting the choice in a profile. Round-trips with
    /// [`from_name`](Self::from_name).
    pub fn name(self) -> &'static str {
        match self {
            MonoFont::Hack => "hack",
            MonoFont::Cascadia => "cascadia",
            MonoFont::JetBrains => "jetbrains",
            MonoFont::DejaVu => "dejavu",
        }
    }

    /// Parse a persisted token back to a face; unknown/None tokens fall back to the
    /// default (Cascadia).
    pub fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("hack") => MonoFont::Hack,
            Some("jetbrains") => MonoFont::JetBrains,
            Some("dejavu") => MonoFont::DejaVu,
            _ => MonoFont::Cascadia,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_font_name_round_trips() {
        for &font in MonoFont::ALL {
            assert_eq!(MonoFont::from_name(Some(font.name())), font);
        }
    }

    #[test]
    fn unknown_or_missing_token_falls_back_to_cascadia() {
        assert_eq!(MonoFont::from_name(None), MonoFont::Cascadia);
        assert_eq!(MonoFont::from_name(Some("comic-sans")), MonoFont::Cascadia);
    }
}
