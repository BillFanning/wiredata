//! The per-channel scheduler.
//!
//! Each message in a channel fires on its own independent interval. The
//! scheduler tracks a next-fire time per message and, on each [`poll`](Schedule::poll), hands
//! the talker loop the message that is due earliest (ties broken by message
//! order). A message whose interval is zero is *dormant*: it is kept so its
//! interval can later be changed, but it never fires.

use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;

use crate::core::message::{CompiledMessage, MessageConfig};
use crate::core::timing::{ActiveCadence, CadenceAlignment};

const CLOCK_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const CLOCK_STEP_THRESHOLD: Duration = Duration::from_millis(250);

/// One compiled message tracked by the scheduler.
#[derive(Debug)]
struct ScheduledMessage {
    compiled: CompiledMessage,
    /// Send interval. Zero means the message is dormant.
    interval: Duration,
    /// When this message should next fire. `None` while the compiled schedule is
    /// unarmed or this message is dormant.
    next_fire: Option<Instant>,
}

impl ScheduledMessage {
    fn is_active(&self) -> bool {
        !self.interval.is_zero()
    }
}

/// What the talker loop should do next, returned by [`Schedule::poll`].
#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    /// Message `index` is due. `scheduled_for` is its original monotonic
    /// deadline, retained so the runner can measure handling lateness.
    Due {
        index: usize,
        scheduled_for: Instant,
        /// The cadence this fire belongs to. Carried so a receiver can place
        /// the skipped grid points below on the timeline without holding its
        /// own copy of the schedule, which could have moved since.
        interval: Duration,
        /// Cadence points of *this* message that this poll passed over: they
        /// lie at `scheduled_for + interval`, `+ 2 * interval`, … and none of
        /// them will ever fire (see the stall policy on [`Schedule::poll`]).
        ///
        /// Reported here, at the poll that skips them, because that is the
        /// only moment their position in time is known. A count recovered
        /// later can say how many were lost but not when — and *when* is what
        /// decides who was holding the thread.
        skipped: u64,
    },
    /// Nothing is due yet — sleep until at most this instant.
    Wait(Instant),
    /// No active messages; nothing fires until an interval is changed.
    Idle,
}

/// A channel's messages, scheduled by next-fire time.
#[derive(Debug)]
pub struct Schedule {
    messages: Vec<ScheduledMessage>,
    /// Cumulative count of cadence grid points that were skipped (will never
    /// fire) under the stall policy — see [`Schedule::poll`]. A growing value
    /// means the send loop couldn't keep to the configured intervals.
    missed_sends: u64,
    /// Compilation is pure preflight; the runner arms cadence only after its
    /// interface is ready, so setup time can never count as missed sends.
    armed: bool,
    alignment: CadenceAlignment,
    wall_anchor: Option<(Instant, SystemTime)>,
    next_clock_check: Option<Instant>,
    clock_realignments: u64,
}

impl Schedule {
    /// Compile a channel's messages into a runnable schedule.
    ///
    /// `start` is the reference instant: every active message is scheduled to
    /// fire at `start` (i.e. immediately). Returns an error if `messages` is
    /// empty or a payload fails to compile.
    pub fn compile(messages: &[MessageConfig], start: Instant) -> anyhow::Result<Self> {
        let mut schedule = Self::compile_unarmed(messages)?;
        schedule.arm(start);
        Ok(schedule)
    }

    /// Compile payloads and intervals without establishing any deadlines.
    /// Production preflight uses this form; [`Schedule::arm`] belongs at the
    /// runner boundary after predecessor cleanup and interface opening.
    pub fn compile_unarmed(messages: &[MessageConfig]) -> anyhow::Result<Self> {
        anyhow::ensure!(!messages.is_empty(), "channel has no messages");
        let messages = messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let compiled = m
                    .compile()
                    .with_context(|| format!("compiling message {}", i + 1))?;
                Ok(ScheduledMessage {
                    compiled,
                    interval: Duration::from_millis(m.interval_ms),
                    next_fire: None,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            messages,
            missed_sends: 0,
            armed: false,
            alignment: CadenceAlignment::Immediate,
            wall_anchor: None,
            next_clock_check: None,
            clock_realignments: 0,
        })
    }

    /// Select the cadence phase policy before the runner arms this schedule.
    ///
    /// [`CadenceAlignment::UtcPhase`] uses exact interval multiples from the
    /// Unix epoch, not rounded civil-time subdivisions. For example, a 1.5 s
    /// interval alternates between whole- and half-second wall-clock phases.
    pub fn with_alignment(mut self, alignment: CadenceAlignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Establish a fresh cadence grid. Every active message is due immediately
    /// at `start`; dormant messages remain unscheduled.
    pub fn arm(&mut self, start: Instant) {
        self.arm_at(start, SystemTime::now());
    }

    /// Establish a fresh cadence grid from paired monotonic/wall-clock anchors.
    pub fn arm_at(&mut self, start: Instant, wall_clock: SystemTime) {
        self.align_deadlines(start, wall_clock);
        self.wall_anchor =
            (self.alignment == CadenceAlignment::UtcPhase).then_some((start, wall_clock));
        self.next_clock_check = self
            .wall_anchor
            .and_then(|_| start.checked_add(CLOCK_CHECK_INTERVAL));
        for message in &mut self.messages {
            if self.alignment == CadenceAlignment::Immediate {
                message.next_fire = message.is_active().then_some(start);
            }
        }
        self.missed_sends = 0;
        self.clock_realignments = 0;
        self.armed = true;
    }

    fn align_deadlines(&mut self, now: Instant, wall_clock: SystemTime) {
        for message in &mut self.messages {
            message.next_fire = if !message.is_active() {
                None
            } else if self.alignment == CadenceAlignment::Immediate {
                Some(now)
            } else {
                next_phase_delay(wall_clock, message.interval)
                    .and_then(|delay| now.checked_add(delay))
            };
        }
    }

    /// Check at most once per second for a material wall-clock step. UTC-aligned
    /// schedules rebase only future deadlines and never replay the skipped wall grid.
    pub fn reconcile_wall_clock(&mut self, now: Instant) -> bool {
        if self.alignment != CadenceAlignment::UtcPhase
            || self.next_clock_check.is_none_or(|check| now < check)
        {
            return false;
        }
        self.reconcile_wall_clock_at(now, SystemTime::now())
    }

    fn reconcile_wall_clock_at(&mut self, now: Instant, wall_clock: SystemTime) -> bool {
        self.next_clock_check = now.checked_add(CLOCK_CHECK_INTERVAL);
        let Some((anchor_mono, anchor_wall)) = self.wall_anchor else {
            self.wall_anchor = Some((now, wall_clock));
            return false;
        };
        let expected = anchor_wall
            .checked_add(now.saturating_duration_since(anchor_mono))
            .unwrap_or(anchor_wall);
        if system_time_distance(expected, wall_clock) < CLOCK_STEP_THRESHOLD {
            return false;
        }

        self.align_deadlines(now, wall_clock);
        self.wall_anchor = Some((now, wall_clock));
        self.clock_realignments = self.clock_realignments.saturating_add(1);
        true
    }

    pub fn cadence_alignment(&self) -> CadenceAlignment {
        self.alignment
    }

    pub fn clock_realignments(&self) -> u64 {
        self.clock_realignments
    }

    /// Number of messages in the schedule (active and dormant).
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Always `false` — [`Schedule::compile`] rejects an empty message list.
    /// Provided for API completeness alongside [`Schedule::len`].
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Index of the active message with the earliest next-fire time.
    ///
    /// Ties are broken by message order (lowest index first).
    fn earliest(&self) -> Option<usize> {
        self.messages
            .iter()
            .enumerate()
            .filter_map(|(i, message)| message.next_fire.map(|next| (i, next)))
            .min_by_key(|(i, next)| (*next, *i))
            .map(|(i, _)| i)
    }

    /// Decide what to do at `now`.
    ///
    /// If a message is due, its next-fire time is advanced by its interval
    /// (from the previous fire time, so the cadence does not drift) and the
    /// due message is returned by index. Otherwise reports how long to wait.
    ///
    /// **Stall policy: fire once, skip the backlog.** After a stall (a
    /// blocked send, machine sleep), the missed fire times are *not* burst
    /// out back-to-back: one send fires now, then the next-fire time jumps
    /// to the first point of the message's original cadence grid that lies
    /// in the future. Talker generates test traffic — a receiver cares about
    /// cadence, not about conservation of message count.
    ///
    /// The points dropped that way are both added to
    /// [`missed_sends`](Self::missed_sends) and reported on the returned
    /// [`Tick::Due`], so a caller can attribute them while it still knows what
    /// was happening at the time.
    pub fn poll(&mut self, now: Instant) -> Tick {
        // Defensive convenience for direct core users. The production runner
        // arms explicitly after interface open, but an unarmed schedule still
        // starts from its first actual poll rather than aging from compile time.
        if !self.armed {
            self.arm(now);
        }
        let Some(index) = self.earliest() else {
            return Tick::Idle;
        };
        let msg = &mut self.messages[index];
        let Some(next_fire) = msg.next_fire else {
            return Tick::Idle;
        };
        let interval = msg.interval;
        if next_fire <= now {
            let Some(mut following) = next_fire.checked_add(interval) else {
                warn_deadline_unrepresentable(index, interval);
                msg.next_fire = None;
                return Tick::Due {
                    index,
                    scheduled_for: next_fire,
                    interval,
                    skipped: 0,
                };
            };
            let mut skipped = 0u64;
            if following <= now {
                // More than one interval behind: skip the missed grid
                // points. Integer math, not a loop — a long sleep with a
                // short interval could mean millions of missed points.
                // (`interval` is non-zero: `earliest` only yields active
                // messages.)
                let late = now.duration_since(following).as_nanos();
                let interval_nanos = interval.as_nanos();
                // Grid points in (old next_fire, now] that will never fire:
                // the one at `next_fire` plus one per full interval of
                // additional lateness.
                skipped = (late / interval_nanos + 1) as u64;
                let rem = late % interval_nanos;
                following = (now - Duration::from_nanos(rem as u64))
                    .checked_add(interval)
                    .unwrap_or(following);
            }
            msg.next_fire = Some(following);
            self.missed_sends = self.missed_sends.saturating_add(skipped);
            Tick::Due {
                index,
                scheduled_for: next_fire,
                interval,
                skipped,
            }
        } else {
            Tick::Wait(next_fire)
        }
    }

    /// Cumulative count of sends skipped under the stall policy (grid points
    /// that will never fire). Monotonic for the life of the schedule; a
    /// nonzero, growing value at high message rates means the interface's
    /// send call blocks longer than the configured interval.
    pub fn missed_sends(&self) -> u64 {
        self.missed_sends
    }

    /// Render one compiled message immediately before a send attempt.
    ///
    /// Keeping rendering separate from [`poll`](Self::poll) lets the runner
    /// suppress a due fire during retry backoff without allocating payload
    /// bytes or sampling a dynamic timestamp that will never reach `send`.
    pub fn render(&self, index: usize) -> Option<Vec<u8>> {
        self.messages
            .get(index)
            .map(|message| message.compiled.render())
    }

    /// Lossy code-page substitution positions for one compiled message.
    /// This metadata is copied only when a rate-limited display sample is
    /// emitted, keeping it out of the normal send path.
    pub(crate) fn replacement_wire_offsets(&self, index: usize) -> &[usize] {
        self.messages
            .get(index)
            .map_or(&[], |message| message.compiled.replacement_wire_offsets())
    }

    /// The independent cadences this schedule is running: how many messages are
    /// active and the span of their intervals. `None` when every message is
    /// dormant.
    ///
    /// One pass, no allocation — the runner calls this every loop pass to
    /// re-check timer policy, so [`Schedule::set_interval`] changes take effect
    /// immediately.
    pub fn active_cadence(&self) -> Option<ActiveCadence> {
        let mut active = self.messages.iter().filter(|m| m.is_active());
        let mut cadence = ActiveCadence {
            messages: 1,
            shortest: active.next()?.interval,
        };
        for message in active {
            cadence.messages += 1;
            cadence.shortest = cadence.shortest.min(message.interval);
        }
        Some(cadence)
    }

    /// Each message's wire bytes and current interval, in schedule order.
    /// A zero interval means dormant.
    ///
    /// This is the *running* schedule's own demand, so a capacity readout for a
    /// live channel can describe what is sending rather than what is on screen.
    pub fn message_demand(&self) -> impl Iterator<Item = (usize, Duration)> + '_ {
        self.messages
            .iter()
            .map(|message| (message.compiled.wire_len(), message.interval))
    }

    /// Change message `index`'s send interval, effective immediately.
    ///
    /// An interval of 0 makes the message dormant. A non-zero interval
    /// (re)schedules it to fire at `now + interval`. An out-of-range index is
    /// rejected (`false`).
    pub fn set_interval(&mut self, index: usize, interval_ms: u64, now: Instant) -> bool {
        self.set_interval_at(index, interval_ms, now, SystemTime::now())
    }

    fn set_interval_at(
        &mut self,
        index: usize,
        interval_ms: u64,
        now: Instant,
        wall_clock: SystemTime,
    ) -> bool {
        let armed = self.armed;
        if let Some(msg) = self.messages.get_mut(index) {
            msg.interval = Duration::from_millis(interval_ms);
            msg.next_fire = if !armed || !msg.is_active() {
                None
            } else if self.alignment == CadenceAlignment::UtcPhase {
                next_phase_delay(wall_clock, msg.interval).and_then(|delay| now.checked_add(delay))
            } else {
                let next = now.checked_add(msg.interval);
                if next.is_none() {
                    warn_deadline_unrepresentable(index, msg.interval);
                }
                next
            };
            true
        } else {
            false
        }
    }
}

/// A next send past what `Instant` can represent leaves the message dormant.
/// No supported platform reaches this — an `Instant` spans far more than the
/// largest millisecond interval — so it is reported rather than handled, and
/// never silent.
fn warn_deadline_unrepresentable(index: usize, interval: Duration) {
    tracing::warn!(
        "message {}: its next send, {interval:?} ahead, is past what the system clock \
         can represent; it will not be sent again",
        index + 1
    );
}

/// Delay to the strict next Unix-epoch multiple of `interval`.
///
/// The interval need not divide a second, minute, or day. This deliberately
/// preserves the interval's epoch grid instead of rounding it to a nearby
/// civil-time boundary.
fn next_phase_delay(wall_clock: SystemTime, interval: Duration) -> Option<Duration> {
    if interval.is_zero() {
        return None;
    }
    let wall_nanos = match wall_clock.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => i128::try_from(since.as_nanos()).ok()?,
        Err(before) => -i128::try_from(before.duration().as_nanos()).ok()?,
    };
    let interval_nanos = i128::try_from(interval.as_nanos()).ok()?;
    let remainder = wall_nanos.rem_euclid(interval_nanos);
    let wait_nanos = if remainder == 0 {
        interval_nanos
    } else {
        interval_nanos - remainder
    };
    let wait_nanos = u128::try_from(wait_nanos).ok()?;
    let secs = u64::try_from(wait_nanos / 1_000_000_000).ok()?;
    let nanos = u32::try_from(wait_nanos % 1_000_000_000).ok()?;
    Some(Duration::new(secs, nanos))
}

fn system_time_distance(left: SystemTime, right: SystemTime) -> Duration {
    left.duration_since(right)
        .unwrap_or_else(|before| before.duration())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::message::PayloadConfig;

    fn msg(hex: &str, interval_ms: u64) -> MessageConfig {
        MessageConfig::new(PayloadConfig::raw_hex(hex), interval_ms)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ── compile ───────────────────────────────────────────────────────────────

    #[test]
    fn compile_empty_returns_error() {
        assert!(Schedule::compile(&[], Instant::now()).is_err());
    }

    #[test]
    fn compile_bad_payload_returns_error_with_context() {
        let err = Schedule::compile(&[msg("XYZ", 100)], Instant::now()).unwrap_err();
        assert!(err.to_string().contains("message 1"));
    }

    #[test]
    fn unarmed_preflight_starts_at_first_poll_without_false_misses() {
        let compiled_at = Instant::now();
        let first_poll = compiled_at + ms(10_000);
        let mut schedule = Schedule::compile_unarmed(&[msg("AB", 100)]).unwrap();

        assert!(matches!(schedule.poll(first_poll), Tick::Due { .. }));
        assert_eq!(schedule.missed_sends(), 0);
        assert_eq!(schedule.poll(first_poll), Tick::Wait(first_poll + ms(100)));
    }

    // ── poll ──────────────────────────────────────────────────────────────────

    #[test]
    fn single_message_fires_immediately_then_waits() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        match s.poll(t0) {
            Tick::Due {
                index,
                scheduled_for,
                interval,
                skipped,
            } => {
                assert_eq!(index, 0);
                assert_eq!(scheduled_for, t0);
                assert_eq!(interval, ms(100));
                assert_eq!(skipped, 0);
                assert_eq!(s.render(index), Some(vec![0xAB]));
            }
            other => panic!("expected Due, got {other:?}"),
        }
        assert_eq!(s.poll(t0), Tick::Wait(t0 + ms(100)));
        assert_eq!(s.poll(t0 + ms(99)), Tick::Wait(t0 + ms(100)));
        assert!(matches!(s.poll(t0 + ms(100)), Tick::Due { index: 0, .. }));
    }

    #[test]
    fn independent_intervals_fire_at_their_own_rate() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("01", 100), msg("02", 300)], t0).unwrap();
        // both due at t0; tie broken by message order
        assert!(matches!(s.poll(t0), Tick::Due { index: 0, .. }));
        assert!(matches!(s.poll(t0), Tick::Due { index: 1, .. }));
        assert_eq!(s.poll(t0), Tick::Wait(t0 + ms(100)));
        // the 100 ms message fires at 100 and 200 on its own
        assert!(matches!(s.poll(t0 + ms(100)), Tick::Due { index: 0, .. }));
        assert!(matches!(s.poll(t0 + ms(200)), Tick::Due { index: 0, .. }));
        // at 300 both are due again; message order decides
        assert!(matches!(s.poll(t0 + ms(300)), Tick::Due { index: 0, .. }));
        assert!(matches!(s.poll(t0 + ms(300)), Tick::Due { index: 1, .. }));
    }

    #[test]
    fn next_fire_does_not_drift_when_polled_late() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(matches!(s.poll(t0), Tick::Due { .. })); // next fire -> t0+100
                                                         // polled late at t0+150: still due; next fire advances from 100, not 150
        assert!(matches!(
            s.poll(t0 + ms(150)),
            Tick::Due {
                scheduled_for,
                ..
            } if scheduled_for == t0 + ms(100)
        ));
        assert_eq!(s.poll(t0 + ms(150)), Tick::Wait(t0 + ms(200)));
    }

    #[test]
    fn stall_fires_once_then_skips_missed_ticks_staying_on_grid() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(matches!(s.poll(t0), Tick::Due { .. })); // next fire → t0+100
                                                         // 9½ intervals late: exactly one catch-up send fires…
        assert!(matches!(
            s.poll(t0 + ms(1050)),
            Tick::Due {
                scheduled_for,
                skipped: 9,
                ..
            } if scheduled_for == t0 + ms(100)
        ));
        // …and the next fire is the first *future* point of the original
        // cadence grid (t0+1100) — not nine burst sends, and no drift.
        assert_eq!(s.poll(t0 + ms(1050)), Tick::Wait(t0 + ms(1100)));
        // The nine skipped grid points (t0+200 … t0+1000) are counted.
        assert_eq!(s.missed_sends(), 9);
    }

    /// The running total and the per-tick report are two views of one event,
    /// so they must never disagree. The counter is what the run summary
    /// publishes; the tick is what attribution charges to a blocking send. If
    /// these drifted, a channel could report misses that no message caused —
    /// or the reverse — and nothing else in the app would notice.
    #[test]
    fn the_running_total_is_exactly_what_the_ticks_reported() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100), msg("CD", 250)], t0).unwrap();
        let mut reported = 0u64;
        // Poll across an even stall, an uneven one, and points that are on
        // grid, draining every message due at each instant.
        for offset in [0, 30, 1050, 1100, 1337, 3000] {
            while let Tick::Due { skipped, .. } = s.poll(t0 + ms(offset)) {
                reported += skipped;
            }
        }
        assert!(reported > 0, "the stalls should have skipped something");
        assert_eq!(reported, s.missed_sends());
    }

    #[test]
    fn stall_of_exactly_one_interval_does_not_skip() {
        // One interval late is the boundary: the normal advance already
        // lands the next fire in the future, so no skipping happens.
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(matches!(s.poll(t0), Tick::Due { .. }));
        assert!(matches!(s.poll(t0 + ms(199)), Tick::Due { .. }));
        assert_eq!(s.poll(t0 + ms(199)), Tick::Wait(t0 + ms(200)));
        assert_eq!(s.missed_sends(), 0);
    }

    #[test]
    fn on_time_sends_never_count_as_missed() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(matches!(s.poll(t0), Tick::Due { .. }));
        assert!(matches!(s.poll(t0 + ms(100)), Tick::Due { .. }));
        assert!(matches!(s.poll(t0 + ms(200)), Tick::Due { .. }));
        assert_eq!(s.missed_sends(), 0);
    }

    #[test]
    fn missed_sends_accumulate_across_stalls() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(matches!(s.poll(t0), Tick::Due { .. })); // next fire → t0+100
                                                         // Two intervals late: the t0+200 point is skipped (fires at 250 as
                                                         // the late t0+100 point; next fire lands on t0+300).
        assert!(matches!(s.poll(t0 + ms(250)), Tick::Due { .. }));
        assert_eq!(s.missed_sends(), 1);
        assert!(matches!(s.poll(t0 + ms(300)), Tick::Due { .. })); // on grid
                                                                   // A second stall adds to the same counter: the send at 550 is the
                                                                   // late t0+400 point, so only t0+500 is skipped.
        assert!(matches!(s.poll(t0 + ms(550)), Tick::Due { .. }));
        assert_eq!(s.missed_sends(), 2);
    }

    #[test]
    fn schedule_len_counts_all_messages() {
        let s = Schedule::compile(&[msg("AB", 100), msg("CD", 0)], Instant::now()).unwrap();
        assert_eq!(s.len(), 2);
        assert!(!s.is_empty());
    }

    #[test]
    fn active_cadence_counts_messages_and_reports_the_tightest_interval() {
        let t0 = Instant::now();
        let s = Schedule::compile(&[msg("AB", 100), msg("CD", 10), msg("EF", 0)], t0).unwrap();
        let cadence = s.active_cadence().expect("two messages are active");
        // The dormant message is excluded from the count and from the minimum.
        assert_eq!(cadence.messages, 2);
        assert_eq!(cadence.shortest, ms(10));
    }

    #[test]
    fn active_cadence_is_absent_when_every_message_is_dormant() {
        let s = Schedule::compile(&[msg("AB", 0), msg("CD", 0)], Instant::now()).unwrap();
        assert_eq!(s.active_cadence(), None);
    }

    #[test]
    fn active_cadence_follows_set_interval() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100), msg("CD", 40)], t0).unwrap();
        assert_eq!(s.active_cadence().map(|c| c.messages), Some(2));

        // Making the fast message dormant shrinks the count and moves the
        // minimum up to the remaining message's interval.
        s.set_interval(1, 0, t0);
        let cadence = s.active_cadence().expect("one message is still active");
        assert_eq!(cadence.messages, 1);
        assert_eq!(cadence.shortest, ms(100));
    }

    #[test]
    fn nmea_payload_compiles_and_fires() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(
            &[MessageConfig::new(
                PayloadConfig::nmea("GP", "GGA", vec![]),
                1000,
            )],
            t0,
        )
        .unwrap();
        match s.poll(t0) {
            Tick::Due { index, .. } => {
                let payload = s.render(index).unwrap();
                let wire = std::str::from_utf8(&payload).unwrap();
                assert!(wire.starts_with("$GPGGA*"));
            }
            other => panic!("expected Due, got {other:?}"),
        }
    }

    // ── dormant messages ──────────────────────────────────────────────────────

    #[test]
    fn dormant_message_never_fires() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("00", 0), msg("11", 100)], t0).unwrap();
        // only the active message fires
        assert!(matches!(s.poll(t0), Tick::Due { index: 1, .. }));
        assert_eq!(s.poll(t0), Tick::Wait(t0 + ms(100)));
    }

    #[test]
    fn all_dormant_is_idle() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("00", 0), msg("11", 0)], t0).unwrap();
        assert_eq!(s.poll(t0), Tick::Idle);
    }

    // ── set_interval ──────────────────────────────────────────────────────────

    #[test]
    fn set_interval_to_zero_makes_dormant() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(s.set_interval(0, 0, t0));
        assert_eq!(s.poll(t0), Tick::Idle);
    }

    #[test]
    fn set_interval_reschedules_from_now() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(s.set_interval(0, 200, t0 + ms(50)));
        assert_eq!(s.poll(t0 + ms(50)), Tick::Wait(t0 + ms(250)));
        assert!(matches!(s.poll(t0 + ms(250)), Tick::Due { index: 0, .. }));
    }

    #[test]
    fn set_interval_can_revive_a_dormant_message() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 0)], t0).unwrap();
        assert_eq!(s.poll(t0), Tick::Idle);
        assert!(s.set_interval(0, 100, t0));
        assert_eq!(s.poll(t0), Tick::Wait(t0 + ms(100)));
    }

    #[test]
    fn set_interval_out_of_range_is_ignored() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(!s.set_interval(99, 500, t0)); // must not panic
        assert!(matches!(s.poll(t0), Tick::Due { index: 0, .. }));
    }

    #[test]
    fn utc_alignment_waits_for_the_next_strict_epoch_phase() {
        let t0 = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + ms(12_345);
        let mut schedule = Schedule::compile_unarmed(&[msg("AB", 1_000)])
            .unwrap()
            .with_alignment(CadenceAlignment::UtcPhase);
        schedule.arm_at(t0, wall);

        assert_eq!(schedule.poll(t0), Tick::Wait(t0 + ms(655)));
        assert!(matches!(
            schedule.poll(t0 + ms(655)),
            Tick::Due { scheduled_for, .. } if scheduled_for == t0 + ms(655)
        ));
        assert_eq!(schedule.poll(t0 + ms(655)), Tick::Wait(t0 + ms(1_655)));

        let mut exact = Schedule::compile_unarmed(&[msg("AB", 1_000)])
            .unwrap()
            .with_alignment(CadenceAlignment::UtcPhase);
        exact.arm_at(t0, SystemTime::UNIX_EPOCH + ms(12_000));
        assert_eq!(
            exact.poll(t0),
            Tick::Wait(t0 + ms(1_000)),
            "an exact boundary waits for the next boundary rather than firing immediately"
        );
    }

    #[test]
    fn utc_alignment_keeps_epoch_phase_for_intervals_that_do_not_divide_a_second() {
        let t0 = Instant::now();
        let mut schedule = Schedule::compile_unarmed(&[msg("AB", 1_500)])
            .unwrap()
            .with_alignment(CadenceAlignment::UtcPhase);

        // Epoch multiples of 1.5 s alternate between whole- and half-second
        // wall-clock positions; this mode does not round to a civil-time unit.
        schedule.arm_at(t0, SystemTime::UNIX_EPOCH + ms(10_100));
        assert_eq!(schedule.poll(t0), Tick::Wait(t0 + ms(400)));
        assert!(matches!(
            schedule.poll(t0 + ms(400)),
            Tick::Due { scheduled_for, .. } if scheduled_for == t0 + ms(400)
        ));
        assert_eq!(schedule.poll(t0 + ms(400)), Tick::Wait(t0 + ms(1_900)));
    }

    #[test]
    fn wall_clock_step_rebases_future_deadlines_without_replaying_history() {
        let t0 = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + ms(10_250);
        let mut schedule = Schedule::compile_unarmed(&[msg("AB", 1_000)])
            .unwrap()
            .with_alignment(CadenceAlignment::UtcPhase);
        schedule.arm_at(t0, wall);
        assert_eq!(schedule.poll(t0), Tick::Wait(t0 + ms(750)));

        let checked_at = t0 + Duration::from_secs(1);
        let stepped_wall = SystemTime::UNIX_EPOCH + ms(15_400);
        assert!(schedule.reconcile_wall_clock_at(checked_at, stepped_wall));
        assert_eq!(schedule.clock_realignments(), 1);
        assert_eq!(schedule.missed_sends(), 0);
        assert_eq!(schedule.poll(checked_at), Tick::Wait(checked_at + ms(600)));
    }

    #[test]
    fn small_wall_clock_drift_does_not_churn_the_monotonic_grid() {
        let t0 = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + ms(10_250);
        let mut schedule = Schedule::compile_unarmed(&[msg("AB", 1_000)])
            .unwrap()
            .with_alignment(CadenceAlignment::UtcPhase);
        schedule.arm_at(t0, wall);

        let checked_at = t0 + Duration::from_secs(1);
        let expected_plus_100ms = SystemTime::UNIX_EPOCH + ms(11_350);
        assert!(!schedule.reconcile_wall_clock_at(checked_at, expected_plus_100ms));
        assert_eq!(schedule.clock_realignments(), 0);
        assert!(matches!(
            schedule.poll(checked_at),
            Tick::Due { scheduled_for, .. } if scheduled_for == t0 + ms(750)
        ));
        assert_eq!(schedule.poll(checked_at), Tick::Wait(t0 + ms(1_750)));
    }

    #[test]
    fn aligned_live_interval_change_uses_the_new_intervals_next_utc_phase() {
        let t0 = Instant::now();
        let mut schedule = Schedule::compile_unarmed(&[msg("AB", 1_000)])
            .unwrap()
            .with_alignment(CadenceAlignment::UtcPhase);
        schedule.arm_at(t0, SystemTime::UNIX_EPOCH + ms(10_250));

        assert!(schedule.set_interval_at(
            0,
            2_000,
            t0 + ms(100),
            SystemTime::UNIX_EPOCH + ms(10_350),
        ));
        assert_eq!(schedule.poll(t0 + ms(100)), Tick::Wait(t0 + ms(1_750)));
    }
}
