pub mod ais;
pub mod checksum;
pub mod sentence_type;
pub mod talker_id;
pub mod time;

mod error;
mod proprietary;
mod sentence;

pub use ais::AisSentence;
pub use error::NmeaError;
pub use proprietary::{PashrData, PrdidData, ProprietarySentence};
pub use sentence::{NmeaChecksumMode, NmeaSentence};
pub use sentence_type::{SentenceType, TimeFieldKind};
pub use talker_id::TalkerId;
pub use time::format_utc_time;

/// The result of parsing any NMEA sentence.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub enum AnyNmeaSentence {
    Standard(NmeaSentence),
    Proprietary(ProprietarySentence),
    Ais(AisSentence),
}

/// Sentences are ASCII (ADR-058). Every parser checks this before slicing at
/// byte offsets, so a multibyte character can never be split.
pub(crate) fn require_ascii(line: &str) -> Result<(), NmeaError> {
    if line.is_ascii() {
        Ok(())
    } else {
        Err(NmeaError::Parse(
            "sentence contains non-ASCII characters".to_string(),
        ))
    }
}

/// Parse any NMEA sentence, dispatching by its start delimiter:
/// `!` → AIS, `$P` → proprietary, `$` → standard.
pub fn parse(line: &str) -> Result<AnyNmeaSentence, NmeaError> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    match trimmed.as_bytes().first() {
        Some(b'!') => AisSentence::parse(line).map(AnyNmeaSentence::Ais),
        Some(b'$') => {
            if trimmed[1..].starts_with('P') {
                ProprietarySentence::parse(line).map(AnyNmeaSentence::Proprietary)
            } else {
                NmeaSentence::parse(line).map(AnyNmeaSentence::Standard)
            }
        }
        _ => Err(NmeaError::MissingStartDelimiter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_standard() {
        let wire = NmeaSentence::new(TalkerId::GP, SentenceType::GGA, vec![]).to_wire();
        assert!(matches!(
            parse(&wire).unwrap(),
            AnyNmeaSentence::Standard(_)
        ));
    }

    #[test]
    fn dispatch_proprietary_prdid() {
        let wire = ProprietarySentence::Prdid(PrdidData {
            pitch: 1.0,
            roll: 2.0,
            heading: 3.0,
        })
        .to_wire();
        assert!(matches!(
            parse(&wire).unwrap(),
            AnyNmeaSentence::Proprietary(_)
        ));
    }

    #[test]
    fn dispatch_proprietary_raw() {
        let wire = ProprietarySentence::Raw {
            identifier: "FOO".to_string(),
            fields: vec![],
        }
        .to_wire();
        assert!(matches!(
            parse(&wire).unwrap(),
            AnyNmeaSentence::Proprietary(_)
        ));
    }

    #[test]
    fn dispatch_ais() {
        let wire = AisSentence::new(false, "A", &[0x01, 0x02, 0x03]).to_wire();
        assert!(matches!(parse(&wire).unwrap(), AnyNmeaSentence::Ais(_)));
    }

    #[test]
    fn dispatch_unknown_delimiter() {
        let err = parse("GPGGA*47").unwrap_err();
        assert!(matches!(err, NmeaError::MissingStartDelimiter));
    }

    #[test]
    fn dispatch_empty_line() {
        let err = parse("").unwrap_err();
        assert!(matches!(err, NmeaError::MissingStartDelimiter));
    }
}
