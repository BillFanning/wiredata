# TODO — Talker

Implementation reminders for the `talker` application and the workspace as a whole.
Tasks specific to the `nmea0183` library live in
[`nmea0183/docs/TODO.md`](../../nmea0183/docs/TODO.md). Not architectural decisions
(those go in the ADR's Open questions section).

Cross off items as they are completed. Add new ones inline as they come up.

---

## Spec 2.5.0 requirements not yet built (2026-09-30)

The spec states these; the code does not do them yet.

- [ ] **TCP client (ADR-059, §4.5).** `TcpClientInterface` has no retry
  preparation, so it never reconnects, and it never reads the socket.
- [ ] **Possibly partial (ADR-059, §4.4).** Not a send outcome yet, and wire bytes
  are not counted apart from whole messages.
- [ ] **Strict profiles (ADR-062, §8.2).** Unknown keys are ignored, and
  `extract_version` treats a missing version as current.

## Profiles

- [ ] **Make the profile directory user-configurable.** Default stays the OS config
  dir (`dirs::config_dir()/talker/profiles`, `core::profile::default_dir`) — the safe,
  always-writable choice. Add an override so users can point talker at a directory of
  their choosing (e.g. a `--profile-dir` CLI flag, a `TALKER_PROFILE_DIR` env var, and/or
  a GUI setting), which also enables a portable "profiles next to the .exe" layout
  without making it the default. Sample profiles ship in `talker/profiles/`.

## Logging

- [ ] **Define file-log retention before promising bounded disk use.** GUI rotation
  starts a new file daily; CLI profiles may select never, hourly, or daily rotation.
  None deletes old files or bounds disk use. Choose an age, file-count, or byte limit,
  including how cleanup failure is reported, before describing file logging as
  disk-bounded.

## Docs

- [x] **Spec §8.1 wording tighten** — folded into spec v2.1.1 (2026-07-16): the
  "Queue model" bullet now reads "tracks each message's next-fire-time
  (conceptually a priority queue)". (External review, 2026-07-10.)
- [x] **Spec §3.2 detail-header readouts** — folded into spec v2.1.1
  (2026-07-16): §3.2 now describes the shipped master–detail pane (wire-facts /
  throughput / observer-health readout grouping post-ADR-018, lifecycle button
  pair, list-row contents, Profile menu in the list header, ✕ removal,
  "Configure interface" / "Configure messages" titles). The original item's
  `TalkerStatus::Sent` field references had gone stale (ADR-018 replaced `Sent`
  with counters + samples); the spec text was written from the current
  `show_channel_header`. (GUI-merge harmonization, 2026-07-10.)

## Robustness & performance (external review, 2026-07-11)

- [ ] **Hardware-check serial recovery on Windows.** With a running schedule,
  unplug and replace a USB serial adapter that returns under the same COM name;
  confirm that the same run resumes after the port becomes available. Separately
  hold CTS low through a write timeout and confirm that transient flow control
  does not cause port reopen or DTR-reset churn. The automated coverage fixes the
  state and accounting contract; this check validates real driver mappings and
  hardware behavior.
- [ ] **Define automatic TCP reconnect after an established stream fails.** The
  current bounded failure episode retries the existing stream, which cannot heal
  after a reset or broken connection. Decide whether and how to establish a fresh
  connection from the confirmed address at an eligible retry. A failed reconnect
  must withhold that due send without render, interface-write, or send-call timing;
  it must not count as a failed write or replay a write that may already have
  transferred a prefix. UDP retains its existing socket because replacing it can
  change source-port semantics. Amend the current-behaviour spec and record the
  transport decision before implementation.
- [x] **Telemetry split (ADR-018, accepted + implemented 2026-07-11).**
  `TalkerStatus::Sent` replaced by the three lanes: `Counters` (≤5 Hz +
  final-at-stop, cumulative totals plus current send-failure state,
  self-correcting), `SendSample` (payload-bearing, ≤10 Hz newest-per-interval),
  immediate errors. The owner picks the policy
  via `ObserverPolicy` (`sampled()` for the GUI, `every_send()` for CLI
  `--echo`). Pinned by `sampled_policy_bounds_payload_traffic` + the reworked
  `sends_on_schedule_and_reports_self_describing_counts`. Spec "status"
  wording: fold into the next spec pass (no bump alone).
- [ ] **Confirm the sampled Output pane reads correctly in a live run** (ADR-018).
  Above ~10 Hz the pane shows the newest-per-interval `SendSample`, not every
  send — the one user-visible consequence of the telemetry split, shipped
  2026-07-11 and never confirmed against a real high-rate run. If the sampling
  reads as lost data rather than a sample, the fix is presentation (how the pane
  labels what it is showing), not the lane.
- [x] **Observer-path allocations — MEASURED 2026-07-12, three of four killed
  by the baselines** (`cargo bench -p talker`, criterion baseline `main`).
  Decision rule: proceed only above ~1% of a core at 100 Hz–1 kHz. Verdicts:
  - Scheduler heap: KILLED. `poll` idle-scan is linear at ~1.7 ns/message
    (19 ns @ 8, 874 ns @ 512); even 512 messages at 1 kHz is 0.09% CPU.
  - `render_into`/reusable send buffer: KILLED. `poll` due-send is 130 ns @
    64 B and 136 ns @ 1 KiB — the per-send clone is a memcpy; ADR-018 already
    removed the expensive per-send observer copies.
  - shortest-interval-scan caching (then `min_active_interval`, now
    `active_cadence`): KILLED. 761 ns @ 512 messages, ~25 ns at
    realistic counts; 0.08% CPU at 1 kHz.
  - GUI output row ring/virtualization: KILLED and reverted. See below.
- [x] **Output-pane virtualization experiment — REVERTED 2026-07-13.** The
  `show_rows` port measured no frame-time improvement at talker's 200-message
  cap (~0.7 ms steady / ~1.25 ms peak for the whole UI, identical with the
  pane stopped and cleared). Review then found a correctness cost: soft wraps
  became separate selectable labels, so copied text gained synthetic newlines
  and selection could not retain an offscreen virtualized endpoint. The pane
  is again one memoized selectable label, preserving exact logical flow.
  Listener keeps virtualization because its byte-bounded window is ~1 MB and
  its custom stream view owns logical offsets; the workloads are not equivalent.
- [x] **Command enqueue failures** — DONE (`6d4da3b`). Failed `Stop`/
  interface-update/`SetInterval` enqueue attempts surface in the channel's
  error banner, distinguishing queue-full (runner wedged) from runner-exited
  (a moot Stop stays silent). Listener's sibling landed in the same commit.
- [x] **TalkerSupervisor (ADR-019) — DONE 2026-07-11.**
  `core::supervisor::TalkerSupervisor` owns the channel slots (runner threads,
  channel pairs, draining, `ChannelTelemetry`); the GUI holds view-state only
  and reads `telemetry(i)`. Settlements + two behaviour improvements (exact
  totals at rest via kept draining receivers; orphan reaping) recorded in the
  ADR-019 entry. CLI adoption rides with the future ad-hoc CLI work (the
  parity item below) — the one-shot headless run keeps its blocking `--echo`
  funnel by design. Pinned by the `core::supervisor` unit tests.
- [x] **Palette bypasses** — done 2026-08-05, ADR-048. The durable boundary:
  shared chrome colors (status, severity, destructive) come from
  `wiredata-ui`; content-semantic highlights stay application-owned.

## External review round 4 (2026-08-06)

Fixed, with the reasoning in the ADRs named — not restated here:

- Miss attribution kept one send window, so a skip behind an earlier write went
  uncharged and the callout reported the shortfall as an idle thread. Ring of
  retained windows, partitioned charge, honest wording (ADR-051 correction).
- The routing line stated the largest culprit and the remainder but not the
  charged total, so with 5 + 4 + 3 it accounted for 8 of 12.
- A Hex line was bounded by cells, and a Mark spends one cell but several
  columns, so an annotated line overran the pane and was cut mid-byte
  (listener ADR-041).
- `Evidence` was defined in `wiredata-ui` and adopted nowhere; removed.
- Two palette tests asserted less than their names (ADR-050 correction), and
  broadening the glyph-size test found `○` shipping without an optical
  correction, so a low control line rendered smaller than the high one beside it.
- Doc drift: listener §57 claimed the sidecar carried a monotonic reading it
  never wrote; the glyph module said "three symbols" with four defined.

Still open:

- [x] **Split `talker/src/gui/diagnostics.rs`** — done 2026-08-07. One submodule
  per readout family (`outcomes`, `capacity`, `cadence`, `per_message`,
  `missed_routing`, `timing`), each owning its wording, thresholds and tests;
  `mod.rs` keeps only what more than one row needs and re-exports flat, so no
  call site changed. Movement only — every function name and all 28 tests are
  the same, verified line by line against the original.
  (Also open here: **only the interface write counts as holding the channel** —
  one item, tracked under the ADR-045 review disposition below. A second
  checkbox for it would count one piece of work twice.)

## Colour accessibility (2026-08-06)

Closed. The pass ran from a red-green deficiency making `fault` unreadable
against `warning` through to a full audit of colour-only states, and the whole
account — what changed, what was cleared and why — is in **ADR-049 / ADR-050**
and the commits they name. It is not repeated here: this file tracks work
outstanding, and an investigation narrative kept alongside it is the same
duplication the review that prompted the pass complained about.

One item survives as work:

- [ ] **`running` and `warning` do not earn their pair budget.** Confirmed
  indistinguishable under the same deficiency. Not a functional failure —
  everywhere the two appear a glyph and a word already carry the state — so the
  question is whether two accents are worth keeping for reinforcement one reader
  does not receive. Revisit if a third state ever wants a colour; see ADR-050 for
  the pair argument.

## Robustness (external review round 2, 2026-07-12)

- [x] **Transactional draft → profile flush.** `flush_drafts_to_profile` was
  a `filter_map`: an invalid draft silently vanished from Save AND compressed
  the profile indices, so Start could read another channel's messages. Now
  all-or-none and index-preserving (`drafts_to_channels`), with a blocking
  dialog on save listing exactly what to fix; Start reads messages straight
  from its own channel's drafts, strictly. Pinned by
  `drafts_to_channels_is_all_or_none_with_reasons` +
  `drafts_to_channels_preserves_indices_when_valid`.
- [x] **`join_all` is safe on live handles** — the command sender is dropped
  before the join (a live sender deadlocked: the runner kept waiting for
  commands), and receivers are drained so a runner block-sending its final
  counters always completes.
- [x] **"Exact at rest" is now a guarantee, not best-effort** — the final
  `Counters` at runner exit is a blocking send (cadence no longer matters at
  exit; the owner drains stopped runners until thread exit). Error statuses
  (`ConnectionError`/`SendRecovered`) now route through `emit_status`, so a
  dropped one is at least counted in `dropped_statuses`.
- [x] **Error-class separation for `last_error`** — `ChannelTelemetry` splits
  `last_error` (current interface class; refreshed by retry snapshots and cleared
  by a live `SendSample`/`SendRecovered`) from `command_error` (control-plane
  class; cleared only by a later successfully executed command for the same
  target, or start), and the UI banner prefers the command error. Repeated
  counters repair a dropped failure or recovery edge and the cumulative episode
  count repairs the app-wide error tally.
- [x] **Correlated command execution (ADR-021)** — enqueue success is no
  longer presented as application. Live interface/interval commands carry
  ids; the runner reports `Applied`/`Failed` on a reliable control lane; the
  supervisor validates id + target, retains failures per target, and changes
  applied runtime state only on confirmed success. Start-time interface state
  is also confirmed by the runner. Pinned by
  `interface_execution_result_controls_applied_state_and_scoped_error` and
  `start_time_interface_becomes_applied_only_after_open_succeeds`, plus
  `rejected_interval_reports_execution_failure`.
- [x] **Whole-run reconciliation + exclusive-resource updates (ADR-022).**
  `AppliedRunConfig` confirms interface and messages together; failed opens
  leave both pending; unexpected runner exit fails every accepted command.
  Same-port serial and same-bound-port UDP changes reconfigure the owned
  handle with rollback instead of attempting an impossible double-open. Stop
  resolves unobserved command results and clears the applied-run baseline.
  Pinned by `accepted_command_fails_when_runner_exits_before_execution` and
  `same_bound_port_reconfigures_without_a_second_bind`.
- [x] **Complete scheduled-send outcomes.** Cumulative counters now distinguish
  successful, interface-failed, retry-suppressed, and scheduler-missed sends.
  The GUI's shortfall percentage uses all four outcomes instead of calling
  `sent + missed` "attempts" while silently excluding failures/suppression.
- [x] **Per-run GUI rate state.** Apply & Restart resets the throughput
  estimator and Output sampling latch, so the prior run cannot advertise a
  stale rate or sampling badge during the new run's first measurement window.
- [x] **Code-page fallback (ADR-023).** Unsupported
  Unicode scalars in ASCII/code-page payloads become visible `?` (`0x3F`)
  substitutions instead of rejecting the schedule; malformed byte markers
  remain errors. Contrast-aware amber backgrounds identify unsupported source
  characters and their fallback bytes in both Wire preview and live Output
  without marking literal question marks. Replacement offsets are compiled once
  and travel only with the rate-limited display sample.
- [x] **Bounded multiline message editors (ADR-024).** UTF-8, UTF-16, and
  ASCII editors show explicit line breaks, never soft-wrap, grow from three
  through eight visible rows, and provide horizontal and vertical scrolling
  on overflow. Horizontal sizing reuses the editor's non-wrapping galley, and
  unchanged marker-aware repaints do not copy the message repair snapshot.
- [x] **Active-run replacement preflight (ADR-025).** A complete candidate
  interface, message list, and schedule now compile before supervisor state is
  touched. Invalid edits disable `Apply & Restart`, surface their exact
  one-based message error, and leave the current runner and applied run intact.
- [x] **Sample lane rotates across messages** — the lane skips a repeat of the
  last sampled index while due (never longer than one full cycle), so an
  aligned multi-message schedule no longer shows message 0 forever. Pinned by
  `sample_lane_rotates_across_messages`.
- [x] **Stable ChannelIds (ADR-020)** — `core::channel::ChannelId` minted per
  slot; runners start with a `RunnerIdentity { id, label }` and stamp the id
  into every `TalkerStatus` and structured `channel` tracing field; GUI log
  tallies are keyed by id (`HashMap<ChannelId, LogCounts>`) and mapped to
  rows via `TalkerSupervisor::channel_id(i)`. Positional routing remains
  only where position is the meaning (`PayloadSample::slot`, CLI echo tag).
  Pinned by `channel_ids_are_stable_across_slot_removal` + the id assertions
  in the runner status tests.
- [x] **Timestamp/checksum render path — MEASURED 2026-07-12, KILLED (with a
  threshold).** `schedule/due-and-render/64B-timestamp-crc16`: ~2.1 µs/send
  vs ~126 ns for the static 64B clone — the "render is ~free" reading does
  **not** generalize (17×, mostly the three chrono `format().to_string()`
  temporaries), but the decision does: at the ADR-017 practical ceiling
  (~1 kHz) that is ~0.2% of a core, well under the ~1% action rule.
  `render_into` + a preformatted timestamp buffer becomes worth building
  only if dynamically-rendered sends approach ~5 kHz sustained. Same-run
  comparisons only; absolute numbers swing with ambient load.
- [x] **Live NMEA render path — MEASURED 2026-07-17, KILLED (with the same
  threshold).** The paired equal-wire-shape Criterion cases measured static GGA
  at ~140 ns/send and live millisecond GGA at ~2.21 µs/send. The field-vector
  clone, substitutions, and checksum rebuild are about 16x the static clone, but
  consume only ~0.22% of one core at 1,000 sends/s. Keep the simpler compiled
  template; revisit direct borrowed-field/buffer rendering only near 4.5–5 kHz
  sustained live-NMEA sends. Same-run comparison on 2026-07-17; command:
  `cargo bench -p talker --bench scheduler -- nmea-gga`.

## Timing and telemetry plan (2026-07-19)

- [x] **Truthful send-path boundaries (ADR-032).** Due selection now returns the
  original monotonic deadline without rendering. The runner records cumulative,
  fixed-size deadline-lateness, render-duration, and synchronous-send-call
  histograms; retry-suppressed fires do not render. The selected detail pane shows
  warm-up, p99 upper bounds, and maximums with boundary tooltips.
- [x] **Recent-window health without raw samples (ADR-033).** Ten fixed one-second
  histogram segments supply the explicitly labeled recent view; cumulative run
  maxima remain visible and final recent state is retained at Stop.
- [x] **Surface timer mode and reason (ADR-033).** Core reports Standard / Windows
  1 ms / request failed / native deadline waits plus the shortest active interval.
  The GUI presents that state without re-deriving platform policy. Timer reconciliation
  now follows queued interval changes, and a failed begin is never paired with an end.
- [x] **Explicit Precise timing mode (ADR-034).** Standard and Precise both keep the
  automatic continuous Windows 1 ms request below 32 ms. At 32 ms or longer, Precise
  uses bounded final-32-ms windows while Standard keeps native waits. Dormant
  schedules hold no request, non-Windows targets keep one native wait, and timer
  telemetry exposes configured mode, active reason, and request outcome. Timestamp
  formatting remains independent; the additive field defaults to Standard within
  profile schema v2.
- [x] **Explicit timer lifecycle state (ADR-040).** `TimerReconciler` owns the guard,
  staged waits, derived status, edge notifications, counter invalidation, and
  observer-drop accounting. The proposed continuous hold for near-threshold Precise
  intervals was rejected without timing or energy evidence; ADR-034's bounded windows
  remain in force.
- [x] **Optional UTC phase alignment (ADR-037).** This remains separate from Precise
  wake policy. Each message targets its strict next Unix-epoch-modulo interval phase,
  then advances monotonically; intervals need not divide a day. Material wall-clock
  steps rebase future deadlines only, with no replay, and telemetry reports the
  re-alignment count without implying physical wire time.
- [x] **Shared process timer policy (ADR-038).** `wiredata-timing` now owns the
  refcounted Windows 1 ms request and minimized-window opt-out used by both apps.
  Application thresholds, wait staging, cadence, and timer telemetry remain local;
  Linux and macOS retain native deadline waits.
- [x] **Shared bounded duration primitives (ADR-039).** `wiredata-telemetry` now owns
  the fixed duration buckets and ten-segment aging engine used by both apps. Send
  aggregates, measurement boundaries, retention, and presentation remain local.
- [x] **Decision-oriented diagnostics summary (ADR-041).** The selected-channel
  view leads with compact Send outcomes, Cadence, and Capacity rows plus
  exception-only Attention, while the exact telemetry and caveats remain under
  collapsed details. There is no composite health score. Classification stays
  Talker-owned; only the identical egui card/row/callout chrome is shared through
  `wiredata-ui`.
- [x] **Capacity preflight (ADR-035).** Current-draft exact wire lengths and
  intervals produce aggregate message/byte demand. Serial adds framing-aware baud
  utilization, marks >100% physically over capacity and >=80% low-margin, but stays
  advisory for deliberate overload tests. After 20 paired samples, recent (or
  cumulative for slow schedules) render/send p99 bounds estimate application
  headroom with explicit buffering, burst, stale-draft, and non-joint-p99 caveats.
- [x] **Run summary and export (ADR-036).** Each completed runner emits one exact,
  self-contained summary after its final counters. The supervisor retains the newest
  process-unique run per channel across ordinary restarts, and the selected-channel
  GUI offers a collapsed final readout plus an on-click, versioned clipboard report
  with times, outcomes, timer/timing, and platform/build facts.
- [x] **Per-message timing and measured blame (ADR-045).** The runner records
  per-message lateness, render, and send-call histograms beside the existing
  `per_message_counts`, and charges each deadline's delay to whichever send held
  the channel thread when it passed — a backlog stays charged to the send that
  opened it, and an idle-thread wake delay is charged to nobody. Surfaced as a
  collapsed per-message table and as positionally-aligned `per_message_*` keys in
  the clipboard report. Per-message *miss* counts are refused by decision: skips
  accrue as `late / interval + 1`, so they name the victim.

## Review disposition — per-message blame and warm-up (2026-08-01)

Applied: see ADR-045 and the commits it names. (The previous inventory here
listed a Render column among the changes; `MessageRow` has none and the spec
asks for none — a restated UI schema nobody rechecked, which is why this section
now points at its ADR instead of duplicating it.)

Not applied, deliberately:

- [x] **Attribute misses at the moment they occur (ADR-051).** Done 2026-08-06.
  The deepest point in the review. `Schedule::poll` reports `skipped` and
  `interval` on `Tick::Due`, and `MessageTimingRecorder::record_skips` charges
  those points to whichever send held the thread as each one passed — the same
  rule `blocked_others` uses, now shared as `blocker_for`. Surfaces as
  `MessageTiming::missed_others`, the `per_message_missed_others` report lane,
  and the **Cost to others** column (was *Delay caused*). The missed-send
  callout gained its one convicting branch, which states the unattributed
  remainder rather than absorbing it. Pinned by
  `miss_blame_scales_with_overload_where_delay_blame_thins_out` (the defect
  itself: a longer block cannot yield more lateness samples),
  `an_enormous_stall_is_charged_without_visiting_each_point` (closed form, not a
  loop), and `charging_skips_does_not_disinherit_the_deadline_handled_next`
  (the skip lookup must not advance the backlog cursor).
- [ ] **Only the interface write counts as holding the channel.** The boundary
  ADR-051 states rather than fixes: a point is charged to whichever message was
  inside `send` when it passed, so time spent rendering is charged to nobody.
  Justified today because render is 1–3 µs and below the histogram's first
  bucket — check it again if a payload format ever makes rendering expensive,
  since the fix is just widening the recorded window.
- [ ] **Cache the Cadence grouping and its tooltip.** `cadence_groups` allocates
  and `cadence_tooltip` builds a ~1 kB string every repaint, hovered or not.
  Caching needs either app-side state keyed on the interval set, or a
  `wiredata-ui` change so `signal_row` takes a tooltip closure evaluated only on
  hover. The second is the better shape and touches shared chrome, so it belongs
  with the next chrome pass rather than bolted on here.
- [ ] **`percent` and `compact_duration` mark nothing as approximate.** Rejected
  for `compact_duration` — the maximum is exact and hedging every rounded value
  would put a qualifier on every number in the pane. Revisit only if a rounded
  figure is ever shown next to a threshold that reads off it.

## Cadence rework follow-ups (2026-07-31)

Left behind by the ADR-045 per-message work and the Cadence rewording that
preceded it. The first two are cross-crate consistency debts, not local cleanups.

- [x] **The shared readout vocabulary no longer describes Talker's Cadence row.**
  `wiredata-ui/src/diagnostics.rs` documents the state names both apps must share
  (`warm-up (N) · max X`, `p99 ≤ X`, `no recent <noun>s · run max X`,
  `awaiting first <noun>`). `cadence_decision` (now in
  `talker/src/gui/diagnostics/cadence.rs`, after the 2026-08-07 split)
  renders them in plain language instead — `worst send started X late of N
  sends`, `nothing sent in <window> · worst this run X`, `awaiting the first
  scheduled send`; the warm-up form is gone entirely (ADR-046). Talker's own split
  closed first (`timing_metric` and the Cadence row share one rule), and
  the shared table in `wiredata-ui/src/diagnostics.rs` now carries a real
  rendering from **both** apps per state, so a wording change one side cannot
  describe is a divergence rather than a variation.
- [x] **Retire Listener's warm-up gate (ADR-046 → listener ADR-037).** Done
  2026-08-05. `TIMING_WARMUP_SAMPLES` is gone from
  `listener/src/gui/detail/diagnostics.rs`; `timing_figures` states the worst
  value and adds `99% ≤ X` only when `p99 < max`. Listener's nouns came with it —
  Handoff/Processing count chunks, Idle-rule lateness counts firings (and says
  so, where it used to print a bare integer), read gaps report the *longest*
  rather than the worst because a quiet source is not a fault, and chunk sizes
  add a median only when reads actually varied. The three tooltips that taught
  the retired state are rewritten, pinned by
  `no_readout_teaches_the_retired_warm_up_state`.
  `MIN_SERVICE_SAMPLES` (`talker/src/core/capacity.rs`) was **not** part of this:
  it gates the headroom projection, which genuinely needs samples, and merely
  shares the value 20.
  `timing_figures` is deliberately duplicated in both apps — it reads a
  `wiredata-telemetry` histogram and `wiredata-ui` depends on `egui` alone. If
  that ever earns sharing, it takes two `Duration`s, not a histogram.
- [x] **`ActiveCadence::longest` / `is_uniform` dropped.** They existed to render
  an interval span that grouped per-message intervals now render better, leaving
  them to serve only the sub-second window between a channel's first
  `TimerStatus` and its first `Counters` — not worth a second source of the
  distribution. `ChannelDemand::longest_interval` went with them; that fallback
  now reads `N messages, shortest X`.
- [x] **The Cadence line named the shortest interval twice.** `relative_to_shortest`
  now reads `(2.0% of the shortest interval)`: the ratio stays, the restated
  duration goes, since the schedule phrase leads the same line with its groups
  sorted shortest-first. Its `upper_bound` flag went with it — once the warm-up
  gate was retired (ADR-046) the percentage only ever qualifies an exact maximum,
  never a bucket bound, so all three call sites passed `false`.
- [x] **Missed-sends routing built** (`missed_send_routing`). One callout, shown
  only when cadence points have been skipped, naming where the cause lies in
  decisiveness order: an erroring interface, a physically oversubscribed serial
  line, the message whose sends blocked the others, application headroom, then
  the render/send-call timing. It routes rather than reports — the only figure it
  restates is the blocking message's, whose table is collapsed by default.
  *(Merged into the Listener migration item above: it and the vocabulary pass are
  the same edit to the same rows, and splitting them would reword Listener's
  diagnostics twice.)*

## Workspace items (external review, 2026-07-11)

- [x] **Criterion benchmark harness** — landed: `talker/benches/scheduler.rs`
  (poll idle-scan at 8/64/512 messages, due-and-render incl. the per-send payload
  clone at 64 B/1 KiB, `active_cadence`) and `listener/benches/pipeline.rs`
  (64-byte ingest floor, steady-state at the scrollback cap, BytePattern rule
  scaling at 1/8/32). `cargo bench -p talker` / `-p listener`; smoke-tested via
  `cargo bench -- --test`; clippy covers them via `--all-targets`. These are
  the baselines gating every "(behind benchmarks)" item here and in the
  listener TODO — measure before optimizing.
- [x] **Soak tests (the harness's second half)** — landed as `#[ignore]`d
  integration tests, invoked manually / nightly (not the per-push gate):
  `cargo test -p talker --test soak -- --ignored` and the listener sibling;
  duration via `WIREDATA_SOAK_SECS` (default 10 s).
  - `multi_channel_udp_soak_totals_exact_at_rest`: 4 channels × 1,000
    sends/s over real loopback UDP; telemetry == wire exactly at rest.
  - `tcp_failed_send_storm_stays_bounded`: ~500 fires/s against a dead peer
    for the whole window → ONE edge-triggered error, live runner,
    deliverable Stop.
  - listener `sustained_recording_records_every_byte`: ~64 KB/s recorded to
    a real file; sent == retained activity == `.raw` bytes, recording queue
    never near the cap.
  - Still open (needs an injectable slow writer, not just a real disk):
    **slow-disk recording with rotation** — fold into whatever next touches
    the recorder task's writer seam.
  - [ ] **Soak the Serial stall cell under sustained backpressure** (listener
    `SerialStallState` / `retry_stalled_send`, ADR-034). Unit tests cover the
    episode edges, poison recovery, and the unwind guard, but nothing drives a
    real reader against a Transport→Pipeline queue that stays full for a whole
    window. Hold a bounded ingest receiver without draining it, run the
    `BlockingReader` seam at rate, and assert: `completed_episodes` only ever
    grows, `completed_total` never exceeds wall time, `active_for` is `Some`
    while stalled and `None` within one snapshot of resuming, and the final
    snapshot has no active episode. This is the path where a missed edge shows
    up as a stall duration that grows forever, so a long window is the point.
  - [ ] **Soak the fast-then-dormant snapshot expiry** (talker
    `RecentSnapshotState` / `recent_snapshot_state`, ADR-042). The
    classification is unit-tested and the GUI's consumption is tested against
    synthetic states, but nothing exercises the real transition end to end:
    run a channel fast enough to warm the recent window past
    `MIN_SERVICE_SAMPLES`, make every message dormant, then hold past
    `RECENT_WINDOW` and assert the supervisor's retained snapshot classifies
    Expired, that measured headroom stops consuming it and falls back to
    labelled run-wide timing, and — the reason the heartbeat was rejected —
    that the dormant runner emitted no further `Counters` in that window.
- [ ] **`wiredata-display` extraction — only together with talker adopting the
  incremental renderer** (row-ring item above): the protocol-neutral
  Raw/Rendered/Hex stream machinery could move to an egui-free shared crate so
  talker reuses listener's incremental rendering. Standalone extraction is
  speculative crate surface — don't do it first.

## Week-in-review cleanup (2026-07-16)

- [ ] **Split `talker/src/gui/widgets.rs`** (~2.5 k lines after the GUI-merge
  week) into a `widgets/` directory of focused modules mirroring listener's
  shape (`status`, `summary`/blockers, the field editors, text-edit helpers).
  Mechanical move, re-exported flat so call sites keep `widgets::<name>` — do
  it as a standalone commit (blame churn), not bundled with feature work.
- [x] **Per-frame telemetry clone removed** — `TalkerSupervisor::telemetry_ref`
  borrows for the row snapshot / rate sampler / status bar; the owning
  `telemetry()` clone remains for the detail header.
- [x] **Start blockers no longer format per frame** —
  `widgets::any_start_blocker_analyzed` (lazy `visit_start_blockers` sink)
  gates Start-all; strings only materialize for the disabled tooltip via
  `start_blockers_analyzed`.
- [x] **Listener mark-history hash gated** — `ChannelView::marks_version`
  bumps on every mark mutation; `StreamRenderCache` compares versions and
  only re-hashes (`marks_signature_below`) when marks actually changed.
- [x] **Chrome dedup into `wiredata-ui`** — `palette::active(ui)` (replaces
  talker's `theme_palette`, listener's `gui/theme.rs` global mirror, and
  selection's private copy), `style::theme_toggle_button`,
  `selection::{severity_counts_line, last_error_line}`, `install_chrome`;
  listener's `gui/fonts.rs` + `widgets/format.rs` re-export shims deleted.

### External review fixes (2026-07-16)

- [x] **Listener row-cache unbounded growth** — count-triggered batch merging
  advanced the front batch's end in lockstep with the window start, so with
  more deltas per retained window than `MAX_ROW_BATCHES` no batch ever became
  evictable and `rows` grew for the channel's lifetime. Fixed by byte-quantum
  coalescing (`row_batch_quantum`, batch ends freeze); pinned by
  `tiny_deltas_with_a_sliding_window_keep_rows_bounded`.
- [x] **Orphaned talker runner wedge** — `poll` never drained an orphan's
  status lane, so the runner's blocking final-Counters send could wedge on a
  full queue and hold the interface until exit. Orphans now drain-and-discard
  status; pinned by `removal_with_a_saturated_status_queue_still_reaps_the_orphan`.
- [x] **TCP acceptor faults surfaced (§162)** — a spontaneously ending
  acceptor now sets the channel's `faulted` flag and emits `ChannelFaulted`
  (was: silent `listener_open = false`); pinned by
  `acceptor_fault_sets_faulted_and_emits_channel_faulted`.
- [x] **Recorder stops off the acquisition loop** — listener ADR-022; pinned
  by `recorder_stop_retires_detached_and_still_reports_the_outcome`.
- [ ] **Acceptor fault CAUSE into the listener channel's retained
  diagnostics** — the fault flag + event land (§162), but the cause string
  only reaches the log via `tracing`; route it into `retained_diagnostics`
  like start faults (needs a supervisor→orchestrator path).

## macOS target (planned, 2026-07-10)

- [ ] **App Nap opt-out in `core::timing` (ADR-017/ADR-034 counterpart).** macOS timers
  are sub-ms (no `timeBeginPeriod` analog needed), but App Nap throttles the
  timers of hidden/occluded apps — the macOS analog of the Windows 11 timer
  throttling we opt out of. Implement `raise()`/`lower()` for
  `cfg(target_os = "macos")`: hold an `NSProcessInfo`
  `beginActivityWithOptions(NSActivityLatencyCritical |
  NSActivityUserInitiated, reason)` token for active high-rate work and measure
  whether Precise final windows also need it; today's non-Windows Precise path keeps
  one native wait. End the token when the protected activity ends. Needs
  `objc2`/`objc2-foundation` as a
  `cfg(target_os = "macos")` dependency. The refcount plumbing
  (`ResolutionCounter`) is platform-neutral and already in place.
- [ ] **Platform pass.** Verify `serialport` enumeration on macOS (ports are
  `/dev/cu.*`; prefer `cu` over `tty` devices), eframe/winit windowing, and
  that all `windows-sys` usage stays `cfg(windows)`-gated. Fonts are bundled,
  so no font work expected.

## When writing the project README

- [ ] Document the system packages required on Linux for `eframe` (`libxcb`, `libxkbcommon`, etc.) per ADR-003 consequences.
- [ ] Document MSRV and the `rustup update stable` requirement per ADR-008.
- [ ] **High-rate timing section (user doc)** — from the 2026-07-10 timing
  discussion; ADR-017 has the design rationale, this is the user-facing telling:
  - Windows wakes sleeps on a 15.625 ms tick; talker auto-requests 1 ms
    resolution while any schedule has an interval < 32 ms (`core::timing`,
    no elevation needed, released when the last fast channel stops, cleaned
    up by the OS even on a kill).
  - A channel set to Precise uses the same continuous Windows policy below 32 ms,
    then requests it only for the final 32 ms at 32 ms or longer. Standard is the
    default. Precise does not align sends to wall-clock boundaries; on macOS/Linux it
    keeps the native one-stage wait because there is no equivalent timer-resolution
    request.
  - What **Missed sends** means: one missed send = one message transmission
    skipped under the stall policy ("fire once, skip the backlog, stay on
    grid" — cadence over count); `should-have-fired = sent + missed`;
    per-channel total across all messages in the schedule.
  - **Status queue / Display updates dropped** vs **Missed sends**: the first
    two affect only what the Output pane shows (drop-and-count, sends never
    delayed); missed sends means the wire cadence itself broke.
  - Practical rate ceilings: ~100 Hz clean out of the box (post-ADR-017);
    ~500 Hz–1 kHz is wake-quantization territory (hybrid spin-wait was
    considered and deliberately not built — one pegged core per fast channel,
    worse when oversubscribed; revisit only on a real ≥1 kHz UDP need).
    **Measured 2026-07-13 (release, one dev Win11 box), UDP loopback:**
    500 Hz (2 ms) missed ~1.5% of grid points — essentially on-target;
    1 kHz (1 ms) missed ~⅓ of grid points (~667 Hz effective) — the 1 ms
    send grid sits right at the ~1 ms Windows timer granularity, so wake
    jitter overshoots and the stall policy skips. This is the documented
    ceiling, not a regression; CPU/observer cost is not the bottleneck
    (timer resolution is), so only the deferred spin-wait would move it.
  - Serial line-rate math: `payload_bytes × 10 / baud` must fit the interval
    (a 40-byte sentence at 115200 baud ≈ 3.5 ms → 1 kHz is physically
    impossible regardless of timers).
  - Minimized windows: talker opts out of Windows 11 timer throttling at
    startup, so minimized long soaks keep cadence; macOS will need the App
    Nap equivalent (see "macOS target" above).

## Future work — out of scope for spec v2.0

- **AIS as a sendable `talker` payload.** The `nmea0183` crate already builds and parses `!AIVDM`/`!AIVDO` and armors the 6-bit payload, but spec v2.0 §5.1 lists exactly five message formats and AIS is not one of them. Exposing AIS in the `talker` message editor — whether as pre-armored raw bytes or as a structured per-message-type editor (Type 1/5/18/24…) — is a feature beyond the current spec. Revisit only with a spec amendment. See the ADR-012 context note and the 2026-05-22 discussion.

- **Manual transmit / inject for troubleshooting.** Captured here because transmit is `talker`'s job, not `listener`'s (listener is strictly receive-side). Beyond talker's scheduled/profile-driven sends, a troubleshooting workflow wants **ad-hoc, one-shot injection** — type or pick a payload and fire it once at a serial port or network endpoint to provoke a device, while `listener` observes the response on the same or another channel. This is the natural talker counterpart to the listener troubleshooting use case (see the listener "primary use cases" notes, 2026-06). A real feature here needs a spec amendment: define how one-shot/manual sends relate to the §5.1 message formats and the scheduler, and whether it's CLI, GUI, or both. Revisit only with that amendment.

---

## Completed during the spec v2.0 upgrade

The sections below were open in TODO v1.0 and are now done; kept here so the history is not lost.

- **`core::logging`** — dual-mode logging is implemented: CLI destinations are
  fixed at launch; the GUI has five independent pane filters and can start or stop
  its rotating file destination during the session without doing file I/O on the
  UI or send threads.
- **`core::profile`** — schema v2 with `CURRENT_VERSION` checked on every load; `#[serde(default)]` on all fields; `#[non_exhaustive]` on profile enums. OQ-2 (`toml = "1"` is sufficient) and OQ-3 (profiles use a `talker`-side NMEA representation, so the `nmea0183` `serde` feature stays off) are resolved — see the Open questions section of the ADR.
