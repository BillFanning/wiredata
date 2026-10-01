//! Long-running soak scenarios: sustained multi-channel sending over real
//! sockets and a failed-send storm, at durations criterion can't express.
//!
//! All tests are `#[ignore]`d — they are for manual / nightly invocation, not
//! the per-push CI gate:
//!
//! ```text
//! cargo test -p talker --test soak -- --ignored --nocapture
//! ```
//!
//! Duration is `WIREDATA_SOAK_SECS` (default 10 s; a nightly run might use
//! 600+). Assertions are chosen to stay meaningful at any duration: exactness
//! claims are exact (totals at rest), cadence claims are generous fractions
//! (a loaded CI VM legitimately misses sends under the stall policy).

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use talker::core::channel::{InterfaceConfig, TcpClientConfig, UdpConfig};
use talker::core::message::{MessageConfig, PayloadConfig};
use talker::core::runner::ObserverPolicy;
use talker::core::scheduler::Schedule;
use talker::core::supervisor::{CommandOutcome, TalkerSupervisor};

fn soak_secs() -> u64 {
    std::env::var("WIREDATA_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
}

/// A UDP sink on its own thread: counts every received byte until told to
/// stop. A dedicated tight recv loop per channel keeps loopback UDP lossless
/// at these rates (the OS receive buffer never fills).
struct UdpSink {
    bytes: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
    addr: std::net::SocketAddr,
}

impl UdpSink {
    fn spawn() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sink");
        let addr = socket.local_addr().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let bytes = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (b, s) = (Arc::clone(&bytes), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                match socket.recv(&mut buf) {
                    Ok(n) => {
                        b.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    Err(_) => {
                        // Timeout: the only moment we check for shutdown, so a
                        // quiescent line is drained before the thread exits.
                        if s.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                }
            }
        });
        Self {
            bytes,
            stop,
            thread,
            addr,
        }
    }

    /// Wait until the byte count stops growing, then stop and return it.
    fn settle_and_stop(self) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = self.bytes.load(Ordering::Relaxed);
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let now = self.bytes.load(Ordering::Relaxed);
            if now == last || Instant::now() >= deadline {
                break;
            }
            last = now;
        }
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().expect("sink thread");
        self.bytes.load(Ordering::Relaxed)
    }
}

fn msg(hex: &str, interval_ms: u64) -> MessageConfig {
    MessageConfig::new(PayloadConfig::raw_hex(hex), interval_ms)
}

/// Poll the supervisor until `done`, at the GUI-ish cadence, hard-bounded.
fn poll_until(
    sup: &mut TalkerSupervisor,
    timeout: Duration,
    mut done: impl FnMut(&TalkerSupervisor) -> bool,
) {
    let deadline = Instant::now() + timeout;
    while !done(sup) {
        assert!(
            Instant::now() < deadline,
            "condition not reached in {timeout:?}"
        );
        sup.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Four channels, each an aligned two-message schedule at 2 ms (≈1,000
/// sends/s per channel), sustained for the soak window over real loopback
/// UDP. At rest, every channel's telemetry must equal its sink's byte count
/// exactly — the ADR-018 "exact at rest" promise, end to end, at rate, for
/// minutes if asked.
#[test]
#[ignore = "soak: run manually / nightly (see module docs)"]
fn multi_channel_udp_soak_totals_exact_at_rest() {
    const CHANNELS: usize = 4;
    // 32-byte payloads: "AB" * 32 hex chars → 32 bytes per send.
    let payload_hex_a = "AB".repeat(32);
    let payload_hex_b = "CD".repeat(32);

    let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
    let mut sinks = Vec::new();
    for i in 0..CHANNELS {
        let sink = UdpSink::spawn();
        sup.push_slot();
        let messages = vec![msg(&payload_hex_a, 2), msg(&payload_hex_b, 2)];
        let schedule = Schedule::compile(&messages, Instant::now()).unwrap();
        sup.start(
            i,
            (i + 1).to_string(),
            InterfaceConfig::Udp(UdpConfig::unicast(sink.addr)),
            messages,
            schedule,
        );
        sinks.push(sink);
    }

    let secs = soak_secs();
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        sup.poll();
        std::thread::sleep(Duration::from_millis(5));
    }

    sup.stop_all();
    // Drain the stopping runners' tails so the final Counters land.
    poll_until(&mut sup, Duration::from_secs(15), |s| !s.any_draining());

    // Roughly 1,000 sends/s per channel were scheduled; require half even on
    // a loaded machine (missed sends are counted, not silently lost).
    let expected_floor = (secs * 1_000) / 2;
    for (i, sink) in sinks.into_iter().enumerate() {
        let received = sink.settle_and_stop();
        let t = sup.telemetry(i);
        assert_eq!(
            t.total_bytes, received,
            "channel {i}: telemetry bytes must equal the wire exactly at rest \
             (sent {} sends, sink saw {received} bytes)",
            t.total_count
        );
        assert!(
            t.total_count >= expected_floor,
            "channel {i}: cadence collapsed — {} sends in {secs}s (floor {expected_floor}); \
             missed_sends={}",
            t.total_count,
            t.missed_sends
        );
        assert!(
            t.last_error.is_none() && t.command_error.is_none(),
            "channel {i}: unexpected error at rest: {:?}",
            t.banner_error()
        );
    }
}

/// A 2 ms schedule against a peer that accepts one connection and drops it:
/// ~500 due fires/s against a dead interface for the whole soak window. The
/// bounded-backoff retry policy and edge-triggered error reporting must keep
/// the storm silent — one ConnectionError episode, a live runner, and a
/// deliverable Stop at the end (no wedge, no per-fire error flood).
#[test]
#[ignore = "soak: run manually / nightly (see module docs)"]
fn tcp_failed_send_storm_stays_bounded() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    // Accept exactly one connection and drop it immediately, then close the
    // listener too: the talker's established stream dies (RST on the next
    // writes) and nothing can bring it back for the rest of the soak.
    let acceptor = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            drop(stream);
        }
    });

    let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
    sup.push_slot();
    let messages = vec![msg("ABCD", 2)];
    let schedule = Schedule::compile(&messages, Instant::now()).unwrap();
    sup.start(
        0,
        "1",
        InterfaceConfig::TcpClient(TcpClientConfig::new(addr)),
        messages,
        schedule,
    );

    let secs = soak_secs();
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        sup.poll();
        std::thread::sleep(Duration::from_millis(5));
    }

    let t = sup.telemetry(0);
    assert!(
        sup.is_running(0),
        "the runner must survive a permanently failing interface"
    );
    assert!(
        t.last_error.is_some(),
        "the failing episode's first error must be on the banner"
    );
    // Edge-triggered: ONE ConnectionError opens the episode and repeats are
    // counted, not re-reported. Allow a little platform slack (a send can
    // half-succeed into the OS buffer around the RST), but a per-fire storm
    // (~500/s × the window) must be impossible.
    assert!(
        t.errors_total <= 3,
        "expected an edge-triggered error, not a storm: {} errors in {secs}s",
        t.errors_total
    );

    assert_eq!(sup.stop(0), CommandOutcome::Enqueued, "Stop must enqueue");
    poll_until(&mut sup, Duration::from_secs(15), |s| !s.any_draining());
    acceptor.join().expect("acceptor thread");
}
