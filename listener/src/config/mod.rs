//! Profile schema, configuration load/save, validation, and templates.
//!
//! This is `listener-config` (§128). It owns the persisted [`Profile`] schema
//! (§67–§80), load/save, validation (§71), and starting templates (§81–§85). It
//! stores configuration only — never runtime objects (§5.7, §69). Mapping a
//! validated config to live transports is the runtime's job (§128),
//! not config's.

pub mod limits;
pub mod schema;
pub mod templates;

pub use limits::LimitError;
pub use schema::*;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::ChannelKind;
use crate::record::{is_filesystem_safe, FileRotationPolicy};
use crate::transport::udp::UdpMode;

/// The schema version this build understands (§72.1). Bumped to 2 for the v2.0
/// stream-only schema (extraction/decoder/subsample fields removed — ADR-010), then
/// to 3 when the single `recording` table was split into independent
/// `raw_recording` / `display_recording` (ADR-013) — a clean break, so v1/v2 profiles
/// are refused with a "recreate the profile" error.
pub const CURRENT_VERSION: u32 = 3;

/// A persisted Listener workspace (§67, §72). A complete set of configured
/// Channels plus profile-level defaults.
///
/// Loads strictly (ADR-048): a key the schema does not have is refused, with
/// its line, rather than passing for a default. Missing fields still take
/// their defaults, so an additive change within a schema version still loads
/// older files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub defaults: DefaultConfig,
}

impl Profile {
    /// A new, empty profile stamped with the current schema version.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: CURRENT_VERSION,
            name: name.into(),
            channels: Vec::new(),
            defaults: DefaultConfig::default(),
        }
    }

    /// Parse a profile from TOML text, enforcing schema-version compatibility
    /// (§72.1). The version is checked first, so a profile from another schema
    /// is refused for that rather than for keys this one does not have.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let doc: toml::Table = toml::from_str(text)?;
        match doc.get("schema_version") {
            None => return Err(ConfigError::MissingSchemaVersion),
            Some(toml::Value::Integer(found)) => {
                // Out of range is left to the typed parse, which says so.
                if let Ok(found) = u32::try_from(*found) {
                    check_version(found)?;
                }
            }
            // A wrong type is left to the typed parse, which names it.
            Some(_) => {}
        }
        let profile: Profile = toml::from_str(text)?;
        check_version(profile.schema_version)?;
        Ok(profile)
    }

    /// Serialize this profile to TOML text.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Load a profile from a file (§70). Does not start channels or touch
    /// interfaces — it only reads and validates the schema version.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::from_toml(&std::fs::read_to_string(path)?)
    }

    /// Save this profile to a file.
    ///
    /// The write is atomic: the text goes to a temp file beside the target,
    /// which is then renamed over it, so a crash mid-save leaves the previous
    /// profile whole. Each save gets its own temp file, so two saves racing on
    /// one path (the GUI and a CLI) never write into each other's.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let tmp = write_temp_sibling(path, &self.to_toml()?)?;
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
        Ok(())
    }

    /// Validate every channel without starting anything (§71). A channel's
    /// validity is reported independently — one invalid channel does not
    /// invalidate the others. Returns `(channel name, result)` per channel.
    ///
    /// Channel-name **uniqueness** (§6, ADR-014) is a workspace-level rule, so it is
    /// applied here rather than in per-channel `validate_channel`: a name shared by two
    /// or more channels marks *every* channel carrying that name as invalid (the user
    /// must rename to disambiguate). Names compare case-insensitively, matching the
    /// filename collision they guard against on case-insensitive filesystems.
    pub fn validate(&self) -> Vec<(String, Result<(), Vec<ChannelConfigError>>)> {
        use std::collections::HashMap;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for channel in &self.channels {
            *counts
                .entry(channel.name.as_str().to_ascii_lowercase())
                .or_insert(0) += 1;
        }
        self.channels
            .iter()
            .map(|channel| {
                let result = validate_channel(channel, &self.defaults);
                let duplicated = counts
                    .get(&channel.name.as_str().to_ascii_lowercase())
                    .is_some_and(|&n| n > 1);
                let result = if duplicated {
                    let mut errors = result.err().unwrap_or_default();
                    errors.push(ChannelConfigError::DuplicateChannelName);
                    Err(errors)
                } else {
                    result
                };
                (channel.name.as_str().to_string(), result)
            })
            .collect()
    }
}

/// Validate one channel's configuration (§71). Resource existence (e.g. whether
/// a COM port is present) is deferred to Start; this checks only structural
/// validity.
/// Whether a Channel's name must be filesystem-safe (§59, §71). When a
/// recording rotates, the name becomes part of its generated filenames;
/// non-rotating recordings use a fixed destination path and do not constrain
/// it. Either recording can rotate (they are independent — ADR-013), so both
/// count. Whether it records on start does not matter: the Record toggle or a
/// match rule can begin it live.
pub fn name_must_be_filesystem_safe(channel: &ChannelConfig) -> bool {
    channel.raw_recording.file_rotation != FileRotationPolicy::None
        || channel.display_recording.file_rotation != FileRotationPolicy::None
}

pub fn validate_channel(
    channel: &ChannelConfig,
    defaults: &DefaultConfig,
) -> Result<(), Vec<ChannelConfigError>> {
    let mut errors = Vec::new();

    // The Channel Kind must match its interface, and a TCP Connection is
    // runtime-only — it can never appear in a persisted profile (§16.3).
    let kind_matches = matches!(
        (channel.kind, &channel.interface),
        (ChannelKind::Serial, InterfaceConfig::Serial(_))
            | (ChannelKind::Udp, InterfaceConfig::Udp(_))
            | (ChannelKind::TcpListener, InterfaceConfig::TcpListener(_))
    );
    if channel.kind == ChannelKind::TcpConnection {
        errors.push(ChannelConfigError::TcpConnectionNotPersistable);
    } else if !kind_matches {
        errors.push(ChannelConfigError::KindInterfaceMismatch);
    }
    // Disabled in this release (§4.1, ADR-047): its connections' data cannot be
    // displayed or recorded yet (ADR-024). The code stays for when it returns.
    if channel.kind == ChannelKind::TcpListener
        || matches!(channel.interface, InterfaceConfig::TcpListener(_))
    {
        errors.push(ChannelConfigError::TcpListenerUnavailable);
    }

    if let InterfaceConfig::Udp(udp) = &channel.interface {
        if udp.mode == UdpMode::Multicast
            && udp
                .multicast_group
                .as_deref()
                .is_none_or(|g| g.trim().is_empty())
        {
            errors.push(ChannelConfigError::MissingMulticastGroup);
        }
        // Not offered for unicast (§15, ADR-047).
        if udp.shared_port && udp.mode == UdpMode::Unicast {
            errors.push(ChannelConfigError::SharedPortUnicast);
        }
    }

    if name_must_be_filesystem_safe(channel) && !is_filesystem_safe(channel.name.as_str()) {
        errors.push(ChannelConfigError::InvalidChannelName);
    }

    // Match Rules (§50.2, §165). A `BytePattern` with an empty pattern would match
    // everywhere (an empty needle is always found) — reject it.
    for rule in &channel.match_rules {
        if let MatchCondition::BytePattern { pattern } = &rule.condition {
            if pattern.is_empty() {
                errors.push(ChannelConfigError::EmptyMatchPattern);
            }
        }
        for action in &rule.actions {
            if let MatchAction::Mark {
                timestamp:
                    Some(MarkTimestamp {
                        style: MarkTimestampStyle::NmeaZda { talker },
                        ..
                    }),
            } = action
            {
                if let Err(error) = crate::core::validate_zda_talker_id(talker) {
                    errors.push(ChannelConfigError::InvalidZdaTalkerId(error));
                }
            }
        }
    }

    // Retention must be bounded (§80): use the channel's own limits, or the
    // profile default if the channel sets none.
    let effective = if channel.retention.is_unbounded() {
        defaults
            .retention
            .clone()
            .unwrap_or_else(|| channel.retention.clone())
    } else {
        channel.retention.clone()
    };
    if effective.is_unbounded() {
        errors.push(ChannelConfigError::UnboundedRetention);
    }

    // Limits (ADR-048, §71): the runtime refuses the same values at Start.
    let limits = effective
        .limit_errors()
        .into_iter()
        .chain(limits::match_rule_errors(&channel.match_rules))
        .chain(channel.reconnect.limit_errors())
        .chain(limits::size_cap_error(
            "raw",
            channel.raw_recording.size_cap,
        ))
        .chain(limits::size_cap_error(
            "display",
            channel.display_recording.size_cap,
        ));
    errors.extend(limits.map(ChannelConfigError::Limit));

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Non-fatal configuration warnings (§71): the channel is still valid and will
/// run, but something is likely a mistake. Surfaced to the user (e.g. printed at
/// profile load) without skipping the channel.
pub fn channel_warnings(_channel: &ChannelConfig) -> Vec<ChannelConfigWarning> {
    // No warning conditions currently exist; the v1 decoded-field-without-decoder
    // warning was removed with the decoder (ADR-010). The surface stays so future
    // stream-era warnings (§71) have a home.
    Vec::new()
}

/// Write `text` to a new temp file beside `path` and return its path.
///
/// The process id and a per-process counter make the name unique, and
/// `create_new` refuses a name that is already taken rather than writing into
/// another save's file.
fn write_temp_sibling(path: &Path, text: &str) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = std::path::PathBuf::from(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    if let Err(error) = file.write_all(text.as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(tmp)
}

/// Errors from loading or saving a profile (§72.1).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("profile parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("profile serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("profile schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("profile schema version {found} is older than {supported} and has no migration")]
    SchemaTooOld { found: u32, supported: u32 },
    /// A profile that does not say its schema is not taken to be current
    /// (ADR-048).
    #[error("the profile has no schema_version: add `schema_version = {CURRENT_VERSION}`")]
    MissingSchemaVersion,
}

/// Schema-version compatibility (§72.1): equal loads; newer is refused;
/// older is refused (no migration exists for the v1 series yet).
fn check_version(found: u32) -> Result<(), ConfigError> {
    use std::cmp::Ordering::*;
    match found.cmp(&CURRENT_VERSION) {
        Equal => Ok(()),
        Greater => Err(ConfigError::SchemaTooNew {
            found,
            supported: CURRENT_VERSION,
        }),
        Less => Err(ConfigError::SchemaTooOld {
            found,
            supported: CURRENT_VERSION,
        }),
    }
}

/// A structural problem in one channel's configuration (§71).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelConfigError {
    #[error("channel kind does not match its interface configuration")]
    KindInterfaceMismatch,
    #[error("TCP Connection channels are runtime-only and cannot be persisted")]
    TcpConnectionNotPersistable,
    #[error(
        "TCP Listener isn't available in this release: received data can't be displayed or \
         recorded yet"
    )]
    TcpListenerUnavailable,
    #[error("multicast UDP requires a multicast group address")]
    MissingMulticastGroup,
    #[error(
        "a shared port is offered for broadcast and multicast only: in unicast the OS \
         delivers each datagram to only one of the programs sharing the port"
    )]
    SharedPortUnicast,
    #[error("retention is unbounded: set a limit on the channel or in defaults")]
    UnboundedRetention,
    #[error(
        "channel name is not filesystem-safe but recording file rotation uses it in filenames (§59)"
    )]
    InvalidChannelName,
    #[error("channel name duplicates another channel's — names must be unique (§6, §71)")]
    DuplicateChannelName,
    #[error("a match rule's byte pattern is empty (it would match at every byte offset, §50.2)")]
    EmptyMatchPattern,
    #[error("invalid NMEA ZDA Mark talker ID: {0}")]
    InvalidZdaTalkerId(crate::core::ZdaTalkerIdError),
    /// A setting outside its limit (ADR-048, §71).
    #[error(transparent)]
    Limit(LimitError),
}

/// A non-fatal configuration warning (§71): the channel runs, but this is likely
/// not what the user intended. Currently empty — the v1 decoded-field-without-
/// decoder warning went with the decoder (ADR-010).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ChannelConfigWarning {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp files a save left beside `path`.
    fn temp_siblings(path: &Path) -> Vec<std::path::PathBuf> {
        let prefix = format!("{}.", path.file_name().unwrap().to_string_lossy());
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with(&prefix) && name.ends_with(".tmp")
            })
            .collect()
    }

    /// Saves racing on one path — say the GUI and a CLI saving the same
    /// profile — each succeed, and the file ends up holding one whole profile.
    #[test]
    fn concurrent_saves_to_one_path_each_land_whole() {
        let path = std::env::temp_dir().join(format!(
            "listener_concurrent_save_{}.toml",
            std::process::id()
        ));
        let writers: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // Different lengths, so a torn write would not parse.
                    let mut profile = Profile::new(format!("writer {n}"));
                    profile.channels = vec![templates::udp_template(); n];
                    for _ in 0..25 {
                        profile.save(&path).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let loaded = Profile::load(&path).unwrap();
        assert!(loaded.name.starts_with("writer "));
        assert_eq!(temp_siblings(&path), Vec::<std::path::PathBuf>::new());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_byte_pattern_match_rule_is_rejected() {
        let mut channel = templates::udp_template();
        channel.match_rules = vec![MatchRule {
            name: "everything".to_string(),
            condition: MatchCondition::BytePattern { pattern: vec![] },
            actions: vec![MatchAction::Mark { timestamp: None }],
            enabled: true,
        }];
        let errs = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(errs.contains(&ChannelConfigError::EmptyMatchPattern));

        // A non-empty pattern validates.
        channel.match_rules[0].condition = MatchCondition::BytePattern {
            pattern: b"GGA".to_vec(),
        };
        assert!(validate_channel(&channel, &DefaultConfig::default()).is_ok());
    }

    #[test]
    fn match_rules_round_trip_through_toml() {
        let mut profile = Profile::new("rules");
        let mut channel = templates::serial_template();
        channel.match_rules = vec![
            MatchRule {
                name: "GGA mark".to_string(),
                condition: MatchCondition::BytePattern {
                    pattern: b"$GPGGA".to_vec(),
                },
                // A timestamped Mark with a separator, so the full MarkTimestamp
                // shape (incl. the additive `separator`) round-trips (§72.1).
                actions: vec![MatchAction::Mark {
                    timestamp: Some(MarkTimestamp {
                        position: MarkPosition::After,
                        style: MarkTimestampStyle::Plain {},
                        format: crate::core::TimestampConfig {
                            include_date: false,
                            include_millis: true,
                            include_timezone: false,
                        },
                        separator: ", ".to_string(),
                    }),
                }],
                enabled: true,
            },
            MatchRule {
                name: "go quiet".to_string(),
                condition: MatchCondition::Idle { timeout_ms: 5_000 },
                actions: vec![
                    MatchAction::Notify {
                        severity: crate::diagnostics::DiagnosticSeverity::Warning,
                    },
                    MatchAction::Record {
                        target: RecordTarget::Both,
                        control: RecordControl::Begin,
                    },
                    MatchAction::PauseDisplay { view: Some(0) },
                ],
                enabled: false,
            },
        ];
        profile.channels = vec![channel];
        let toml = profile.to_toml().expect("serialize");
        let parsed = Profile::from_toml(&toml).expect("round trip");
        assert_eq!(parsed, profile);
    }

    #[test]
    fn a_shared_port_is_offered_for_broadcast_and_multicast_only() {
        let old: UdpConfig =
            toml::from_str("bind_address = '0.0.0.0'\nport = 9000\nmode = 'Broadcast'\n").unwrap();
        assert!(!old.shared_port, "off by default");

        let mut channel = templates::udp_template();
        let InterfaceConfig::Udp(udp) = &mut channel.interface else {
            unreachable!("the UDP template is UDP")
        };
        udp.port = 9000;
        udp.shared_port = true;
        udp.mode = UdpMode::Broadcast;
        assert_eq!(
            validate_channel(&channel, &DefaultConfig::default()),
            Ok(())
        );

        let InterfaceConfig::Udp(udp) = &mut channel.interface else {
            unreachable!("the UDP template is UDP")
        };
        udp.mode = UdpMode::Unicast;
        assert_eq!(
            validate_channel(&channel, &DefaultConfig::default()),
            Err(vec![ChannelConfigError::SharedPortUnicast])
        );
    }

    #[test]
    fn udp_kernel_timestamps_are_additive_and_default_off() {
        let old: UdpConfig =
            toml::from_str("bind_address = '0.0.0.0'\nport = 9000\nmode = 'Unicast'\n").unwrap();
        assert!(!old.kernel_timestamps);

        let mut enabled = old;
        enabled.kernel_timestamps = true;
        let text = toml::to_string(&enabled).unwrap();
        assert!(
            toml::from_str::<UdpConfig>(&text)
                .unwrap()
                .kernel_timestamps
        );
    }

    #[test]
    fn mark_timestamp_without_style_defaults_to_plain() {
        let timestamp: MarkTimestamp = toml::from_str("separator = ' '").unwrap();
        assert_eq!(timestamp.style, MarkTimestampStyle::Plain {});
    }

    #[test]
    fn zda_mark_style_round_trips_and_defaults_its_talker() {
        let timestamp = MarkTimestamp {
            position: MarkPosition::After,
            style: MarkTimestampStyle::NmeaZda {
                talker: "RECEIVER_A".to_string(),
            },
            format: crate::core::TimestampConfig {
                include_millis: true,
                ..Default::default()
            },
            separator: "\r\n".to_string(),
        };
        let text = toml::to_string(&timestamp).unwrap();
        assert_eq!(toml::from_str::<MarkTimestamp>(&text).unwrap(), timestamp);

        let defaulted: MarkTimestamp = toml::from_str("[style]\nkind = 'nmea_zda'").unwrap();
        assert_eq!(
            defaulted.style,
            MarkTimestampStyle::NmeaZda {
                talker: "GP".to_string()
            }
        );
    }

    #[test]
    fn channel_validation_accepts_long_zda_talkers_and_rejects_unsafe_ones() {
        let mut channel = templates::udp_template();
        channel.match_rules.push(MatchRule {
            name: "zda".to_string(),
            condition: MatchCondition::BytePattern {
                pattern: b"$GP".to_vec(),
            },
            actions: vec![MatchAction::Mark {
                timestamp: Some(MarkTimestamp {
                    style: MarkTimestampStyle::NmeaZda {
                        talker: "CUSTOM_RECEIVER".to_string(),
                    },
                    ..Default::default()
                }),
            }],
            enabled: true,
        });
        assert!(validate_channel(&channel, &DefaultConfig::default()).is_ok());

        let MatchAction::Mark {
            timestamp: Some(timestamp),
        } = &mut channel.match_rules[0].actions[0]
        else {
            panic!("test rule must contain a timestamped Mark");
        };
        timestamp.style = MarkTimestampStyle::NmeaZda {
            talker: "BAD,ID".to_string(),
        };
        let errors = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(errors.iter().any(|error| matches!(
            error,
            ChannelConfigError::InvalidZdaTalkerId(
                crate::core::ZdaTalkerIdError::InvalidCharacter(',')
            )
        )));
    }

    #[test]
    fn recording_defaults_arm_a_new_channel() {
        // New channels are ready to record once asked: a writable home-area
        // destination, Hourly rotation, Append on-exists — both taps (ADR-013).
        // Recording itself stays off until the user enables it.
        use crate::record::{FileRotationPolicy, OverwritePolicy};
        let raw = RawRecordingConfig::default();
        assert!(raw.destination.is_some(), "a writable default destination");
        assert_eq!(raw.overwrite_policy, OverwritePolicy::AppendIfExists);
        assert_eq!(raw.file_rotation, FileRotationPolicy::Hourly);
        assert!(!raw.enabled);

        let disp = DisplayRecordingConfig::default();
        assert_eq!(disp.destination, raw.destination, "shared default folder");
        assert_eq!(disp.overwrite_policy, OverwritePolicy::AppendIfExists);
        assert_eq!(disp.file_rotation, FileRotationPolicy::Hourly);
        assert!(!disp.enabled);
    }

    #[test]
    fn templates_are_valid() {
        let mut profile = Profile::new("templates");
        profile.channels = vec![templates::serial_template(), templates::udp_template()];
        for (name, result) in profile.validate() {
            assert!(result.is_ok(), "{name} should be valid: {result:?}");
        }
    }

    #[test]
    fn a_tcp_listener_is_not_available_in_this_release() {
        // §4.1, ADR-047: rejected with the reason, not silently skipped.
        let err = validate_channel(
            &templates::tcp_listener_template(),
            &DefaultConfig::default(),
        )
        .unwrap_err();
        assert_eq!(err, [ChannelConfigError::TcpListenerUnavailable]);
        assert_eq!(
            err[0].to_string(),
            "TCP Listener isn't available in this release: received data can't be displayed \
             or recorded yet"
        );
    }

    #[test]
    fn newer_schema_version_is_refused() {
        let toml = format!("schema_version = {}\nname = \"x\"\n", CURRENT_VERSION + 1);
        assert!(matches!(
            Profile::from_toml(&toml),
            Err(ConfigError::SchemaTooNew { .. })
        ));
    }

    #[test]
    fn older_schema_version_is_refused() {
        let toml = "schema_version = 0\nname = \"x\"\n";
        assert!(matches!(
            Profile::from_toml(toml),
            Err(ConfigError::SchemaTooOld { .. })
        ));
    }

    #[test]
    fn missing_additive_fields_default_in() {
        // Only the required fields are present; everything else defaults (§72.1).
        let toml = "schema_version = 3\nname = \"minimal\"\n";
        let profile = Profile::from_toml(toml).expect("defaults fill in");
        assert_eq!(profile.schema_version, CURRENT_VERSION);
        assert!(profile.channels.is_empty());
        assert_eq!(profile.defaults, DefaultConfig::default());
    }

    /// The example profile the spec points readers to (§72) must load, every
    /// Channel in it must be valid, and nothing it says may be dropped on the
    /// way into the profile types.
    #[test]
    fn the_example_profile_loads_and_says_only_what_the_schema_has() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/profiles/profile.example.toml"
        ));
        let profile = Profile::load(path).expect("profile.example.toml loads");
        assert!(!profile.channels.is_empty());
        for (name, result) in profile.validate() {
            assert_eq!(result, Ok(()), "{name}");
        }

        fn contained(written: &toml::Value, read_back: &toml::Value) -> bool {
            match (written, read_back) {
                (toml::Value::Table(w), toml::Value::Table(r)) => w
                    .iter()
                    .all(|(key, value)| r.get(key).is_some_and(|back| contained(value, back))),
                (toml::Value::Array(w), toml::Value::Array(r)) => {
                    w.len() == r.len() && w.iter().zip(r).all(|(a, b)| contained(a, b))
                }
                _ => written == read_back,
            }
        }
        let written: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let read_back: toml::Value = toml::from_str(&profile.to_toml().unwrap()).unwrap();
        assert!(
            contained(&written, &read_back),
            "profile.example.toml holds a value the profile types drop"
        );
    }

    /// ADR-048: a profile without `schema_version` is not taken to be current.
    #[test]
    fn a_missing_schema_version_is_refused() {
        let err = Profile::from_toml("name = \"x\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::MissingSchemaVersion));
        assert_eq!(
            err.to_string(),
            "the profile has no schema_version: add `schema_version = 3`"
        );
    }

    /// A profile that uses every profile type, so a misspelled key can be put
    /// in each.
    fn every_type() -> Profile {
        use crate::core::ChannelName;
        use crate::diagnostics::DiagnosticSeverity;

        let mut udp = templates::udp_template();
        udp.name = ChannelName::new("udp");
        udp.raw_recording.disk_guard = Some(DiskGuard {
            min_free: DiskThreshold::Percent { percent: 5 },
            on_low: LowDiskAction::Warn,
        });
        udp.display_recording.disk_guard = Some(DiskGuard {
            min_free: DiskThreshold::Bytes { bytes: 1 << 30 },
            on_low: LowDiskAction::StopRecording,
        });
        udp.match_rules = vec![
            MatchRule {
                name: "gga".to_owned(),
                condition: MatchCondition::BytePattern {
                    pattern: b"$GPGGA".to_vec(),
                },
                actions: vec![
                    MatchAction::Record {
                        target: RecordTarget::Raw,
                        control: RecordControl::Begin,
                    },
                    MatchAction::Mark {
                        timestamp: Some(MarkTimestamp {
                            style: MarkTimestampStyle::NmeaZda {
                                talker: "GP".to_owned(),
                            },
                            ..MarkTimestamp::default()
                        }),
                    },
                    MatchAction::Notify {
                        severity: DiagnosticSeverity::Warning,
                    },
                    MatchAction::PauseDisplay { view: Some(0) },
                    MatchAction::Mark {
                        timestamp: Some(MarkTimestamp::default()),
                    },
                ],
                enabled: true,
            },
            MatchRule {
                name: "quiet".to_owned(),
                condition: MatchCondition::Idle { timeout_ms: 1_000 },
                actions: Vec::new(),
                enabled: true,
            },
        ];
        let mut serial = templates::serial_template();
        serial.name = ChannelName::new("serial");
        let mut tcp = templates::tcp_listener_template();
        tcp.name = ChannelName::new("tcp");

        let mut profile = Profile::new("every type");
        profile.channels = vec![udp, serial, tcp];
        profile.defaults = DefaultConfig {
            display: Some(DisplayConfig::default()),
            retention: Some(RetentionConfig::with_byte_limit(1_000)),
        };
        profile
    }

    /// ADR-048: an unknown key is refused in every profile type, and the
    /// message names it and its line. Serde's deny-unknown-fields support is
    /// partial for internally tagged enums, so each type is tried, not one.
    #[test]
    fn a_misspelled_key_is_refused_wherever_it_is() {
        let text = every_type().to_toml().unwrap();
        Profile::from_toml(&text).expect("the profile without a misspelling loads");
        let doc: toml::Value = toml::from_str(&text).unwrap();

        // (where, the type found there)
        let cases = [
            ("", "Profile"),
            ("channels.0", "ChannelConfig"),
            ("channels.0.interface", "UdpConfig"),
            ("channels.1.interface", "SerialConfig"),
            ("channels.2.interface", "TcpListenerConfig"),
            ("channels.0.reconnect", "ReconnectPolicy"),
            ("channels.0.display", "DisplayConfig"),
            ("channels.0.display.views.0", "DisplayViewConfig"),
            ("channels.0.display.views.0.hex_grouping", "HexGrouping"),
            ("channels.0.raw_recording", "RawRecordingConfig"),
            ("channels.0.raw_recording.disk_guard", "DiskGuard"),
            (
                "channels.0.raw_recording.disk_guard.min_free",
                "DiskThreshold::Percent",
            ),
            ("channels.0.display_recording", "DisplayRecordingConfig"),
            (
                "channels.0.display_recording.disk_guard.min_free",
                "DiskThreshold::Bytes",
            ),
            ("channels.0.retention", "RetentionConfig"),
            ("channels.0.match_rules.0", "MatchRule"),
            (
                "channels.0.match_rules.0.condition",
                "MatchCondition::BytePattern",
            ),
            ("channels.0.match_rules.1.condition", "MatchCondition::Idle"),
            ("channels.0.match_rules.0.actions.0", "MatchAction::Record"),
            ("channels.0.match_rules.0.actions.1", "MatchAction::Mark"),
            (
                "channels.0.match_rules.0.actions.1.timestamp",
                "MarkTimestamp",
            ),
            (
                "channels.0.match_rules.0.actions.1.timestamp.style",
                "MarkTimestampStyle::NmeaZda",
            ),
            (
                "channels.0.match_rules.0.actions.1.timestamp.format",
                "TimestampConfig",
            ),
            ("channels.0.match_rules.0.actions.2", "MatchAction::Notify"),
            (
                "channels.0.match_rules.0.actions.3",
                "MatchAction::PauseDisplay",
            ),
            (
                "channels.0.match_rules.0.actions.4.timestamp.style",
                "MarkTimestampStyle::Plain",
            ),
            ("defaults", "DefaultConfig"),
            ("defaults.display", "DisplayConfig in defaults"),
            ("defaults.retention", "RetentionConfig in defaults"),
        ];
        for (path, kind) in cases {
            let mut misspelled = doc.clone();
            let mut table = &mut misspelled;
            for step in path.split('.').filter(|step| !step.is_empty()) {
                table = match step.parse::<usize>() {
                    Ok(index) => &mut table[index],
                    Err(_) => &mut table[step],
                };
            }
            table
                .as_table_mut()
                .unwrap_or_else(|| panic!("{kind}: {path} is not a table"))
                .insert("misspelled_key".to_owned(), toml::Value::Integer(1));
            let text = toml::to_string(&misspelled).unwrap();
            let err = Profile::from_toml(&text)
                .expect_err(&format!("{kind} accepted an unknown key"))
                .to_string();
            assert!(
                err.contains("unknown field `misspelled_key`"),
                "{kind}: {err}"
            );
            assert!(err.contains(" at line "), "{kind}: no line in {err}");
        }
    }

    /// ADR-048, §71: a profile holding a value past a limit names it.
    #[test]
    fn a_setting_past_its_limit_is_rejected_with_the_limit() {
        let mut channel = templates::udp_template();
        channel.retention.byte_limit = Some(limits::MAX_SCROLLBACK_BYTES + 1);
        channel.reconnect.multiplier = 20.0;
        channel.raw_recording.size_cap = Some(1024);
        channel.match_rules = vec![MatchRule {
            name: "long".to_owned(),
            condition: MatchCondition::BytePattern {
                pattern: vec![b'x'; limits::MAX_MATCH_PATTERN_BYTES + 1],
            },
            actions: Vec::new(),
            enabled: true,
        }];
        let errors = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        let limits: Vec<&LimitError> = errors
            .iter()
            .filter_map(|error| match error {
                ChannelConfigError::Limit(limit) => Some(limit),
                _ => None,
            })
            .collect();
        assert!(
            matches!(limits[0], LimitError::Scrollback { .. }),
            "{errors:?}"
        );
        assert!(
            matches!(limits[1], LimitError::MatchPattern { .. }),
            "{errors:?}"
        );
        assert!(
            matches!(limits[2], LimitError::BackoffMultiplier { .. }),
            "{errors:?}"
        );
        assert!(
            matches!(limits[3], LimitError::SizeCap { .. }),
            "{errors:?}"
        );
        assert_eq!(limits.len(), 4, "{errors:?}");
        // The message is the limit's own: it says what to change.
        assert_eq!(
            ChannelConfigError::Limit(limits[2].clone()).to_string(),
            "reconnect multiplier 20 is outside 1.0–10"
        );
    }

    /// §59, §71: a rotating recording names its files after the Channel, and
    /// it can begin live, so the name is checked whether or not it records on
    /// start. A recording that does not rotate writes to a fixed path.
    #[test]
    fn a_channel_name_is_checked_as_a_filename_whenever_rotation_is_configured() {
        use crate::core::ChannelName;
        let unsafe_name = |raw: FileRotationPolicy, display: FileRotationPolicy| {
            let mut channel = templates::udp_template();
            channel.name = ChannelName::new("GPS/feed");
            channel.raw_recording.enabled = false;
            channel.raw_recording.file_rotation = raw;
            channel.display_recording.enabled = false;
            channel.display_recording.file_rotation = display;
            validate_channel(&channel, &DefaultConfig::default())
        };
        for (raw, display) in [
            (FileRotationPolicy::Hourly, FileRotationPolicy::None),
            (FileRotationPolicy::None, FileRotationPolicy::Daily),
        ] {
            assert_eq!(
                unsafe_name(raw, display),
                Err(vec![ChannelConfigError::InvalidChannelName]),
                "{raw:?} / {display:?}"
            );
        }
        assert_eq!(
            unsafe_name(FileRotationPolicy::None, FileRotationPolicy::None),
            Ok(())
        );
    }

    #[test]
    fn unbounded_retention_is_rejected() {
        let mut channel = templates::udp_template();
        channel.retention = RetentionConfig::default(); // all None
        let err = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(err.contains(&ChannelConfigError::UnboundedRetention));
    }

    #[test]
    fn profile_default_retention_satisfies_a_channel_without_one() {
        let mut channel = templates::udp_template();
        channel.retention = RetentionConfig::default(); // all None
        let defaults = DefaultConfig {
            retention: Some(RetentionConfig::with_byte_limit(500)),
            ..DefaultConfig::default()
        };
        assert!(validate_channel(&channel, &defaults).is_ok());
    }

    #[test]
    fn duplicate_channel_names_are_rejected_for_every_carrier() {
        // Two channels share a name (case-insensitively) → both are flagged; the third,
        // uniquely named, stays valid. Uniqueness is a workspace rule (§6, ADR-014).
        let mut profile = Profile::new("dupes");
        use crate::core::ChannelName;
        let mut a = templates::udp_template();
        a.name = ChannelName::new("Feed");
        let mut b = templates::udp_template();
        b.name = ChannelName::new("feed"); // same name, different case
        let mut c = templates::udp_template();
        c.name = ChannelName::new("Other");
        profile.channels = vec![a, b, c];

        let results = profile.validate();
        let dup = |name: &str| {
            results
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, r)| {
                    r.as_ref()
                        .err()
                        .is_some_and(|e| e.contains(&ChannelConfigError::DuplicateChannelName))
                })
                .unwrap()
        };
        assert!(dup("Feed"), "both duplicate carriers are flagged");
        assert!(dup("feed"), "both duplicate carriers are flagged");
        assert!(!dup("Other"), "the unique name stays valid");
    }

    #[test]
    fn kind_interface_mismatch_is_caught() {
        let mut channel = templates::udp_template();
        channel.kind = ChannelKind::Serial; // interface is still UDP
        let err = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(err.contains(&ChannelConfigError::KindInterfaceMismatch));
    }

    #[test]
    fn profiles_never_carry_tcp_connection_channels() {
        let mut channel = templates::tcp_listener_template();
        channel.kind = ChannelKind::TcpConnection;
        let err = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(err.contains(&ChannelConfigError::TcpConnectionNotPersistable));
    }
}
