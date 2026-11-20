//! Profile schema, configuration load/save, validation, and templates.
//!
//! This is `listener-config` (§128). It owns the persisted [`Profile`] schema
//! (§67–§80), load/save, validation (§71), and starting templates (§81–§85). It
//! stores configuration only — never runtime objects (§5.7, §69). Mapping a
//! validated config to live transports is the runtime's job (§128),
//! not config's.

mod legacy;
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
    ///
    /// Drops the warnings [`from_toml_with_warnings`](Self::from_toml_with_warnings)
    /// returns: use that where there is somewhere to say them.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Self::from_toml_with_warnings(text).map(|(profile, _)| profile)
    }

    /// [`from_toml`](Self::from_toml), and what loading had to give up.
    ///
    /// A Channel has one Display View, saved as `display.view`. A profile
    /// saved when it could have several, as a `views` list, still loads: the
    /// first view is kept, and each warning names a view or a `PauseDisplay`
    /// target that had to be dropped. A Channel that names a TCP Listener is
    /// left out, with a warning, since this build cannot receive on one.
    /// Saving writes the current form.
    pub fn from_toml_with_warnings(text: &str) -> Result<(Self, Vec<String>), ConfigError> {
        let mut doc: toml::Table = toml::from_str(text)?;
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
        // A TCP Listener Channel cannot be parsed, so it is taken out first.
        let tcp_warnings = legacy::drop_tcp_channels(&mut doc);
        let first = if tcp_warnings.is_empty() {
            toml::from_str::<Profile>(text)
        } else {
            doc.clone().try_into::<Profile>()
        };
        let (profile, mut warnings) = match first {
            Ok(profile) => (profile, Vec::new()),
            Err(error) => {
                // Only the old shape is rewritten. Any other failure is the
                // file's own, reported with the line it is on.
                let upgraded = legacy::upgrade(&mut doc);
                if !upgraded.changed {
                    return Err(error.into());
                }
                let profile: Profile = toml::Value::Table(doc).try_into()?;
                (profile, upgraded.warnings)
            }
        };
        warnings.splice(0..0, tcp_warnings);
        check_version(profile.schema_version)?;
        Ok((profile, warnings))
    }

    /// Serialize this profile to TOML text.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Load a profile from a file (§70). Does not start channels or touch
    /// interfaces — it only reads and validates the schema version.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::load_with_warnings(path).map(|(profile, _)| profile)
    }

    /// [`load`](Self::load), and the warnings of
    /// [`from_toml_with_warnings`](Self::from_toml_with_warnings).
    pub fn load_with_warnings(path: &Path) -> Result<(Self, Vec<String>), ConfigError> {
        Self::from_toml_with_warnings(&std::fs::read_to_string(path)?)
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

    /// This profile's channels as they will run: every profile-level default
    /// written into each channel that leaves that setting unset, so the
    /// channel carries its own explicit value and the `defaults` section is no
    /// longer needed. Order is kept. This is the one place defaults are
    /// resolved; both loaders and [`validate`](Self::validate) use it.
    ///
    /// The inherited settings are:
    /// - `retention`: a channel with no limit set takes the default's whole
    ///   retention. A channel with any limit of its own keeps only its own.
    /// - `display`: a channel whose display is the built-in default takes the
    ///   default's display. A channel cannot ask for the built-in display
    ///   while the profile default says otherwise.
    pub fn into_effective_channels(self) -> Vec<ChannelConfig> {
        let defaults = self.defaults;
        self.channels
            .into_iter()
            .map(|channel| effective_channel(channel, &defaults))
            .collect()
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

/// A channel with each profile default written in where the channel leaves
/// that setting unset. See [`Profile::into_effective_channels`].
fn effective_channel(mut channel: ChannelConfig, defaults: &DefaultConfig) -> ChannelConfig {
    if channel.retention.is_unbounded() {
        if let Some(retention) = &defaults.retention {
            channel.retention = retention.clone();
        }
    }
    if channel.display == DisplayConfig::default() {
        if let Some(display) = &defaults.display {
            channel.display = display.clone();
        }
    }
    channel
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
    // Judge the channel as it will run.
    let channel = &effective_channel(channel.clone(), defaults);

    // The Channel Kind must match its interface.
    let kind_matches = matches!(
        (channel.kind, &channel.interface),
        (ChannelKind::Serial, InterfaceConfig::Serial(_))
            | (ChannelKind::Udp, InterfaceConfig::Udp(_))
    );
    if !kind_matches {
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

    // Retention must be bounded (§80).
    if channel.retention.is_unbounded() {
        errors.push(ChannelConfigError::UnboundedRetention);
    }

    // Limits (ADR-048, §71): the runtime refuses the same values at Start.
    let limits = channel
        .retention
        .limit_errors()
        .into_iter()
        .chain(limits::match_rule_errors(&channel.match_rules))
        .chain(limits::size_cap_error(
            "raw",
            channel.raw_recording.size_cap,
        ))
        .chain(limits::size_cap_error(
            "display",
            channel.display_recording.size_cap,
        ))
        .chain(limits::disk_guard_error(
            "raw",
            channel.raw_recording.disk_guard,
        ))
        .chain(limits::disk_guard_error(
            "display",
            channel.display_recording.disk_guard,
        ));
    errors.extend(limits.map(ChannelConfigError::Limit));

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
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
        "channel name can't be used in a file name, and rotating recordings name their files \
         after it"
    )]
    InvalidChannelName,
    #[error("another channel has this name; names must be unique")]
    DuplicateChannelName,
    #[error("a match rule's byte pattern is empty; it would match at every byte")]
    EmptyMatchPattern,
    #[error("invalid NMEA ZDA Mark talker ID: {0}")]
    InvalidZdaTalkerId(crate::core::ZdaTalkerIdError),
    /// A setting outside its limit (ADR-048, §71).
    #[error(transparent)]
    Limit(LimitError),
}

/// A Channel's problems as one line for a person to read: each message, joined
/// with "; ". The CLI and GUI print it when they skip a Channel (§71).
pub fn describe_channel_errors(errors: &[ChannelConfigError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

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
                    MatchAction::PauseDisplay {},
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
    fn a_display_recording_saved_without_mark_gaps_loads_with_it_on() {
        // A profile from before the option existed gets the gap lines; one that
        // turns them off keeps that choice through a save and a load.
        let old: DisplayRecordingConfig = toml::from_str("enabled = true\n").unwrap();
        assert!(old.mark_gaps);
        assert!(DisplayRecordingConfig::default().mark_gaps);

        let off = DisplayRecordingConfig {
            mark_gaps: false,
            ..DisplayRecordingConfig::default()
        };
        let text = toml::to_string(&off).unwrap();
        assert!(
            !toml::from_str::<DisplayRecordingConfig>(&text)
                .unwrap()
                .mark_gaps
        );
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

    /// A profile as an older build saved it: one UDP Channel with `display`
    /// lines and `rules` appended.
    fn old_profile(display: &str, rules: &str) -> String {
        format!(
            r#"schema_version = 3
name = "old"

[[channels]]
name = "GPS"
kind = "Udp"

[channels.interface]
type = "Udp"
bind_address = "0.0.0.0"
port = 10110
mode = "Unicast"

{display}

[channels.retention]
byte_limit = 65536

{rules}
"#
        )
    }

    const HEX_VIEW: &str = r#"
[[channels.display.views]]
mode = "Hex"
encoding = "Ascii"
character_rendering = "Token"
wrapping = "Wrap"
font_size = 15.0
"#;

    fn hex_view() -> DisplayViewConfig {
        DisplayViewConfig {
            mode: crate::display::DisplayMode::Hex,
            encoding: crate::display::DisplayEncoding::Ascii,
            character_rendering: crate::display::CharacterRendering::Token,
            wrapping: crate::display::WrappingMode::Wrap,
            font_size: Some(15.0),
            ..DisplayViewConfig::default()
        }
    }

    #[test]
    fn an_old_profile_with_one_view_loads_with_the_same_view_and_no_warning() {
        let rules = r#"
[[channels.match_rules]]
name = "freeze"
condition = { kind = "Idle", timeout_ms = 1000 }
actions = [{ kind = "PauseDisplay", view = 0 }, { kind = "Notify", severity = "Event" }]
"#;
        let (profile, warnings) =
            Profile::from_toml_with_warnings(&old_profile(HEX_VIEW, rules)).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        let channel = &profile.channels[0];
        assert_eq!(channel.display.view, hex_view());
        assert_eq!(
            channel.match_rules[0].actions[0],
            MatchAction::PauseDisplay {}
        );
        assert_eq!(channel.match_rules[0].actions.len(), 2);
    }

    #[test]
    fn an_old_profile_with_several_views_keeps_the_first_and_warns() {
        let views = format!(
            "{HEX_VIEW}\n[[channels.display.views]]\nmode = \"Raw\"\nencoding = \"Utf8\"\n\
             character_rendering = \"Native\"\nwrapping = \"NoWrap\"\n\n\
             [[channels.display.views]]\nmode = \"Rendered\"\nencoding = \"Utf8\"\n\
             character_rendering = \"Glyph\"\nwrapping = \"NoWrap\"\n"
        );
        let (profile, warnings) =
            Profile::from_toml_with_warnings(&old_profile(&views, "")).unwrap();
        assert_eq!(profile.channels[0].display.view, hex_view());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("channel \"GPS\""), "{warnings:?}");
        assert!(warnings[0].contains("had 3 display views"), "{warnings:?}");
        assert!(warnings[0].contains("other 2 dropped"), "{warnings:?}");
    }

    #[test]
    fn an_old_profile_with_an_empty_view_list_takes_the_default_view() {
        let (profile, warnings) =
            Profile::from_toml_with_warnings(&old_profile("[channels.display]\nviews = []", ""))
                .unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(profile.channels[0].display, DisplayConfig::default());
        assert_eq!(
            profile.channels[0].display.view,
            DisplayViewConfig::default()
        );
    }

    #[test]
    fn an_old_pause_action_naming_a_later_view_is_removed_with_a_warning() {
        let rules = r#"
[[channels.match_rules]]
name = "freeze"
condition = { kind = "Idle", timeout_ms = 1000 }
actions = [{ kind = "PauseDisplay", view = 1 }, { kind = "Notify", severity = "Event" }]
"#;
        let (profile, warnings) =
            Profile::from_toml_with_warnings(&old_profile(HEX_VIEW, rules)).unwrap();
        assert_eq!(profile.channels[0].match_rules[0].actions.len(), 1);
        assert!(matches!(
            profile.channels[0].match_rules[0].actions[0],
            MatchAction::Notify { .. }
        ));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("rule \"freeze\""), "{warnings:?}");
        assert!(warnings[0].contains("display view 1"), "{warnings:?}");
    }

    #[test]
    fn an_old_profile_saves_in_the_new_form_and_that_loads_to_the_same_profile() {
        let (old, _) = Profile::from_toml_with_warnings(&old_profile(HEX_VIEW, "")).unwrap();
        let saved = old.to_toml().unwrap();
        assert!(saved.contains("[channels.display.view]"), "{saved}");
        assert!(!saved.contains("views"), "{saved}");
        let (reloaded, warnings) = Profile::from_toml_with_warnings(&saved).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(reloaded, old);
    }

    #[test]
    fn the_new_form_round_trips() {
        let mut channel = templates::udp_template();
        channel.display.view = hex_view();
        channel.display.view.hex_grouping = HexGrouping {
            bytes_per_group: 4,
            groups_per_line: 2,
        };
        let mut profile = Profile::new("new");
        profile.channels = vec![channel];
        let (loaded, warnings) =
            Profile::from_toml_with_warnings(&profile.to_toml().unwrap()).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(loaded, profile);
    }

    #[test]
    fn an_old_profile_default_display_is_upgraded_too() {
        let text =
            "schema_version = 3\nname = \"d\"\n\n[[defaults.display.views]]\nmode = \"Hex\"\n\
             encoding = \"Ascii\"\ncharacter_rendering = \"Token\"\nwrapping = \"Wrap\"\n\
             font_size = 15.0\n\n[[defaults.display.views]]\nmode = \"Raw\"\n\
             encoding = \"Utf8\"\ncharacter_rendering = \"Native\"\nwrapping = \"NoWrap\"\n";
        let (profile, warnings) = Profile::from_toml_with_warnings(text).unwrap();
        assert_eq!(profile.defaults.display.unwrap().view, hex_view());
        assert!(
            warnings[0].contains("the profile's defaults"),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_old_profile_with_a_misspelled_key_is_still_refused() {
        let text = old_profile(HEX_VIEW, "").replace("byte_limit", "byte_limt");
        let err = Profile::from_toml(&text).unwrap_err().to_string();
        assert!(err.contains("byte_limt"), "{err}");
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
                    MatchAction::PauseDisplay {},
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

        let mut profile = Profile::new("every type");
        profile.channels = vec![udp, serial];
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
            ("channels.0.reconnect", "ReconnectPolicy"),
            ("channels.0.display", "DisplayConfig"),
            ("channels.0.display.view", "DisplayViewConfig"),
            ("channels.0.display.view.hex_grouping", "HexGrouping"),
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
            matches!(limits[2], LimitError::SizeCap { .. }),
            "{errors:?}"
        );
        assert_eq!(limits.len(), 3, "{errors:?}");
        // The message is the limit's own: it says what to change.
        assert_eq!(
            ChannelConfigError::Limit(limits[0].clone()).to_string(),
            limits[0].to_string()
        );
    }

    #[test]
    fn a_disk_guard_threshold_the_guard_cannot_act_on_skips_the_channel() {
        let with = |min_free| {
            let mut channel = templates::udp_template();
            channel.raw_recording.disk_guard = Some(DiskGuard {
                min_free,
                on_low: LowDiskAction::Warn,
            });
            validate_channel(&channel, &DefaultConfig::default())
        };
        assert_eq!(with(DiskThreshold::Percent { percent: 90 }), Ok(()));
        assert_eq!(with(DiskThreshold::Bytes { bytes: 1 }), Ok(()));
        let errors = with(DiskThreshold::Percent { percent: 91 }).unwrap_err();
        assert_eq!(
            errors,
            vec![ChannelConfigError::Limit(LimitError::DiskGuardPercent {
                recording: "raw",
                found: 91
            })]
        );
        assert!(with(DiskThreshold::Bytes { bytes: 0 }).is_err());
    }

    #[test]
    fn channel_errors_read_as_their_messages() {
        let errors = vec![
            ChannelConfigError::DuplicateChannelName,
            ChannelConfigError::Limit(LimitError::Diagnostics {
                severity: "event",
                found: 2_001,
            }),
        ];
        assert_eq!(
            describe_channel_errors(&errors),
            "another channel has this name; names must be unique; \
             event retention of 2001 is over the limit of 2000"
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
    fn a_channel_of_a_tcp_kind_is_left_out_with_a_warning() {
        let text = "schema_version = 3
name = \"p\"
                    [[channels]]
kind = \"TcpConnection\"
name = \"conn\"
                    [channels.interface]
type = \"Udp\"
bind_address = \"0.0.0.0\"
                    port = 1
mode = \"Unicast\"
";
        let (profile, warnings) = Profile::from_toml_with_warnings(text).unwrap();
        assert!(profile.channels.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("conn"), "{warnings:?}");
    }

    fn profile_with_defaults(channels: Vec<ChannelConfig>) -> Profile {
        let mut profile = Profile::new("defaults");
        profile.channels = channels;
        profile.defaults = DefaultConfig {
            display: Some(DisplayConfig { view: hex_view() }),
            retention: Some(RetentionConfig::with_byte_limit(4_096)),
        };
        profile
    }

    fn unset_channel(name: &str) -> ChannelConfig {
        let mut channel = templates::udp_template();
        channel.name = crate::core::ChannelName::new(name);
        channel.retention = RetentionConfig::default();
        channel.display = DisplayConfig::default();
        channel
    }

    #[test]
    fn effective_channels_carry_the_profile_defaults_they_leave_unset() {
        let channels = profile_with_defaults(vec![unset_channel("A")]).into_effective_channels();
        assert_eq!(channels[0].retention.byte_limit, Some(4_096));
        assert_eq!(channels[0].display.view, hex_view());
    }

    #[test]
    fn effective_channels_keep_the_values_they_set_themselves() {
        let mut own = unset_channel("Own");
        own.retention = RetentionConfig {
            event_limit: Some(7),
            ..RetentionConfig::default()
        };
        let mut view = hex_view();
        view.font_size = Some(30.0);
        own.display = DisplayConfig { view: view.clone() };
        let channels = profile_with_defaults(vec![own]).into_effective_channels();
        assert_eq!(channels[0].retention.byte_limit, None);
        assert_eq!(channels[0].retention.event_limit, Some(7));
        assert_eq!(channels[0].display.view, view);
    }

    #[test]
    fn effective_channels_keep_their_order_and_everything_else() {
        let profile = profile_with_defaults(vec![unset_channel("A"), unset_channel("B")]);
        let before = profile.channels.clone();
        let channels = profile.into_effective_channels();
        assert_eq!(channels.len(), 2);
        for (after, before) in channels.iter().zip(&before) {
            assert_eq!(after.name, before.name);
            assert_eq!(after.interface, before.interface);
            assert_eq!(after.match_rules, before.match_rules);
        }
    }

    #[test]
    fn validation_judges_the_effective_channel() {
        let mut profile = profile_with_defaults(vec![unset_channel("A")]);
        assert!(profile.validate()[0].1.is_ok());
        profile.defaults.retention = None;
        assert!(profile.validate()[0].1.is_err());
    }

    #[test]
    fn effective_settings_survive_a_save_with_empty_defaults() {
        let effective = profile_with_defaults(vec![unset_channel("A")]).into_effective_channels();
        // What a GUI Save writes: the channels as registered, empty defaults.
        let mut saved = Profile::new("saved");
        saved.channels = effective.clone();
        let reloaded = Profile::from_toml(&saved.to_toml().unwrap()).unwrap();
        assert_eq!(reloaded.defaults, DefaultConfig::default());
        assert!(reloaded.validate()[0].1.is_ok());
        assert_eq!(reloaded.into_effective_channels(), effective);
    }
}
