use std::fmt;

use crate::checksum;
use crate::error::NmeaError;
use crate::sentence_type::SentenceType;
use crate::talker_id::TalkerId;

/// How the trailing `*XX` checksum is rendered when serializing a
/// [`NmeaSentence`]. Used by [`NmeaSentence::to_wire_with`].
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum NmeaChecksumMode {
    /// Append `*XX\r\n` with the correct XOR checksum (protocol default).
    #[default]
    Correct,
    /// Append only `\r\n` — no `*XX` suffix at all.
    Omit,
    /// Append `*XX\r\n` with an intentionally-wrong byte (correct ^ 0xFF).
    /// Useful for testing how a downstream parser reacts to a bad checksum.
    Wrong,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct NmeaSentence {
    pub talker_id: TalkerId,
    pub sentence_type: SentenceType,
    pub fields: Vec<String>,
}

impl NmeaSentence {
    pub fn new(talker_id: TalkerId, sentence_type: SentenceType, fields: Vec<String>) -> Self {
        Self {
            talker_id,
            sentence_type,
            fields,
        }
    }

    /// Return the field at `index`, or `None` if out of range.
    pub fn field(&self, index: usize) -> Option<&str> {
        self.fields.get(index).map(String::as_str)
    }

    /// Compute the XOR checksum for this sentence.
    pub fn checksum(&self) -> u8 {
        checksum::xor(self.payload().as_bytes())
    }

    /// Serialize to wire format with a correct checksum:
    /// `$<talker><type>,<f1>,...*XX\r\n`.
    ///
    /// Equivalent to [`Self::to_wire_with`] called with
    /// [`NmeaChecksumMode::Correct`].
    pub fn to_wire(&self) -> String {
        self.to_wire_with(NmeaChecksumMode::Correct)
    }

    /// Serialize to wire format with caller-selected checksum handling.
    ///
    /// - [`NmeaChecksumMode::Correct`]: `$...*XX\r\n` with the correct XOR.
    /// - [`NmeaChecksumMode::Omit`]:    `$...\r\n` with no `*XX` suffix.
    /// - [`NmeaChecksumMode::Wrong`]:   `$...*XX\r\n` with a deliberately
    ///   wrong byte (correct ^ 0xFF) — for negative testing of parsers.
    pub fn to_wire_with(&self, mode: NmeaChecksumMode) -> String {
        let payload = self.payload();
        let cs = checksum::xor(payload.as_bytes());
        match mode {
            NmeaChecksumMode::Correct => format!("${payload}*{cs:02X}\r\n"),
            NmeaChecksumMode::Omit => format!("${payload}\r\n"),
            NmeaChecksumMode::Wrong => format!("${}*{:02X}\r\n", payload, cs ^ 0xFF),
        }
    }

    /// Parse a NMEA sentence string. Accepts lines with or without `\r\n`.
    /// Validates the checksum, which must be exactly two hex digits, and rejects
    /// non-ASCII input (ADR-058). Does not accept proprietary `$P...` sentences.
    pub fn parse(line: &str) -> Result<Self, NmeaError> {
        let line = line.trim_end_matches(['\r', '\n']);
        crate::require_ascii(line)?;
        let rest = line
            .strip_prefix('$')
            .ok_or(NmeaError::MissingStartDelimiter)?;

        if rest.starts_with('P') {
            return Err(NmeaError::Parse(
                "use ProprietarySentence::parse for $P sentences".to_string(),
            ));
        }

        // Split checksum suffix
        let (body, chk) = match rest.rsplit_once('*') {
            Some((b, c)) => (b, c),
            None => return Err(NmeaError::MissingChecksum),
        };

        let expected = checksum::parse_suffix(chk)?;
        let computed = checksum::xor(body.as_bytes());
        if expected != computed {
            return Err(NmeaError::InvalidChecksum { expected, computed });
        }

        // Split header (talker + type) from the field list. A comma starts the
        // list even when nothing follows it, so `$GPHDT,*..` keeps its one empty
        // field and round-trips (ADR-058).
        let (header, fields) = match body.split_once(',') {
            Some((h, f)) => (h, f.split(',').map(str::to_string).collect()),
            None => (body, Vec::new()),
        };

        // Standard sentences: talker is 2 chars, sentence type is 3 chars → header is 5 chars.
        if header.len() < 5 {
            return Err(NmeaError::InvalidTalkerId(header.to_string()));
        }
        let (talker_str, type_str) = header.split_at(header.len() - 3);
        let talker_id: TalkerId = talker_str.parse().unwrap();
        let sentence_type: SentenceType = type_str.parse().unwrap();

        Ok(Self {
            talker_id,
            sentence_type,
            fields,
        })
    }

    fn payload(&self) -> String {
        let mut s = format!("{}{}", self.talker_id, self.sentence_type);
        for field in &self.fields {
            s.push(',');
            s.push_str(field);
        }
        s
    }
}

impl fmt::Display for NmeaSentence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- construction ---

    #[test]
    fn construct_gga() {
        let fields = vec![
            "123519".to_string(),
            "4807.038".to_string(),
            "N".to_string(),
            "01131.000".to_string(),
            "E".to_string(),
            "1".to_string(),
            "08".to_string(),
            "0.9".to_string(),
            "545.4".to_string(),
            "M".to_string(),
            "46.9".to_string(),
            "M".to_string(),
            "".to_string(),
            "".to_string(),
        ];
        let s = NmeaSentence::new(TalkerId::GP, SentenceType::GGA, fields);
        assert_eq!(
            s.to_wire(),
            "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n"
        );
    }

    #[test]
    fn to_wire_with_omit_drops_checksum_suffix() {
        let s = NmeaSentence::new(
            TalkerId::GP,
            SentenceType::HDT,
            vec!["123.4".into(), "T".into()],
        );
        let correct = s.to_wire_with(NmeaChecksumMode::Correct);
        let omitted = s.to_wire_with(NmeaChecksumMode::Omit);
        assert!(correct.contains('*'));
        assert!(!omitted.contains('*'));
        assert!(omitted.ends_with("\r\n"));
        assert_eq!(omitted, "$GPHDT,123.4,T\r\n");
    }

    #[test]
    fn to_wire_with_wrong_flips_checksum() {
        let s = NmeaSentence::new(
            TalkerId::GP,
            SentenceType::HDT,
            vec!["123.4".into(), "T".into()],
        );
        let correct = s.to_wire_with(NmeaChecksumMode::Correct);
        let wrong = s.to_wire_with(NmeaChecksumMode::Wrong);
        // Same length and shape, only the checksum byte differs.
        assert_eq!(correct.len(), wrong.len());
        assert_ne!(correct, wrong);
        // The wrong wire must fail parsing as InvalidChecksum, never anything else.
        match NmeaSentence::parse(&wrong) {
            Err(NmeaError::InvalidChecksum { .. }) => {}
            other => panic!("expected InvalidChecksum, got {other:?}"),
        }
    }

    #[test]
    fn to_wire_default_matches_correct_mode() {
        let s = NmeaSentence::new(
            TalkerId::GP,
            SentenceType::HDT,
            vec!["123.4".into(), "T".into()],
        );
        assert_eq!(s.to_wire(), s.to_wire_with(NmeaChecksumMode::Correct));
    }

    #[test]
    fn construct_rmc() {
        // $GPRMC,220516,A,5133.82,N,00042.24,W,173.8,231.8,130694,004.2,W*70
        let fields = vec![
            "220516".to_string(),
            "A".to_string(),
            "5133.82".to_string(),
            "N".to_string(),
            "00042.24".to_string(),
            "W".to_string(),
            "173.8".to_string(),
            "231.8".to_string(),
            "130694".to_string(),
            "004.2".to_string(),
            "W".to_string(),
        ];
        let s = NmeaSentence::new(TalkerId::GP, SentenceType::RMC, fields);
        let wire = s.to_wire();
        // Checksum must be valid — parse it back.
        let parsed = NmeaSentence::parse(&wire).unwrap();
        assert_eq!(parsed.talker_id, TalkerId::GP);
        assert_eq!(parsed.sentence_type, SentenceType::RMC);
    }

    #[test]
    fn construct_custom_talker_and_type() {
        let s = NmeaSentence::new(
            TalkerId::Custom("HE".to_string()),
            SentenceType::Custom("HDG".to_string()),
            vec!["359.9".to_string()],
        );
        let wire = s.to_wire();
        assert!(wire.starts_with("$HEHDG,359.9*"));
    }

    #[test]
    fn empty_fields() {
        let s = NmeaSentence::new(TalkerId::GP, SentenceType::GLL, vec![]);
        let wire = s.to_wire();
        let parsed = NmeaSentence::parse(&wire).unwrap();
        assert!(parsed.fields.is_empty());
    }

    // --- parsing ---

    #[test]
    fn parse_gga_with_crlf() {
        let line = "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n";
        let s = NmeaSentence::parse(line).unwrap();
        assert_eq!(s.talker_id, TalkerId::GP);
        assert_eq!(s.sentence_type, SentenceType::GGA);
        assert_eq!(s.field(0), Some("123519"));
        assert_eq!(s.field(13), Some(""));
    }

    #[test]
    fn parse_gnss_talker() {
        let s = NmeaSentence::new(TalkerId::GN, SentenceType::RMC, vec!["field".to_string()]);
        let parsed = NmeaSentence::parse(&s.to_wire()).unwrap();
        assert_eq!(parsed.talker_id, TalkerId::GN);
    }

    #[test]
    fn parse_bad_checksum() {
        // Flip last hex digit
        let line = "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*48\r\n";
        let err = NmeaSentence::parse(line).unwrap_err();
        assert!(matches!(err, NmeaError::InvalidChecksum { .. }));
    }

    #[test]
    fn parse_missing_checksum() {
        let err = NmeaSentence::parse("$GPGGA,123519").unwrap_err();
        assert!(matches!(err, NmeaError::MissingChecksum));
    }

    #[test]
    fn parse_missing_dollar() {
        let err = NmeaSentence::parse("GPGGA,123519*47").unwrap_err();
        assert!(matches!(err, NmeaError::MissingStartDelimiter));
    }

    #[test]
    fn parse_rejects_proprietary() {
        let err = NmeaSentence::parse("$PASHR,045.67,T*XX").unwrap_err();
        assert!(matches!(err, NmeaError::Parse(_)));
    }

    #[test]
    fn parse_rejects_non_ascii_without_panicking() {
        // 'é' is two bytes, and the talker/type split falls inside it.
        let body = "GP\u{e9}GA,1";
        let line = format!("${body}*{:02X}", checksum::xor(body.as_bytes()));
        let err = NmeaSentence::parse(&line).unwrap_err();
        assert!(matches!(err, NmeaError::Parse(_)));
    }

    #[test]
    fn parse_requires_exactly_two_hex_checksum_digits() {
        let body = "GPHDT,123.4,T";
        let cs = checksum::xor(body.as_bytes());
        for suffix in [format!("00{cs:02X}"), format!("+{cs:02X}")] {
            let err = NmeaSentence::parse(&format!("${body}*{suffix}")).unwrap_err();
            assert!(matches!(err, NmeaError::Parse(_)), "suffix {suffix:?}");
        }
        assert!(NmeaSentence::parse(&format!("${body}*{cs:02x}")).is_ok());
    }

    #[test]
    fn an_explicit_empty_field_round_trips() {
        let s = NmeaSentence::new(TalkerId::GP, SentenceType::HDT, vec![String::new()]);
        let wire = s.to_wire();
        assert!(wire.starts_with("$GPHDT,*"));
        assert_eq!(NmeaSentence::parse(&wire).unwrap(), s);
    }

    #[test]
    fn field_out_of_range() {
        let s = NmeaSentence::new(TalkerId::GP, SentenceType::GGA, vec!["a".to_string()]);
        assert_eq!(s.field(0), Some("a"));
        assert_eq!(s.field(1), None);
    }
}
