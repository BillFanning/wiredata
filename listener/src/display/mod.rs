//! Raw, rendered, and hex display rendering.
//!
//! This is `listener-display` (spec §128, §40–§49). Display is presentation
//! only: it never alters bytes or recordings (§5.4, §40).
//! The rendering pipeline is pure (no I/O, no async): bytes are decoded per the
//! Display Encoding (§47) into characters, each character is rendered per the
//! Character Rendering mode (§46), and the Display Mode (§42) assembles them as
//! Raw, Rendered, or Hex with optional wrapping.
//!
//! The persisted [`DisplayViewConfig`](crate::config) selects these settings;
//! purely visual options (font, colors) do not affect the produced text and so
//! live only in that config. The enums here are defined by display (their
//! functional owner); the config module will reference them.

mod encoding;
mod render;

pub(crate) use encoding::valid_utf8_prefix;
pub use render::{AnnotationPlacement, DisplayView, RenderAnnotation, StreamRenderer};

use crate::core::{ChannelId, ChunkTime};

/// Display Mode — how received data is assembled for a view (§42).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DisplayMode {
    Raw,
    Rendered,
    Hex,
}

/// Display Encoding — how bytes are decoded into characters (§47, §80.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DisplayEncoding {
    Ascii,
    Utf8,
    Utf16Le,
    Utf16Be,
    Latin1,
}

/// Character Rendering — how each character is displayed (§46, §80.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CharacterRendering {
    /// The actual character, unchanged (controls are passed through).
    Native,
    /// `[CR]` `[LF]` `[TAB]` `[NUL]` `[SP]` …
    Token,
    /// `␍` `␊` `␉` `␀` `␠` (Unicode Control Pictures).
    Glyph,
    /// `<0D>` `<0A>` `<09>` `<00>` `<20>`
    HexEscape,
}

/// Wrapping Mode for a view (§80.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WrappingMode {
    NoWrap,
    Wrap,
}

/// The rendered representation of a span of stream bytes for one Display View
/// (§141).
///
/// Display Recording consumes this *after* rendering (§54) — it is explicitly
/// not byte-exact and is not a substitute for Raw Recording. Any inline Mark
/// annotations (§50.2 compact time or NMEA ZDA) are spliced into `text`; the
/// `timestamp` here is the chunk's arrival time, used to drive **time-based rotation**
/// (§59) — a rotating display recorder picks the period file from it.
#[derive(Clone, Debug)]
pub struct RenderedOutput {
    pub channel_id: ChannelId,
    /// The rendered text for this view (Raw/Rendered/Hex, §42).
    pub text: String,
    /// The rendered chunk's arrival [`ChunkTime`] — drives time-based rotation
    /// (§59): a rotating display recorder picks the period file from it. `None`
    /// (e.g. a `‹MARK …›` marker line, which has no source chunk) falls back to
    /// `now()` for rotation.
    pub timestamp: Option<ChunkTime>,
}
