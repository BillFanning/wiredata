use std::fmt;
use std::str::FromStr;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SentenceType {
    /// Waypoint arrival alarm
    AAM,
    /// AIS addressed and binary broadcast acknowledgement
    ABK,
    /// AIS addressed binary and safety related message
    ABM,
    /// Acknowledge alarm
    ACK,
    /// AIS interrogation request
    AIR,
    /// Acknowledge detail alarm condition
    AKD,
    /// Set detail alarm condition
    ALA,
    /// GPS almanac data
    ALM,
    /// Autopilot sentence A
    APA,
    /// Autopilot sentence B
    APB,
    /// `$PASHR` — Ashtech attitude/heading. Pair with [`TalkerId::P`](crate::TalkerId::P).
    ASHR,
    /// Bearing and distance to waypoint, dead reckoning
    BEC,
    /// Bearing, origin to destination
    BOD,
    /// Bearing and distance to waypoint, great circle
    BWC,
    /// Bearing and distance to waypoint, rhumb line
    BWR,
    /// Bearing, waypoint to waypoint
    BWW,
    /// Depth below keel
    DBK,
    /// Depth below surface
    DBS,
    /// Depth below transducer
    DBT,
    /// DECCA position (obsolete)
    DCN,
    /// Depth of water
    DPT,
    /// Digital selective calling information
    DSC,
    /// Expanded DSC
    DSE,
    /// DSC transponder initiate
    DSI,
    /// DSC transponder response
    DSR,
    /// Datum reference
    DTM,
    /// Frequency set information
    FSI,
    /// GPS satellite fault detection
    GBS,
    /// GPS fix data
    GGA,
    /// Geographic position, Loran-C
    GLC,
    /// Geographic position, latitude/longitude
    GLL,
    /// GNSS fix data
    GNS,
    /// GPS range residuals
    GRS,
    /// GPS DOP and active satellites
    GSA,
    /// GPS pseudorange error statistics
    GST,
    /// Satellites in view
    GSV,
    /// Geographic position, time differences
    GTD,
    /// TRANSIT position (obsolete)
    GXA,
    /// Heading, deviation and variation
    HDG,
    /// Heading, magnetic
    HDM,
    /// Heading, true
    HDT,
    /// Trawl headrope to footrope and bottom
    HFB,
    /// Heading steering command
    HSC,
    /// Trawl door spread speed
    ITS,
    /// Loran-C signal data
    LCD,
    /// Meteorological composite
    MDA,
    /// Humidity
    MHU,
    /// MSK receiver interface
    MSK,
    /// MSK receiver signal status
    MSS,
    /// Air temperature (obsolete, superseded by XDR)
    MTA,
    /// Mean temperature of water
    MTW,
    /// Wind direction and speed
    MWD,
    /// Wind speed and angle
    MWV,
    /// Omega lane numbers
    OLN,
    /// Own ship data
    OSD,
    /// Waypoints in active route
    R00,
    /// `$PRDID` — Teledyne RDI ADCP pitch/roll/heading. Pair with
    /// [`TalkerId::P`](crate::TalkerId::P); PRDID has no checksum by convention, so the
    /// `NmeaChecksumMode::Omit` mode is appropriate when building one.
    RDID,
    /// Recommended minimum specific Loran-C data (obsolete)
    RMA,
    /// Recommended minimum navigation information
    RMB,
    /// Recommended minimum specific GPS/transit data
    RMC,
    /// Rate of turn
    ROT,
    /// Revolutions
    RPM,
    /// Rudder sensor angle
    RSA,
    /// RADAR system data
    RSD,
    /// Routes
    RTE,
    /// Scanning frequency information
    SFI,
    /// Multiple data ID
    STN,
    /// Trawl door spread distance
    TDS,
    /// Trawl filling indicator
    TFI,
    /// True heading and status
    THS,
    /// Target latitude and longitude
    TLL,
    /// Trawl position, cartesian coordinates
    TPC,
    /// Trawl position, relative to vessel
    TPR,
    /// Trawl position, true
    TPT,
    /// TRANSIT fix data (obsolete)
    TRF,
    /// Tracked target message
    TTM,
    /// Transmission of multi-language text
    TUT,
    /// Text transmission
    TXT,
    /// Dual ground/water speed
    VBW,
    /// AIS VHF data-link message
    VDM,
    /// AIS VHF data-link own vessel report
    VDO,
    /// Set and drift
    VDR,
    /// Water speed and heading
    VHW,
    /// Distance traveled through water
    VLW,
    /// Speed measured parallel to wind
    VPW,
    /// Track made good and ground speed
    VTG,
    /// Relative wind speed and angle
    VWR,
    /// True wind speed and angle
    VWT,
    /// Waypoint closure velocity
    WCV,
    /// Distance, waypoint to waypoint
    WNC,
    /// Waypoint location
    WPL,
    /// Transducer measurements
    XDR,
    /// Cross-track error, measured
    XTE,
    /// Cross-track error, dead reckoning
    XTR,
    /// Time and date
    ZDA,
    /// Time and distance to variable point
    ZDL,
    /// UTC and time from origin waypoint
    ZFO,
    /// UTC and time to destination waypoint
    ZTG,
    /// Non-standard or future sentence types.
    Custom(String),
}

/// A date/time value carried by one field of a standard NMEA sentence.
///
/// Field positions returned by [`SentenceType::time_fields`] are zero-based
/// indices into [`NmeaSentence::fields`](crate::NmeaSentence::fields): the
/// talker ID and sentence mnemonic are not fields.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeFieldKind {
    /// UTC time of day as `hhmmss` with an optional fractional suffix.
    UtcTime,
    /// UTC calendar date as `ddmmyy`.
    DateDdmmyy,
    /// UTC day of month as `dd`.
    Day,
    /// UTC month as `mm`.
    Month,
    /// Four-digit UTC year as `yyyy`.
    Year,
}

impl TimeFieldKind {
    /// Short user-facing description suitable for a field-substitution hint.
    pub const fn label(self) -> &'static str {
        match self {
            Self::UtcTime => "UTC time",
            Self::DateDdmmyy => "UTC date (ddmmyy)",
            Self::Day => "UTC day",
            Self::Month => "UTC month",
            Self::Year => "UTC year",
        }
    }
}

const TIME_AT_0: &[(usize, TimeFieldKind)] = &[(0, TimeFieldKind::UtcTime)];
const GLL_TIME_FIELDS: &[(usize, TimeFieldKind)] = &[(4, TimeFieldKind::UtcTime)];
const RMC_TIME_FIELDS: &[(usize, TimeFieldKind)] =
    &[(0, TimeFieldKind::UtcTime), (8, TimeFieldKind::DateDdmmyy)];
const ZDA_TIME_FIELDS: &[(usize, TimeFieldKind)] = &[
    (0, TimeFieldKind::UtcTime),
    (1, TimeFieldKind::Day),
    (2, TimeFieldKind::Month),
    (3, TimeFieldKind::Year),
];

/// All standard sentence-type mnemonics in this enum, in declaration order.
/// Intended for UIs that want to present every option (e.g. a filterable
/// dropdown). Does not include the `Custom(String)` variant.
pub const ALL: &[&str] = &[
    "AAM", "ABK", "ABM", "ACK", "AIR", "AKD", "ALA", "ALM", "APA", "APB", "ASHR", "BEC", "BOD",
    "BWC", "BWR", "BWW", "DBK", "DBS", "DBT", "DCN", "DPT", "DSC", "DSE", "DSI", "DSR", "DTM",
    "FSI", "GBS", "GGA", "GLC", "GLL", "GNS", "GRS", "GSA", "GST", "GSV", "GTD", "GXA", "HDG",
    "HDM", "HDT", "HFB", "HSC", "ITS", "LCD", "MDA", "MHU", "MSK", "MSS", "MTA", "MTW", "MWD",
    "MWV", "OLN", "OSD", "R00", "RDID", "RMA", "RMB", "RMC", "ROT", "RPM", "RSA", "RSD", "RTE",
    "SFI", "STN", "TDS", "TFI", "THS", "TLL", "TPC", "TPR", "TPT", "TRF", "TTM", "TUT", "TXT",
    "VBW", "VDM", "VDO", "VDR", "VHW", "VLW", "VPW", "VTG", "VWR", "VWT", "WCV", "WNC", "WPL",
    "XDR", "XTE", "XTR", "ZDA", "ZDL", "ZFO", "ZTG",
];

/// `(code, description)` pairs for every entry in [`ALL`], in the same order.
/// Descriptions are short single-line summaries lifted from the variant doc
/// comments. Intended for UIs that want to render `GGA — GPS fix data` rows.
pub const ALL_WITH_DESC: &[(&str, &str)] = &[
    ("AAM", "Waypoint arrival alarm"),
    ("ABK", "AIS addressed and binary broadcast acknowledgement"),
    ("ABM", "AIS addressed binary and safety related message"),
    ("ACK", "Acknowledge alarm"),
    ("AIR", "AIS interrogation request"),
    ("AKD", "Acknowledge detail alarm condition"),
    ("ALA", "Set detail alarm condition"),
    ("ALM", "GPS almanac data"),
    ("APA", "Autopilot sentence A"),
    ("APB", "Autopilot sentence B"),
    (
        "ASHR",
        "Ashtech attitude/heading ($PASHR — pair with talker P)",
    ),
    ("BEC", "Bearing & distance to waypoint, dead reckoning"),
    ("BOD", "Bearing, origin to destination"),
    ("BWC", "Bearing & distance to waypoint, great circle"),
    ("BWR", "Bearing & distance to waypoint, rhumb line"),
    ("BWW", "Bearing, waypoint to waypoint"),
    ("DBK", "Depth below keel"),
    ("DBS", "Depth below surface"),
    ("DBT", "Depth below transducer"),
    ("DCN", "DECCA position (obsolete)"),
    ("DPT", "Depth of water"),
    ("DSC", "Digital selective calling information"),
    ("DSE", "Expanded DSC"),
    ("DSI", "DSC transponder initiate"),
    ("DSR", "DSC transponder response"),
    ("DTM", "Datum reference"),
    ("FSI", "Frequency set information"),
    ("GBS", "GPS satellite fault detection"),
    ("GGA", "GPS fix data"),
    ("GLC", "Geographic position, Loran-C"),
    ("GLL", "Geographic position, latitude/longitude"),
    ("GNS", "GNSS fix data"),
    ("GRS", "GPS range residuals"),
    ("GSA", "GPS DOP and active satellites"),
    ("GST", "GPS pseudorange error statistics"),
    ("GSV", "Satellites in view"),
    ("GTD", "Geographic position, time differences"),
    ("GXA", "TRANSIT position (obsolete)"),
    ("HDG", "Heading, deviation and variation"),
    ("HDM", "Heading, magnetic"),
    ("HDT", "Heading, true"),
    ("HFB", "Trawl headrope to footrope and bottom"),
    ("HSC", "Heading steering command"),
    ("ITS", "Trawl door spread speed"),
    ("LCD", "Loran-C signal data"),
    ("MDA", "Meteorological composite"),
    ("MHU", "Humidity"),
    ("MSK", "MSK receiver interface"),
    ("MSS", "MSK receiver signal status"),
    ("MTA", "Air temperature (obsolete, see XDR)"),
    ("MTW", "Mean temperature of water"),
    ("MWD", "Wind direction and speed"),
    ("MWV", "Wind speed and angle"),
    ("OLN", "Omega lane numbers"),
    ("OSD", "Own ship data"),
    ("R00", "Waypoints in active route"),
    (
        "RDID",
        "Teledyne RDI ADCP pitch/roll/heading ($PRDID — pair with talker P, no checksum)",
    ),
    (
        "RMA",
        "Recommended minimum specific Loran-C data (obsolete)",
    ),
    ("RMB", "Recommended minimum navigation information"),
    ("RMC", "Recommended minimum specific GPS/transit data"),
    ("ROT", "Rate of turn"),
    ("RPM", "Revolutions"),
    ("RSA", "Rudder sensor angle"),
    ("RSD", "RADAR system data"),
    ("RTE", "Routes"),
    ("SFI", "Scanning frequency information"),
    ("STN", "Multiple data ID"),
    ("TDS", "Trawl door spread distance"),
    ("TFI", "Trawl filling indicator"),
    ("THS", "True heading and status"),
    ("TLL", "Target latitude and longitude"),
    ("TPC", "Trawl position, cartesian coordinates"),
    ("TPR", "Trawl position, relative to vessel"),
    ("TPT", "Trawl position, true"),
    ("TRF", "TRANSIT fix data (obsolete)"),
    ("TTM", "Tracked target message"),
    ("TUT", "Transmission of multi-language text"),
    ("TXT", "Text transmission"),
    ("VBW", "Dual ground/water speed"),
    ("VDM", "AIS VHF data-link message"),
    ("VDO", "AIS VHF data-link own vessel report"),
    ("VDR", "Set and drift"),
    ("VHW", "Water speed and heading"),
    ("VLW", "Distance traveled through water"),
    ("VPW", "Speed measured parallel to wind"),
    ("VTG", "Track made good and ground speed"),
    ("VWR", "Relative wind speed and angle"),
    ("VWT", "True wind speed and angle"),
    ("WCV", "Waypoint closure velocity"),
    ("WNC", "Distance, waypoint to waypoint"),
    ("WPL", "Waypoint location"),
    ("XDR", "Transducer measurements"),
    ("XTE", "Cross-track error, measured"),
    ("XTR", "Cross-track error, dead reckoning"),
    ("ZDA", "Time and date"),
    ("ZDL", "Time and distance to variable point"),
    ("ZFO", "UTC and time from origin waypoint"),
    ("ZTG", "UTC and time to destination waypoint"),
];

impl SentenceType {
    /// Date/time fields whose typed values may be replaced by a caller's live
    /// UTC instant. The table follows the stable field positions published for
    /// these sentence types; an empty slice means no live-time substitution is
    /// defined, not that the sentence can never contain a time-like value.
    pub const fn time_fields(&self) -> &'static [(usize, TimeFieldKind)] {
        match self {
            // First data field is UTC. ASHR is the known `$PASHR` construction;
            // GRS, ZFO, and ZTG are included explicitly rather than inferred
            // from their names so additions remain reviewable.
            Self::ASHR
            | Self::BWC
            | Self::BWR
            | Self::GBS
            | Self::GGA
            | Self::GNS
            | Self::GRS
            | Self::GST
            | Self::ZFO
            | Self::ZTG => TIME_AT_0,
            Self::GLL => GLL_TIME_FIELDS,
            Self::RMC => RMC_TIME_FIELDS,
            Self::ZDA => ZDA_TIME_FIELDS,
            _ => &[],
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::AAM => "AAM",
            Self::ABK => "ABK",
            Self::ABM => "ABM",
            Self::ACK => "ACK",
            Self::AIR => "AIR",
            Self::AKD => "AKD",
            Self::ALA => "ALA",
            Self::ALM => "ALM",
            Self::APA => "APA",
            Self::APB => "APB",
            Self::ASHR => "ASHR",
            Self::BEC => "BEC",
            Self::BOD => "BOD",
            Self::BWC => "BWC",
            Self::BWR => "BWR",
            Self::BWW => "BWW",
            Self::DBK => "DBK",
            Self::DBS => "DBS",
            Self::DBT => "DBT",
            Self::DCN => "DCN",
            Self::DPT => "DPT",
            Self::DSC => "DSC",
            Self::DSE => "DSE",
            Self::DSI => "DSI",
            Self::DSR => "DSR",
            Self::DTM => "DTM",
            Self::FSI => "FSI",
            Self::GBS => "GBS",
            Self::GGA => "GGA",
            Self::GLC => "GLC",
            Self::GLL => "GLL",
            Self::GNS => "GNS",
            Self::GRS => "GRS",
            Self::GSA => "GSA",
            Self::GST => "GST",
            Self::GSV => "GSV",
            Self::GTD => "GTD",
            Self::GXA => "GXA",
            Self::HDG => "HDG",
            Self::HDM => "HDM",
            Self::HDT => "HDT",
            Self::HFB => "HFB",
            Self::HSC => "HSC",
            Self::ITS => "ITS",
            Self::LCD => "LCD",
            Self::MDA => "MDA",
            Self::MHU => "MHU",
            Self::MSK => "MSK",
            Self::MSS => "MSS",
            Self::MTA => "MTA",
            Self::MTW => "MTW",
            Self::MWD => "MWD",
            Self::MWV => "MWV",
            Self::OLN => "OLN",
            Self::OSD => "OSD",
            Self::R00 => "R00",
            Self::RDID => "RDID",
            Self::RMA => "RMA",
            Self::RMB => "RMB",
            Self::RMC => "RMC",
            Self::ROT => "ROT",
            Self::RPM => "RPM",
            Self::RSA => "RSA",
            Self::RSD => "RSD",
            Self::RTE => "RTE",
            Self::SFI => "SFI",
            Self::STN => "STN",
            Self::TDS => "TDS",
            Self::TFI => "TFI",
            Self::THS => "THS",
            Self::TLL => "TLL",
            Self::TPC => "TPC",
            Self::TPR => "TPR",
            Self::TPT => "TPT",
            Self::TRF => "TRF",
            Self::TTM => "TTM",
            Self::TUT => "TUT",
            Self::TXT => "TXT",
            Self::VBW => "VBW",
            Self::VDM => "VDM",
            Self::VDO => "VDO",
            Self::VDR => "VDR",
            Self::VHW => "VHW",
            Self::VLW => "VLW",
            Self::VPW => "VPW",
            Self::VTG => "VTG",
            Self::VWR => "VWR",
            Self::VWT => "VWT",
            Self::WCV => "WCV",
            Self::WNC => "WNC",
            Self::WPL => "WPL",
            Self::XDR => "XDR",
            Self::XTE => "XTE",
            Self::XTR => "XTR",
            Self::ZDA => "ZDA",
            Self::ZDL => "ZDL",
            Self::ZFO => "ZFO",
            Self::ZTG => "ZTG",
            Self::Custom(s) => s.as_str(),
        }
    }
}

impl fmt::Display for SentenceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SentenceType {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "AAM" => Self::AAM,
            "ABK" => Self::ABK,
            "ABM" => Self::ABM,
            "ACK" => Self::ACK,
            "AIR" => Self::AIR,
            "AKD" => Self::AKD,
            "ALA" => Self::ALA,
            "ALM" => Self::ALM,
            "APA" => Self::APA,
            "APB" => Self::APB,
            "ASHR" => Self::ASHR,
            "BEC" => Self::BEC,
            "BOD" => Self::BOD,
            "BWC" => Self::BWC,
            "BWR" => Self::BWR,
            "BWW" => Self::BWW,
            "DBK" => Self::DBK,
            "DBS" => Self::DBS,
            "DBT" => Self::DBT,
            "DCN" => Self::DCN,
            "DPT" => Self::DPT,
            "DSC" => Self::DSC,
            "DSE" => Self::DSE,
            "DSI" => Self::DSI,
            "DSR" => Self::DSR,
            "DTM" => Self::DTM,
            "FSI" => Self::FSI,
            "GBS" => Self::GBS,
            "GGA" => Self::GGA,
            "GLC" => Self::GLC,
            "GLL" => Self::GLL,
            "GNS" => Self::GNS,
            "GRS" => Self::GRS,
            "GSA" => Self::GSA,
            "GST" => Self::GST,
            "GSV" => Self::GSV,
            "GTD" => Self::GTD,
            "GXA" => Self::GXA,
            "HDG" => Self::HDG,
            "HDM" => Self::HDM,
            "HDT" => Self::HDT,
            "HFB" => Self::HFB,
            "HSC" => Self::HSC,
            "ITS" => Self::ITS,
            "LCD" => Self::LCD,
            "MDA" => Self::MDA,
            "MHU" => Self::MHU,
            "MSK" => Self::MSK,
            "MSS" => Self::MSS,
            "MTA" => Self::MTA,
            "MTW" => Self::MTW,
            "MWD" => Self::MWD,
            "MWV" => Self::MWV,
            "OLN" => Self::OLN,
            "OSD" => Self::OSD,
            "R00" => Self::R00,
            "RDID" => Self::RDID,
            "RMA" => Self::RMA,
            "RMB" => Self::RMB,
            "RMC" => Self::RMC,
            "ROT" => Self::ROT,
            "RPM" => Self::RPM,
            "RSA" => Self::RSA,
            "RSD" => Self::RSD,
            "RTE" => Self::RTE,
            "SFI" => Self::SFI,
            "STN" => Self::STN,
            "TDS" => Self::TDS,
            "TFI" => Self::TFI,
            "THS" => Self::THS,
            "TLL" => Self::TLL,
            "TPC" => Self::TPC,
            "TPR" => Self::TPR,
            "TPT" => Self::TPT,
            "TRF" => Self::TRF,
            "TTM" => Self::TTM,
            "TUT" => Self::TUT,
            "TXT" => Self::TXT,
            "VBW" => Self::VBW,
            "VDM" => Self::VDM,
            "VDO" => Self::VDO,
            "VDR" => Self::VDR,
            "VHW" => Self::VHW,
            "VLW" => Self::VLW,
            "VPW" => Self::VPW,
            "VTG" => Self::VTG,
            "VWR" => Self::VWR,
            "VWT" => Self::VWT,
            "WCV" => Self::WCV,
            "WNC" => Self::WNC,
            "WPL" => Self::WPL,
            "XDR" => Self::XDR,
            "XTE" => Self::XTE,
            "XTR" => Self::XTR,
            "ZDA" => Self::ZDA,
            "ZDL" => Self::ZDL,
            "ZFO" => Self::ZFO,
            "ZTG" => Self::ZTG,
            _ => Self::Custom(s.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_constant_matches_enum() {
        for s in ALL {
            let parsed: SentenceType = s.parse().expect("ALL entry parses");
            assert!(
                !matches!(parsed, SentenceType::Custom(_)),
                "ALL contains {s:?} but it parsed as Custom — the enum is missing this variant",
            );
            assert_eq!(parsed.as_str(), *s, "round-trip mismatch for {s:?}");
        }
    }

    #[test]
    fn all_with_desc_is_aligned_with_all() {
        assert_eq!(ALL.len(), ALL_WITH_DESC.len());
        for (i, (code, desc)) in ALL_WITH_DESC.iter().enumerate() {
            assert_eq!(*code, ALL[i], "ALL_WITH_DESC[{i}] code mismatch");
            assert!(!desc.is_empty(), "ALL_WITH_DESC[{i}] description is empty");
        }
    }

    #[test]
    fn known_types_round_trip() {
        let cases = [
            (SentenceType::AAM, "AAM"),
            (SentenceType::ABK, "ABK"),
            (SentenceType::ABM, "ABM"),
            (SentenceType::ACK, "ACK"),
            (SentenceType::AIR, "AIR"),
            (SentenceType::AKD, "AKD"),
            (SentenceType::ALA, "ALA"),
            (SentenceType::ALM, "ALM"),
            (SentenceType::APA, "APA"),
            (SentenceType::APB, "APB"),
            (SentenceType::ASHR, "ASHR"),
            (SentenceType::BEC, "BEC"),
            (SentenceType::BOD, "BOD"),
            (SentenceType::BWC, "BWC"),
            (SentenceType::BWR, "BWR"),
            (SentenceType::BWW, "BWW"),
            (SentenceType::DBK, "DBK"),
            (SentenceType::DBS, "DBS"),
            (SentenceType::DBT, "DBT"),
            (SentenceType::DCN, "DCN"),
            (SentenceType::DPT, "DPT"),
            (SentenceType::DSC, "DSC"),
            (SentenceType::DSE, "DSE"),
            (SentenceType::DSI, "DSI"),
            (SentenceType::DSR, "DSR"),
            (SentenceType::DTM, "DTM"),
            (SentenceType::FSI, "FSI"),
            (SentenceType::GBS, "GBS"),
            (SentenceType::GGA, "GGA"),
            (SentenceType::GLC, "GLC"),
            (SentenceType::GLL, "GLL"),
            (SentenceType::GNS, "GNS"),
            (SentenceType::GRS, "GRS"),
            (SentenceType::GSA, "GSA"),
            (SentenceType::GST, "GST"),
            (SentenceType::GSV, "GSV"),
            (SentenceType::GTD, "GTD"),
            (SentenceType::GXA, "GXA"),
            (SentenceType::HDG, "HDG"),
            (SentenceType::HDM, "HDM"),
            (SentenceType::HDT, "HDT"),
            (SentenceType::HFB, "HFB"),
            (SentenceType::HSC, "HSC"),
            (SentenceType::ITS, "ITS"),
            (SentenceType::LCD, "LCD"),
            (SentenceType::MDA, "MDA"),
            (SentenceType::MHU, "MHU"),
            (SentenceType::MSK, "MSK"),
            (SentenceType::MSS, "MSS"),
            (SentenceType::MTA, "MTA"),
            (SentenceType::MTW, "MTW"),
            (SentenceType::MWD, "MWD"),
            (SentenceType::MWV, "MWV"),
            (SentenceType::OLN, "OLN"),
            (SentenceType::OSD, "OSD"),
            (SentenceType::R00, "R00"),
            (SentenceType::RDID, "RDID"),
            (SentenceType::RMA, "RMA"),
            (SentenceType::RMB, "RMB"),
            (SentenceType::RMC, "RMC"),
            (SentenceType::ROT, "ROT"),
            (SentenceType::RPM, "RPM"),
            (SentenceType::RSA, "RSA"),
            (SentenceType::RSD, "RSD"),
            (SentenceType::RTE, "RTE"),
            (SentenceType::SFI, "SFI"),
            (SentenceType::STN, "STN"),
            (SentenceType::TDS, "TDS"),
            (SentenceType::TFI, "TFI"),
            (SentenceType::THS, "THS"),
            (SentenceType::TLL, "TLL"),
            (SentenceType::TPC, "TPC"),
            (SentenceType::TPR, "TPR"),
            (SentenceType::TPT, "TPT"),
            (SentenceType::TRF, "TRF"),
            (SentenceType::TTM, "TTM"),
            (SentenceType::TUT, "TUT"),
            (SentenceType::TXT, "TXT"),
            (SentenceType::VBW, "VBW"),
            (SentenceType::VDM, "VDM"),
            (SentenceType::VDO, "VDO"),
            (SentenceType::VDR, "VDR"),
            (SentenceType::VHW, "VHW"),
            (SentenceType::VLW, "VLW"),
            (SentenceType::VPW, "VPW"),
            (SentenceType::VTG, "VTG"),
            (SentenceType::VWR, "VWR"),
            (SentenceType::VWT, "VWT"),
            (SentenceType::WCV, "WCV"),
            (SentenceType::WNC, "WNC"),
            (SentenceType::WPL, "WPL"),
            (SentenceType::XDR, "XDR"),
            (SentenceType::XTE, "XTE"),
            (SentenceType::XTR, "XTR"),
            (SentenceType::ZDA, "ZDA"),
            (SentenceType::ZDL, "ZDL"),
            (SentenceType::ZFO, "ZFO"),
            (SentenceType::ZTG, "ZTG"),
        ];
        for (variant, s) in cases {
            assert_eq!(variant.as_str(), s);
            assert_eq!(s.parse::<SentenceType>().unwrap(), variant);
            assert_eq!(variant.to_string(), s);
        }
    }

    #[test]
    fn custom_type_round_trip() {
        let t: SentenceType = "FOO".parse().unwrap();
        assert_eq!(t, SentenceType::Custom("FOO".to_string()));
        assert_eq!(t.to_string(), "FOO");
    }

    #[test]
    fn live_time_field_map_is_explicit_and_complete_for_supported_types() {
        use TimeFieldKind::{DateDdmmyy, Day, Month, UtcTime, Year};

        let cases: &[(SentenceType, &[(usize, TimeFieldKind)])] = &[
            (SentenceType::ASHR, &[(0, UtcTime)]),
            (SentenceType::BWC, &[(0, UtcTime)]),
            (SentenceType::BWR, &[(0, UtcTime)]),
            (SentenceType::GBS, &[(0, UtcTime)]),
            (SentenceType::GGA, &[(0, UtcTime)]),
            (SentenceType::GLL, &[(4, UtcTime)]),
            (SentenceType::GNS, &[(0, UtcTime)]),
            (SentenceType::GRS, &[(0, UtcTime)]),
            (SentenceType::GST, &[(0, UtcTime)]),
            (SentenceType::RMC, &[(0, UtcTime), (8, DateDdmmyy)]),
            (
                SentenceType::ZDA,
                &[(0, UtcTime), (1, Day), (2, Month), (3, Year)],
            ),
            (SentenceType::ZFO, &[(0, UtcTime)]),
            (SentenceType::ZTG, &[(0, UtcTime)]),
        ];

        for (sentence, expected) in cases {
            assert_eq!(sentence.time_fields(), *expected, "{sentence}");
        }
        assert!(SentenceType::Custom("ZZZ".into()).time_fields().is_empty());
        assert!(SentenceType::HDT.time_fields().is_empty());
    }

    #[test]
    fn every_time_field_map_is_sorted_and_has_unique_positions() {
        for code in ALL {
            let sentence: SentenceType = code.parse().unwrap();
            let fields = sentence.time_fields();
            assert!(
                fields.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "{code} time-field positions are not strictly increasing: {fields:?}"
            );
        }
    }
}
