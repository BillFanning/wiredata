//! Profile schema, configuration load/save, validation, and templates.
//!
//! This is `listener-config` (§128). It owns the persisted [`Profile`] schema
//! (§67–§80), load/save, validation (§71), and starting templates (§81–§85). It
//! stores configuration only — never runtime objects (§5.7, §69). Mapping a
//! validated config to live transports is the runtime's job (§128),
//! not config's.

pub mod schema;
pub mod templates;

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

fn current_version() -> u32 {
    CURRENT_VERSION
}

/// A persisted Listener workspace (§67, §72). A complete set of configured
/// Channels plus profile-level defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default = "current_version")]
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
    /// (§72.1).
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let profile: Profile = toml::from_str(text)?;
        profile.check_version()?;
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

    /// Schema-version compatibility (§72.1): equal loads; newer is refused;
    /// older is refused (no migration exists for the v1 series yet).
    fn check_version(&self) -> Result<(), ConfigError> {
        use std::cmp::Ordering::*;
        match self.schema_version.cmp(&CURRENT_VERSION) {
            Equal => Ok(()),
            Greater => Err(ConfigError::SchemaTooNew {
                found: self.schema_version,
                supported: CURRENT_VERSION,
            }),
            Less => Err(ConfigError::SchemaTooOld {
                found: self.schema_version,
                supported: CURRENT_VERSION,
            }),
        }
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

    if let InterfaceConfig::Udp(udp) = &channel.interface {
        if udp.mode == UdpMode::Multicast
            && udp
                .multicast_group
                .as_deref()
                .is_none_or(|g| g.trim().is_empty())
        {
            errors.push(ChannelConfigError::MissingMulticastGroup);
        }
    }

    // When a recording rotates, the channel name becomes part of generated filenames
    // (§59), so it must be filesystem-safe (§71). Non-rotating recordings use a fixed
    // destination path and do not constrain the name. Either recording can rotate
    // (they are independent now — ADR-013), so check both.
    let raw_rotates = channel.raw_recording.enabled
        && channel.raw_recording.file_rotation != FileRotationPolicy::None;
    let display_rotates = channel.display_recording.enabled
        && channel.display_recording.file_rotation != FileRotationPolicy::None;
    if (raw_rotates || display_rotates) && !is_filesystem_safe(channel.name.as_str()) {
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
}

/// A structural problem in one channel's configuration (§71).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelConfigError {
    #[error("channel kind does not match its interface configuration")]
    KindInterfaceMismatch,
    #[error("TCP Connection channels are runtime-only and cannot be persisted")]
    TcpConnectionNotPersistable,
    #[error("multicast UDP requires a multicast group address")]
    MissingMulticastGroup,
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
                        style: MarkTimestampStyle::Plain,
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
        assert_eq!(timestamp.style, MarkTimestampStyle::Plain);
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
        profile.channels = vec![
            templates::serial_template(),
            templates::udp_template(),
            templates::tcp_listener_template(),
        ];
        for (name, result) in profile.validate() {
            assert!(result.is_ok(), "{name} should be valid: {result:?}");
        }
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
        let toml = "name = \"minimal\"\n";
        let profile = Profile::from_toml(toml).expect("defaults fill in");
        assert_eq!(profile.schema_version, CURRENT_VERSION);
        assert!(profile.channels.is_empty());
        assert_eq!(profile.defaults, DefaultConfig::default());
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
