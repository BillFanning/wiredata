# Listener — documentation revision history

Superseded revision notes for [listener_specification.md](listener_specification.md), newest first.
It also holds the full text of ADR entries retired or merged out of [ADR.md](ADR.md),
under Retired decisions.

They are kept out of the specification itself so that document states **current
behaviour** and carries its version in one place. Each entry records what changed
and why at the time it changed; where a decision stands behind it, the ADR named
in the entry is the authority. Nothing here is normative — if this disagrees with
the specification, the specification is right and this is history.

---

Revision note (2026-11-20) — serial lines, shutdown and the event log:

- **§3.1.** Exit code 3 no longer counts retries running out; nothing gives up.
- **§14.3, §50.3, §161 (ADR-073).** An output line a profile leaves unset is set
  low at open, so the level shown is the level set; under RTS/CTS flow control
  RTS reads "RTS set by flow control". Lines show only while the port is running.
- **§118.** A start fault that keeps repeating is written once, then at most once
  every 10 minutes with a count.
- **§3.1 (ADR-052).** The CLI reads a stop request while a port is still opening.

Revision note (2026-11-19) — reconnect is on or off, at a fixed pace:

- **§9.1, §80.1, §162 (ADR-072).** A Channel's `ReconnectPolicy` holds only
  `enabled`. Reconnecting waits 1 s, doubling to 30 s, and goes on until the
  Channel starts, the pace a recording's reopen uses. Nothing gives up, so
  the give-up event and status are gone.
- **§71.** The reconnect limits are gone with the settings they checked.

Revision note (2026-11-19) — event log files are kept:

- **§118 (talker ADR-069).** Event log files are no longer deleted after 30
  days. They are kept until an operator removes them.
- **§50.3.** Linemon never retries or reopens a failed port by itself; Start
  opens it again.

Revision note (2026-11-19) — Linemon's controls:

- **§50.3 (ADR-070).** The UDP port number has − and + beside it; serial
  ports are chosen from the drop-down only. A custom baud rate is typed at
  the bottom of the baud list. A change that leaves no port chosen marks the
  display. The checkbox is named Clear display on port change. The UDP
  settings are two lines, as in a channel's editor, with no More button.
- **§15.** Port sharing stays in place for unicast, disabled, and says why.

Revision note (2026-11-16) — the pop-out and the Channel's display pause:

- **§50.4 (ADR-071).** Pausing the Channel's display pauses its pop-out too,
  and the pop-out says so. Resume shows what arrived while the pop-out was
  paused, and removing the Channel closes its pop-out.

Revision note (2026-11-15) — Linemon on its own, and the pop-out display:

- **§50.3 (ADR-070).** Linemon is a program of its own, not a window of
  Listener. It no longer names a channel that holds a port, and a channel no
  longer names Linemon.
- **§50.4 (ADR-071).** New: a channel's display can be popped out into a
  window of its own.
- **Appendix A.** What Linemon leaves for later is a record button, time
  stamps on each line, finding the baud rate, TCP and find.

Revision note (2026-11-15) — Linemon, as built:

- **§50.3 (ADR-070).** Adds Start, Stop and Reopen port on start, and says
  that a typed value takes effect when its field is left. Names the buttons
  as they appear: Previous, Next and Copy scroll buffer. A port held by
  Linemon or by a channel is refused the same way in both directions, and a
  refused port waits for Start.
- **§91, §113.** Quoted on-screen messages write "channel" in lower case, as
  the screen does.

Revision note (2026-11-15) — Linemon:

- **§50.3 (ADR-070).** New: Linemon, a window for looking at one stream.
- **Appendix A.** Lists what Linemon leaves for later.

Revision note (2026-11-15) — fixes from the follow-up code review:

- **§71 (ADR-064, ADR-065).** A rule may have at most 16 actions and a Mark
  separator at most 256 bytes. Record triggers whose Stop match can end inside
  their Begin match are refused.
- **§50.2.** A bare Mark is placed before its match, in the recording span
  its read goes to.
- **§91 (ADR-068).** Figures left from an earlier run are labelled as such
  when the latest run left no final answer.
- **§99 (ADR-066).** Runtime events travel with the periodic updates; only
  results of user actions and profile loads are never dropped.
- **§113.** A Channel whose pipeline failed before confirming its close
  counts as finalization incomplete.

Revision note (2026-11-14) — fixes from the November code review:

- **§50.2 (ADR-064, ADR-065).** A Record action takes effect at its match,
  and one read fires at most 4,096 matches. A Mark on a multi-byte character
  is placed against the whole character.
- **§56.1, §59.** A failed close of a rotated file is a recording fault. A due
  flush is not put off by queued data.
- **§9.1.** The 60 s stability window runs from the reconnect's successful
  start.
- **§113.** The run summary and the copied report say whether finalization
  completed.
- **§99 (ADR-066).** One-time GUI results are never dropped.
- **§91 (ADR-068).** Status that is not from the latest request says so, and
  how old it is.
- **§99 (ADR-069).** The GUI's driver waits on no Channel queue and no file
  without a limit.
- **§80.1 (ADR-067).** A profile's defaults are written into each Channel at
  load.
- **§118.** The CLI waits at most 2 s for the event log at exit.

Revision note (2026-11-14) — times are local unless they say UTC:

- **§54, §56.1 (ADR-063).** Gap times, in a `‹GAP …›` line and in a gap
  record, are ISO 8601 local time with milliseconds and offset. They had a
  space for the `T` and no offset.
- **§91.** States the rule, and that the GUI shows a completed run's start
  and finish in local time.

Revision note (2026-11-14) — the ZDA zone sign and disk-guard limits:

- **§50.2.** ZDA marks write NMEA's local zone, the hours added to local
  time to get UTC, so a zone east of Greenwich is negative. They used to
  write the ISO 8601 offset, the opposite sign.
- **§56.2, §71.** A disk-guard threshold must be 1 to 90 percent or more
  than 0 bytes; anything else makes the Channel invalid.

Revision note (2026-11-14) — one stuck Channel cannot hold up the others:

- **§91, §113 (ADR-062).** A status request waits at most 300 ms, and a
  Channel that does not answer is shown with its last status of the current run.
  A Faulted Channel's own finalization is bounded by the stop grace period. A
  Begin on a recording already running returns at once.

Revision note (2026-11-14) — TCP receive code removed:

- **§4, §7, §9, §13, §16, §69, §71, §73, §76, §85, §92, §94, §96, §97, §102,
  §104, §137, §138, §154, §162, Appendix A (ADR-047).** The TCP Listener and TCP
  Connection code is removed, no longer only disabled. §16 now points to
  Appendix A, and the TCP kinds, events, transport contracts and templates are
  gone from the sections that listed them.
- **§72.** A saved profile that names a TCP Listener loads without that Channel,
  with a warning naming it.

Revision note (2026-11-13) — one display view:

- **§11, §48, §50.2, §51, §54, §58, §78 (ADR-060).** A Channel has exactly one
  display view, saved as `display.view`; the `views` list, `DisplayViewId` and
  the `PauseDisplay` target are gone. Profiles saved with a `views` list still
  load, keeping the first view and warning if there were more.
- **§72.** The example profile is removed.

Revision note (2026-11-13) — idle rules, reconnects and gap lines:

- **§50.2, §91, §126, §157 (ADR-030, amended).** Idle rules use an ordinary deadline
  wait; the Windows 1 ms precision window, its lateness histograms and timer
  counts are gone. A rule can fire up to about 16 ms late on Windows.
- **§9.1 (ADR-058).** A fault within 60 s of a successful reconnect continues
  the attempt count and backoff, so a flapping device reaches `max_attempts`.
- **§54, §56, §56.1, §79, §142 (ADR-059).** A Display Recording marks a gap
  that lost bytes with a `‹GAP …›` line where it resumes, under a new **Mark
  gaps** option, on by default; Raw never. `RecordingStopReason` and the
  `finalize` reason are gone: since ADR-043 a fault opens a gap instead.

Revision note (2026-11-13) — one shape for received bytes:

- **§138.** `ReceivedData.payload` is one byte vector. It was an enum of a
  stream chunk and a datagram, which every reader treated the same way.

Revision note (2026-11-13) — hover text and the diagnostics card:

- **§3.2 Hover text (ADR-056).** Every hover that explains something opens
  with a bold sentence, then short paragraphs in plain, complete sentences. A
  hover used to be one dense paragraph, and several stated things the code
  does not do.
- **§3.2 Receive diagnostics card (ADR-057).** The card has no title and no
  status badge; its rows and callouts carry its warnings.

Revision note (2026-11-12) — console output never holds up reception:

- **§3.1 (ADR-055).** Every line printed while Channels run goes through a
  bounded queue, and a line the console does not take is dropped and counted.
  A Channel used to wait for the console: a text selection that paused a
  console window paused reception with it.

Revision note (2026-11-12) — a failed event log is tried again:

- **§118.** A log file that cannot be opened or written is opened again about
  once a minute, and the recovered log says how long entries are missing. One
  failed write, or one failed open of the next day's file, used to end the
  event log for the rest of the run.

Revision note (2026-11-12) — the end of a Serial stall is reported:

- **§101, §137.** When a reception stall that reached the warning threshold
  ends because the blocked hand-off goes through, the reader sends a second
  advisory notice with the stall's full length, and the runtime emits
  `ReceptionResumed`. The warning raised at the threshold could only state the
  length so far, so a stall of ten seconds read as a quarter of one.

Revision note (2026-10-01) — an example profile:

- **§72** points to a complete example profile,
  [`listener/profiles/profile.example.toml`](../profiles/profile.example.toml),
  which a test keeps loading.

Revision note (2026-09-30) — requirements for unattended, weeks-long logging:

- **Recording continuity (ADR-043), §56.1, §56.2, §57, §59.** A recording fault
  is no longer terminal. The recording records a gap and continues in a new,
  numbered segment. Recovery never recreates a vanished folder, and files have a
  soft size cap. Rotation only moves forward in time, and Listener never deletes
  recordings.
- **Persistent event log (ADR-044), §118.** Every diagnostic also goes to a daily
  log file, kept 30 days.
- **Unattended GUI (ADR-045), §9.1, §70, §159.** Reconnect is a visible,
  confirmed choice, and one profile can be registered to resume on launch after
  a cancellable countdown.
- **Headless CLI (ADR-046, talker ADR-060), §3.1, §113.** The CLI starts what it
  can, warns loudly, stops on every OS stop signal, and exits with defined codes.
- **Network (ADR-047).** The TCP Listener is disabled until its data can be shown
  and recorded (§4.1, §16 and the sections that offer it). UDP states its bind
  scope in words and can request a shared port (§15, §75).
- **Strict profiles and bounded capacities (ADR-048), §71, §72.1, §80, §88,
  §124.** Unknown keys and a missing `schema_version` are refused, and every
  capacity has a limit.
- **§50.2 matching** reaches across any number of short reads, and a boundary
  split is noted once per run.
- **Drift corrected.** Export (§60–§64) and live network adjustment (§76.1,
  §167) were never implemented and move to Appendix A. §156 now agrees with §48.

Revision note (2026-08-08) — a Mark takes a row of its own in Hex:

- **§45 Hex Mark placement (ADR-042).** A `Mark` is a row, not a cell in a byte
  row. Hex is a fixed-width grid and reading a field down a column is what the
  mode is for; a `Mark` spliced into a byte row is as wide as its text, so
  everything after it landed at an unpredictable column. The byte run now breaks
  **at the byte the `Mark` targets**, because a `Mark` identifies a byte and a
  coarser placement would answer a different question. The short row that
  results is explained by the `Mark` on the line below it.
- **§45 the column bound is retained** and restated for what it now does: with
  `Mark`s off the byte rows it is the renderer holding the pane limit for
  itself, not the thing standing between a `Mark` and a split byte.
- A Hex `.disp` recording now carries `Mark`s on their own lines, following the
  display as it always has. Byte-exact timestamping remains `.raw` plus its
  `.raw.idx` sidecar (ADR-039), which perturbs nothing.

Revision note (2026-08-06) — a Hex line is bounded by columns:

- **§45 Hex line width (ADR-041).** A row ends when the next cell would exceed
  the pane, not only when the byte budget is spent. An inline `Mark` occupies one
  cell and as many columns as its text, so the cell-count bound alone let a line
  overrun and be cut mid-byte by the viewer's character splitter. A `Mark` wider
  than the whole pane may still overrun; that cuts Mark text, never a byte.
- **§57 sidecar record.** The prose said `ChunkTime` — wall clock *and*
  monotonic — reached the sidecar, six lines above the record format that carries
  only the wall-clock value. The description now matches the format.

Revision note (2026-08-06) — Hex grouping and the timestamp sidecar become specified behaviour:

- **§45 Hex grouping (ADR-038/ADR-040).** `HexGrouping` was specified and
  persisted but read by nothing. Its two halves now have stated reach: bytes per
  group is rendering and appears in the `.disp`; groups per line is view layout,
  never reaches a recording, and is reduced when the pane cannot show it. The
  section states the invariant that follows — no displayed row splits a byte or
  begins or ends with a separator.
- **§57 Raw timestamp sidecar (ADR-039).** The flag is no longer config-only: the
  Raw recording editor exposes it. An offset is a position in the `.raw` beside
  it, so appending continues from that file's end rather than restarting — the
  previous behaviour indexed an appended run's blocks onto the previous run's
  bytes while leaving the `.raw` itself byte-exact.
- No schema, transport, queue, or telemetry change; `CURRENT_VERSION` is
  unchanged. Existing profiles are unaffected.

Revision note (2026-07-27) — authoritative Serial stall snapshots:

- **§91.2 / §97 / §99 / §101 / §104 / §128 Serial backpressure telemetry
  (ADR-034).** Each successfully opened Serial run now owns reader-maintained
  authoritative stall state. Runtime snapshots derive completed episode count, total,
  and maximum directly from that state and expose any active episode's elapsed time
  separately; warning delivery is not an accounting boundary.
- **§137 / §152 / §157 advisory reporting and verification.** The established
  threshold warning remains an at-most-once best-effort signal. A dropped transport
  notice omits its diagnostic and event; a full event queue may omit only the event.
  Neither changes polled metrics, and sub-threshold episodes are still counted.
- The bounded queue, received bytes, warning threshold, profile and recording schemas,
  and completed-run clipboard keys are unchanged.

Previous revision note (shared bounded telemetry primitives):

- **§91.2 / §102 / §127–§128 bounded duration telemetry (ADR-032).** The
  internal, non-published, dependency-free `wiredata-telemetry` crate now owns the
  fixed duration histogram and bounded ten-by-one-second recent-window engine shared
  with Talker.
- Listener keeps its measurement boundaries and application-specific aggregates,
  including `ByteHistogram`, `ChunkShape`, transport/timer summaries,
  stats/snapshots, completed-run retention, and GUI presentation.
- This is an implementation extraction only: receive behavior, profile schema,
  recording formats, and Listener's exact Idle-deadline timer policy are unchanged.

Revision v2.2.1 (bounded receive timing, transport health, and completed-run reports):

- **§26 / §91.2 / §102 receive telemetry (ADR-026–ADR-029).** Listener exposes
  cumulative and recent post-read handoff and total pipeline-processing histograms,
  cumulative chunk size/inter-read-gap shape, exact queue high-water marks, Serial
  stall duration, and platform-explicit UDP loss counters. Histograms are bounded,
  use p99 bucket upper bounds, and add no per-chunk GUI event traffic.
- **§50.2 exact Idle deadlines (ADR-030).** Idle rules wait on their next monotonic
  deadline rather than a periodic rule poll and report firing lateness. Windows asks
  for 1 ms timer resolution only during the final 32 ms; Linux and macOS keep native
  deadline waits. The mechanism is shared with Talker through `wiredata-timing`.
- **§15 / §26 / §75 optional UDP kernel time (ADR-029).** Linux can request
  `SO_TIMESTAMPNS` software receive times; other platforms and missing ancillary
  metadata explicitly fall back to post-read wall time. The post-read monotonic point
  remains the ordering and elapsed-time anchor in every mode.
- **§86 / §91.2 retained run report (ADR-031).** The newest completed run retains
  exact bytes/chunks, diagnostics, queue peaks, timing, timer policy, transport health,
  and build/platform facts, with an on-click versioned clipboard report. Retention is
  process-local and bounded to one summary per Channel.
- `UdpConfig.kernel_timestamps` is additive and defaults off; no other new setting is
  persisted. Profile `schema_version` remains 3.

Revision v2.2 (NMEA ZDA Mark annotations and exact splice geometry):

- **§50.2 Mark styles (ADR-025).** A timestamped Mark may retain compact local
  time or emit an inline checksum-bearing NMEA ZDA sentence. ZDA time/date fields
  are UTC, zone fields carry the local offset, milliseconds are optional, and
  custom talker IDs longer than two characters are supported under a bounded
  framing-safe policy.
- `Before` anchors on the first match byte and `After` on the final match byte.
  Separators remain verbatim and may contain CR/LF; Raw, Rendered, and Hex renderers
  reset their line/column continuation state around those controls. `.raw` remains
  byte-exact.
- Listener again depends on `nmea0183`, solely to construct presentation text. It
  still performs no protocol decoding or interpretation of received bytes.
- `MarkTimestamp.style` is additive and defaults to `Plain`; profile
  `schema_version` remains 3.

Revision v2.1 (scope amendments — one display view per channel; TCP connection-channel
visibility deferred; a §155 correction):

- **§48 one Display View per Channel (ADR-023).** v2.1 narrows §48 from "multiple
  simultaneous Display Views" to exactly **one logical Display View per Channel**,
  with Raw / Rendered / Hex as that view's switchable *modes*. This documents shipped
  reality (pause gating and the GUI have only ever used the first view) and matches
  talker's one-Output-pane model. The profile schema's `views` list is unchanged
  (additive forward-compatibility); entries beyond the first are ignored. Multiple
  simultaneous views move to Appendix A.
- **§16 TCP Connection-Channel visibility is deferred (ADR-024; parked by user
  decision 2026-07-11).** Accepted connections run full pipelines and emit
  connect/disconnect lifecycle events, but are not individually inspectable (no
  per-connection snapshot, stream view, recording, or match rules), and
  `recv_buffer_bytes` is not yet applied to accepted sockets. §16.2's independence
  requirements remain the architecture; their user-facing surfacing moves to
  Appendix A until a real TCP-inspection need promotes it.
- **§155 correction** — the BytePattern acceptance criterion said matches are
  "highlighted in the stream"; on-screen highlighting was removed in v2.0
  (ADR-015). The criterion now names the shipped observables: the rule-firing
  log and inline `Mark` splices.

Revision v2.0.7 (inline Mark timestamps; remove two abandoned timestamp pieces):

The `Mark` match action may now carry an optional **inline timestamp** (§50.2): the
matched byte pattern's local **arrival** time is spliced into the rendered display and
the Display Recording (`.disp`) — configurably **before** or **after** the match —
never into the byte-exact `.raw` stream (§53). The timestamp format mirrors talker's
toggleable `TimestampConfig` (time-of-day always shown; date / milliseconds / local UTC
offset optional), formatted in local time. It is rendered by splicing the timestamp
string at the match's byte offset in every view mode (Raw/Rendered/Hex) — a text
insertion, not on-screen byte-range styling (contrast the removed `Highlight`, ADR-015).
A bare `Mark` (no timestamp) keeps its `‹MARK …›` marker-line behaviour.

Two abandoned, never-completed timestamp pieces are removed so this is the **only**
timestamping mechanism: (a) the per-chunk Display-Recording timestamp
(`DisplayRecordingConfig.timestamp_enabled` and its GUI checkbox) — it duplicated
nothing useful and was never surfaced; and (b) the spec-only
`TimestampDisplay`/`TimestampSource`/`TimestampResolution` types (§26/§57/§72), which had
no implementation. The byte-exact **Raw Recording timestamp sidecar** (`.raw.idx`, §57)
is **kept** — it stays out of the `.raw` bytes — with a TODO to expose it in the UI. No
`schema_version` change (the removed display field is an additive-safe drop; dev-only
profiles). See the new ADR.

Revision v2.0.6 (drop the Highlight match action):

The `Highlight` match action and its `HighlightStyle` type are removed (§50.2/§165/§72).
`Highlight` was never rendered — precise on-screen byte-range styling across all view
modes was disproportionate to listener's simple goal; the `Mark` action already serves
correlation into the display recording. The remaining Find & Triggers actions
(`Record`/`Mark`/`Notify`/`PauseDisplay`) and conditions (`BytePattern`/`Idle`) are
unchanged. No `schema_version` change (variant removal; dev-only profiles). See ADR-015.

Revision v2.0.5 (diagnostics retention across a restart):

New §89.1: a Channel's retained diagnostics (§92–§94) persist across that Channel's
own stop→start within a session, so the log spans a restart instead of starting blank;
a start that faults (e.g. a bind conflict) is retained as an Error and survives the
next start. Bounded by the §88 limits; runtime-only (not persisted to disk); the stream
scrollback is not carried across a restart (§8.5). See listener ADR-006. No schema
change.

Also reconciles the docs with the code (doc-catch-up, no behavior change): §137 gains
`RecordingStarted` (present since ADR-012/-013; it lets observers clear a prior
recording fault); §99 describes diagnostics retention as the per-severity count-bounded
`DiagnosticLog` actually shipped, not the abandoned single drop-oldest-low-priority
queue.

Revision v2.0.4 (recording-destination uniqueness):

Two recordings may no longer write the same file (§121, ADR-014). §6 makes Channel
Names **unique** (was "need not be unique"); §71 validation rejects a duplicate name;
§55 fails enabling (as a *recording* fault, Channel stays Running) if an OS advisory
lock on the destination cannot be taken; §121 states the two-layer rule (unique names +
advisory lock). No schema change — names were already required filesystem-safe.

Revision v2.0.3 (recording-config reconciliation):

§52 and §72 are reconciled to the §79 independent Raw/Display recording config
(v2.0.2, ADR-013): §52 no longer defines a `RecordingMode` enum, and §72's
`ChannelConfig` carries `raw_recording` + `display_recording` instead of a single
`recording` field. No behavior change — these sections were stale after the v2.0.2
edit only updated §79.

Revision v2.0.2 (independent Raw/Display recording config):

§79 replaces the single `RecordingConfig`/`RecordingMode` with independent
`RawRecordingConfig` and `DisplayRecordingConfig` — Raw and Display recording tap the
pipeline separately and are now configured separately (each with its own destination),
so a Channel may run both at once. `schema_version` bumps to 3 (clean break; v1/v2
profiles refused). See listener ADR-013.

Revision v2.0.1 (command-surface reconciliation):

§136 no longer defines a `RuntimeCommand` enum. The command surface (UI → runtime)
is the `Listener`'s async method API directly; the GUI's own `UiCommand` is the
presentation-layer transport the driver maps onto those calls. See listener ADR-012.
`RuntimeEvent` (§137) is unchanged.

Revision v2.0 (architecture pivot — **stream-only**):

`Listener` is now a pure **stream** acquisition / inspection / recording tool.
Received bytes are a single verbatim stream that is displayed (Raw / Rendered / Hex)
and recorded. The entire Message infrastructure is removed — there is no Message
Mode, Message Extraction, Message Numbering, decoders, NMEA decoding, integrity
metadata, message-framed recording, or message-keyed display.

Removed (the superseded sections are kept as numbered stubs so existing §N
cross-references stay valid): §19 Message Mode; §20–23 Extraction; §24 Message
Number; §28 Integrity Metadata; §29–39 Decoders + all NMEA sections; §49 Metadata
Display; §50.1 Subsampling; §77 Decoder Config; §83 NMEA Serial Template; §131–135
Message types; §139/§140 Extractor/Decoder traits; §105/§107 Extractor/Decoder
ownership; and the message-framed recording path.

Rebased onto the stream:
- §17–18 — the input model is **stream only**; no per-channel Stream/Message switch.
- §40–46 — the display renders the verbatim stream (Raw/Rendered/Hex + character
  rendering); §41 has a single source (the stream).
- §50.2 / §165 — **Find & Triggers**: conditions are `BytePattern` (cross-chunk
  stream scan) and `Idle`; actions are
  `Record { Begin | Stop, target: Raw | Display | Both }`, `Notify`, and `Mark` (a
  time/offset marker into the display and the display recording). No decoded-field or
  message-size conditions; `Mark` anchors on a **byte offset**, not a Message Number.
  (An on-screen `Highlight` styling action was considered and dropped — see ADR-015.)
- §51–59 / §79 / §142 — recording is **Raw** (byte-exact, extension **`.raw`**) and
  **Display** (the rendered output of the active view mode, extension **`.disp`**),
  each with optional timestamps and time-based rotation. `.ssdat` and subsampling are
  removed; `.raw` replaces the former `.dat`.

`schema_version` bumps (a breaking config change: the extraction, decoder, subsample,
and message-recording fields are gone). `nmea0183` remains a workspace crate.
Listener v2.2 reacquires it only to construct presentation-only ZDA Mark annotations;
the receive path still performs no decoding.

Revision v1.2 (feature expansion — troubleshooting & long-run logging). _Parts of
this note are superseded by v2.0 above — the supersessions are flagged inline._
- §59 — **time-based file rotation** (Hourly/Daily); generated filenames
  `<channel>_<start-time>`. _v2.0: extensions are now **`.raw`** (raw data) and
  **`.disp`** (display recording), plus **`.log`** (diagnostic log only); the former
  `.dat`/`.ssdat` are gone (§50.1)._ Filesystem-safe channel-name constraints (§71).
  Size-based rotation stays deferred.
- §50.1 — sink subsampling. _Removed in v2.0 (it was a Message-oriented filter)._
- §50.2 — **Match Rules & Triggers**: a predicate fires actions (highlight, begin/stop
  recording, mark, notify, pause). _v2.0: conditions are byte-pattern / idle only; the
  decoded-field and message-size conditions are removed (§50.2)._
- §14.3/§14.4 — full **serial control-line** monitor + control (RTS/DTR set and
  live-toggle; CTS/DSR/DCD/RI live display); port enumeration + hot-plug; RS-422/485
  phased.
- §75/§76 — **network live adjustment**: SO_RCVBUF, multicast join/leave + interface.
- §78 — display extras: per-view **source**, **timestamp format**, **hex grouping**, and
  per-message **annotation toggles** (number + timestamp) replacing `metadata_visible`.
  _v2.0.7: the per-view timestamp-format display extra and its `TimestampDisplay`/
  `TimestampSource`/`TimestampResolution` types are removed (never implemented); the only
  display timestamp is the per-match `Mark` timestamp (§50.2)._
- §9.1 opt-in **auto-reconnect**; §91.1 **liveness / activity monitor**; §56.2
  **disk-space guard** for recording.
- §136/§137 — command/event vocabulary expanded (incl. a dedicated `ReceptionStalled`
  event resolving the ADR-007 overload); both enums now `#[non_exhaustive]`.
Config changes are additive (`#[serde(default)]`), so `schema_version` is unchanged;
`metadata_visible` is retired (old profiles still load).

Revision v1.1.1:
- Spec relocated to the crate's own `docs/` directory; document-location paths corrected.
- §127/§128 — resolved OQ-L1: `Listener` ships as a **single `listener` crate** with
  the proposed `listener-*` units realized as `src/` modules (boundaries unchanged);
  corrected the layout that nested `nmea0183` (it is a top-level sibling crate).
No runtime, data-model, or schema changes from v1.1 — the packaging decision does not
alter the normative module boundaries or behavior.


---

## Retired decisions

The full text of ADR entries that were retired, or merged into another entry, on
2026-11-14, in number order. [ADR.md](ADR.md) keeps each number with one line that says
why and where the decision lives now. Nothing here is normative.

### ADR-002 — Messages are immutable; decoders are read-only

> **Superseded by ADR-010 (spec v2.0):** Messages and decoders are removed. The
> surviving principles live on as §103 (received chunks are immutable, shared via
> `Arc`) and §5.4/§5.5 (display/recording never alter received bytes).

**Authoritative context:** spec Part on Messages/Decoders (§131–§135, §140; removed in v2.0).

**Decision:** A `Message` is immutable once emitted by the extractor. Decoders never mutate Messages — they only produce read-only annotations (protocol metadata, integrity metadata). Display formatting never affects what is recorded (Raw Recording is independent of rendering).

**Consequences:**
- Multiple downstream consumers (display, recording, decoding) can share Messages without coordination or copying.
- Raw recording fidelity is guaranteed regardless of decoder or display behavior.

### ADR-005 — Delimiter extraction emits a Message at every delimiter, including empty payloads

> **Obsolete under ADR-010 (spec v2.0):** delimiter extraction is removed entirely;
> there is nothing to emit. Kept for historical context only.

**Authoritative context:** spec §21 (delimiter extraction; removed in v2.0) and §150 (the
required "consecutive delimiters" test). The spec mandated the test but did not
prescribe the outcome, so the behavior was fixed here.

**Decision:** `DelimiterExtractor` completes a Message at **every** delimiter
occurrence, even when no payload bytes precede it. Consecutive delimiters
therefore yield empty-payload Messages, and a leading delimiter yields a leading
empty Message — mirroring standard string-split semantics (`"a\n\nb".split` →
`["a", "", "b"]`). When `include_delimiter` is true the "empty" Message still
contains the delimiter bytes; when false its payload is zero-length. Un-terminated
trailing bytes are an incomplete Message and are discarded at `finish` (§112),
not emitted as a final empty Message.

**Rationale:** Listener is a forensic receive-side tool — "bad data is often the
most important data" (§37). Silently collapsing runs of delimiters would hide the
on-wire structure (e.g. stray blank lines, double CRLFs) that an operator is
often looking for. Emit-on-every-delimiter is deterministic (§125), matches the
"extraction defines structure" principle (§5.2), and is the simplest rule to
reason about. A downstream decoder marks an empty/`$`-less Message as it sees fit
(§37); extraction does not pre-judge meaning.

**Consequences:**
- A noisy source can produce many empty Messages; they consume Message Numbers
  and retention slots like any other Message. Bounding pathological cases
  (oversized un-terminated buffers, §119/§124) is a separate config/runtime
  concern, not an extractor responsibility.
- This rule is observable behavior and is pinned by unit tests in
  `extract/delimiter.rs`.

### ADR-024 — TCP Connection-Channel surfacing is deferred scope

**Status:** Accepted 2026-07-16 (spec v2.1; parked by user decision 2026-07-11).
**Context:** every accepted TCP connection runs a full pipeline (Model A, §16.4),
but the supervisor drops its request handle — so connections surface only
connect/disconnect lifecycle events: no per-connection snapshot, stream view,
recording (blocked on §59 filename templating), or match rules, and
`recv_buffer_bytes` is not applied to accepted sockets. The spec read as if the
full per-connection surface existed.

**Decision:** Record the shipped boundary as normative v2.1 scope. §16.2's
independence requirements remain the architecture; their user-facing surfacing is
Appendix-A deferred until a real TCP-inspection need promotes it (UC1 today is
serial/UDP). When promoted, the design direction stays what the TODO records: the
supervisor retains per-connection handles in a registry keyed by the minted
`ChannelId`, `Listener::snapshot`/`stream_delta` route through it, and the GUI
decides between sub-tabs and dynamic top-level channels.

**Consequences:** The spec no longer promises an inspection surface the runtime
doesn't expose. The full-pipeline-per-connection cost is acknowledged as paying for
recording-readiness and §16.2 independence, not for visibility; a lighter
per-connection pipeline is an option if promotion is far off.

ADR-030 was rewritten in place on 2026-11-14 to the current decision. The entry as it stood
before, with its 2026-11-13 amendment, read:

### ADR-030 — Idle rules wait on exact deadlines with bounded Windows precision (first-made text)

**Status:** Accepted 2026-07-19 (spec v2.2.1). **Amended 2026-11-13:** the
exact-deadline wait stands; the Windows precision window, the 1 ms request,
the lateness histograms and the wait counts are removed. The user judged
about 16 ms of possible lateness on Windows acceptable for a rule that fires
on a quiet stream, against a timer request, two histograms, a summary and a
GUI readout kept only for it. Idle rules now use one ordinary deadline wait
on every platform. The text below records the decision as first made.

**Context:** Idle rules were evaluated by a shared 250 ms maintenance tick, adding up
to one tick of avoidable lateness before executor or OS wake jitter. Receiving data is
event-driven and gains nothing from a finer timer period, but an explicitly configured
timer condition should target its own monotonic deadline. Holding a Windows 1 ms
request for an entire quiet channel would pay a continuous power cost.

**Decision:** The pipeline computes the earliest armed Idle-rule deadline for each
select pass and waits directly on it. New data, commands, or cancellation discard the
future and recompute it; only a completed deadline wait records a timer-policy sample
and evaluates rules. Each firing records nonnegative monotonic lateness in cumulative
and ten-segment recent histograms. Recorder maintenance remains on its independent
250 ms tick.

On Windows, a deadline more than 32 ms away first waits natively to the final window,
then holds the shared 1 ms RAII guard through the deadline and releases it immediately
after wake. Linux and macOS use one native Tokio deadline wait. The internal
`wiredata-timing` crate owns only the process-wide refcounted Windows begin/end calls
and minimized-window throttling opt-out shared with Talker; Listener owns its rule,
window, and telemetry policy. Successful requests, failed requests, and native waits
are counted separately. There is no spin wait or dedicated timing thread.

**Consequences:** Idle evaluation no longer carries intentional 250 ms polling delay,
while ordinary receive paths make no timer-resolution request. Windows precision is
held only near a real rule deadline and composes safely with Talker in the shared
mechanism. Wake accuracy still depends on executor and OS scheduling, and no timer
policy improves arrival timestamps. macOS App Nap remains a separate activity-policy
question rather than a resolution-call analog.

### ADR-040 — Fit-to-width resolves to whole bytes before rendering

**Status:** Accepted 2026-08-05. Amends ADR-038; from external review.

**Context:** ADR-038 put Hex wrapping in the renderer, where a cell boundary is
known, and said so as though it covered the feature. It covered one half. A
configured `groups_per_line` produced a real line length; fit-to-width — the
*default* — passed zero, meaning "do not wrap", and left the row to
`split_stream_rows`, which cuts at a raw monospace column.

That column does not respect cells. A row could arrive as `" 03 04 0"`: a
separator stranded at the head, and a byte delivered as `0` here and `A` on the
next row. The comment above the renderer construction claimed the cell-aware
guarantee for the whole Hex path, so the code read as though this were handled.

A second defect sat beside it. The row cache's rebuild key was channel, mode,
control-character rendering and pane width. Grouping was absent, so changing
bytes-per-group restyled only newly arriving bytes and left the existing
scrollback in the old shape until an unrelated change forced a rebuild — a
setting that appeared not to work.

**Decision:** Fit-to-width becomes a byte count *before* rendering. `G` groups of
`B` bytes occupy `G × B × 2` digits plus `G − 1` separators, so the largest `G`
fitting `cols` columns is `(cols + 1) / (2B + 1)`. The renderer then wraps at
that many bytes and the splitter has nothing left to cut.

Two edges follow from refusing to return an oversized line, since an oversized
line is precisely what re-engages the splitter:

- **A configured group count is clamped, not overflowed.** Asking for sixteen
  groups in a pane holding four shows four. The alternative is showing sixteen
  broken ones, and the control's help now states the reduction.
- **When not one whole group fits, the fallback is whole bytes** — not one group
  anyway. A byte is the smallest unit that can be wrapped without misreporting
  the data.

Both the splitter and the resolver read one `MIN_WRAP_COLS`, so the width the
renderer wraps to and the width the splitter would cut at cannot disagree.

Grouping and resolved line length join the row cache's rebuild key.

**Consequences:** One invariant now holds and is tested directly rather than
asserted in prose: **no pane width or grouping splits a byte.**
`no_pane_width_or_grouping_ever_splits_a_hex_byte` sweeps four group sizes, four
line settings and eight pane widths over all 256 byte values, checking every row
is whole hex groups joined by single spaces, with no leading or trailing
separator, and that the rows still reconstruct the stream exactly. It was
confirmed to fail against the previous behaviour, reporting `" 03 04 0"`.

`split_stream_rows` remains character-based and still wraps the text modes, where
a character *is* the unit. The comment at the call site now says which stage owns
which, instead of claiming the stronger boundary for both.

### ADR-041 — A Hex line is bounded by columns, not by cells

**Status:** Accepted 2026-08-06. Amends ADR-040; from external review.

**Context:** ADR-040 resolved fit-to-width to a byte count so the renderer,
which knows cell boundaries, would do the wrapping. It held for bytes and broke
for Marks.

The renderer wrapped when its *cell* count reached the line's byte budget, and
an inline `Mark` (§50.2) counts as one cell while occupying as many columns as
its text. A line correct by cell count could therefore be far wider than the
pane, and the oversized line went to `split_stream_rows` — which is exactly the
re-engagement ADR-040 set out to prevent. At eight columns with one byte per
group, a `[T]` before the second byte rendered `41 [T] 42` and came back as
`41 [T] 4` and `2`.

The test that was supposed to cover this passed no annotations, so
`no_pane_width_or_grouping_ever_splits_a_hex_byte` proved the invariant only in
the case that was never at risk. Its name claimed the general result.

**Decision:** The renderer tracks the columns it has written, not only the cells,
and ends a line when the next cell would not fit the pane **or** the byte budget
is spent — whichever comes first. A byte is two columns; an annotation is as
wide as its text; a separator counts where one is emitted.

The pane width is attached to the view by `rebuild_rows`, which already receives
`wrap_cols`, rather than by the GUI call site. The two settings cannot then be
supplied apart: a view carrying a Hex line length but no column bound is the
exact configuration that produced this defect, and it is now unconstructible on
the path that matters. The column bound is tied to the line-length request, so
the recorder's view — which asks for no line length — still emits the exact
rendered stream at any width (§54).

An annotation wider than the whole pane still overruns, because there is
nowhere for it to go. That cuts Mark text, never a byte, and is stated rather
than silently absorbed.

**Consequences:** the invariant — no pane width, grouping or annotation
splits a byte — is checked across all four dimensions crossed, and was
confirmed capable of failing before the fix.

A reference implementation must be configured as the thing it references. The
one-shot comparison for the incremental row cache was not, so the equality it
asserted could be satisfied by both sides being wrong, and for Hex at a narrow
pane both sides were. That is a property of any such pairing, not of this
one.

**Known structural risk.** Hex still wraps in two stages: cell-aware rendering
followed by a generic character splitter. Two successive defects have now lived
in the seam between them. If this path is touched again, the renderer should
return final rows and `split_stream_rows` should not see Hex at all.

### ADR-042 — In Hex, a Mark takes a row of its own

**Status:** Accepted 2026-08-08.

**Context:** ADR-041 made an annotated Hex line fit the pane, which fixed the
byte being cut in half. It did not address why the annotation was hard to place
to begin with.

Hex is a fixed-width grid, and the grid is the reason to read it: byte *N* sits
at a predictable column, so a reader can follow one field down a column across
many rows. A `Mark` is text of arbitrary width. Splicing it into a byte row
pushes everything after it to an unpredictable column and forces an early wrap,
so row lengths stop matching. The result was correct — no byte was split — and
still destroyed the affordance the mode exists for.

The `NmeaZda` style makes it concrete: `$GPZDA,120000.00,08,08,2026,00,00*4F`
is about 37 columns. At 80 columns with one byte per group a row holds roughly
26 bytes, so one `Mark` costs more than an entire row of data.

Removing `Mark`s from Hex was considered and rejected. Hex is the mode used for
binary protocols, which is exactly where "when did this pattern occur" is asked;
and rule behaviour that silently changes with the display mode is a matrix the
user has to carry.

**Decision:** In Hex, an annotation is a row, not a cell. The byte run ends, the
`Mark` text occupies its own line, and bytes resume on a fresh row and a fresh
group.

The break is **at the marked byte**, not at the next row boundary. A `Mark`
identifies a byte; placing it only near the right row would answer a coarser
question than the one asked. This costs a short row, which is acceptable because
the row is self-explaining — the reason for it is on the line immediately below.

`Mark` text that already ends in CR/LF gets no second break, so a multi-line
annotation still renders as written.

**Consequences:** the column-width interaction that produced ADR-041's defect
cannot recur for byte rows, because no annotation shares one. The invariant a
byte row can now assert is stronger than "its hex tokens are whole bytes": a
byte row is *only* bytes and separators, and the property test asserts that
across grouping, pane width, `Mark` width and placement.

ADR-041's column bound is kept. It is no longer the thing standing between a
`Mark` and a split byte, but it remains the renderer enforcing "never wider than
the pane" for itself rather than trusting the resolved byte count to agree.

A `Mark` wider than the pane still overruns and is cut by the character
splitter. That cuts annotation text, never a byte, and is the one overrun this
design accepts.

The `.disp` recording follows the display, as it always has: a Hex recording now
carries `Mark`s on their own lines. It was never byte-exact — that is `.raw`,
whose sidecar (`.raw.idx`, ADR-039) remains the precise, non-perturbing way to
timestamp bytes.

### ADR-057 — The Receive diagnostics card has no status badge

**Status:** Accepted 2026-11-13. Amends ADR-033, whose card carried the badge.

**Context:** Talker removed its card badge (talker ADR-066) once every reason
it rose for was found marked elsewhere. Listener's badge rose only from the
Transport and Pressure rows. Both mark themselves with ⚠ when they want
attention, and kernel drops and Serial stalls also have their own callouts.
Its calm words were said elsewhere as well: MONITORING by the Channel status
at the top of the pane, LAST RUN by the Pipeline row's "~final 10 s", and
AWAITING DATA by the rows' own "awaiting".

**Decision:** The card is drawn with `wiredata_ui::diagnostics::diagnostic_card`,
with no title and no badge. `wiredata-ui` no longer has a badge at all.

**Boundary:** ADR-033's other decisions stand: the three rows, the bounded
claims, and the callouts.

**Consequences:** The card starts with the Transport row, a row higher.

### ADR-059 — Display recordings mark their gaps; fault-finalization is gone

**Status:** Accepted 2026-11-13. Completes ADR-043.

**Context:** §142 still said a recorder's `finalize` would "write the
truncation marker (for faults)", driven by a `RecordingStopReason`. ADR-043
had already turned recording faults into gaps that continue in a new segment:
nothing ended a recording with a fault, every `finalize` ignored its reason,
and no recorder wrote a marker. A gap was recorded only in the event log and
diagnostics, so a `.disp` file read on its own showed no sign of one.

**Decision:**

- `RecordingStopReason` and the `finalize` parameter are removed; a stop
  always writes the accepted backlog, then finalizes.
- When a Display Recording resumes after a gap that lost bytes, the new
  segment starts with one line, `‹GAP <reason> · <start> to <end> · not
  recorded›` (§54), in the style of the `‹MARK›` lines but with no leading
  newline, since a resumed segment is always a fresh file.
- It is a per-recording choice, **Mark gaps** (`mark_gaps`), on by default:
  after a long run, a gap is what a reader most needs to find. A profile
  saved before the option existed loads with it on.
- A Raw Recording never gets the line: `.raw` stays exactly the bytes
  received (§53).

**Boundary:** No line is written for a gap that lost no bytes, or for a gap
still open when recording stops; the event log reports both, as before. The
controller stays generic over its items: the recorder side writes the line,
through a hook Raw recorders leave empty.

**Consequences:** A `.disp` file says where its gaps are without the event
log beside it.
