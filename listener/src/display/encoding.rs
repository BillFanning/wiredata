//! Decoding received bytes into characters per the selected Display Encoding
//! (spec §47, §80.1).
//!
//! Decoding is lossy and never panics on malformed input (§119): invalid
//! sequences become the Unicode replacement character `U+FFFD` rather than
//! being dropped, so the operator still sees that *something* was there.

use super::DisplayEncoding;

/// Split `bytes` at the end of its longest valid UTF-8 prefix: the prefix as text,
/// and the rest. The rest is empty when all of `bytes` is valid; otherwise its
/// first byte is the first one that is not part of a valid sequence, so a caller
/// can handle that byte and continue with `&rest[1..]`.
pub(crate) fn valid_utf8_prefix(bytes: &[u8]) -> (&str, &[u8]) {
    let valid = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) => e.valid_up_to(),
    };
    let (prefix, rest) = bytes.split_at(valid);
    // The prefix is valid UTF-8 by construction; the fallback is unreachable.
    (std::str::from_utf8(prefix).unwrap_or(""), rest)
}

/// Decode `bytes` into characters, pairing each character with the **byte offset**
/// at which its source bytes begin (§50.2 inline annotations need to map a byte
/// offset onto the decoded character stream). The offsets are non-decreasing and the
/// first is `0`; a character produced from several bytes (UTF-8/UTF-16) reports the
/// offset of its first byte.
pub(crate) fn decode_with_offsets(bytes: &[u8], encoding: DisplayEncoding) -> Vec<(char, usize)> {
    match encoding {
        // One byte → one char: offset == index.
        DisplayEncoding::Ascii => bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| (if b <= 0x7F { b as char } else { '\u{FFFD}' }, i))
            .collect(),
        DisplayEncoding::Latin1 => bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| (b as char, i))
            .collect(),
        // UTF-8: walk the input tracking each char's starting byte offset. Lossy
        // decoding replaces an invalid byte with U+FFFD and advances by one byte, so
        // offsets stay aligned to the input.
        DisplayEncoding::Utf8 => {
            let mut out = Vec::new();
            let mut i = 0usize;
            while i < bytes.len() {
                let (valid, rest) = valid_utf8_prefix(&bytes[i..]);
                for c in valid.chars() {
                    out.push((c, i));
                    i += c.len_utf8();
                }
                if !rest.is_empty() {
                    out.push(('\u{FFFD}', i));
                    i += 1;
                }
            }
            out
        }
        DisplayEncoding::Utf16Le => decode_utf16_with_offsets(bytes, false),
        DisplayEncoding::Utf16Be => decode_utf16_with_offsets(bytes, true),
    }
}

fn decode_utf16_with_offsets(bytes: &[u8], big_endian: bool) -> Vec<(char, usize)> {
    // Decode unit-by-unit so each resulting char can carry the byte offset of its
    // first code unit. A surrogate pair (2 units → 1 char) reports the first unit's
    // offset; an unpaired surrogate or odd trailing byte becomes U+FFFD.
    let mut out = Vec::new();
    let mut i = 0usize;
    let unit_at = |i: usize| {
        let pair = [bytes[i], bytes[i + 1]];
        if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    };
    while i + 2 <= bytes.len() {
        let u = unit_at(i);
        if (0xD800..=0xDBFF).contains(&u) && i + 4 <= bytes.len() {
            let low = unit_at(i + 2);
            if (0xDC00..=0xDFFF).contains(&low) {
                let cp = 0x10000 + (((u as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                out.push((char::from_u32(cp).unwrap_or('\u{FFFD}'), i));
                i += 4;
                continue;
            }
        }
        out.push((char::from_u32(u as u32).unwrap_or('\u{FFFD}'), i));
        i += 2;
    }
    if i < bytes.len() {
        // A trailing odd byte cannot form a code unit — surface it as replacement.
        out.push(('\u{FFFD}', i));
    }
    out
}

/// How many trailing bytes of `bytes` are an **incomplete** (not invalid)
/// multi-byte sequence under `encoding` — the carry a *streaming* renderer must
/// hold back until the next chunk so a character split across two reads decodes
/// as itself instead of `U+FFFD` (§119: lossy replacement is for *invalid*
/// input, and a read boundary is not the data's fault). Always `0` for the
/// single-byte encodings; at most 3 otherwise.
pub(crate) fn incomplete_tail(bytes: &[u8], encoding: DisplayEncoding) -> usize {
    match encoding {
        DisplayEncoding::Ascii | DisplayEncoding::Latin1 => 0,
        DisplayEncoding::Utf8 => incomplete_utf8_tail(bytes),
        DisplayEncoding::Utf16Le => incomplete_utf16_tail(bytes, false),
        DisplayEncoding::Utf16Be => incomplete_utf16_tail(bytes, true),
    }
}

/// The trailing bytes of a truncated UTF-8 sequence: walk back over up to three
/// continuation bytes to a lead byte; if the lead declares more bytes than are
/// present, that whole tail is incomplete. Anything malformed (stray
/// continuations, an invalid lead, or a *complete* sequence) returns 0 — the
/// lossy decoder deals with it as data.
fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    let n = bytes.len();
    let mut i = n;
    let mut continuations = 0;
    while i > 0 && continuations < 3 && bytes[i - 1] & 0xC0 == 0x80 {
        i -= 1;
        continuations += 1;
    }
    if i == 0 {
        return 0; // nothing but continuation bytes — invalid, not incomplete
    }
    let lead = bytes[i - 1];
    // 0xC0, 0xC1 and 0xF5..: no valid sequence starts with them.
    if matches!(lead, 0xC0 | 0xC1 | 0xF5..=0xFF) {
        return 0;
    }
    let need = match lead {
        b if b & 0x80 == 0x00 => 1,
        b if b & 0xE0 == 0xC0 => 2,
        b if b & 0xF0 == 0xE0 => 3,
        b if b & 0xF8 == 0xF0 => 4,
        _ => return 0, // invalid lead — data, not a boundary artifact
    };
    let have = n - (i - 1); // lead + its continuations so far
    if need > have {
        have
    } else {
        0
    }
}

/// The trailing bytes of a truncated UTF-16 sequence: an odd leftover byte,
/// plus a lone **high** surrogate unit waiting for its low half.
fn incomplete_utf16_tail(bytes: &[u8], big_endian: bool) -> usize {
    let odd = bytes.len() % 2;
    let even = bytes.len() - odd;
    let mut tail = odd;
    if even >= 2 {
        let pair = [bytes[even - 2], bytes[even - 1]];
        let unit = if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        };
        if (0xD800..=0xDBFF).contains(&unit) {
            tail += 2;
        }
    }
    tail
}

/// Decode `bytes` into a sequence of characters under `encoding` — the character
/// stream without byte offsets. Kept as a test convenience over
/// [`decode_with_offsets`] (which production rendering uses so it can splice inline
/// annotations, §50.2).
#[cfg(test)]
pub(crate) fn decode(bytes: &[u8], encoding: DisplayEncoding) -> Vec<char> {
    decode_with_offsets(bytes, encoding)
        .into_iter()
        .map(|(c, _)| c)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_marks_high_bytes_as_replacement() {
        assert_eq!(
            decode(&[0x41, 0x80, 0x42], DisplayEncoding::Ascii),
            ['A', '\u{FFFD}', 'B']
        );
    }

    #[test]
    fn latin1_maps_every_byte() {
        // 0xE9 is 'é' in ISO-8859-1.
        assert_eq!(decode(&[0xE9], DisplayEncoding::Latin1), ['é']);
    }

    #[test]
    fn utf8_is_lossy_on_invalid_sequences() {
        // 0xFF is never valid UTF-8.
        assert_eq!(
            decode(&[0x41, 0xFF], DisplayEncoding::Utf8),
            ['A', '\u{FFFD}']
        );
    }

    #[test]
    fn offsets_track_multibyte_utf8() {
        // "aéb": 'a'@0, 'é'@1 (2 bytes), 'b'@3.
        assert_eq!(
            decode_with_offsets("aéb".as_bytes(), DisplayEncoding::Utf8),
            [('a', 0), ('é', 1), ('b', 3)]
        );
    }

    #[test]
    fn offsets_are_one_per_byte_for_ascii_and_latin1() {
        assert_eq!(
            decode_with_offsets(b"AB", DisplayEncoding::Ascii),
            [('A', 0), ('B', 1)]
        );
        assert_eq!(
            decode_with_offsets(&[0xE9, 0x41], DisplayEncoding::Latin1),
            [('é', 0), ('A', 1)]
        );
    }

    #[test]
    fn offsets_track_utf16_units_and_pairs() {
        // 'A'@0 (1 unit), then a surrogate pair 😀 (U+1F600) @2 (2 units, 4 bytes).
        let bytes = [0x41, 0x00, 0x3D, 0xD8, 0x00, 0xDE];
        assert_eq!(
            decode_with_offsets(&bytes, DisplayEncoding::Utf16Le),
            [('A', 0), ('😀', 2)]
        );
    }

    /// No valid sequence starts with these bytes, so a chunk ending in one has
    /// nothing to wait for: it decodes to U+FFFD at once.
    #[test]
    fn a_trailing_byte_that_can_never_lead_is_not_held_back() {
        for lead in [0xC0, 0xC1, 0xF5, 0xF6, 0xF7] {
            assert_eq!(incomplete_tail(&[b'a', lead], DisplayEncoding::Utf8), 0);
            assert_eq!(
                decode(&[b'a', lead], DisplayEncoding::Utf8),
                vec!['a', '\u{FFFD}']
            );
        }
        // The nearest bytes that can lead are still held back.
        assert_eq!(incomplete_tail(&[0xC2], DisplayEncoding::Utf8), 1);
        assert_eq!(incomplete_tail(&[0xF4], DisplayEncoding::Utf8), 1);
    }

    #[test]
    fn incomplete_tails_hold_back_split_sequences_only() {
        use DisplayEncoding::*;
        // UTF-8: a lead expecting more bytes than present is held back.
        assert_eq!(incomplete_tail("aé".as_bytes(), Utf8), 0); // complete
        assert_eq!(incomplete_tail(&[b'a', 0xC3], Utf8), 1); // é split after lead
        assert_eq!(incomplete_tail(&[0xE2, 0x82], Utf8), 2); // € split mid-way
        assert_eq!(incomplete_tail(&[0xF0, 0x9F, 0x98], Utf8), 3); // 😀 3 of 4
                                                                   // Malformed input is data for the lossy decoder, not a carry.
        assert_eq!(incomplete_tail(&[0x80, 0x80], Utf8), 0); // stray continuations
        assert_eq!(incomplete_tail(&[b'a', 0xFF], Utf8), 0); // invalid lead
                                                             // Single-byte encodings never carry.
        assert_eq!(incomplete_tail(&[0xC3], Ascii), 0);
        assert_eq!(incomplete_tail(&[0xC3], Latin1), 0);
        // UTF-16: odd leftover byte, and a lone high surrogate awaiting its pair.
        assert_eq!(incomplete_tail(&[0x41, 0x00, 0x42], Utf16Le), 1);
        assert_eq!(incomplete_tail(&[0x3D, 0xD8], Utf16Le), 2); // high surrogate
        assert_eq!(incomplete_tail(&[0x3D, 0xD8, 0x01], Utf16Le), 3);
        assert_eq!(incomplete_tail(&[0x41, 0x00], Utf16Le), 0); // complete 'A'
    }

    #[test]
    fn utf16_respects_byte_order_and_odd_tail() {
        // 'A' = U+0041.
        assert_eq!(decode(&[0x41, 0x00], DisplayEncoding::Utf16Le), ['A']);
        assert_eq!(decode(&[0x00, 0x41], DisplayEncoding::Utf16Be), ['A']);
        // Odd trailing byte → replacement, no panic.
        assert_eq!(
            decode(&[0x41, 0x00, 0x42], DisplayEncoding::Utf16Le),
            ['A', '\u{FFFD}']
        );
    }
}
