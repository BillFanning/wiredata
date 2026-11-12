//! Inline raw-byte markers for text payloads (spec §5.3).
//!
//! In a UTF-8 or ASCII message, a non-printable byte is written as the marker
//! `‹XX›` — U+2039, two hex digits, U+203A — for example `‹1B›` for ESC. This
//! module splits such text into literal runs and marker bytes; the GUI uses
//! the same split to highlight markers as they are typed.

use std::ops::Range;

/// One piece of marker-aware text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// A run of literal text.
    Text,
    /// A raw byte written as a `‹XX›` marker.
    Byte(u8),
}

const OPEN: char = '\u{2039}'; // ‹
const CLOSE: char = '\u{203A}'; // ›

/// Total UTF-8 byte length of a `‹XX›` marker: `‹` + 2 hex digits + `›`.
const MARKER_LEN: usize = 3 + 2 + 3;

/// Split `text` into segments, each tagged with its byte range in `text`.
///
/// `‹XX›` is a raw-byte marker; any `‹` not forming a complete marker is
/// treated as literal text. Returned ranges are non-empty and fall on `char`
/// boundaries, so `&text[range]` is always valid.
pub fn segments(text: &str) -> Vec<(Range<usize>, Segment)> {
    let mut out: Vec<(Range<usize>, Segment)> = Vec::new();
    let mut literal_start = 0;
    let mut i = 0;
    while i < text.len() {
        if let Some(byte) = match_marker(text, i) {
            if i > literal_start {
                out.push((literal_start..i, Segment::Text));
            }
            let end = i + MARKER_LEN;
            out.push((i..end, Segment::Byte(byte)));
            i = end;
            literal_start = i;
        } else {
            // `i` is on a char boundary; advance past one character.
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if literal_start < text.len() {
        out.push((literal_start..text.len(), Segment::Text));
    }
    out
}

/// Make marker deletion atomic.
///
/// `prev` is the text before the user's edit, `curr` the text after. An edit
/// that removes or changes part of a complete `‹XX›` marker in `prev` leaves
/// the rest of it in `curr`; this strips that rest, so one keypress removes
/// the whole 4-character unit rather than leaving an orphan `‹` or `›`.
///
/// The edit is taken to be one replaced region: what differs between `prev`
/// and `curr` once their common start and end are set aside. For each marker
/// that region touches:
///
/// - an edit inside the marker removes the marker and whatever was typed
///   there, since text typed inside a marker belongs to nothing;
/// - an edit that runs out of the marker, at either end, removes the part of
///   the marker left behind and keeps what was typed;
/// - an edit that covers the whole marker has nothing left to remove.
///
/// Text typed at a marker's edge is outside it, and the marker is left alone.
pub fn repair_after_edit(prev: &str, curr: &mut String) {
    if prev == curr.as_str() {
        return;
    }
    let edit = EditRegion::between(prev, curr);
    // Spans of `curr` to remove, in ascending order and never overlapping:
    // a marker the edit runs out of contributes only the side that survived.
    let mut leftovers: Vec<Range<usize>> = Vec::new();
    for (marker, seg) in segments(prev) {
        if !matches!(seg, Segment::Byte(_)) {
            continue;
        }
        let touched = if edit.start == edit.prev_end {
            // Nothing was removed: text typed at either edge is outside.
            marker.start < edit.start && edit.start < marker.end
        } else {
            edit.start < marker.end && marker.start < edit.prev_end
        };
        if !touched {
            continue;
        }
        let head_left = marker.start < edit.start;
        let tail_left = edit.prev_end < marker.end;
        let starts_inside = marker.start <= edit.start;
        let ends_inside = edit.prev_end <= marker.end;
        // Where the marker's surviving tail ends in `curr`: it lies in the
        // text the two share after the edit.
        let tail_end = edit.curr_end + marker.end.saturating_sub(edit.prev_end);
        if starts_inside && ends_inside {
            if head_left || tail_left {
                leftovers.push(marker.start..tail_end);
            }
        } else if head_left {
            leftovers.push(marker.start..edit.start);
        } else if tail_left {
            leftovers.push(edit.curr_end..tail_end);
        }
    }
    for span in leftovers.into_iter().rev() {
        curr.replace_range(span, "");
    }
}

/// The one region an edit replaced: `prev[start..prev_end]` became
/// `curr[start..curr_end]`, and the text on either side is the same in both.
///
/// Every bound is on a character boundary of the text it indexes, so slicing
/// at them cannot panic.
struct EditRegion {
    start: usize,
    prev_end: usize,
    curr_end: usize,
}

impl EditRegion {
    fn between(prev: &str, curr: &str) -> Self {
        let (p, c) = (prev.as_bytes(), curr.as_bytes());
        // Two different characters can share their first bytes, so step back
        // to the start of the character the texts part in.
        let mut start = p.iter().zip(c).take_while(|(a, b)| a == b).count();
        while !prev.is_char_boundary(start) {
            start -= 1;
        }
        // Counted after `start`, so the shared end never overlaps the shared
        // start. Two characters can share their last bytes too.
        let mut shared_end = p[start..]
            .iter()
            .rev()
            .zip(c[start..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        while !prev.is_char_boundary(prev.len() - shared_end) {
            shared_end -= 1;
        }
        Self {
            start,
            prev_end: prev.len() - shared_end,
            curr_end: curr.len() - shared_end,
        }
    }
}

/// If a complete `‹XX›` marker starts at byte `i`, return its byte value.
fn match_marker(text: &str, i: usize) -> Option<u8> {
    let after_open = text[i..].strip_prefix(OPEN)?;
    let mut chars = after_open.chars();
    let h1 = chars.next()?;
    let h2 = chars.next()?;
    if !h1.is_ascii_hexdigit() || !h2.is_ascii_hexdigit() {
        return None;
    }
    // h1 and h2 are ASCII, so they occupy exactly two bytes.
    after_open[2..].strip_prefix(CLOSE)?;
    u8::from_str_radix(&after_open[..2], 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect `segments` as `(literal-or-byte)` for easy assertions.
    fn parts(text: &str) -> Vec<Segment> {
        segments(text).into_iter().map(|(_, s)| s).collect()
    }

    #[test]
    fn plain_text_is_one_literal_segment() {
        assert_eq!(parts("hello world"), vec![Segment::Text]);
    }

    #[test]
    fn empty_text_has_no_segments() {
        assert!(segments("").is_empty());
    }

    #[test]
    fn a_marker_between_text() {
        assert_eq!(
            parts("A‹0D›B"),
            vec![Segment::Text, Segment::Byte(0x0D), Segment::Text]
        );
    }

    #[test]
    fn marker_only() {
        assert_eq!(parts("‹1B›"), vec![Segment::Byte(0x1B)]);
    }

    #[test]
    fn adjacent_markers() {
        assert_eq!(
            parts("‹0D›‹0A›"),
            vec![Segment::Byte(0x0D), Segment::Byte(0x0A)]
        );
    }

    #[test]
    fn lowercase_hex_is_accepted() {
        assert_eq!(parts("‹ff›"), vec![Segment::Byte(0xFF)]);
    }

    #[test]
    fn segment_ranges_slice_the_original() {
        let text = "A‹0D›B";
        let segs = segments(text);
        assert_eq!(&text[segs[0].0.clone()], "A");
        assert_eq!(&text[segs[1].0.clone()], "‹0D›");
        assert_eq!(&text[segs[2].0.clone()], "B");
    }

    #[test]
    fn incomplete_marker_is_literal() {
        // No hex digits, one digit, non-hex digits, or no closer.
        for s in ["‹›", "‹1›", "‹XY›", "‹1B", "a ‹ b"] {
            assert_eq!(parts(s), vec![Segment::Text], "{s:?}");
        }
    }

    #[test]
    fn marker_at_end_of_multibyte_text() {
        assert_eq!(parts("café‹1B›"), vec![Segment::Text, Segment::Byte(0x1B)]);
    }

    // ── repair_after_edit ──────────────────────────────────────────────────────

    /// Simulate an edit: `prev` is the text before, and `applied(prev)`
    /// returns the text the editor produced. Run `repair_after_edit` on the
    /// result and check the final state.
    fn after_edit(prev: &str, applied: impl FnOnce(&str) -> String) -> String {
        let mut curr = applied(prev);
        repair_after_edit(prev, &mut curr);
        curr
    }

    #[test]
    fn delete_inside_marker_removes_whole_marker() {
        // Cursor between '1' and 'B' in 'A‹1B›B', press Delete — egui removes
        // the 'B'. After repair, the whole marker is gone.
        let out = after_edit("A‹1B›B", |s| s.replacen('B', "", 1));
        assert_eq!(out, "AB");
    }

    #[test]
    fn backspace_after_closing_removes_whole_marker() {
        // Cursor right after '›'; backspace removes the '›' (3 UTF-8 bytes).
        let out = after_edit("A‹1B›B", |s| s.replace('\u{203A}', ""));
        assert_eq!(out, "AB");
    }

    #[test]
    fn delete_at_opening_removes_whole_marker() {
        // Cursor right before '‹'; delete removes the '‹'.
        let out = after_edit("A‹1B›B", |s| s.replace('\u{2039}', ""));
        assert_eq!(out, "AB");
    }

    #[test]
    fn typing_inside_marker_removes_whole_marker() {
        // Cursor between '‹' and '1'; type 'X'.
        let out = after_edit("A‹1B›B", |_| "A‹X1B›B".to_string());
        assert_eq!(out, "AB");
    }

    #[test]
    fn edit_outside_marker_is_left_alone() {
        // Prepend a char — the marker is untouched and should stay intact.
        let out = after_edit("A‹1B›B", |s| format!("Z{s}"));
        assert_eq!(out, "ZA‹1B›B");
        // Append after the trailing literal.
        let out = after_edit("A‹1B›B", |s| format!("{s}Z"));
        assert_eq!(out, "A‹1B›BZ");
        // Insert between '›' and the trailing 'B'.
        let out = after_edit("A‹1B›B", |s| s.replace("›B", "›ZB"));
        assert_eq!(out, "A‹1B›ZB");
    }

    #[test]
    fn unchanged_text_is_unchanged() {
        let out = after_edit("hello ‹FF› world", |s| s.to_string());
        assert_eq!(out, "hello ‹FF› world");
    }

    #[test]
    fn replacing_from_inside_a_marker_to_past_it_keeps_what_was_typed() {
        // Select 'B›a' and type a character. The marker's head goes; the
        // typed character and the text after the selection stay. A two-byte
        // and a three-byte character put the old end-of-marker arithmetic
        // inside a character.
        for typed in ["x", "é", "€"] {
            let out = after_edit("‹1B›ab", |_| format!("‹1{typed}b"));
            assert_eq!(out, format!("{typed}b"), "typed {typed:?}");
        }
    }

    #[test]
    fn replacing_from_before_a_marker_into_it_removes_its_tail() {
        // Select 'a‹1' and type 'x': 'B›' is what is left of the marker.
        let out = after_edit("a‹1B›b", |_| "xB›b".to_string());
        assert_eq!(out, "xb");
    }

    #[test]
    fn typing_over_a_whole_marker_keeps_what_was_typed() {
        let out = after_edit("a‹1B›b", |_| "axb".to_string());
        assert_eq!(out, "axb");
    }

    #[test]
    fn an_edit_across_two_markers_leaves_neither_half() {
        // Select from inside the first marker to inside the second, type 'x'.
        let out = after_edit("‹1B›é‹0D›", |_| "‹1xD›".to_string());
        assert_eq!(out, "x");
    }

    #[test]
    fn no_single_edit_panics_or_leaves_part_of_a_marker() {
        // Every replacement of every range of the text that an editor can
        // make, with text of one, two and three bytes.
        let prev = "a‹1B›é‹0D›b";
        let boundaries: Vec<usize> = prev
            .char_indices()
            .map(|(i, _)| i)
            .chain([prev.len()])
            .collect();
        for &from in &boundaries {
            for &to in boundaries.iter().filter(|&&to| to >= from) {
                for typed in ["", "x", "é", "€"] {
                    let edited = format!("{}{typed}{}", &prev[..from], &prev[to..]);
                    let mut out = edited.clone();
                    repair_after_edit(prev, &mut out);
                    for (range, segment) in segments(&out) {
                        if segment == Segment::Text {
                            assert!(
                                !out[range].contains([OPEN, CLOSE]),
                                "{edited:?} was repaired to {out:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
