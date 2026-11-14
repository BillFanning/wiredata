//! Integration tests: profile load/save behavior (spec §70, §159).
//! Black-box — public API only.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use listener::config::{templates, HexGrouping, Profile, RetentionConfig};
use listener::core::ChannelState;
use listener::runtime::Listener;

/// A unique temp path per call. Tests in one binary run concurrently, so the
/// path must not collide — a wall-clock stamp is unsafe (Windows clock resolution
/// is coarse enough for two parallel tests to share a nanosecond). A monotonic
/// counter plus the pid guarantees uniqueness.
fn temp_profile_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("listener-it-profile-{pid}-{n}.toml"))
}

#[test]
fn profile_round_trips_through_a_toml_file() {
    let mut profile = Profile::new("integration workspace");
    profile.channels = vec![templates::udp_template(), templates::serial_template()];

    let path = temp_profile_path();
    profile.save(&path).unwrap();
    let loaded = Profile::load(&path).unwrap();
    assert_eq!(loaded, profile);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn loading_a_profile_does_not_start_channels() {
    // §70: loaded channels restore in Stopped state — load never starts a
    // channel or begins recording. There is no runtime state to restore
    // (message numbers, connections): the schema has no place for it (§159).
    let mut profile = Profile::new("workspace");
    profile.channels = vec![templates::udp_template(), templates::serial_template()];

    let path = temp_profile_path();
    profile.save(&path).unwrap();
    let loaded = Profile::load(&path).unwrap();

    let mut listener = Listener::with_default_capacities();
    for config in loaded.channels {
        let id = listener.add_channel(config);
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn hex_grouping_round_trips_through_a_profile() {
    // §45/§1431: a per-view Hex grouping survives a save/load cycle, so a profile
    // can express it (the gap this closes — the field was documented but absent).
    let mut channel = templates::udp_template();
    channel.display.view.mode = listener::display::DisplayMode::Hex;
    channel.display.view.hex_grouping = HexGrouping {
        bytes_per_group: 4,
        groups_per_line: 4,
    };
    let mut profile = Profile::new("workspace");
    profile.channels = vec![channel];

    let path = temp_profile_path();
    profile.save(&path).unwrap();
    let loaded = Profile::load(&path).unwrap();
    assert_eq!(loaded, profile);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_profile_without_hex_grouping_loads_with_the_default() {
    // The field is `#[serde(default)]`, so a profile written before it existed
    // (its TOML omits `hex_grouping`) must still load, defaulting the field.
    let mut profile = Profile::new("workspace");
    profile.channels = vec![templates::udp_template()];
    let mut toml = profile.to_toml().unwrap();
    // Strip every `hex_grouping` table to simulate a pre-field profile.
    toml = toml
        .lines()
        .filter(|l| {
            !l.contains("hex_grouping")
                && !l.contains("bytes_per_group")
                && !l.contains("groups_per_line")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let loaded = Profile::from_toml(&toml).expect("loads without the field");
    assert_eq!(
        loaded.channels[0].display.view.hex_grouping,
        HexGrouping::default()
    );
}

#[test]
fn an_old_profile_with_a_view_list_loads_warns_and_resaves_in_the_new_form() {
    // Profiles saved when a Channel could carry several views wrote a `views`
    // list. Load keeps the first and says what it dropped; saving writes `view`.
    let old = r#"schema_version = 3
name = "old"

[[channels]]
name = "GPS"
kind = "Udp"

[channels.interface]
type = "Udp"
bind_address = "127.0.0.1"
port = 10110
mode = "Unicast"

[[channels.display.views]]
mode = "Hex"
encoding = "Ascii"
character_rendering = "Token"
wrapping = "NoWrap"

[[channels.display.views]]
mode = "Raw"
encoding = "Utf8"
character_rendering = "Native"
wrapping = "NoWrap"

[channels.retention]
byte_limit = 65536
"#;
    let path = temp_profile_path();
    std::fs::write(&path, old).unwrap();
    let (loaded, warnings) = Profile::load_with_warnings(&path).unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(
        loaded.channels[0].display.view.mode,
        listener::display::DisplayMode::Hex
    );

    loaded.save(&path).unwrap();
    let (reloaded, warnings) = Profile::load_with_warnings(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(reloaded, loaded);
    assert!(!std::fs::read_to_string(&path).unwrap().contains("views"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_profile_that_names_a_tcp_listener_loads_without_it() {
    // TCP Listener Channels are no longer offered. A profile saved with one
    // still loads: the Channel is left out and a warning names it.
    let mut profile = Profile::new("workspace");
    profile.channels = vec![templates::udp_template()];
    let text = format!(
        "{}
[[channels]]
kind = \"TcpListener\"
name = \"old_tcp\"
         [channels.interface]
type = \"TcpListener\"
bind_address = \"0.0.0.0\"
         port = 7000
max_connections = 4
",
        profile.to_toml().unwrap()
    );

    let (loaded, warnings) = Profile::from_toml_with_warnings(&text).unwrap();
    assert_eq!(loaded, profile);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("old_tcp"), "{warnings:?}");
}

#[test]
fn unbounded_retention_is_rejected() {
    // §80: a channel that sets no retention limit is invalid.
    let mut channel = templates::udp_template();
    channel.retention = RetentionConfig::default(); // all None
    let mut profile = Profile::new("workspace");
    profile.channels = vec![channel];

    assert!(profile.validate()[0].1.is_err());
}
