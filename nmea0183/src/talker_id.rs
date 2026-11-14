use std::fmt;
use std::str::FromStr;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TalkerId {
    // ── AIS ──────────────────────────────────────────────────────────────────
    /// Independent AIS base station
    AB,
    /// Dependent AIS base station
    AD,
    /// Autopilot, general
    AG,
    /// Mobile Class A or B AIS station
    AI,
    /// AIS aid to navigation station
    AN,
    /// Autopilot, magnetic
    AP,
    /// AIS receiving station
    AR,
    /// AIS transmitting station
    AT,
    /// AIS simplex repeater
    AX,
    // ── Communications ───────────────────────────────────────────────────────
    /// Communications, digital selective calling (DSC)
    CD,
    /// Communications, data receiver (beacon receiver)
    CR,
    /// Communications, satellite
    CS,
    /// Communications, radio-telephone (MF/HF)
    CT,
    /// Communications, radio-telephone (VHF)
    CV,
    /// Communications, scanning receiver
    CX,
    // ── Direction finding / legacy positioning ───────────────────────────────
    /// DECCA navigation (obsolete)
    DE,
    /// Direction finder
    DF,
    // ── Electronic systems ───────────────────────────────────────────────────
    /// Electronic chart display and information system (ECDIS)
    EC,
    /// Emergency position indicating radio beacon (EPIRB)
    EP,
    /// Engine room monitoring systems
    ER,
    // ── GNSS ─────────────────────────────────────────────────────────────────
    /// Galileo
    GA,
    /// BeiDou (CNSS)
    GB,
    /// BeiDou (alternate, legacy designation)
    BD,
    /// NavIC / IRNSS
    GI,
    /// GLONASS
    GL,
    /// Combined / multi-constellation GNSS
    GN,
    /// GPS
    GP,
    /// QZSS
    GQ,
    // ── Heading ──────────────────────────────────────────────────────────────
    /// Heading, magnetic compass
    HC,
    /// Heading, north-seeking gyro
    HE,
    /// Heading, fluxgate compass
    HF,
    /// Heading, non north-seeking gyro
    HN,
    // ── Integrated systems ───────────────────────────────────────────────────
    /// Integrated instrumentation
    II,
    /// Integrated navigation
    IN,
    // ── Legacy / obsolete positioning ────────────────────────────────────────
    /// Loran A (obsolete)
    LA,
    /// Loran C (obsolete)
    LC,
    /// Microwave positioning system (obsolete)
    MP,
    /// OMEGA navigation system (obsolete)
    OM,
    /// TRANSIT navigation system (obsolete)
    TR,
    // ── Other onboard systems ────────────────────────────────────────────────
    /// Navigation light controller
    NL,
    /// RADAR and/or ARPA
    RA,
    /// Physical shore AIS station
    SA,
    /// Sounder, depth
    SD,
    /// Electronic positioning system, other/general
    SN,
    /// Sounder, scanning
    SS,
    /// Turn rate indicator
    TI,
    // ── Proprietary ──────────────────────────────────────────────────────────
    /// Proprietary-sentence marker. The single character after `$` for
    /// manufacturer-defined sentences such as `$PASHR` (Ashtech) or
    /// `$PRDID` (Teledyne RDI). Strictly this is not a talker ID — the
    /// remaining 4 characters are a vendor identifier plus sentence code
    /// — but exposing it here lets callers build proprietary sentences
    /// through the same `talker + sentence_type` API.
    P,
    // ── Velocity sensors ─────────────────────────────────────────────────────
    /// Velocity sensor, Doppler
    VD,
    /// Velocity sensor, magnetic
    VM,
    /// Voyage data recorder
    VR,
    /// Velocity sensor, mechanical (water)
    VW,
    // ── Weather ──────────────────────────────────────────────────────────────
    /// Weather instruments
    WI,
    // ── Transducers (obsolete — superseded by XDR sentence) ──────────────────
    /// Transducer, temperature
    YC,
    /// Transducer, displacement (angular or linear)
    YD,
    /// Transducer, frequency
    YF,
    /// Transducer, level
    YL,
    /// Transducer, pressure
    YP,
    /// Transducer, flow rate
    YR,
    /// Transducer, status
    YS,
    /// Transducer, tachometer
    YT,
    /// Transducer, volume
    YV,
    /// Transducer, general
    YX,
    // ── Timekeepers ──────────────────────────────────────────────────────────
    /// Timekeeper, atomic clock
    ZA,
    /// Timekeeper, chronometer
    ZC,
    /// Timekeeper, quartz
    ZQ,
    // ── Catch-all ────────────────────────────────────────────────────────────
    /// Non-standard or future talker IDs.
    Custom(String),
}

/// All standard two-character talker IDs in this enum, in declaration order.
/// Intended for UIs that want to present every option (e.g. a filterable
/// dropdown). Does not include the `Custom(String)` variant.
pub const ALL: &[&str] = &[
    "AB", "AD", "AG", "AI", "AN", "AP", "AR", "AT", "AX", "CD", "CR", "CS", "CT", "CV", "CX", "DE",
    "DF", "EC", "EP", "ER", "GA", "GB", "BD", "GI", "GL", "GN", "GP", "GQ", "HC", "HE", "HF", "HN",
    "II", "IN", "LA", "LC", "MP", "OM", "TR", "NL", "RA", "SA", "SD", "SN", "SS", "TI", "P", "VD",
    "VM", "VR", "VW", "WI", "YC", "YD", "YF", "YL", "YP", "YR", "YS", "YT", "YV", "YX", "ZA", "ZC",
    "ZQ",
];

/// `(code, description)` pairs for every entry in [`ALL`], in the same order.
/// Descriptions are short single-line summaries lifted from the variant doc
/// comments. Intended for UIs that want to render `GP — GPS` style rows.
pub const ALL_WITH_DESC: &[(&str, &str)] = &[
    ("AB", "Independent AIS base station"),
    ("AD", "Dependent AIS base station"),
    ("AG", "Autopilot, general"),
    ("AI", "Mobile Class A or B AIS station"),
    ("AN", "AIS aid to navigation station"),
    ("AP", "Autopilot, magnetic"),
    ("AR", "AIS receiving station"),
    ("AT", "AIS transmitting station"),
    ("AX", "AIS simplex repeater"),
    ("CD", "Communications, DSC (digital selective calling)"),
    ("CR", "Communications, data receiver (beacon receiver)"),
    ("CS", "Communications, satellite"),
    ("CT", "Communications, radio-telephone (MF/HF)"),
    ("CV", "Communications, radio-telephone (VHF)"),
    ("CX", "Communications, scanning receiver"),
    ("DE", "DECCA navigation (obsolete)"),
    ("DF", "Direction finder"),
    ("EC", "ECDIS (electronic chart display)"),
    ("EP", "EPIRB (emergency position-indicating radio beacon)"),
    ("ER", "Engine room monitoring systems"),
    ("GA", "Galileo"),
    ("GB", "BeiDou (CNSS)"),
    ("BD", "BeiDou (alternate, legacy designation)"),
    ("GI", "NavIC / IRNSS"),
    ("GL", "GLONASS"),
    ("GN", "Combined / multi-constellation GNSS"),
    ("GP", "GPS"),
    ("GQ", "QZSS"),
    ("HC", "Heading, magnetic compass"),
    ("HE", "Heading, north-seeking gyro"),
    ("HF", "Heading, fluxgate compass"),
    ("HN", "Heading, non north-seeking gyro"),
    ("II", "Integrated instrumentation"),
    ("IN", "Integrated navigation"),
    ("LA", "Loran A (obsolete)"),
    ("LC", "Loran C (obsolete)"),
    ("MP", "Microwave positioning system (obsolete)"),
    ("OM", "OMEGA navigation system (obsolete)"),
    ("TR", "TRANSIT navigation system (obsolete)"),
    ("NL", "Navigation light controller"),
    ("RA", "RADAR and/or ARPA"),
    ("SA", "Physical shore AIS station"),
    ("SD", "Sounder, depth"),
    ("SN", "Electronic positioning system, other / general"),
    ("SS", "Sounder, scanning"),
    ("TI", "Turn rate indicator"),
    ("P", "Proprietary marker ($PASHR, $PRDID, etc.)"),
    ("VD", "Velocity sensor, Doppler"),
    ("VM", "Velocity sensor, magnetic"),
    ("VR", "Voyage data recorder"),
    ("VW", "Velocity sensor, mechanical (water)"),
    ("WI", "Weather instruments"),
    ("YC", "Transducer, temperature (obsolete)"),
    ("YD", "Transducer, displacement (obsolete)"),
    ("YF", "Transducer, frequency (obsolete)"),
    ("YL", "Transducer, level (obsolete)"),
    ("YP", "Transducer, pressure (obsolete)"),
    ("YR", "Transducer, flow rate (obsolete)"),
    ("YS", "Transducer, status (obsolete)"),
    ("YT", "Transducer, tachometer (obsolete)"),
    ("YV", "Transducer, volume (obsolete)"),
    ("YX", "Transducer, general (obsolete)"),
    ("ZA", "Timekeeper, atomic clock"),
    ("ZC", "Timekeeper, chronometer"),
    ("ZQ", "Timekeeper, quartz"),
];

impl TalkerId {
    pub fn as_str(&self) -> &str {
        match self {
            Self::AB => "AB",
            Self::AD => "AD",
            Self::AG => "AG",
            Self::AI => "AI",
            Self::AN => "AN",
            Self::AP => "AP",
            Self::AR => "AR",
            Self::AT => "AT",
            Self::AX => "AX",
            Self::CD => "CD",
            Self::CR => "CR",
            Self::CS => "CS",
            Self::CT => "CT",
            Self::CV => "CV",
            Self::CX => "CX",
            Self::DE => "DE",
            Self::DF => "DF",
            Self::EC => "EC",
            Self::EP => "EP",
            Self::ER => "ER",
            Self::GA => "GA",
            Self::GB => "GB",
            Self::BD => "BD",
            Self::GI => "GI",
            Self::GL => "GL",
            Self::GN => "GN",
            Self::GP => "GP",
            Self::GQ => "GQ",
            Self::HC => "HC",
            Self::HE => "HE",
            Self::HF => "HF",
            Self::HN => "HN",
            Self::II => "II",
            Self::IN => "IN",
            Self::LA => "LA",
            Self::LC => "LC",
            Self::MP => "MP",
            Self::OM => "OM",
            Self::TR => "TR",
            Self::NL => "NL",
            Self::RA => "RA",
            Self::SA => "SA",
            Self::SD => "SD",
            Self::SN => "SN",
            Self::SS => "SS",
            Self::TI => "TI",
            Self::P => "P",
            Self::VD => "VD",
            Self::VM => "VM",
            Self::VR => "VR",
            Self::VW => "VW",
            Self::WI => "WI",
            Self::YC => "YC",
            Self::YD => "YD",
            Self::YF => "YF",
            Self::YL => "YL",
            Self::YP => "YP",
            Self::YR => "YR",
            Self::YS => "YS",
            Self::YT => "YT",
            Self::YV => "YV",
            Self::YX => "YX",
            Self::ZA => "ZA",
            Self::ZC => "ZC",
            Self::ZQ => "ZQ",
            Self::Custom(s) => s.as_str(),
        }
    }
}

impl fmt::Display for TalkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TalkerId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "AB" => Self::AB,
            "AD" => Self::AD,
            "AG" => Self::AG,
            "AI" => Self::AI,
            "AN" => Self::AN,
            "AP" => Self::AP,
            "AR" => Self::AR,
            "AT" => Self::AT,
            "AX" => Self::AX,
            "CD" => Self::CD,
            "CR" => Self::CR,
            "CS" => Self::CS,
            "CT" => Self::CT,
            "CV" => Self::CV,
            "CX" => Self::CX,
            "DE" => Self::DE,
            "DF" => Self::DF,
            "EC" => Self::EC,
            "EP" => Self::EP,
            "ER" => Self::ER,
            "GA" => Self::GA,
            "GB" => Self::GB,
            "BD" => Self::BD,
            "GI" => Self::GI,
            "GL" => Self::GL,
            "GN" => Self::GN,
            "GP" => Self::GP,
            "GQ" => Self::GQ,
            "HC" => Self::HC,
            "HE" => Self::HE,
            "HF" => Self::HF,
            "HN" => Self::HN,
            "II" => Self::II,
            "IN" => Self::IN,
            "LA" => Self::LA,
            "LC" => Self::LC,
            "MP" => Self::MP,
            "OM" => Self::OM,
            "TR" => Self::TR,
            "NL" => Self::NL,
            "RA" => Self::RA,
            "SA" => Self::SA,
            "SD" => Self::SD,
            "SN" => Self::SN,
            "SS" => Self::SS,
            "TI" => Self::TI,
            "P" => Self::P,
            "VD" => Self::VD,
            "VM" => Self::VM,
            "VR" => Self::VR,
            "VW" => Self::VW,
            "WI" => Self::WI,
            "YC" => Self::YC,
            "YD" => Self::YD,
            "YF" => Self::YF,
            "YL" => Self::YL,
            "YP" => Self::YP,
            "YR" => Self::YR,
            "YS" => Self::YS,
            "YT" => Self::YT,
            "YV" => Self::YV,
            "YX" => Self::YX,
            "ZA" => Self::ZA,
            "ZC" => Self::ZC,
            "ZQ" => Self::ZQ,
            _ => Self::Custom(s.to_string()),
        })
    }
}

/// Infallible conversion: any text is a valid `TalkerId` (unknown codes become
/// `Custom`). Same mapping as [`FromStr`], without the `Result`.
impl From<&str> for TalkerId {
    fn from(s: &str) -> Self {
        match s.parse() {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_constant_matches_enum() {
        // Every entry in `ALL` must parse back to a named variant — not the
        // `Custom` catch-all — so the const stays in sync with the enum.
        for s in ALL {
            let parsed: TalkerId = s.parse().expect("ALL entry parses");
            assert!(
                !matches!(parsed, TalkerId::Custom(_)),
                "ALL contains {s:?} but it parsed as Custom — the enum is missing this variant",
            );
            assert_eq!(parsed.as_str(), *s, "round-trip mismatch for {s:?}");
        }
    }

    #[test]
    fn all_with_desc_is_aligned_with_all() {
        // Same length, same codes in the same order — so callers can pick
        // either constant without losing entries.
        assert_eq!(ALL.len(), ALL_WITH_DESC.len());
        for (i, (code, desc)) in ALL_WITH_DESC.iter().enumerate() {
            assert_eq!(*code, ALL[i], "ALL_WITH_DESC[{i}] code mismatch");
            assert!(!desc.is_empty(), "ALL_WITH_DESC[{i}] description is empty");
        }
    }

    #[test]
    fn known_ids_round_trip() {
        let cases = [
            (TalkerId::AB, "AB"),
            (TalkerId::AD, "AD"),
            (TalkerId::AG, "AG"),
            (TalkerId::AI, "AI"),
            (TalkerId::AN, "AN"),
            (TalkerId::AP, "AP"),
            (TalkerId::AR, "AR"),
            (TalkerId::AT, "AT"),
            (TalkerId::AX, "AX"),
            (TalkerId::CD, "CD"),
            (TalkerId::CR, "CR"),
            (TalkerId::CS, "CS"),
            (TalkerId::CT, "CT"),
            (TalkerId::CV, "CV"),
            (TalkerId::CX, "CX"),
            (TalkerId::DE, "DE"),
            (TalkerId::DF, "DF"),
            (TalkerId::EC, "EC"),
            (TalkerId::EP, "EP"),
            (TalkerId::ER, "ER"),
            (TalkerId::GA, "GA"),
            (TalkerId::GB, "GB"),
            (TalkerId::BD, "BD"),
            (TalkerId::GI, "GI"),
            (TalkerId::GL, "GL"),
            (TalkerId::GN, "GN"),
            (TalkerId::GP, "GP"),
            (TalkerId::GQ, "GQ"),
            (TalkerId::HC, "HC"),
            (TalkerId::HE, "HE"),
            (TalkerId::HF, "HF"),
            (TalkerId::HN, "HN"),
            (TalkerId::II, "II"),
            (TalkerId::IN, "IN"),
            (TalkerId::LA, "LA"),
            (TalkerId::LC, "LC"),
            (TalkerId::MP, "MP"),
            (TalkerId::OM, "OM"),
            (TalkerId::TR, "TR"),
            (TalkerId::NL, "NL"),
            (TalkerId::RA, "RA"),
            (TalkerId::SA, "SA"),
            (TalkerId::SD, "SD"),
            (TalkerId::SN, "SN"),
            (TalkerId::SS, "SS"),
            (TalkerId::TI, "TI"),
            (TalkerId::P, "P"),
            (TalkerId::VD, "VD"),
            (TalkerId::VM, "VM"),
            (TalkerId::VR, "VR"),
            (TalkerId::VW, "VW"),
            (TalkerId::WI, "WI"),
            (TalkerId::YC, "YC"),
            (TalkerId::YD, "YD"),
            (TalkerId::YF, "YF"),
            (TalkerId::YL, "YL"),
            (TalkerId::YP, "YP"),
            (TalkerId::YR, "YR"),
            (TalkerId::YS, "YS"),
            (TalkerId::YT, "YT"),
            (TalkerId::YV, "YV"),
            (TalkerId::YX, "YX"),
            (TalkerId::ZA, "ZA"),
            (TalkerId::ZC, "ZC"),
            (TalkerId::ZQ, "ZQ"),
        ];
        for (variant, s) in cases {
            assert_eq!(variant.as_str(), s);
            assert_eq!(s.parse::<TalkerId>().unwrap(), variant);
            assert_eq!(variant.to_string(), s);
        }
    }

    #[test]
    fn custom_id_round_trip() {
        let id: TalkerId = "XY".parse().unwrap();
        assert_eq!(id, TalkerId::Custom("XY".to_string()));
        assert_eq!(id.to_string(), "XY");
    }

    #[test]
    fn from_str_slice_matches_parse() {
        for text in ["", "XY", "GP", "GGA", "FOO", "ZZZZ"] {
            let parsed: TalkerId = text.parse().unwrap();
            assert_eq!(TalkerId::from(text), parsed);
        }
        assert_eq!(TalkerId::from("XQZ"), TalkerId::Custom("XQZ".to_string()));
    }
}
