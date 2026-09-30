//! Single-byte code pages for the ASCII message format (spec §5.2).
//!
//! Bytes 0x00..0x7F are plain ASCII in every code page; 0x80..0xFF differ.
//! The mapping tables are transcribed from the Unicode Consortium's vendor
//! mapping files (MICSFT/PC/CP437.TXT, MICSFT/WINDOWS/CP1252.TXT,
//! APPLE/ROMAN.TXT) and verified by the tests below.

use serde::{Deserialize, Serialize};

/// A single-byte code page selectable for the ASCII message format.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodePage {
    /// ISO-8859-1 (Latin-1) — standard on Linux/Unix.
    #[default]
    Iso8859_1,
    /// Windows-1252 — Windows Western European (ANSI).
    Windows1252,
    /// CP437 — original IBM PC / DOS character set.
    Cp437,
    /// Mac OS Roman — classic Mac OS Western European.
    MacRoman,
}

/// Encode `text` into bytes using `code_page`.
///
/// A Unicode scalar that the selected single-byte code page cannot represent
/// becomes `?` (`0x3F`), the conventional lossy-encoding fallback. Callers
/// surface replacements before sending via [`encode_char`].
pub(super) fn encode(text: &str, code_page: CodePage) -> Vec<u8> {
    text.chars()
        .map(|c| encode_char(c, code_page).unwrap_or(b'?'))
        .collect()
}

/// Decode one byte through `code_page`. Inverse of `encode_char`.
/// Every byte 0x00..=0xFF maps to exactly one `char`; the five
/// undefined bytes in Windows-1252 (0x81, 0x8D, 0x8F, 0x90, 0x9D)
/// decode to `U+FFFD`. Public so the GUI message preview can mix
/// decoded characters with its own per-byte chrome (e.g. `‹XX›`
/// markers for control bytes).
pub fn decode_byte(b: u8, code_page: CodePage) -> char {
    if b < 0x80 {
        return b as char;
    }
    let idx = (b - 0x80) as usize;
    match code_page {
        CodePage::Iso8859_1 => b as char,
        CodePage::Windows1252 => decode_windows1252(b),
        CodePage::Cp437 => CP437_HIGH[idx],
        CodePage::MacRoman => MAC_ROMAN_HIGH[idx],
    }
}

fn decode_windows1252(b: u8) -> char {
    // 0xA0..=0xFF are identical to ISO-8859-1; only 0x80..=0x9F
    // need the lookup table.
    if (0xA0..=0xFF).contains(&b) {
        return b as char;
    }
    CP1252_SPECIAL
        .iter()
        .find(|&&(byte, _)| byte == b)
        .map_or('\u{FFFD}', |&(_, c)| c)
}

pub(super) fn encode_char(c: char, code_page: CodePage) -> Option<u8> {
    let cp = c as u32;
    if cp < 0x80 {
        return Some(cp as u8);
    }
    match code_page {
        CodePage::Iso8859_1 => (cp <= 0xFF).then_some(cp as u8),
        CodePage::Windows1252 => encode_windows1252(c),
        CodePage::Cp437 => find_in_high(&CP437_HIGH, c),
        CodePage::MacRoman => find_in_high(&MAC_ROMAN_HIGH, c),
    }
}

/// Find `c` in a 0x80..0xFF mapping table and return its byte.
fn find_in_high(high: &[char; 128], c: char) -> Option<u8> {
    high.iter()
        .position(|&mapped| mapped == c)
        .map(|i| 0x80 + i as u8)
}

fn encode_windows1252(c: char) -> Option<u8> {
    let cp = c as u32;
    // 0xA0..=0xFF are identical to ISO-8859-1.
    if (0xA0..=0xFF).contains(&cp) {
        return Some(cp as u8);
    }
    // 0x80..=0x9F is a special set; five of those bytes are undefined.
    CP1252_SPECIAL
        .iter()
        .find(|&&(_, mapped)| mapped == c)
        .map(|&(byte, _)| byte)
}

/// CP437 mapping for bytes 0x80..=0xFF.
#[rustfmt::skip]
const CP437_HIGH: [char; 128] = [
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}',
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}',
    '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}',
    '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00A2}', '\u{00A3}', '\u{00A5}', '\u{20A7}', '\u{0192}',
    '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}',
    '\u{00BF}', '\u{2310}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
    '\u{03B1}', '\u{00DF}', '\u{0393}', '\u{03C0}', '\u{03A3}', '\u{03C3}', '\u{00B5}', '\u{03C4}',
    '\u{03A6}', '\u{0398}', '\u{03A9}', '\u{03B4}', '\u{221E}', '\u{03C6}', '\u{03B5}', '\u{2229}',
    '\u{2261}', '\u{00B1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00F7}', '\u{2248}',
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{207F}', '\u{00B2}', '\u{25A0}', '\u{00A0}',
];

/// Mac OS Roman mapping for bytes 0x80..=0xFF.
#[rustfmt::skip]
const MAC_ROMAN_HIGH: [char; 128] = [
    '\u{00C4}', '\u{00C5}', '\u{00C7}', '\u{00C9}', '\u{00D1}', '\u{00D6}', '\u{00DC}', '\u{00E1}',
    '\u{00E0}', '\u{00E2}', '\u{00E4}', '\u{00E3}', '\u{00E5}', '\u{00E7}', '\u{00E9}', '\u{00E8}',
    '\u{00EA}', '\u{00EB}', '\u{00ED}', '\u{00EC}', '\u{00EE}', '\u{00EF}', '\u{00F1}', '\u{00F3}',
    '\u{00F2}', '\u{00F4}', '\u{00F6}', '\u{00F5}', '\u{00FA}', '\u{00F9}', '\u{00FB}', '\u{00FC}',
    '\u{2020}', '\u{00B0}', '\u{00A2}', '\u{00A3}', '\u{00A7}', '\u{2022}', '\u{00B6}', '\u{00DF}',
    '\u{00AE}', '\u{00A9}', '\u{2122}', '\u{00B4}', '\u{00A8}', '\u{2260}', '\u{00C6}', '\u{00D8}',
    '\u{221E}', '\u{00B1}', '\u{2264}', '\u{2265}', '\u{00A5}', '\u{00B5}', '\u{2202}', '\u{2211}',
    '\u{220F}', '\u{03C0}', '\u{222B}', '\u{00AA}', '\u{00BA}', '\u{03A9}', '\u{00E6}', '\u{00F8}',
    '\u{00BF}', '\u{00A1}', '\u{00AC}', '\u{221A}', '\u{0192}', '\u{2248}', '\u{2206}', '\u{00AB}',
    '\u{00BB}', '\u{2026}', '\u{00A0}', '\u{00C0}', '\u{00C3}', '\u{00D5}', '\u{0152}', '\u{0153}',
    '\u{2013}', '\u{2014}', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '\u{00F7}', '\u{25CA}',
    '\u{00FF}', '\u{0178}', '\u{2044}', '\u{20AC}', '\u{2039}', '\u{203A}', '\u{FB01}', '\u{FB02}',
    '\u{2021}', '\u{00B7}', '\u{201A}', '\u{201E}', '\u{2030}', '\u{00C2}', '\u{00CA}', '\u{00C1}',
    '\u{00CB}', '\u{00C8}', '\u{00CD}', '\u{00CE}', '\u{00CF}', '\u{00CC}', '\u{00D3}', '\u{00D4}',
    '\u{F8FF}', '\u{00D2}', '\u{00DA}', '\u{00DB}', '\u{00D9}', '\u{0131}', '\u{02C6}', '\u{02DC}',
    '\u{00AF}', '\u{02D8}', '\u{02D9}', '\u{02DA}', '\u{00B8}', '\u{02DD}', '\u{02DB}', '\u{02C7}',
];

/// Windows-1252 mapping for the special range 0x80..=0x9F.
///
/// The five omitted bytes — 0x81, 0x8D, 0x8F, 0x90, 0x9D — are undefined.
/// Bytes 0xA0..=0xFF are identical to ISO-8859-1 and handled separately.
#[rustfmt::skip]
const CP1252_SPECIAL: &[(u8, char)] = &[
    (0x80, '\u{20AC}'), (0x82, '\u{201A}'), (0x83, '\u{0192}'), (0x84, '\u{201E}'),
    (0x85, '\u{2026}'), (0x86, '\u{2020}'), (0x87, '\u{2021}'), (0x88, '\u{02C6}'),
    (0x89, '\u{2030}'), (0x8A, '\u{0160}'), (0x8B, '\u{2039}'), (0x8C, '\u{0152}'),
    (0x8E, '\u{017D}'), (0x91, '\u{2018}'), (0x92, '\u{2019}'), (0x93, '\u{201C}'),
    (0x94, '\u{201D}'), (0x95, '\u{2022}'), (0x96, '\u{2013}'), (0x97, '\u{2014}'),
    (0x98, '\u{02DC}'), (0x99, '\u{2122}'), (0x9A, '\u{0161}'), (0x9B, '\u{203A}'),
    (0x9C, '\u{0153}'), (0x9E, '\u{017E}'), (0x9F, '\u{0178}'),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passes_through_every_code_page() {
        for cp in [
            CodePage::Iso8859_1,
            CodePage::Windows1252,
            CodePage::Cp437,
            CodePage::MacRoman,
        ] {
            assert_eq!(encode("Hello, world!", cp), b"Hello, world!");
        }
    }

    #[test]
    fn iso8859_1_maps_latin1_directly() {
        // é = U+00E9 -> 0xE9; ÿ = U+00FF -> 0xFF
        assert_eq!(encode("é", CodePage::Iso8859_1), vec![0xE9]);
        assert_eq!(encode("ÿ", CodePage::Iso8859_1), vec![0xFF]);
    }

    #[test]
    fn iso8859_1_replaces_non_latin1() {
        // Euro sign is not in Latin-1, so it uses the conventional fallback.
        assert_eq!(encode("€", CodePage::Iso8859_1), b"?");
    }

    #[test]
    fn windows1252_special_and_latin1_ranges() {
        assert_eq!(encode("€", CodePage::Windows1252), vec![0x80]);
        assert_eq!(encode("™", CodePage::Windows1252), vec![0x99]);
        assert_eq!(encode("Ÿ", CodePage::Windows1252), vec![0x9F]);
        // 0xA0..0xFF identical to Latin-1
        assert_eq!(encode("é", CodePage::Windows1252), vec![0xE9]);
    }

    #[test]
    fn cp437_known_mappings() {
        assert_eq!(encode("Ç", CodePage::Cp437), vec![0x80]);
        assert_eq!(encode("ƒ", CodePage::Cp437), vec![0x9F]);
        assert_eq!(encode("░", CodePage::Cp437), vec![0xB0]);
        assert_eq!(encode("√", CodePage::Cp437), vec![0xFB]);
    }

    #[test]
    fn mac_roman_known_mappings() {
        assert_eq!(encode("Ä", CodePage::MacRoman), vec![0x80]);
        assert_eq!(encode("†", CodePage::MacRoman), vec![0xA0]);
        assert_eq!(encode("€", CodePage::MacRoman), vec![0xDB]);
        assert_eq!(encode("ˇ", CodePage::MacRoman), vec![0xFF]);
    }

    #[test]
    fn each_unrepresentable_scalar_becomes_one_question_mark() {
        assert_eq!(encode("a中b中c日d月", CodePage::Cp437), b"a?b?c?d?");
    }

    /// Every byte 0x80..=0xFF in CP437 and Mac Roman round-trips, and each
    /// table holds 128 distinct characters (no transcription collisions).
    #[test]
    fn high_tables_are_consistent() {
        for table in [&CP437_HIGH, &MAC_ROMAN_HIGH] {
            for (i, &c) in table.iter().enumerate() {
                let byte = 0x80 + i as u8;
                assert_eq!(find_in_high(table, c), Some(byte));
            }
        }
    }

    // ── decode_byte ───────────────────────────────────────────────────────────

    #[test]
    fn decode_ascii_range_is_identity() {
        // 0x00..=0x7F decodes the same in every code page.
        for cp in [
            CodePage::Iso8859_1,
            CodePage::Windows1252,
            CodePage::Cp437,
            CodePage::MacRoman,
        ] {
            for b in 0u8..=0x7F {
                assert_eq!(decode_byte(b, cp), b as char, "{cp:?} byte {b:02X}");
            }
        }
    }

    #[test]
    fn decode_iso8859_1_high_is_identity() {
        // Latin-1 maps 0x80..=0xFF to the same codepoint as the byte
        // (0x80..=0x9F are C1 controls, 0xA0..=0xFF the printable
        // Latin-1 range).
        for b in 0x80u8..=0xFF {
            assert_eq!(decode_byte(b, CodePage::Iso8859_1), b as char);
        }
    }

    #[test]
    fn decode_cp437_high_bytes_round_trip_through_encode() {
        // Each byte 0x80..=0xFF decodes to some char, which must
        // encode back to the original byte — proves the tables are
        // mutual inverses (no transcription mismatch between encode
        // and decode).
        for b in 0x80u8..=0xFF {
            let mut s = String::new();
            s.push(decode_byte(b, CodePage::Cp437));
            assert_eq!(encode(&s, CodePage::Cp437), vec![b]);
        }
    }

    #[test]
    fn decode_windows1252_special_and_undefined() {
        // Defined special chars in 0x80..=0x9F.
        assert_eq!(decode_byte(0x80, CodePage::Windows1252), '€');
        assert_eq!(decode_byte(0x99, CodePage::Windows1252), '™');
        assert_eq!(decode_byte(0x9F, CodePage::Windows1252), 'Ÿ');
        // Undefined bytes fall back to the replacement character.
        for b in [0x81, 0x8D, 0x8F, 0x90, 0x9D] {
            assert_eq!(decode_byte(b, CodePage::Windows1252), '\u{FFFD}');
        }
        // 0xA0..=0xFF identical to Latin-1.
        assert_eq!(decode_byte(0xE9, CodePage::Windows1252), 'é');
    }

    #[test]
    fn decode_mac_roman_known_mappings() {
        assert_eq!(decode_byte(0x80, CodePage::MacRoman), 'Ä');
        assert_eq!(decode_byte(0xA0, CodePage::MacRoman), '†');
        assert_eq!(decode_byte(0xDB, CodePage::MacRoman), '€');
    }
}
