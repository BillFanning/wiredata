# Listener — documentation revision history

Superseded revision notes for [listener_specification.md](listener_specification.md), newest first.

They are kept out of the specification itself so that document states **current
behaviour** and carries its version in one place. Each entry records what changed
and why at the time it changed; where a decision stands behind it, the ADR named
in the entry is the authority. Nothing here is normative — if this disagrees with
the specification, the specification is right and this is history.

---

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
