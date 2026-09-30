//! Message definitions: payload format/encoding, timestamp, and checksum.
//!
//! A [`MessageConfig`] is compiled to a [`CompiledMessage`], which renders the
//! wire bytes — `[timestamp][payload][checksum]` — fresh on each send so the
//! timestamp is current.

mod checksum;
mod codepage;
mod marker;
mod timestamp;

pub use checksum::{ChecksumAlgorithm, ChecksumConfig};
pub use codepage::{decode_byte as decode_codepage_byte, CodePage};

/// Best-effort UTF-8 decode for human-facing byte previews.
///
/// Valid UTF-8 sequences decode normally. Any invalid byte falls
/// back to a Latin-1 interpretation — every byte `0x00..=0xFF`
/// maps 1:1 to `U+0000..=U+00FF`, which means every byte has a
/// glyph in every system font. Notably this turns the otherwise-
/// tofu replacement character for stray high bytes (`0xEE` etc.)
/// into a printable Latin-1 character (`î`), making the output
/// usable for mixed text + binary streams rather than substituting
/// U+FFFD.
///
/// Shared infrastructure: used by the GUI display pane's Rendered
/// mode and by the CLI `--echo` runner's default format.
pub fn decode_utf8_lossy_latin1(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match std::str::from_utf8(&bytes[i..]) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(e) => {
                let up_to = e.valid_up_to();
                if up_to > 0 {
                    // Safe: `from_utf8` says these bytes are valid.
                    out.push_str(std::str::from_utf8(&bytes[i..i + up_to]).unwrap());
                }
                // The byte at `i + up_to` is the first invalid one —
                // emit it as its Latin-1 char and advance past it.
                out.push(bytes[i + up_to] as char);
                i += up_to + 1;
            }
        }
    }
    out
}

pub use marker::{repair_after_edit, segments, Segment};

/// Unsupported characters that an ASCII/code-page payload will replace.
/// Valid `‹XX›` byte markers are excluded because they already describe an
/// exact wire byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodePageReplacementSummary {
    pub count: usize,
    pub characters: Vec<char>,
    /// Byte offsets within the compiled payload where fallback `?` bytes occur.
    pub payload_offsets: Vec<usize>,
}

pub(crate) fn code_page_encodes(c: char, code_page: CodePage) -> bool {
    codepage::encode_char(c, code_page).is_some()
}

pub(crate) fn code_page_replacements(
    text: &str,
    code_page: CodePage,
) -> Option<CodePageReplacementSummary> {
    let mut count = 0;
    let mut characters = std::collections::BTreeSet::new();
    let mut payload_offsets = Vec::new();
    let mut payload_offset = 0;
    for (range, segment) in segments(text) {
        match segment {
            Segment::Text => {
                for c in text[range].chars() {
                    // Literal marker delimiters are syntax errors handled by
                    // `compile_ascii`, not lossy code-page replacements.
                    if !matches!(c, '\u{2039}' | '\u{203A}') && !code_page_encodes(c, code_page) {
                        count += 1;
                        characters.insert(c);
                        payload_offsets.push(payload_offset);
                    }
                    payload_offset += 1;
                }
            }
            Segment::Byte(_) => payload_offset += 1,
        }
    }
    (count > 0).then(|| CodePageReplacementSummary {
        count,
        characters: characters.into_iter().collect(),
        payload_offsets,
    })
}

pub use timestamp::TimestampConfig;

use serde::{Deserialize, Serialize};

/// Byte order for multi-byte text encodings (UTF-16).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteOrder {
    /// Most-significant byte first (network order).
    #[default]
    BigEndian,
    /// Least-significant byte first.
    LittleEndian,
}

/// How an NMEA payload's trailing `*XX` checksum is rendered.
///
/// `talker`-side mirror of [`nmea0183::NmeaChecksumMode`], so the profile
/// schema can serialize/deserialize without enabling the `serde` feature
/// on `nmea0183` (per OQ-3 / ADR-014). Converted at compile time.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NmeaChecksumMode {
    /// Protocol default: append `*XX\r\n` with the correct XOR.
    #[default]
    Correct,
    /// Omit `*XX` entirely — wire ends with `\r\n` directly after fields.
    Omit,
    /// Append `*XX\r\n` with a deliberately wrong byte (correct ^ 0xFF).
    Wrong,
}

impl From<NmeaChecksumMode> for nmea0183::NmeaChecksumMode {
    fn from(m: NmeaChecksumMode) -> Self {
        match m {
            NmeaChecksumMode::Correct => Self::Correct,
            NmeaChecksumMode::Omit => Self::Omit,
            NmeaChecksumMode::Wrong => Self::Wrong,
        }
    }
}

/// One message in a channel: a payload, a send interval, and optional
/// timestamp and checksum.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageConfig {
    pub payload: PayloadConfig,
    pub interval_ms: u64,
    #[serde(default)]
    pub timestamp: Option<TimestampConfig>,
    #[serde(default)]
    pub checksum: Option<ChecksumConfig>,
}

impl MessageConfig {
    /// Create a message with no timestamp and no checksum.
    pub fn new(payload: PayloadConfig, interval_ms: u64) -> Self {
        Self {
            payload,
            interval_ms,
            timestamp: None,
            checksum: None,
        }
    }

    /// Compile and validate this message. Static payloads are encoded once;
    /// live NMEA keeps only its immutable template for per-send rendering.
    pub fn compile(&self) -> anyhow::Result<CompiledMessage> {
        let payload = self.payload.compile_payload()?;
        let timestamp_len = self.timestamp.map_or(0, |timestamp| timestamp.wire_len());
        let replacement_wire_offsets = match &self.payload {
            PayloadConfig::Ascii { text, code_page } => code_page_replacements(text, *code_page)
                .map(|summary| {
                    summary
                        .payload_offsets
                        .into_iter()
                        .map(|offset| timestamp_len + offset)
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(CompiledMessage {
            payload,
            timestamp: self.timestamp,
            checksum: self.checksum,
            replacement_wire_offsets,
        })
    }

    /// Check that this message would compile, without keeping the result.
    ///
    /// The one shared validation surface (a `compile()` dry-run): the GUI
    /// uses it to gate Start / drive red borders, the CLI to fail a profile
    /// before any interface is opened. Anything that passes here cannot fail
    /// later at `Schedule::compile` time.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.compile().map(|_| ())
    }
}

/// A validated message ready to render wire bytes per send.
#[derive(Debug, Clone)]
pub struct CompiledMessage {
    payload: CompiledPayload,
    timestamp: Option<TimestampConfig>,
    checksum: Option<ChecksumConfig>,
    /// Positions of lossy code-page fallback bytes in the final wire message.
    /// The timestamp prefix is included; the appended checksum cannot shift
    /// these positions.
    replacement_wire_offsets: Vec<usize>,
}

impl CompiledMessage {
    /// Produce the wire bytes for one send: `[timestamp][payload][checksum]`.
    ///
    /// The timestamp and any live NMEA fields use the same current instant;
    /// the outer checksum is computed over timestamp and payload together.
    pub fn render(&self) -> Vec<u8> {
        self.render_at(chrono::Utc::now())
    }

    /// Wire bytes one send produces, without producing them.
    ///
    /// Lets a *running* schedule report its own demand, so capacity readouts
    /// can describe the configuration that is actually sending rather than the
    /// one on screen. Every term is fixed at compile time: the timestamp's
    /// formatted width, the payload's compiled length (a live NMEA template
    /// keeps the width of its rendered form), and the checksum's algorithm.
    pub fn wire_len(&self) -> usize {
        self.timestamp.map_or(0, |timestamp| timestamp.wire_len())
            + self.payload.wire_len_hint()
            + self.checksum.map_or(0, |checksum| checksum.wire_len())
    }

    #[cfg(test)]
    fn wire_len_matches_render(&self) -> bool {
        self.wire_len() == self.render().len()
    }

    /// Byte positions that should be visually identified as lossy code-page
    /// substitutions. Computed once at compile time, never on the send path.
    pub(crate) fn replacement_wire_offsets(&self) -> &[usize] {
        &self.replacement_wire_offsets
    }

    /// Like [`Self::render`], but uses `now` for the prepended timestamp and
    /// live NMEA fields instead of reading the wall clock. Useful for stable
    /// previews and deterministic tests.
    pub fn render_at(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload.wire_len_hint() + 16);
        if let Some(ts) = &self.timestamp {
            out.extend_from_slice(ts.format(now).as_bytes());
        }
        self.payload.append_at(now, &mut out);
        if let Some(cs) = &self.checksum {
            let sum = cs.compute(&out);
            out.extend_from_slice(&sum);
        }
        out
    }
}

/// The payload source for one message.
///
/// `compile()` converts this to wire bytes; a compiled message retains live
/// NMEA templates so their known UTC fields can advance on every send.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PayloadConfig {
    /// Raw bytes as a hex string (spaces and hyphens are stripped).
    /// Example: `"DE AD BE EF"` or `"DEADBEEF"`.
    RawHex { data: String },
    /// Unicode text encoded as UTF-8.
    Utf8 {
        #[serde(default)]
        text: String,
    },
    /// Unicode text encoded as UTF-16, with a configurable byte order and an
    /// optional leading byte-order mark.
    Utf16 {
        #[serde(default)]
        text: String,
        #[serde(default)]
        byte_order: ByteOrder,
        #[serde(default)]
        bom: bool,
        /// When `true`, the text is parsed for `‹XX›` byte markers
        /// (spec §5.3) and each marker emits one raw byte alongside
        /// the normal UTF-16-encoded text. When `false` (default),
        /// `‹` and `›` are treated as literal characters and the
        /// text encodes cleanly to well-formed UTF-16. Off by
        /// default so the typical Unicode workflow stays simple;
        /// flip on for fuzz / corruption tests.
        #[serde(default)]
        allow_raw_bytes: bool,
    },
    /// Text encoded with a single-byte code page.
    Ascii {
        #[serde(default)]
        text: String,
        #[serde(default)]
        code_page: CodePage,
    },
    /// A standard NMEA 0183 sentence. Fields are the payload values after the
    /// sentence type; the trailing `*XX` checksum is appended per
    /// `nmea_checksum`.
    Nmea {
        talker: String,
        sentence_type: String,
        #[serde(default)]
        fields: Vec<String>,
        /// How the protocol-internal `*XX` checksum is rendered.
        /// Defaults to [`NmeaChecksumMode::Correct`]; older profiles
        /// without this field deserialize to that default.
        #[serde(default)]
        nmea_checksum: NmeaChecksumMode,
        /// Replace the sentence type's known UTC time/date fields at each
        /// send. Unsupported sentence types fail message validation instead
        /// of silently retaining stale typed values.
        #[serde(default)]
        live_time: bool,
        /// Include three fractional-second digits in a live UTC time field.
        #[serde(default)]
        live_time_millis: bool,
    },
}

impl PayloadConfig {
    pub fn raw_hex(hex: impl Into<String>) -> Self {
        Self::RawHex { data: hex.into() }
    }

    pub fn nmea(
        talker: impl Into<String>,
        sentence_type: impl Into<String>,
        fields: Vec<String>,
    ) -> Self {
        Self::Nmea {
            talker: talker.into(),
            sentence_type: sentence_type.into(),
            fields,
            nmea_checksum: NmeaChecksumMode::Correct,
            live_time: false,
            live_time_millis: false,
        }
    }

    /// Build an NMEA payload whose known UTC fields are refreshed per send.
    pub fn nmea_live(
        talker: impl Into<String>,
        sentence_type: impl Into<String>,
        fields: Vec<String>,
        include_millis: bool,
    ) -> Self {
        Self::Nmea {
            talker: talker.into(),
            sentence_type: sentence_type.into(),
            fields,
            nmea_checksum: NmeaChecksumMode::Correct,
            live_time: true,
            live_time_millis: include_millis,
        }
    }

    fn compile_payload(&self) -> anyhow::Result<CompiledPayload> {
        match self {
            Self::RawHex { data } => compile_hex(data).map(CompiledPayload::Static),
            Self::Utf8 { text } => Ok(CompiledPayload::Static(compile_utf8(text))),
            Self::Utf16 {
                text,
                byte_order,
                bom,
                allow_raw_bytes,
            } => Ok(CompiledPayload::Static(encode_utf16(
                text,
                *byte_order,
                *bom,
                *allow_raw_bytes,
            ))),
            Self::Ascii { text, code_page } => {
                compile_ascii(text, *code_page).map(CompiledPayload::Static)
            }
            Self::Nmea {
                talker,
                sentence_type,
                fields,
                nmea_checksum,
                live_time,
                live_time_millis,
            } => {
                if *live_time {
                    let (talker_id, parsed_type) = parse_nmea_identity(talker, sentence_type)?;
                    anyhow::ensure!(
                        !parsed_type.time_fields().is_empty(),
                        "NMEA sentence type {sentence_type:?} has no defined live UTC fields"
                    );
                    Ok(CompiledPayload::NmeaLive(NmeaLiveTemplate::new(
                        talker_id,
                        parsed_type,
                        fields.clone(),
                        *nmea_checksum,
                        *live_time_millis,
                    )))
                } else {
                    compile_nmea(talker, sentence_type, fields, *nmea_checksum)
                        .map(CompiledPayload::Static)
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum CompiledPayload {
    Static(Vec<u8>),
    NmeaLive(NmeaLiveTemplate),
}

impl CompiledPayload {
    fn wire_len_hint(&self) -> usize {
        match self {
            Self::Static(bytes) => bytes.len(),
            Self::NmeaLive(template) => template.wire_len_hint(),
        }
    }

    fn append_at(&self, now: chrono::DateTime<chrono::Utc>, out: &mut Vec<u8>) {
        match self {
            Self::Static(bytes) => out.extend_from_slice(bytes),
            Self::NmeaLive(template) => template.append_at(now, out),
        }
    }
}

#[derive(Debug, Clone)]
struct NmeaLiveTemplate {
    talker: nmea0183::TalkerId,
    sentence_type: nmea0183::SentenceType,
    fields: Vec<String>,
    nmea_checksum: NmeaChecksumMode,
    include_millis: bool,
    wire_len_hint: usize,
}

impl NmeaLiveTemplate {
    fn new(
        talker: nmea0183::TalkerId,
        sentence_type: nmea0183::SentenceType,
        fields: Vec<String>,
        nmea_checksum: NmeaChecksumMode,
        include_millis: bool,
    ) -> Self {
        let mut template = Self {
            talker,
            sentence_type,
            fields,
            nmea_checksum,
            include_millis,
            wire_len_hint: 0,
        };
        // All substituted fields are fixed width. Render once at compile time
        // so the send path does not stringify IDs or rescan fields for capacity.
        template.wire_len_hint = template.render_at(chrono::Utc::now()).len();
        template
    }

    fn wire_len_hint(&self) -> usize {
        self.wire_len_hint
    }

    fn render_at(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.wire_len_hint());
        self.append_at(now, &mut out);
        out
    }

    fn append_at(&self, now: chrono::DateTime<chrono::Utc>, out: &mut Vec<u8>) {
        use chrono::{Datelike, Timelike};
        use nmea0183::{format_utc_time, NmeaSentence, TimeFieldKind};

        let time_fields = self.sentence_type.time_fields();
        let required_len = time_fields.last().map_or(self.fields.len(), |(index, _)| {
            self.fields.len().max(index + 1)
        });
        let mut fields = self.fields.clone();
        fields.resize(required_len, String::new());

        for &(index, kind) in time_fields {
            fields[index] = match kind {
                TimeFieldKind::UtcTime => format_utc_time(
                    now.hour(),
                    now.minute(),
                    now.second(),
                    now.timestamp_subsec_millis(),
                    self.include_millis,
                ),
                TimeFieldKind::DateDdmmyy => {
                    format!("{:02}{:02}{:02}", now.day(), now.month(), now.year() % 100)
                }
                TimeFieldKind::Day => format!("{:02}", now.day()),
                TimeFieldKind::Month => format!("{:02}", now.month()),
                TimeFieldKind::Year => format!("{:04}", now.year()),
            };
        }

        let sentence = NmeaSentence::new(self.talker.clone(), self.sentence_type.clone(), fields);
        out.extend_from_slice(sentence.to_wire_with(self.nmea_checksum.into()).as_bytes());
    }
}

fn compile_hex(data: &str) -> anyhow::Result<Vec<u8>> {
    // Strict by design: anything but hex digits and the allowed separators
    // (space, hyphen) is rejected — a typo in test data must surface, not be
    // skipped. Iterating chars (not byte slices) keeps non-ASCII input a
    // clean error rather than a slice panic.
    let mut out = Vec::with_capacity(data.len() / 2);
    let mut pending: Option<u8> = None;
    for c in data.chars() {
        if matches!(c, ' ' | '-') {
            continue;
        }
        let digit = c.to_digit(16).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid hex character {c:?} in {data:?} (expected 0-9 A-F, spaces, or hyphens)"
            )
        })? as u8;
        pending = match pending {
            None => Some(digit),
            Some(high) => {
                out.push((high << 4) | digit);
                None
            }
        };
    }
    anyhow::ensure!(
        pending.is_none(),
        "hex string has odd length after stripping separators: {data:?}"
    );
    Ok(out)
}

/// Encode UTF-8 text, expanding `‹XX›` markers to raw bytes (spec §5.3).
fn compile_utf8(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for (range, segment) in segments(text) {
        match segment {
            Segment::Text => out.extend_from_slice(text[range].as_bytes()),
            Segment::Byte(b) => out.push(b),
        }
    }
    out
}

/// Encode ASCII text with `code_page`, expanding `‹XX›` markers to raw bytes.
fn compile_ascii(text: &str, code_page: CodePage) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    for (range, segment) in segments(text) {
        match segment {
            Segment::Text => {
                let chunk = &text[range];
                anyhow::ensure!(
                    !chunk.contains('\u{2039}') && !chunk.contains('\u{203A}'),
                    "text contains '‹' or '›' that isn't part of a complete \
                     ‹XX› byte marker — complete it with two hex digits and a \
                     closing '›' (e.g. ‹1B›), or remove the character"
                );
                out.extend(codepage::encode(chunk, code_page));
            }
            Segment::Byte(b) => out.push(b),
        }
    }
    Ok(out)
}

/// Encode UTF-16 text.
///
/// `allow_raw_bytes = false` (default): the text encodes cleanly via
/// `encode_utf16()` — `‹` / `›` are just two more Unicode characters.
/// The wire is always a whole number of 16-bit code units.
///
/// `allow_raw_bytes = true`: the text is parsed for `‹XX›` byte
/// markers (spec §5.3, same syntax as UTF-8 / ASCII). Each marker
/// emits one raw byte alongside the normal code units. A message
/// with an odd count of marker bytes will end up byte-mis-aligned
/// on the wire — that's the point of the opt-in flag.
fn encode_utf16(text: &str, byte_order: ByteOrder, bom: bool, allow_raw_bytes: bool) -> Vec<u8> {
    let push_unit = |out: &mut Vec<u8>, u: u16| match byte_order {
        ByteOrder::BigEndian => out.extend_from_slice(&u.to_be_bytes()),
        ByteOrder::LittleEndian => out.extend_from_slice(&u.to_le_bytes()),
    };
    let mut out: Vec<u8> = Vec::with_capacity(text.len() * 2 + 2);
    if bom {
        push_unit(&mut out, 0xFEFF);
    }
    if allow_raw_bytes {
        for (range, segment) in segments(text) {
            match segment {
                Segment::Text => {
                    for u in text[range].encode_utf16() {
                        push_unit(&mut out, u);
                    }
                }
                Segment::Byte(b) => out.push(b),
            }
        }
    } else {
        for u in text.encode_utf16() {
            push_unit(&mut out, u);
        }
    }
    out
}

fn compile_nmea(
    talker: &str,
    sentence_type: &str,
    fields: &[String],
    nmea_checksum: NmeaChecksumMode,
) -> anyhow::Result<Vec<u8>> {
    let (talker_id, parsed_type) = parse_nmea_identity(talker, sentence_type)?;
    Ok(compile_parsed_nmea(
        talker_id,
        parsed_type,
        fields,
        nmea_checksum,
    ))
}

fn parse_nmea_identity(
    talker: &str,
    sentence_type: &str,
) -> anyhow::Result<(nmea0183::TalkerId, nmea0183::SentenceType)> {
    use nmea0183::{SentenceType, TalkerId};

    // Arbitrary (non-standard) talker IDs and sentence types are a feature —
    // unknown strings parse into the enums' `Custom` variants. But characters
    // with structural meaning in NMEA framing would corrupt the sentence
    // around the field data, so those are rejected with a pointed error.
    for (what, s) in [("talker ID", talker), ("sentence type", sentence_type)] {
        if let Some(c) = s
            .chars()
            .find(|c| matches!(c, '$' | '!' | ',' | '*' | '\r' | '\n'))
        {
            anyhow::bail!("NMEA {what} {s:?} contains {c:?}, which would corrupt sentence framing");
        }
    }
    // Both `FromStr` impls have `Err = Infallible` (unknown strings become
    // `Custom`), so these expects cannot fire.
    let talker_id: TalkerId = talker.parse().expect("TalkerId parse is infallible");
    let st: SentenceType = sentence_type
        .parse()
        .expect("SentenceType parse is infallible");
    Ok((talker_id, st))
}

fn compile_parsed_nmea(
    talker: nmea0183::TalkerId,
    sentence_type: nmea0183::SentenceType,
    fields: &[String],
    nmea_checksum: NmeaChecksumMode,
) -> Vec<u8> {
    nmea0183::NmeaSentence::new(talker, sentence_type, fields.to_vec())
        .to_wire_with(nmea_checksum.into())
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn utc_at(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
        millis: u32,
    ) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(year, month, day, hour, minute, second)
            .unwrap()
            .with_nanosecond(millis * 1_000_000)
            .unwrap()
    }

    fn parse_rendered_nmea(
        message: &CompiledMessage,
        at: chrono::DateTime<chrono::Utc>,
    ) -> nmea0183::NmeaSentence {
        let wire = String::from_utf8(message.render_at(at)).unwrap();
        nmea0183::NmeaSentence::parse(&wire).unwrap()
    }

    fn render_payload(payload: PayloadConfig) -> anyhow::Result<Vec<u8>> {
        MessageConfig::new(payload, 1)
            .compile()
            .map(|message| message.render_at(utc_at(2026, 1, 1, 0, 0, 0, 0)))
    }

    /// A running channel's capacity is calculated from `wire_len`, so it has to
    /// equal what a send actually puts on the wire — across every combination
    /// of the three terms that make it up.
    #[test]
    fn wire_len_equals_the_rendered_length_it_predicts() {
        let cases = [
            MessageConfig::new(PayloadConfig::raw_hex("DEADBEEF"), 100),
            MessageConfig {
                timestamp: Some(TimestampConfig::default()),
                ..MessageConfig::new(PayloadConfig::raw_hex("DEADBEEF"), 100)
            },
            MessageConfig {
                checksum: Some(ChecksumConfig {
                    algorithm: ChecksumAlgorithm::Crc32,
                    intentionally_wrong: false,
                }),
                ..MessageConfig::new(
                    PayloadConfig::Utf8 {
                        text: "hello".to_owned(),
                    },
                    100,
                )
            },
            MessageConfig {
                timestamp: Some(TimestampConfig::default()),
                checksum: Some(ChecksumConfig {
                    algorithm: ChecksumAlgorithm::Crc16Kermit,
                    intentionally_wrong: true,
                }),
                ..MessageConfig::new(
                    PayloadConfig::Utf8 {
                        text: "hello".to_owned(),
                    },
                    100,
                )
            },
        ];
        for config in cases {
            let compiled = config.compile().expect("compiles");
            assert!(
                compiled.wire_len_matches_render(),
                "predicted {} but rendered {} for {config:?}",
                compiled.wire_len(),
                compiled.render().len()
            );
        }
    }

    // ── RawHex ────────────────────────────────────────────────────────────────

    #[test]
    fn compile_raw_hex_basic() {
        assert_eq!(
            render_payload(PayloadConfig::raw_hex("DEADBEEF")).unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn compile_raw_hex_with_separators() {
        assert_eq!(
            render_payload(PayloadConfig::raw_hex("DE AD-BE EF")).unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn compile_raw_hex_odd_length_errors() {
        assert!(render_payload(PayloadConfig::raw_hex("DEA")).is_err());
    }

    #[test]
    fn compile_raw_hex_invalid_byte_errors() {
        assert!(render_payload(PayloadConfig::raw_hex("DEXZ")).is_err());
    }

    #[test]
    fn compile_raw_hex_accepts_lowercase() {
        assert_eq!(
            render_payload(PayloadConfig::raw_hex("dead beef")).unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn compile_raw_hex_non_ascii_errors_without_panicking() {
        // Multi-byte characters used to make byte-indexed slicing panic
        // mid-character; now they are a clean error.
        for input in ["€€", "DE€D", "0\u{00E9}"] {
            let err = render_payload(PayloadConfig::raw_hex(input)).unwrap_err();
            assert!(
                format!("{err:#}").contains("invalid hex character"),
                "input {input:?} gave: {err:#}"
            );
        }
    }

    #[test]
    fn compile_raw_hex_rejects_sign_characters() {
        // `from_str_radix` used to accept a leading '+' inside a pair
        // ("+F" parsed as 0x0F); strict scanning rejects it.
        assert!(render_payload(PayloadConfig::raw_hex("+F")).is_err());
    }

    // ── UTF-8 ─────────────────────────────────────────────────────────────────

    #[test]
    fn compile_utf8() {
        let p = PayloadConfig::Utf8 {
            text: "héllo".to_string(),
        };
        assert_eq!(render_payload(p).unwrap(), "héllo".as_bytes());
    }

    #[test]
    fn text_formats_preserve_explicit_line_feeds() {
        let utf8 = PayloadConfig::Utf8 {
            text: "first\nsecond".to_string(),
        };
        let ascii = PayloadConfig::Ascii {
            text: "first\nsecond".to_string(),
            code_page: CodePage::Iso8859_1,
        };
        let utf16 = PayloadConfig::Utf16 {
            text: "A\nB".to_string(),
            byte_order: ByteOrder::BigEndian,
            bom: false,
            allow_raw_bytes: false,
        };

        assert_eq!(render_payload(utf8).unwrap(), b"first\nsecond");
        assert_eq!(render_payload(ascii).unwrap(), b"first\nsecond");
        assert_eq!(
            render_payload(utf16).unwrap(),
            vec![0x00, b'A', 0x00, b'\n', 0x00, b'B']
        );
    }

    // ── UTF-16 ────────────────────────────────────────────────────────────────

    #[test]
    fn compile_utf16_big_endian_no_bom() {
        let p = PayloadConfig::Utf16 {
            text: "AB".to_string(),
            byte_order: ByteOrder::BigEndian,
            bom: false,
            allow_raw_bytes: false,
        };
        assert_eq!(render_payload(p).unwrap(), vec![0x00, 0x41, 0x00, 0x42]);
    }

    #[test]
    fn compile_utf16_little_endian_with_bom() {
        let p = PayloadConfig::Utf16 {
            text: "A".to_string(),
            byte_order: ByteOrder::LittleEndian,
            bom: true,
            allow_raw_bytes: false,
        };
        // BOM U+FEFF then 'A' U+0041, little-endian
        assert_eq!(render_payload(p).unwrap(), vec![0xFF, 0xFE, 0x41, 0x00]);
    }

    #[test]
    fn compile_utf16_default_treats_marker_chars_as_literal() {
        // With raw-bytes OFF, `‹FF›` is just five Unicode characters
        // — they all encode normally to UTF-16 (‹ = U+2039, FF as
        // two ASCII chars, › = U+203A).
        let p = PayloadConfig::Utf16 {
            text: "\u{2039}FF\u{203A}".to_string(),
            byte_order: ByteOrder::BigEndian,
            bom: false,
            allow_raw_bytes: false,
        };
        // ‹ → 20 39, 'F' → 00 46, 'F' → 00 46, › → 20 3A
        assert_eq!(
            render_payload(p).unwrap(),
            vec![0x20, 0x39, 0x00, 0x46, 0x00, 0x46, 0x20, 0x3A]
        );
    }

    #[test]
    fn compile_utf16_raw_bytes_expands_markers() {
        // With raw-bytes ON, markers emit single bytes side-by-side
        // with normal UTF-16 code units. Note this can produce an
        // odd byte count — that's the user's call.
        let p = PayloadConfig::Utf16 {
            text: "A\u{2039}FF\u{203A}B".to_string(),
            byte_order: ByteOrder::BigEndian,
            bom: false,
            allow_raw_bytes: true,
        };
        // 'A' → 00 41, marker ‹FF› → FF, 'B' → 00 42  →  5 bytes total
        assert_eq!(
            render_payload(p).unwrap(),
            vec![0x00, 0x41, 0xFF, 0x00, 0x42]
        );
    }

    #[test]
    fn compile_utf16_raw_bytes_with_bom_little_endian() {
        let p = PayloadConfig::Utf16 {
            text: "\u{2039}01\u{203A}\u{2039}02\u{203A}".to_string(),
            byte_order: ByteOrder::LittleEndian,
            bom: true,
            allow_raw_bytes: true,
        };
        // BOM (LE) FF FE, then two raw marker bytes 01 02
        assert_eq!(render_payload(p).unwrap(), vec![0xFF, 0xFE, 0x01, 0x02]);
    }

    #[test]
    fn compile_utf16_surrogate_pair() {
        // U+1F600 encodes as a surrogate pair: D83D DE00
        let p = PayloadConfig::Utf16 {
            text: "\u{1F600}".to_string(),
            byte_order: ByteOrder::BigEndian,
            bom: false,
            allow_raw_bytes: false,
        };
        assert_eq!(render_payload(p).unwrap(), vec![0xD8, 0x3D, 0xDE, 0x00]);
    }

    // ── ASCII / code pages ────────────────────────────────────────────────────

    #[test]
    fn compile_ascii_iso8859_1() {
        let p = PayloadConfig::Ascii {
            text: "café".to_string(),
            code_page: CodePage::Iso8859_1,
        };
        assert_eq!(render_payload(p).unwrap(), vec![b'c', b'a', b'f', 0xE9]);
    }

    #[test]
    fn compile_ascii_replaces_unrepresentable_characters() {
        let p = PayloadConfig::Ascii {
            text: "—…→↔✅".to_string(),
            code_page: CodePage::Iso8859_1,
        };
        assert_eq!(render_payload(p).unwrap(), b"?????");
    }

    #[test]
    fn replacement_summary_counts_occurrences_and_ignores_byte_markers() {
        let summary = code_page_replacements("—‹FF›—✅", CodePage::Iso8859_1).unwrap();
        assert_eq!(summary.count, 3);
        assert_eq!(summary.characters, vec!['—', '✅']);
        assert_eq!(summary.payload_offsets, vec![0, 2, 3]);
    }

    #[test]
    fn compiled_replacement_offsets_include_timestamp_prefix() {
        let message = MessageConfig {
            payload: PayloadConfig::Ascii {
                text: "?—‹FF›✅".to_string(),
                code_page: CodePage::Iso8859_1,
            },
            interval_ms: 100,
            timestamp: Some(TimestampConfig {
                include_date: true,
                include_millis: true,
                include_timezone: true,
            }),
            checksum: None,
        };
        let compiled = message.compile().unwrap();

        assert_eq!(compiled.render_at(chrono::Utc::now()).len(), 24 + 4);
        assert_eq!(compiled.replacement_wire_offsets(), &[25, 27]);
    }

    #[test]
    fn compile_utf8_expands_byte_markers() {
        let p = PayloadConfig::Utf8 {
            text: "AB‹0D›‹0A›".to_string(),
        };
        assert_eq!(render_payload(p).unwrap(), vec![0x41, 0x42, 0x0D, 0x0A]);
    }

    #[test]
    fn compile_ascii_orphan_marker_error_mentions_marker_syntax() {
        // Stray '‹' with no closing '›' + two hex digits — encoder fails
        // because U+2039 isn't in ISO-8859-1, but the user-facing error
        // should point at the marker syntax rather than the raw codepoint.
        let p = PayloadConfig::Ascii {
            text: "hello‹world".to_string(),
            code_page: CodePage::Iso8859_1,
        };
        let err = format!("{:#}", render_payload(p).unwrap_err());
        assert!(err.contains("byte marker"), "error was: {err}");
        assert!(err.contains("‹XX›"), "error was: {err}");
    }

    #[test]
    fn compile_ascii_expands_byte_markers() {
        let p = PayloadConfig::Ascii {
            text: "X‹FF›Y".to_string(),
            code_page: CodePage::Iso8859_1,
        };
        assert_eq!(render_payload(p).unwrap(), vec![0x58, 0xFF, 0x59]);
    }

    // ── NMEA ──────────────────────────────────────────────────────────────────

    #[test]
    fn compile_nmea_wire_format() {
        let p = PayloadConfig::nmea("GP", "GGA", vec!["123519".to_string()]);
        let wire = String::from_utf8(render_payload(p).unwrap()).unwrap();
        assert!(wire.starts_with("$GPGGA,123519*"));
        assert!(wire.ends_with("\r\n"));
    }

    #[test]
    fn compile_nmea_custom_talker_and_sentence_still_compile() {
        // Non-standard IDs are a deliberate capability (Custom variants).
        let p = PayloadConfig::nmea("ZZ", "ABC", vec![]);
        let wire = String::from_utf8(render_payload(p).unwrap()).unwrap();
        assert!(wire.starts_with("$ZZABC"), "wire was: {wire}");
    }

    #[test]
    fn compile_nmea_rejects_framing_characters() {
        // Structural NMEA characters in the talker / sentence-type strings
        // would corrupt the sentence framing — clean error, not silent send.
        for (talker, sentence) in [
            ("G,P", "GGA"),
            ("GP", "GG*A"),
            ("$GP", "GGA"),
            ("GP", "GGA\r\n"),
            ("G!P", "GGA"),
        ] {
            let err = render_payload(PayloadConfig::nmea(talker, sentence, vec![])).unwrap_err();
            assert!(
                format!("{err:#}").contains("sentence framing"),
                "({talker:?},{sentence:?}) gave: {err:#}"
            );
        }
    }

    #[test]
    fn live_rmc_replaces_only_known_fields_on_each_render() {
        let typed = vec![
            "typed-time",
            "A",
            "4916.45",
            "N",
            "12311.12",
            "W",
            "0.5",
            "54.7",
            "typed-date",
            "",
            "A",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let message = MessageConfig::new(
            PayloadConfig::nmea_live("GP", "RMC", typed.clone(), false),
            1000,
        )
        .compile()
        .unwrap();

        let first = parse_rendered_nmea(&message, utc_at(2026, 7, 17, 12, 34, 56, 789));
        let second = parse_rendered_nmea(&message, utc_at(2026, 7, 18, 1, 2, 3, 4));

        let mut expected_first = typed.clone();
        expected_first[0] = "123456".to_string();
        expected_first[8] = "170726".to_string();
        assert_eq!(first.fields, expected_first);

        let mut expected_second = typed;
        expected_second[0] = "010203".to_string();
        expected_second[8] = "180726".to_string();
        assert_eq!(second.fields, expected_second);
    }

    #[test]
    fn live_time_milliseconds_are_opt_in() {
        let without =
            MessageConfig::new(PayloadConfig::nmea_live("GP", "GGA", vec![], false), 1000)
                .compile()
                .unwrap();
        let with = MessageConfig::new(PayloadConfig::nmea_live("GP", "GGA", vec![], true), 1000)
            .compile()
            .unwrap();
        let at = utc_at(2026, 7, 17, 1, 2, 3, 45);

        assert_eq!(parse_rendered_nmea(&without, at).field(0), Some("010203"));
        assert_eq!(parse_rendered_nmea(&with, at).field(0), Some("010203.045"));
    }

    #[test]
    fn live_time_formats_a_chrono_leap_second_without_growing_the_field() {
        let without =
            MessageConfig::new(PayloadConfig::nmea_live("GP", "GGA", vec![], false), 1000)
                .compile()
                .unwrap();
        let with = MessageConfig::new(PayloadConfig::nmea_live("GP", "GGA", vec![], true), 1000)
            .compile()
            .unwrap();
        let leap = utc_at(2016, 12, 31, 23, 59, 59, 1_800);

        assert_eq!(parse_rendered_nmea(&without, leap).field(0), Some("235960"));
        assert_eq!(
            parse_rendered_nmea(&with, leap).field(0),
            Some("235960.800")
        );
    }

    #[test]
    fn prepended_timestamp_and_live_nmea_use_the_same_instant() {
        let message = MessageConfig {
            payload: PayloadConfig::nmea_live("GP", "GGA", vec![], true),
            interval_ms: 1000,
            timestamp: Some(TimestampConfig {
                include_millis: true,
                ..Default::default()
            }),
            checksum: None,
        }
        .compile()
        .unwrap();
        let wire = String::from_utf8(message.render_at(utc_at(2026, 7, 17, 1, 2, 3, 45))).unwrap();

        assert!(
            wire.starts_with("01:02:03.045$GPGGA,010203.045*"),
            "wire was {wire:?}"
        );
    }

    #[test]
    fn live_rmc_extends_short_field_list_through_date() {
        let message =
            MessageConfig::new(PayloadConfig::nmea_live("GN", "RMC", vec![], false), 1000)
                .compile()
                .unwrap();
        let sentence = parse_rendered_nmea(&message, utc_at(2026, 12, 3, 4, 5, 6, 0));

        assert_eq!(sentence.fields.len(), 9);
        assert_eq!(sentence.field(0), Some("040506"));
        assert!(sentence.fields[1..8].iter().all(String::is_empty));
        assert_eq!(sentence.field(8), Some("031226"));
    }

    #[test]
    fn bare_live_zda_emits_complete_utc_date_and_time() {
        let message = MessageConfig::new(PayloadConfig::nmea_live("GP", "ZDA", vec![], true), 1000)
            .compile()
            .unwrap();
        let sentence = parse_rendered_nmea(&message, utc_at(2026, 7, 9, 1, 2, 3, 7));

        assert_eq!(sentence.fields, ["010203.007", "09", "07", "2026"]);
    }

    #[test]
    fn unsupported_live_time_sentence_fails_preflight() {
        for sentence_type in ["HDT", "CUSTOM"] {
            let message = MessageConfig::new(
                PayloadConfig::nmea_live("GP", sentence_type, vec![], false),
                1000,
            );
            let error = format!("{:#}", message.compile().unwrap_err());
            assert!(error.contains("no defined live UTC fields"), "{error}");
            assert!(error.contains(sentence_type), "{error}");
        }
    }

    #[test]
    fn old_nmea_profile_defaults_live_time_flags_off() {
        let json = r#"{"type":"nmea","talker":"GP","sentence_type":"GGA","fields":[]}"#;
        let payload: PayloadConfig = serde_json::from_str(json).unwrap();
        assert_eq!(payload, PayloadConfig::nmea("GP", "GGA", vec![]));
    }

    // ── MessageConfig / CompiledMessage ───────────────────────────────────────

    #[test]
    fn render_plain_payload_is_just_the_payload() {
        let m = MessageConfig::new(PayloadConfig::raw_hex("AABB"), 1000);
        assert_eq!(m.compile().unwrap().render(), vec![0xAA, 0xBB]);
    }

    #[test]
    fn render_appends_checksum_over_payload() {
        let m = MessageConfig {
            payload: PayloadConfig::raw_hex("01 02 03"),
            interval_ms: 1000,
            timestamp: None,
            checksum: Some(ChecksumConfig {
                algorithm: ChecksumAlgorithm::Xor,
                intentionally_wrong: false,
            }),
        };
        // payload 01 02 03, XOR = 00, appended
        assert_eq!(m.compile().unwrap().render(), vec![0x01, 0x02, 0x03, 0x00]);
    }

    #[test]
    fn render_prepends_timestamp_then_payload() {
        let m = MessageConfig {
            payload: PayloadConfig::Utf8 {
                text: "X".to_string(),
            },
            interval_ms: 1000,
            timestamp: Some(TimestampConfig::default()),
            checksum: None,
        };
        let out = m.compile().unwrap().render();
        // "HH:MM:SS" (8 bytes) followed by the payload 'X'
        assert_eq!(out.len(), 9);
        assert_eq!(out[8], b'X');
        assert_eq!(out[2], b':');
    }

    #[test]
    fn render_checksum_covers_timestamp_and_payload() {
        let m = MessageConfig {
            payload: PayloadConfig::raw_hex("FF"),
            interval_ms: 1000,
            timestamp: Some(TimestampConfig::default()),
            checksum: Some(ChecksumConfig {
                algorithm: ChecksumAlgorithm::Xor,
                intentionally_wrong: false,
            }),
        };
        let out = m.compile().unwrap().render();
        // last byte is the XOR of everything before it
        let body_xor = out[..out.len() - 1].iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(*out.last().unwrap(), body_xor);
    }

    #[test]
    fn message_config_round_trip() {
        let m = MessageConfig {
            payload: PayloadConfig::nmea("GP", "RMC", vec![]),
            interval_ms: 500,
            timestamp: Some(TimestampConfig {
                include_date: true,
                include_millis: true,
                include_timezone: true,
            }),
            checksum: Some(ChecksumConfig {
                algorithm: ChecksumAlgorithm::Crc16Kermit,
                intentionally_wrong: false,
            }),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MessageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn message_config_round_trip_defaults_timestamp_checksum() {
        let json = r#"{"payload":{"type":"raw_hex","data":"AB"},"interval_ms":100}"#;
        let m: MessageConfig = serde_json::from_str(json).unwrap();
        assert!(m.timestamp.is_none());
        assert!(m.checksum.is_none());
    }

    // ── decode_utf8_lossy_latin1 ──────────────────────────────────────────────

    #[test]
    fn decode_pure_ascii_passes_through() {
        assert_eq!(decode_utf8_lossy_latin1(b"hello"), "hello");
    }

    #[test]
    fn decode_valid_utf8_decodes() {
        assert_eq!(decode_utf8_lossy_latin1("héllo".as_bytes()), "héllo");
    }

    #[test]
    fn decode_lone_high_byte_falls_back_to_latin1() {
        // 0xEE → î (U+00EE)
        assert_eq!(decode_utf8_lossy_latin1(&[0xEE]), "\u{00EE}");
        // 0xFF → ÿ (U+00FF)
        assert_eq!(decode_utf8_lossy_latin1(&[0xFF]), "\u{00FF}");
    }

    #[test]
    fn decode_mixes_utf8_and_high_bytes() {
        // "A" (ASCII) + 0xEE (invalid UTF-8) + "B" (ASCII)
        assert_eq!(decode_utf8_lossy_latin1(b"A\xEEB"), "A\u{00EE}B");
    }

    #[test]
    fn decode_empty_input_yields_empty() {
        assert_eq!(decode_utf8_lossy_latin1(b""), "");
    }

    #[test]
    fn payload_round_trips() {
        for p in [
            PayloadConfig::raw_hex("AABB"),
            PayloadConfig::Utf8 {
                text: "hello".to_string(),
            },
            PayloadConfig::Utf16 {
                text: "hi".to_string(),
                byte_order: ByteOrder::LittleEndian,
                bom: true,
                allow_raw_bytes: false,
            },
            PayloadConfig::Ascii {
                text: "x".to_string(),
                code_page: CodePage::Windows1252,
            },
            PayloadConfig::nmea("GP", "GGA", vec!["f".to_string()]),
            PayloadConfig::nmea_live("GN", "RMC", vec![], true),
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let back: PayloadConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back);
        }
    }
}
