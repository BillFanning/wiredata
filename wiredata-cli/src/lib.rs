//! What both command-line applications tell an operator during an unattended
//! run, and the exit code it ends with (talker ADR-060, listener ADR-046 and
//! ADR-050).
//!
//! - A WARNING when a channel does not start or goes down, naming it and why.
//! - A reminder every five minutes while any channel stays down, with how long
//!   it has been down and the latest reason.
//! - A line when a channel recovers.
//! - A summary at the end naming every channel's outcome, which also decides
//!   whether the run was degraded (exit code 3).
//!
//! Pure bookkeeping, generic over each application's channel id: the caller
//! observes its own runtime, prints the lines, and adds to the summary what
//! only it knows.

use std::time::{Duration, Instant};

/// Exit code: an invalid profile, nothing could start, or `--require-all`
/// failed.
pub const EXIT_CANNOT_START: u8 = 2;
/// Exit code: a channel never started, or it was down at the stop.
pub const EXIT_DEGRADED: u8 = 3;
/// Exit code: the stop did not finish within its time limit, so the end of
/// what the run wrote may be missing.
pub const EXIT_STOP_INCOMPLETE: u8 = 4;

/// The exit code for how a run ended. 0 is healthy, or every outage
/// recovered; 1, an internal error, is the application's to report. An
/// incomplete stop wins over degraded: it is the one that says data written
/// at the end may be missing.
pub fn exit_status(stop_incomplete: bool, degraded: bool) -> u8 {
    if stop_incomplete {
        EXIT_STOP_INCOMPLETE
    } else if degraded {
        EXIT_DEGRADED
    } else {
        0
    }
}

/// How often a reminder repeats while any channel is down.
pub const REMINDER_EVERY: Duration = Duration::from_secs(5 * 60);

/// One channel's health over the run.
#[derive(Debug)]
struct Watched<K> {
    id: K,
    name: String,
    /// Whether it is retried while down.
    retries: bool,
    /// Whether it has run at all this session.
    ever_up: bool,
    /// When the current outage began; `None` while it runs.
    down_since: Option<Instant>,
    /// The latest reason it is down.
    reason: String,
    /// Outages it recovered from, and their total length.
    recovered: u32,
    recovered_for: Duration,
}

/// The health of every channel in the run, in the order they were added.
#[derive(Debug)]
pub struct HealthWatch<K> {
    channels: Vec<Watched<K>>,
    /// When the last reminder went out; reminders start five minutes after
    /// the first outage.
    reminded_at: Option<Instant>,
    /// What happens to a channel that is down and not retried, in the
    /// application's words.
    not_retrying: &'static str,
}

impl<K: Copy + PartialEq> HealthWatch<K> {
    /// A watch whose warnings end with `not_retrying` for a channel that is
    /// not retried while down. One that is retried ends with the words given
    /// to [`down`](Self::down).
    pub fn new(not_retrying: &'static str) -> Self {
        Self {
            channels: Vec::new(),
            reminded_at: None,
            not_retrying,
        }
    }

    /// Watch a channel. `retries` is whether it is retried while down.
    pub fn add(&mut self, id: K, name: &str, retries: bool) {
        self.channels.push(Watched {
            id,
            name: name.to_owned(),
            retries,
            ever_up: false,
            down_since: None,
            reason: String::new(),
            recovered: 0,
            recovered_for: Duration::ZERO,
        });
    }

    fn get(&mut self, id: K) -> Option<&mut Watched<K>> {
        self.channels.iter_mut().find(|c| c.id == id)
    }

    /// The channel is running. Returns the recovery line when it was down.
    pub fn up(&mut self, id: K, now: Instant) -> Option<String> {
        let channel = self.get(id)?;
        let first_start = !channel.ever_up;
        channel.ever_up = true;
        let since = channel.down_since.take()?;
        let down_for = now.saturating_duration_since(since);
        channel.recovered += 1;
        channel.recovered_for += down_for;
        let name = &channel.name;
        Some(if first_start {
            format!("RECOVERED: [{name}] started after {}", describe(down_for))
        } else {
            format!(
                "RECOVERED: [{name}] is running again after {} down",
                describe(down_for)
            )
        })
    }

    /// The channel did not start, or went down. Returns the warning for a new
    /// outage; a channel already down only takes the newer reason.
    ///
    /// `retrying` says how this outage is being retried, such as
    /// [`doubling_retry`]'s words. It is given per outage because an
    /// application can retry a failed start and a failure while running on
    /// different schedules. A channel that is not retried does not use it.
    pub fn down(&mut self, id: K, reason: &str, retrying: &str, now: Instant) -> Option<String> {
        let not_retrying = self.not_retrying;
        let channel = self.get(id)?;
        channel.reason = reason.to_owned();
        if channel.down_since.is_some() {
            return None;
        }
        channel.down_since = Some(now);
        let what = if channel.ever_up {
            "is down"
        } else {
            "did not start"
        };
        let next = if channel.retries {
            retrying
        } else {
            not_retrying
        };
        Some(format!(
            "WARNING: [{}] {what}: {reason} — {next}",
            channel.name
        ))
    }

    /// Whether a reminder is due: some channel is down, and five minutes have
    /// passed since the last reminder or, before the first, since the earliest
    /// outage began.
    pub fn reminder_due(&self, now: Instant) -> bool {
        let earliest = self.channels.iter().filter_map(|c| c.down_since).min();
        let Some(earliest) = earliest else {
            return false;
        };
        let from = self.reminded_at.map_or(earliest, |at| at.max(earliest));
        now.saturating_duration_since(from) >= REMINDER_EVERY
    }

    /// One reminder line per channel that is down, when one is due.
    pub fn reminders(&mut self, now: Instant) -> Vec<String> {
        if !self.reminder_due(now) {
            return Vec::new();
        }
        self.reminded_at = Some(now);
        self.channels
            .iter()
            .filter_map(|c| {
                let since = c.down_since?;
                let down_for = describe(now.saturating_duration_since(since));
                let what = if c.ever_up {
                    format!("has been down for {down_for}")
                } else {
                    format!("has not started after {down_for}")
                };
                Some(format!("WARNING: [{}] {what}: {}", c.name, c.reason))
            })
            .collect()
    }

    /// Whether nothing is running and nothing will retry — the run cannot do
    /// anything (exit code 2).
    pub fn nothing_can_run(&self) -> bool {
        self.channels
            .iter()
            .all(|c| c.down_since.is_some() && !c.retries)
    }

    /// Whether the run is degraded (exit code 3): a channel never started, or
    /// it is down now.
    pub fn degraded(&self) -> bool {
        self.channels
            .iter()
            .any(|c| !c.ever_up || c.down_since.is_some())
    }

    /// The final summary, one line per channel in the order added, with its id
    /// so the application can add what only it knows.
    pub fn summary(&self, now: Instant) -> Vec<(K, String)> {
        self.channels
            .iter()
            .map(|c| {
                let line = match (c.down_since, c.ever_up) {
                    (Some(_), false) => format!("[{}] never started: {}", c.name, c.reason),
                    (Some(since), true) => format!(
                        "[{}] down at the stop, for {}: {}",
                        c.name,
                        describe(now.saturating_duration_since(since)),
                        c.reason
                    ),
                    (None, _) if c.recovered == 0 => format!("[{}] ran throughout", c.name),
                    (None, _) => format!(
                        "[{}] recovered from {} outage{}, {} down in all",
                        c.name,
                        c.recovered,
                        if c.recovered == 1 { "" } else { "s" },
                        describe(c.recovered_for)
                    ),
                };
                (c.id, line)
            })
            .collect()
    }
}

/// How a channel is retried on a delay that doubles from `first` to `max`, in
/// words for [`HealthWatch::down`]: "retrying in 1 s, doubling to 30 s".
pub fn doubling_retry(first: Duration, max: Duration) -> String {
    format!(
        "retrying in {}, doubling to {}",
        describe(first),
        describe(max)
    )
}

/// A length of time in the largest units an operator reads at a glance:
/// milliseconds under a second, then whole seconds, minutes, hours. It is how
/// every line here states an outage, and an application's own lines about one
/// can state it the same way.
pub fn describe(span: Duration) -> String {
    if span < Duration::from_secs(1) {
        return format!("{} ms", span.as_millis());
    }
    let secs = span.as_secs();
    match (secs / 3600, (secs % 3600) / 60) {
        (0, 0) => format!("{secs} s"),
        (0, minutes) => format!("{minutes} min"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn watch_one(retries: bool) -> (HealthWatch<u32>, u32, Instant) {
        let mut watch = HealthWatch::new("not retrying, so it stays down");
        watch.add(1, "GPS", retries);
        (watch, 1, Instant::now())
    }

    fn lines(summary: Vec<(u32, String)>) -> Vec<String> {
        summary.into_iter().map(|(_, line)| line).collect()
    }

    #[test]
    fn the_exit_code_says_how_the_run_ended() {
        assert_eq!(exit_status(false, false), 0);
        assert_eq!(exit_status(false, true), EXIT_DEGRADED);
        assert_eq!(exit_status(true, false), EXIT_STOP_INCOMPLETE);
        assert_eq!(exit_status(true, true), EXIT_STOP_INCOMPLETE, "4 wins");
    }

    #[test]
    fn a_channel_that_starts_late_is_warned_about_then_recovers() {
        // The boot case: the adapter enumerates after the service starts.
        let (mut watch, id, t0) = watch_one(true);
        assert_eq!(
            watch.down(id, "COM3 not found", "retrying", t0).as_deref(),
            Some("WARNING: [GPS] did not start: COM3 not found — retrying")
        );
        assert_eq!(
            watch.down(id, "COM3 still not found", "retrying", t0 + MIN),
            None,
            "one warning per outage"
        );
        assert!(watch.degraded());
        assert_eq!(
            watch.up(id, t0 + 2 * MIN).as_deref(),
            Some("RECOVERED: [GPS] started after 2 min")
        );
        assert!(!watch.degraded(), "every outage recovered");
        assert_eq!(
            lines(watch.summary(t0 + 3 * MIN)),
            ["[GPS] recovered from 1 outage, 2 min down in all"]
        );
    }

    #[test]
    fn a_channel_that_never_starts_degrades_the_run() {
        let (mut watch, id, t0) = watch_one(false);
        assert_eq!(
            watch.down(id, "COM3 not found", "retrying", t0).as_deref(),
            Some("WARNING: [GPS] did not start: COM3 not found — not retrying, so it stays down")
        );
        assert!(watch.degraded());
        assert_eq!(
            lines(watch.summary(t0)),
            ["[GPS] never started: COM3 not found"]
        );
    }

    #[test]
    fn a_running_channel_that_goes_down_is_named_with_its_reason() {
        let (mut watch, id, t0) = watch_one(true);
        assert_eq!(watch.up(id, t0), None, "starting is not a recovery");
        assert_eq!(
            watch.down(id, "device removed", "retrying", t0).as_deref(),
            Some("WARNING: [GPS] is down: device removed — retrying")
        );
        assert_eq!(
            watch.up(id, t0 + 90 * Duration::from_secs(1)).as_deref(),
            Some("RECOVERED: [GPS] is running again after 1 min down")
        );
        assert_eq!(
            lines(watch.summary(t0 + MIN)),
            ["[GPS] recovered from 1 outage, 1 min down in all"]
        );
    }

    #[test]
    fn reminders_repeat_every_five_minutes_with_the_latest_reason() {
        let (mut watch, id, t0) = watch_one(true);
        watch.up(id, t0);
        watch.down(id, "device removed", "retrying", t0);
        assert!(watch.reminders(t0 + 4 * MIN).is_empty());
        assert_eq!(
            watch.down(id, "COM3 not found", "retrying", t0 + 4 * MIN),
            None,
            "the newer reason, no new warning"
        );
        assert_eq!(
            watch.reminders(t0 + 5 * MIN),
            ["WARNING: [GPS] has been down for 5 min: COM3 not found"]
        );
        assert!(
            watch.reminders(t0 + 9 * MIN).is_empty(),
            "not again until five minutes later"
        );
        assert_eq!(watch.reminders(t0 + 10 * MIN).len(), 1);
        watch.up(id, t0 + 11 * MIN);
        assert!(watch.reminders(t0 + 20 * MIN).is_empty(), "nothing is down");
    }

    #[test]
    fn a_run_with_nothing_running_and_nothing_retrying_cannot_do_anything() {
        let (mut watch, id, t0) = watch_one(false);
        watch.down(id, "COM3 not found", "retrying", t0);
        assert!(watch.nothing_can_run());

        let (mut watch, id, t0) = watch_one(true);
        watch.down(id, "COM3 not found", "retrying", t0);
        assert!(
            !watch.nothing_can_run(),
            "a retrying channel may still start"
        );
    }

    #[test]
    fn the_summary_names_each_channel_by_id_in_order() {
        let mut watch = HealthWatch::new("stays down");
        let t0 = Instant::now();
        watch.add(7, "GPS", true);
        watch.add(9, "AIS", true);
        watch.up(7, t0);
        watch.up(9, t0);
        watch.down(9, "device removed", "retrying", t0);
        assert_eq!(
            watch.summary(t0 + 12 * MIN),
            [
                (7, "[GPS] ran throughout".to_owned()),
                (
                    9,
                    "[AIS] down at the stop, for 12 min: device removed".to_owned()
                ),
            ]
        );
        assert!(
            watch.degraded(),
            "a channel down at the stop degrades the run"
        );
    }

    #[test]
    fn each_outage_says_how_it_is_being_retried() {
        // An application can retry a failed start and a failure while running
        // on different schedules; the warning carries the one that applies.
        let (mut watch, id, t0) = watch_one(true);
        let slow = doubling_retry(Duration::from_secs(1), Duration::from_secs(30));
        let fast = doubling_retry(Duration::from_millis(250), Duration::from_secs(5));
        assert_eq!(
            watch.down(id, "COM3 not found", &slow, t0).as_deref(),
            Some(
                "WARNING: [GPS] did not start: COM3 not found — retrying in 1 s, doubling to 30 s"
            )
        );
        watch.up(id, t0 + MIN);
        assert_eq!(
            watch
                .down(id, "device removed", &fast, t0 + 2 * MIN)
                .as_deref(),
            Some("WARNING: [GPS] is down: device removed — retrying in 250 ms, doubling to 5 s")
        );
    }

    #[test]
    fn a_channel_that_is_not_retried_never_says_it_is() {
        let (mut watch, id, t0) = watch_one(false);
        assert_eq!(
            watch.down(id, "COM3 not found", "retrying", t0).as_deref(),
            Some("WARNING: [GPS] did not start: COM3 not found — not retrying, so it stays down")
        );
    }

    #[test]
    fn a_span_under_a_second_is_described_in_milliseconds() {
        assert_eq!(describe(Duration::ZERO), "0 ms");
        assert_eq!(describe(Duration::from_millis(260)), "260 ms");
        assert_eq!(describe(Duration::from_millis(999)), "999 ms");
    }

    #[test]
    fn a_span_of_a_second_or_more_is_described_in_whole_larger_units() {
        assert_eq!(describe(Duration::from_secs(1)), "1 s");
        assert_eq!(describe(Duration::from_secs(59)), "59 s");
        assert_eq!(describe(Duration::from_secs(61)), "1 min");
        assert_eq!(
            describe(Duration::from_secs(2 * 3600 + 5 * 60)),
            "2 h 5 min"
        );
    }
}
