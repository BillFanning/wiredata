use std::fmt;

use crate::checksum;
use crate::error::NmeaError;

/// Data carried by `$PRDID` sentences.
///
/// `$PRDID` (Teledyne RDI ADCP) does not include a checksum by convention.
/// Wire format: `$PRDID,{pitch},{roll},{heading}\r\n`
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PrdidData {
    /// Pitch in degrees (positive = bow up).
    pub pitch: f64,
    /// Roll in degrees (positive = starboard up).
    pub roll: f64,
    /// True heading in degrees (0–359.99).
    pub heading: f64,
}

/// Data carried by `$PASHR` sentences.
///
/// Wire format (with UTC):    `$PASHR,HHMMSS.ss,HHH.HH,T,RRR.RR,PPP.PP,aaa.aa,r.rrr,p.ppp,ss.ss,Q*CS\r\n`
/// Wire format (without UTC): `$PASHR,HHH.HH,T,RRR.RR,PPP.PP,aaa.aa,r.rrr,p.ppp,ss.ss,Q*CS\r\n`
///
/// Field 10 (`gnss_quality`) is raw `u8`; Trimble and Novatel define its values differently.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PashrData {
    /// UTC time string e.g. `"123519.00"`; `None` if omitted.
    pub utc_time: Option<String>,
    /// True heading in degrees (0–359.99).
    pub heading: f64,
    /// Roll in degrees (positive = starboard up).
    pub roll: f64,
    /// Pitch in degrees (positive = bow up).
    pub pitch: f64,
    /// Heave in metres (positive = up).
    pub heave: f64,
    /// Roll accuracy (1-sigma, degrees).
    pub roll_accuracy: f64,
    /// Pitch accuracy (1-sigma, degrees).
    pub pitch_accuracy: f64,
    /// Heading accuracy (1-sigma, degrees).
    pub heading_accuracy: f64,
    /// Raw GNSS quality flag.
    pub gnss_quality: u8,
}

/// A proprietary NMEA sentence (talker prefix `$P`).
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ProprietarySentence {
    Prdid(PrdidData),
    Pashr(PashrData),
    /// Any other `$P` sentence.
    /// `identifier` is the part after `$P` and before the first `,` (e.g. `"GRMZ"`).
    Raw {
        identifier: String,
        fields: Vec<String>,
    },
}

impl ProprietarySentence {
    /// Serialize to wire format including `\r\n`.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Prdid(d) => d.to_wire(),
            Self::Pashr(d) => d.to_wire(),
            Self::Raw { identifier, fields } => {
                let payload = if fields.is_empty() {
                    format!("P{identifier}")
                } else {
                    format!("P{},{}", identifier, fields.join(","))
                };
                let cs = checksum::xor(payload.as_bytes());
                format!("${}*{:02X}\r\n", payload, cs)
            }
        }
    }

    /// Parse a proprietary sentence string. Accepts lines with or without `\r\n`.
    /// Rejects non-ASCII input and any checksum other than two hex digits
    /// (ADR-058).
    pub fn parse(line: &str) -> Result<Self, NmeaError> {
        let line = line.trim_end_matches(['\r', '\n']);
        crate::require_ascii(line)?;
        let rest = line
            .strip_prefix('$')
            .ok_or(NmeaError::MissingStartDelimiter)?;

        if !rest.starts_with('P') {
            return Err(NmeaError::Parse("not a proprietary sentence".to_string()));
        }

        // PRDID has no checksum — detect it before requiring '*'.
        if let Some(after) = rest.strip_prefix("PRDID") {
            let fields_str = after.strip_prefix(',').unwrap_or(after);
            return parse_prdid(fields_str);
        }

        // All other proprietary sentences require a checksum.
        let (body, chk) = rest.rsplit_once('*').ok_or(NmeaError::MissingChecksum)?;

        let expected = checksum::parse_suffix(chk)?;
        let computed = checksum::xor(body.as_bytes());
        if expected != computed {
            return Err(NmeaError::InvalidChecksum { expected, computed });
        }

        // Strip leading 'P', then split identifier from fields.
        let after_p = &body[1..];
        let (ident, fields_str) = match after_p.split_once(',') {
            Some((id, fs)) => (id, Some(fs)),
            None => (after_p, None),
        };

        match ident {
            "ASHR" => parse_pashr(fields_str.unwrap_or("")),
            _ => Ok(Self::Raw {
                identifier: ident.to_string(),
                // A comma starts the field list even when nothing follows it,
                // so `$PFOO,*..` keeps its one empty field (ADR-058).
                fields: fields_str
                    .map(|fs| fs.split(',').map(str::to_string).collect())
                    .unwrap_or_default(),
            }),
        }
    }
}

impl fmt::Display for ProprietarySentence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_wire())
    }
}

// ── PrdidData ────────────────────────────────────────────────────────────────

impl PrdidData {
    pub fn new(pitch: f64, roll: f64, heading: f64) -> Self {
        Self {
            pitch,
            roll,
            heading,
        }
    }

    pub fn to_wire(&self) -> String {
        format!(
            "$PRDID,{:.2},{:.2},{:.2}\r\n",
            self.pitch, self.roll, self.heading
        )
    }
}

fn parse_prdid(fields_str: &str) -> Result<ProprietarySentence, NmeaError> {
    let mut it = fields_str.split(',');

    let pitch = parse_f64(it.next(), 0, "pitch")?;
    let roll = parse_f64(it.next(), 1, "roll")?;
    let heading = parse_f64(it.next(), 2, "heading")?;

    Ok(ProprietarySentence::Prdid(PrdidData {
        pitch,
        roll,
        heading,
    }))
}

// ── PashrData ────────────────────────────────────────────────────────────────

impl PashrData {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        utc_time: Option<String>,
        heading: f64,
        roll: f64,
        pitch: f64,
        heave: f64,
        roll_accuracy: f64,
        pitch_accuracy: f64,
        heading_accuracy: f64,
        gnss_quality: u8,
    ) -> Self {
        Self {
            utc_time,
            heading,
            roll,
            pitch,
            heave,
            roll_accuracy,
            pitch_accuracy,
            heading_accuracy,
            gnss_quality,
        }
    }

    pub fn to_wire(&self) -> String {
        let payload = if let Some(utc) = &self.utc_time {
            format!(
                "PASHR,{},{:.2},T,{:.2},{:.2},{:.2},{:.3},{:.3},{:.3},{}",
                utc,
                self.heading,
                self.roll,
                self.pitch,
                self.heave,
                self.roll_accuracy,
                self.pitch_accuracy,
                self.heading_accuracy,
                self.gnss_quality,
            )
        } else {
            format!(
                "PASHR,{:.2},T,{:.2},{:.2},{:.2},{:.3},{:.3},{:.3},{}",
                self.heading,
                self.roll,
                self.pitch,
                self.heave,
                self.roll_accuracy,
                self.pitch_accuracy,
                self.heading_accuracy,
                self.gnss_quality,
            )
        };
        let cs = checksum::xor(payload.as_bytes());
        format!("${}*{:02X}\r\n", payload, cs)
    }
}

fn parse_pashr(fields_str: &str) -> Result<ProprietarySentence, NmeaError> {
    let parts: Vec<&str> = fields_str.split(',').collect();

    // Disambiguate: with UTC the "T" indicator is at index 2; without, it is at index 1.
    let (utc_time, base) = if parts.get(1).copied() == Some("T") {
        (None, 0usize)
    } else if parts.get(2).copied() == Some("T") {
        let utc = parts.first().copied().unwrap_or("").to_string();
        (Some(utc), 1usize)
    } else {
        return Err(NmeaError::Parse(
            "PASHR: cannot locate 'T' indicator in field 1 or 2".to_string(),
        ));
    };

    let heading = parse_f64(parts.get(base).copied(), base, "heading")?;
    // skip "T" at base+1
    let roll = parse_f64(parts.get(base + 2).copied(), base + 2, "roll")?;
    let pitch = parse_f64(parts.get(base + 3).copied(), base + 3, "pitch")?;
    let heave = parse_f64(parts.get(base + 4).copied(), base + 4, "heave")?;
    let roll_accuracy = parse_f64(parts.get(base + 5).copied(), base + 5, "roll_accuracy")?;
    let pitch_accuracy = parse_f64(parts.get(base + 6).copied(), base + 6, "pitch_accuracy")?;
    let heading_accuracy = parse_f64(parts.get(base + 7).copied(), base + 7, "heading_accuracy")?;
    let gnss_quality = parse_u8(parts.get(base + 8).copied(), base + 8, "gnss_quality")?;

    Ok(ProprietarySentence::Pashr(PashrData {
        utc_time,
        heading,
        roll,
        pitch,
        heave,
        roll_accuracy,
        pitch_accuracy,
        heading_accuracy,
        gnss_quality,
    }))
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn parse_f64(s: Option<&str>, index: usize, name: &str) -> Result<f64, NmeaError> {
    s.ok_or_else(|| NmeaError::InvalidField {
        index,
        message: format!("missing field '{name}'"),
    })?
    .parse::<f64>()
    .map_err(|e| NmeaError::InvalidField {
        index,
        message: format!("'{name}' is not a valid f64: {e}"),
    })
}

fn parse_u8(s: Option<&str>, index: usize, name: &str) -> Result<u8, NmeaError> {
    s.ok_or_else(|| NmeaError::InvalidField {
        index,
        message: format!("missing field '{name}'"),
    })?
    .parse::<u8>()
    .map_err(|e| NmeaError::InvalidField {
        index,
        message: format!("'{name}' is not a valid u8: {e}"),
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- PRDID ---

    #[test]
    fn prdid_to_wire_no_checksum() {
        let d = PrdidData {
            pitch: 1.50,
            roll: 0.30,
            heading: 127.45,
        };
        let wire = ProprietarySentence::Prdid(d).to_wire();
        assert_eq!(wire, "$PRDID,1.50,0.30,127.45\r\n");
        assert!(!wire.contains('*'), "PRDID must not contain a checksum");
    }

    #[test]
    fn prdid_parse_basic() {
        let wire = "$PRDID,1.50,0.30,127.45\r\n";
        let s = ProprietarySentence::parse(wire).unwrap();
        assert_eq!(
            s,
            ProprietarySentence::Prdid(PrdidData {
                pitch: 1.50,
                roll: 0.30,
                heading: 127.45
            })
        );
    }

    #[test]
    fn prdid_round_trip() {
        let d = PrdidData {
            pitch: -3.75,
            roll: 0.01,
            heading: 0.50,
        };
        let wire = ProprietarySentence::Prdid(d.clone()).to_wire();
        let parsed = ProprietarySentence::parse(&wire).unwrap();
        assert_eq!(parsed, ProprietarySentence::Prdid(d));
    }

    #[test]
    fn prdid_missing_field() {
        let err = ProprietarySentence::parse("$PRDID,1.50,0.30\r\n").unwrap_err();
        assert!(matches!(err, NmeaError::InvalidField { index: 2, .. }));
    }

    // --- PASHR ---

    fn sample_pashr_no_utc() -> PashrData {
        PashrData {
            utc_time: None,
            heading: 45.67,
            roll: 1.23,
            pitch: -2.45,
            heave: 0.12,
            roll_accuracy: 0.010,
            pitch_accuracy: 0.010,
            heading_accuracy: 0.100,
            gnss_quality: 1,
        }
    }

    fn sample_pashr_with_utc() -> PashrData {
        PashrData {
            utc_time: Some("123519.00".to_string()),
            ..sample_pashr_no_utc()
        }
    }

    #[test]
    fn pashr_to_wire_no_utc() {
        let wire = ProprietarySentence::Pashr(sample_pashr_no_utc()).to_wire();
        assert!(wire.starts_with("$PASHR,45.67,T,"));
        assert!(wire.contains("*"));
    }

    #[test]
    fn pashr_to_wire_with_utc() {
        let wire = ProprietarySentence::Pashr(sample_pashr_with_utc()).to_wire();
        assert!(wire.starts_with("$PASHR,123519.00,45.67,T,"));
    }

    #[test]
    fn pashr_round_trip_no_utc() {
        let d = sample_pashr_no_utc();
        let wire = ProprietarySentence::Pashr(d.clone()).to_wire();
        let parsed = ProprietarySentence::parse(&wire).unwrap();
        assert_eq!(parsed, ProprietarySentence::Pashr(d));
    }

    #[test]
    fn pashr_round_trip_with_utc() {
        let d = sample_pashr_with_utc();
        let wire = ProprietarySentence::Pashr(d.clone()).to_wire();
        let parsed = ProprietarySentence::parse(&wire).unwrap();
        assert_eq!(parsed, ProprietarySentence::Pashr(d));
    }

    #[test]
    fn pashr_bad_checksum() {
        let wire = ProprietarySentence::Pashr(sample_pashr_no_utc()).to_wire();
        // Corrupt the checksum digit
        let bad = wire.replace(['\r', '\n'], "");
        let bad = format!("{}X\r\n", &bad[..bad.len() - 1]);
        let err = ProprietarySentence::parse(&bad).unwrap_err();
        assert!(matches!(
            err,
            NmeaError::InvalidChecksum { .. } | NmeaError::Parse(_)
        ));
    }

    // --- Raw ---

    #[test]
    fn raw_to_wire_and_parse() {
        let s = ProprietarySentence::Raw {
            identifier: "GRMZ".to_string(),
            fields: vec!["93".to_string(), "f".to_string(), "3".to_string()],
        };
        let wire = s.to_wire();
        assert!(wire.starts_with("$PGRMZ,93,f,3*"));
        let parsed = ProprietarySentence::parse(&wire).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn raw_no_fields() {
        let s = ProprietarySentence::Raw {
            identifier: "FOO".to_string(),
            fields: vec![],
        };
        let wire = s.to_wire();
        assert!(wire.starts_with("$PFOO*"));
        let parsed = ProprietarySentence::parse(&wire).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn raw_explicit_empty_field_round_trips() {
        let s = ProprietarySentence::Raw {
            identifier: "FOO".to_string(),
            fields: vec![String::new()],
        };
        let wire = s.to_wire();
        assert!(wire.starts_with("$PFOO,*"));
        assert_eq!(ProprietarySentence::parse(&wire).unwrap(), s);
    }

    #[test]
    fn rejects_non_ascii_and_loose_checksum_digits() {
        let body = "PGRMZ,93,f,3";
        let cs = checksum::xor(body.as_bytes());
        for line in [format!("${body}*00{cs:02X}"), format!("${body}*+{cs:02X}")] {
            let err = ProprietarySentence::parse(&line).unwrap_err();
            assert!(matches!(err, NmeaError::Parse(_)), "{line:?}");
        }
        let body = "PGRMZ,9\u{e9}";
        let line = format!("${body}*{:02X}", checksum::xor(body.as_bytes()));
        let err = ProprietarySentence::parse(&line).unwrap_err();
        assert!(matches!(err, NmeaError::Parse(_)));
    }

    #[test]
    fn raw_missing_checksum() {
        let err = ProprietarySentence::parse("$PGRMZ,93,f,3").unwrap_err();
        assert!(matches!(err, NmeaError::MissingChecksum));
    }

    #[test]
    fn rejects_non_proprietary() {
        let err = ProprietarySentence::parse("$GPGGA,123519*47").unwrap_err();
        assert!(matches!(err, NmeaError::Parse(_)));
    }

    #[test]
    fn missing_dollar() {
        let err = ProprietarySentence::parse("PRDID,1.0,2.0,3.0").unwrap_err();
        assert!(matches!(err, NmeaError::MissingStartDelimiter));
    }
}
