# Talker — documentation revision history

Superseded revision notes for
[talker_specification.md](talker_specification.md) and [ADR.md](ADR.md), newest first.

They are kept out of the specification itself so that document states **current
behaviour** and carries its version in one place. Each entry records what changed
and why at the time it changed; where a decision stands behind it, the ADR named
in the entry is the authority. Nothing here is normative — if this disagrees with
the specification, the specification is right and this is history.

---

## Specification

Revision note (2026-09-30) — unattended operation and protocol-correct
checksums:

- **§3.1 unattended CLI (ADR-060)** — the CLI starts the channels that open,
  retries the others, warns loudly while any is down, stops gracefully on every
  OS stop signal, and exits with defined codes. `--require-all` keeps
  all-or-nothing.
- **§4.5 TCP client (ADR-059)** — reconnects at retry points and never resends
  the failed message; replies from the peer are drained and counted.
- **§3.2 / §4.4 possibly partial (ADR-059)** — a write that fails after
  transferring bytes is its own send outcome, for serial and TCP.
- **§7 checksums (ADR-057)** — "CRC-16/CCITT" is named for what it computes,
  CRC-16/KERMIT, stored as `crc16_kermit`. MODBUS is appended low byte first.
- **§8.2 profiles (ADR-057, ADR-062)** — schema 3; unknown keys and a missing
  `version` are refused. The illustrative example gives way to the real
  `profiles/profile.example.toml`.
- **Drift corrected** — §8.1 and §8.2 no longer describe the Standard/Precise
  timing mode that ADR-047 removed. §12.1 drops TCP reconnect, now decided.

Revision note (2026-08-12) — both GUI log destinations now disclose when they
cannot keep up:

- **§4.4 / §9.2 pane completeness** — a persistent,
  session-cumulative notice reports entries that did not reach the Log pane.
  Those entries are absent from its retained history and its per-channel
  INFO/WARN/ERROR tallies, while saved files remain an independent destination.
- **§9.2 retained history and saved gaps** — all five levels share one
  newest-2,000-entry history before pane visibility is applied. File loss
  remains non-blocking and session-counted; after entries accepted ahead of a
  loss drain, or when the enabled destination closes, the worker attempts a
  plain gap line for the omitted batch. The visible queue-loss count remains
  available if the destination can no longer be written.
- **§9.2 file access and limits** — GUI logging uses a fixed local-data
  directory when the platform provides one, rotates daily, deletes no old
  files, and adds a
  non-blocking **Open folder** action that neither enables logging nor shares
  the file worker. Folder failures remain visible in the Log pane.

Revision note (2026-08-11) — logging controls and channel fault state now say
exactly what is current, what is retained, and what is saved:

- **§3.1 / §9.2 logging (ADR-006, corrected)** — the GUI exposes all five
  severities, separates collection from pane visibility, and offers a
  session-local file toggle backed by non-blocking file work and visible failure
  reporting. CLI logging remains launch policy from the profile; neither mode
  reads `RUST_LOG`.
- **§3.2 / §4.4 current fault versus run history (ADR-056)** — a channel row
  shows the unresolved command or interface problem now. Successful sending
  clears the current interface fault without erasing this run's Send outcomes
  or log counts; command faults clear only through their own success path.
- **§5.7 / §8.1 repair after a dropped live update (ADR-018/056)** — periodic
  and final counters repeat the active send/reopen fault and the cumulative
  failure-episode count, so a dropped failure or recovery edge is repaired
  without a heartbeat or a reliable send-path observer queue.

Revision note (2026-08-10) — a replaced serial device can recover in the same
run without broadening network recovery:

- **§4.2 running serial recovery (ADR-055)** — after a running serial write
  reports an unusable handle, Talker closes it before reopening the same
  configured port. Known transient flow-control results retry the existing
  handle; device, permission, operation-aborted, and unclassified errors replace
  it. Initial-open failure, a replacement that returns under another port name,
  and uncertainty after a partial write remain explicit boundaries.
- **§9.2 retry accounting (ADR-055)** — reopening is attempted at the next due
  send allowed by bounded backoff. An unavailable replacement withholds that
  send without rendering or entering failed-write and send-call timing totals.
  The single recovery log edge names the port when sending recovers on an
  automatically reopened handle. The five-second backoff cap limits retry
  frequency; it is not a recovery-time promise.
- **§12.1 deferred TCP reconnect** — an established TCP stream that fails still
  uses bounded retries without creating a new connection. Automatic reconnect
  remains open until its partial-write, replay, and retry-accounting boundaries
  receive a separate decision.

Revision note (2026-08-10) — diagnostic guidance now speaks from the
technician's side of the screen:

- **§3.2 current timing (ADR-042/046)** — “recent snapshot” is replaced by the
  period it describes: `last ~10 s`, with update age when useful, an unavailable
  state when stale, and `final ~10 s before stop` at a clean run end. Cadence
  keeps that period on its main line and moves its sample count into the tooltip,
  paired with the current, final, or run-wide period that supplied the figure.
- **§3.2 stable timing figures (ADR-046)** — Cadence, render, and send-call
  readouts lead with their longest observation; a distinct percentile follows
  in the same qualifier. Its appearance can no longer move the main figure under
  the reader.
- **§3.2 missed-send result (ADR-051)** — the amount-stating branch describes
  the observed overlap directly: Talker was waiting for another message's serial
  or network send to finish when the misses occurred. The screen no longer asks
  a technician to translate “charged” before following the result.
- **§3.2 evidence limits (ADR-051)** — four labelled points distinguish a
  measured missed-send count from a message offered only as a delay-based lead,
  explain what an unmatched miss does and does not mean, and separate direct
  findings from earlier failures and estimated application headroom. The
  history-sizing proof remains in ADR-051 rather than appearing in operational
  guidance.
- **§3.2 / §5.7 live updates (ADR-052)** — the live-update queue moves beside
  its dropped-update warning above Output. Its tooltip states that drops can
  also affect diagnostic readouts. The queue and sampling lines share the same
  quiet treatment; only the active drop warning carries urgency.
- **§4.2 absent serial devices** — Windows' known filesystem-oriented file/path
  not-found messages now read `no device is present at <port>`. The broader
  serial error category also covers busy and access-denied ports, so every other
  operating-system message remains unchanged.

Revision note (2026-08-09) — three readouts stop claiming more than they measure:

- **§3.2 uncharged misses (ADR-051, corrected)** — the remainder no longer offers
  "the send that held it has aged out of the retained record" as an explanation.
  The retained history is sized from the schedule, so a write that could have
  spanned a point has not been forgotten. What the record supports is one
  negative fact — no measured interface write spanned those points — which a late
  deadline wake, work outside the send call, and a genuinely free thread all
  produce alike.
- **§5.7 completeness notices (ADR-052)** — while the dropped-update warning is
  unacknowledged, the **Output** section header reads `Output ⚠ <n> dropped`.
  The pane is collapsed by default and the warning no longer raises the
  diagnostics card's badge, so without the marked header the condition could be
  on screen and unseeable.
- **§9.2 edge-triggered channel conditions (ADR-053)** — the five-second settle
  is a **minimum, not a deadline**: it is judged on a cadence point the channel
  reaches, so a slow schedule reports at its next send. The table no longer
  claims all three conditions close — discarded updates are a run total with no
  recovery edge to report, which is now stated rather than left as a dash.

Revision note (2026-08-09) — a warning may be acknowledged, and is raised where
it lands:

- **§3.2 dismissible warnings (ADR-052)** — the missed-send routing callout and
  the Output pane's dropped-update warning each carry a **Dismiss** button.
  Dismissal records the run counter behind the warning, which returns when that
  counter is exceeded, so the reader is told once per occurrence and a spreading
  fault is never silent. A counter below the record means a new run and discards
  it. Dismissing the routing hides advice, never the counts: the misses stay on
  the send-outcomes line and keep the card's badge raised.
- **§3.2 / §5.7 completeness notices (ADR-052)** — the dropped-update warning
  moves out of the diagnostics card into the **Output** pane, above the sampling
  note, because what those drops cost is that pane's completeness rather than
  anything about the wire. The card's badge no longer rises for them; the queue
  gauge stays under Timing & runtime details. The sampling note keeps its calm
  treatment but is set in a stronger weight, since it qualifies every line
  beneath it.
- **§9.2 edge-triggered channel conditions (ADR-053)** — a channel falling off
  its send schedule is now logged: WARN on the first skipped send, INFO once
  five seconds pass with none skipped, carrying the episode's total, and the
  run's own total if it stops mid-episode. Missed sends previously appeared only
  as a live counter, so the log recorded connection loss but never cadence loss,
  and CLI mode had no account of them at all. The dropped-update warning was
  reworded in the same pass to state what it costs the reader rather than the
  queue behind it.
- **§9.2 who the log is written for (ADR-054)** — a log line states what
  happened to the reader's channel, in the vocabulary the screen uses. The
  structured `channel` field is a claim that the event is about that channel's
  operation, and it is what raises the channel row's warning badge; talker's own
  bookkeeping faults therefore no longer carry it, name themselves as internal,
  say what they cost, and are rate limited on a decade cadence.

Revision note (2026-08-06) — a missed send names its cause, honestly:

- **§3.2 missed-send routing (ADR-051, corrected)** — the amount-stating branch
  must close its own arithmetic: charged total, largest single share, and
  remainder. Naming only the largest culprit dropped every other charged message
  out of the sentence. The remainder is stated as **not charged to any send** and
  never as an idle channel — a miss goes uncharged both when the thread was free
  and when the send that held it has aged out of the retained record, and nothing
  measured can tell those apart.

Revision note (2026-08-06) — a missed send names its cause:

- **§3.2 missed-send routing (ADR-051)** — one branch of the callout may now
  convict rather than route: misses charged at the moment they were skipped are
  evidence about the misses themselves, so that branch states an amount and
  names a message. It must also state the share no send accounts for. Every
  other branch keeps its hedged verbs and its old justification.
- **§3.2 per-message timing (ADR-051)** — **Delay caused** becomes **Cost to
  others** and carries both currencies with their nouns attached
  (`430 ms late · 37 missed`) rather than taking an eighth column. A
  victim-side miss count is still refused for ADR-045's reason; the count now
  shown is the culprit's, and unlike the delay figures beside it, it does not
  thin out under heavy overload.

Revision note (2026-07-31) — no warm-up state; skipped sends name a cause:

- **§3.2 Cadence (ADR-046)** — the twenty-sample warm-up state is removed. Below
  a hundred samples the p99 bucket *is* the maximum's bucket, so the gate only
  relabelled the same number. One measured form now serves every sample count:
  the worst delay with its sample count, plus a percentile where that differs.
  The percentage no longer restates the shortest interval the same line already
  names.
- **§3.2 missed-send routing (ADR-045)** — skipped cadence points get one callout
  suggesting where to look, in decisiveness order, rather than restating a count
  already on the send-outcomes line. It routes rather than convicts: run totals
  are described in the past tense, and blocking evidence is qualified because it
  comes from deadlines the channel reached rather than the ones it skipped.
- **§3.2 Capacity describes what is running (ADR-035, amended)** — demand comes
  from the running schedule's own wire sizes and intervals, and the serial verdict
  from the interface the runner confirmed open. A channel that is not running is
  projected from the settings shown and labelled a projection. Unapplied edits can
  no longer produce a capacity verdict about a schedule that is not sending, which
  also removes the qualifier the missed-send routing previously needed.
- **§3.2 per-message blame is two figures, not one (ADR-045)** — **longest
  block** is an elapsed hold; **delay caused** sums the waiting imposed across
  every message displaced and can exceed the send that caused it. Presenting the
  sum as a hold time was arithmetically impossible.
- **§3.2 one count per line** — a readout that already names a sample count does
  not repeat it beside every figure; a boundary states its own only where its
  population differs.
- **§3.2 no render column** — payload construction sits below the first histogram
  bucket that would report it, so it stays in the clipboard report and the
  channel-wide work line.
- **§3.2 Work per send** replaces the *Timing health* line, which duplicated the
  Cadence row's deadline measurement; what remains is the only channel-wide view
  of render and the send call as they are now.
- **§3.2 an unfinished message edit** reads "Finish message setup to calculate
  cadence" rather than "No messages sending", which contradicted the Capacity row
  one line above.
- **§3.2 percentages over a limit** read `>100%`, since flooring alone rendered
  100.4% as "100%" beside an alert that fired for exceeding it.
- **§3.2 / §8.1 the Standard/Precise choice is removed (ADR-047)** — the
  deadline-wait policy follows the shortest active interval, and nothing replaces
  the control in the editor. The choice was inert below 32 ms and on platforms
  without a timer-resolution request; a read-only preview of the derived policy
  was rejected too, since its outcome does not vary between the bands. Profiles
  carrying `timing_mode` still load; the field is ignored. The clipboard report
  drops `timing_mode`, keeping `timer_policy` and `timer_reason`, which state
  what actually applied.

Previous revision note (per-message cadence and measured blame):

- **§3.2 Cadence (ADR-045)** — the row leads with the schedule, grouped by
  distinct interval, so a pooled lateness figure is never read as one message's
  behaviour. Wording is stated for a reader who has not seen the source.
- **§3.2 per-message timing (ADR-045)** — a new collapsed table places what each
  message suffered beside what its own sends cost the others. The blame column
  is measured, not inferred from send duration. Per-message miss counts are
  deliberately not offered, since skips concentrate on the shortest interval and
  would name the victim.
- **§3.2 clipboard report (ADR-045)** — new `per_message_*` keys, positionally
  aligned with the existing `per_message_sent` lane.

Revision v2.4.5 (send vocabulary and single-rendering outcomes):

- **§3.2 detail-pane wording (ADR-044)** — *interface* is the term for a
  configured serial/UDP/TCP endpoint, so the editor section is **Configure
  interface**; *connection* is reserved for Listener's accepted TCP peer
  sessions. The send readout is **Send outcomes**, since it counts scheduled
  sends rather than the interface itself.
- **§3.2 detail-pane layout (ADR-044)** — the counted outcomes render in exactly
  one place, on an always-visible line beneath the `status · interface` row,
  followed by an `Accepted:` line carrying the cumulative byte total and the
  rolling five-second rates. The diagnostics card keeps only Cadence and
  Capacity and raises no unsent callout, though the send-outcome tone still
  escalates its badge. `unsent` is shown as the aggregate of `failed +
  suppressed + missed` with its components parenthesised. This corrects a §3.2
  bullet that still described a *Wire facts* grouping the GUI had already
  replaced.
- Byte rates scale by SI unit and read `0.0 B/s` at rest. Profiles, profile
  schema `version` 2, clipboard-report keys, wire output, and cadence are
  unchanged.

Revision v2.4.4 (truthful pushed-snapshot freshness):

- **§3.2 / §8.1 timing snapshot provenance (ADR-042)** — every pushed
  counter/timing snapshot now carries its exact monotonic compute instant and
  whether it is the runner's mandatory final snapshot. Capacity, Cadence, and
  detailed Timing share one freshness classification: a non-final recent snapshot
  expires once its age reaches the bounded recent window, while a final snapshot
  remains final-at-run-end evidence. An expired recent snapshot cannot drive measured
  headroom; a warmed run-wide fallback is labelled explicitly. Dormant runners
  retain their indefinite zero-wakeup command wait—no telemetry heartbeat is added.
  Profiles, clipboard-report format, and wire output are unchanged.

Revision v2.4.3 (shared telemetry and explicit timer reconciliation):

- **§2.1 / §3.2 / §8.1 bounded telemetry (ADR-039)** — the workspace adds the
  dependency-free internal `wiredata-telemetry` crate for the fixed duration
  buckets and bounded ten-segment recent-window engine shared by Talker and
  Listener. Talker's send-specific aggregates, measurement boundaries, capacity
  interpretation, completed-run retention, and presentation remain local.
- **§3.2 / §8.1 timer reconciliation (ADR-040)** — a runner-local
  `TimerReconciler` now owns guard transitions and timer-status reconciliation.
  The established Windows policy remains unchanged: any interval below 32 ms holds
  the shared 1 ms request continuously, while Precise uses final 32 ms windows at
  32 ms or longer. Standard mode and non-Windows waits are unchanged. Profiles and
  wire formats are unchanged.

Revision v2.4.2 (optional UTC phase alignment and shared timer policy):

- **§3.2 / §8.1 cadence phase (ADR-037)** — each channel may retain the default
  immediate first send or wait for every active message's strict next
  Unix-epoch-modulo interval boundary. After that anchor, cadence remains monotonic.
  A wall-clock displacement of at least 250 ms is checked at most once per second
  and rephases only future deadlines; it never replays elapsed wall-clock points.
- **§8.2 process timer policy (ADR-038)** — Talker and Listener now share the small
  `wiredata-timing` platform boundary for refcounted Windows 1 ms requests and the
  minimized-window throttling opt-out. Linux and macOS retain native deadline waits
  and make no analogous resolution request.
- Alignment is independent of Standard/Precise wake policy and does not establish
  physical wire time. The additive channel field defaults to immediate and is omitted
  in that state, so profile schema `version` 2 remains unchanged.

Revision v2.4.1 (retained completed-run summary and clipboard report):

- **§3.2 / §8.1 completed-run summary (ADR-036)** — after a send loop ends,
  Talker retains the newest exact summary for that stable channel slot across
  ordinary stop/restart cycles. It includes wall-clock start/finish, monotonic
  elapsed duration, final sent/failed/suppressed/missed outcomes, per-message
  counts, observer drops, bounded timing, timer policy, and build/platform facts.
- The selected-channel GUI exposes the summary in a collapsed block and creates a
  versioned line-oriented clipboard report only when **Copy summary** is clicked.
  Process-unique run ordering prevents a late predecessor tail from replacing a
  newer completion. Retention is process-local, fresh profile slots clear it, and
  profiles, wire bytes, scheduler cadence, and profile schema `version` 2 are
  unchanged.

Revision v2.4 (capacity preflight and measured application headroom):

- **§3.2 / §8.1 capacity preflight (ADR-035)** — the selected channel reports
  current-draft message and byte rates from exact compiled wire lengths. Serial
  channels also report UART utilization using one start bit plus the configured
  data, parity, and stop bits for every wire byte. Sustained demand above 100% is
  identified as physically over capacity; 80% and above is highlighted as low
  practical margin. The warning is advisory, so intentional overload tests remain
  possible.
- After 20 paired observations, Talker compares the draft rate with the sum of the
  separate render and synchronous-send p99 histogram upper bounds. Recent data is
  preferred and cumulative run data supports slow schedules. This is not a joint
  p99, a physical-wire measurement, or a hard real-time guarantee. Capacity uses
  existing memoized preview lengths and adds no per-frame payload render. Profiles
  and profile schema `version` 2 are unchanged.

Revision v2.3 (explicit precision timing and deadline-window timer resolution):

- **§3.2 / §8.1 / §8.2 channel timing (ADR-034)** — each channel has a
  `Standard` or `Precise` timing mode. Standard preserves the automatic continuous
  Windows 1 ms timer request for active intervals below 32 ms. On a slower Precise
  schedule, the runner requests 1 ms resolution only for the final 32 ms before the
  next deadline and releases it before rendering and sending; dormant schedules hold
  no request. Commands remain able to interrupt every wait.
- Non-Windows targets keep one native deadline wait and make no Windows-style timer
  request. Precise improves Windows deadline-wake cadence; the timing mode alone does
  not choose wall-clock phase, improve the wall clock itself, or prove when bytes
  leave hardware. v2.4.2 adds phase as an independent option. The profile field is
  additive and defaults to Standard, so profile `version` remains 2.

Revision v2.2 (live UTC fields in NMEA payloads):

- **§5.1 / §6.4 live NMEA time (ADR-029)** — an NMEA message may replace the
  known time/date fields for its sentence type at every send. UTC time is
  `hhmmss` or `hhmmss.sss`; RMC/ZDA calendar fields use their protocol widths.
  Short typed field lists are extended, the internal NMEA checksum is regenerated,
  and unsupported sentence types fail preflight instead of silently staying static.
- The two profile fields are additive and default off; profile `version` remains 2.

Revision v2.1.1 (corrections and doc-alignment; no behavior change):

- **§8.1 scheduling** — the "Queue model" bullet no longer claims the scheduler
  "maintains a priority queue"; it tracks each message's next-fire-time
  (conceptually a priority queue — the v2.0.1/v2.1 clarification, now applied to
  the bullet itself). Implementation unchanged.
- **§5.1 / §5.2 ASCII payloads** — ASCII text is not "restricted" to the selected
  code page: characters the code page cannot encode compile to a visible `?`
  (`0x3F`) fallback byte, flagged in the editor and preview (ADR-023). The format
  table and §5.2 now state the shipped fallback behavior.
- **§3.2 GUI** — describes the master–detail detail pane as shipped after the
  GUI-merge harmonization and the ADR-018 telemetry split: the detail header's
  wire-facts and performance readouts, the lifecycle button pair, list-row
  contents, and the Profile menu's home in the channel-list header.

Revision v2.1 (master–detail GUI, named channels, shared chrome):

- **§2.2 GUI layout** — the stacked all-channels card view is replaced by a
  **master–detail** layout: a collapsible channel list on the left (per-row status
  glyph, name, interface summary, sent count + msgs/s, last error) and a detail pane
  for the **selected** channel (header with editable name and actions, Connection
  editor, Messages editor, Output display pane). `+ Add` (per interface kind) and
  Start all / Stop all live in the list header.
- **§2.1 / §6** — the workspace has a fourth crate, `wiredata-ui` (internal shared GUI
  chrome — fonts, palette, base style; talker ADR-016). Channels gain an optional
  display `name` in the profile schema — additive `#[serde(default)]`, omitted when
  empty, so older profiles load unchanged and unnamed channels serialize as before.
  Names are cosmetic (channels stay positional); duplicates are allowed but hinted.
- **§3.2 GUI state** — window geometry is **not** persisted anymore (the window opens
  at its default size; restoring geometry after the window is shown caused a visible
  double-flash). Zoom, theme, and the last profile path still persist.
- **§2.4 / §4.3 channel transport** — the `+ Add` menu chooses a channel's transport
  from the same UDP / TCP / Serial templates as listener. The transport is structural;
  the detail pane configures that transport's parameters but does not change its kind.

Revision v2.0.1 (corrections to match the implementation and its dependencies):

- **§4.2 serial parameters** — parity is `None, Even, Odd` and stop bits are `1, 2`.
  The `serialport` backend offers no Mark/Space parity and no 1.5 stop bits; the
  earlier list over-promised the dependency (neither ever worked). Corrected, not
  removed behavior.
- **§4.1 UDP Multicast** — now states the full configurable set: group address,
  port, outgoing interface, and TTL (all implemented).
- **§8.1 scheduling** — clarified that the "priority-queue scheduler" is a
  conceptual model, implemented as a linear next-fire scan (equivalent at the
  message counts talker handles).
- **§3.1 CLI** — single-channel ad-hoc invocation without a profile is marked as
  planned; the current CLI loads a profile.

No `schema_version` change: profiles are unaffected (the multicast `interface`/`ttl`
keys are additive `#[serde(default)]` fields — older profiles load unchanged).

---

## Architecture Decision Record

Revision note (2026-09-30) — decisions for unattended operation:

- **ADR-059** gives the TCP client reconnect at retry points, counts a write that
  fails after transferring bytes as possibly partial, and drains peer replies.
- **ADR-060** sets one CLI contract for both apps: start what can start, warn
  loudly, fixed exit codes, and a graceful stop on every OS stop signal.
- **ADR-061** moves talker's bounded log-file worker into a shared
  `wiredata-log` crate.
- **ADR-062** refuses unknown profile keys and a missing `version`.

Revision note (2026-09-30) — outer checksums match their protocols' conventions:

- **ADR-057** appends CRC-16/MODBUS low byte first, as MODBUS RTU carries it, and
  names the algorithm labelled "CRC-16/CCITT" for what it computes: KERMIT. The
  stored name changes, so the talker profile schema moves to 3.

Revision note (2026-08-12) — both GUI log routes now disclose observer loss:

- **ADR-006 (corrected)** gives the pane and file independent bounded,
  non-blocking loss accounts. Pane loss qualifies retained history and channel
  tallies; the file worker attempts a direct gap marker after earlier accepted
  entries, with enabled-session boundaries preventing reassignment.
- **ADR-006 (boundary added)** keeps one chronological pane history, a fixed GUI
  directory when platform local data is available, and time-only rotation.
  **Open folder** runs independently of the UI and file worker and never enables
  logging; directory choice, retention, and disk-use bounds remain outside this
  change.

Revision note (2026-08-11) — logging and channel health separate control,
history, and current state:

- **ADR-006 (corrected)** records five logging levels, the shared collection
  threshold, pane-only visibility switches, and the GUI's session-local
  bounded-queue file destination with visible loss/failure reporting and no
  retention promise.
- **ADR-010 (corrected)** keeps profiles and GUI state separate while naming
  only the state actually retained; runtime logging controls reset each launch.
- **ADR-018 / ADR-056** add current send/reopen state and a cumulative failure-
  episode count to counter snapshots. Immediate edges remain responsive; repeated
  counters repair dropped edges without erasing run outcomes or log history.

Revision note (2026-08-10) — serial device replacement gets a transport-specific
recovery boundary:

- **ADR-055** replaces an unusable running serial handle before the next eligible
  write, while preserving the existing edge-triggered failure episode and bounded
  backoff. It defines why known flow-control-shaped errors retain the handle while
  unknown operating-system errors replace it, why reopening stays outside render
  and send-call timing, and the limits around initial open, port renaming, retry
  latency, partial writes, the single enriched recovery log edge, and the
  separately deferred TCP reconnect policy.

Revision note (2026-08-10) — four existing decisions carry their evidence in
the reader's vocabulary:

- **ADR-042** keeps freshness provenance internal while presentation names the
  period it supports: last ~10 seconds, its useful update age, unavailable recent
  timing, or the final period before stop.
- **ADR-046** keeps the sample count but pairs Cadence's count with the current,
  final, or run-wide period that supplied its figure. Longest timing stays in one
  position and any distinct percentile follows it.
- **ADR-051** keeps a measured missed-send count distinct from a delay-based
  lead. The screen describes observed overlap instead of “charging”; the
  retained-history proof stays in the decision rather than operational help.
- **ADR-052** places the shared live-update queue gauge with its warning above
  Output while stating that drops can also affect diagnostic readouts. The gauge
  and standing sampling note are quiet context; the warning alone carries
  attention.

Revision note (2026-08-09) — three decisions state their boundaries, and stop
telling their own story:

- **ADR-052 (boundary added)** — placement by consequence has to reach the
  reader. Moving the dropped-update warning into the Output pane while removing
  the card badge put it inside a section that is collapsed by default, so the
  warning could be on screen and unseeable. A warning inside a collapsed section
  now marks that section's header.
- **ADR-053 (boundary added)** — the settle window is a **minimum, not a
  deadline**: recovery is judged where the skip count arrives, on a cadence
  point the channel reaches, so a slow schedule reports at its next send rather
  than at five seconds, and nothing is woken to announce its own recovery. The
  line is scoped to cadence alone — sends can be failing while every point is
  reached.
- **ADR-054 (corrected)** — the decade rate limit was applied by convention to
  five of the six internal faults, and the sixth, on the send path, was the one
  best placed to flood. The tally and the sentence shape now live in one module
  every caller reaches for.
- **All three, compressed.** They carried the debugging story, superseded
  wording, and test mechanics. AGENTS.md gives those to the commit message; an
  ADR states the decision, the boundary it does not cross, and what follows.

Revision note (2026-08-09) — a warning may be acknowledged, and is raised where
it lands:

- **ADR-052** makes two callouts dismissible, and moves one of them. Both stand
  on a run counter that only grows, so dismissal records the count and the
  warning returns when it is exceeded — acknowledgement, not deletion, and a
  counter below the record can only mean a new run. The dropped-update warning
  leaves the diagnostics card for the **Output** pane whose completeness it
  actually describes, and stops raising the card's badge, which had been
  reading ATTENTION over a card containing no reason for it. The boundary: only
  a warning standing on a growing counter is dismissible — a live interface
  fault, an impossible serial schedule, and a failed timer request each describe
  a condition still true while it is on screen.
- **ADR-053** puts losing cadence into the log, which had recorded failed sends
  since the runner shipped and missed ones nowhere. WARN on the first skipped
  send, INFO once five seconds pass without another, carrying the episode's
  total — the edges only, because skips concentrate on the shortest interval and
  a line each would flood the log with the fault's own symptom. It closes a CLI
  blind spot where a channel could skip half its cadence points with nothing on
  stdout, and makes the log answer *when* a channel fell behind rather than only
  what the totals are now. The drop warning of ADR-052 was reworded in the same
  pass to state its consequence rather than the queue behind it.
- **ADR-054** states who the log is written for, after a sweep found a dozen
  strings naming internals the reader has no access to — including the three
  that fire when Start is pressed with something unfilled. It also settles what
  `channel = …` means: it claims the event is about that channel's operation,
  and it is what raises the channel row's warning badge. Six internal
  bookkeeping faults carried it and should not have, so a bug in our own command
  tracking was summoning the reader to their serial link. They now name the
  channel in their text, say they are internal, say what it cost, and report on
  a decade cadence so a wedged state machine cannot flood the log.

Revision note (2026-08-06) — misses are measured where they happen:

- **ADR-051** amends the scope limit ADR-045 set for itself. Delay blame is
  collected per deadline the channel *reaches*, so a ten-second block against a
  10 ms cadence destroys a thousand cadence points and yields one lateness
  sample — the evidence thinned out as the fault grew. `Schedule::poll` now
  reports the points it skips, and each is charged to whichever send held the
  thread as it passed — searched over a retained send history sized from the
  schedule, so a channel cannot outgrow its own attribution. The missed-send
  callout gains one branch entitled to convict, and must state the share it
  cannot account for. ADR-045's refusal of a *victim-side* miss count still
  stands and is unchanged.

Revision note (2026-08-06) — four accents, and one rule:

- **ADR-050** reduces the palette from ten colours to four — `fault`, `warning`,
  `running`, `idle`, the same states `SignalTone` already names. Five greys were
  emphasis rather than meaning and became the theme's own `weak_text_color`;
  three more fields meant one thing in two places. The burden a palette carries
  is *pairs*, not colours: ten is forty-five to keep distinguishable, four is
  six, and two of the three ever checked had failed. It also states the rule the
  whole accessibility pass produced — colour reinforces a state, it never
  carries one alone — and fixes a live case found while applying it, where
  Running and Reconnecting shared the `●` glyph and differed only by the two
  colours already confirmed indistinguishable.

Revision note (2026-08-06) — the fault colour becomes visible:

- **ADR-049** makes the fault colour blue. Red was not distinguishable from the
  amber warning under a red-green colour deficiency, so the applications' most
  important signal was the one that did not arrive. Palette fields now name the
  role and not the hue — `fault`, not `fault_red` — because a palette exists so
  a colour can change, and this rename is that argument's own proof. Two further
  colour-only defects are recorded in the TODO rather than fixed here, along
  with the larger question of whether ten colours are needed at all.

Revision note (2026-08-05) — semantic color has one source:

- **ADR-048** routes Talker's remaining hardcoded status, severity and
  destructive colors through `wiredata_ui::palette`, so a fault in the log panel
  is the same red as a fault anywhere else in either application. The palette
  gains `tint`, which derives a surface fill from an accent and the theme's own
  panel color — replacing hand-named light/dark background pairs, which were the
  same decision made twice and drifted the moment an accent changed. Content
  annotation colors stay Talker-owned and are named as such.
- **Correction (2026-08-05):** as first written, ADR-048 said the diagnostics
  card's private `translucent` helper "is the same function" as `tint`. It is
  not, and it still exists: `translucent` returns an alpha-adjusted color and
  remains in use for the card's two strokes. What changed is three call sites
  that blended it into the panel fill.

Revision note (2026-07-31) — the warm-up gate retires:

- **ADR-046** removes the twenty-sample warm-up gate from Talker's readouts. It
  never guarded a bad computation: the percentile rank is
  `ceil(samples × 99 / 100)`, which equals `samples` for any count up to 99, so
  below a hundred samples the p99 bucket *is* the maximum's bucket and the gate
  only relabelled the same number — at 20, while the two statistics separate at
  100. Readouts now state the maximum with its sample count and add a percentile
  only when `p99 < max`, a data-derived test needing no constant. The count is
  stated once per line rather than beside every figure on it. Long runs gain a
  figure they lacked, since a percentile alone hid one-off stalls.
  `MIN_SERVICE_SAMPLES` is a different gate — it guards the headroom projection —
  and stays. Listener's migration is pending and recorded as deliberate.

Revision note (2026-07-31) — measured blame for deadline delay:

- **ADR-045** adds per-message timing and, with it, the missing half of every
  cadence readout Talker had. Deadline lateness names only the *victim* — and
  because the scheduler skips `late / interval + 1` grid points, the victim is
  almost always the message with the tightest interval rather than the one
  responsible. The runner now charges a message with another's delay for the
  portion that elapsed while its own send held the channel thread, keeps a
  backlog charged to the send that opened it, and charges nothing when no send
  spanned the deadline. Per-message miss counts are deliberately not offered.
  The clipboard report gains per-message timing keys; no profile schema, wire
  output, or cadence behavior changes.

Revision note (2026-07-28) — one vocabulary, each counted fact rendered once:

- **ADR-044** settles *interface* as the term for a configured serial/UDP/TCP
  endpoint, reserving *connection* for Listener's accepted TCP peer sessions, and
  renames the send readout to **Send outcomes** because it counts scheduled sends
  rather than the interface. The counted outcomes now render in exactly one place
  above the diagnostics card, which keeps only the readouts that need
  interpretation, and the outcome line is stated as visible arithmetic
  (`scheduled - failed - suppressed - missed = sent`) so the successful
  remainder is defined by the equation rather than by a noun claiming more than
  the application can observe. The shared row chrome tooltips its label as well
  as its value. No profile schema, clipboard-report keys, wire output, or cadence
  behavior changes.

Revision note (2026-07-27) — telemetry ownership across the two applications:

- **ADR-043** records why only Talker's timing telemetry carries a capture instant.
  Talker pushes collapsed snapshots from its send path, so they age between
  emissions; Listener collapses each recent window when a snapshot request is
  served, so a capture instant there would always read "now". The rule is that
  whichever application retains a collapsed snapshot across time owns proving its
  age, and neither mechanism is ported to the other — a Listener capture instant
  would be dead weight, and compute-on-demand on Talker would have to wake a dormant
  runner. The two panels stay consistent in vocabulary rather than mechanism. See
  listener ADR-035 for the same decision from Listener's side.

Revision note (2026-07-27) — truthful pushed-snapshot freshness, which introduced
ADR-039 through ADR-042:

- **ADR-039** moves the bounded duration-histogram buckets and the ten-segment
  recent window into `wiredata-telemetry`, so the two applications cannot drift
  apart on the numbers they present.
- **ADR-040** makes the timer-resolution guard lifecycle explicit and keeps Precise
  deadline windows bounded.
- **ADR-041** lets the diagnostics surface lead with decisions without hiding the
  telemetry behind them.
- **ADR-042** records the runner-supplied capture instant and explicit final
  provenance that distinguish current, expired, and final timing evidence without
  adding a dormant telemetry heartbeat. Capacity, Cadence, and detailed Timing now
  consume one freshness classification, and expired recent timing cannot silently
  drive measured headroom.
