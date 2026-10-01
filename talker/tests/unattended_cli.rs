//! The unattended CLI contract end to end (ADR-060): start what can start, say
//! what is down, and exit with a code that says how the run ended.
//!
//! Unix only: the run is stopped with SIGTERM, as `systemctl stop` does.
#![cfg(unix)]

use std::io::Read;
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

fn profile(channels: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "talker-unattended-{}-{}.toml",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, format!("version = 3\n{channels}")).unwrap();
    path
}

/// A channel that opens and sends to `sink`.
fn sending_to(sink: &UdpSocket) -> String {
    let destination = sink.local_addr().unwrap();
    format!(
        r#"
[[channels]]
name = "Sends"
[channels.interface]
type = "udp"
[channels.interface.mode]
type = "unicast"
destination = "{destination}"
[[channels.messages]]
interval_ms = 100
[channels.messages.payload]
type = "raw_hex"
data = "41"
"#
    )
}

/// A channel whose peer is never there: a TCP client to a port nobody
/// listens on, standing in for a device that is not plugged in.
fn never_opens() -> String {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!(
        r#"
[[channels]]
name = "Missing"
[channels.interface]
type = "tcp_client"
address = "127.0.0.1:{port}"
[[channels.messages]]
interval_ms = 100
[channels.messages.payload]
type = "raw_hex"
data = "41"
"#
    )
}

/// Run the profile for two seconds, stop it with SIGTERM, and return the exit
/// code and what it wrote to stderr.
fn run_then_stop(profile: &Path) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_talker"))
        .arg("--profile-path")
        .arg(profile)
        .arg("--quiet")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("talker starts");
    std::thread::sleep(Duration::from_secs(2));
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "talker did not stop");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let _ = std::fs::remove_file(profile);
    (status.code(), stderr)
}

#[test]
fn a_channel_that_never_opens_degrades_the_run_while_the_others_send() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let path = profile(&format!("{}{}", sending_to(&sink), never_opens()));
    let (code, stderr) = run_then_stop(&path);
    assert!(
        stderr.contains("WARNING: [Missing] did not start"),
        "{stderr}"
    );
    assert!(stderr.contains("[Missing] never started"), "{stderr}");
    assert!(stderr.contains("[Sends] ran throughout; sent"), "{stderr}");
    assert_eq!(code, Some(3), "{stderr}");
}

#[test]
fn a_run_where_every_channel_sends_exits_0() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let path = profile(&sending_to(&sink));
    let (code, stderr) = run_then_stop(&path);
    assert!(stderr.contains("[Sends] ran throughout; sent"), "{stderr}");
    assert_eq!(code, Some(0), "{stderr}");
}
