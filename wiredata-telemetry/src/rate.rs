//! A bounded rolling-rate meter over one-second buckets.

use std::time::{Duration, Instant};

const BUCKETS: usize = 5;

/// The longest span a [`RollingRate`] averages over.
pub const RATE_WINDOW: Duration = Duration::from_secs(BUCKETS as u64);

/// The shortest span a [`RollingRate`] divides by. A rate over less than a
/// second is a guess: one chunk a few milliseconds after the epoch would read
/// as thousands of times the true rate. Until a full second has passed the
/// meter reads as though one second had.
pub const RATE_MIN_SPAN: Duration = Duration::from_secs(1);

/// Events or bytes per second, averaged over the last [`RATE_WINDOW`].
///
/// Five one-second buckets are indexed from an `epoch`. The rate sums the
/// current, still-filling second with the four whole seconds before it and
/// divides by the time those buckets actually cover: four seconds plus the
/// elapsed part of the current one. A steady stream therefore reads the same
/// at every point in the second. During the first [`RATE_WINDOW`] after the
/// epoch the buckets cover only the time since the epoch, so that is the
/// divisor (never less than [`RATE_MIN_SPAN`]). Memory is constant, and data
/// older than the window contributes nothing, so a quiet stream reads zero.
///
/// Time is always passed in. Instants must not run backwards; one that does is
/// treated as the newest second already seen.
#[derive(Clone, Copy, Debug)]
pub struct RollingRate {
    epoch: Instant,
    buckets: [u64; BUCKETS],
    /// The newest second index that has been advanced to.
    newest_sec: u64,
}

impl RollingRate {
    /// A meter whose second 0 begins at `epoch`.
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            buckets: [0; BUCKETS],
            newest_sec: 0,
        }
    }

    /// Forget everything and start a new window at `epoch`.
    pub fn reset(&mut self, epoch: Instant) {
        *self = Self::new(epoch);
    }

    fn second_of(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.epoch).as_secs()
    }

    /// Advance the ring to `sec`, clearing the buckets of skipped seconds so a
    /// quiet gap leaves no stale counts.
    fn advance_to(&mut self, sec: u64) {
        if sec <= self.newest_sec {
            return;
        }
        let gap = (sec - self.newest_sec).min(BUCKETS as u64);
        for offset in 1..=gap {
            let index = ((self.newest_sec + offset) % BUCKETS as u64) as usize;
            self.buckets[index] = 0;
        }
        self.newest_sec = sec;
    }

    /// Add `amount` (bytes, messages) that arrived at `at`.
    pub fn record(&mut self, at: Instant, amount: u64) {
        let sec = self.second_of(at).max(self.newest_sec);
        self.advance_to(sec);
        let index = (sec % BUCKETS as u64) as usize;
        self.buckets[index] = self.buckets[index].saturating_add(amount);
    }

    /// The rate per second as of `now`.
    pub fn rate_at(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.epoch);
        let now_sec = elapsed.as_secs();
        let mut sum = 0u64;
        for sec in now_sec.saturating_sub(BUCKETS as u64 - 1)..=now_sec {
            // Skip seconds after the newest recorded one, and seconds the ring
            // no longer represents.
            if sec > self.newest_sec || self.newest_sec - sec >= BUCKETS as u64 {
                continue;
            }
            sum = sum.saturating_add(self.buckets[(sec % BUCKETS as u64) as usize]);
        }
        let whole_before = Duration::from_secs(BUCKETS as u64 - 1);
        let covered = elapsed
            .min(whole_before + Duration::from_nanos(u64::from(elapsed.subsec_nanos())))
            .max(RATE_MIN_SPAN);
        sum as f64 / covered.as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// 1,000 B/s as ten bytes every 10 ms, the chunk at the middle of each
    /// tick, recorded in time order up to (not including) `until_ms`.
    fn feed_steady(meter: &mut RollingRate, t0: Instant, from_ms: u64, until_ms: u64) {
        for tick in from_ms / 10..until_ms / 10 {
            meter.record(t0 + ms(tick * 10 + 5), 10);
        }
    }

    #[test]
    fn a_steady_stream_reads_correctly_at_any_point_in_the_second() {
        let t0 = Instant::now();
        let mut meter = RollingRate::new(t0);
        for tick in 0..1_200u64 {
            let now = tick * 10;
            if now >= 5_000 {
                let rate = meter.rate_at(t0 + ms(now));
                assert!(
                    (rate - 1_000.0).abs() <= 10.0,
                    "at {now} ms the rate read {rate}"
                );
            }
            feed_steady(&mut meter, t0, now, now + 10);
        }
    }

    #[test]
    fn the_old_divide_by_five_sawtooth_is_gone() {
        let t0 = Instant::now();
        let mut meter = RollingRate::new(t0);
        feed_steady(&mut meter, t0, 0, 9_100);
        // Early in a second the current bucket is nearly empty; dividing by 5
        // would read about 800. Dividing by the covered 4.1 s reads 1,000.
        let early = meter.rate_at(t0 + ms(9_100));
        assert!((early - 1_000.0).abs() < 10.0, "read {early}");
    }

    #[test]
    fn the_rate_falls_to_zero_once_traffic_stops() {
        let t0 = Instant::now();
        let mut meter = RollingRate::new(t0);
        feed_steady(&mut meter, t0, 0, 3_000);
        assert!(meter.rate_at(t0 + ms(3_000)) > 0.0);
        // The last bucket is second 2; it leaves the window at 7 s.
        assert!(meter.rate_at(t0 + ms(6_900)) > 0.0);
        assert_eq!(meter.rate_at(t0 + ms(7_000)), 0.0);
        assert_eq!(meter.rate_at(t0 + Duration::from_secs(60)), 0.0);
    }

    #[test]
    fn start_up_divides_by_the_time_since_the_epoch() {
        let t0 = Instant::now();
        let mut meter = RollingRate::new(t0);
        feed_steady(&mut meter, t0, 0, 2_000);
        // At 2 s, 2,000 bytes over 2 s: correct at once, not 400.
        assert_eq!(meter.rate_at(t0 + ms(2_000)), 1_000.0);
        // At 3 s, 3,000 bytes over 3 s.
        feed_steady(&mut meter, t0, 2_000, 3_000);
        assert_eq!(meter.rate_at(t0 + ms(3_000)), 1_000.0);
    }

    #[test]
    fn the_first_second_is_not_divided_by_less_than_a_second() {
        let t0 = Instant::now();
        let mut meter = RollingRate::new(t0);
        meter.record(t0 + ms(5), 64);
        assert_eq!(meter.rate_at(t0 + ms(10)), 64.0);
        assert_eq!(meter.rate_at(t0), 64.0);
    }

    #[test]
    fn an_empty_meter_reads_zero_and_reset_forgets() {
        let t0 = Instant::now();
        let mut meter = RollingRate::new(t0);
        assert_eq!(meter.rate_at(t0 + ms(100)), 0.0);
        meter.record(t0 + ms(100), 500);
        assert!(meter.rate_at(t0 + ms(200)) > 0.0);
        meter.reset(t0 + Duration::from_secs(1));
        assert_eq!(meter.rate_at(t0 + Duration::from_secs(2)), 0.0);
    }

    #[test]
    fn a_gap_longer_than_the_window_leaves_no_stale_counts() {
        let t0 = Instant::now();
        let mut meter = RollingRate::new(t0);
        meter.record(t0 + ms(100), 1_000_000);
        meter.record(t0 + Duration::from_secs(30), 10);
        assert_eq!(meter.rate_at(t0 + Duration::from_secs(30)), 10.0 / 4.0);
    }

    #[test]
    fn an_instant_that_runs_backwards_counts_as_the_newest_second() {
        let t0 = Instant::now();
        let mut meter = RollingRate::new(t0);
        meter.record(t0 + Duration::from_secs(8), 10);
        meter.record(t0 + Duration::from_secs(2), 10);
        assert_eq!(meter.rate_at(t0 + Duration::from_secs(8)), 20.0 / 4.0);
    }
}
