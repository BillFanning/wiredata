//! Shared bounded telemetry primitives for Talker and Listener.
//!
//! The histograms retain fixed-size bucket counts rather than individual
//! samples. Recording and recent-window aging therefore remain allocation-free
//! and bounded independently of run length and sample rate.

mod rate;

pub use rate::{RollingRate, RATE_MIN_SPAN, RATE_WINDOW};

use std::time::{Duration, Instant};

const BUCKET_UPPER_US: [u64; 15] = [
    50, 100, 250, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000, 256_000,
    512_000, 1_000_000,
];
const BUCKET_COUNT: usize = BUCKET_UPPER_US.len() + 1;
const RECENT_SEGMENTS: usize = 10;
const RECENT_SEGMENT: Duration = Duration::from_secs(1);

/// The approximate time covered by [`RecentDurationHistogram`].
pub const RECENT_WINDOW: Duration = Duration::from_secs(RECENT_SEGMENTS as u64);

/// A fixed-size cumulative histogram of non-negative durations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DurationHistogram {
    samples: u64,
    buckets: [u64; BUCKET_COUNT],
    total_nanos: u128,
    max_nanos: u64,
}

impl DurationHistogram {
    /// Record one duration without allocation.
    pub fn record(&mut self, duration: Duration) {
        let full_nanos = duration.as_nanos();
        let nanos = full_nanos.min(u64::MAX as u128) as u64;
        let bucket =
            BUCKET_UPPER_US.partition_point(|upper| full_nanos > u128::from(*upper) * 1_000);

        self.samples = self.samples.saturating_add(1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.total_nanos = self.total_nanos.saturating_add(full_nanos);
        self.max_nanos = self.max_nanos.max(nanos);
    }

    /// Number of recorded durations.
    pub fn sample_count(&self) -> u64 {
        self.samples
    }

    /// Arithmetic mean of all recorded durations.
    pub fn mean(&self) -> Option<Duration> {
        (self.samples > 0).then(|| {
            Duration::from_nanos(
                (self.total_nanos / self.samples as u128).min(u64::MAX as u128) as u64,
            )
        })
    }

    /// Largest recorded duration.
    pub fn max(&self) -> Option<Duration> {
        (self.samples > 0).then(|| Duration::from_nanos(self.max_nanos))
    }

    /// Return the upper bound of the bucket containing `percentile`.
    ///
    /// The overflow bucket has no fixed upper edge, so its observed maximum
    /// is returned. Callers should present this as an estimate (for example,
    /// `p99 <= 4 ms`) rather than an exact percentile value.
    pub fn percentile_upper_bound(&self, percentile: u8) -> Option<Duration> {
        if self.samples == 0 {
            return None;
        }

        let percentile = u128::from(percentile.clamp(1, 100));
        let rank = (u128::from(self.samples) * percentile).div_ceil(100);
        let mut cumulative = 0u128;
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative += u128::from(*count);
            if cumulative >= rank {
                return Some(if index < BUCKET_UPPER_US.len() {
                    Duration::from_micros(BUCKET_UPPER_US[index])
                } else {
                    Duration::from_nanos(self.max_nanos)
                });
            }
        }

        // Counts only diverge after u64 saturation. The maximum remains the
        // most useful bounded answer in that unreachable-in-practice case.
        Some(Duration::from_nanos(self.max_nanos))
    }

    fn merge(&mut self, other: &Self) {
        self.samples = self.samples.saturating_add(other.samples);
        for (count, other_count) in self.buckets.iter_mut().zip(other.buckets) {
            *count = count.saturating_add(other_count);
        }
        self.total_nanos = self.total_nanos.saturating_add(other.total_nanos);
        self.max_nanos = self.max_nanos.max(other.max_nanos);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct TimedHistogram {
    started_at: Option<Instant>,
    histogram: DurationHistogram,
}

/// A fixed-memory approximation of the last [`RECENT_WINDOW`] of samples.
#[derive(Debug, Default)]
pub struct RecentDurationHistogram {
    segments: [TimedHistogram; RECENT_SEGMENTS],
    current: usize,
    current_started: Option<Instant>,
}

impl RecentDurationHistogram {
    /// Record a duration in the segment containing monotonic instant `now`.
    pub fn record_at(&mut self, now: Instant, duration: Duration) {
        self.advance(now);
        self.segments[self.current].histogram.record(duration);
    }

    /// Merge segments that are younger than [`RECENT_WINDOW`] at `now`.
    pub fn snapshot_at(&self, now: Instant) -> DurationHistogram {
        let mut snapshot = DurationHistogram::default();
        for segment in &self.segments {
            if segment
                .started_at
                .is_some_and(|start| now.saturating_duration_since(start) < RECENT_WINDOW)
            {
                snapshot.merge(&segment.histogram);
            }
        }
        snapshot
    }

    fn advance(&mut self, now: Instant) {
        let Some(started) = self.current_started else {
            self.current_started = Some(now);
            self.segments[self.current].started_at = Some(now);
            return;
        };
        let elapsed = now.saturating_duration_since(started);
        let steps = elapsed.as_nanos() / RECENT_SEGMENT.as_nanos();
        if steps == 0 {
            return;
        }

        let remainder = elapsed.as_nanos() % RECENT_SEGMENT.as_nanos();
        let aligned_start = now
            .checked_sub(Duration::from_nanos(remainder as u64))
            .unwrap_or(now);
        if steps >= RECENT_SEGMENTS as u128 {
            self.segments.fill(TimedHistogram::default());
            self.current = 0;
        } else {
            for _ in 0..steps as usize {
                self.current = (self.current + 1) % RECENT_SEGMENTS;
                self.segments[self.current] = TimedHistogram::default();
            }
        }
        self.current_started = Some(aligned_start);
        self.segments[self.current].started_at = Some(aligned_start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_has_no_summary() {
        let histogram = DurationHistogram::default();
        assert_eq!(histogram.sample_count(), 0);
        assert_eq!(histogram.mean(), None);
        assert_eq!(histogram.max(), None);
        assert_eq!(histogram.percentile_upper_bound(99), None);
    }

    #[test]
    fn records_mean_max_and_bucket_upper_bounds() {
        let mut histogram = DurationHistogram::default();
        histogram.record(Duration::from_micros(1));
        histogram.record(Duration::from_micros(100));
        histogram.record(Duration::from_micros(900));
        histogram.record(Duration::from_secs(2));

        assert_eq!(histogram.sample_count(), 4);
        assert_eq!(histogram.mean(), Some(Duration::from_nanos(500_250_250)));
        assert_eq!(histogram.max(), Some(Duration::from_secs(2)));
        assert_eq!(
            histogram.percentile_upper_bound(50),
            Some(Duration::from_micros(100))
        );
        assert_eq!(
            histogram.percentile_upper_bound(75),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            histogram.percentile_upper_bound(99),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn exact_bucket_edges_stay_in_the_lower_bucket() {
        let mut histogram = DurationHistogram::default();
        histogram.record(Duration::from_micros(50));
        histogram.record(Duration::from_nanos(50_001));
        histogram.record(Duration::from_secs(1));
        histogram.record(Duration::from_nanos(1_000_000_001));

        assert_eq!(
            histogram.percentile_upper_bound(25),
            Some(Duration::from_micros(50))
        );
        assert_eq!(
            histogram.percentile_upper_bound(50),
            Some(Duration::from_micros(100))
        );
        assert_eq!(
            histogram.percentile_upper_bound(75),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            histogram.percentile_upper_bound(100),
            Some(Duration::from_nanos(1_000_000_001))
        );
    }

    #[test]
    fn percentile_input_is_clamped_to_a_valid_rank() {
        let mut histogram = DurationHistogram::default();
        histogram.record(Duration::from_micros(50));
        histogram.record(Duration::from_micros(500));

        assert_eq!(
            histogram.percentile_upper_bound(0),
            Some(Duration::from_micros(50))
        );
        assert_eq!(
            histogram.percentile_upper_bound(255),
            Some(Duration::from_micros(500))
        );
    }

    #[test]
    fn summary_durations_saturate_at_the_duration_nanos_boundary() {
        let mut histogram = DurationHistogram::default();
        histogram.record(Duration::from_secs(u64::MAX));

        assert_eq!(histogram.sample_count(), 1);
        assert_eq!(histogram.mean(), Some(Duration::from_nanos(u64::MAX)));
        assert_eq!(histogram.max(), Some(Duration::from_nanos(u64::MAX)));
    }

    #[test]
    fn recent_window_ages_out_whole_fixed_segments() {
        let t0 = Instant::now();
        let mut recent = RecentDurationHistogram::default();
        recent.record_at(t0, Duration::from_micros(100));
        recent.record_at(t0 + Duration::from_secs(9), Duration::from_micros(900));

        assert_eq!(
            recent
                .snapshot_at(t0 + Duration::from_secs(9))
                .sample_count(),
            2
        );
        let later = recent.snapshot_at(t0 + Duration::from_secs(10));
        assert_eq!(later.sample_count(), 1, "the oldest segment aged out");
        assert_eq!(later.max(), Some(Duration::from_micros(900)));
        assert_eq!(
            recent
                .snapshot_at(t0 + Duration::from_secs(20))
                .sample_count(),
            0,
            "an idle window contains no stale samples"
        );
    }

    #[test]
    fn partial_segment_rollover_stays_aligned() {
        let t0 = Instant::now();
        let mut recent = RecentDurationHistogram::default();
        recent.record_at(t0, Duration::from_micros(100));
        recent.record_at(
            t0 + Duration::from_millis(1_500),
            Duration::from_micros(900),
        );

        let before_cutoff = recent.snapshot_at(t0 + Duration::from_millis(10_999));
        assert_eq!(before_cutoff.sample_count(), 1);
        assert_eq!(before_cutoff.max(), Some(Duration::from_micros(900)));
        assert_eq!(
            recent
                .snapshot_at(t0 + Duration::from_secs(11))
                .sample_count(),
            0
        );
    }

    #[test]
    fn a_gap_larger_than_the_window_reuses_bounded_storage() {
        let t0 = Instant::now();
        let mut recent = RecentDurationHistogram::default();
        recent.record_at(t0, Duration::from_millis(1));
        recent.record_at(t0 + Duration::from_secs(30), Duration::from_millis(2));

        let snapshot = recent.snapshot_at(t0 + Duration::from_secs(30));
        assert_eq!(snapshot.sample_count(), 1);
        assert_eq!(snapshot.max(), Some(Duration::from_millis(2)));
    }
}
