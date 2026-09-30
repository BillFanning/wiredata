//! Per-Channel activity monitor and liveness (spec §91.1, §166).
//!
//! [`ActivityMeter`] is a small, **bounded** accumulator the pipeline updates per
//! received chunk. It answers the first
//! troubleshooting question — *is data arriving?* — via a rolling throughput and
//! the time of the last data. It is a **fact source, not a policy source**: it
//! reports `last_data_at` and rates; each consumer decides "idle" against its own
//! threshold (a display threshold, or a Match Rule's `Idle` timeout — §50.2), so
//! there is one source of truth and no competing definitions of idle.
//!
//! The rolling rate uses a fixed ring of one-second buckets (`WINDOW_SECS`), so
//! memory is constant regardless of throughput and never affects reception (§100,
//! §124). A quiet stream decays to a zero rate.

use std::time::Instant;

/// The rolling-rate window, in seconds (also the number of ring buckets).
const WINDOW_SECS: u64 = 5;
const BUCKETS: usize = WINDOW_SECS as usize;

/// A point-in-time liveness read-model (§91.1, §166), included in a
/// [`ChannelSnapshot`](super::snapshot::ChannelSnapshot). `last_data_at` is an
/// in-process `Instant`; a consumer computes idle as `now - last_data_at`.
#[derive(Clone, Copy, Debug)]
pub struct ChannelActivity {
    /// When the last chunk/datagram arrived; `None` until the first data.
    pub last_data_at: Option<Instant>,
    pub bytes_per_sec: f64,
    /// Total bytes received since the Channel started (§25) — the byte-based
    /// liveness counter.
    pub total_bytes: u64,
}

/// Internal per-Channel activity accumulator (bounded ring of one-second buckets).
pub struct ActivityMeter {
    epoch: Instant,
    last_data_at: Option<Instant>,
    bytes: [u64; BUCKETS],
    /// The most recent second index that has received data.
    newest_sec: u64,
    /// Running total of all bytes received (monotonic; not windowed).
    total_bytes: u64,
}

impl ActivityMeter {
    pub fn new() -> Self {
        Self::with_epoch(Instant::now())
    }

    /// Construct with an explicit epoch — the time origin for second indexing.
    /// (`pub(crate)` so tests can feed deterministic `epoch + Duration` instants;
    /// `Instant` has no public absolute constructor.)
    pub(crate) fn with_epoch(epoch: Instant) -> Self {
        Self {
            epoch,
            last_data_at: None,
            bytes: [0; BUCKETS],
            newest_sec: 0,
            total_bytes: 0,
        }
    }

    fn sec_of(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.epoch).as_secs()
    }

    /// Advance the ring to `sec`, clearing buckets for any skipped seconds so a
    /// quiet gap doesn't leave stale counts. Monotonic time never moves back.
    fn advance_to(&mut self, sec: u64) {
        if sec <= self.newest_sec {
            return;
        }
        let gap = (sec - self.newest_sec).min(BUCKETS as u64);
        for i in 1..=gap {
            let idx = ((self.newest_sec + i) % BUCKETS as u64) as usize;
            self.bytes[idx] = 0;
        }
        self.newest_sec = sec;
    }

    /// Record `bytes` received at `at` (a pre-extraction chunk/datagram).
    pub fn record_chunk(&mut self, at: Instant, bytes: usize) {
        let sec = self.sec_of(at);
        self.advance_to(sec);
        self.bytes[(sec % BUCKETS as u64) as usize] += bytes as u64;
        self.total_bytes += bytes as u64;
        self.last_data_at = Some(at);
    }

    /// Total bytes received since start (§25) — the stream offset of the next byte.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Most recent transport capture time without computing the rolling rate.
    pub fn last_data_at(&self) -> Option<Instant> {
        self.last_data_at
    }

    /// The read-model as of `now`: rolling rates over the last `WINDOW_SECS`,
    /// decaying to zero when data has stopped.
    pub fn snapshot(&self, now: Instant) -> ChannelActivity {
        let now_sec = self.sec_of(now);
        let mut bytes = 0u64;
        for s in now_sec.saturating_sub(BUCKETS as u64 - 1)..=now_sec {
            // Skip seconds with no data: in the future of recorded data, or older
            // than the ring can represent.
            if s > self.newest_sec || self.newest_sec.saturating_sub(s) >= BUCKETS as u64 {
                continue;
            }
            let idx = (s % BUCKETS as u64) as usize;
            bytes += self.bytes[idx];
        }
        let w = WINDOW_SECS as f64;
        ChannelActivity {
            last_data_at: self.last_data_at,
            bytes_per_sec: bytes as f64 / w,
            total_bytes: self.total_bytes,
        }
    }
}

impl Default for ActivityMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn rates_reflect_a_recent_window_and_decay_to_zero() {
        let base = Instant::now();
        let mut m = ActivityMeter::with_epoch(base);

        // 100 bytes in second 0, then again in second 1.
        m.record_chunk(base + ms(500), 100);
        m.record_chunk(base + ms(1500), 100);

        // At t=2s the 5s window still holds both seconds: 200 bytes.
        let a = m.snapshot(base + Duration::from_secs(2));
        assert_eq!(a.bytes_per_sec, 200.0 / 5.0);
        assert!(a.last_data_at.is_some());
        assert_eq!(a.total_bytes, 200);

        // Long after the data stops, the rate decays to zero — but last_data_at
        // still records when data last arrived (the liveness/idle fact).
        let quiet = m.snapshot(base + Duration::from_secs(60));
        assert_eq!(quiet.bytes_per_sec, 0.0);
        assert!(quiet.last_data_at.is_some());
        assert_eq!(quiet.total_bytes, 200); // monotonic — does not decay with the window
    }

    #[test]
    fn no_data_yet_is_all_zero() {
        let base = Instant::now();
        let m = ActivityMeter::with_epoch(base);
        let a = m.snapshot(base + ms(100));
        assert_eq!(a.bytes_per_sec, 0.0);
        assert!(a.last_data_at.is_none());
    }

    #[test]
    fn old_data_falls_out_of_the_window() {
        let base = Instant::now();
        let mut m = ActivityMeter::with_epoch(base);
        m.record_chunk(base + ms(100), 500); // second 0

        // Within the window: counted.
        let inside = m.snapshot(base + Duration::from_secs(3));
        assert_eq!(inside.bytes_per_sec, 500.0 / 5.0);

        // Past the window: gone (older than WINDOW_SECS behind `now`).
        let outside = m.snapshot(base + Duration::from_secs(6));
        assert_eq!(outside.bytes_per_sec, 0.0);
    }
}
