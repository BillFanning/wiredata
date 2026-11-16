//! Linemon's display buffer (listener ADR-070, spec §50.3).
//!
//! Linemon is fed by a hidden Channel that restarts on every port or setting
//! change. Each restart is a new pipeline run: its stream offsets begin again
//! at zero and its [`StreamDelta`]s carry a new `generation`. A Channel's view
//! clears on a new generation; [`LinemonStream`] does not. It keeps one
//! continuous byte window in its **own** offset space, which only ever grows,
//! and records a marker line at each change, until the user clears it.
//!
//! The buffer holds no egui. [`LinemonStream::show`] draws it through the same
//! viewer a Channel uses, with a row cache the caller owns.

use std::collections::VecDeque;

use crate::core::{ChannelId, MatchRuleId};
use crate::display::{CharacterRendering, DisplayMode};
use crate::runtime::StreamDelta;

use super::detail::stream_view::{compose_rows, show_stream_rows, StreamInput};
use super::state::{StreamMark, MAX_STREAM_MARKS};
use super::view_prefs::ViewPrefs;
use super::StreamRenderCache;

/// The hidden Channel's pipeline run the buffer is following: its generation and
/// the next offset in **that run's** offset space.
#[derive(Clone, Copy, Debug)]
struct Run {
    generation: u64,
    cursor: u64,
}

/// What the marker text depends on: the display mode, and for Raw whether a
/// line feed is drawn as a line break.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Framing {
    mode: DisplayMode,
    chars: CharacterRendering,
}

/// The markers as the renderer is given them for one [`Framing`].
struct Framed {
    framing: Framing,
    /// `marks_version` the list was built from.
    marks_version: u64,
    /// Whether the first marker sat at the window's first byte when built (no
    /// byte before it, so no line break is needed to start its line).
    first_at_base: bool,
    /// Changes every time the list is rebuilt; this is the version the row
    /// cache sees.
    serial: u64,
    marks: Vec<StreamMark>,
}

/// Linemon's display buffer: bytes and marker lines that survive restarts of
/// the hidden Channel behind them.
pub(super) struct LinemonStream {
    bytes: VecDeque<u8>,
    /// Own offset of `bytes[0]`. Never decreases, and is never reused after
    /// [`clear`](Self::clear), so a row cache keyed on offsets cannot take new
    /// data for old.
    base_offset: u64,
    /// Marker lines, offset-sorted, each at the own offset of the byte it
    /// precedes. Unframed: the text the caller gave.
    marks: Vec<StreamMark>,
    marks_version: u64,
    /// The one id every marker carries. A marker is not a rule firing; the
    /// dedup in `ChannelView::merge_marks` never sees these, and the row cache
    /// does not look at the id.
    marker_rule: MatchRuleId,
    cap: usize,
    run: Option<Run>,
    framed: Option<Framed>,
    framed_serial: u64,
    /// Key the row cache is built under. Minted here and never entered in the
    /// main model, so it matches no Channel.
    cache_key: ChannelId,
}

impl LinemonStream {
    /// An empty buffer holding at most `cap` bytes.
    pub(super) fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            base_offset: 0,
            marks: Vec::new(),
            marks_version: 0,
            marker_rule: MatchRuleId::new(),
            cap,
            run: None,
            framed: None,
            framed_serial: 0,
            cache_key: ChannelId::new(),
        }
    }

    /// Change the byte cap; bytes past it are dropped at once, oldest first.
    pub(super) fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        self.enforce_cap();
    }

    /// Fold a delta from the hidden Channel.
    ///
    /// Within one generation the rules are those of `ChannelView`: a malformed
    /// range is refused, a duplicate or stale delta adds nothing, an overlapping
    /// one adds only its unseen suffix, and when the runtime evicted past the
    /// cursor the returned window is taken whole instead of appended across the
    /// gap. A **new** generation clears nothing: the buffer follows the new run
    /// from its `base_offset` and appends after the bytes already held.
    ///
    /// The cursor into the run is kept here only to apply those rules. The
    /// driver keeps the cursor it asks the runtime from, so none is exposed.
    pub(super) fn append(&mut self, delta: &StreamDelta) {
        let bytes: &[u8] = &delta.bytes;
        let (base, end) = (delta.base_offset, delta.end_offset);
        if end.checked_sub(base) != Some(bytes.len() as u64) {
            return;
        }
        let unseen_from = match self.run {
            Some(run) if run.generation == delta.generation => {
                if end <= run.cursor {
                    return;
                }
                // Past the cursor: the window's first byte is unseen. At or
                // before it: skip what was already folded.
                usize::try_from(run.cursor.saturating_sub(base)).unwrap_or(usize::MAX)
            }
            _ => 0,
        };
        let Some(unseen) = bytes.get(unseen_from..) else {
            return;
        };
        self.run = Some(Run {
            generation: delta.generation,
            cursor: end,
        });
        self.bytes.extend(unseen.iter().copied());
        self.enforce_cap();
    }

    /// Record a marker line, `text` as given, at the current end of the data. It
    /// is drawn on a line of its own before the next byte that arrives, or after
    /// the last byte when none has. Two changes in a row give two lines.
    pub(super) fn mark_change(&mut self, text: &str) {
        self.marks.push(StreamMark {
            offset: self.end_offset(),
            rule_id: self.marker_rule,
            before: true,
            text: text.to_string(),
        });
        if self.marks.len() > MAX_STREAM_MARKS {
            let excess = self.marks.len() - MAX_STREAM_MARKS;
            self.marks.drain(..excess);
        }
        self.marks_version += 1;
    }

    /// Drop every byte and marker. The run being followed is kept, so the same
    /// run's older bytes are not folded in again. Offsets carry on past the old
    /// end and skip one, so the first byte after a clear never sits where a row
    /// cache that kept up would take it for the next byte of the old window.
    pub(super) fn clear(&mut self) {
        self.base_offset = self.end_offset() + 1;
        self.bytes.clear();
        self.marks.clear();
        self.marks_version += 1;
    }

    /// Bytes held.
    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether no bytes are held. Markers alone do not count.
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The window as one contiguous slice.
    #[cfg(test)]
    pub(super) fn window(&mut self) -> &[u8] {
        self.bytes.make_contiguous()
    }

    /// Own offset of the window's first byte.
    #[cfg(test)]
    pub(super) fn start_offset(&self) -> u64 {
        self.base_offset
    }

    /// Own offset one past the window's last byte.
    pub(super) fn end_offset(&self) -> u64 {
        self.base_offset + self.bytes.len() as u64
    }

    /// The markers as recorded (unframed), offset-sorted.
    #[cfg(test)]
    pub(super) fn marks(&self) -> &[StreamMark] {
        &self.marks
    }

    /// Changes whenever the markers change.
    #[cfg(test)]
    pub(super) fn marks_version(&self) -> u64 {
        self.marks_version
    }

    /// Draw the buffer through the shared stream viewer, with `cache` as its
    /// row cache. The caller owns the cache, so this does not disturb the row
    /// cache of the channel display the main window draws.
    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        prefs: &ViewPrefs,
        cache: &mut Option<StreamRenderCache>,
        jump_to_bottom: bool,
    ) {
        let framing = Framing {
            mode: prefs.mode,
            chars: prefs.chars,
        };
        let note =
            (self.bytes.is_empty() && self.marks.is_empty()).then_some("no data received yet");
        self.with_input(framing, |input| {
            show_stream_rows(ui, prefs, cache, &input, note, jump_to_bottom);
        });
    }

    /// Everything in the display as one string, rows joined with `\n`, exactly
    /// as the last [`show`](Self::show) drew it with `cache`. Empty when `cache`
    /// has not been built yet.
    pub(super) fn copy_text(&mut self, cache: &Option<StreamRenderCache>) -> String {
        let Some(c) = cache.as_ref().filter(|c| c.channel == self.cache_key) else {
            return String::new();
        };
        let framing = Framing {
            mode: c.mode,
            chars: c.chars,
        };
        let drawn_to = c.rendered_cursor;
        self.refresh_framed(framing);
        let Some(framed) = self.framed.as_ref() else {
            return String::new();
        };
        let split = framed.marks.partition_point(|m| m.offset < drawn_to);
        compose_rows(cache, self.cache_key, &framed.marks[split..]).to_text()
    }

    /// Hand `f` the viewer's inputs for `framing`.
    fn with_input<R>(&mut self, framing: Framing, f: impl FnOnce(StreamInput) -> R) -> R {
        self.bytes.make_contiguous();
        self.refresh_framed(framing);
        let (window, rest) = self.bytes.as_slices();
        debug_assert!(rest.is_empty(), "make_contiguous left a split deque");
        let cursor = self.base_offset + window.len() as u64;
        let (marks, serial): (&[StreamMark], u64) = match self.framed.as_ref() {
            Some(framed) => (&framed.marks, framed.serial),
            None => (&[], 0),
        };
        let split = marks.partition_point(|m| m.offset < cursor);
        f(StreamInput {
            key: self.cache_key,
            window,
            window_start: self.base_offset,
            cursor,
            marks,
            marks_version: serial,
            trailing: &marks[split..],
        })
    }

    /// Drop bytes past the cap, then the markers whose byte went with them.
    fn enforce_cap(&mut self) {
        let overflow = self.bytes.len().saturating_sub(self.cap);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.base_offset += overflow as u64;
        }
        let evicted = self.marks.partition_point(|m| m.offset < self.base_offset);
        if evicted > 0 {
            self.marks.drain(..evicted);
            self.marks_version += 1;
        }
    }

    /// Rebuild the framed markers when something they depend on changed.
    ///
    /// A marker must land on a line of its own in every mode, and what that
    /// takes differs:
    ///
    /// - Hex: the renderer already breaks the byte row at an annotation and
    ///   gives it a row, so the text goes in as given.
    /// - Rendered and Raw: the renderer splices annotation text in verbatim, so
    ///   the text carries its own breaks. A line feed ends the text, so the
    ///   next byte starts a fresh line. A line feed also starts it, unless
    ///   nothing precedes the marker (its first byte, or right after another
    ///   marker at the same place) or the byte before it already ended the line
    ///   as drawn: a LF in Rendered, a LF in Raw with control characters passed
    ///   through. Raw shows any other rendering of LF as a glyph, which does
    ///   not end the line.
    fn refresh_framed(&mut self, framing: Framing) {
        let first_at_base = self
            .marks
            .first()
            .is_some_and(|m| m.offset <= self.base_offset);
        if let Some(f) = &self.framed {
            if f.framing == framing
                && f.marks_version == self.marks_version
                && f.first_at_base == first_at_base
            {
                return;
            }
        }
        let mut marks = Vec::with_capacity(self.marks.len());
        for (i, m) in self.marks.iter().enumerate() {
            let first_at_offset = i == 0 || self.marks[i - 1].offset != m.offset;
            let previous_byte = m
                .offset
                .checked_sub(self.base_offset + 1)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| self.bytes.get(i))
                .copied();
            let lead = first_at_offset
                && previous_byte.is_some_and(|b| match framing.mode {
                    DisplayMode::Hex => false,
                    DisplayMode::Rendered => b != b'\n',
                    DisplayMode::Raw => {
                        !(framing.chars == CharacterRendering::Native && b == b'\n')
                    }
                });
            let text = match framing.mode {
                DisplayMode::Hex => m.text.clone(),
                DisplayMode::Rendered | DisplayMode::Raw => {
                    format!("{}{}\n", if lead { "\n" } else { "" }, m.text)
                }
            };
            marks.push(StreamMark {
                offset: m.offset,
                rule_id: m.rule_id,
                before: true,
                text,
            });
        }
        self.framed_serial += 1;
        self.framed = Some(Framed {
            framing,
            marks_version: self.marks_version,
            first_at_base,
            serial: self.framed_serial,
            marks,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{DisplayEncoding, DisplayView, WrappingMode};
    use crate::gui::detail::stream_view::build_rows;

    fn delta(generation: u64, base: u64, bytes: &[u8]) -> StreamDelta {
        StreamDelta {
            generation,
            base_offset: base,
            bytes: bytes.to_vec().into(),
            end_offset: base + bytes.len() as u64,
        }
    }

    fn held(s: &mut LinemonStream) -> Vec<u8> {
        s.window().to_vec()
    }

    fn view(mode: DisplayMode) -> DisplayView {
        DisplayView {
            mode,
            encoding: DisplayEncoding::Utf8,
            character_rendering: CharacterRendering::Native,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_group: 1,
            hex_bytes_per_line: 16,
        }
    }

    /// The rows the display shows for `s`, through a fresh row cache.
    fn rows(s: &mut LinemonStream, mode: DisplayMode) -> Vec<String> {
        let mut cache = None;
        rows_with(s, &mut cache, mode)
    }

    fn rows_with(
        s: &mut LinemonStream,
        cache: &mut Option<StreamRenderCache>,
        mode: DisplayMode,
    ) -> Vec<String> {
        let framing = Framing {
            mode,
            chars: CharacterRendering::Native,
        };
        let v = view(mode);
        s.with_input(framing, |input| {
            let r = build_rows(cache, &input, &v, 80);
            (0..r.len()).filter_map(|i| r.get(i).cloned()).collect()
        })
    }

    fn strings(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|r| r.to_string()).collect()
    }

    /// The rows for two CR LF terminated sentences after a marker.
    fn sentence_rows(mode: DisplayMode, chars: CharacterRendering) -> Vec<String> {
        let mut s = LinemonStream::new(1024);
        s.mark_change(MARKER);
        s.append(&delta(1, 0, b"$GPGGA,1\r\n$GPRMC,2\r\n"));
        let mut v = view(mode);
        v.character_rendering = chars;
        let mut cache = None;
        s.with_input(Framing { mode, chars }, |input| {
            let r = build_rows(&mut cache, &input, &v, 80);
            (0..r.len()).filter_map(|i| r.get(i).cloned()).collect()
        })
    }

    #[test]
    fn line_ends_are_shown_in_every_view() {
        use CharacterRendering::Glyph;
        // Hex shows the two bytes.
        let hex = sentence_rows(DisplayMode::Hex, Glyph).join(" ");
        assert_eq!(hex.matches("0D 0A").count(), 2, "{hex}");
        // Rendered ends the line at them.
        assert_eq!(
            sentence_rows(DisplayMode::Rendered, Glyph),
            strings(&[MARKER, "$GPGGA,1", "$GPRMC,2", ""])
        );
        // Raw shows them as control pictures and does not end the line.
        assert_eq!(
            sentence_rows(DisplayMode::Raw, Glyph),
            strings(&[MARKER, "$GPGGA,1\u{240d}\u{240a}$GPRMC,2\u{240d}\u{240a}"])
        );
    }

    const MARKER: &str = "— COM4, 9600 8N1 —";

    /// Two runs of the hidden Channel with a marker at the join.
    fn two_runs(first: &[u8], second: &[u8]) -> LinemonStream {
        let mut s = LinemonStream::new(1024);
        s.append(&delta(1, 0, first));
        s.mark_change(MARKER);
        s.append(&delta(2, 0, second));
        s
    }

    #[test]
    fn two_generations_are_kept_in_order_with_nothing_cleared() {
        let mut s = LinemonStream::new(1024);
        s.append(&delta(1, 0, b"alpha"));
        s.append(&delta(2, 0, b"beta"));
        assert_eq!(held(&mut s), b"alphabeta");
        assert_eq!((s.start_offset(), s.end_offset(), s.len()), (0, 9, 9));
    }

    #[test]
    fn a_marker_between_generations_sits_exactly_at_the_join() {
        let mut s = two_runs(b"alpha", b"beta");
        assert_eq!(s.marks().len(), 1);
        assert_eq!(s.marks()[0].offset, 5);
        assert_eq!(s.marks()[0].text, MARKER);
        assert_eq!(held(&mut s), b"alphabeta");
    }

    #[test]
    fn two_markers_without_data_between_both_survive_in_order() {
        let mut s = LinemonStream::new(1024);
        s.append(&delta(1, 0, b"abc"));
        s.mark_change("first");
        s.mark_change("second");
        let texts: Vec<_> = s.marks().iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(s.marks()[0].offset, s.marks()[1].offset);
        assert_eq!(
            rows(&mut s, DisplayMode::Rendered),
            strings(&["abc", "first", "second", ""])
        );
        s.append(&delta(2, 0, b"def"));
        assert_eq!(
            rows(&mut s, DisplayMode::Rendered),
            strings(&["abc", "first", "second", "def"])
        );
    }

    #[test]
    fn clear_empties_and_later_data_starts_fresh_with_larger_offsets() {
        let mut s = two_runs(b"alpha", b"beta");
        let end_before = s.end_offset();
        s.clear();
        assert!(s.is_empty());
        assert!(s.marks().is_empty());
        assert_eq!(rows(&mut s, DisplayMode::Rendered), strings(&[""]));
        s.append(&delta(2, 4, b"gamma"));
        assert_eq!(held(&mut s), b"gamma");
        assert!(s.start_offset() > end_before);
        assert_eq!(s.end_offset(), s.start_offset() + 5);
        assert_eq!(rows(&mut s, DisplayMode::Rendered), strings(&["gamma"]));
    }

    #[test]
    fn a_row_cache_does_not_mistake_data_after_clear_for_old_data() {
        let mut s = LinemonStream::new(1024);
        let mut cache = None;
        s.append(&delta(1, 0, b"alpha"));
        assert_eq!(
            rows_with(&mut s, &mut cache, DisplayMode::Rendered),
            strings(&["alpha"])
        );
        s.clear();
        s.append(&delta(1, 5, b"bravo"));
        assert_eq!(
            rows_with(&mut s, &mut cache, DisplayMode::Rendered),
            strings(&["bravo"])
        );
    }

    #[test]
    fn the_cap_drops_the_oldest_bytes_and_the_markers_that_went_with_them() {
        let mut s = LinemonStream::new(6);
        s.append(&delta(1, 0, b"ab"));
        s.mark_change("early");
        s.append(&delta(1, 2, b"cdef"));
        s.mark_change("late");
        assert_eq!(held(&mut s), b"abcdef");
        assert_eq!(s.marks().len(), 2);
        let version = s.marks_version();
        s.append(&delta(1, 6, b"ghi"));
        assert_eq!(held(&mut s), b"defghi");
        assert_eq!(s.start_offset(), 3);
        let texts: Vec<_> = s.marks().iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["late"]);
        assert_ne!(s.marks_version(), version);
    }

    #[test]
    fn lowering_the_cap_trims_at_once() {
        let mut s = LinemonStream::new(100);
        s.append(&delta(1, 0, b"abcdef"));
        s.set_cap(2);
        assert_eq!(held(&mut s), b"ef");
        assert_eq!(s.start_offset(), 4);
    }

    #[test]
    fn the_marker_count_is_capped() {
        let mut s = LinemonStream::new(1024);
        for i in 0..MAX_STREAM_MARKS + 5 {
            s.mark_change(&format!("m{i}"));
        }
        assert_eq!(s.marks().len(), MAX_STREAM_MARKS);
        assert_eq!(s.marks()[0].text, "m5");
    }

    #[test]
    fn a_stale_or_duplicate_delta_adds_nothing() {
        let mut s = LinemonStream::new(1024);
        s.append(&delta(1, 0, b"alpha"));
        s.append(&delta(1, 5, b"bravo"));
        s.append(&delta(1, 0, b"alpha"));
        s.append(&delta(1, 5, b"bravo"));
        s.append(&delta(1, 3, b"ha"));
        assert_eq!(held(&mut s), b"alphabravo");
    }

    #[test]
    fn an_overlapping_delta_adds_only_its_unseen_suffix() {
        let mut s = LinemonStream::new(1024);
        s.append(&delta(1, 0, b"alpha"));
        s.append(&delta(1, 2, b"phabravo"));
        assert_eq!(held(&mut s), b"alphabravo");
        assert_eq!(s.end_offset(), 10);
    }

    #[test]
    fn an_eviction_gap_inside_a_run_continues_from_the_returned_window() {
        let mut s = LinemonStream::new(1024);
        s.append(&delta(1, 0, b"alpha"));
        s.append(&delta(1, 20, b"zulu"));
        assert_eq!(held(&mut s), b"alphazulu");
        // The run's cursor moved to the window's end, so the next read follows on.
        s.append(&delta(1, 24, b"!"));
        assert_eq!(held(&mut s), b"alphazulu!");
    }

    #[test]
    fn a_malformed_delta_is_ignored() {
        let mut s = LinemonStream::new(1024);
        s.append(&delta(1, 0, b"abc"));
        let mut bad = delta(1, 3, b"def");
        bad.end_offset = 99;
        s.append(&bad);
        let mut backwards = delta(2, 10, b"x");
        backwards.end_offset = 5;
        s.append(&backwards);
        assert_eq!(held(&mut s), b"abc");
        // A rejected delta of a new generation must not change the run followed.
        s.append(&delta(1, 3, b"def"));
        assert_eq!(held(&mut s), b"abcdef");
    }

    #[test]
    fn a_new_generation_with_a_nonzero_base_is_followed_from_there() {
        let mut s = LinemonStream::new(1024);
        s.append(&delta(1, 0, b"abc"));
        s.append(&delta(2, 7, b"def"));
        assert_eq!(held(&mut s), b"abcdef");
        s.append(&delta(2, 10, b"ghi"));
        assert_eq!(held(&mut s), b"abcdefghi");
        // The older run's bytes are stale to the new one, and not folded in again.
        s.append(&delta(2, 7, b"def"));
        assert_eq!(held(&mut s), b"abcdefghi");
    }

    #[test]
    fn rows_after_two_generations_have_the_marker_on_its_own_line() {
        let want = [
            (DisplayMode::Raw, strings(&["alpha", MARKER, "beta"])),
            (DisplayMode::Rendered, strings(&["alpha", MARKER, "beta"])),
            (
                DisplayMode::Hex,
                strings(&["61 6C 70 68 61", MARKER, "62 65 74 61"]),
            ),
        ];
        for (mode, expected) in want {
            let mut s = two_runs(b"alpha", b"beta");
            assert_eq!(rows(&mut s, mode), expected, "{mode:?}");
            let mut cache = None;
            rows_with(&mut s, &mut cache, mode);
            assert_eq!(s.copy_text(&cache), expected.join("\n"), "{mode:?}");
        }
    }

    #[test]
    fn a_marker_after_a_line_end_does_not_leave_a_blank_row() {
        let want = [
            (DisplayMode::Raw, strings(&["alpha", MARKER, "beta"])),
            (DisplayMode::Rendered, strings(&["alpha", MARKER, "beta"])),
            (
                DisplayMode::Hex,
                strings(&["61 6C 70 68 61 0A", MARKER, "62 65 74 61"]),
            ),
        ];
        for (mode, expected) in want {
            let mut s = two_runs(b"alpha\n", b"beta");
            assert_eq!(rows(&mut s, mode), expected, "{mode:?}");
        }
    }

    #[test]
    fn a_marker_with_no_data_after_it_is_drawn_at_once_and_stays_put() {
        for mode in [DisplayMode::Raw, DisplayMode::Rendered, DisplayMode::Hex] {
            let mut waiting = LinemonStream::new(1024);
            waiting.append(&delta(1, 0, b"alpha"));
            waiting.mark_change(MARKER);
            let before = rows(&mut waiting, mode);
            assert!(before.contains(&MARKER.to_string()), "{mode:?}");
            assert_eq!(before.last().map(String::as_str), Some(""), "{mode:?}");

            // Once a byte arrives the marker rows are those drawn while waiting.
            waiting.append(&delta(2, 0, b"beta"));
            let after = rows(&mut waiting, mode);
            assert_eq!(after[..before.len() - 1], before[..before.len() - 1]);
            assert_eq!(after, rows(&mut two_runs(b"alpha", b"beta"), mode));
        }
    }

    #[test]
    fn a_marker_in_an_empty_display_is_drawn() {
        let mut s = LinemonStream::new(1024);
        s.mark_change(MARKER);
        for mode in [DisplayMode::Raw, DisplayMode::Rendered, DisplayMode::Hex] {
            assert_eq!(rows(&mut s, mode), strings(&[MARKER, ""]), "{mode:?}");
        }
    }

    #[test]
    fn the_incremental_cache_matches_a_fresh_one() {
        for mode in [DisplayMode::Raw, DisplayMode::Rendered, DisplayMode::Hex] {
            let mut s = LinemonStream::new(1024);
            let mut cache = None;
            s.append(&delta(1, 0, b"alpha"));
            rows_with(&mut s, &mut cache, mode);
            s.mark_change("one");
            rows_with(&mut s, &mut cache, mode);
            s.mark_change("two");
            s.append(&delta(2, 0, b"beta"));
            rows_with(&mut s, &mut cache, mode);
            s.append(&delta(2, 4, b"gamma"));
            let incremental = rows_with(&mut s, &mut cache, mode);
            assert_eq!(incremental, rows(&mut s, mode), "{mode:?}");
        }
    }
}
