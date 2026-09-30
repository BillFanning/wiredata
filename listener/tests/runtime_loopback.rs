//! Integration tests: loopback UDP/TCP channels driven through the runtime
//! orchestrator (spec §153–§155). Black-box — public API only. v2.0 is stream-
//! only: there are no Messages, so tests assert on the verbatim stream scrollback
//! and byte-based liveness via on-demand snapshots (ADR-010).

use std::time::Duration;

use listener::config::InterfaceConfig;
use listener::core::{ChannelId, ChannelState, RuntimeEvent};
use listener::runtime::{ChannelSnapshot, Listener};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Receiver;

/// Reserve a loopback port for a channel to bind, without the classic
/// bind-drop-rebind race.
///
/// The naive helper let the OS pick an ephemeral port, read its number, dropped
/// the socket, and returned the bare number — leaving a window in which a
/// sibling test (the OS reuses a just-freed ephemeral port) grabbed the same
/// number and won the rebind, failing the test with `AddrInUse`. Instead we
/// walk a private cursor so concurrent in-process callers never pick the same
/// candidate, seed it from the pid so separate test binaries don't march in
/// lockstep, and verify each candidate is bindable before handing it out. A
/// window against an *external* binder remains (no socket-handoff API), but the
/// in-process collision this suite hit is gone.
///
/// This is a copy of the crate's `test_ports` helper: an integration test is a
/// separate crate and cannot see the library's `#[cfg(test)]` items — and it
/// runs in its own process, so a private cursor here is the right scope.
fn reserve_port(bindable: impl Fn(u16) -> bool) -> u16 {
    use std::sync::atomic::{AtomicU32, Ordering};
    const BASE: u32 = 45_000;
    const SPAN: u32 = 20_000; // 45000..=64999
    static CURSOR: AtomicU32 = AtomicU32::new(0);
    let seed = std::process::id() % SPAN;
    for _ in 0..SPAN {
        let n = CURSOR.fetch_add(1, Ordering::Relaxed);
        let port = (BASE + (seed + n) % SPAN) as u16;
        if bindable(port) {
            return port;
        }
    }
    panic!("no free loopback port found for the test");
}

fn free_udp_port() -> u16 {
    reserve_port(|p| std::net::UdpSocket::bind(("127.0.0.1", p)).is_ok())
}

fn free_tcp_port() -> u16 {
    reserve_port(|p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok())
}

/// Poll a running channel's snapshot until `pred` is satisfied, or time out.
/// Replaces the v1 `MessageReceived`-event wait — liveness is byte-based now.
async fn await_snapshot(
    listener: &Listener,
    id: ChannelId,
    pred: impl Fn(&ChannelSnapshot) -> bool,
) -> ChannelSnapshot {
    let wait = async {
        loop {
            if let Some(s) = listener.snapshot(id).await {
                if pred(&s) {
                    return s;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for the snapshot condition")
}

/// Fetch a running channel's full stream scrollback verbatim via the incremental
/// stream path (§87, ADR-009) — the bytes are no longer bundled into the snapshot.
async fn stream_bytes(listener: &Listener, id: ChannelId) -> Vec<u8> {
    listener
        .stream_delta(id, 0)
        .await
        .map(|d| d.bytes.to_vec())
        .unwrap_or_default()
}

/// Await the next `TcpClientConnected` event, with a timeout.
async fn next_connection(events: &mut Receiver<RuntimeEvent>) -> ChannelId {
    let wait = async {
        loop {
            match events.recv().await.expect("event stream closed") {
                RuntimeEvent::TcpClientConnected(id) => return id,
                _ => continue,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for a connection")
}

async fn stop(listener: &mut Listener, id: ChannelId) {
    tokio::time::timeout(Duration::from_secs(5), listener.stop(id))
        .await
        .expect("stop hung")
        .expect("stop failed");
}

fn talker_udp_sender(port: u16) -> Box<dyn talker::core::channel::Interface> {
    use talker::core::channel::{InterfaceConfig as TalkerInterfaceConfig, UdpConfig};

    TalkerInterfaceConfig::Udp(UdpConfig::unicast(([127, 0, 0, 1], port).into()))
        .open()
        .expect("open Talker UDP loopback sender")
}

/// Await the next `MatchTriggered` event, with a timeout.
async fn next_match(events: &mut Receiver<RuntimeEvent>) -> ChannelId {
    let wait = async {
        loop {
            match events.recv().await.expect("event stream closed") {
                RuntimeEvent::MatchTriggered(id, _rule) => return id,
                _ => continue,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for a match")
}

#[tokio::test]
async fn udp_channel_receives_datagrams_and_stops_cleanly() {
    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();
    assert_eq!(listener.state(id), Some(ChannelState::Running));

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"alpha", ("127.0.0.1", port)).await.unwrap();
    client.send_to(b"bravo", ("127.0.0.1", port)).await.unwrap();

    // The datagrams land in the stream scrollback, concatenated verbatim (§18).
    let snap = await_snapshot(&listener, id, |s| s.stream_end_offset >= 10).await;
    assert_eq!(snap.activity.total_bytes, 10);
    assert_eq!(stream_bytes(&listener, id).await, b"alphabravo");

    stop(&mut listener, id).await;
    assert_eq!(listener.state(id), Some(ChannelState::Stopped));
}

#[tokio::test]
async fn talker_live_nmea_advances_over_udp_into_listener() {
    use talker::core::message::{MessageConfig, PayloadConfig};

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let message = MessageConfig::new(
        PayloadConfig::nmea_live(
            "GP",
            "GGA",
            ",4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,"
                .split(',')
                .map(str::to_string)
                .collect(),
            true,
        ),
        1,
    )
    .compile()
    .unwrap();
    let mut sender = talker_udp_sender(port);
    let first = message.render();
    sender.send(&first).unwrap();

    let second = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let candidate = message.render();
            if candidate != first {
                break candidate;
            }
        }
    })
    .await
    .expect("live NMEA millisecond field did not advance");
    sender.send(&second).unwrap();

    let expected_len = first.len() + second.len();
    let _ = await_snapshot(&listener, id, |s| {
        s.activity.total_bytes >= expected_len as u64
    })
    .await;
    let received = stream_bytes(&listener, id).await;
    assert_eq!(received, [first.as_slice(), second.as_slice()].concat());

    let text = std::str::from_utf8(&received).unwrap();
    let parsed: Vec<_> = text
        .split_terminator("\r\n")
        .map(|wire| nmea0183::NmeaSentence::parse(wire).unwrap())
        .collect();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].sentence_type, nmea0183::SentenceType::GGA);
    assert_eq!(parsed[1].sentence_type, nmea0183::SentenceType::GGA);
    assert_ne!(parsed[0].field(0), parsed[1].field(0));
    assert_eq!(parsed[0].fields[1..], parsed[1].fields[1..]);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn talker_input_gets_zda_in_live_mark_and_disp_but_never_raw() {
    use std::sync::atomic::{AtomicU64, Ordering};

    use listener::config::{
        DisplayRecordingConfig, MarkPosition, MarkTimestamp, MarkTimestampStyle, MatchAction,
        MatchCondition, MatchRule, RawRecordingConfig,
    };
    use listener::core::TimestampConfig;
    use listener::record::{FileRotationPolicy, OverwritePolicy};
    use talker::core::message::{MessageConfig, PayloadConfig};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let raw_path = std::env::temp_dir().join(format!(
        "listener-talker-zda-{}-{n}.raw",
        std::process::id()
    ));
    let disp_path = std::env::temp_dir().join(format!(
        "listener-talker-zda-{}-{n}.disp",
        std::process::id()
    ));

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    config.match_rules = vec![MatchRule {
        name: "ZDA correlate".to_string(),
        condition: MatchCondition::BytePattern {
            pattern: b"$GP".to_vec(),
        },
        actions: vec![MatchAction::Mark {
            timestamp: Some(MarkTimestamp {
                position: MarkPosition::Before,
                style: MarkTimestampStyle::NmeaZda {
                    talker: "RECEIVER_A".to_string(),
                },
                format: TimestampConfig {
                    include_millis: true,
                    ..Default::default()
                },
                separator: "\r\n".to_string(),
            }),
        }],
        enabled: true,
    }];
    config.raw_recording = RawRecordingConfig {
        enabled: true,
        destination: Some(raw_path.clone()),
        timestamp_enabled: false,
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::None,
        disk_guard: None,
        size_cap: None,
    };
    config.display_recording = DisplayRecordingConfig {
        enabled: true,
        destination: Some(disp_path.clone()),
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::None,
        size_cap: None,
    };

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let message = MessageConfig::new(
        PayloadConfig::nmea("GP", "GGA", vec!["123519".to_string()]),
        1,
    )
    .compile()
    .unwrap();
    let wire = message.render();
    talker_udp_sender(port).send(&wire).unwrap();

    let snapshot = await_snapshot(&listener, id, |s| !s.matches.is_empty()).await;
    let mark = snapshot.matches[0]
        .mark
        .as_ref()
        .expect("timestamped Mark contributes a live-view annotation")
        .clone();
    assert!(mark.before);
    assert_eq!(mark.view_offset, Some(0));
    let zda = mark
        .text
        .strip_suffix("\r\n")
        .expect("configured multiline separator is retained");
    let parsed_zda = nmea0183::NmeaSentence::parse(zda).unwrap();
    assert_eq!(parsed_zda.talker_id.to_string(), "RECEIVER_A");
    assert_eq!(parsed_zda.sentence_type, nmea0183::SentenceType::ZDA);

    stop(&mut listener, id).await;

    let raw = std::fs::read(&raw_path).unwrap();
    let disp = std::fs::read_to_string(&disp_path).unwrap();
    assert_eq!(raw, wire);
    assert!(!raw
        .windows(b"RECEIVER_AZDA".len())
        .any(|w| w == b"RECEIVER_AZDA"));
    assert!(disp.contains(&mark.text), "display recording was {disp:?}");
    assert!(
        disp.contains(std::str::from_utf8(&wire).unwrap()),
        "display recording was {disp:?}"
    );

    let _ = std::fs::remove_file(raw_path);
    let _ = std::fs::remove_file(disp_path);
}

#[tokio::test]
async fn udp_channel_match_rule_fires_an_action_and_is_observable() {
    use listener::config::{MatchAction, MatchCondition, MatchRule};
    use listener::diagnostics::DiagnosticSeverity;

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    // A byte-pattern rule that raises a Notify when "ALARM" appears (§50.2, §165).
    config.match_rules = vec![MatchRule {
        name: "alarm".to_string(),
        condition: MatchCondition::BytePattern {
            pattern: b"ALARM".to_vec(),
        },
        actions: vec![MatchAction::Notify {
            severity: DiagnosticSeverity::Warning,
        }],
        enabled: true,
    }];

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // A non-matching datagram: received, but no rule fires.
    client.send_to(b"quiet", ("127.0.0.1", port)).await.unwrap();
    // A matching datagram: the rule fires (push half — MatchTriggered event).
    client
        .send_to(b"ALARM now", ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(next_match(&mut events).await, id);

    // Pull half: the firing is in the snapshot, anchored at the matching chunk's
    // stream byte offset (5, after "quiet"), and Notify left a warning diagnostic.
    let snap = listener
        .snapshot(id)
        .await
        .expect("a running channel snapshot");
    assert_eq!(snap.matches.len(), 1);
    assert_eq!(snap.matches[0].byte_offset, Some(5));
    assert_eq!(snap.diagnostics.warnings.len(), 1);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn tcp_listener_accepts_a_client_and_receives_data() {
    let port = free_tcp_port();
    let mut config = listener::config::templates::tcp_listener_template();
    if let InterfaceConfig::TcpListener(tcp) = &mut config.interface {
        tcp.bind_address = "127.0.0.1".to_string();
        tcp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    // A fresh connection channel is minted, distinct from the listener (§16.4).
    let conn_id = next_connection(&mut events).await;
    assert_ne!(conn_id, id);

    client.write_all(b"sentence\n").await.unwrap();

    // Stopping the listener terminates its connections (§13). (Per-connection
    // snapshots are deferred, so we assert the lifecycle here.)
    stop(&mut listener, id).await;
    assert_eq!(listener.state(id), Some(ChannelState::Stopped));
}

#[tokio::test]
async fn snapshot_exposes_the_verbatim_stream_of_a_running_channel() {
    // Observability surface (§137, ADR-006): a live snapshot reveals the verbatim
    // received bytes — including a bad-checksum NMEA sentence carried as plain bytes.
    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"$GPGLL,4916.45,N,12311.12,W*00", ("127.0.0.1", port))
        .await
        .unwrap();
    // A deliberately wrong checksum is "bad data is still data" — carried verbatim.
    client
        .send_to(b"$GPHDT,274.07,T*FF", ("127.0.0.1", port))
        .await
        .unwrap();

    let expected: &[u8] = b"$GPGLL,4916.45,N,12311.12,W*00$GPHDT,274.07,T*FF";
    let snapshot = await_snapshot(&listener, id, |s| {
        s.stream_end_offset >= expected.len() as u64
    })
    .await;
    assert_eq!(snapshot.channel_id, id);
    assert_eq!(stream_bytes(&listener, id).await, expected);

    // A stopped channel serves one final snapshot — the pipeline's last diagnostics,
    // captured after finalize — so the GUI can show the stop-time notes (the periodic
    // poll never fires during the synchronous stop). It includes a "Channel stopped"
    // INFO.
    stop(&mut listener, id).await;
    let final_snapshot = listener
        .snapshot(id)
        .await
        .expect("a stopped channel serves its final snapshot");
    assert!(
        final_snapshot
            .diagnostics
            .events
            .iter()
            .any(|e| e.message == "Channel stopped"),
        "the final snapshot carries the stop-time diagnostics"
    );
}

#[tokio::test]
async fn rotation_writes_a_named_period_file_through_the_orchestrator() {
    // §163: a rotating Raw recording writes a period file named
    // <channel>_<period>.raw into the destination directory, driven through the
    // orchestrator. Boundary-crossing across periods is unit-tested in
    // record::file_rotation with crafted timestamps; here we prove wiring + naming.
    use listener::config::RawRecordingConfig;
    use listener::record::{FileRotationPolicy, OverwritePolicy};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "listener-rot-it-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    config.name = listener::core::ChannelName::new("gpsfeed");
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    config.raw_recording = RawRecordingConfig {
        enabled: true,
        destination: Some(dir.clone()),
        timestamp_enabled: false,
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::Hourly,
        disk_guard: None,
        size_cap: None,
    };

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"$GPGGA,test", ("127.0.0.1", port))
        .await
        .unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 11).await;

    stop(&mut listener, id).await; // finalizes + flushes the recording

    // Exactly one rotated file, named gpsfeed_<period>.raw, holding the datagram.
    let mut raws: Vec<_> = std::fs::read_dir(&dir)
        .expect("rotation directory exists")
        .filter_map(|e| e.ok().map(|e| e.file_name().into_string().unwrap()))
        .filter(|n| n.starts_with("gpsfeed_") && n.ends_with(".raw"))
        .collect();
    raws.sort();
    assert_eq!(raws.len(), 1, "one period file, got {raws:?}");
    assert_eq!(std::fs::read(dir.join(&raws[0])).unwrap(), b"$GPGGA,test");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn set_recording_toggles_raw_recording_live_through_the_orchestrator() {
    // ADR-012: a running channel records nothing until set_recording(true), then a
    // .raw file captures from that point, and set_recording(false) finalizes it —
    // no restart. The channel has a destination but enabled=false, so it does not
    // auto-record at Start; the arming is present (from the destination) for the live
    // toggle (ADR-013).
    use listener::config::RawRecordingConfig;
    use listener::record::{FileRotationPolicy, OverwritePolicy};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "listener-liverec-it-{}-{}.raw",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    // No destination in the start config — the live toggle supplies the recording
    // settings at call time (ADR-012: read settings when Record is pressed), proving a
    // destination set after start records with no restart.
    let live_raw = RawRecordingConfig {
        enabled: false,
        destination: Some(path.clone()),
        timestamp_enabled: false,
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::None,
        disk_guard: None,
        size_cap: None,
    };

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // Data before enabling recording: received, but not recorded.
    client
        .send_to(b"before", ("127.0.0.1", port))
        .await
        .unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 6).await;
    assert!(listener.snapshot(id).await.unwrap().raw_recording.is_none());

    // Live Begin with the settings supplied now (no restart), then data to capture.
    assert!(listener.set_recording(id, true, live_raw.clone()).await);
    let _ = await_snapshot(&listener, id, |s| s.raw_recording.is_some()).await;
    client
        .send_to(b"DURING", ("127.0.0.1", port))
        .await
        .unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 12).await;

    // Live Stop finalizes; later data is not written.
    assert!(listener.set_recording(id, false, live_raw).await);
    let _ = await_snapshot(&listener, id, |s| s.raw_recording.is_none()).await;
    client.send_to(b"after", ("127.0.0.1", port)).await.unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 17).await;

    stop(&mut listener, id).await;

    // The file holds only the bytes received while recording was on.
    assert_eq!(std::fs::read(&path).unwrap(), b"DURING");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn set_display_recording_toggles_display_recording_live_through_the_orchestrator() {
    // ADR-012 (Display sibling): a running channel writes no .disp until
    // set_display_recording(true), then the rendered view is captured from that
    // point, and set_display_recording(false) finalizes it — no restart. The
    // settings are supplied at call time, proving a destination set after start
    // records live.
    use listener::config::DisplayRecordingConfig;
    use listener::record::{FileRotationPolicy, OverwritePolicy};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "listener-livedisp-it-{}-{}.disp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    let live_display = DisplayRecordingConfig {
        enabled: false,
        destination: Some(path.clone()),
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::None,
        size_cap: None,
    };

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // Data before enabling: received, but not display-recorded.
    client
        .send_to(b"before", ("127.0.0.1", port))
        .await
        .unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 6).await;
    assert!(listener
        .snapshot(id)
        .await
        .unwrap()
        .display_recording
        .is_none());

    // Live Begin with the settings supplied now (no restart), then data to capture.
    assert!(
        listener
            .set_display_recording(id, true, live_display.clone())
            .await
    );
    let _ = await_snapshot(&listener, id, |s| s.display_recording.is_some()).await;
    client
        .send_to(b"DURING", ("127.0.0.1", port))
        .await
        .unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 12).await;

    // Live Stop finalizes; later data is not written.
    assert!(
        listener
            .set_display_recording(id, false, live_display)
            .await
    );
    let _ = await_snapshot(&listener, id, |s| s.display_recording.is_none()).await;
    client.send_to(b"after", ("127.0.0.1", port)).await.unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 17).await;

    stop(&mut listener, id).await;

    // The .disp holds only the span recorded while on (default view renders
    // ASCII verbatim; the recorder writes one rendered chunk per line).
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("DURING"), "{written:?}");
    assert!(
        !written.contains("before") && !written.contains("after"),
        "{written:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn raw_and_display_recording_run_to_independent_destinations() {
    // ADR-013: Raw and Display recording are independently configured and run
    // simultaneously to *different* files — the "Both" case the merged config could
    // not express. Raw captures verbatim bytes (.raw); Display captures the rendered
    // view (.disp). Here the view renders ASCII as-is, so both hold the same text.
    use listener::config::{DisplayRecordingConfig, RawRecordingConfig};
    use listener::record::{FileRotationPolicy, OverwritePolicy};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let raw_path =
        std::env::temp_dir().join(format!("listener-both-raw-{}-{n}.raw", std::process::id()));
    let disp_path = std::env::temp_dir().join(format!(
        "listener-both-disp-{}-{n}.disp",
        std::process::id()
    ));

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    config.raw_recording = RawRecordingConfig {
        enabled: true,
        destination: Some(raw_path.clone()),
        timestamp_enabled: false,
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::None,
        disk_guard: None,
        size_cap: None,
    };
    config.display_recording = DisplayRecordingConfig {
        enabled: true,
        destination: Some(disp_path.clone()),
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::None,
        size_cap: None,
    };

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"HELLO", ("127.0.0.1", port)).await.unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 5).await;

    stop(&mut listener, id).await; // finalizes both recordings

    // Both files exist, at their own destinations, holding the data.
    assert_eq!(std::fs::read(&raw_path).unwrap(), b"HELLO");
    let disp = std::fs::read(&disp_path).unwrap();
    assert!(
        disp.windows(5).any(|w| w == b"HELLO"),
        "display recording holds the rendered text, got {disp:?}"
    );
    let _ = std::fs::remove_file(&raw_path);
    let _ = std::fs::remove_file(&disp_path);
}

#[tokio::test]
async fn snapshot_surfaces_channel_liveness() {
    // §166: a running channel's snapshot reports byte-based liveness — throughput
    // and total bytes registered, and the last-data time set once data arrives.
    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"hello-liveness", ("127.0.0.1", port))
        .await
        .unwrap();

    let snap = await_snapshot(&listener, id, |s| s.activity.total_bytes > 0).await;
    assert!(snap.activity.last_data_at.is_some(), "data has arrived");
    assert!(snap.activity.bytes_per_sec > 0.0, "throughput registered");
    assert_eq!(snap.activity.total_bytes, 14);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn pausing_a_display_view_freezes_only_the_stream_display() {
    // §50/§58: pausing the (default) Display View freezes the stream scrollback;
    // reception, the byte counter, and recording continue.
    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template(); // Raw + Hex → two views
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let views = listener.display_views(id);
    assert_eq!(views.len(), 2);
    // Pausing the default (first) view freezes the shared scrollback (§50.1 removed;
    // the scrollback honors the default view's pause).
    listener.pause_display(id, views[0]).unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"alpha", ("127.0.0.1", port)).await.unwrap();
    client.send_to(b"bravo", ("127.0.0.1", port)).await.unwrap();

    // The byte counter advances even though the scrollback stays frozen/empty.
    let snap = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 10).await;
    assert!(snap.display_views[0].paused);
    assert_eq!(snap.stream_end_offset, 0); // scrollback frozen while paused
    assert!(stream_bytes(&listener, id).await.is_empty());
    assert_eq!(snap.activity.total_bytes, 10);

    // Resuming accumulates only new data — no backfill of what was missed.
    listener.resume_display(id, views[0]).unwrap();
    client
        .send_to(b"charlie", ("127.0.0.1", port))
        .await
        .unwrap();
    let snap = await_snapshot(&listener, id, |s| s.stream_end_offset > 0).await;
    assert_eq!(stream_bytes(&listener, id).await, b"charlie"); // only post-resume data
    assert_eq!(snap.activity.total_bytes, 17);

    stop(&mut listener, id).await;
}
