# TODO — Listener

Work the `listener` code does not do yet. Decisions go in [ADR.md](ADR.md) and
behaviour in the [specification](listener_specification.md), whose Appendix A
lists what is deferred on purpose. Workspace-level tasks live in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md).

Each item names the symbol or section where the work lands, so it can be found
again. Remove an item when it is done: the commit says what changed.

---

## Features

- [ ] **Disk guard in the GUI (§56.2).** Neither recording editor,
  `edit_raw_recording` nor `edit_display_recording`, sets `disk_guard`, so a
  guard comes only from a profile.
- [ ] **A general match-rule editor (§50.2).** `edit_mark_rules` edits only a
  `BytePattern` rule with a `Mark` action (ADR-016). `Idle` conditions and the
  `Record`, `Notify` and `PauseDisplay` actions arrive only through profiles.
- [ ] **A glyph for a bare `Mark` (optional).** A `Mark` with no timestamp writes
  its `‹MARK …›` line into the `.disp`, but the live view draws nothing for it.
  Low priority: kept simple on purpose.
- [ ] **RS-422 and RS-485 (§14.5).** They follow RS-232; nothing is specific to
  them yet beyond the RTS line of §14.3.

## Soak

- [ ] **The Serial stall cell under sustained backpressure (ADR-034).**
  `SerialStallState` and `retry_stalled_send` have unit tests for their edges,
  poison recovery and unwind guard, but no run holds the Transport→Pipeline
  queue full for a whole window. The soak harness (ADR-051) sends UDP only, so
  this needs its own test in `listener/tests/soak.rs`: hold a bounded ingest
  receiver without draining it, run the `BlockingReader` seam at rate, and check
  that `completed_episodes` only grows, `completed_total` never exceeds wall
  time, `active_for` is set while stalled and clear within one snapshot of
  resuming, and the final snapshot has no episode open. A missed edge shows up
  as a stall that grows forever, so a long window is the point.
- [ ] **Slow-disk recording with rotation.** Needs an injectable slow writer
  rather than a real disk; do it with whatever next touches the recorder's
  writer seam.
- [ ] **Soak runs 1 and 2 (ADR-051).** The procedure and pass criteria are in
  [the soak runbook](../../wiredata-soak/README.md). The Linux runner has not
  been run yet.
