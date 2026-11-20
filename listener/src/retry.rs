//! How Listener paces a retry (§9.1, §55). Reconnecting a Channel and reopening
//! a recording use the same pace, from here, so the two cannot drift apart.
//! Talker paces its open retry the same way with its own constants
//! (`OPEN_RETRY_FIRST` and `OPEN_RETRY_MAX` in its runner).

use std::time::Duration;

/// The first wait after a failure.
pub const RETRY_FIRST: Duration = Duration::from_secs(1);

/// The longest wait. Each failure doubles the wait up to this, and retrying
/// goes on at this wait until it works.
pub const RETRY_MAX: Duration = Duration::from_secs(30);

/// How often the CLI and the GUI look for a reconnect that has fallen due. The
/// runtime has no loop of its own (ADR-006), so each front end drives it.
pub const RECONNECT_CHECK_EVERY: Duration = Duration::from_millis(500);

/// A wait that doubles from `first` to `max` and stays there. Tests shorten it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            first: RETRY_FIRST,
            max: RETRY_MAX,
        }
    }
}

impl Backoff {
    /// The wait that follows `current`: twice as long, but no longer than `max`.
    pub fn after(self, current: Duration) -> Duration {
        current.saturating_mul(2).min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_to_the_longest_and_stays_there() {
        let backoff = Backoff::default();
        let mut wait = backoff.first;
        let mut waits = vec![wait];
        for _ in 0..7 {
            wait = backoff.after(wait);
            waits.push(wait);
        }
        let secs: Vec<u64> = waits.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
