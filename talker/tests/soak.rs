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

use talker::core::capacity::{
    measured_service_estimate, service_sample_count, ChannelDemand, MessageDemand,
    MIN_SERVICE_SAMPLES,
};
use talker::core::channel::{InterfaceConfig, UdpConfig};
use talker::core::message::{MessageConfig, PayloadConfig};
use talker::core::runner::{ObserverPolicy, STOP_LIMIT};
use talker::core::scheduler::Schedule;
use talker::core::supervisor::{CommandCompletion, CommandOutcome, TalkerSupervisor};
use talker::core::telemetry::{recent_snapshot_state, RecentSnapshotState, RECENT_WINDOW};

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

/// Poll channel 0 at the GUI-ish cadence until `until`, requiring on every
/// poll that its retained snapshot's capture instant is still `captured_at`.
fn hold_without_counters(sup: &mut TalkerSupervisor, captured_at: Instant, until: Instant) {
    while Instant::now() < until {
        sup.poll();
        let t = sup.telemetry_ref(0).expect("slot 0");
        assert_eq!(
            t.recent_timing_captured_at,
            Some(captured_at),
            "a dormant runner delivered Counters: the snapshot was recaptured"
        );
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

/// A 2 ms schedule whose every send fails: a unicast channel aimed at the
/// broadcast address, which the system refuses because the socket is not
/// allowed to broadcast. ~500 due fires/s against a dead interface for the
/// whole soak window. The
/// bounded-backoff retry policy and edge-triggered error reporting must keep
/// the storm silent — one SendFailed episode, a live runner, and a
/// deliverable Stop at the end (no wedge, no per-fire error flood).
#[test]
#[ignore = "soak: run manually / nightly (see module docs)"]
fn failed_send_storm_stays_bounded() {
    let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
    sup.push_slot();
    let messages = vec![msg("ABCD", 2)];
    let schedule = Schedule::compile(&messages, Instant::now()).unwrap();
    sup.start(
        0,
        "1",
        InterfaceConfig::Udp(UdpConfig::unicast("255.255.255.255:9".parse().unwrap())),
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
    // Edge-triggered: ONE SendFailed opens the episode and repeats are
    // counted, not re-reported. Allow a little platform slack, but a per-fire
    // storm (~500/s × the window) must be impossible.
    assert!(
        t.errors_total <= 3,
        "expected an edge-triggered error, not a storm: {} errors in {secs}s",
        t.errors_total
    );

    assert_eq!(sup.stop(0), CommandOutcome::Enqueued, "Stop must enqueue");
    poll_until(&mut sup, Duration::from_secs(15), |s| !s.any_draining());
}

/// One channel runs at 5 ms until its recent window is warm, then its only
/// message is made dormant with a live interval change of zero, and the
/// supervisor is polled for a further `RECENT_WINDOW` and a second: the
/// ADR-042 transition from a current snapshot to an expired one, end to end,
/// with no heartbeat in between. The hold is fixed by the window, so
/// `WIREDATA_SOAK_SECS` does not apply.
///
/// What it asserts:
///
/// - Warm: before the interval change, the recent window holds at least
///   `MIN_SERVICE_SAMPLES` render and send samples, read from the supervisor's
///   telemetry, and the retained snapshot classifies as Current.
/// - No further `Counters`: from the poll that reports the change applied until
///   the end of the hold, the retained snapshot's capture instant never moves.
///   The supervisor writes it only from a `Counters` update, and every update
///   carries its own fresh instant, so an update the dormant runner sent, on a
///   timer or otherwise, would change it. The runner emits updates only from
///   its send path and sends each one before it reports a later command as
///   applied, so the first poll that sees the completion has already drained
///   every update that preceded it. The wire corroborates this: the sink
///   receives no byte once a short settle has passed.
/// - Expired: after the hold the same snapshot classifies as Expired, not
///   Current and not Final.
/// - Headroom inputs: the stale recent window would still give a measured
///   estimate, which is the figure expiry must keep out of headroom, and the
///   run-wide timing alone is enough to give one to fall back on.
///
/// What it does not prove: the choice between those two timings and the
/// run-wide fallback label are made in GUI code that an integration test
/// cannot reach, so only that code's own tests cover them. Nor does it say
/// anything about a runner that exits without its final update, or about a
/// slow but not dormant cadence, where each send can still refresh the
/// snapshot.
#[test]
#[ignore = "soak: run manually / nightly (see module docs)"]
fn fast_then_dormant_snapshot_expires_without_further_counters() {
    let sink = UdpSink::spawn();
    let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
    sup.push_slot();
    let messages = vec![msg("ABCD", 5)];
    let schedule = Schedule::compile(&messages, Instant::now()).unwrap();
    sup.start(
        0,
        "1",
        InterfaceConfig::Udp(UdpConfig::unicast(sink.addr)),
        messages,
        schedule,
    );

    // Warm: wait on the telemetry itself, not on a guessed time.
    poll_until(&mut sup, Duration::from_secs(15), |s| {
        let t = s.telemetry_ref(0).expect("slot 0");
        service_sample_count(t.recent_timing) >= MIN_SERVICE_SAMPLES
    });

    // Dormant: the live interval change a user makes in the GUI. Wait for the
    // runner's confirmation, so the next step knows it has stopped sending.
    let submission = sup.set_interval(0, 0, 0);
    assert_eq!(submission.outcome, CommandOutcome::Enqueued);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut applied = false;
    while !applied {
        assert!(
            Instant::now() < deadline,
            "the interval change was not confirmed in 5 s"
        );
        sup.poll();
        for completion in sup.take_command_completions() {
            match completion {
                CommandCompletion::Applied { id, .. } if id == submission.id => applied = true,
                CommandCompletion::Failed { id, message, .. } if id == submission.id => {
                    panic!("the interval change failed: {message}")
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let dormant = sup.telemetry(0);
    let captured_at = dormant
        .recent_timing_captured_at
        .expect("Counters arrived while the window warmed");
    assert!(
        service_sample_count(dormant.recent_timing) >= MIN_SERVICE_SAMPLES,
        "the window was warm when the channel went dormant"
    );
    assert!(
        matches!(
            recent_snapshot_state(
                Some(captured_at),
                Instant::now(),
                dormant.recent_timing_is_final
            ),
            RecentSnapshotState::Current(_)
        ),
        "the snapshot is Current when the channel goes dormant"
    );

    // Let anything already on the loopback reach the sink, then hold. Age runs
    // from the snapshot's own capture instant, so ending the hold a second
    // after that instant plus the window is past the flip however old the
    // snapshot was when the channel went dormant.
    hold_without_counters(
        &mut sup,
        captured_at,
        Instant::now() + Duration::from_millis(100),
    );
    let wire_at_dormancy = sink.bytes.load(Ordering::Relaxed);
    hold_without_counters(
        &mut sup,
        captured_at,
        captured_at + RECENT_WINDOW + Duration::from_secs(1),
    );

    let expired = sup.telemetry(0);
    assert!(
        matches!(
            recent_snapshot_state(
                expired.recent_timing_captured_at,
                Instant::now(),
                expired.recent_timing_is_final
            ),
            RecentSnapshotState::Expired(age) if age >= RECENT_WINDOW
        ),
        "after the hold the retained snapshot must be Expired"
    );
    assert_eq!(
        sink.bytes.load(Ordering::Relaxed),
        wire_at_dormancy,
        "a dormant channel put bytes on the wire"
    );

    // Headroom inputs, from the same retained telemetry the GUI reads.
    let demand = ChannelDemand::from_messages(
        expired
            .per_message_timing
            .iter()
            .map(|m| MessageDemand::new(m.wire_bytes, m.interval_ms())),
    );
    assert!(
        measured_service_estimate(demand, expired.recent_timing).is_some(),
        "the stale recent window still gives an estimate, so only expiry keeps it out"
    );
    assert!(
        service_sample_count(expired.timing) >= MIN_SERVICE_SAMPLES
            && measured_service_estimate(demand, expired.timing).is_some(),
        "run-wide timing must be enough to fall back on"
    );

    sup.stop_all();
    let left_behind = sup.join_all(STOP_LIMIT);
    assert!(
        left_behind.is_empty(),
        "runner did not stop: {left_behind:?}"
    );
    sink.settle_and_stop();
}
