//! Raw, Rendered, and Hex rendering plus the four character-rendering modes
//! and wrapping (spec §42–§46), and the configured [`DisplayView`] renderer.

use super::encoding::decode_with_offsets;
use super::{CharacterRendering, DisplayEncoding, DisplayMode, WrappingMode};

/// Where an inline annotation string is spliced relative to the byte it targets
/// (§50.2 Mark timestamps). Renderer-local so the pure display layer does not depend
/// on `config`; the pipeline maps `config::MarkPosition` onto this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationPlacement {
    /// Splice immediately before the target byte's rendering.
    Before,
    /// Splice immediately after the target byte's rendering.
    After,
}

/// An inline text annotation to splice into rendered output at a byte offset
/// (§50.2). `offset` is a byte index into the *rendered span's* bytes (0-based).
#[derive(Clone, Debug)]
pub struct RenderAnnotation {
    pub offset: usize,
    pub placement: AnnotationPlacement,
    pub text: String,
}

/// Bracketed ASCII control tokens for 0x00..=0x1F (index == code point),
/// pre-formed so Token rendering never allocates per character.
const CONTROL_TOKENS: [&str; 32] = [
    "[NUL]", "[SOH]", "[STX]", "[ETX]", "[EOT]", "[ENQ]", "[ACK]", "[BEL]", "[BS]", "[TAB]",
    "[LF]", "[VT]", "[FF]", "[CR]", "[SO]", "[SI]", "[DLE]", "[DC1]", "[DC2]", "[DC3]", "[DC4]",
    "[NAK]", "[SYN]", "[ETB]", "[CAN]", "[EM]", "[SUB]", "[ESC]", "[FS]", "[GS]", "[RS]", "[US]",
];

/// Uppercase hex digits for direct two-digit emission (no `format!`).
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

/// Whether a character is "special" — a control character or the space — and so
/// gets replaced by the Token/Glyph/HexEscape renderings (§43, §46). Space is
/// included so it can be made visible (§43).
fn is_special(c: char) -> bool {
    let cp = c as u32;
    cp <= 0x20 || cp == 0x7F
}

/// Direct cell emitter for Raw mode: writes cells (each kept intact) straight
/// into the output, soft-wrapping at `width` columns (§43). Replaces the former
/// per-cell `Vec<String>` — one allocation per *render*, not per byte.
struct CellWriter {
    out: String,
    width: Option<usize>,
    line_len: usize,
}

impl CellWriter {
    fn new(width: Option<usize>, capacity: usize) -> Self {
        Self {
            out: String::with_capacity(capacity),
            width: width.filter(|&w| w > 0),
            line_len: 0,
        }
    }

    /// Account for a cell of `len` columns, wrapping first if it won't fit —
    /// the cell itself stays intact (§43 wrapping).
    fn start_cell(&mut self, len: usize) {
        if let Some(w) = self.width {
            if self.line_len > 0 && self.line_len + len > w {
                self.out.push('\n');
                self.line_len = 0;
            }
        }
        self.line_len += len;
    }

    /// An annotation cell (an already-formatted string, e.g. a Mark timestamp).
    fn cell_str(&mut self, s: &str) {
        self.start_cell(s.chars().count());
        self.out.push_str(s);
    }

    /// Emit annotation text verbatim while honoring explicit line controls.
    /// Single-line annotations remain one indivisible cell. A multiline
    /// annotation resets wrap state at each CR/LF and keeps each line segment
    /// intact rather than wrapping inside it.
    fn annotation(&mut self, s: &str) {
        if !s.contains(['\r', '\n']) {
            self.cell_str(s);
            return;
        }
        let mut start = 0;
        for (i, c) in s.char_indices() {
            if !matches!(c, '\r' | '\n') {
                continue;
            }
            let segment = &s[start..i];
            if !segment.is_empty() {
                self.cell_str(segment);
            }
            self.out.push(c);
            self.line_len = 0;
            start = i + c.len_utf8();
        }
        let tail = &s[start..];
        if !tail.is_empty() {
            self.cell_str(tail);
        }
    }

    fn cell_char(&mut self, c: char) {
        self.start_cell(1);
        self.out.push(c);
    }

    /// One rendered character cell (§46) — allocation-free per character.
    fn cell_rendered(&mut self, c: char, mode: CharacterRendering) {
        match mode {
            // Pass the character through unchanged — including control characters,
            // which Raw display does not interpret (§43). Use the other modes to
            // make them visible.
            CharacterRendering::Native => self.cell_char(c),
            CharacterRendering::Token => match c {
                _ if !is_special(c) => self.cell_char(c),
                ' ' => self.cell_str("[SP]"),
                '\u{7F}' => self.cell_str("[DEL]"),
                c => self.cell_str(CONTROL_TOKENS[c as usize]),
            },
            CharacterRendering::Glyph => match c as u32 {
                // Control Pictures block: U+2400 + code point covers 0x00..=0x20
                // (so space → U+2420 ␠); DEL → U+2421 ␡.
                cp @ 0..=0x20 => self.cell_char(char::from_u32(0x2400 + cp).unwrap()),
                0x7F => self.cell_char('\u{2421}'),
                _ => self.cell_char(c),
            },
            CharacterRendering::HexEscape => {
                if is_special(c) {
                    // `<XX>` — is_special caps the code point at 0x7F, two digits.
                    let cp = c as usize;
                    self.start_cell(4);
                    self.out.push('<');
                    self.out.push(HEX_DIGITS[(cp >> 4) & 0xF] as char);
                    self.out.push(HEX_DIGITS[cp & 0xF] as char);
                    self.out.push('>');
                } else {
                    self.cell_char(c)
                }
            }
        }
    }
}

/// Forward-only cursor over offset-sorted `annotations`. The render walks visit
/// offsets in ascending order, so the run of annotations at each offset is found
/// by advancing a pointer — O(bytes + annotations) overall. (The previous
/// per-offset filter rescanned the whole list for **every byte**: O(bytes ×
/// annotations), which froze the UI once a dense Mark rule had pinned thousands
/// of timestamps into a large scrollback window.)
struct AnnotationWalker<'a> {
    annotations: &'a [RenderAnnotation],
    next: usize,
}

impl<'a> AnnotationWalker<'a> {
    fn new(annotations: &'a [RenderAnnotation]) -> Self {
        Self {
            annotations,
            next: 0,
        }
    }

    /// The annotations at exactly `offset`, in input order (stable stacking for
    /// multiple marks on one byte). Must be called with non-decreasing offsets;
    /// an annotation at an offset the walk never visits is skipped.
    fn run_at(&mut self, offset: usize) -> &'a [RenderAnnotation] {
        self.run_in(offset, offset + 1)
    }

    /// The annotations anchored on any byte of `start..end`, in offset order and
    /// then input order. The text walks call this with one decoded character's
    /// byte span, so a mark anchored on a later byte of a multi-byte character
    /// is placed against that character rather than lost. Spans must not go
    /// backwards.
    fn run_in(&mut self, start: usize, end: usize) -> &'a [RenderAnnotation] {
        while self.next < self.annotations.len() && self.annotations[self.next].offset < start {
            self.next += 1;
        }
        let first = self.next;
        while self.next < self.annotations.len() && self.annotations[self.next].offset < end {
            self.next += 1;
        }
        &self.annotations[first..self.next]
    }
}

/// Each decoded character with the byte span `start..end` it came from. The
/// decoder's offsets are contiguous, so a character ends where the next begins
/// and the last ends at `len`.
fn char_spans(
    decoded: &[(char, usize)],
    len: usize,
) -> impl Iterator<Item = (char, usize, usize)> + '_ {
    decoded.iter().enumerate().map(move |(i, &(c, start))| {
        let end = decoded.get(i + 1).map_or(len, |&(_, next)| next);
        (c, start, end)
    })
}

/// Split the annotation run at one offset into the texts spliced **before** and
/// **after** that byte, preserving input order.
fn split_run(
    run: &[RenderAnnotation],
) -> (
    impl Iterator<Item = &str> + '_,
    impl Iterator<Item = &str> + '_,
) {
    let before = run
        .iter()
        .filter(|a| a.placement == AnnotationPlacement::Before)
        .map(|a| a.text.as_str());
    let after = run
        .iter()
        .filter(|a| a.placement == AnnotationPlacement::After)
        .map(|a| a.text.as_str());
    (before, after)
}

/// Text after the final explicit line control, or `None` for single-line text.
/// CRLF naturally resolves to the portion after LF.
fn tail_after_line_control(text: &str) -> Option<&str> {
    text.char_indices()
        .rev()
        .find(|(_, c)| matches!(c, '\r' | '\n'))
        .map(|(i, c)| &text[i + c.len_utf8()..])
}

/// Hex-renderer position, carried across chunks so a streaming caller's output
/// is identical to a one-shot render of the same bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HexCursor {
    /// Cells emitted on the current output line, for wrapping.
    cells_on_line: usize,
    /// Bytes emitted into the current group, for spacing (§45).
    bytes_in_group: usize,
    /// Monospace columns already written on the current output line.
    ///
    /// Tracked separately from the cell count because cells are **not** the
    /// same width: a byte is two columns, an annotation is as wide as its
    /// text. A line of N cells can therefore be far wider than N columns, and
    /// a cell count alone cannot keep a rendered line inside the pane.
    columns_on_line: usize,
}

/// Open the next cell: wrap the line if it is full, otherwise separate from the
/// previous cell. `grouped` is false for a byte continuing a group — those run
/// together, which is the whole point of `bytes_per_group`.
///
/// `width` is the cell's monospace width. A line ends when it has taken all the
/// cells it was asked for **or** when the next cell would not fit within
/// `max_columns`. Only bytes reach here — annotations take their own row — so
/// the two bounds normally agree; the column one is the renderer holding the
/// pane limit itself rather than inheriting the caller's arithmetic.
fn start_hex_cell(
    out: &mut String,
    cursor: &mut HexCursor,
    per_line: Option<usize>,
    max_columns: Option<usize>,
    separator: &str,
    grouped: bool,
    width: usize,
) {
    if cursor.cells_on_line > 0 {
        let lead = if grouped {
            0
        } else {
            separator.chars().count()
        };
        let full = per_line.is_some_and(|n| cursor.cells_on_line >= n)
            || max_columns.is_some_and(|n| cursor.columns_on_line + lead + width > n);
        if full {
            out.push('\n');
            cursor.cells_on_line = 0;
            cursor.bytes_in_group = 0;
            cursor.columns_on_line = 0;
        } else if !grouped {
            out.push_str(separator);
            cursor.columns_on_line += lead;
        }
    }
    cursor.cells_on_line += 1;
    cursor.columns_on_line += width;
}

/// Emit an annotation as a line of its own, breaking the byte run at the byte
/// it marks (§45, ADR-042).
///
/// Hex is a fixed-width grid, and that grid is the whole reason to read it:
/// byte *N* sits at a predictable column, so a reader can follow one field down
/// a column across many rows. A Mark spliced into a byte row destroys that for
/// everything after it — its text is as wide as it is, so the bytes following
/// land at unpredictable columns and the row's length stops matching its
/// neighbours'. Giving it its own line costs a short row and keeps the grid.
///
/// The break is at the marked byte, not at the next row boundary. A Mark says
/// *this byte*, so a rendering that only placed it near the right row would be
/// answering a coarser question than the one asked. The ragged row that results
/// explains itself: the reason is sitting on the line below it.
fn push_hex_annotation(out: &mut String, cursor: &mut HexCursor, text: &str) {
    if cursor.cells_on_line > 0 {
        out.push('\n');
    }
    out.push_str(text);
    // Mark text that already ends a line does not need another break; anything
    // after it on that line is the caller's own continuation.
    if !text.ends_with(['\r', '\n']) {
        out.push('\n');
    }
    // A fresh row, and a fresh group: a Mark never shifts the grouping of what
    // follows it.
    cursor.cells_on_line = 0;
    cursor.bytes_in_group = 0;
    cursor.columns_on_line = 0;
}

/// Render bytes as Hex (§45): each byte as two uppercase hex digits. Every
/// `bytes_per_group` bytes run together as one group; groups are joined by
/// `separator`. A `bytes_per_group` of 1 is the conventional hexdump, one byte
/// per group, and is what a zero also means — no group can be empty.
///
/// When `bytes_per_line` is `Some`, wrap to that many byte cells per line;
/// `max_columns` bounds the same line by width. Both are kept, and they are not
/// redundant: the byte count honours the reader's requested groups-per-line,
/// while the column bound is the renderer enforcing "never wider than the pane"
/// for itself rather than trusting a caller's arithmetic to agree with it.
///
/// `annotations` take a line of their own (see [`push_hex_annotation`]) and so
/// never share a row with bytes. One wider than the pane still overruns, since
/// there is nowhere else for it to go — but that cuts annotation text, never a
/// byte.
/// Emits directly into one pre-sized `String` — no per-byte allocation.
/// `cursor` carries streaming continuation state across chunks: an annotation
/// ending in CR/LF resets the line, and a group survives a chunk boundary so a
/// transport's read sizes never show up in the rendered spacing.
fn render_hex(
    bytes: &[u8],
    separator: &str,
    bytes_per_group: usize,
    bytes_per_line: Option<usize>,
    max_columns: Option<usize>,
    annotations: &[RenderAnnotation],
    cursor: &mut HexCursor,
) -> String {
    let per_line = bytes_per_line.filter(|&n| n > 0);
    let max_columns = max_columns.filter(|&n| n > 0);
    let per_group = bytes_per_group.max(1);
    let mut walker = AnnotationWalker::new(annotations);
    let mut out = String::with_capacity(bytes.len() * (2 + separator.len()));
    for (i, b) in bytes.iter().enumerate() {
        let (before, after) = split_run(walker.run_at(i));
        for s in before {
            push_hex_annotation(&mut out, cursor, s);
        }
        let continues_group = cursor.bytes_in_group > 0;
        start_hex_cell(
            &mut out,
            cursor,
            per_line,
            max_columns,
            separator,
            continues_group,
            HEX_CELL_COLUMNS,
        );
        out.push(HEX_DIGITS[(b >> 4) as usize] as char);
        out.push(HEX_DIGITS[(b & 0xF) as usize] as char);
        cursor.bytes_in_group = (cursor.bytes_in_group + 1) % per_group;
        for s in after {
            push_hex_annotation(&mut out, cursor, s);
        }
    }
    // Annotations targeting the one-past-the-end offset (an `After` on the final byte
    // is handled above; a `Before` at len is a trailing mark) attach at the end.
    let (end_before, _) = split_run(walker.run_at(bytes.len()));
    for s in end_before {
        push_hex_annotation(&mut out, cursor, s);
    }
    out
}

/// Columns one byte occupies in Hex: two digits, always.
const HEX_CELL_COLUMNS: usize = 2;

/// Hard-wrap each existing line of `text` to `width` characters.
fn wrap_lines(text: &str, width: Option<usize>) -> String {
    let Some(w) = width.filter(|&w| w > 0) else {
        return text.to_string();
    };
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let chars: Vec<char> = line.chars().collect();
        for (j, chunk) in chars.chunks(w).enumerate() {
            if j > 0 {
                out.push('\n');
            }
            out.extend(chunk);
        }
    }
    out
}

/// Rendered Display (§44): terminal-style text. LF starts a new line, CR is
/// dropped (so CRLF collapses to a single break), TAB expands to the next
/// 8-column tab stop, and other control characters are not printed. v1 does not
/// emulate cursor movement.
/// `col` is the terminal column carried in/out so tab stops stay correct when a
/// streaming caller renders chunk by chunk (a one-shot caller passes `&mut 0`).
fn render_rendered(
    bytes: &[u8],
    encoding: DisplayEncoding,
    annotations: &[RenderAnnotation],
    col: &mut usize,
) -> String {
    let mut out = String::new();
    let mut walker = AnnotationWalker::new(annotations);
    let push_annotation = |out: &mut String, text: &str, col: &mut usize| {
        out.push_str(text);
        let Some(tail) = tail_after_line_control(text) else {
            // Preserve the established single-line rule: presentation text does
            // not shift the underlying stream's tab stops.
            return;
        };
        *col = 0;
        for c in tail.chars() {
            match c {
                '\t' => *col += 8 - (*col % 8),
                c if is_special(c) && c != ' ' => {}
                _ => *col += 1,
            }
        }
    };
    let decoded = decode_with_offsets(bytes, encoding);
    for (c, start, end) in char_spans(&decoded, bytes.len()) {
        let (before, after) = split_run(walker.run_in(start, end));
        for s in before {
            push_annotation(&mut out, s, col);
        }
        match c {
            '\n' => {
                out.push('\n');
                *col = 0;
            }
            '\r' => {}
            '\t' => {
                let spaces = 8 - (*col % 8);
                for _ in 0..spaces {
                    out.push(' ');
                }
                *col += spaces;
            }
            // A space is an ordinary printable character in terminal output — it
            // must pass through. (`is_special` treats 0x20 as special only so Raw
            // mode's Token/Glyph/HexEscape can *make it visible*; that does not
            // apply here.) Guard it before the control-dropping arm below.
            ' ' => {
                out.push(' ');
                *col += 1;
            }
            c if is_special(c) => {} // other controls are not printed
            c => {
                out.push(c);
                *col += 1;
            }
        }
        for s in after {
            push_annotation(&mut out, s, col);
        }
    }
    // A trailing annotation at the one-past-end offset (e.g. an After on the final
    // byte lands above; a Before at len is a trailing mark).
    let (end_before, _) = split_run(walker.run_at(bytes.len()));
    for s in end_before {
        push_annotation(&mut out, s, col);
    }
    out
}

/// A configured Display View renderer (§47, §78, §141). Holds the presentation
/// settings that affect rendered output; purely visual options (font, colors)
/// live in the persisted config and do not affect the produced text.
#[derive(Clone, Debug)]
pub struct DisplayView {
    pub mode: DisplayMode,
    pub encoding: DisplayEncoding,
    pub character_rendering: CharacterRendering,
    pub wrapping: WrappingMode,
    /// View width in columns; wrapping is applied only when this is `Some`.
    pub wrap_width: Option<usize>,
    pub hex_separator: String,
    /// Bytes that run together between separators in Hex display (§45). 1 is the
    /// conventional hexdump; 0 is treated as 1, since no group can be empty.
    pub hex_bytes_per_group: usize,
    /// Bytes per line for Hex display when wrapping (§45). Zero means "fit to
    /// the display width" — the viewer's job, not the renderer's.
    pub hex_bytes_per_line: usize,
}

impl Default for DisplayView {
    fn default() -> Self {
        Self {
            mode: DisplayMode::Raw,
            encoding: DisplayEncoding::Utf8,
            character_rendering: CharacterRendering::Native,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_group: 1,
            hex_bytes_per_line: 16,
        }
    }
}

impl DisplayView {
    /// Render raw stream bytes to the view's text representation (§41).
    pub fn render_text(&self, bytes: &[u8]) -> String {
        self.render_text_annotated(bytes, &[])
    }

    /// Render raw stream bytes, splicing inline `annotations` (§50.2 Mark
    /// timestamps) before/after their target byte in every mode. Annotations may
    /// arrive in any order (sorted internally); offsets past the span's end are
    /// ignored. Text insertion only — no byte→coordinate mapping — so it works
    /// uniformly across Raw, Rendered, and Hex.
    pub fn render_text_annotated(&self, bytes: &[u8], annotations: &[RenderAnnotation]) -> String {
        // The forward-only AnnotationWalker needs offset-sorted input and would
        // silently drop an out-of-order (lower-offset) annotation. Callers mostly
        // pass sorted lists, but a chunk where several rules fired collects its
        // annotations rule-major, which can interleave offsets — enforce the
        // invariant here (O(n) check when already sorted; stable sort otherwise,
        // so equal-offset marks keep their arrival order) rather than make every
        // call site re-prove it.
        let sorted_buf: Vec<RenderAnnotation>;
        let annotations = if annotations.is_sorted_by_key(|a| a.offset) {
            annotations
        } else {
            sorted_buf = {
                let mut v = annotations.to_vec();
                v.sort_by_key(|a| a.offset);
                v
            };
            &sorted_buf
        };
        let wrap = matches!(self.wrapping, WrappingMode::Wrap);
        match self.mode {
            DisplayMode::Hex => {
                let bytes_per_line = wrap.then_some(self.hex_bytes_per_line);
                let mut cursor = HexCursor::default();
                render_hex(
                    bytes,
                    &self.hex_separator,
                    self.hex_bytes_per_group,
                    bytes_per_line,
                    // Only a view that already wraps gets a column bound, so a
                    // non-wrapping caller cannot acquire one by side effect.
                    bytes_per_line.and(self.wrap_width),
                    annotations,
                    &mut cursor,
                )
            }
            DisplayMode::Raw => {
                // One cell per byte's rendered char, with annotation strings spliced
                // in as their own cells so wrapping keeps each intact — emitted
                // straight into the output (no per-byte allocation).
                let mut walker = AnnotationWalker::new(annotations);
                let mut w = CellWriter::new(if wrap { self.wrap_width } else { None }, bytes.len());
                let decoded = decode_with_offsets(bytes, self.encoding);
                for (c, start, end) in char_spans(&decoded, bytes.len()) {
                    let (before, after) = split_run(walker.run_in(start, end));
                    for s in before {
                        w.annotation(s);
                    }
                    w.cell_rendered(c, self.character_rendering);
                    for s in after {
                        w.annotation(s);
                    }
                }
                let (end_before, _) = split_run(walker.run_at(bytes.len()));
                for s in end_before {
                    w.annotation(s);
                }
                w.out
            }
            DisplayMode::Rendered => {
                let text = render_rendered(bytes, self.encoding, annotations, &mut 0);
                if wrap {
                    wrap_lines(&text, self.wrap_width)
                } else {
                    text
                }
            }
        }
    }
}

/// A **streaming** renderer for Display Recording (§54): renders the received
/// stream chunk by chunk into the *exact rendered stream* — the same text as if
/// the whole stream were rendered at once. Chunk boundaries are reception
/// details (ADR-010) and leave no trace in the output:
/// - a multi-byte character split across reads decodes as itself (the
///   incomplete tail is carried into the next chunk), never `U+FFFD`;
/// - Rendered-mode tab stops keep their terminal column across chunks;
/// - Hex cells get exactly one separator between them, across chunks too.
///
/// **No hard wraps are ever emitted** (the view's wrapping config is ignored
/// here): line breaks come only from the data, and soft-wrapping at the display
/// edge is the *viewer's* job — the Notepad model. Owned per active recording;
/// state starts fresh at each recording begin and spans rotation boundaries.
pub struct StreamRenderer {
    view: DisplayView,
    /// Undecoded tail of the previous chunk — an incomplete multi-byte sequence
    /// held back until its remaining bytes arrive.
    carry: Vec<u8>,
    /// Annotations whose target byte is still in `carry` (or beyond), offsets
    /// relative to `carry[0]`.
    pending: Vec<RenderAnnotation>,
    /// Rendered-mode terminal column, so tab stops survive chunk boundaries.
    col: usize,
    /// Hex line and group position. Explicit annotation newlines reset the line
    /// so the next chunk starts flush, and the group survives the chunk
    /// boundary so read sizes never show up in the spacing.
    hex_cursor: HexCursor,
}

impl StreamRenderer {
    pub fn new(view: DisplayView) -> Self {
        Self {
            view,
            carry: Vec::new(),
            pending: Vec::new(),
            col: 0,
            hex_cursor: HexCursor::default(),
        }
    }

    /// Render one received chunk; `annotations` offsets are relative to
    /// `bytes[0]` (any order — sorted internally). Returns the text to append
    /// to the recording, possibly empty (e.g. the whole chunk is an incomplete
    /// tail). An annotation at/past the renderable end is deferred and splices
    /// before its byte when that byte arrives (or at [`finish`](Self::finish)).
    pub fn render_chunk(&mut self, bytes: &[u8], annotations: &[RenderAnnotation]) -> String {
        // Re-join the carried tail so a split character decodes whole; shift the
        // incoming annotation offsets past it and merge with the deferred ones.
        let carry_len = self.carry.len();
        let joined: Vec<u8>;
        let all: &[u8] = if carry_len == 0 {
            bytes
        } else {
            joined = {
                let mut j = std::mem::take(&mut self.carry);
                j.extend_from_slice(bytes);
                j
            };
            &joined
        };
        let mut anns: Vec<RenderAnnotation> = std::mem::take(&mut self.pending);
        anns.extend(annotations.iter().map(|a| RenderAnnotation {
            offset: a.offset + carry_len,
            placement: a.placement,
            text: a.text.clone(),
        }));
        anns.sort_by_key(|a| a.offset);

        // Hold back an incomplete multi-byte tail (§119: lossy replacement is
        // for invalid data; a read boundary is not the data's fault).
        let tail = super::encoding::incomplete_tail(all, self.view.encoding);
        let render_len = all.len() - tail;
        let (now, defer): (Vec<_>, Vec<_>) = anns.into_iter().partition(|a| a.offset < render_len);
        self.pending = defer
            .into_iter()
            .map(|a| RenderAnnotation {
                offset: a.offset - render_len,
                placement: a.placement,
                text: a.text,
            })
            .collect();
        self.carry = all[render_len..].to_vec();
        self.render_slice(&all[..render_len], &now)
    }

    /// Flush at recording finalize: render any still-carried tail (now genuinely
    /// truncated data — the lossy decoder applies) plus deferred annotations.
    /// Possibly empty; the caller appends it before closing the file.
    pub fn finish(&mut self) -> String {
        if self.carry.is_empty() && self.pending.is_empty() {
            return String::new();
        }
        let carry = std::mem::take(&mut self.carry);
        let pending = std::mem::take(&mut self.pending);
        self.render_slice(&carry, &pending)
    }

    /// One mode dispatch shared by `render_chunk`/`finish` — the same emitters
    /// as the one-shot renderer, threaded with this recording's state.
    fn render_slice(&mut self, bytes: &[u8], annotations: &[RenderAnnotation]) -> String {
        match self.view.mode {
            // A zero `hex_bytes_per_line` means "do not wrap", which is what the
            // recorder's view always carries: `.disp` is the exact rendered
            // stream and soft wrapping is the viewer's job (ADR-018). The GUI
            // sets a real line length when the user asks for fixed columns, and
            // wrapping here — where cells are known — keeps a separator from
            // being stranded at the head of the next line.
            DisplayMode::Hex => {
                let per_line = Some(self.view.hex_bytes_per_line).filter(|&n| n > 0);
                render_hex(
                    bytes,
                    &self.view.hex_separator,
                    self.view.hex_bytes_per_group,
                    per_line,
                    // Tied to the same request: the recorder's view asks for no
                    // line length, so `.disp` stays the exact rendered stream
                    // however wide the viewer happens to be.
                    per_line.and(self.view.wrap_width),
                    annotations,
                    &mut self.hex_cursor,
                )
            }
            DisplayMode::Rendered => {
                render_rendered(bytes, self.view.encoding, annotations, &mut self.col)
            }
            DisplayMode::Raw => {
                let mut walker = AnnotationWalker::new(annotations);
                let mut w = CellWriter::new(None, bytes.len());
                let decoded = decode_with_offsets(bytes, self.view.encoding);
                for (c, start, end) in char_spans(&decoded, bytes.len()) {
                    let (before, after) = split_run(walker.run_in(start, end));
                    for s in before {
                        w.annotation(s);
                    }
                    w.cell_rendered(c, self.view.character_rendering);
                    for s in after {
                        w.annotation(s);
                    }
                }
                let (end_before, _) = split_run(walker.run_at(bytes.len()));
                for s in end_before {
                    w.annotation(s);
                }
                w.out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(mode: DisplayMode, rendering: CharacterRendering) -> DisplayView {
        DisplayView {
            mode,
            character_rendering: rendering,
            ..DisplayView::default()
        }
    }

    #[test]
    fn hex_is_uppercase_two_digit_space_separated() {
        let v = view(DisplayMode::Hex, CharacterRendering::Native);
        assert_eq!(v.render_text(b"Hi\n"), "48 69 0A");
    }

    #[test]
    fn hex_wraps_at_bytes_per_line() {
        let mut v = view(DisplayMode::Hex, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.hex_bytes_per_line = 2;
        assert_eq!(v.render_text(b"ABCDE"), "41 42\n43 44\n45");
    }

    /// Bytes inside a group run together; only groups are separated (§45).
    #[test]
    fn hex_groups_bytes_between_separators() {
        let mut v = view(DisplayMode::Hex, CharacterRendering::Native);
        assert_eq!(v.render_text(b"ABCDE"), "41 42 43 44 45", "1 = hexdump");

        v.hex_bytes_per_group = 2;
        assert_eq!(v.render_text(b"ABCDE"), "4142 4344 45");

        v.hex_bytes_per_group = 4;
        assert_eq!(v.render_text(b"ABCDE"), "41424344 45");

        // Zero cannot mean "one unbroken run of the whole stream", so it is the
        // conventional single byte per group.
        v.hex_bytes_per_group = 0;
        assert_eq!(v.render_text(b"ABCDE"), "41 42 43 44 45");
    }

    /// Grouping is rendering, so it reaches a Display recording — and it must
    /// not depend on how the transport happened to divide the stream into reads.
    #[test]
    fn hex_grouping_is_chunking_invariant() {
        let mut v = view(DisplayMode::Hex, CharacterRendering::Native);
        v.hex_bytes_per_group = 4;
        let data = b"ABCDEFGHIJ";
        let want = v.render_text(data);
        assert_eq!(want, "41424344 45464748 494A");
        for chunk in [1usize, 2, 3, 7] {
            let mut renderer = StreamRenderer::new(v.clone());
            let mut got = String::new();
            for slice in data.chunks(chunk) {
                got.push_str(&renderer.render_chunk(slice, &[]));
            }
            got.push_str(&renderer.finish());
            assert_eq!(got, want, "chunk {chunk}");
        }
    }

    #[test]
    fn raw_native_passes_control_bytes_through_unchanged() {
        // No interpretation: CR is preserved as a literal carriage return (§43).
        let v = view(DisplayMode::Raw, CharacterRendering::Native);
        assert_eq!(v.render_text(b"A\r\nB"), "A\r\nB");
    }

    #[test]
    fn raw_token_names_controls_and_space() {
        let v = view(DisplayMode::Raw, CharacterRendering::Token);
        assert_eq!(v.render_text(b"A\r\n B"), "A[CR][LF][SP]B");
    }

    #[test]
    fn raw_glyph_uses_control_pictures() {
        let v = view(DisplayMode::Raw, CharacterRendering::Glyph);
        // \t → ␉ (U+2409), \n → ␊ (U+240A), space → ␠ (U+2420).
        assert_eq!(v.render_text(b"\t\n "), "␉␊␠");
    }

    #[test]
    fn raw_hex_escape_escapes_controls_and_space() {
        let v = view(DisplayMode::Raw, CharacterRendering::HexEscape);
        assert_eq!(v.render_text(b"A\r\n B"), "A<0D><0A><20>B");
    }

    #[test]
    fn raw_wraps_by_cells_keeping_tokens_intact() {
        let mut v = view(DisplayMode::Raw, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.wrap_width = Some(3);
        assert_eq!(v.render_text(b"ABCDEFG"), "ABC\nDEF\nG");
    }

    #[test]
    fn rendered_interprets_crlf_and_tabs() {
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        // CR dropped, LF is a newline.
        assert_eq!(v.render_text(b"a\r\nb"), "a\nb");
        // Tab expands to the next 8-column stop.
        assert_eq!(v.render_text(b"a\tb"), "a       b");
        // Other controls are not printed.
        assert_eq!(v.render_text(b"a\x07b"), "ab");
    }

    #[test]
    fn rendered_preserves_literal_spaces() {
        // Regression: a space (0x20) is printable in terminal output and must not be
        // dropped as a "special" control (it is visible in Raw/Hex but was vanishing
        // in Rendered).
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        assert_eq!(v.render_text(b"a b  c"), "a b  c");
        assert_eq!(v.render_text(b"$GPGGA, 123, 45"), "$GPGGA, 123, 45");
        // Tab-stop column accounting still tracks spaces: after "ab " (col 3) a tab
        // advances to column 8 → 5 spaces.
        assert_eq!(v.render_text(b"ab \tX"), "ab      X");
    }

    fn ann(offset: usize, placement: AnnotationPlacement, text: &str) -> RenderAnnotation {
        RenderAnnotation {
            offset,
            placement,
            text: text.to_string(),
        }
    }

    /// One character's bytes, the encoding they are in, and the character.
    fn unicode_cases() -> Vec<(DisplayEncoding, Vec<u8>, &'static str)> {
        vec![
            (
                DisplayEncoding::Utf8,
                "\u{e9}".as_bytes().to_vec(),
                "\u{e9}",
            ),
            (
                DisplayEncoding::Utf8,
                "\u{20ac}".as_bytes().to_vec(),
                "\u{20ac}",
            ),
            (
                DisplayEncoding::Utf8,
                "\u{1F600}".as_bytes().to_vec(),
                "\u{1F600}",
            ),
            (DisplayEncoding::Utf16Be, vec![0x00, 0xE9], "\u{e9}"),
            (DisplayEncoding::Utf16Le, vec![0xE9, 0x00], "\u{e9}"),
            // U+1F600 is the surrogate pair D83D DE00.
            (
                DisplayEncoding::Utf16Be,
                vec![0xD8, 0x3D, 0xDE, 0x00],
                "\u{1F600}",
            ),
            (
                DisplayEncoding::Utf16Le,
                vec![0x3D, 0xD8, 0x00, 0xDE],
                "\u{1F600}",
            ),
        ]
    }

    fn encoded_view(mode: DisplayMode, encoding: DisplayEncoding) -> DisplayView {
        DisplayView {
            encoding,
            ..view(mode, CharacterRendering::Native)
        }
    }

    #[test]
    fn an_after_annotation_anywhere_in_a_character_follows_that_character() {
        for mode in [DisplayMode::Raw, DisplayMode::Rendered] {
            for (encoding, bytes, text) in unicode_cases() {
                let v = encoded_view(mode, encoding);
                // UTF-8 gets an ASCII lead byte, so the anchor is not offset 0;
                // UTF-16 starts on the character.
                let (lead, shift) = if matches!(encoding, DisplayEncoding::Utf8) {
                    (b"a".as_slice(), 1)
                } else {
                    (b"".as_slice(), 0)
                };
                let data = [lead, &bytes].concat();
                let lead = std::str::from_utf8(lead).unwrap();
                for anchor in 0..bytes.len() {
                    let anns = [ann(anchor + shift, AnnotationPlacement::After, "[T]")];
                    assert_eq!(
                        v.render_text_annotated(&data, &anns),
                        format!("{lead}{text}[T]"),
                        "{mode:?} {encoding:?} anchor {anchor}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_before_annotation_inside_a_character_precedes_that_character() {
        for mode in [DisplayMode::Raw, DisplayMode::Rendered] {
            for (encoding, bytes, text) in unicode_cases() {
                let v = encoded_view(mode, encoding);
                for anchor in 0..bytes.len() {
                    let anns = [ann(anchor, AnnotationPlacement::Before, "[T]")];
                    assert_eq!(
                        v.render_text_annotated(&bytes, &anns),
                        format!("[T]{text}"),
                        "{mode:?} {encoding:?} anchor {anchor}"
                    );
                }
            }
        }
    }

    #[test]
    fn annotations_inside_one_character_keep_their_order() {
        let v = encoded_view(DisplayMode::Raw, DisplayEncoding::Utf8);
        let anns = [
            ann(0, AnnotationPlacement::After, "[1]"),
            ann(1, AnnotationPlacement::After, "[2]"),
            ann(1, AnnotationPlacement::Before, "[0]"),
        ];
        assert_eq!(
            v.render_text_annotated("\u{e9}".as_bytes(), &anns),
            "[0]\u{e9}[1][2]"
        );
    }

    #[test]
    fn an_after_annotation_on_the_second_half_of_a_split_character_is_kept() {
        for mode in [DisplayMode::Raw, DisplayMode::Rendered] {
            let mut r = StreamRenderer::new(encoded_view(mode, DisplayEncoding::Utf8));
            let mut out = r.render_chunk(&[0xC3], &[]);
            out.push_str(&r.render_chunk(&[0xA9], &[ann(0, AnnotationPlacement::After, "[T]")]));
            out.push_str(&r.finish());
            assert_eq!(out, "\u{e9}[T]", "{mode:?}");
        }
    }

    #[test]
    fn an_after_annotation_on_the_held_back_first_half_waits_for_the_whole_character() {
        for mode in [DisplayMode::Raw, DisplayMode::Rendered] {
            let mut r = StreamRenderer::new(encoded_view(mode, DisplayEncoding::Utf8));
            let mut out = r.render_chunk(&[0xC3], &[ann(0, AnnotationPlacement::After, "[T]")]);
            out.push_str(&r.render_chunk(&[0xA9], &[]));
            out.push_str(&r.finish());
            assert_eq!(out, "\u{e9}[T]", "{mode:?}");
        }
    }

    #[test]
    fn rendered_splices_timestamp_before_the_match() {
        // The canonical case: `[ts]` immediately before the `$` of a GGA sentence.
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let anns = [ann(3, AnnotationPlacement::Before, "[17:42:03]")];
        assert_eq!(
            v.render_text_annotated(b"ab\n$GPGGA", &anns),
            "ab\n[17:42:03]$GPGGA"
        );
    }

    #[test]
    fn rendered_splices_timestamp_after_the_match() {
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        // After the byte at offset 2 (the second 'b').
        let anns = [ann(2, AnnotationPlacement::After, "<T>")];
        assert_eq!(v.render_text_annotated(b"abbc", &anns), "abb<T>c");
    }

    #[test]
    fn raw_splices_annotation_as_its_own_cell() {
        let v = view(DisplayMode::Raw, CharacterRendering::HexEscape);
        let anns = [ann(1, AnnotationPlacement::Before, "[T]")];
        // Control bytes still escape; the annotation lands before the byte at 1.
        assert_eq!(v.render_text_annotated(b"A\nB", &anns), "A[T]<0A>B");
    }

    #[test]
    fn raw_annotation_cell_is_kept_intact_when_wrapping() {
        let mut v = view(DisplayMode::Raw, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.wrap_width = Some(3);
        // "[TS]" is one cell (4 cols): it forces a wrap before it rather than splitting.
        let anns = [ann(2, AnnotationPlacement::Before, "[TS]")];
        assert_eq!(v.render_text_annotated(b"ABCDE", &anns), "AB\n[TS]\nCDE");
    }

    /// Hex is a column grid, so a Mark takes a row of its own and the byte run
    /// breaks at the byte it marks — not at the next row boundary, because a
    /// Mark says *this byte* (ADR-042).
    #[test]
    fn hex_gives_an_annotation_its_own_row() {
        let v = view(DisplayMode::Hex, CharacterRendering::Native);
        let anns = [ann(1, AnnotationPlacement::Before, "[T]")];
        assert_eq!(v.render_text_annotated(b"ABC", &anns), "41\n[T]\n42 43");
    }

    /// An annotation no longer spends a byte-line slot, because it is not on
    /// the byte line: two bytes per line still means two bytes per line.
    #[test]
    fn an_annotation_row_does_not_consume_the_byte_line_budget() {
        let mut v = view(DisplayMode::Hex, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.hex_bytes_per_line = 2;
        let anns = [ann(1, AnnotationPlacement::Before, "[T]")];
        assert_eq!(v.render_text_annotated(b"ABC", &anns), "41\n[T]\n42 43");
    }

    #[test]
    fn multiline_annotation_resets_raw_wrap_state() {
        let mut v = view(DisplayMode::Raw, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.wrap_width = Some(4);
        let anns = [ann(1, AnnotationPlacement::After, "X\n")];
        assert_eq!(v.render_text_annotated(b"ABCD", &anns), "ABX\nCD");
    }

    #[test]
    fn multiline_annotation_resets_rendered_tab_column() {
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let anns = [ann(0, AnnotationPlacement::After, "<T>\n")];
        assert_eq!(v.render_text_annotated(b"a\tX", &anns), "a<T>\n        X");
    }

    // --- StreamRenderer (ADR-018: the exact rendered stream) ---

    #[test]
    fn stream_renderer_is_chunking_invariant() {
        // The defining invariant: rendering byte-at-a-time equals rendering the
        // whole stream at once — read boundaries leave no trace. Exercises
        // multi-byte UTF-8, CRLF, tabs, and controls across all three modes.
        let data = "a\tb€é\r\nx\x07y $GPGGA,1*77\r\n".as_bytes();
        for (mode, chars) in [
            (DisplayMode::Rendered, CharacterRendering::Native),
            (DisplayMode::Raw, CharacterRendering::HexEscape),
            (DisplayMode::Raw, CharacterRendering::Token),
            (DisplayMode::Hex, CharacterRendering::Native),
        ] {
            let v = view(mode, chars);
            let whole = {
                let mut r = StreamRenderer::new(v.clone());
                let mut out = r.render_chunk(data, &[]);
                out.push_str(&r.finish());
                out
            };
            let byte_at_a_time = {
                let mut r = StreamRenderer::new(v);
                let mut out = String::new();
                for b in data {
                    out.push_str(&r.render_chunk(std::slice::from_ref(b), &[]));
                }
                out.push_str(&r.finish());
                out
            };
            assert_eq!(
                byte_at_a_time, whole,
                "chunking changed {mode:?}/{chars:?} output"
            );
        }
    }

    #[test]
    fn stream_renderer_rejoins_a_split_utf8_character() {
        // The old per-chunk render turned a UTF-8 character split across two
        // reads into U+FFFD; the carry re-joins it.
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let mut r = StreamRenderer::new(v);
        let bytes = "é".as_bytes(); // [0xC3, 0xA9]
        let first = r.render_chunk(&bytes[..1], &[]);
        assert!(first.is_empty(), "the lead byte is carried, not replaced");
        let second = r.render_chunk(&bytes[1..], &[]);
        assert_eq!(second, "é");
        assert!(r.finish().is_empty());
    }

    #[test]
    fn stream_renderer_finish_renders_a_truncated_tail_lossily() {
        // A carry still held at finalize is genuinely truncated data — the
        // lossy decoder applies (§119).
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let mut r = StreamRenderer::new(v);
        assert!(r.render_chunk(&[b'a', 0xC3], &[]).ends_with('a'));
        assert_eq!(r.finish(), "\u{FFFD}");
    }

    #[test]
    fn stream_renderer_hex_separates_across_chunks_and_tabs_keep_columns() {
        // Hex: exactly one separator between cells, including across the chunk
        // boundary (the old code restarted flush, gluing "41 4243 44").
        let v = view(DisplayMode::Hex, CharacterRendering::Native);
        let mut r = StreamRenderer::new(v);
        let mut out = r.render_chunk(b"AB", &[]);
        out.push_str(&r.render_chunk(b"CD", &[]));
        assert_eq!(out, "41 42 43 44");

        // Rendered: the tab column carries, so a tab right after a boundary
        // still expands to the next 8-column stop of the whole stream.
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let mut r = StreamRenderer::new(v);
        let mut out = r.render_chunk(b"ab", &[]);
        out.push_str(&r.render_chunk(b"\tX", &[]));
        assert_eq!(out, "ab      X"); // col 2 → tab to 8
    }

    #[test]
    fn multiline_annotation_resets_hex_continuation_across_chunks() {
        let v = view(DisplayMode::Hex, CharacterRendering::Native);
        let mut r = StreamRenderer::new(v);
        let anns = [ann(0, AnnotationPlacement::After, "<T>\r\n")];
        let mut out = r.render_chunk(b"A", &anns);
        out.push_str(&r.render_chunk(b"B", &[]));
        // Mark text that already ends a line gets no second break.
        assert_eq!(out, "41\n<T>\r\n42");
    }

    #[test]
    fn stream_renderer_defers_an_annotation_for_a_carried_byte() {
        // A Mark targeting a byte still held in the carry splices when that
        // byte finally renders — on the correct side of it.
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let mut r = StreamRenderer::new(v);
        // Chunk 1: "x" + the lead byte of é; the annotation targets offset 1
        // (the é), which is carried.
        let bytes = "xé".as_bytes();
        let anns = [ann(1, AnnotationPlacement::Before, "[T]")];
        let first = r.render_chunk(&bytes[..2], &anns);
        assert_eq!(first, "x", "the annotated byte is still in the carry");
        let second = r.render_chunk(&bytes[2..], &[]);
        assert_eq!(second, "[T]é", "the deferred mark splices before its byte");
    }

    #[test]
    fn unsorted_annotations_all_splice() {
        // Regression: annotations collected rule-major can interleave offsets
        // (rule A at 5, rule B at 2). The forward-only walker would silently drop
        // the lower one; the renderer sorts internally so both splice.
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let anns = [
            ann(5, AnnotationPlacement::Before, "[B]"),
            ann(2, AnnotationPlacement::Before, "[A]"),
        ];
        assert_eq!(
            v.render_text_annotated(b"xxAxxBxx", &anns),
            "xx[A]Axx[B]Bxx"
        );
        // Same guarantee in Hex and Raw modes.
        let v = view(DisplayMode::Hex, CharacterRendering::Native);
        assert_eq!(
            v.render_text_annotated(
                b"abc",
                &[
                    ann(2, AnnotationPlacement::Before, "[2]"),
                    ann(0, AnnotationPlacement::Before, "[0]"),
                ]
            ),
            "[0]
61 62
[2]
63"
        );
    }

    #[test]
    fn annotation_at_end_offset_is_a_trailing_mark() {
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let anns = [ann(2, AnnotationPlacement::Before, "<end>")];
        assert_eq!(v.render_text_annotated(b"ab", &anns), "ab<end>");
    }

    #[test]
    fn annotation_maps_onto_multibyte_utf8_boundary() {
        // "é" is two UTF-8 bytes (0xC3 0xA9) at offset 1; an annotation before the
        // byte after it (offset 3, 'b') must land after the single 'é' char.
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let anns = [ann(3, AnnotationPlacement::Before, "|")];
        assert_eq!(v.render_text_annotated("aéb".as_bytes(), &anns), "aé|b");
    }
}
