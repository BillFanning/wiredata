//! Long-running soak scenario, in process (listener ADR-051 has the harness
//! for runs across processes):
//! sustained reception with Raw recording to a real file, at durations the
//! unit tests can't express. `#[ignore]`d — manual / nightly, not the
//! per-push CI gate:
//!
//! ```text
//! cargo test -p listener --test soak -- --ignored --nocapture
//! ```
//!
//! Duration is `WIREDATA_SOAK_SECS` (default 10 s).

use std::time::{Duration, Instant};

use listener::config::{InterfaceConfig, RawRecordingConfig};
use listener::record::{FileRotationPolicy, OverwritePolicy};
use listener::runtime::Listener;

fn soak_secs() -> u64 {
    std::env::var("WIREDATA_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
}

/// ~1,000 64-byte datagrams/s into a UDP channel recording Raw to disk for
/// the whole soak window. At rest, three counts must agree exactly: bytes
/// sent, bytes the channel's retained activity reports, and bytes in the
/// finalized `.raw` file (recording is verbatim; timestamps off). Also pins
/// that the bounded recording queue never approached overflow — the disk
/// kept up at rate for the whole window.
#[tokio::test]
#[ignore = "soak: run manually / nightly (see module docs)"]
async fn sustained_recording_records_every_byte() {
    let port = {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let mut dir = std::env::temp_dir();
    dir.push(format!("listener-soak-{}.raw", uuid::Uuid::new_v4()));
    let destination = dir.clone();

    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    config.raw_recording = RawRecordingConfig {
        enabled: true,
        destination: Some(destination.clone()),
        timestamp_enabled: false,
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::None,
        disk_guard: None,
        size_cap: None,
    };

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    // Sender: 64-byte datagrams on a 1 ms tick for the soak window.
    let payload = [0x5Au8; 64];
    let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut sent_bytes: u64 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(1));
    let deadline = Instant::now() + Duration::from_secs(soak_secs());
    let mut queue_peak_seen = 0usize;
    let mut queue_capacity = 0usize;
    while Instant::now() < deadline {
        tick.tick().await;
        sender.send_to(&payload, ("127.0.0.1", port)).await.unwrap();
        sent_bytes += payload.len() as u64;
        // Sample the recording-queue high-water mark as we go (the stats lane
        // an overview polls); every ~256 sends is plenty.
        if sent_bytes.is_multiple_of(256 * 64) {
            if let Some(stats) = listener.channel_stats(id).await {
                if let Some(q) = stats.raw_recording_queue {
                    queue_peak_seen = queue_peak_seen.max(q.peak);
                    queue_capacity = q.capacity;
                }
            }
        }
    }

    // Let the last datagrams land, then stop (drains the accepted backlog and
    // finalizes the recording, §110).
    let arrival_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let received = listener
            .channel_stats(id)
            .await
            .map(|s| s.activity.total_bytes)
            .unwrap_or(0);
        if received == sent_bytes || Instant::now() >= arrival_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::timeout(Duration::from_secs(15), listener.stop(id))
        .await
        .expect("stop hung")
        .expect("stop failed");

    // Exact at rest, three ways: sent == retained activity == the .raw file.
    let stats = listener.channel_stats(id).await.expect("retained stats");
    assert_eq!(
        stats.activity.total_bytes, sent_bytes,
        "retained activity must equal the bytes sent (UDP loopback at this \
         rate is lossless; a mismatch means the pipeline dropped)"
    );
    let recorded = tokio::fs::read(&destination).await.expect("read .raw");
    assert_eq!(
        recorded.len() as u64,
        sent_bytes,
        "the finalized .raw must hold every received byte, verbatim"
    );
    assert!(
        recorded.iter().all(|&b| b == 0x5A),
        "recorded bytes must be the payload, unmangled"
    );
    // The disk kept up: the bounded recording queue never came near the cap
    // (a peak at capacity precedes a QueueOverflow recording fault).
    if queue_capacity > 0 {
        assert!(
            queue_peak_seen < queue_capacity / 2,
            "recording queue peaked at {queue_peak_seen}/{queue_capacity} — \
             the disk is barely keeping up at this rate"
        );
    }
    assert_eq!(
        stats.error_count, 0,
        "no error diagnostics after a clean soak"
    );

    let _ = tokio::fs::remove_file(&destination).await;
    let mut sidecar = destination.clone();
    sidecar.set_extension("idx");
    let _ = tokio::fs::remove_file(&sidecar).await;
}
