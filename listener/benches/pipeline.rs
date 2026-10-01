//! Pipeline hot-path benchmarks, from the external review of 2026-07-11; what
//! they measured and decided is listener ADR-053. The scenarios:
//!
//! - per-chunk ingest cost at the typical 64-byte serial read size (the floor:
//!   activity meter + scrollback append, no rules, no recorders);
//! - the same ingest in steady state at the scrollback byte cap, where every
//!   chunk front-evicts (`VecDeque` drain churn — the "(behind benchmarks)"
//!   eviction item);
//! - match-rule scan scaling (1/8/32 `BytePattern` rules that never match —
//!   pure scan cost, the Aho–Corasick candidate's baseline);
//! - dense-match firing (rules that fire on EVERY chunk — the firing cost the
//!   never-match cases exclude, gating the Aho–Corasick parking verdict);
//! - the selected-channel observability poll at the diagnostics cap
//!   (`snapshot()` and the GUI's per-arrival clone + chronological sort).
//!
//! Run with `cargo bench -p listener`. These are baselines for the perf items
//! filed in the TODOs — measure here before optimizing there.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use listener::config::{MatchAction, MatchCondition, MatchRule};
use listener::core::{ChannelId, ChunkTime};
use listener::diagnostics::{Diagnostic, DiagnosticSeverity};
use listener::runtime::{ChannelPipeline, PipelineCapacities};
use listener::transport::{ReceivedData, ReceivedPayload, TransportNotice};
use std::hint::black_box;
use std::sync::Arc;

/// A 64-byte NMEA-shaped line: the common serial read-chunk size (§147).
fn payload_64b() -> Vec<u8> {
    let mut line = b"$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M".to_vec();
    line.extend_from_slice(b",*\r\n");
    assert_eq!(line.len(), 64);
    line
}

fn chunk(cid: ChannelId, bytes: &[u8]) -> ReceivedData {
    ReceivedData {
        channel_id: cid,
        payload: ReceivedPayload::Bytes(bytes.to_vec()),
        received_at: ChunkTime::now(),
    }
}

/// `n` enabled `BytePattern` rules that can never match the bench payload —
/// the per-chunk scan cost with zero firing overhead.
fn no_match_rules(n: usize) -> Vec<MatchRule> {
    (0..n)
        .map(|i| MatchRule {
            name: format!("bench-rule-{i}"),
            condition: MatchCondition::BytePattern {
                pattern: format!("$GPZZZ,{i},NOMATCH*").into_bytes(),
            },
            actions: Vec::new(),
            enabled: true,
        })
        .collect()
}

fn bench_ingest(c: &mut Criterion) {
    let payload = payload_64b();
    let cid = ChannelId::new();

    // TRUE below-cap floor: a fresh, lightly warmed pipeline per iteration
    // (setup is untimed), so no sample ever measures the capped state. The
    // first version reused one pipeline across the whole run — it filled to
    // the cap after ~2k iterations, so "floor" and "at-cap" measured the
    // same thing and the eviction-cost comparison was self-to-self.
    c.bench_function("ingest/64B/below-cap", |b| {
        b.iter_batched(
            || {
                let mut p = ChannelPipeline::new(cid, PipelineCapacities::default());
                for _ in 0..16 {
                    p.ingest(chunk(cid, &payload));
                }
                (p, chunk(cid, &payload))
            },
            |(mut p, data)| p.ingest(data),
            BatchSize::LargeInput,
        )
    });

    // Steady state at the byte cap: pre-fill past `stream_display` so every
    // chunk evicts its own length from the front.
    let mut at_cap = ChannelPipeline::new(cid, PipelineCapacities::default());
    let cap_bytes = PipelineCapacities::default().stream_display;
    for _ in 0..(cap_bytes / payload.len() + 16) {
        at_cap.ingest(chunk(cid, &payload));
    }
    c.bench_function("ingest/64B/at-scrollback-cap", |b| {
        b.iter_batched(
            || chunk(cid, &payload),
            |data| at_cap.ingest(data),
            BatchSize::SmallInput,
        )
    });
}

fn bench_match_rule_scaling(c: &mut Criterion) {
    let payload = payload_64b();
    let cid = ChannelId::new();
    for n in [1usize, 8, 32] {
        let mut pipeline = ChannelPipeline::new(cid, PipelineCapacities::default())
            .with_match_rules(&no_match_rules(n));
        c.bench_function(&format!("ingest/64B/{n}-byte-pattern-rules"), |b| {
            b.iter_batched(
                || chunk(cid, &payload),
                |data| pipeline.ingest(data),
                BatchSize::SmallInput,
            )
        });
    }
}

/// `n` enabled rules that fire on **every** chunk (`$GPGGA` opens each bench
/// payload) — measures what the never-match scaling cases exclude: per-firing
/// bookkeeping (the `TriggeredMatch` record, the bounded recent-matches ring)
/// and, when `actions` is non-empty, action application.
fn firing_rules(n: usize, actions: Vec<MatchAction>) -> Vec<MatchRule> {
    (0..n)
        .map(|i| MatchRule {
            name: format!("bench-firing-rule-{i}"),
            condition: MatchCondition::BytePattern {
                pattern: b"$GPGGA".to_vec(),
            },
            actions: actions.clone(),
            enabled: true,
        })
        .collect()
}

fn bench_dense_match_firing(c: &mut Criterion) {
    let payload = payload_64b();
    let cid = ChannelId::new();

    // Bare firings: rule bookkeeping only, no actions.
    for n in [1usize, 8] {
        let mut pipeline = ChannelPipeline::new(cid, PipelineCapacities::default())
            .with_match_rules(&firing_rules(n, Vec::new()));
        c.bench_function(&format!("ingest/64B/{n}-rules-firing-every-chunk"), |b| {
            b.iter_batched(
                || chunk(cid, &payload),
                |data| pipeline.ingest(data),
                BatchSize::SmallInput,
            )
        });
    }

    // The action path: one Notify per firing records a diagnostic each chunk
    // (the heaviest always-synchronous action — Record/Mark involve recorders
    // and view splices that need a fuller harness).
    let mut pipeline =
        ChannelPipeline::new(cid, PipelineCapacities::default()).with_match_rules(&firing_rules(
            1,
            vec![MatchAction::Notify {
                severity: DiagnosticSeverity::Event,
            }],
        ));
    c.bench_function("ingest/64B/1-rule-notify-every-chunk", |b| {
        b.iter_batched(
            || chunk(cid, &payload),
            |data| pipeline.ingest(data),
            BatchSize::SmallInput,
        )
    });
}

/// The selected-channel poll at the retained-diagnostics cap: the 5 Hz
/// snapshot clones every retained diagnostic, and the GUI then clones + sorts
/// them into one chronological timeline per arrival (`into_sorted_vec`,
/// cached per snapshot). The filed estimate says ~1 ms/s at 5 Hz — this is
/// the "verify, don't guess" measurement gating any sequence-number/delta
/// work.
fn bench_snapshot_at_diagnostics_cap(c: &mut Criterion) {
    let cid = ChannelId::new();
    // Seed well past the per-severity caps so the log sits exactly at its
    // bounded maximum, with realistic mixed severities and message sizes.
    let seed: Vec<Diagnostic> = (0..4096)
        .map(|i| {
            let msg = format!("bench diagnostic {i}: recording queue depth 42/256 on channel x");
            match i % 3 {
                0 => Diagnostic::event(msg),
                1 => Diagnostic::warning(msg),
                _ => Diagnostic::error(msg),
            }
        })
        .collect();
    let mut pipeline =
        ChannelPipeline::new(cid, PipelineCapacities::default()).with_prior_diagnostics(seed);

    // The steady state: the log is unchanged between polls, so the shared
    // snapshot is reused rather than rebuilt from every retained entry.
    c.bench_function("snapshot/full-at-diagnostics-cap", |b| {
        b.iter(|| black_box(pipeline.snapshot()))
    });

    // The same poll when a diagnostic has just landed, which is what actually
    // pays for the rebuild. A stall notice is the public path that records one.
    c.bench_function("snapshot/full-at-diagnostics-cap-after-change", |b| {
        b.iter(|| {
            pipeline.record_notice(TransportNotice::ReceptionStalled {
                channel_id: cid,
                stalled_for: std::time::Duration::from_millis(250),
            });
            black_box(pipeline.snapshot())
        })
    });

    // The GUI's per-arrival step, isolated: flatten+sort the snapshot's
    // diagnostics chronologically (gui::state fold).
    let snap = pipeline.snapshot();
    c.bench_function("snapshot/sorted-timeline-at-cap", |b| {
        b.iter_batched(
            || Arc::clone(&snap.diagnostics),
            |d| black_box(d.to_sorted_vec()),
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(
    benches,
    bench_ingest,
    bench_match_rule_scaling,
    bench_dense_match_firing,
    bench_snapshot_at_diagnostics_cap
);
criterion_main!(benches);
