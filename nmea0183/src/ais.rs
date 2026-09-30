//! AIS (Automatic Identification System) VHF data-link sentences.
//!
//! AIS sentences start with `!` (not `$`), use the `AI` talker, and carry a
//! 6-bit ASCII armored binary payload. This module handles the sentence
//! envelope and the armoring codec ([`armor`] / [`unarmor`]); it does not
//! interpret the AIS message bitstream itself.

use std::fmt;

use crate::checksum;
use crate::error::NmeaError;

/// An AIS VHF data-link sentence — `!AIVDM` (received from other vessels) or
/// `!AIVDO` (own-vessel report).
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AisSentence {
    /// `false` → `!AIVDM`; `true` → `!AIVDO`.
    pub own_vessel: bool,
    /// Total number of fragments this message spans (usually 1).
    pub fragment_count: u32,
    /// 1-based index of this fragment within the message.
    pub fragment_number: u32,
    /// Sequential message ID, present for multi-fragment messages.
    pub sequence_id: Option<u32>,
    /// Radio channel (`"A"`, `"B"`, `"1"`, `"2"`, or empty).
    pub channel: String,
    /// The 6-bit ASCII armored data payload.
    pub payload: String,
    /// Number of fill (padding) bits in the final 6-bit group (0–5).
    pub fill_bits: u8,
}

impl AisSentence {
    /// Build a single-fragment AIS sentence from raw binary `data`.
    ///
    /// The data is 6-bit armored automatically.
    pub fn new(own_vessel: bool, channel: impl Into<String>, data: &[u8]) -> Self {
        let (payload, fill_bits) = armor(data);
        Self {
            own_vessel,
            fragment_count: 1,
            fragment_number: 1,
            sequence_id: None,
            channel: channel.into(),
            payload,
            fill_bits,
        }
    }

    /// The sentence formatter identifier: `"AIVDM"` or `"AIVDO"`.
    pub fn formatter(&self) -> &'static str {
        if self.own_vessel {
            "AIVDO"
        } else {
            "AIVDM"
        }
    }

    /// Serialize to wire format including `\r\n`:
    /// `!AIVDM,count,num,seq,channel,payload,fill*CS`
    pub fn to_wire(&self) -> String {
        let seq = self.sequence_id.map(|s| s.to_string()).unwrap_or_default();
        let body = format!(
            "{},{},{},{},{},{},{}",
            self.formatter(),
            self.fragment_count,
            self.fragment_number,
            seq,
            self.channel,
            self.payload,
            self.fill_bits,
        );
        let cs = checksum::xor(body.as_bytes());
        format!("!{body}*{cs:02X}\r\n")
    }

    /// Parse an AIS sentence string. Accepts lines with or without `\r\n`.
    /// Validates the checksum, which must be exactly two hex digits, and
    /// rejects non-ASCII input and out-of-range fill bits and fragment numbers
    /// (ADR-058).
    pub fn parse(line: &str) -> Result<Self, NmeaError> {
        let line = line.trim_end_matches(['\r', '\n']);
        crate::require_ascii(line)?;
        let rest = line
            .strip_prefix('!')
            .ok_or(NmeaError::MissingStartDelimiter)?;

        let (body, chk) = rest.rsplit_once('*').ok_or(NmeaError::MissingChecksum)?;
        let expected = checksum::parse_suffix(chk)?;
        let computed = checksum::xor(body.as_bytes());
        if expected != computed {
            return Err(NmeaError::InvalidChecksum { expected, computed });
        }

        let parts: Vec<&str> = body.split(',').collect();
        if parts.len() != 7 {
            return Err(NmeaError::Parse(format!(
                "AIS sentence has {} comma-separated fields, expected 7",
                parts.len()
            )));
        }

        let own_vessel = match parts[0] {
            "AIVDM" => false,
            "AIVDO" => true,
            other => {
                return Err(NmeaError::Parse(format!(
                    "not an AIS sentence: formatter {other:?}"
                )))
            }
        };

        let fragment_count = parse_u32(parts[1], "fragment count")?;
        let fragment_number = parse_u32(parts[2], "fragment number")?;
        if fragment_number == 0 || fragment_number > fragment_count {
            return Err(NmeaError::Parse(format!(
                "fragment {fragment_number} of {fragment_count} is out of range"
            )));
        }
        let fill_bits = parts[6]
            .parse::<u8>()
            .ok()
            .filter(|bits| *bits <= 5)
            .ok_or_else(|| NmeaError::Parse(format!("invalid fill bits: {:?}", parts[6])))?;

        Ok(Self {
            own_vessel,
            fragment_count,
            fragment_number,
            sequence_id: if parts[3].is_empty() {
                None
            } else {
                Some(parse_u32(parts[3], "sequence id")?)
            },
            channel: parts[4].to_string(),
            payload: parts[5].to_string(),
            fill_bits,
        })
    }
}

impl fmt::Display for AisSentence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_wire())
    }
}

fn parse_u32(s: &str, name: &str) -> Result<u32, NmeaError> {
    s.parse::<u32>()
        .map_err(|_| NmeaError::Parse(format!("invalid {name}: {s:?}")))
}

// ── 6-bit ASCII armor codec ───────────────────────────────────────────────────

/// 6-bit ASCII armor `data` into an AIS payload string.
///
/// Returns the armored payload and the number of fill (padding) bits added to
/// the final 6-bit group.
pub fn armor(data: &[u8]) -> (String, u8) {
    let total_bits = data.len() * 8;
    let mut out = String::new();
    let mut bit = 0;
    while bit < total_bits {
        let mut value = 0u8;
        for i in 0..6 {
            let b = bit + i;
            let one = if b < total_bits {
                (data[b / 8] >> (7 - b % 8)) & 1
            } else {
                0
            };
            value = (value << 1) | one;
        }
        out.push(sixbit_to_char(value));
        bit += 6;
    }
    // Each armored char is one ASCII byte, so `out.len()` is the char count.
    let fill = (out.len() * 6 - total_bits) as u8;
    (out, fill)
}

/// Reverse [`armor`]: decode an armored AIS payload back to bytes.
///
/// `fill_bits` is dropped from the end. If the remaining bit length is not a
/// multiple of 8 the final byte is zero-padded in its low bits.
pub fn unarmor(payload: &str, fill_bits: u8) -> Result<Vec<u8>, NmeaError> {
    let mut bits: Vec<u8> = Vec::with_capacity(payload.len() * 6);
    for c in payload.chars() {
        let value = char_to_sixbit(c)?;
        for shift in (0..6).rev() {
            bits.push((value >> shift) & 1);
        }
    }
    let keep = bits.len().saturating_sub(fill_bits as usize);
    bits.truncate(keep);

    let mut out = Vec::with_capacity(bits.len().div_ceil(8));
    for chunk in bits.chunks(8) {
        let mut byte = 0u8;
        for (i, &one) in chunk.iter().enumerate() {
            byte |= one << (7 - i);
        }
        out.push(byte);
    }
    Ok(out)
}

/// Encode a 6-bit value (0–63) to its armor character.
fn sixbit_to_char(value: u8) -> char {
    let mut c = value + 48;
    if c > 87 {
        c += 8;
    }
    c as char
}

/// Decode an armor character to its 6-bit value (0–63).
fn char_to_sixbit(c: char) -> Result<u8, NmeaError> {
    let n = c as u32;
    let value = match n {
        48..=87 => n - 48,
        96..=119 => n - 56,
        _ => {
            return Err(NmeaError::Parse(format!(
                "invalid 6-bit armor character {c:?}"
            )))
        }
    };
    Ok(value as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── armor / unarmor ───────────────────────────────────────────────────────

    #[test]
    fn armor_all_zero_byte() {
        // 8 bits of zero -> two 6-bit groups, 4 fill bits.
        assert_eq!(armor(&[0x00]), ("00".to_string(), 4));
    }

    #[test]
    fn armor_known_byte() {
        // 0xFF = 1111_1111 -> group 111111 (63 -> 'w'), group 110000 (48 -> 'h'),
        // with 4 fill bits in the second group.
        assert_eq!(armor(&[0xFF]), ("wh".to_string(), 4));
    }

    #[test]
    fn armor_empty() {
        assert_eq!(armor(&[]), (String::new(), 0));
    }

    #[test]
    fn armor_unarmor_round_trip() {
        for data in [
            &b""[..],
            &b"\x00"[..],
            &b"\xFF"[..],
            &b"\x01\x02\x03"[..],
            &b"AIS test payload"[..],
            &[0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34][..],
        ] {
            let (payload, fill) = armor(data);
            assert_eq!(
                unarmor(&payload, fill).unwrap(),
                data,
                "round trip {data:?}"
            );
        }
    }

    #[test]
    fn unarmor_rejects_invalid_character() {
        // '!' (0x21) is outside both armor ranges.
        assert!(unarmor("!", 0).is_err());
    }

    // ── AisSentence ───────────────────────────────────────────────────────────

    #[test]
    fn to_wire_aivdm_shape() {
        let s = AisSentence::new(false, "A", &[0x01, 0x02, 0x03]);
        let wire = s.to_wire();
        assert!(wire.starts_with("!AIVDM,1,1,,A,"));
        assert!(wire.contains('*'));
        assert!(wire.ends_with("\r\n"));
    }

    #[test]
    fn to_wire_aivdo_for_own_vessel() {
        let s = AisSentence::new(true, "B", &[0xAA]);
        assert!(s.to_wire().starts_with("!AIVDO,"));
    }

    #[test]
    fn round_trip_single_fragment() {
        let s = AisSentence::new(false, "A", &[0xDE, 0xAD, 0xBE, 0xEF]);
        let parsed = AisSentence::parse(&s.to_wire()).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn round_trip_multi_fragment() {
        let mut s = AisSentence::new(true, "B", &[0x11, 0x22]);
        s.fragment_count = 2;
        s.fragment_number = 2;
        s.sequence_id = Some(7);
        let parsed = AisSentence::parse(&s.to_wire()).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn parse_recovers_payload_bytes() {
        let data = [0x48, 0x69, 0x21];
        let s = AisSentence::new(false, "A", &data);
        let parsed = AisSentence::parse(&s.to_wire()).unwrap();
        assert_eq!(unarmor(&parsed.payload, parsed.fill_bits).unwrap(), data);
    }

    #[test]
    fn parse_rejects_missing_bang() {
        let err = AisSentence::parse("AIVDM,1,1,,A,000,0*00").unwrap_err();
        assert!(matches!(err, NmeaError::MissingStartDelimiter));
    }

    #[test]
    fn parse_rejects_missing_checksum() {
        let err = AisSentence::parse("!AIVDM,1,1,,A,000,0").unwrap_err();
        assert!(matches!(err, NmeaError::MissingChecksum));
    }

    #[test]
    fn parse_rejects_bad_checksum() {
        let mut wire = AisSentence::new(false, "A", &[0x01]).to_wire();
        // Corrupt the last checksum digit.
        wire = wire.trim_end().to_string();
        let last = wire.pop().unwrap();
        wire.push(if last == '0' { '1' } else { '0' });
        let err = AisSentence::parse(&wire).unwrap_err();
        assert!(matches!(
            err,
            NmeaError::InvalidChecksum { .. } | NmeaError::Parse(_)
        ));
    }

    #[test]
    fn parse_rejects_wrong_field_count() {
        // checksum of "AIVDM,1,1" is computed so the parse reaches the field check
        let body = "AIVDM,1,1";
        let cs = checksum::xor(body.as_bytes());
        let line = format!("!{body}*{cs:02X}");
        let err = AisSentence::parse(&line).unwrap_err();
        assert!(matches!(err, NmeaError::Parse(_)));
    }

    #[test]
    fn parse_rejects_non_ais_formatter() {
        let body = "GPGGA,1,1,,A,000,0";
        let cs = checksum::xor(body.as_bytes());
        let line = format!("!{body}*{cs:02X}");
        let err = AisSentence::parse(&line).unwrap_err();
        assert!(matches!(err, NmeaError::Parse(_)));
    }

    /// `!body*CS` with the body's true checksum, so a parse reaches the field checks.
    fn line_with_checksum(body: &str) -> String {
        format!("!{body}*{:02X}", checksum::xor(body.as_bytes()))
    }

    #[test]
    fn parse_rejects_out_of_range_fill_bits_and_fragments() {
        for body in [
            "AIVDM,1,1,,A,000,6", // fill bits above 5
            "AIVDM,0,0,,A,000,0", // no fragments
            "AIVDM,1,0,,A,000,0", // fragment numbers start at 1
            "AIVDM,2,3,,A,000,0", // fragment 3 of 2
        ] {
            let err = AisSentence::parse(&line_with_checksum(body)).unwrap_err();
            assert!(matches!(err, NmeaError::Parse(_)), "{body}");
        }
        assert!(AisSentence::parse(&line_with_checksum("AIVDM,2,2,7,B,000,5")).is_ok());
    }

    #[test]
    fn parse_rejects_non_ascii_and_loose_checksum_digits() {
        let err = AisSentence::parse(&line_with_checksum("AIVDM,1,1,,A,0\u{e9}0,0")).unwrap_err();
        assert!(matches!(err, NmeaError::Parse(_)));
        let body = "AIVDM,1,1,,A,000,0";
        let cs = checksum::xor(body.as_bytes());
        for line in [format!("!{body}*00{cs:02X}"), format!("!{body}*+{cs:02X}")] {
            let err = AisSentence::parse(&line).unwrap_err();
            assert!(matches!(err, NmeaError::Parse(_)), "{line:?}");
        }
    }

    #[test]
    fn formatter_strings() {
        assert_eq!(AisSentence::new(false, "A", &[]).formatter(), "AIVDM");
        assert_eq!(AisSentence::new(true, "A", &[]).formatter(), "AIVDO");
    }
}
