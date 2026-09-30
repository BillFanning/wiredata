//! Scheduler hot-path benchmarks (workspace benchmark harness — TODO,
//! external review 2026-07-11). The runner's loop calls these once per pass,
//! so their cost scales directly with send rate (ADR-017 moved the practical
//! ceiling to ~500 Hz–1 kHz; these baselines say what the loop can afford):
//!
//! - `poll` when nothing is due — the linear next-fire scan (spec §8.1's
//!   "conceptually a priority queue"; adopt a real heap only if this shows
//!   message-count scans matter);
//! - `poll` returning a due message followed by `render` — includes the
//!   per-send wire-bytes clone the "observer-path allocations" TODO targets
//!   (`render_into` candidate);
//! - `active_cadence` — re-scanned every loop pass for the ADR-017
//!   high-resolution timer gate (caching candidate).
//!
//! Run with `cargo bench -p talker`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use talker::core::message::{
    ChecksumAlgorithm, ChecksumConfig, MessageConfig, PayloadConfig, TimestampConfig,
};
use talker::core::scheduler::{Schedule, Tick};

fn msg(byte_len: usize, interval_ms: u64) -> MessageConfig {
    MessageConfig::new(PayloadConfig::raw_hex("AB".repeat(byte_len)), interval_ms)
}

/// Drain every immediately-due fire so subsequent polls at `now` hit the
/// nothing-due scan path.
fn drain_due(schedule: &mut Schedule, now: Instant) {
    while matches!(schedule.poll(now), Tick::Due { .. }) {}
}

fn poll_due_and_render(schedule: &mut Schedule, now: Instant) -> Vec<u8> {
    let Tick::Due { index, .. } = schedule.poll(now) else {
        panic!("benchmark cadence must produce a due message");
    };
    schedule
        .render(index)
        .expect("due index belongs to schedule")
}

fn bench_poll_scan(c: &mut Criterion) {
    for n in [8usize, 64, 512] {
        let start = Instant::now();
        let messages: Vec<MessageConfig> = (0..n).map(|_| msg(32, 1_000)).collect();
        let mut schedule = Schedule::compile(&messages, start).unwrap();
        drain_due(&mut schedule, start);
        c.bench_function(&format!("schedule/poll-idle-scan/{n}-messages"), |b| {
            b.iter(|| black_box(schedule.poll(black_box(start))))
        });
    }
}

fn bench_poll_due_send(c: &mut Criterion) {
    for (label, bytes) in [("64B", 64usize), ("1KiB", 1024)] {
        let start = Instant::now();
        let mut schedule = Schedule::compile(&[msg(bytes, 1)], start).unwrap();
        let mut now = start;
        c.bench_function(&format!("schedule/due-and-render/{label}"), |b| {
            b.iter(|| {
                // March exactly one interval per iteration so every poll is a
                // due fire: measures the scan + the per-send payload clone.
                now += Duration::from_millis(1);
                black_box(poll_due_and_render(&mut schedule, now))
            })
        });
    }
}

/// The dynamic-render counterpart of `due-and-render`: the same 64-byte payload
/// with a full timestamp (date+millis+timezone, three chrono format calls into
/// a temporary `String`) prepended and a CRC-16/KERMIT appended per send. The
/// static case's `render_into` KILL verdict covered only the plain payload
/// clone; this is the case that says whether that verdict generalizes to the
/// per-send rendering path (the "observer-path allocations" TODO).
// The config structs are `#[non_exhaustive]`, so a bench (outside the crate)
// cannot use struct literals — Default + field assignment is the only way in.
#[allow(clippy::field_reassign_with_default)]
fn bench_poll_due_send_rendered(c: &mut Criterion) {
    let start = Instant::now();
    let mut message = msg(64, 1);
    let mut ts = TimestampConfig::default();
    ts.include_date = true;
    ts.include_millis = true;
    ts.include_timezone = true;
    message.timestamp = Some(ts);
    let mut cs = ChecksumConfig::default();
    cs.algorithm = ChecksumAlgorithm::Crc16Kermit;
    message.checksum = Some(cs);
    let mut schedule = Schedule::compile(&[message], start).unwrap();
    let mut now = start;
    c.bench_function("schedule/due-and-render/64B-timestamp-crc16", |b| {
        b.iter(|| {
            now += Duration::from_millis(1);
            black_box(poll_due_and_render(&mut schedule, now))
        })
    });

    // Paired equal-shape sentences isolate the live substitution cost: same
    // identity, fields, checksum mode, and wire length; only live rendering differs.
    let fields = "010203.004,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,"
        .split(',')
        .map(str::to_string)
        .collect::<Vec<_>>();
    let message = MessageConfig::new(PayloadConfig::nmea("GP", "GGA", fields.clone()), 1);
    let mut schedule = Schedule::compile(&[message], start).unwrap();
    let mut now = start;
    c.bench_function("schedule/due-and-render/static-nmea-gga", |b| {
        b.iter(|| {
            now += Duration::from_millis(1);
            black_box(poll_due_and_render(&mut schedule, now))
        })
    });

    let message = MessageConfig::new(PayloadConfig::nmea_live("GP", "GGA", fields, true), 1);
    let mut schedule = Schedule::compile(&[message], start).unwrap();
    let mut now = start;
    c.bench_function("schedule/due-and-render/live-nmea-gga", |b| {
        b.iter(|| {
            now += Duration::from_millis(1);
            black_box(poll_due_and_render(&mut schedule, now))
        })
    });
}

/// The runner re-checks this on every loop pass to reconcile timer policy, so
/// it is on the hot path at whatever the shortest interval is.
fn bench_active_cadence(c: &mut Criterion) {
    let start = Instant::now();
    let messages: Vec<MessageConfig> = (0..512).map(|_| msg(32, 1_000)).collect();
    let schedule = Schedule::compile(&messages, start).unwrap();
    c.bench_function("schedule/active-cadence/512-messages", |b| {
        b.iter(|| black_box(schedule.active_cadence()))
    });
}

criterion_group!(
    benches,
    bench_poll_scan,
    bench_poll_due_send,
    bench_poll_due_send_rendered,
    bench_active_cadence
);
criterion_main!(benches);
