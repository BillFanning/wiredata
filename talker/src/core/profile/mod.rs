use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::core::channel::ChannelConfig;
use crate::core::logging::LoggingConfig;

/// Current profile schema version.
///
/// Schema 3 renamed the stored CRC-16/KERMIT algorithm (ADR-057). Like the
/// move to 2, it is a clean break: nothing was deployed, so older profiles are
/// refused at load time with a "recreate the profile" message, not migrated
/// (ADR-013).
pub const CURRENT_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Schema version. A profile whose version differs from
    /// [`CURRENT_VERSION`] is rejected at load time.
    #[serde(default = "current_version")]
    pub version: u32,
    /// In-memory display name. **Not serialized** — the file root is
    /// the authoritative name, so the GUI overlays this from
    /// `path.file_stem()` on load and save. Old TOMLs that still
    /// contain `name = "..."` parse cleanly; the field is ignored.
    #[serde(skip)]
    pub name: String,
    /// The channels defined by this profile. Each channel has one interface
    /// and its own list of messages.
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
}

fn current_version() -> u32 {
    CURRENT_VERSION
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            name: String::new(),
            channels: vec![],
            logging: LoggingConfig::default(),
        }
    }
}

impl Profile {
    /// Construct a new profile with the given in-memory display name.
    /// The name is **not** persisted to TOML — see [`Profile::name`].
    /// Useful mostly for tests and quick in-memory construction; the
    /// GUI overrides the name from the file root on load and save.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Load a profile from a TOML file.
    ///
    /// A profile whose version is newer than [`CURRENT_VERSION`] is rejected,
    /// and so is an older one — earlier schemas are not migrated.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading profile {:?}", path))?;

        // Parse to a raw Value first so the version can be inspected before
        // deserializing the typed struct.
        let doc: toml::Value =
            toml::from_str(&content).with_context(|| format!("parsing profile {:?}", path))?;

        let version = extract_version(&doc)?;
        if version > CURRENT_VERSION {
            anyhow::bail!(
                "profile version {version} is newer than this binary supports ({CURRENT_VERSION})"
            );
        }
        if version < CURRENT_VERSION {
            anyhow::bail!(
                "profile schema v{version} is not supported; it predates the current \
                 v{CURRENT_VERSION} schema — recreate the profile"
            );
        }

        let profile: Self = serde::Deserialize::deserialize(doc)
            .with_context(|| format!("deserializing profile {:?}", path))?;

        Ok(profile)
    }

    /// Check that every message in every channel would compile and fits its
    /// channel's interface.
    ///
    /// Cheap preflight (see [`InterfaceConfig::check_message`]): run it after
    /// load to surface all payload errors before any interface is opened or
    /// any thread spawned. Labels are 1-based to match the UI.
    ///
    /// [`InterfaceConfig::check_message`]: crate::core::channel::InterfaceConfig::check_message
    pub fn validate(&self) -> anyhow::Result<()> {
        for (ci, channel) in self.channels.iter().enumerate() {
            for (mi, message) in channel.messages.iter().enumerate() {
                channel
                    .interface
                    .check_message(message)
                    .with_context(|| format!("channel {} message {}", ci + 1, mi + 1))?;
            }
        }
        Ok(())
    }

    /// Serialize this profile to a TOML file, creating parent directories as
    /// needed.
    ///
    /// The write is atomic: content goes to a sibling temp file which is
    /// renamed over the target, so a crash mid-save can never leave a
    /// truncated profile — the previous file survives intact until the
    /// rename replaces it whole. Each save gets its own temp file, so two
    /// saves racing on one path (the GUI and a CLI) never write into each
    /// other's.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {:?}", parent))?;
        }
        let content = toml::to_string(self).context("serializing profile to TOML")?;
        let tmp = write_temp_sibling(path, &content)
            .with_context(|| format!("writing a profile temp file beside {:?}", path))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow::Error::new(e)
                .context(format!("moving saved profile into place at {:?}", path)));
        }
        Ok(())
    }
}

/// Write `content` to a new temp file beside `path` and return its path.
///
/// The process id and a per-process counter make the name unique, and
/// `create_new` refuses a name that is already taken rather than writing into
/// another save's file.
fn write_temp_sibling(path: &Path, content: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    if let Err(e) = file.write_all(content.as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

/// The OS-specific directory where profiles are stored by default.
///
/// Returns `None` if the platform config directory cannot be determined.
pub fn default_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("talker").join("profiles"))
}

fn extract_version(doc: &toml::Value) -> anyhow::Result<u32> {
    match doc.get("version") {
        None => Ok(CURRENT_VERSION),
        Some(toml::Value::Integer(v)) => {
            u32::try_from(*v).context("profile version field is out of range")
        }
        Some(other) => {
            anyhow::bail!("profile version field has wrong type: {other:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use crate::core::channel::{ChannelConfig, InterfaceConfig, TcpClientConfig, UdpConfig};
    use crate::core::message::{MessageConfig, PayloadConfig};
    use crate::core::timing::CadenceAlignment;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("talker_profile_test_{name}.toml"))
    }

    // ── defaults ─────────────────────────────────────────────────────────────

    #[test]
    fn default_profile_has_current_version() {
        assert_eq!(Profile::default().version, CURRENT_VERSION);
    }

    #[test]
    fn default_profile_has_no_channels() {
        assert!(Profile::default().channels.is_empty());
    }

    #[test]
    fn new_sets_name() {
        let p = Profile::new("my-profile");
        assert_eq!(p.name, "my-profile");
        assert_eq!(p.version, CURRENT_VERSION);
    }

    // ── validate ──────────────────────────────────────────────────────────────

    #[test]
    fn validate_passes_a_good_profile_and_labels_a_bad_message() {
        let iface = InterfaceConfig::TcpClient(TcpClientConfig::new(
            "127.0.0.1:4000".parse::<SocketAddr>().unwrap(),
        ));
        let mut p = Profile::new("v");
        p.channels = vec![
            ChannelConfig::new(
                iface.clone(),
                vec![MessageConfig::new(PayloadConfig::raw_hex("AABB"), 100)],
            ),
            ChannelConfig::new(
                iface,
                vec![MessageConfig::new(PayloadConfig::raw_hex("XYZ"), 100)],
            ),
        ];
        let err = p.validate().unwrap_err();
        // 1-based labels pointing at the offending message.
        assert!(
            format!("{err:#}").contains("channel 2 message 1"),
            "{err:#}"
        );
        p.channels.pop();
        assert!(p.validate().is_ok());
    }

    // ── save / load round-trip ────────────────────────────────────────────────

    #[test]
    fn round_trip_empty_profile() {
        let path = temp_path("empty");
        let original = Profile::new("empty");
        original.save(&path).unwrap();
        let loaded = Profile::load(&path).unwrap();
        // `name` isn't serialized — GUI overlays it from the file stem.
        // At the library level, a fresh load always returns name = "".
        assert_eq!(loaded.name, "");
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert!(loaded.channels.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trip_with_channels() {
        let path = temp_path("full");
        let addr: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let mut profile = Profile::new("full");
        let mut precise = ChannelConfig::new(
            InterfaceConfig::TcpClient(TcpClientConfig::new(addr)),
            vec![MessageConfig::new(PayloadConfig::raw_hex("AABB"), 500)],
        );
        precise.cadence_alignment = CadenceAlignment::UtcPhase;
        profile.channels.push(precise);
        profile.channels.push(ChannelConfig::new(
            InterfaceConfig::Udp(UdpConfig::unicast(addr)),
            vec![MessageConfig::new(
                PayloadConfig::nmea("GP", "GGA", vec![]),
                1000,
            )],
        ));

        profile.save(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            saved.matches("cadence_alignment = \"utc_phase\"").count(),
            1
        );
        let loaded = Profile::load(&path).unwrap();

        // `name` deliberately isn't round-tripped (see comment above).
        assert_eq!(loaded.channels.len(), 2);
        assert_eq!(loaded.channels[0].messages.len(), 1);
        assert_eq!(loaded.channels[1].messages.len(), 1);
        assert_eq!(
            loaded.channels[0].cadence_alignment,
            CadenceAlignment::UtcPhase
        );
        assert_eq!(
            loaded.channels[1].cadence_alignment,
            CadenceAlignment::Immediate
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn name_is_not_written_to_toml() {
        let path = temp_path("named");
        Profile::new("should-not-appear").save(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("name"),
            "saved TOML still contains a `name` field: {content}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Temp files a save left beside `path`.
    fn temp_siblings(path: &Path) -> Vec<PathBuf> {
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

    #[test]
    fn save_replaces_existing_file_and_leaves_no_temp() {
        let path = temp_path("atomic");
        std::fs::write(&path, "version = 2\n# old content\n").unwrap();
        Profile::new("atomic").save(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("old content"));
        assert_eq!(temp_siblings(&path), Vec::<PathBuf>::new());
        let _ = std::fs::remove_file(&path);
    }

    /// Saves racing on one path — say the GUI and a CLI saving the same
    /// profile — each succeed, and the file ends up holding one whole profile.
    #[test]
    fn concurrent_saves_to_one_path_each_land_whole() {
        let path = temp_path(&format!("concurrent_{}", std::process::id()));
        let writers: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // Different lengths, so a torn write would not parse.
                    let iface = InterfaceConfig::TcpClient(TcpClientConfig::new(
                        "127.0.0.1:4000".parse::<SocketAddr>().unwrap(),
                    ));
                    let channel = ChannelConfig::new(
                        iface,
                        vec![MessageConfig::new(PayloadConfig::raw_hex("AABB"), 100)],
                    );
                    let mut profile = Profile::new("racing");
                    profile.channels = vec![channel; n];
                    for _ in 0..25 {
                        profile.save(&path).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        Profile::load(&path).unwrap();
        assert_eq!(temp_siblings(&path), Vec::<PathBuf>::new());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_creates_parent_directories() {
        let path = std::env::temp_dir()
            .join("talker_profile_test_subdir")
            .join("nested")
            .join("profile.toml");
        Profile::new("nested").save(&path).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    // ── version checks ────────────────────────────────────────────────────────

    #[test]
    fn load_rejects_future_version() {
        let path = temp_path("future");
        let content = format!("version = {}\nname = \"future\"\n", CURRENT_VERSION + 1);
        std::fs::write(&path, content).unwrap();
        let err = Profile::load(&path).unwrap_err();
        assert!(err.to_string().contains("newer than this binary supports"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_v1_profile() {
        let path = temp_path("v1");
        std::fs::write(&path, "version = 1\nname = \"old\"\n").unwrap();
        let err = Profile::load(&path).unwrap_err();
        assert!(err.to_string().contains("not supported"));
        let _ = std::fs::remove_file(&path);
    }

    /// Schema 3 renamed a stored checksum name (ADR-057), so a version-2 file is
    /// refused rather than read with a name that no longer exists.
    #[test]
    fn load_rejects_v2_profile() {
        let path = temp_path("v2");
        std::fs::write(&path, "version = 2\n").unwrap();
        let err = Profile::load(&path).unwrap_err();
        assert!(err.to_string().contains("recreate the profile"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_version_defaults_to_current() {
        let path = temp_path("noversion");
        // Old-style file with a `name` key — silently ignored by the
        // skipped `Profile::name`, so this still parses cleanly.
        std::fs::write(&path, "name = \"no-version\"\n").unwrap();
        let loaded = Profile::load(&path).unwrap();
        assert_eq!(loaded.version, CURRENT_VERSION);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let err = Profile::load(Path::new("/no/such/profile.toml")).unwrap_err();
        assert!(err.to_string().contains("reading profile"));
    }

    #[test]
    fn load_invalid_toml_returns_error() {
        let path = temp_path("bad_toml");
        std::fs::write(&path, "this is not toml ][").unwrap();
        let err = Profile::load(&path).unwrap_err();
        assert!(err.to_string().contains("parsing profile"));
        let _ = std::fs::remove_file(&path);
    }

    /// The sample profile shipped with the crate must stay valid.
    #[test]
    fn sample_profile_toml_loads() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/profiles/profile.example.toml"
        ));
        let profile = Profile::load(path).expect("profile.example.toml should load");
        assert_eq!(profile.version, CURRENT_VERSION);
        assert!(!profile.channels.is_empty());

        // The spec points readers here, so a misspelled key must not pass for a
        // default: everything the file says has to survive a round trip through
        // the profile types.
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
        let read_back = toml::Value::try_from(&profile).unwrap();
        assert!(
            contained(&written, &read_back),
            "profile.example.toml holds a key or value the profile types drop"
        );
    }

    // ── default_dir ───────────────────────────────────────────────────────────

    #[test]
    fn default_dir_ends_with_talker_profiles() {
        if let Some(dir) = default_dir() {
            assert!(dir.ends_with("talker/profiles") || dir.ends_with("talker\\profiles"));
        }
        // On platforms where config_dir() is None this test is a no-op.
    }
}
