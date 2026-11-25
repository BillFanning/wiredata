# Architecture Decision Record — Listener

**Crate:** listener
**Status:** Draft (tracks the listener spec)

---

This file is the home for `Listener`'s architecture decisions. Some decisions are
authored inline in the spec where the surrounding context lives — those are listed
here with a pointer to the authoritative section in
[`listener_specification.md`](listener_specification.md) rather than duplicated. Workspace-
and `talker`-level decisions live in [`talker/docs/ADR.md`](../../talker/docs/ADR.md);
`nmea0183` decisions in [`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md). Listener
keeps its own ADR numbering (it is a separate crate), so Listener's ADR-001 is *not*
the same as talker's ADR-001.

---

## ADR-001 — Concurrency model: Tokio hybrid

**Authoritative text:** spec §97.1 (with §97.2 blocking-to-async handoff, §99 backpressure, §111 shutdown). Summarized here so the decision is discoverable from the ADR index.

**Decision:** A hybrid runtime. Tokio owns orchestration — command handling, cancellation, bounded queues, fan-out, async-native network I/O, shutdown. Continuous blocking serial receive loops run on dedicated OS threads that own the interface handle and hand data to the async side through bounded `tokio::sync::mpsc` channels (`try_send`, with a bounded retry loop while the queue is full — see ADR-007's refinement: the retry loop lets the stall notice fire mid-stall and observes cancellation). `spawn_blocking` is reserved for bounded, finite operations (open/close, file create/flush, port enumeration) — never for continuous receive loops. Transports are push-based: data-bearing transports emit `ReceivedData`. (TCP listeners once emitted `NewConnection`; the TCP code was removed by ADR-047.)

**Contrast with `talker`:** talker deliberately uses **no** async runtime (talker ADR-002 — `std::thread` + `crossbeam-channel`), because it manages a bounded set of *outbound* senders against synchronous `serialport`/`eframe` APIs. Listener faces the opposite shape: many *inbound* network sources and async-native socket I/O — where Tokio's orchestration earns its keep. The two crates therefore reach opposite conclusions for the same reason (fit the runtime to the I/O shape), and that divergence is intentional.

**Consequences:**
- `blocking_send` backpressure can stall a serial reader → UART/driver overrun, reported as transport-specific loss (§99, §101).
- Continuous blocking loops need a bounded read timeout so they can observe cancellation; shutdown never relies on interrupting an in-progress blocking read (§111).
- Timing is chunk-granular (`ChunkTime`, §138), not true per-byte hardware timing; recording timestamps and liveness derive from it (§26, §57, §166). *(Originally "per-message timing in the extractor" — both removed by ADR-010.)*

---

## ADR-002 — Messages are immutable; decoders are read-only

**Retired 2026-11-14.** Messages and decoders were removed by ADR-010; their surviving principles are spec §103 (received chunks are immutable and shared) and §5.4 and §5.5 (display and recording never alter received bytes). Text in [REVISIONS.md](REVISIONS.md#retired-decisions).

---

## ADR-003 — Profile schema: single monotonic `schema_version`

**Authoritative text:** spec §72.1.

**Decision:** Profiles carry a single monotonically increasing `schema_version: u32`. Each binary knows one current version. Additive changes within that supported schema load via field defaults; a profile newer than the binary is refused. A breaking older schema is refused unless an explicit migration for that schema has been implemented. This mirrors `talker`'s clean-break v2 behavior (talker ADR-013) deliberately, so the two tools share one mental model.

---

## ADR-004 — Single crate, modular internals (resolves OQ-L1)

**Authoritative text:** spec §127–§128 (revised in listener spec v1.1.1).

**Context:** Spec §127 originally sketched a twelve-crate split (`listener-core`, `listener-runtime`, `listener-transport`, `listener-extract`, `listener-decode`, `listener-display`, `listener-record`, `listener-retention`, `listener-config`, `listener-diagnostics`, `listener-cli`, `listener-gui`) and nested `nmea0183` inside `listener`. The crate actually exists as a single `listener` crate alongside `talker` and `nmea0183`.

**Decision:** Keep **one `listener` crate**. The twelve `listener-*` units are realized as modules under `src/` (`core/`, `transport/`, `display/`, `record/`, `runtime/`, …; `extract/` and `decode/` were later removed by ADR-010), preserving the §128 boundaries and dependency direction; only the packaging is collapsed. `nmea0183` is a top-level workspace sibling (shared with `talker`), referenced as an ordinary dependency — not nested. The `lib.rs` + thin-`main.rs` shape follows talker ADR-014, keeping module APIs unit-testable.

**Consequences:**
- §127 is the literal `src/` module map; §128 boundaries are normative whether a unit is a module (now) or a crate (later).
- A later split is non-disruptive: extract a module into a `listener-*` member crate when an external consumer or compile-time concern justifies it. That future split, if taken, gets its own ADR.

## ADR-005 — Delimiter extraction emits a Message at every delimiter, including empty payloads

**Retired 2026-11-14.** Delimiter extraction was removed by ADR-010, so there is nothing left to emit. Text in [REVISIONS.md](REVISIONS.md#retired-decisions).

## ADR-006 — Runtime observability & fault-state ownership

**Authoritative context:** spec §94/§101 (transport fault reporting), §137
(`RuntimeEvent`; the §136 `RuntimeCommand` enum was later removed by ADR-012 — the
command surface is the `Listener` method API), §8/§9 (state machine).

**Context:** A spontaneously-faulting transport (a read/accept error, not a
commanded stop) is detected by a per-channel **fault monitor** — a detached
`tokio` task that owns the transport join handle (ADR-001 / `spawn_monitored_channel`).
The orchestrator (`runtime::Listener`) is **method-based**: it has no background
event loop, so the monitor cannot mutate its state. Before this decision the
monitor only emitted `ChannelFaulted`; `Listener`'s stored `ChannelState` stayed
`Running` until the next command, so `state()` could report `Running` for a
channel whose transport had already died.

Three models were considered:

1. **Event-only (lazy).** The monitor emits `ChannelFaulted`; the stored state is
   reconciled only when the next command runs. Simplest, but `state()` lies and
   command validation can act on a stale state.
2. **Actor / event-loop owner.** Turn the runtime into a task that consumes its
   own `RuntimeEvent` stream and owns all state. Clean single-writer model, but a
   significant architectural commitment for v1.
3. **Shared per-channel state cell.** The monitor flips a lightweight shared flag
   that the orchestrator reads when reporting state and validating commands.

**Decision:** Adopt **model 3**, with a clear division of responsibility:

- **`RuntimeEvent` is the presentation surface, but advisory — not a guaranteed
  ledger.** The CLI/GUI fold the event stream into their own view models; they do
  **not** read or own transport or pipeline state. Events are sent non-blocking
  (`try_send`) so a slow/saturated observer can never stall the runtime — which means
  an event *may be dropped* under saturation. The design tolerates this because the
  **truth lives elsewhere and is re-derivable**: durable facts (diagnostics, recording
  state, liveness, the effective channel state) live in the periodically-polled
  **snapshot**, so a dropped event self-corrects within one poll (≤200 ms at 5 Hz).
  Events are best read as *wake-up / status hints* that make the UI feel live between
  polls, **not** as the authoritative record. (If a future requirement needs
  exactly-once lifecycle delivery, split critical lifecycle events onto an awaitable
  channel — see model 2 — rather than treat the advisory stream as a ledger.)
- **`Listener` keeps internal channel state only for command validation and
  lifecycle control** (enforcing the legal §9 transitions).
- **Spontaneous transport faults reconcile that internal state through a
  lightweight shared per-channel flag** (`Arc<AtomicBool>`). The fault monitor
  sets the flag *and* emits `ChannelFaulted`; `Listener::state` and command
  validation treat a set flag as `Faulted` (an "effective state" overlaid on the
  stored lifecycle state). A fresh flag is installed at each `start`; `stop`
  clears it (`Faulted → Stopped`, §8.5). No actor/event-loop rewrite.

We deliberately do **not** adopt model 2 yet. If GUI requirements prove that
events plus on-demand snapshots are insufficient — e.g. the runtime must *push*
rich incremental state — revisit with a new ADR; the shared-flag design leaves a
clean path and does not foreclose it.

**Consequences:**
- `state()` is accurate immediately after a spontaneous fault, without a
  background runtime loop.
- The stored `ChannelState` plus the fault flag together form the *effective*
  state; everything user-facing (`state`, `start`/`stop` validation, `shutdown`)
  reads the effective state.
- Reconciliation covers **data channels** (serial/UDP) via the fault monitor. A
  TCP **listener-acceptor** fault is not reconciled through this flag in v1 (its
  supervisor reports per-connection faults as events); revisit if acceptor faults
  need to surface as listener state. (TCP receive code removed 2026-11-14, ADR-047.)
- Live readout of retained content (the stream scrollback / diagnostics under
  ADR-010; originally retained messages / decoded metadata) while a channel runs
  still requires the on-demand snapshot API — built since, and the message-era
  parts of this ADR are superseded by ADR-010.
- **Diagnostics are runtime-written, GUI-rendered (single writer).** Every diagnostic
  is recorded once in the pipeline's `DiagnosticLog` with a real timestamp; the GUI
  never synthesizes entries — it renders the `ChannelSnapshot.diagnostics` it polls.
  Three delivery details follow from the method-based (no event-loop) runtime: (a) at
  **stop** the orchestrator takes one *final* snapshot of the returned, already-finalized
  pipeline (the 5 Hz poll can't fire during the synchronous stop) so the stop-time
  Channel-Stopped/Recording-Stopped events reach the GUI; (b) a Channel's diagnostics are
  **retained across its own stop→start** (spec §89.1) in a per-channel `retained_diagnostics`
  vec, served via a minimal snapshot while not running and replayed (`DiagnosticLog::seed`)
  into the next pipeline; (c) a **start fault** that never runs a pipeline is appended to
  that vec as an Error diagnostic so it shows and survives a restart. Pinned by
  `diagnostics_are_retained_across_a_stop_start_cycle`,
  `a_start_fault_is_retained_as_an_error_diagnostic`, and the loopback final-snapshot test.
- Pinned by tests: `runtime::channel` (`is_faulted` after a fault) and
  `runtime::listener` (`state()` reconciles to `Faulted`, then clears on `stop`).

**Correction (2026-07-12).** The claim above that "the effective channel state
live[s] in the periodically-polled snapshot" was **aspirational, not true as
built**: until now neither `ChannelStats` nor `ChannelSnapshot` carried a
`ChannelState`, and both lanes went silent (`None`) for a non-running channel —
so a dropped lifecycle event did *not* self-correct; the GUI row stayed stale
(external review round 2, High). Now made true as written: both structs carry
`state` (the orchestrator stamps `effective_state()` over the pipeline's
placeholder — lifecycle belongs to the orchestrator, not the pipeline) plus
`reconnect_pending` (so a polled `Faulted` distinguishes "still retrying" =
Reconnecting from "not retrying" = Faulted without depending on the
advisory reconnect events), and `snapshot`/`channel_stats` always serve for a
known channel — live when running, synthesized from the retained
diagnostics/activity otherwise. The GUI reducer reconciles its derived status
from every polled update, leaving the transitional `Starting`/`Stopping` states
to settle on their own. Pinned by
`a_dropped_lifecycle_event_self_corrects_on_the_next_poll`,
`polled_fault_with_reconnect_pending_reads_reconnecting`, and
`transitional_polled_states_do_not_flap_the_row`.

**Correction (2026-11-14):** Events are advisory on the way into the driver,
as stated above, but not on the last hop, from the driver to the GUI. There a
dropped update was not always repeated by the next poll: a lost "Channel
added" left a Channel the GUI never showed, a lost "removed" left a ghost, and
a lost "reconfigured" left an old editor baseline. One-time results now travel
a lane that never drops, and only the updates the next poll repeats are
dropped (ADR-066).

## ADR-007 — Transport data-loss observability boundary

**Authoritative context:** spec §99 (backpressure matrix), §100 (reception
priority), §101 (data-loss reporting), §97.1 / ADR-001 (the blocking serial
reader), §56.1 (recorder fault), §137 (the fixed `RuntimeEvent` set).

**Context:** §101 requires Listener to report discarded data (channel, time,
overflow type, *estimated loss where practical*) but is explicit that some loss
"is not reliably observable from userland; Listener reports the loss it can
detect rather than guaranteeing detection of all loss." We need a precise,
honest line between what we report and what we cannot — and to avoid both silent
loss and fabricated loss counts.

**Decision:** Classify transport-relevant loss into three tiers and fix the
reporting contract for each.

1. **Detected and reported, with a known truncation point — recorder overflow
   (§56.1).** Under sustained overload Listener preserves reception and *faults*
   the affected recording rather than stalling the reader or writing a gapped
   file. The recorder emits `RecordingFaulted(ChannelId)` and records the
   truncation point; the artifact is contiguous and byte-exact up to a known end.
   *Status: implemented, then superseded by ADR-043 (2026-09-30):* an overflow
   now opens a recorded gap and the recording continues in a new segment,
   rather than faulting the recording.

2. **Detectable as an event, not quantifiable — a sustained reader stall on the
   Transport→Extractor edge (§97.1, §99).** This is the only edge permitted to
   backpressure the reader. In process it *stalls, never drops* — zero loss
   inside our queues. But a stall long enough lets the OS/UART receive buffer
   overrun, losing bytes *upstream of us*. We can detect the stall; we cannot
   count the lost bytes (no portable userland signal for UART/driver overrun).
   **Contract:** the serial reader watches its send to the full queue; a stall
   beyond a heuristic threshold (`STALL_WARNING`, 250 ms) sends a
   `TransportNotice::ReceptionStalled { channel_id, stalled_for }` once per stall
   episode — self-describing, carrying the observed stall **duration** (the honest
   §101 proxy) and **no fabricated byte count**. The pipeline records it as a
   Warning `Diagnostic` and emits `ReceptionStalled` (see the seam below). Momentary
   backpressure that drains quickly is normal and must not notify. A stall that
   notified and then ends because the send goes through sends
   `TransportNotice::ReceptionResumed` with its full length, under the same
   non-blocking rule: the first notice can only say the threshold was passed. *(Refined: the
   stall is a bounded retry loop, not a parked `blocking_send`, so the notice is
   raised **while the stall is ongoing** — a permanently wedged pipeline is not
   silent — and cancellation is observed mid-stall, §111.)*
   *Status: implemented and tested* (`sustained_stall_sends_a_reception_stalled_notice`,
   `a_wedged_pipeline_raises_the_notice_while_still_stalled_and_can_cancel`,
   `momentary_backpressure_updates_totals_without_a_warning`,
   `run_channel_records_a_transport_notice_as_a_diagnostic`).

3. **Fundamentally unobservable — pre-receive kernel/NIC loss.** Kernel-dropped
   UDP datagrams, and any bytes lost in the driver/NIC before our `recv`, leave
   no reliable userland signal. Listener **does not report** what it cannot
   detect and **does not fabricate** loss for data it never saw: it numbers and
   reports exactly what it received (§24). Quantifying this is out of scope for
   v1; reducing it is an operational concern (socket buffer sizing), not a
   reporting one.

**Event signal — `WarningRaised` then `ReceptionStalled` (superseded by spec
v1.2).** When this ADR was written the `RuntimeEvent` set was fixed (spec §137), so
a reader stall reused `WarningRaised` — a §93 Warning — rather than a new variant
needing sign-off. **Spec v1.2 added a dedicated `ReceptionStalled(ChannelId,
Duration)` event** (§137) and marked the enum `#[non_exhaustive]`, so the overload
is retired: the stall now emits `ReceptionStalled` carrying the duration. The
retained §95 `Diagnostic` remains the **rich detail surface** (channel, time,
human text); the event is the lightweight signal. *Implemented:* the seam emits
`ReceptionStalled`, and `RuntimeEvent` is `#[non_exhaustive]`. *(The sibling
`RuntimeCommand` enum mentioned in the original was removed by ADR-012.)*

**The transport→diagnostics seam (implements tier 2's record).** A transport states
*what happened* via a `TransportNotice` (in `transport/`, with no dependency on the
diagnostics/event vocabulary); the **pipeline** — the channel's `DiagnosticLog`
owner — decides how it is recorded and reported. `run_channel` drains a bounded
`Receiver<TransportNotice>` in its `select!` (alongside ingest and snapshot
requests) and calls `record_notice`, which writes a Warning `Diagnostic` (naming
the channel and stall duration) **and** emits the spec-appropriate §137 event —
`ReceptionStalled` (carrying the duration) under spec v1.2 — keeping the §95 record
and the §137 event paired in one owner. The notice channel is created for every
data channel. Only the live serial reader receives a sender clone
(`with_notice_sender`), because only serial can stall the reader (§97.1); every
runtime monitor retains the original sender for the terminal-fault contract added
by ADR-020. The §95 record is visible in `ChannelSnapshot.diagnostics`, which is
what a GUI reads.

Why this shape (vs. the alternatives): the transport must not depend on
`diagnostics`/`RuntimeEvent` (§128 layering), so it emits a transport-local notice,
not a `Diagnostic`. The notice is **self-describing** (carries its `channel_id`) so
the pipeline formats and routes it without assuming which channel a notice is for.
The `run()` transport contract is left unchanged — UDP/TCP are async and never
stall the reader, so their transport implementations do not receive a notice
sender; serial attaches one via a builder. Terminal outcomes are joined and
reported by runtime monitors outside that transport contract (ADR-020). (TCP receive code removed 2026-11-14, ADR-047.)

The live-stall notice path is **advisory and bounded** (`TRANSPORT_NOTICES`, 16):
the serial reader uses `try_send` and drops on full, named explicitly because the
dropped item is itself a loss-warning — we accept losing a warning under overload,
but never block the reader to keep one (that would cause the stall it warns of).
A dropped-advisory counter can be added later only if the drop rate proves to
matter. Terminal fault causes later gained a distinct delivery contract in
ADR-020: they share the bounded queue but await capacity after reception ends.

**Consequences:**
- `WarningRaised` was shared with recording-enable failures (§55), so a UI could
  not distinguish them from the event alone. Spec v1.2's dedicated
  `ReceptionStalled` event removes that ambiguity for stalls; the retained
  `Diagnostic` text still carries the rich detail (channel, duration).
- UDP/TCP backpressure does not stall an OS thread (async `send().await`), so
  there is no serial-style stall notice; sustained UDP backpressure manifests as
  tier 3 (kernel drops) and is unreportable by design.
- `TransportNotice` is `#[non_exhaustive]`: future non-terminal transport
  conditions (e.g. a recoverable read hiccup) extend it without a breaking change.

## ADR-008 — GUI↔runtime bridge: a background driver task owns the `Listener`

**Status:** Accepted. **Context:** spec §3 (thin presentation layers), §136/§137
(command/event vocabulary), §10 (observable state); AGENTS §5 ("UI threads never
perform I/O and never block"); ADR-001 (Tokio hybrid) and ADR-006 (events are the
authoritative push surface, snapshots the pull surface).

**Problem.** egui/eframe is a *synchronous, main-thread, immediate-mode* loop
(`eframe::run_native` owns the OS event loop and calls `App::update` per frame). The
`Listener` orchestrator is *async* (`async fn start/stop/snapshot/reconnect_tick/…`).
Calling those from inside `update` via `Runtime::block_on` would block the UI thread
on every interaction — exactly what AGENTS §5 forbids. So the GUI cannot own the
`Listener` directly.

**Decision.** Put a **background "driver" task** between the egui App and the
`Listener`, mirroring talker's UI↔talker-thread command/status split (AGENTS §5)
adapted to listener's Tokio model:

- A dedicated Tokio runtime (its own thread) **owns the `Listener`**. It runs a loop
  that: drains a **command channel** (GUI → driver) and calls the matching `Listener`
  method; forwards the `Listener`'s `RuntimeEvent` stream to the GUI; and on a timer
  (a few Hz) polls `snapshot(id)` for each running Channel and pushes the result to
  the GUI. It also drives `reconnect_tick` (the loop the CLI already runs).
- Two message types cross the boundary:
  - `UiCommand` (GUI → driver): `Start/Stop/ApplyPending/AddChannel/EnableRecording/
    PauseDisplay/ResumeDisplay/SetRts/SetDtr/SetMatchRuleEnabled/MarkNow/Shutdown`.
    This is the path that finally needs a command channel **into the running
    pipeline** for the dynamic §165/§161 actions (live rule-toggle, MarkNow,
    mid-run recording) — see the deferred items; building it is part of this work.
  - `UiUpdate` (driver → GUI): `Event(RuntimeEvent)` and
    `Snapshot(ChannelId, ChannelSnapshot)`.
- The driver holds a clone of `egui::Context` and calls `request_repaint()` when it
  pushes an update, so a streaming source wakes the UI without the App busy-polling.
- The egui App is **pure presentation**: it folds `UiUpdate`s into a testable
  per-Channel view-model (`AppState::apply`), lays out widgets reading that model,
  and emits `UiCommand`s on interaction. No `Listener`, no I/O, no `block_on`.

**Why not the alternatives.**
- *`block_on` in `update`* — violates AGENTS §5 (UI blocks on orchestration and on
  every async snapshot); also fights egui's frame budget.
- *GUI owns the `Listener` directly* — impossible cleanly: the methods are `async`
  and take `&mut self`; the immediate-mode loop has nowhere to `.await`.
- *Snapshot-on-request from the UI thread* — would still block or require an async
  round-trip per frame; the driver's timer-push keeps the UI reading owned, already
  -current view-models (ADR-006's pull surface, fetched off the UI thread).

**Consequences.**
- The business logic (command handling, view-model fold) lives in testable structs;
  the `gui/` egui code stays thin (AGENTS §5) and is exercised only by eye.
- Channels carry **owned** snapshots/events, so the UI never shares mutable pipeline
  state and a slow UI can never stall reception (the driver's periodic pushes are advisory,
  `try_send`/drop-newest like the other observer edges, §99; its one-time
  results are never dropped, ADR-066).
- The GUI's `UiCommand` is the command surface from the App's side; the driver
  translates it into direct `Listener` async method calls. _(Superseded in part by
  ADR-012: there is no separate `core::RuntimeCommand` enum to "align with §136" — the
  method API is the command surface. The missing **command channel into `run_channel`**
  is still the seam that unblocks the deferred §165 live actions, built as `Listener`
  methods + an internal pipeline command, not a top-level command enum.)_
- GUI-only state (window geometry, last layout) uses eframe's built-in persistence,
  never the profile schema (mirrors the talker rule).

**Build order (small, reversible first):** (1) deps + `--gui` dispatch + a minimal
window [this step]; (2) the driver + `UiCommand`/`UiUpdate` + a pure `AppState`
reducer, unit-tested, no egui; (3) a one-Channel vertical slice (list + start/stop +
a live snapshot pane); (4) breadth by mapping snapshot fields to panes.

---

## ADR-009 — The live viewer is fed from the verbatim byte stream

**Status:** Accepted. Trimmed 2026-11-14 to the decision that survives; ADR-010 removed
the Message half this entry was written beside, and ADR-011 replaced its delivery.

**Context:** The live viewer was first wired to extracted, numbered Messages, which is
the wrong source for wire troubleshooting. Delimiter extraction strips the terminator
that defines each NMEA sentence, so the viewer could not show what is on the wire.
Stream-mode channels produced no Messages at all. And non-UDP reads return whatever
the OS has when it has it, so a read can split a sentence anywhere, and a view
segmented by Messages or chunks diverged between serial and UDP for the same data.

**Decision:** The live viewer is fed from the verbatim pre-extraction byte stream.
A bounded per-Channel byte ring is appended in `Pipeline::ingest`, a non-blocking tap
alongside the raw recorder (§53, §99.1). The GUI renders it as one continuous,
selectable view through the `DisplayView` renderer: Rendered honors the data's real
CR/LF (§44), Hex is a bytes-per-line dump, Raw shows control pictures.
Concatenating chunks in arrival order reconstructs the exact wire stream regardless of
read-chunk boundaries, because nothing is reframed. Serial and UDP therefore behave
identically, and every Channel has a working viewer.

**Why not the alternatives.** Rendering from extracted Messages loses the delimiters
and segments on arbitrary frame boundaries. A pseudo-stream built from Message history
has the same source defect. Reusing the raw-recording path would write to disk, where
the viewer needs an in-memory, bounded tail.

**Consequences:** The ring is capped by bytes, since there are no Message boundaries to
count. Stream view has no per-message prefix or numbers (§18). How the tail reaches
the GUI is ADR-011's decision, and ADR-010 holds the removal of the Messages source
this entry once planned to restore.

---

## ADR-010 — Stream-only architecture: the Message infrastructure is removed (spec v2.0)

**Status:** Accepted. **Context:** spec **v2.0** (revision note + §17–18, §40–46, §50.2, §51–59), which supersedes the Message-mode half of v1.x. ADR-009 (the Stream display source) was the first step; v2.0 finishes the trajectory by removing the Message half rather than keeping both.

**Decision.** `Listener` is a pure **stream** tool. Received bytes are one verbatim stream that is displayed (Raw/Rendered/Hex), searched (Find & Triggers), and recorded (Raw `.raw` + Display `.disp`). Removed: Message Mode, Message Extraction (delimiter / fixed-length / protocol), Message Numbering, decoders + the `nmea0183` dependency, integrity metadata, message-framed recording (`.ssdat`) + subsampling, the Messages display source, and message-keyed Match conditions. Find & Triggers re-root on the byte stream — `BytePattern` (cross-chunk scan) + `Idle` conditions; `Highlight` (byte range), `Record`, `Notify`, `Mark` actions anchored on **byte offset**. *(The `Highlight` action was later removed — see ADR-015.)*

**Why.** The actual use (single-stream troubleshooting; long-run multi-channel logging) is stream-centric. The extraction→decode→message spine added surface (framing config, decode config, numbering, `.ssdat`) the workflow never used, and made the wire view harder to keep faithful (delimiter extraction strips the very bytes that define a sentence). A stream-only tool is simpler, smaller, and a better fit; `nmea0183` lives on for `talker`.

**Consequences.**
- `extract/` and `decode/` modules are removed; `listener` drops its `nmea0183` dependency.
- The pipeline collapses to: transport → bounded pipeline queue → non-blocking fan-out (raw recorder, display/scrollback, display recorder, find/triggers, diagnostics). The single backpressure edge (§99) is now **Transport→Pipeline**.
- `ChannelConfig` loses `extraction` and `decoder`; `RecordingConfig` loses `subsample`; `DisplayViewConfig` loses `source`/`annotations`/`subsample`/`timestamp`; `RetentionConfig` is byte-based. `schema_version` bumps (breaking; v1 profiles refused).
- Recording extensions: **`.raw`** (was `.dat`) and `.disp`; `.ssdat` gone.
- The GUI loses the framing selector, decode toggle, Messages-source switch, and per-message toggles (some only just built); the Stream viewer, byte-based liveness, recording UI, and pause all stay.
- Reversible in git; low-regret — the message path was unused in the workflow.

**Build order.** (1) spec rewrite to v2.0 [done]; (2) strip the runtime (remove `extract/`/`decode/`, collapse the pipeline, byte-based retention); (3) trim the config schema + GUI; (4) re-root Find & Triggers on the stream; (5) test cleanup.

## ADR-011 — Live stream delivery is incremental, not bundled in the snapshot

**Status:** Accepted. **Context:** spec §87 (bounded stream scrollback), §100 (reception priority / non-blocking observers), §166 (liveness); ADR-006 (snapshots are the pull surface), ADR-008 (the GUI↔runtime driver polls at ~5 Hz), ADR-009 (the live viewer is fed from the verbatim stream).

**Problem.** ADR-009's `ChannelSnapshot` bundled the whole stream scrollback (`stream_tail: Arc<[u8]>`, capped at the byte-retention limit — ~1 MB by default). The ADR-008 driver polls the selected channel's snapshot at 5 Hz, and the GUI re-rendered the tail into one selectable `egui::Label` each frame. Both costs scaled with the *buffer*, not with new data: at 5 Hz a full-buffer clone shipped continuously, and a non-virtualized ~1 MB selectable label stalled the UI. The symptom (reported) was the GUI becoming unresponsive after a steady low-rate source had run long enough to fill the scrollback (~minutes) — confirming the cost was buffer-fill, not throughput. This is the opposite of what a high-throughput acquisition tool needs.

**Decision.** Split the pull surface so nothing is O(buffer) in steady state:
- The **snapshot carries only the small, bounded observable state** — diagnostics, recent match firings, view pause, recording state, liveness, and the stream's `stream_end_offset` (a cursor target). It no longer carries the scrollback bytes.
- The scrollback is read **incrementally** through a new `PipelineRequest::StreamDelta { since }` → `StreamDelta { generation, base_offset, bytes, end_offset }`: only the bytes at/after the consumer's absolute cursor. A cursor behind the (bounded) retained window returns the whole window with `base_offset > since` — a **reset** signal, not an append. The pipeline tracks `stream_dropped` (bytes evicted from the front) so an absolute offset locates a byte in (or past) the ring in O(returned bytes). `generation` is a process-unique identity minted for each pipeline run because offsets legitimately restart at zero.
- The **driver** retains a `(generation, offset)` cursor per channel across selection changes, so revisiting a tab does not re-ship its retained window. It advances a non-empty delta only after the bounded UI update channel accepts it; a dropped update is fetched again rather than becoming a permanent display gap. Caught-up empty deltas remain no-ops, except that the first empty delta from a new generation is forwarded so a restart clears old bytes before new data arrives. Start/reconnect lifecycle events eagerly forget the cursor, but generation remains the correctness boundary if such an advisory event is delayed or dropped.
- The **GUI** stores the explicit absolute offset of `stream_bytes[0]` as well as the end cursor. It validates each delta range, resets on generation change or a forward gap, ignores wholly stale/duplicate same-run deltas, and appends only the unseen suffix of an overlap. Front eviction advances the explicit base. The invariant is always `cursor - base == retained length`; no offset is reconstructed as `cursor - length` after folding an untrusted or stale delta. Rendering remains memoized by cursor + view mode and **virtualized** with `ScrollArea::show_rows` so only visible rows are laid out.

End to end the steady-state cost is now: ingest O(chunk), snapshot O(small bounded state), stream delta O(new bytes), GUI render/layout O(new bytes)/O(visible rows). Nothing re-touches the whole buffer.

**Why not the alternatives.**
- *Keep the tail in the snapshot but cap the rendered region.* Still clones the (capped) region every poll and re-renders a fixed slab each frame — O(slab), not O(new); and it drops scrollback-to-start from the live view. Incremental is strictly cheaper and keeps the full window.
- *Diff the tail in the GUI against the last snapshot.* The expensive clone (snapshot→GUI, 5 Hz) would remain; only the render would be saved. The waste is on the wire, so the fix belongs at the request boundary.
- *Virtualization alone.* Fixes the layout stall but leaves the 5 Hz full-buffer clone. Necessary but not sufficient; we do both.

**Consequences.**
- `ChannelSnapshot.stream_tail` is removed; `stream_end_offset` replaces it. Tests that asserted verbatim bytes now fetch via `stream_delta` (a `#[cfg(test)]` `stream_tail()` accessor remains on the pipeline for unit tests).
- A new `UiUpdate::StreamDelta` rides beside `Snapshot`/`Stats`; the App folds it into per-channel accumulated bytes.
- The reset-on-eviction contract (`base_offset > since`) is the consumer's signal to re-seed rather than append; restart is identified independently by generation, so reused offsets cannot be mistaken for stale same-run data.
- This refines ADR-006's pull surface exactly along the "push rich incremental state" axis that ADR-006 left open; no actor/event-loop rewrite was needed.
- Pinned by tests: `pipeline` (`stream_delta_serves_only_new_bytes_since_a_cursor`, `stream_delta_resets_when_the_cursor_was_evicted`), `gui::state` (`stream_deltas_accumulate_incrementally`, `stream_delta_reset_on_eviction_replaces_rather_than_appends`, `overlapping_delta_behind_the_cursor_does_not_underflow`, `malformed_delta_range_is_ignored_without_disturbing_the_window`, `overlapping_delta_appends_only_its_unseen_suffix`, `a_new_generation_replaces_old_bytes_even_without_a_lifecycle_event`, `restart_clears_accumulated_stream`), plus the bridge's empty-new-generation forwarding assertion.

## ADR-012 — The command surface is the `Listener` method API; no `RuntimeCommand` enum

**Status:** Accepted. **Context:** spec §136 (Runtime Commands), §3 (UI owns no runtime state), ADR-006 (events are the push surface; commands are direct method calls), ADR-008 (the GUI↔runtime driver translates `UiCommand` into `Listener` calls).

**Problem.** `core::RuntimeCommand` was defined (spec §136) as the UI→runtime command vocabulary, mirroring `RuntimeEvent`. But it was never constructed or matched anywhere — ADR-008 itself called it "vestigial … commands are direct `Listener` methods," anticipating a future where the GUI bridge would "finally align it with §136 and dispatch it for real." That future did not arrive, and the architecture that *did* land makes it redundant:

- The runtime exposes commands as **async `Listener` methods** (`start`/`stop`/`apply_pending`/`pause_display`/`set_rts`/…). They take `&mut self` and `.await`; this is the real, tested command API used by both the CLI and the GUI driver.
- The GUI has its **own** `UiCommand` enum (`gui::bridge`) — its on-the-wire form across the App↔driver channel — which the driver translates into those method calls. `UiCommand` is already *richer* than `RuntimeCommand` ever was (`AddChannel`, `RemoveChannel`, `Rename`, `Reconfigure`, `Select`, `SaveProfile`/`LoadProfile`), so `RuntimeCommand` is not even a superset to grow into.

A second, parallel command enum on top of a working method API + a GUI transport enum is a layer with no callers — exactly the kind of spec-vs-code drift the project guards against.

**Decision.** Remove `core::RuntimeCommand`. The **command surface is the `Listener` async method API**; the GUI's `UiCommand` is the presentation-layer transport the driver maps onto it (ADR-008). `RuntimeEvent` is unaffected — it is genuinely the push surface (ADR-006) and stays in `core::command`.

**Why not the alternatives.**
- *Keep the enum and wire it for real (the original §136 intent).* Would add a dispatch layer parallel to the working method API and the GUI's `UiCommand`, for no capability gain — two enums and a method API all expressing the same operations. The deferred live actions (`SetMatchRuleEnabled`, `MarkNow`, mid-run record toggle) need a **command channel into `run_channel`** (ADR-008), not a top-level orchestrator enum; that seam is where they will land, as new `Listener` methods + an internal pipeline command.
- *Keep it as documentation only.* Leaves an exported, untested type that reads as load-bearing and re-accretes the drift on the next audit.

**Consequences.**
- `core::RuntimeCommand` and its `pub use` are gone; `DisplayViewId` (since removed by ADR-060) is no longer imported by `core::command` (only `RuntimeEvent`'s `MatchRuleId` remains). No functional change — nothing referenced the enum (160 lib + 6 profile + 7 integration tests, clippy `-D warnings`, fmt all unchanged-green after removal).
- The live-control work (§165, mid-run recording) is unambiguously specified by this ADR: add `Listener` methods + the `run_channel` command channel — not a `RuntimeCommand` variant. *(Mid-run recording shipped this way: `Listener::set_recording` → `PipelineRequest::SetRecording`, commit `ae7e541`; Display recording followed with `set_display_recording` → `SetDisplayRecording`, and the match-rule `Record { Display | Both }` actions drive the same pipeline paths. Live match-rule toggle / `MarkNow` remain to do.)*
- **Supersedes** the ADR-008 note that the bridge would "align `RuntimeCommand` with §136 and dispatch it." It won't; `UiCommand` is that bridge.
- Spec §136 is amended to document the method-API command surface in place of the enum (version-bumped with a revision note, per the workspace versioning rule).

## ADR-013 — Raw and Display recording are independently configured

**Status:** Accepted. **Context:** spec §53 (Raw recording), §54 (Display recording), §79 (Recording Configuration), ADR-010 (the v2.0 stream-only strip), ADR-012 (live recording via `set_recording`).

**Problem.** `.raw` and `.disp` recording were always **architecturally separate**: Raw taps the verbatim received byte stream (before the old `extract()`), while Display records the *rendered* view output downstream. In `ChannelPipeline::ingest` they are still **separate fan-out taps** to this day. But the v2.0 strip, collapsing `extract()`/decode away, left a single `RecordingConfig` with one `mode: RecordingMode` (Disabled/Raw/Display/Both) and **one shared** `destination`/`file_rotation`/`overwrite`/`timestamps`. So a user could not, e.g., record Raw to one file and Display to another, and the GUI had to cram both behind one mode radio. The merge was only ever in the *config*, never the data path.

**Decision.** Split the config to match the data path. `ChannelConfig.recording: RecordingConfig` becomes two independent fields:
- `raw_recording: RawRecordingConfig` — `enabled` + its own `destination`/`overwrite`/`rotation`/`timestamps` + the `disk_guard` (the guard protects long Raw captures, §168).
- `display_recording: DisplayRecordingConfig` — `enabled` + its own `destination`/`overwrite`/`rotation`/`timestamps`.

`RecordingMode` (Disabled/Raw/Display/Both) is retired — its four states are now two independent `enabled` bools, and "Both" is just both enabled (to two destinations), which the old single-destination config could never express.

**Enabled vs. armed.** `raw_recording.enabled` controls *auto-start at channel Start*. The live Record toggle (ADR-012) is **armed by the presence of a destination**, independent of `enabled` — so a channel can be set up to record-on-demand (destination set, `enabled = false`) and toggled at runtime with no restart.

**GUI placement (the change's user-facing intent).** Raw recording gets its own collapsing panel **above** Configure: the header summarizes live state (●/■ + on/off), and inside are the live Record/Stop toggle plus the Raw setup. Display recording lives **under** Configure as "Display record" (it is display configuration). *(Since revised: "Record Display" now sits directly under the Raw block with the same header controls — state glyph, "Record on start", live Record/Stop — and its setup edits apply live like Raw's, via `set_display_recording`/`SetDisplayRecordingConfig`.)*

**Why not the alternatives.**
- *Keep one shared config.* Cannot express independent Raw/Display destinations, contradicts the separate taps, and forces the awkward single mode radio. The split is what the architecture always implied.
- *Migrate old profiles.* A serde shim mapping the old `[recording]`/`mode` table onto the new fields. Rejected for a **clean break** (bump `schema_version` 2 → 3; old profiles refused with "recreate the profile"), consistent with the v1→v2 precedent (ADR-003) — profiles are dev-only today, so no migration code to carry.

**Consequences.**
- `schema_version` bumped to **3**; v1/v2 profiles refused. Spec §79 rewritten (version-bumped with a revision note, per the versioning rule).
- Runtime `build_raw_recorder`/`build_display_recorder`/`record_arming`/disk-guard read their own config; the pipeline data path is unchanged (taps already separate).
- A channel can now run **both** recordings to two destinations at once — pinned by `raw_and_display_recording_run_to_independent_destinations`. Existing rotation, live-record, and enable-failure tests updated to the split config.

## ADR-014 — Recording destinations must be unique; enforced by unique channel names + an OS advisory lock

**Status:** Accepted. **Context:** spec §55 (recording start), §59 (rotation / filename generation), §71 (configuration validation), §79 (recording config), §121 (file safety), ADR-013 (independent Raw/Display recording).

**Problem.** Two Channels can be configured to record to the **same file**. The §121 `OverwritePolicy` guards against clobbering a *pre-existing* file, but it does **not** stop two *live* Channels from opening and interleaving writes into one destination — each passes its own enable check, then both write, corrupting the capture. This is easy to hit: duplicate the obvious destination across two channels, or (because the channel name appears in rotating filenames, §59) run two same-named rotating channels into one folder. The earlier spec explicitly allowed duplicate names (§6: "Channel Names … need not be unique"), which made the rotating-filename collision reachable by construction.

**Decision.** Two complementary layers: the first removes the most common collision by construction, the second is the race-free enforcement point. (A third layer — an in-process pre-check at Start that named the conflicting channel — was prototyped and **removed**: it faulted the whole *channel*, which wrongly stopped reception and showed the bind/port recourse. A recording-destination collision must be a *recording* fault, leaving the channel Running, so enforcement belongs at the recording-arming point, not channel Start.)

1. **Unique, filesystem-safe Channel Names (necessary, not sufficient).** Channel Names become **unique** in addition to the existing filesystem-safe rule (§59/§71). Enforced at add-channel (a per-kind monotonic, never-reused suffix — `UDP_Channel1`, `UDP_Channel2`, …), at rename (a duplicate is not committed and warns inline), and on profile load (a loaded workspace with duplicate names is rejected per §71's per-channel validation). This makes the **rotating** collision impossible: `<channel>_<period>.raw` leaf names cannot collide if names are unique. It does **not** cover the non-rotating case, where the destination is a full path the user typed — different names can still point at the same file.

2. **OS advisory lock for the recording's lifetime.** When a recording opens its file, the recorder takes a cross-platform **advisory exclusive lock** on a companion `<path>.lock` file and holds it until finalize; failure to acquire surfaces as a recording fault (`RecordingFaulted`) that leaves the **channel Running** (reception continues; only recording is off). This catches both two channels here *and* a **second `listener` process**, and closes the start-race window. Implementation notes:
   - The lock is on a **`<path>.lock` companion**, not the data file, so it is independent of the overwrite policy (Refuse must still fail atomically via the data open; Overwrite/Append must not be clobbered by the lock handle) and is taken *before* the data file is opened, so a lock conflict never touches the destination.
   - The lock holder is a **synchronous `std::fs::File`** (using std's own `File::try_lock`, stable since Rust 1.89; no extra crate). A `std::fs::File` closes *deterministically* on drop, releasing the lock the instant the recorder drops — so a Stop→Start can immediately re-lock. A `tokio::fs::File` is unsuitable: it closes the OS handle asynchronously, so its lock would linger past drop and a restart would spuriously fail.
   - The `<path>.lock` companion is **left on disk** (not deleted on finalize) — the conventional lock-file lifecycle; deleting it would race with another holder. It is empty and reused next session, so stray `.lock` files alongside recordings (one per period with rotation) are expected, not a leak.

**Robustness scope (explicit).** On a **local filesystem** the lock is robust on Windows, macOS, and Linux. Two residual gaps are inherent and identical on every OS, not platform bugs: (a) an *unrelated external program* that writes without locking is unaffected on Unix (advisory locks; Windows is mandatory, so it is actually stronger there); (b) over a **network filesystem** (NFS/SMB) advisory locks are unreliable — there the unique-name layer still prevents the rotating collision, and we accept the residual risk for an explicit shared full path. Both are documented, not silently assumed away.

**Why not the alternatives.**
- *Rely on `OverwritePolicy` alone.* It only guards a pre-existing file; it cannot stop two concurrent live writers (the actual failure here).
- *OS open-mode share semantics instead of an explicit lock.* Not uniform: Windows denies a concurrent writer by default, Unix does not. An explicit advisory lock is the same code path on all three OSs. And `O_EXCL`/`CREATE_NEW` only acts at create time, conflicting with Append.
- *An in-process pre-check at Start that names the conflicting channel.* Prototyped, then removed: it faulted the whole channel (stopping reception, showing the bind/port recourse). A destination collision is a *recording* fault, not a channel fault — the lock enforces it at the recording-arming point and leaves the channel Running. The friendly "used by channel X" message is lost; the recording-fault diagnostic still says the destination is in use.
- *Name-derived filenames only (full structural uniqueness; the deferred #3).* Strongest prevent-by-design, but it removes the ability to pick an exact filename (destination becomes a folder) and needs profile migration — a larger UX decision, deferred to Appendix A.

**Consequences.**
- §6 changes: Channel Names are now **unique** (was "need not be unique"). §71 validation gains the uniqueness rule alongside filesystem-safe. §55/§121 gain the destination-lock requirement; a collision is a recording fault that leaves the channel Running.
- No new crate dependency: the advisory lock uses std's `File::try_lock` (stable since 1.89; MSRV is 1.95). `fs4` stays for the disk-space free functions only.
- The lock feeds the existing `RecordingFaulted` event/diagnostic (ADR-013) — no new GUI surface needed.
- Tests: a name-uniqueness validation test; a lock-conflict recorder test; an orchestrator test asserting the second channel stays Running while its recording faults on the destination lock.

## ADR-015 — Drop the `Highlight` match action; keep listener simple

**Status:** Accepted. **Context:** spec §50.2/§165 (Find & Triggers actions), ADR-010 (which introduced `Highlight` in the v2.0 stream model). Supersedes the `Highlight` portion of ADR-010.

**Problem.** `Highlight { style }` was one of five `MatchAction` variants — it was meant to style a matched **byte range** in the on-screen scrollback. It was never rendered: the GUI treated it as a no-op (`MatchAction::Highlight { .. } => {}`) and the styling was always "TODO." Making it real requires a **byte→display-position map across all three view modes** (Hex/Raw/Rendered), each with variable-width glyph rendering and soft-wrapping — a genuinely fiddly subsystem. Weighed against listener's "keep it simple" goal, the on-screen styling was disproportionate to the need: the concrete "getting ready to record" use case is served by **`Mark`**, which drops a `‹MARK …›` marker into the display recording (`.disp`) with no rendering machinery at all.

**Decision.** Remove the `Highlight` action and its `HighlightStyle` config type. Keep the rest of Find & Triggers unchanged: `BytePattern`/`Idle` conditions and the `Record`/`Mark`/`Notify`/`PauseDisplay` actions. `Mark` remains the correlation primitive (into `.disp`, never `.raw`, §49). The byte-offset-precise on-screen rendering that `Highlight` would have needed is not built.

**Why not keep it inert.** A no-op variant invites revival and keeps a `HighlightStyle` config type + a spec paragraph that describe behavior that doesn't exist. Removing it makes the code honest and the schema smaller; a profile can no longer carry a `kind = "Highlight"` action.

**Consequences.**
- `MatchAction::Highlight` and `HighlightStyle` removed from the config schema (§50.2 / §72 struct listing) and the runtime. `MatchAction` is `#[serde(tag = "kind")]`; a profile with a `Highlight` action now fails to parse (profiles are dev-only; consistent with the ADR-013 clean-break precedent — no migration, no `schema_version` bump for a variant removal).
- Spec §50.2/§165 and the overview drop `Highlight` from the action list; ADR-010's historical text points here.
- Manual/interactive highlighting (a later, larger idea) is **not** pursued; if on-screen styling is ever wanted, it returns as a fresh decision with the position-map cost understood up front.

## ADR-016 — `Mark` carries an inline arrival timestamp, rendered by string-splice (no position map)

**Status:** Accepted. **Context:** spec §50.2/§165 (Find & Triggers), §54/§57 (Display Recording & timestamps), §133 (timestamp model), ADR-013 (independent Raw/Display recording), ADR-015 (why `Highlight`'s on-screen styling was dropped). Builds on the "keep listener simple" line ADR-015 drew.

**Problem.** The concrete "getting ready to record" workflow wants a **timestamp next to a byte pattern** — e.g. the local arrival time immediately before each `$GPGGA` — visible both in the live display and in the Display Recording (`.disp`), while the `.raw` stream stays byte-exact (§53). ADR-015 had just rejected `Highlight` because styling a matched **byte range** needs a byte→**screen-coordinate** map across all three view modes. The open question was whether an inline timestamp inherits that same cost.

**Decision.** Give `Mark` an optional `MarkTimestamp { position: Before | After, format: TimestampConfig }`. On a firing, format the matched chunk's **arrival** `wall_clock` (§133) in **local** time (a listener-local `TimestampConfig` mirroring talker's, but `Local` not `Utc`) and **splice that string into the rendered text at the match's byte offset** — before or after the matched bytes. Render it uniformly in Raw/Rendered/Hex via `DisplayView::render_text_annotated`, which walks the bytes (already ordered) and inserts the annotation string when the walk reaches the target offset. The `.disp` recorder writes exactly what the display shows (the splice happens pre-record), so the live view and `.disp` are identical. `.raw` is never touched. A bare `Mark` (no timestamp) keeps the `‹MARK …›` marker-line behaviour.

**Why this is *not* the `Highlight` cost.** Splicing a string during rendering is a **text insertion**, not a coordinate mapping: no glyph-width measurement, no soft-wrap position math, no per-mode screen geometry. The renderers already produce their output by walking bytes/characters in order; "when you reach offset N, emit this string too" is a one-line addition per mode. The one place a byte offset must meet the character stream (multi-byte UTF-8/UTF-16 in Rendered/Raw) is handled by `decode_with_offsets`, which pairs each decoded character with its source byte offset — a small, tested helper, not a subsystem.

**Also removed (one timestamping mechanism, not three).** Two abandoned pieces are deleted so the per-match `Mark` is the *only* timestamp path: (a) the per-chunk Display-Recording timestamp (`DisplayRecordingConfig.timestamp_enabled` + its GUI checkbox), which was never surfaced meaningfully; and (b) the spec-only `TimestampDisplay`/`TimestampSource`/`TimestampResolution` types, which had no implementation. The byte-exact **Raw Recording timestamp sidecar** (`.raw.idx`, §57) is **kept** — it stays out of the `.raw` bytes and is byte-exact-safe — but remains config-only (`RawRecordingConfig.timestamp_enabled`) with a TODO to add a UI toggle. *(Superseded 2026-08-05: the Raw recording editor now carries that toggle, and ADR-039 corrects the offsets it writes. The decision above is kept as the original reasoning.)*

**Consequences.**
- `MatchAction::Mark` becomes `Mark { timestamp: Option<MarkTimestamp> }`; new `MarkTimestamp`/`MarkPosition`/`TimestampConfig` config types. `MatchAction` is `#[serde(tag = "kind")]`; a bare `Mark` still parses (the field is `#[serde(default)]`). No `schema_version` bump — additive field plus an additive-safe drop of the display timestamp field (dev-only profiles, ADR-013 precedent).
- The renderer gains `render_text_annotated`, and the streaming
  `StreamRenderer::render_chunk` takes annotations; `RenderedOutput.timestamp` is retained (it drives time-based Display rotation, §59) but never carries an inline mark — the inline text lives in `RenderedOutput.text`.
- The snapshot's `TriggeredMatch` carries an optional `MarkRender { text, before }` so the live viewer splices the same timestamp the `.disp` got, rebasing the match onto the scrollback window via its view-space `view_offset` (ADR-017 — the stream-space `byte_offset` counts bytes a paused view skipped).
- A minimal in-app editor creates `BytePattern → Mark(+timestamp)` rules; committing uses the existing Apply & Restart path (`config_needs_restart` counts `match_rules`) — no new runtime command. The general match-rule editor (Idle/Record/Notify/PauseDisplay) remains a separate TODO.

## ADR-017 — Two stream offset spaces: stream (all received bytes) vs. view (pause-gated), translated at fire time

**Status:** Accepted. **Context:** spec §50 (Display Pause: "the view stops accumulating new stream bytes"), §50.2 (match firings anchored on byte offsets), §87 (scrollback), ADR-011 (`StreamDelta` offsets), ADR-016 (live-view Mark splicing).

**Problem.** Two offset spaces coexist by construction, and a bug arose from conflating them:

- **Stream space** — `ActivityMeter::total_bytes()`: every byte received since Start. Match firings are anchored here (`FiredRule::match_offset`), diagnostics quote it, and it equals a byte's position in a `.raw` recording that ran from Start.
- **View space** — `stream_dropped + stream_buf.len()`: the scrollback's offsets, used by `StreamDelta`/`stream_end_offset` and the GUI's cursor. Per §50, a paused view stops accumulating, so view space **stops advancing during a pause** while stream space keeps counting; after any pause the two diverge permanently.

The live viewer was rebasing `TriggeredMatch.byte_offset` (stream space) onto its accumulated window (view space), so every post-pause Mark timestamp spliced N bytes late (N = bytes skipped while paused) or vanished. A rule combining `PauseDisplay` + `Mark` hit this immediately. (`.disp` was unaffected — its splice rebases per chunk in stream space consistently.)

**Decision.** Keep both spaces — each is authoritative for its consumer — and **translate at fire time**, where the mapping is exact: in `ChannelPipeline::ingest` the chunk's view-space base (`view_chunk_base`) is known right after the scrollback append (`None` while paused — those bytes have no view position). `apply_fired_rules` stamps each firing with both `byte_offset` (stream space, kept for diagnostics/`.raw` correlation) and a new `view_offset: Option<u64>` (view space); the GUI splices Marks at `view_offset` only. Idle firings carry neither.

**Why not the alternatives.**
- *Unify on one space by making pause advance `stream_dropped` past skipped bytes.* Breaks the ring invariant (`stream_buf[i]` is the byte at offset `dropped + i`) unless the buffer also drops content, which turns pause into a rolling reset and churns the GUI delta protocol.
- *Move pause out of the pipeline into the GUI (never gate the scrollback).* Makes the spaces identical by construction and is attractive long-term, but §50 defines pause as the view not accumulating, resume as "continues from live data, with no backfill" — relocating that behavior is a spec conversation, deferred to the multi-view-pause decision (TODO).
- *Anchor firings in view space only.* Loses the stream-space anchor that diagnostics and `.raw` correlation want, and a firing during pause would have no offset at all.

**Consequences.**
- `TriggeredMatch` gains `view_offset`; a Mark on bytes received while paused is visible in `.disp` (recording never pauses, §58) but not spliced into the live view — the bytes aren't on screen.
- The edge case of a boundary-split match whose carry bytes straddle a pause transition maps approximately (the prior chunk's bytes may not be in the view); `checked_sub` clamps the pre-window edge. Vanishingly rare and self-limiting — the splice is dropped, never misplaced across the window.
- Pinned by `match_view_offset_tracks_the_paused_view_not_the_raw_stream` (pipeline).

## ADR-018 — The `.disp` is the exact rendered stream; no hard wraps, ever (the Notepad model)

**Status:** Accepted. **Context:** spec §54 ("the recorder writes what the display shows, verbatim"), ADR-010 (chunk boundaries are reception details, never structure), §119 (lossy replacement is for *invalid* input), §59 (rotation), the carried TODO item on `.disp` garbling a multi-byte character split across reads.

**Problem.** The Display recorder rendered each received chunk independently and appended `\n` per chunk, so the `.disp`'s line structure tracked **OS read boundaries**: the same byte stream produced different files over serial vs UDP (different chunking — exactly the divergence ADR-009 removed for the live view), a sentence split across two reads gained an artificial line break, a UTF-8/UTF-16 character split across reads became `U+FFFD`, and Hex/tab column state restarted at every read. The `.disp` was the only output in the system that leaked chunk boundaries.

**Decision.** Per active Display recording, a **streaming renderer** (`display::StreamRenderer`) owned by the pipeline's `ViewRecorder` renders chunk by chunk into the *exact rendered stream* — the same text as rendering the whole stream at once, pinned by a chunking-invariance test (byte-at-a-time == one-shot). State per recording: the undecoded multi-byte **carry** (an incomplete tail held for the next chunk; annotations targeting carried bytes defer with it), the Rendered-mode **tab column**, and a Hex **separator-continuation** flag. The carry is flushed lossily at finalize (a truncated stream *is* invalid data at that point). State starts fresh at each recording begin and spans rotation boundaries (each period file stays contiguous; no artifact is reintroduced at rotation).

**No hard wraps are ever written** — the Notepad model, chosen explicitly: the file stores unwrapped content; soft wrap is a viewer toggle. Line breaks come only from the data (Rendered/Raw-Native CR/LF) and from `‹MARK …›` marker lines, which now frame themselves with explicit newlines. Hex is a continuous separator-joined cell stream (its 16-per-line look is likewise a view-time choice). Wrapping at record time was rejected because the display edge is a view-time, mutable property: the width at write time would bake the window-resize history into the file, the recorder (runtime) cannot see the GUI (ADR-008 layering), headless recording has no display edge at all, and any baked width destroys determinism and diffability. The recorder ignores the view config's `wrapping` accordingly.

**Why not the alternatives.**
- *Keep line-per-chunk and document it.* Honest but wrong: adequate only by coincidence at low rates, and it fails precisely under load, when the `.disp` matters most.
- *Delete just the recorder's `\n`.* Breaks Hex (chunks glue as `41 4243`), tab columns, and marker lines — the newline was load-bearing for all three; the fix is state, not deletion.
- *Hard-wrap at a fixed configured width.* Deterministic, but still a lossy view decision baked into content; deferred unless a real consumer needs it.

**Consequences.**
- `DisplayFileRecorder` appends verbatim (no injected newline); `RotatingDisplayRecorder` still rotates per rendered item on arrival time — a rotation boundary can now fall mid-line, which is the correct trade (files stay individually contiguous; the stream is exact across them).
- The pipeline flushes the renderer tail (`finish()`) into the recording at every finalize path (stop, stop-all, channel stop).
- The live viewer and the `.disp` now agree byte-for-byte for Rendered/Raw content (the viewer re-renders the accumulated buffer, which was always boundary-free).
- Pinned by: `stream_renderer_is_chunking_invariant`, `stream_renderer_rejoins_a_split_utf8_character`, `stream_renderer_hex_separates_across_chunks_and_tabs_keep_columns`, `stream_renderer_defers_an_annotation_for_a_carried_byte`, and the pipeline-level `disp_is_the_exact_rendered_stream_across_read_boundaries`.

## ADR-019 — GUI chrome lives in the shared `wiredata-ui` crate

**Status:** Accepted. Rewritten 2026-11-14 to state the current decision.

**Context:** The GUI merge made talker adopt listener's look and feel, and both apps
offer the same light and dark themes. Counterpart of talker ADR-016, which holds the
full rationale and the scope rule.

**Decision.** Listener's GUI **chrome** is the internal workspace crate
**`wiredata-ui`** (egui-only, `publish = false`), shared with talker: the bundled font
stack, the named palette with `LIGHT` and `DARK` instances (four role-named accents,
talker ADR-049), the base widget visuals and style tweaks, and
`human_bytes`. Listener imports `wiredata_ui` paths directly.

- `wiredata_ui::palette::active(ui)` returns the active theme's palette. Pure helpers
  such as `status_color` and `recording_indicator` take `&Palette`, so they are
  unit-testable without a `Ui`.
- The header theme toggle is `wiredata_ui::style::theme_toggle_button`. The choice is
  persisted under the same `dark_mode` storage key talker uses.
- `wiredata_ui::selection` owns the selected and compact channel tabs, shared row
  emphasis, and the complete tab-to-page edge. The apps supply the selected card
  rectangle and its real scroll viewport. The shared painter mirrors egui's resizer
  hover and drag stroke and detours around a tab only when the whole card and both
  rounded turns are visible; a clipped selection gets a straight page divider.
  Historical warning and error counts recede identically on background tabs, while
  app-owned lifecycle glyphs and live faults stay saturated.
- Stream-view content colours stay user-chosen per view (`ColorScheme` offers light and
  dark schemes), deliberately not coupled to the chrome theme.

**Consequences.**
- The font assets, their README and licenses, and the font rationale doc live in
  `wiredata-ui/assets/fonts/`.
- Anything app-specific stays put: view-models, widgets with runtime knowledge, and the
  stream-view `ColorScheme`. Selection is chrome, not channel state.
- A change to the shared look lands in both apps by construction, which removes the
  drift risk that motivated the crate.

## ADR-020 — Terminal transport fault causes use lossless post-reception delivery

**Status:** Accepted 2026-07-12. **Context:** spec §94/§95/§101, ADR-006
(polled lifecycle truth), and ADR-007 (advisory live-stall notices).

**Problem.** A spontaneous transport fault has two observable parts: the lifecycle
state (`Faulted`) and the cause retained in channel diagnostics. The state already
self-corrects through polled snapshots, but the cause was sent with `try_send`
through the same 16-entry queue as advisory `ReceptionStalled` warnings. If that
queue was full at failure time, the only explanatory string was discarded even
though the receive hot path had already ended and no longer needed protection from
blocking.

**Decision.** Keep one bounded transport-condition queue and give its variants two
delivery classes. `ReceptionStalled` remains advisory and uses `try_send` from the
live Serial receive loop. `TransportFaulted` is sent by the runtime monitor only
after joining a failed transport and awaits queue capacity; TCP connection
supervision uses the same helper. `run_channel` remains alive until both ingest and
notice senders close, so it drains the terminal cause before retaining diagnostics.
(TCP receive code removed 2026-11-14, ADR-047.)

**Consequences.** Reception throughput and cancellation behavior are unchanged:
only a post-reception monitor may wait. A full advisory queue can delay fault
finalization but cannot erase its reason; a pipeline that has already been forcibly
removed simply closes the receiver and releases the send. Pinned by
`terminal_fault_waits_for_space_in_a_full_notice_queue` plus the end-to-end
`spontaneous_transport_fault_emits_channel_faulted` diagnostic assertion.

## ADR-021 — Channel transport is chosen at creation

**Status:** Accepted 2026-07-15. **Context:** the existing Add-template GUI model and
its adoption by talker in talker ADR-027.

**Decision:** Keep transport kind structural in the GUI. `+ Add` chooses the UDP or
Serial template (TCP was removed by ADR-047). Configure interface edits the selected
template's parameters and never replaces its `InterfaceConfig` variant.

**Consequences:** This records existing listener behavior rather than adding a new
runtime path. Both apps now present one ordinary transport choice and the same relative
Add-menu order. Reconfiguration, profiles, and the schema are unchanged. A focused GUI test
pins the menu labels and order.

## ADR-022 — Recorder stops retire detached from the acquisition loop

**Status:** Accepted 2026-07-16. **Context:** spec §142 requires recorder
implementations to never block the producer, and §96 keeps reception running when
recording stops. Yet a live Stop (`set_recording(false)`, a rule's `Record { Stop }`,
or the disk guard's `StopRecording` action) awaited `Recording::finalize` — a full
accepted-backlog drain plus file close — **on the pipeline task**, inside the same
select loop that drains ingest. On a slow disk this backed the bounded ingest queue
up into the transport. The disk guard also ran synchronous filesystem space queries
on the pipeline task, where a hung network share could stall reception indefinitely.

**Decision:** Recorder stops retire **detached**. The pipeline moves the taken
recorder into a `JoinSet` task that drains and finalizes off-loop and yields the
arguments for `note_recording_stop`; the run loop reaps completed retirements
non-blockingly each pass (the idle tick bounds the note's latency on a quiet
stream). Ordering rules preserve the previous guarantees: `begin_*` drains in-flight
retirements first (a new file must never open while its predecessor is closing —
same-destination restarts), and `finish` drains before the final snapshot so a
channel stop still reports every outcome (§56.1 honesty — a stop that lost data
never reads clean; a panicked finalize task is reported as a fault, not silence).
Disk-guard space queries move to `spawn_blocking` under a 2 s timeout.

**Consequences:** The recording state reads "off" the moment a stop is requested,
while the flush completes in the background and its outcome note (clean or faulted)
lands within one idle tick. Asynchronous **begin** is unchanged (still awaited
inline): detaching it would need a defined `Starting` recording state or prebuffer —
deferred until something needs it. Pinned by
`recorder_stop_retires_detached_and_still_reports_the_outcome`.

## ADR-023 — One Display View per Channel

**Status:** Accepted 2026-07-16 (spec v2.1). Rewritten 2026-11-14 to state the current
decision; ADR-060 holds the profile change.

**Context:** Spec §48 promised multiple simultaneous Display Views per Channel, but the
runtime and GUI have only ever used the first view: scrollback pause gating keyed on
`display_views.first()`, and the detail pane surfaced one view. A non-primary view could
be *marked* paused but had no per-view stream state to freeze — the abstraction cost
complexity (a `Vec` of views threaded through the pipeline, snapshots, and config)
without delivering the feature.

**Decision:** One **logical** Display View per Channel is normative, with Raw / Rendered
/ Hex as that view's switchable modes — matching talker's single Output pane, so the two
apps present the same display model. The view is saved as `display.view` (ADR-060, which
also holds the loader for older profiles that carried a `views` list). Multiple
simultaneous views are deferred to Appendix A: reviving them requires per-view
render/pause state, not just config plumbing.

**Consequences:** "The view is paused" and "the Channel's display is paused" are the
same statement, closing the pause ambiguity.

## ADR-024 — TCP Connection-Channel surfacing is deferred scope

**Merged into ADR-047 on 2026-11-14.** Surfacing TCP connections was deferred scope, and the TCP code that made the question live is removed; ADR-047 holds both. Text in [REVISIONS.md](REVISIONS.md#retired-decisions).

## ADR-025 — NMEA construction returns for presentation-only ZDA Marks

**Status:** Accepted 2026-07-17 (spec v2.2). **Corrected 2026-11-14:** "zone
fields carry the local offset" below was implemented as the ISO 8601 offset,
local minus UTC, so UTC−5 was written `-05`. NMEA 0183 defines the local zone
the other way round, as the hours added to local time to get UTC, so UTC−5 is
`+05` and a zone east of Greenwich is negative, with the minutes taking the
hours' sign. The zone fields now follow NMEA (spec §50.2). The wording never
named whose definition applied, and the test pinned the code's own reading,
so nothing caught it; nothing was checked against an NMEA reader either.

**Context:** ADR-010 removed protocol decoding and Listener's `nmea0183` dependency,
correctly preserving a byte-stream-only receive path. Timestamped Marks now need an
optional interoperable ZDA form, while user-configured separators may contain CR/LF.
The existing renderer treated annotation text as column-neutral, and `After` anchored
at the first match byte, so a multiline or multi-byte-match annotation could corrupt
layout or split the match.

**Decision:** `MarkTimestamp` gains an additive style: `Plain` keeps compact local
time; `NmeaZda { talker }` constructs
`$<talker>ZDA,<UTC time>,<dd>,<mm>,<yyyy>,<local-zone-hours>,<minutes>*XX` through
`nmea0183`, strips only its trailing CRLF, then appends the configured separator
verbatim. Milliseconds are optional. Two-character talkers are standard; custom IDs
of 1–32 printable ASCII characters are allowed, while whitespace/control characters
and `$ ! , *` are rejected before Start. IDs longer than two are explicitly custom
ZDA-shaped output.

Listener depends on `nmea0183` for construction only. It never parses, validates,
frames, or interprets received bytes as NMEA. ZDA UTC fields use chunk arrival wall
time; zone fields carry the local offset when representable by the conventional ZDA
range, otherwise remain empty. No CR/LF is injected by the style itself; separators
remain the user's line-layout control.

All display renderers now consume explicit CR/LF in annotation text and reset their
wrap, tab-column, or Hex-cell continuation state. A `Before` annotation anchors on the
match's first byte and `After` on its final byte; diagnostic offsets continue to name
the first byte. `.raw` is never changed.

**Consequences:** Existing profiles default to `Plain` with no schema-version bump.
ZDA output is checksum-bearing and identical in the live view and `.disp`, including
newline separators. Construction happens only when a configured Mark fires, not for
ordinary chunks. Tests pin long custom IDs, unsafe-ID validation, UTC/date and zone
formatting, checksum validity, multiline renderer state, complete-match anchoring,
profile round trips, and byte-exact Raw recording.

## ADR-026 — Receive timing starts at the post-read boundary and stays bounded

**Status:** Accepted 2026-07-19.

**Context:** Listener exposed queue depth and throughput but not the time a received
chunk spent between transport delivery and pipeline processing. Its UDP, TCP, and
Serial transports also constructed the payload vector before calling
`ChunkTime::now`, so a large copy was omitted from any downstream userland-delay
measurement. Calling that timestamp true wire arrival would overstate what a
portable userland receiver can know. (TCP receive code removed 2026-11-14, ADR-047.)

**Decision:** Every data transport captures `ChunkTime` immediately after its OS read
returns and before copying the payload into its owned vector. `ChannelPipeline::ingest`
captures a monotonic start time before other work and records the elapsed interval in
a fixed-size cumulative histogram. That histogram retains bucket counts, sample count,
sum, and maximum, never individual samples. It travels through the existing O(1)
`ChannelStats` and bounded `ChannelSnapshot` surfaces and is retained with the final
run state after Stop. The selected-channel GUI shows a warm-up state followed by a
p99 bucket upper bound and maximum.

**Measurement boundary:** the interval includes payload copying after the read,
bounded Transport-to-Pipeline queue wait, runtime scheduling, and handoff overhead. It
excludes device, adapter, driver, and kernel buffering before the read completed. It
is chunk-granular and cannot provide per-byte arrival times. The wall-clock member of
the same `ChunkTime` remains the source for recording and Mark timestamps; elapsed
telemetry uses only `Instant` and is therefore immune to wall-clock adjustments.
ADR-029 later permits optional Linux UDP software metadata to replace only that
wall-clock member while preserving this post-read monotonic anchor.

**Consequences:** The new hot-path work is one monotonic clock read, one fixed bucket
search, and saturating arithmetic per chunk; stats traffic and memory do not scale
with run length or receive rate. The number diagnoses userland handoff pressure when
read alongside ingest-queue depth, but is not end-to-end latency. Listener remains
event-driven and does not request a finer OS timer merely to receive data. ADR-029
resolves optional platform timestamps and ADR-030 resolves strict timer-based rules
without changing this receive boundary. No profile, recording format, or receive
bytes change.

## ADR-027 — Recent receive timing uses a fixed segmented window

**Status:** Accepted 2026-07-19. The local-implementation paragraph was superseded by
ADR-032, which moved the helper into `wiredata-telemetry`.

**Context:** ADR-026's cumulative post-read histogram preserves completed-run truth,
but a long healthy history can dilute a new queueing or task-scheduling slowdown. Raw
sample retention or per-chunk GUI events would make telemetry memory or observer cost
scale with receive rate, undermining the bounded receive architecture.

**Decision:** The pipeline keeps a companion recent histogram as ten fixed one-second
segments. Every chunk records into the current segment alongside the cumulative
histogram. Stats and snapshot creation merge only segments younger than ten seconds;
large gaps clear and reuse the same fixed storage. The GUI uses the explicitly labeled
recent window for warm-up and p99, while retaining the cumulative maximum as the
run-wide outlier. Stop preserves both final summaries with the channel's other retained
state.



**Consequences:** Recent degradation becomes visible without retaining samples,
increasing event traffic, or changing the O(1) stats/snapshot shape. The window is an
approximation at one-second segment granularity, not an exact sliding sample cutoff.
No receive behavior, profile, recording format, or spec version changes.

## ADR-028 — Total ingest processing uses one pipeline boundary

**Status:** Accepted 2026-07-19.

**Context:** ADR-026 measures the userland handoff before pipeline work begins. A high
handoff delay can therefore indicate queueing or task scheduling, but it cannot show
whether `ChannelPipeline::ingest` itself is consuming the available receive budget.
Timing each match, render, and recording operation separately would add several clock
reads to every chunk before evidence shows that stage-level attribution is needed.

**Decision:** `ChannelPipeline::ingest` records one fixed-size cumulative duration
histogram. Its start reuses the monotonic instant already captured for handoff timing;
one additional monotonic clock read follows the final recorder-fault check. The
measurement includes byte accounting, activity and scrollback updates, match-rule
evaluation and immediate actions, display rendering and enqueueing, and recorder-fault
checks. It excludes transport reads, payload copying and queue wait before pipeline
entry, asynchronous recorder writes, and pending Record actions applied later by the
channel run loop.

The histogram uses the same bounded buckets as handoff timing and travels through the
existing O(1) stats and snapshot lanes. Stop retains the completed run's summary;
Start and reconnect reset it. The selected-channel GUI places run p99 and maximum next
to handoff timing and ingest-queue peak so pipeline cost can be distinguished from
handoff pressure. The completed follow-up adds the same ten-segment recent window and
cumulative chunk-size/inter-read-gap histograms. Chunk shape reuses the existing
post-read monotonic capture, adds no clock read, and describes transport reads rather
than protocol records.

**Consequences:** The ingest hot path gains one clock read and fixed histogram updates
per chunk, with no allocations, per-chunk events, or run-length-dependent memory. The
aggregate identifies whether total pipeline work is material but does not attribute
cost to an internal stage; stage-level clocks should be added only when this metric
demonstrates a need. The recent processing window and chunk-shape context make a
short slowdown interpretable without raw sample retention. No receive behavior,
profile, or recording format changes.

## ADR-029 — Transport health keeps unsupported, zero, and fallback distinct

**Status:** Accepted 2026-07-19 (spec v2.2.1).

**Context:** Queue depth and post-read handoff timing show userland pressure but do
not say whether a Serial reader was blocked long enough to risk adapter overrun or
whether the kernel discarded UDP datagrams. A missing platform counter must not look
like a measured zero. Optional kernel arrival timestamps also need to expose their
actual source; silently mixing post-read and kernel boundaries under one label would
make Mark and recording times hard to interpret.

**Decision:** `TransportHealth` uses transport-specific optional/availability types.
The Serial reader accumulates every Transport-to-Pipeline backpressure episode's
duration and publishes count, total, maximum, and active state; its existing 250 ms
warning remains an advisory possible-loss threshold, not the accounting threshold.
ADR-034 later moved these Serial figures to the reader's authoritative state and
made the cumulative values completed-only, with active time reported separately.
Linux UDP enables `SO_RXQ_OVFL`, unwraps the kernel's 32-bit cumulative counter into a
run-local `u64`, and reports supported zero separately from unsupported. Other
platforms remain explicitly unsupported because no equivalent attributable
per-socket counter is available through the current transport boundary.

`UdpConfig` gains additive, default-off `kernel_timestamps`. Linux requests
`SO_TIMESTAMPNS` and parses `SCM_TIMESTAMPNS` from the same `recvmsg` ancillary data.
When present, it replaces only `ChunkTime.wall_clock` and records
`KernelSoftware`; monotonic ordering, handoff, gaps, and processing retain the
post-read capture. Failed setup, unsupported platforms, or missing per-datagram
metadata use and count the post-read fallback. The startup status and observed source
counts both travel through O(1) stats/snapshots without a per-datagram event.

**Consequences:** Users can distinguish inapplicable, unsupported, supported-zero,
and observed-loss states and can tell whether each run actually received kernel
timestamps. Software timestamps reduce one userland scheduling component but are not
hardware or per-byte timing. Serial stall duration is evidence of risk, not a lost-
byte estimate. The only profile addition defaults off, so schema version 3 remains
unchanged.

## ADR-030 — Idle rules wait on one ordinary deadline

**Status:** Accepted 2026-07-19 (spec v2.2.1). Rewritten 2026-11-14 to the decision as
amended on 2026-11-13. The first-made text, with its Windows precision window, is in
[REVISIONS.md](REVISIONS.md#retired-decisions).

**Context:** Idle rules were evaluated by a shared 250 ms maintenance tick, adding up to
one tick of avoidable lateness before executor or OS wake jitter. Receiving data is
event-driven and gains nothing from a finer timer period, but an explicitly configured
timer condition should target its own monotonic deadline. A Windows 1 ms request held
near each deadline was first built, with two lateness histograms, a timer summary and a
GUI readout. The user judged about 16 ms of possible lateness on Windows acceptable for a
rule that fires on a quiet stream, against all of that kept for it alone.

**Decision:** The pipeline computes the earliest armed Idle-rule deadline for each select
pass and waits directly on it, with one ordinary deadline wait on every platform. New
data, commands, or cancellation discard the future and recompute it; only a completed
deadline wait evaluates rules. Recorder maintenance remains on its independent 250 ms
tick. Listener makes no timer-resolution request and keeps no lateness histograms; it
calls `wiredata-timing` only for the minimized-window throttling opt-out it shares with
Talker.

**Consequences:** Idle evaluation no longer carries intentional 250 ms polling delay,
and ordinary receive paths make no timer-resolution request. On Windows an Idle rule can
fire about 16 ms late, and that is accepted. No timer policy improves arrival timestamps.
macOS App Nap remains a separate activity-policy question.

## ADR-031 — Completed Listener runs retain one bounded truth snapshot

**Status:** Accepted 2026-07-19 (spec v2.2.1).

**Context:** Stop retained live counters for inspection, but the next Start reset
them and recorder teardown could erase a queue peak. That made before/after support
comparisons depend on screenshots. A task panic or shutdown grace timeout can also
prevent a final pipeline snapshot, and presenting a default-filled report as exact
would hide that failure.

**Decision:** A run begins only after transport open succeeds and receives a process-
unique increasing `RunId`, paired wall-clock start, and monotonic start. Stop or a
terminal transport fault captures finish time and monotonic elapsed duration, then
builds a self-contained `ListenerRunSummary` before the final snapshot is consumed.
It owns exact bytes/chunks, diagnostics counts, match-boundary saves, cumulative and
recent timing, transport health, ingest/Raw-recorder queue peaks,
end reason, and a `final_snapshot_complete` flag. Raw-recorder queue history is
retained before task retirement so teardown cannot erase its peak.

The orchestrator retains one summary per stable configured Channel and only replaces
it with a newer `RunId`; failed opens never replace it. The selected detail pane keeps
the summary collapsed by default. `Copy summary` creates a stable versioned line-
oriented report with build/platform facts only when clicked, so ordinary repaint does
not allocate the export. Profiles and disk recording contain no summary history.

**Consequences:** A completed run remains comparable after Stop/Start with constant
memory. Wall-clock start/finish may reflect clock changes, while elapsed duration is
monotonic. An incomplete final snapshot is visible as incomplete rather than silently
zero. Retention is process-local and clears with fresh Channel slots/profile load.

## ADR-032 — Bounded duration telemetry primitives live in `wiredata-telemetry`

**Status:** Accepted 2026-07-20.

**Context:** ADR-027 kept Listener's ten-segment recent-duration helper local when a
new crate would have served only one small consumer. Talker now has the same durable
consumer and its duration histogram, fixed buckets, timed segments, and aging logic
are byte-identical. Keeping numeric code duplicated risks silent divergence when a
bucket edge or rollover rule is corrected in only one application.

**Decision:** Adopt workspace ADR-039 and add the internal, non-published,
dependency-free `wiredata-telemetry` crate. It owns the cumulative duration histogram
and bounded ten-by-one-second recent-window engine. Listener continues to expose the
cumulative type through `runtime::telemetry` and keeps all application aggregates and
measurement policy local: `ByteHistogram`, `ChunkShape`, transport summaries,
pipeline boundaries, stats/snapshots, completed-run retention, and GUI presentation.
The separate `wiredata-timing` crate continues to own only process timer-resolution
mechanics.

**Consequences:** Listener and Talker share one tested implementation of the numeric
primitive without sharing runtime architecture or telemetry schemas. Hot-path memory,
allocation, measurement boundaries, retained summaries, profiles, and received bytes
are unchanged. This decision supersedes ADR-027's local-implementation paragraph;
its window semantics and all other consequences remain in force.

## ADR-033 — Diagnostics lead with decisions without overstating receive health

**Status:** Accepted 2026-07-20. **Amended 2026-11-14:** absorbs ADR-057; the card has
no badge.

**Context:** Listener exposes transport-specific counters, queue pressure, handoff
and pipeline timing, chunk shape, timer policy, recorder state, and retained-run
facts. Keeping that evidence visible is essential, but presenting it as one flat set
of readouts makes operators synthesize the likely fault domain before deciding where
to look. A generic health score would erase important availability semantics: an
unsupported UDP drop counter is not a measured zero, a supported zero does not prove
that no bytes were lost elsewhere, and Serial backpressure duration establishes risk
rather than a lost-byte count.

**Decision:** Add a compact, decision-oriented diagnostics summary with three
Listener-owned rows: **Transport**, **Pressure**, and **Pipeline**. Transport
preserves each transport's supported, unsupported, fallback, observed, and
inapplicable states and uses bounded claims such as no kernel drops *observed* when
that is the actual evidence; it never claims no loss or maps unsupported to zero.
Pressure summarizes existing ingest/recorder queue and backpressure evidence without
inventing missing counts. Pipeline summarizes existing handoff and processing timing
while retaining warm-up, incomplete-final-snapshot, and measurement-boundary caveats.
The card has no title and no status badge: it starts with the Transport row and is
drawn with `wiredata_ui::diagnostics::diagnostic_card`, and the shared crate has no
badge at all. A badge once summed the card up as live monitoring, retained last-run
evidence, or no run telemetry yet. It rose only from the Transport and Pressure rows,
which already mark themselves with ⚠ when they want attention, and kernel drops and
Serial stalls have their own callouts. Its calm words were said elsewhere:
monitoring by the Channel status at the top of the pane, last run by the Pipeline
row's "~final 10 s", and awaiting data by the rows' own "awaiting". Absent evidence is
never turned into a healthy state. Talker removed its badge for the same reason
(talker ADR-041).

An **Attention** callout appears only when existing state derives an operator-relevant
exception. It is GUI presentation, not a runtime `Diagnostic`: it creates no event,
does not consume diagnostic retention, and does not change diagnostic or completed-run
counts. There is no persistent green callout and no opaque composite health score.
The absence of Attention means only that no configured exception was derived from the
available evidence. Complete transport, pressure, timing, chunk, timer, recorder, and
run telemetry and their caveats remain available under collapsed details.

`wiredata-ui` may own only the identical egui card, row, and callout
chrome shared with Talker. Listener owns the row labels, evidence selection,
wording, severity mapping, thresholds, and the distinctions among unsupported,
zero, fallback, risk, and observed loss. This is a view-model and layout change only:
receive/runtime behavior, telemetry collection and types, retained snapshots and
summaries, clipboard reports, recording, diagnostics, and profiles are unchanged.
Talker makes the paired send-specific decision in ADR-041.

**Consequences:** The default view points first to the likely receive-side fault
domain without discarding the evidence needed to verify that interpretation. Derived
attention cannot pollute the event history or be mistaken for an observed runtime
incident, and unavailable counters cannot masquerade as clean measurements. Pure
Listener-side classification tests can pin these semantics while shared chrome stays
free of transport and pipeline policy.

## ADR-034 — Serial stall snapshots read authoritative reader state

**Status:** Accepted 2026-07-27.

**Context:** ADR-007 correctly keeps `ReceptionStalled` notices bounded and
non-blocking, because blocking a Serial reader to report backpressure would worsen the
condition being reported. ADR-029 subsequently added cumulative stall telemetry, but
the reader kept its exact episode count, total, and maximum only in loop-local
variables and published completion summaries through the same best-effort notice
queue. The pipeline also inferred an active episode from the threshold warning. A
dropped warning could hide an active stall, while a dropped completion summary could
leave the display accruing an inferred active duration after the reader had resumed.
Advisory delivery had therefore become the accidental authority for snapshot state.

**Decision:** Each successfully opened Serial run owns one shared, transport-local
stall-state cell. The blocking reader opens an episode immediately when a received
block first finds the Transport-to-Pipeline queue full and completes it exactly once
when the retry succeeds, cancellation ends the run, or the pipeline receiver closes.
The cell keeps completed episode count, completed total, completed maximum, and the
active episode's monotonic start separately. It is locked only at those episode edges
and for a small scalar snapshot; retry sleeps and transport I/O never hold the lock.
Poison recovery retains the measured values rather than making authoritative
telemetry disappear.

`ChannelPipeline` holds a clone and derives `SerialStallSummary` directly when it
builds live, stats, or final snapshots. Public cumulative values cover completed
episodes only; `active_for` reports the current episode separately. Completion moves
that elapsed duration into the cumulative fields once. This refines ADR-029's Serial
aggregate semantics to completed-only cumulative values plus separate active elapsed
time. The best-effort
`SerialStallSummary` transport notice and the pipeline's warning-based reconstruction
are removed.

`ReceptionStalled` remains an advisory, non-blocking notice. Once an ongoing episode
reaches ADR-007's established warning threshold, the reader makes at most one
best-effort delivery attempt before its next retry, including a retry that succeeds.
If the transport notice is dropped, its retained warning diagnostic is omitted; if
the notice arrives but the event queue is full, the matching runtime event is
omitted. Neither omission changes authoritative snapshot truth. Complete final
snapshots contain finalized inactive totals after every normal retry-loop exit. The
existing incomplete-summary marker remains the honest boundary when a pipeline-task
panic or shutdown grace limit prevents its final snapshot. This decision does not
change the pre-existing classification of transport task/thread panics.

**Consequences:** Serial backpressure totals and active state now self-correct through
the polled snapshot surface and do not depend on observer capacity. Short episodes
below the warning threshold are still counted without creating warning diagnostics.
The mutex is off the per-chunk hot path and preserves a consistent multi-field
snapshot. No received bytes, queue policy, warning threshold, profile, recording
format, or completed-run clipboard keys change.

## ADR-035 — Telemetry freshness follows the observation model, not the app

**Status:** Accepted 2026-07-27.

**Context:** Talker and Listener now expose comparable receive/send timing telemetry
built on the same bounded primitives (ADR-032; talker ADR-039), and a technician may
well read both panels in one session. The two apps nevertheless observe that
telemetry through opposite mechanisms, which is a direct consequence of their
opposite runtime models (ADR-001 versus talker ADR-002):

- Listener **pulls**. `PipelineRequest::Snapshot` is answered inside the channel's
  own task, and every recent-window histogram is collapsed with
  `snapshot_at(Instant::now())` at the moment the request is served. A served
  "last 10 s" therefore always describes the 10 s ending at the request.
- Talker **pushes**. The runner emits a collapsed snapshot from its send path, and
  the supervisor retains the most recent one. Between emissions — including
  indefinitely, on a dormant schedule — the retained copy ages while its sample
  count does not, which is exactly the hazard talker ADR-042 addresses with an
  explicit capture instant and freshness classification.

Because the surfaces look alike, each app's mechanism invites being ported to the
other. Both ports would be wrong, and neither would be obviously wrong to a reader
looking at one crate.

**Decision:** Freshness handling is a property of the observation model, not of the
telemetry, and is decided per app:

- Listener does **not** carry a capture instant on `ChannelSnapshot`, `ChannelStats`,
  or any histogram it serves. Under pull, a stale window is not representable, so a
  capture instant would be a field that is always "now" — dead weight that implies a
  hazard this app does not have.
- Talker does not adopt Listener's compute-on-demand shape for its counter lane.
  Doing so would require waking a dormant runner to answer a poll, discarding the
  zero-wakeup dormant contract that talker ADR-042 explicitly preserves.

The two panels are kept consistent in **vocabulary**, not in mechanism: both use the
same warm-up wording, the same bounded-percentile notation, and the same
`RECENT_WINDOW`. Listener simply has no *expired* state to name.

If Listener ever gains a pushed or cached telemetry lane — a cross-channel overview
served from retained copies rather than live tasks, say — that lane acquires the
freshness problem and should adopt talker ADR-042's treatment for that lane only.
The rule is that whichever side retains a collapsed snapshot across time owns
proving its age.

**Consequences:** A reader comparing the two crates finds the asymmetry recorded
rather than looking like an omission on one side. No wire, profile, recording, or
snapshot schema changes. See talker ADR-043 for the same decision from talker's side.

## ADR-036 — *Connection* names an accepted peer session, not a configured endpoint

**Status:** Accepted 2026-07-28.

**Context:** Listener's editor section was titled *Configure connection*, matching
Talker's. In Listener that title is actively wrong: **Connection** is already a
first-class runtime concept here — an accepted TCP peer session with its own
lifecycle, its own Channel, and a `max_connections` limit (§16.2). The spec
paragraph introducing the editor sat three lines below "TCP Connection Channel",
so one page used the same word for a live peer session and for the parameters of
a serial port.

The word was also doing a third job. The UDP bind-address help described
`0.0.0.0` as "any interface", meaning the host NIC — so *interface* named both
the thing being configured and the adapter it binds to.

**Decision:** *Connection* is reserved for an accepted TCP peer session. The
editor is **Configure interface**, matching Talker (talker ADR-044) and the
`InterfaceConfig` type both applications already use. Host-adapter references say
**network interface** or **NIC** explicitly, so the bind help cannot be read as
describing the channel's configured endpoint. No TCP session exists now (ADR-047), so Listener has no *connection* at all; the
vocabulary rule stands for if one returns.

Byte rates use the shared `human_byte_rate` helper rather than a hardcoded
`kB/s`, so a fast channel reads `1.2 MB/s` and a stopped one reads `0.0 B/s`,
matching how byte totals already scale.

**Consequences:** The two applications name the same thing the same way while
Listener keeps the distinct concept it genuinely needs. No transport behavior,
profile schema, recording format, or snapshot surface changes. The shared
diagnostics-row chrome now attaches its tooltip to a row's label as well as its
value (talker ADR-044), which reaches Listener's card automatically.

## ADR-037 — Retire the warm-up gate; state the worst value with its sample count

**Status:** Accepted 2026-08-05. Adopts talker ADR-046 on Listener's side.

**Context:** Every duration readout in the Receive diagnostics card was gated on
`TIMING_WARMUP_SAMPLES = 20`: below twenty samples it showed a maximum and
labelled itself *warm-up*, above twenty it showed `p99 ≤ X`. The gate looked like
a guard against an unreliable percentile, and it was not one. The percentile rank
is `ceil(samples × 99 / 100)`, which equals `samples` for every count up to 99,
so below a hundred samples the p99 bucket *is* the bucket holding the maximum.
The gate relabelled a single number as two states, and it drew the line at 20
while the two statistics only begin to separate at 100.

It also cost the reader the one fact that was always exact. A maximum is true at
one sample; the sample count is what says whether to trust it. The gate showed
the count only while it was small and hid it exactly when the reader began
relying on the figure.

Talker reached this conclusion first (ADR-046). The shared vocabulary module
recorded Listener's remaining gate as a deliberate divergence, which made the
migration visible but left two applications describing one state two ways.

**Decision:** No sample-count gate anywhere in Listener's diagnostics. State the
largest value, always, with the population it came from — `worst 4 ms of 812
chunks` — and add `99% ≤ X` only where `p99 < max` proves it is a different
figure. That comparison is exactly the test for the p99 bucket sitting strictly
below the maximum's, so the percentile appears when, and only when, it says
something the maximum does not.

Three consequences follow from applying one rule everywhere:

- **The noun is the measurement's, not the transport's.** Handoff and Processing
  count chunks.
- **The superlative is not always "worst".** Read gaps report the *longest*: a
  quiet source is not a fault, and `worst` would imply one.
- **Chunk sizes obey it too.** A median or percentile appears only when reads
  actually varied, so a datagram socket delivering fixed-size payloads states
  just the largest — which is the whole truth about its reads.

**Consequences:** `TIMING_WARMUP_SAMPLES` is gone, along with the three tooltips
that taught the retired state, and a test pins that no readout teaches it again.
The compact Pipeline row states one chunk count and window for the line, so a
boundary appends its own only when the populations differ — Handoff and
Processing are recorded once each per ingest, but their recent windows are
stamped at the start and end of that work, so a segment boundary can leave them
one sample apart.

`timing_figures` is deliberately duplicated in both applications rather than
shared: it reads a `wiredata-telemetry` histogram, and `wiredata-ui` depends on
`egui` alone (talker ADR-016 / listener ADR-019). The shared module documents the
vocabulary and names both call sites; it does not own the rule.

The detail rows no longer restate *post-read to pipeline* mid-line — the label
names the boundary and the tooltip defines it, and the row now carries counts
that earn the space. No measurement, telemetry type, snapshot surface, or profile
schema changes; this is presentation only.

## ADR-038 — Hex grouping is rendering; line length is layout; a Mark takes its own row

**Status:** Accepted 2026-08-05. **Amended 2026-11-14:** states the whole Hex layout
decision. It absorbs ADR-040, ADR-041 and ADR-042, which each corrected the one before.

**Context:** `HexGrouping { bytes_per_group, groups_per_line }` (§45) was specified,
persisted, and pinned by a profile round-trip test, yet read by nothing: both places that
built a `DisplayView` hardcoded `hex_bytes_per_line: 16`, and no GUI control set the
field. Mapping the config onto the existing line length would not have delivered §45,
because `bytes_per_group` means bytes *run together* between separators (`41424344` at
four), which the renderer could not express.

Hex is a fixed-width grid, and the grid is the reason to read it: byte *N* sits at a
predictable column, so a reader can follow one field down a column. Two things threaten
that, and each produced a defect. A wrap done by a generic character splitter cannot
know a separator from a digit, so it stranded separators and cut bytes in half. And a
`Mark` (§50.2) is text of arbitrary width: counted as one cell it overran the pane, and
spliced into a byte row it pushed everything after it to an unpredictable column. The
`NmeaZda` style makes it concrete: `$GPZDA,120000.00,08,08,2026,00,00*4F` is about 37
columns, more than an entire row of data at 80 columns.

**Decision:** Split the halves by where they belong.

- **`bytes_per_group` is rendering.** `DisplayView` has `hex_bytes_per_group`;
  `render_hex` runs that many bytes together and separates only groups. It therefore
  reaches the `.disp`: the recorded stream is the rendered stream, and the spacing is
  part of what was rendered. A zero resolves to 1 everywhere, since no group can be
  empty.
- **`groups_per_line` is layout.** It sets the viewer's line length and never reaches a
  recording: ADR-018 holds that `.disp` is never hard-wrapped, so `build_display_view`
  passes `hex_bytes_per_line: 0` ("do not wrap"), while the GUI passes a real length.
- **Fit-to-width (`groups_per_line: 0`) resolves to whole bytes before rendering.** `G`
  groups of `B` bytes occupy `G × B × 2` digits plus `G − 1` separators, so the largest
  `G` fitting `cols` columns is `(cols + 1) / (2B + 1)`. The renderer then wraps at that
  many bytes and the character splitter has nothing left to cut. A configured group
  count is clamped to what fits rather than overflowed, and when not one whole group
  fits the fallback is whole bytes. The splitter and the resolver read one
  `MIN_WRAP_COLS`, so they cannot disagree on width.
- **A line is bounded by both the byte budget and the pane's columns.** The renderer
  tracks the columns it has written, not only the cells, and ends a line when the next
  cell would not fit the pane or the byte budget is spent, whichever comes first. A byte
  is two columns, an annotation is as wide as its text, and a separator counts where one
  is emitted. `rebuild_rows`, which already receives the pane width, attaches it to the
  view, so a Hex line length can never be supplied without its column bound; the bound is
  tied to the line-length request, so the recorder's view still emits the exact rendered
  stream at any width (§54).
- **A Mark in Hex takes its own row, at the marked byte.** The byte run ends, the `Mark`
  text occupies its own line, and bytes resume on a fresh row and a fresh group. The break
  is at the marked byte, not the next row boundary, because a `Mark` identifies a byte and
  placing it only near the right row would answer a coarser question. This costs a short
  row, which is self-explaining: the reason for it is on the line below. `Mark` text that
  already ends in CR/LF gets no second break. Removing Marks from Hex was rejected: it is
  the mode for binary protocols, where "when did this pattern occur" is asked, and rule
  behaviour that changes with the display mode is a matrix the user would carry.
- **A group never breaks at a read boundary.** `HexCursor` carries group position across
  chunks alongside line position, so a transport's read sizes are not visible in the
  spacing.
- **The row cache's rebuild key includes grouping and the resolved line length**, so a
  changed setting restyles existing scrollback rather than only new bytes. A mark is
  admitted only inside the window, half-open like `delta_annotations`, so a mark on a
  delta boundary is spliced once.

**Boundary.** A `Mark` wider than the whole pane still overruns and is cut by the
character splitter. That cuts annotation text, never a byte, and is stated rather than
absorbed. The `.disp` follows the display, so a Hex recording carries Marks on their own
lines. It was never byte-exact; that is `.raw`, whose sidecar (`.raw.idx`, ADR-039) is
the precise, non-perturbing way to timestamp bytes.

**Known structural risk.** Hex still wraps in two stages: cell-aware rendering followed by
a generic character splitter, which remains for the text modes, where a character *is*
the unit. Two successive defects have lived in the seam between them. If this path is
touched again, the renderer should return final rows and `split_stream_rows` should not
see Hex at all.

**Consequences:** One invariant holds and is tested across grouping, line setting, pane
width, Mark width and placement: no pane width, grouping or annotation ever splits a
byte, and a byte row is only bytes and separators. A GUI control under Configure display
sets both halves in the units §45 and the schema use (bytes per group, groups per line),
with the resulting bytes per line shown alongside. The setting persists through
`ViewPrefs` like the other view presentation fields; the field already round-tripped
through TOML, so what this closed was the write side.

## ADR-039 — A sidecar offset is a position in a file, not a count for a run

**Status:** Accepted 2026-08-05. Fixes a defect that predated ADR-037/038 but
became reachable when the GUI toggle landed.

**Context:** `RawFileRecorder` counted from zero at construction and keyed each
`.raw.idx` line on that counter. Under `OverwritePolicy::Overwrite` and `Refuse`
the counter and the file position agree, because the file starts empty. Under
`AppendIfExists` they do not: the bytes join an existing file at its end while
the index claims they start at zero.

Three things made this the ordinary path rather than a corner. Append is the
default `overwrite_policy`. Rotation is the default `file_rotation`, and
`effective_overwrite` coerces Refuse to Append whenever rotation is on (§59),
because each period is meant to be resumable. So stopping and restarting a
recording inside its current period — the most natural thing a user does — hit
it every time.

The failure mode is the reason it went unnoticed. The `.raw` stays perfectly
byte-exact; only its index is wrong, and wrong in a way that still parses and
still looks plausible. Nothing surfaces until someone trusts a timestamp.

**Decision:** The recorder tracks `stream_offset` — the absolute position in the
destination where the next byte lands — seeded from the opened file's length.
The name carries the semantics: an index into a `.raw` counts from the start of
the file the bytes actually joined, never from the start of this run.

The length is read from the opened file rather than branched on the policy. A
truncated or freshly created destination reports zero, so one read covers all
three policies and cannot drift out of agreement with the `OpenOptions` above
it. `bytes_written()` became `stream_offset()`: it was documented as the
truncation point on fault (§56.1), which also wants a file position, so both of
its readings improve and neither had an external caller.

**Consequences:** Rotation is correct in both directions — a fresh period file
starts its index at zero because it is empty, and a reopened one continues,
through the same code path. A sidecar enabled on a later run indexes the file it
joins and simply says nothing about the bytes recorded before it existed, which
is the honest result rather than a special case.

The GUI help said offsets "restart at zero" under rotation. That was true of a
fresh period file and false of a reopened one, so the tooltip was teaching the
bug; it now states the invariant instead.

Pinned by `an_appended_sidecar_indexes_from_the_end_of_the_existing_file`,
`a_sidecar_added_to_an_existing_recording_starts_at_that_file_s_end`, and
`rotated_sidecars_index_their_own_period_file_across_a_restart`. All three were
confirmed to fail against the previous behaviour before the fix landed — the
append test reported offsets `0, 0, 2` where the file holds `0, 5, 7`.

## ADR-040 — Fit-to-width resolves to whole bytes before rendering

**Merged into ADR-038 on 2026-11-14.** The Hex layout rules this entry added or corrected are stated in ADR-038. Text in [REVISIONS.md](REVISIONS.md#retired-decisions).

## ADR-041 — A Hex line is bounded by columns, not by cells

**Merged into ADR-038 on 2026-11-14.** The Hex layout rules this entry added or corrected are stated in ADR-038. Text in [REVISIONS.md](REVISIONS.md#retired-decisions).

## ADR-042 — In Hex, a Mark takes a row of its own

**Merged into ADR-038 on 2026-11-14.** The Hex layout rules this entry added or corrected are stated in ADR-038. Text in [REVISIONS.md](REVISIONS.md#retired-decisions).

## ADR-043 — A recording continues across faults as numbered segments with recorded gaps

**Status:** Accepted 2026-09-30. Amends §56.1, §56.2 and §59. **Amended 2026-11-14:**
absorbs ADR-059.

**Context:** §56.1 made a recording fault terminal until a person re-enabled
recording. That suits troubleshooting with someone at the screen. It does not
suit weeks of unattended logging: one queue overflow, one write error or one
unplugged USB drive ends recording for the rest of the run while reception
carries on and nobody is there to press Record.

Recovery has to be designed rather than added, because the easy versions are
wrong. An implementation can move the blocking wait somewhere else, buffer
without bound, drop bytes silently, recreate a vanished folder on the wrong
disk, or churn out empty files under sustained overload. Three existing defects
belong to the same design:

- `begin_recording` opens files on the pipeline task, so a slow drive stalls
  ingest.
- Time rotation reopens with the configured policy, so a clock stepped back to a
  period already used reopens that file, and `Overwrite` truncates it.
- Nothing bounds a file's size. FAT32, common on USB drives, stops at 4 GiB.

**Decision:** Raw and Display recording each run the same controller,
independently.

- **States:** Off → Opening → Recording → Gap → Opening, and any state →
  Stopping → Off. Gap includes the wait before the next attempt.
- **File work never runs on the receive task.** Opening, rotating and finalizing
  happen in the recorder's own task. The pipeline only enqueues, without
  blocking.
- **While Opening,** bytes wait in the bounded recording queue. If the queue
  fills before the file opens, a gap starts there.
- **During a Gap,** bytes are deliberately omitted, never buffered. One gap stays
  open until a segment actually starts.
- **Gap record:** the stream offsets (§25, not file offsets) and wall-clock times
  at both ends, and a reason: open failure, write failure, queue overflow, low
  disk or destination missing. A recording end records manual stop or shutdown
  timeout the same way. Gaps go to the event log (ADR-044) and diagnostics, and a Display
  Recording also marks them in its file (below); there is no separate gaps file.
- **Retry:** 1 s, doubling to 30 s. **Anti-thrash:** a third fault within 10
  minutes stretches the retry to 5 minutes and raises a lasting "recording
  unstable" fault, which stays until recording is stopped.
- **Size cap:** soft. Rotation happens before a write that would exceed it, and a
  chunk is never split, so a sidecar offset always lands on a chunk. The default
  is 2 GiB and the minimum 64 MiB; a chunk is at most 64 KiB, so every file stays
  within its cap.
- **Segments:** `GPS_2026-09-30_08.raw` continues as `GPS_2026-09-30_08_2.raw`,
  `_3` and so on; a single-file destination `run.raw` continues as `run_2.raw`.
  - Names are allocated under the destination lock (ADR-014) by scanning what
    exists.
  - A numbered segment is always created new, never overwritten or appended.
  - Numbering restarts each period.
  - `.raw.idx` takes its name from its `.raw`.
- **`OverwritePolicy` applies to the first open of an enable only.** Every later
  open in that run — rotation, size or recovery — creates a new segment. On a
  restart within a period, `AppendIfExists` appends to the highest-numbered
  segment only if it is under the cap.
- **Rotation only moves forward.** A period earlier than the current one (the
  clock stepped back) keeps writing the current file.
- **Destination identity:** the first successful start writes a
  `.wiredata-destination` marker file in the recording folder. Recovery never
  creates that folder, and resumes only when the folder exists and holds the
  marker. On Linux an unplugged drive can leave its empty mount-point directory
  on the system disk; without the marker, recording does not resume there.
- **Append repair after an interrupted write:** raw bytes are written before
  their index line. On reopening to append, index entries past the end of `.raw`
  are trimmed and a half-written last line is dropped. Raw is authoritative: a
  raw tail with no index entry is kept and reported as "N bytes at the end have
  no timestamp". Index offsets must increase.
- **Low disk** is a lasting, prominent fault, not a warning that scrolls away.
  Display-only recordings are guarded too. With `StopRecording`, the recording
  enters a Gap with reason low disk and resumes in a new segment once free space
  is 10% above the threshold.
- **A Display Recording marks its gaps.** `RecordingStopReason` is gone, and a stop
  always writes the accepted backlog, then finalizes. When a Display Recording resumes
  after a gap that lost bytes, the new segment starts with one line, `‹GAP <reason> ·
  <start> to <end> · not recorded›` (§54), in the style of the `‹MARK›` lines but with
  no leading newline, since a resumed segment is always a fresh file. It is a
  per-recording choice, **Mark gaps** (`mark_gaps`), on by default, because after a long
  run a gap is what a reader most needs to find; a profile saved before the option
  existed loads with it on. A Raw Recording never gets the line: `.raw` stays exactly the
  bytes received (§53). No line is written for a gap that lost no bytes, or for one still
  open when recording stops; the event log reports both. The controller stays generic
  over its items: the recorder side writes the line, through a hook Raw recorders leave
  empty.
- **Retention is external.** Listener never deletes recordings. Status shows the
  total size of the recording's files and the destination's free space.
- **Queues are sized in bytes:** 8 MiB per recording, with a fixed allowance per
  chunk, replacing the 1,024-chunk count.
- **Shutdown:** a finalization that cannot finish within the shutdown grace is
  abandoned, because a blocked file operation cannot be cancelled. It is logged
  as "finalization incomplete", and the runtime shuts down within a time limit so
  a stuck thread cannot keep the process alive.

**Boundary:** Durability covers a process crash, not power loss: there is no
fsync. There is no automatic pruning, no buffering across a gap and no
pre-trigger capture. Recovery onto network filesystems is not supported, since
the marker is the only identity check. TCP connections are not recorded
(ADR-047).

**Alternatives considered:**

- **Keep §56.1's terminal fault:** Rejected for unattended logging, where nobody
  is there to re-enable.
- **Buffer bytes during a gap, in memory or a spool file:** Rejected. It is
  unbounded or moves the same failure to another disk.
- **Recreate the folder and retry:** Rejected, because of the Linux mount-point
  case.
- **Check the filesystem's volume ID:** More precise, but different on every
  platform. The marker is portable and catches the case that matters.
- **A hard cap that splits chunks:** Rejected. It breaks the chunk-to-offset
  sidecar model for a few bytes of precision.

**Consequences:** An unattended recording survives faults, and every loss is
recorded with its extent and cause. §56.1's guarantee moves from the recording
to each segment: every segment is contiguous and byte-exact for the data it
contains, with a known end. Readers join segments in name order and use the gap
records between them.

## ADR-044 — Diagnostics also go to a persistent daily event log

**Status:** Accepted 2026-09-30. Amends §118. Amended 2026-11-19: log files are
kept, not deleted after 30 days (talker ADR-069).

**Context:** Diagnostics live only in memory: bounded, per Channel, and gone when
the process exits or retention evicts them. `init_logging` writes to stdout, and
the runtime emits almost no `tracing` events. An unattended run that loses a
device at 03:00 leaves no trace for the person who arrives at 09:00. Talker
already has a bounded log-file worker with loss counting, gap markers and visible
failure (talker ADR-006).

**Decision:**

- Every diagnostic is also emitted through `tracing`, with the Channel name and
  its UUID as fields. The message text names the Channel by name.
- A file layer writes one file per local day under the platform's local-data
  directory, in `listener/logs`. It uses the shared `wiredata-log` worker (talker
  ADR-061): a dedicated thread behind a bounded, non-blocking handoff, a
  cumulative loss count, a gap line after a loss, a visible fault when the file
  cannot be opened or written, and a flush at shutdown.
- Listener turns a failed log back on about once a minute. The worker reports a
  failure and stops; trying again is the application's choice, so talker, whose
  file log has a person at the GUI, is unchanged.
- Log files are kept. They were deleted after 30 days until talker ADR-069.
- The CLI and the GUI both write it. The CLI prints the log folder at start.

**Boundary:** The event log holds diagnostics, never stream bytes, and is not a
recording (§114). There is no log shipping and no configurable folder yet.

**Alternatives considered:**

- **Blocking file writes:** Rejected. A stalled disk would stall the runtime.
- **`tracing-appender`'s non-blocking writer on its own:** Rejected. It drops
  lines without a gap marker or a count anyone sees.
- **One file per Channel:** Rejected. A cross-channel incident then needs several
  files merged by hand.

**Consequences:** §118 stops being optional, and persistent log rotation leaves
Appendix A. The log folder grows by one file a day until an operator removes
old files (talker ADR-069).

## ADR-045 — Unattended GUI: reconnect is an explicit choice, and resume is registered once

**Status:** Accepted 2026-09-30. Amends §9.1 presentation, §70 and §159.
Amended 2026-11-19: nothing gives up, so the "Gave up" status is gone (ADR-072).

**Context:** Auto-reconnect is opt-in (§9.1) and the GUI offers no control for
it, so a recording Channel stops for good at its first unplug. Separately, the
GUI never starts anything on launch (§70), so after a reboot a logging
workstation sits idle until someone clicks.

**Decision:**

- **Reconnect choice.** Each Channel's interface settings show a "Reconnect
  automatically" checkbox, with its behaviour stated in words.
  - The first time recording is enabled on a Channel with reconnect off, an
    inline choice appears, pre-selected to on. The user confirms either way.
  - Nothing changes a saved profile without the user choosing it.
  - Status reads "Reconnecting — attempt 3, next try in 8 s" and "Gave up after
    N attempts", in words, not colour alone.
- **Resume on launch.**
  - One profile at a time can be registered, in application state, not in the
    profile.
  - On launch, if the registered profile has moved or is invalid, or a recording
    destination lacks its marker (ADR-043), nothing starts and the GUI says why.
  - Otherwise a 10-second "Resuming <profile> in 10 s — Cancel" countdown runs
    before any port opens. The GUI then loads the profile, starts every Channel,
    begins each recording whose `enabled` flag is set, and shows "Resumed
    automatically at 08:14".

**Boundary:** Loading a profile still never starts anything (§70); resume is a
separate action the user registers. The CLI is always explicit about what it
starts. Listener does not install OS autostart entries; the `deploy/` examples
cover that for the CLI.

**Alternatives considered:**

- **A resume flag in each profile:** Rejected. Opening another profile to
  troubleshoot would then silently change what resumes.
- **Always resume the last profile:** Rejected. Ports opening unasked is a
  surprise, and on shared hardware a conflict.
- **Reconnect on by default in the schema:** Rejected. A saved profile would
  behave differently from what its file says.

**Consequences:** A recording Channel's reconnect state is visible and chosen.
A logging workstation that reboots resumes without a click, and gives anyone at
the screen ten seconds to stop it.

## ADR-046 — The listener CLI starts what it can and adopts the shared unattended contract

**Status:** Accepted 2026-09-30. Amends §113; adds CLI behaviour under §3.

**Context:** `listener` gives up when no Channel starts, ends on Ctrl-C only, and
reports start faults by UUID. Under systemd or Task Scheduler that means a
logging service which exits because a USB adapter enumerated late, and a stop
signal that skips finalization.

**Decision:** The listener CLI follows the workspace CLI contract in talker
ADR-060: start what can start, loud warnings that `--quiet` does not hide,
`--require-all`, the shared exit codes, and graceful stop on every OS stop
signal. What is specific to listener:

- A Channel that fails at start is retried under its `ReconnectPolicy`. A Channel
  with reconnect off stays down, and counts as never started.
- At start, a warning names each recording Channel that has reconnect off.
- The run summary lists recording gaps (ADR-043). Gaps alone do not change the
  exit code.
- A finalization abandoned at shutdown (ADR-043) gives exit code 4.
- Output names Channels by name, never by UUID.

**Boundary:** The CLI does not install itself as a service. The systemd unit and
Task Scheduler task in `deploy/` are examples.

**Consequences:** A headless listener outlives a late device, says loudly what
is down, and exits with a code a supervisor can act on.

## ADR-047 — The TCP Listener was removed; UDP states its bind scope and can request a shared port

**Status:** Accepted 2026-09-30. Amends §1, §16, §68, §73, §76, §85, §153, §154, §162 and
Appendix A. **Amended 2026-11-14 (task G6):** the TCP code is removed rather than
disabled, and ADR-024, which deferred surfacing TCP connections, is absorbed here.

**Context:** A TCP Listener accepted connections, but their data could not be displayed
or recorded. Every accepted connection ran a full pipeline, but the supervisor dropped
its request handle, so connections surfaced only connect and disconnect events: no
per-connection snapshot, stream view, recording (blocked on §59 filename templating) or
match rules, and `recv_buffer_bytes` was not applied to accepted sockets. Offering it
invited the belief that data was being captured. It also lacked what a shared network
needs: its template bound all interfaces with no connection cap, there was no keepalive,
and any accept error faulted the Channel. The spec read as if the full per-connection
surface existed.

For UDP, a second program cannot bind the same broadcast or multicast port, which users
sometimes need, and "0.0.0.0" does not tell a reader that the socket is reachable from
the network.

**Decision:**

- **The TCP Listener was removed, not disabled.** It is gone from `+ Add` and `--tcp`,
  and the TCP Listener and TCP Connection code is deleted. A profile that names a TCP
  Listener loads without that Channel and with a warning naming it. The CLI prints the
  warning and the GUI shows it on the profile status line.
- **Re-adding TCP needs a new decision.** The direction it would start from is
  ADR-024's: a supervisor that retains per-connection handles in a registry keyed by the
  minted `ChannelId`, with `Listener::snapshot` and `stream_delta` routed through it, and
  the GUI choosing between sub-tabs and dynamic top-level channels. It would also need a
  default connection cap, keepalive with idle, interval and probe count set for each OS,
  and per-connection handling of accept errors with rate-limited logging.
- **Bind scope in words.** The UDP editor lists local addresses, and labels the
  all-interfaces choice "All interfaces (reachable from the network)". The Channel
  header shows the scope. (Amended 2026-11-24: the header line gives
  the mode, address and port as talker's does, and its hover says who can
  reach the address. The user asked for the line to read as talker's; the
  editor's choice already names the scope.)
- **"Request shared port"** is a per-Channel UDP option, off by default and offered for
  broadcast and multicast only. It sets the platform's address-reuse option before
  binding, and status reports whether the OS applied it.

**Boundary:** This targets shared networks, not untrusted ones: there is no source
allow-list. Sharing is not offered for unicast, where the OS delivers each datagram to
only one of the sharing sockets.

**Alternatives considered:**

- **Disable the TCP Listener but keep its code and keep loading profiles that name it:**
  chosen first, when this entry was written with a profile naming one rejected at
  validation, and replaced on 2026-11-14. What the user sees is the same, the Channel
  left out with a message, while the profile no longer fails to load and no code carries
  a kind that cannot run.
- **Build the connection view now:** Out of scope for this remediation.

**Consequences:** Every Channel kind offered can display and record what it receives. A
UDP Channel says in words who can reach it. The spec no longer promises an inspection
surface the runtime does not have.

## ADR-048 — Profiles load strictly and runtime capacities are bounded

**Status:** Accepted 2026-09-30. Amends §71, §72.1, §80 and §88. Amended
2026-11-19: the reconnect-backoff limits went with the settings they checked
(ADR-072).

**Context:**

- Every profile field is `#[serde(default)]` and unknown keys are ignored, so a
  misspelled key silently becomes a default. For an unattended run that is a
  wrong setting nobody sees.
- A missing `schema_version` loads as the current one.
- `PipelineCapacities` is a plain struct that `Listener::new` trusts as given,
  and `channel_caps` passes a profile's `byte_limit` through unchecked. Nothing
  bounds process memory at the 16-channel headroom target.

**Decision:**

- **Unknown keys are refused.** The message names the key and where it is. Every
  profile struct gets serde's deny-unknown-fields attribute. Serde's support is
  limited with internally tagged enums and `flatten`, so each struct gets a test
  proving that a misspelled key is refused.
- **A missing `schema_version` is refused**, with "add `schema_version = 3`".
- **Limits,** enforced when a profile loads and in constructors:

  | Setting | Limit |
  |---|---|
  | Scrollback | ≤ 16 MiB per Channel |
  | Diagnostics | ≤ 2,000 per severity |
  | Match pattern | ≤ 256 bytes |
  | Rules | ≤ 64 per Channel |
  | Reconnect backoff | initial ≤ max, multiplier 1.0–10 (removed with the settings, ADR-072) |
  | Queue capacities | non-zero, with an upper bound |
  | Segment size cap | ≥ 64 MiB (ADR-043) |

- **`PipelineCapacities` becomes a validated type** with a fallible constructor,
  and `Listener::new` takes only the validated form.
- **Worst case is about 50 MiB per Channel:** a 16 MiB receive queue (256 chunks
  of up to 64 KiB), two 8 MiB recording queues, 16 MiB of scrollback and about
  2 MiB of diagnostics. That is about 800 MiB at 16 Channels, and far less in
  normal use. Socket buffers are kernel memory and are counted separately.

**Boundary:** The schema stays at 3. Strict parsing changes what is accepted, not
the format.

**Alternatives considered:**

- **Warn on unknown keys and continue:** Rejected. An unattended run would carry
  the wrong setting with the warning long scrolled away.
- **Enforce limits only in the GUI:** Rejected. The CLI and hand-edited profiles
  would bypass them.

**Consequences:** A profile either means what it says or does not load. Process
memory has a stated worst case that the soak test can check.

**Correction (2026-11-14):** The worst case above did not hold for dense
matching. A rule set can be written so that every byte matches many rules, and
one read then produced every match at once: 64 one-byte rules over a
65,507-byte UDP datagram is 4,192,448 matches, about 288 MiB of working
memory in one Channel. One read now fires at most 4,096 matches (ADR-065).

## ADR-049 — OS stop requests come from one shared crate, `wiredata-stop`

**Status:** Accepted 2026-09-30. Implements the stop signals of §3.1 and talker
ADR-060. Amended 2026-11-19 and 2026-11-20: the hold was cut to 10 s, then put
back to 30 s, because a stop that first waits out a slow serial open needs more
(ADR-052).

**Context:** §3.1 requires Ctrl-C, SIGTERM on Linux, and console close, logoff
and shutdown on Windows to perform the graceful stop of §113. On Windows,
console logoff and shutdown events reach only programs that have not loaded
`user32.dll` or `gdi32.dll`. `listener.exe` loads both: the GUI shares the
binary, and Windows' `setupapi.dll`, which serial-port enumeration needs, imports
both itself. A CLI binary without the GUI would still load them. Listening for
those console events therefore never fires, and a logoff or shutdown would end
the process without its graceful stop. Talker's CLI is in the same position.

**Decision:** A new internal crate, `wiredata-stop` (`publish = false`), turns
every OS stop request into one stream for both CLIs.

- It listens for Ctrl-C everywhere, SIGTERM on Unix, and Ctrl-Break and console
  close on Windows.
- On Windows a hidden top-level window, on its own thread, receives
  `WM_QUERYENDSESSION` and `WM_ENDSESSION`. Message-only windows do not receive
  those broadcasts. On `WM_ENDSESSION` it sends Logoff or Shutdown and holds
  the session until the application says its graceful stop has finished, up to
  30 s; Windows may end the process sooner.
- Every listener is registered before `listen` returns, and one that cannot be
  registered is reported to the application, which decides how to say so.
- The application owns the meaning: its graceful stop, its time limits and its
  output. Listener calls `finish` only after the runtime has stopped and the
  event log has flushed.

**Boundary:** The crate depends on `windows-sys` on Windows, and on Tokio only
behind its `tokio` feature. It does not install a service or handle
service-control requests. The GUI binary is not covered; it receives
session-end messages through its own window.

**Amended 2026-10-01 for talker:** talker has no async runtime (talker
ADR-002), so the Tokio stream sits behind the `tokio` feature, which listener
enables. Without it, `on_windows_stop` calls a function for Ctrl-C,
Ctrl-Break, console close, logoff and shutdown on Windows. Its console handler
holds a console close until the stop finishes, because Windows ends the
process as soon as the handler returns, and `ctrlc` returns at once. Talker
uses it on Windows and keeps `ctrlc`, with its `termination` feature for
SIGTERM and SIGHUP, on Unix.

**Alternatives considered:**

- **A copy in each CLI:** Rejected. The Win32 window code is subtle, and two
  copies drift.
- **A CLI binary without the GUI:** Rejected. `setupapi.dll` loads `user32.dll`
  regardless.
- **Accept the gap and amend §3.1:** Rejected. A Windows shutdown is an ordinary
  way for an unattended run to end.

**Consequences:** A Windows logoff or shutdown gets the same graceful stop as
Ctrl-C, within whatever time Windows allows.

## ADR-050 — Both CLIs share their unattended-run reporting in `wiredata-cli`

**Status:** Accepted 2026-10-01. Implements the reporting half of ADR-046 and
talker ADR-060.

**Context:** ADR-046 and talker ADR-060 give both CLIs one contract: a WARNING
when a channel does not start or goes down, a reminder every five minutes, a
line on recovery, a final summary, and exit codes 0, 2, 3 and 4. Listener built
that reporting first. Talker needs the same lines and the same rule for when a
run is degraded. Two copies would drift on exactly what an operator, or a
supervisor reading the exit code, depends on.

**Decision:** A new internal crate, `wiredata-cli` (`publish = false`, no
dependencies), holds the health tracker and the exit-code rule.

- The tracker is generic over each application's channel id. The application
  supplies, in its own words, what happens to a channel that is down, and adds
  to each summary line what only it knows: listener its recording faults and
  gaps, talker its send counters and dropped echo lines.
- The exit-code rule: an incomplete stop (4) wins over degraded (3), which wins
  over healthy (0). Exit code 2 is the application's to decide before the run
  starts; 1, an internal error, is reported by `main`.

**Boundary:** The crate does no I/O. Each application observes its own
runtime, prints the lines, and owns its retry policy and stop limits.

**Alternatives considered:**

- **A copy in each CLI:** Rejected, for the drift above, which ADR-044 and
  talker ADR-061 already rejected for the log worker.

**Consequences:** Both CLIs word the same events the same way, and agree on
when a run is degraded.

## ADR-051 — A soak harness that shares no code with Listener checks its recordings

**Status:** Accepted 2026-10-01. Serves the ADR-048 headroom target and the
recording guarantees of §56.1 and §59.

**Context:** A soak run must show that Listener records every datagram it
receives, for days, across rotations, size caps, gaps and recoveries. Counting
bytes shows that some data arrived, not which. A checker built on Listener's
own reader could share the fault it is meant to find.

**Decision:** A new internal crate, `wiredata-soak`, with two programs:

- **`soak-gen`** sends 64-byte datagrams, one series per stream, each carrying
  a marker, its stream, its sequence number, a filler and a checksum. A
  sequence number advances only when its send succeeds, so a refused send is
  not counted as Listener's loss. A manifest of what was sent is rewritten
  every second.
- **`soak-verify`** reads a Channel's `.raw` files and the event log as an
  operator would, and holds them to these rules:
  - A segment is contiguous: nothing missing, repeated or out of order.
  - Between segments, sequence numbers may jump only where a segment opened
    after a logged gap.
  - The run covers what was sent, unless it began after a gap or stopped
    during one.
  - Nothing appears twice or goes backwards, and every record is intact.

**Boundary:** The harness depends on neither application. It reads Listener's
gap lines by their wording, so a change to that wording must change the
harness too. It verifies `.raw` only: a `.disp` file is rendered text. Soak
runs stay out of CI.

**Alternatives considered:**

- **Talker as the generator:** Rejected. Talker carries no sequence numbers,
  and the harness would then test both applications at once.
- **Byte counts alone:** Rejected. They cannot tell a gap from reordering or
  from a duplicate.

**Consequences:** A soak run ends in a pass or a list of exactly which numbers
are missing, repeated or out of order, and where.

## ADR-052 — The GUI driver starts and stops Channels off its loop, one step at a time per Channel

**Status:** Accepted 2026-10-01. Follows from a latency test: the status must keep
updating at least once a second while one Channel's start blocks for 10 s.
Amended 2026-11-20: the CLI reads a stop request while a port opens.

**Context:** The GUI's driver awaited every start and stop inside the loop that
also sends the status. A serial port can take 10 s to open, as a Bluetooth port
whose device is off does, and that froze every readout for those 10 s. A stop
could hold the loop for its 3 s grace. The test saw no status update at all
during a 10 s start.

**Decision:**

- **Start and stop come in three phases.** First a synchronous begin: check the
  transition, mark the Channel Starting or Stopping, and build the interface
  or take the tasks out. Then an owned future: open or bind, or drain within
  the stop grace. Last a synchronous finish: wire the pipeline and land Running
  or Faulted, or land Stopped. `start` and `stop` still run the three back to
  back, as the CLI does.
- **The driver runs the future as a task** and lands its result when the task
  reports back, so the loop keeps sending status meanwhile.
- **Each Channel runs its lifecycle steps one at a time, in order.** A command
  for a Channel mid-step waits its turn: a Stop sent during a slow start runs
  when the start lands. Other Channels carry on, and two slow starts overlap.
  Reconnect attempts are steps too.
- **A workspace change waits for every Channel's steps.** This covers loading a
  profile, a new profile and a resume. It then removes, registers and starts
  as before.

**Added decision (2026-11-15):** Linemon's hidden Channel (ADR-070) is not part of
the workspace, so a workspace change does not wait for its steps. A slow serial
open in Linemon does not hold up a profile load.

**Boundary:** An interface open still cannot be cancelled. In the GUI, a Stop for
a Channel mid-open waits for the open to land; the CLI leaves an outstanding open
behind at exit. At exit the driver lands opens and drains in flight, within the
stop grace, before stopping what is live.

**Added decision (2026-11-20):** The CLI reads stop requests while a port opens.
Its first starts race each open against a stop request, and a reconnect attempt's
open runs as a task beside its loop; the attempt's stop still runs on the loop,
bounded by the stop grace. A stop request that comes while an open is outstanding
leaves the open behind: its Channel is still Starting, which the shutdown skips,
so the other Channels' recordings finish within the time a Windows logoff or
shutdown allows. An outside review found that a 10 s open could otherwise use up
the whole hold (ADR-049).

**Alternatives considered:**

- **Refuse a command for a Channel mid-step:** Rejected. A Stop sent during a
  10 s open would have to be sent again.
- **Cancel the open:** Not possible for a blocking serial open.

**Consequences:** The status keeps updating while any Channel starts or stops,
and a slow Channel delays only its own commands.

## ADR-053 — Three hot paths stay as they are, on measurement

**Status:** Accepted 2026-10-01. Records investigations from the external reviews
of 2026-07-11 and 2026-07-12, which were kept in TODO.md until now.

**Context:** Reviews suggested three optimizations. Each was measured with the
criterion benches in `listener/benches` before anything was built. Absolute
numbers swing about threefold with load on the machine, so only comparisons
within one run count.

**Decision:** None of the three is built.

- **Scrollback eviction.** A 64-byte chunk cost 460 ns below the scrollback
  cap and 436 ns at it, with overlapping confidence intervals, in the same run.
  Eviction is free. (A first measurement compared the capped state with itself;
  the second review caught that, and this is the corrected one.)
- **Match scanning.** Scanning is linear: about 195 ns per rule per chunk, from
  400 ns with one rule to 6.4 µs with 32, on 64-byte chunks. 32 rules at 1,000
  chunks a second take 0.6% of a core. Firing adds the same order again: about
  85 ns a firing with one rule, 40 ns amortized with eight, and a `Notify`
  about 330 ns for its diagnostic. A multi-pattern scanner such as Aho–Corasick
  is worth building only past about 50 rules or 2,000 chunks a second sustained.
- **The selected Channel's diagnostics.** At the retained cap, a snapshot costs
  about 82 µs and the GUI's clone and sort of the list about 18 µs. At the 5 Hz
  poll that is about 0.05% of a core, twenty times under the 1% at which the
  reviews proposed acting. Sequence numbers or deltas are not justified.

**Boundary:** The thresholds are measurements, not limits. The ADR-048 cap of 64
rules sits just past the scanning threshold, and the ADR-048 headroom target
is 1,000 chunks a second: 64 rules then cost about 1.3% of a core. Measure again,
same run against the current code, before building any of these.

**Consequences:** No multi-pattern scanner, no eviction rework and no
diagnostics deltas, until a new measurement crosses its threshold.

## ADR-054 — A Channel whose task ends by itself is faulted

**Status:** Accepted 2026-11-12.

**Context:** A running Channel is a transport that receives, a pipeline that
processes, and a monitor that watches the transport (ADR-006). The spec says
what a Channel does when its transport reports an error. It does not say what
happens when one of the tasks simply ends: a receive task that panics, a serial
thread that ends without reporting, a pipeline that ends under a running
transport. In a run left alone for weeks, that is the failure that matters
most, because nobody is watching.

**Decision:** An end the Channel was not asked for is a fault.

- A transport that ends without reporting an outcome is reported as `Faulted`.
- A transport that reports `Completed` is faulted by the monitor unless the
  Channel's stop has begun. Serial and UDP report `Completed` only when the
  pipeline's queue has closed, so outside a stop it means the pipeline is gone.
- The fault takes the ordinary path: the Channel reads Faulted, `ChannelFaulted`
  is sent, and the reconnect policy (§9.1) restarts the Channel if it has one.
- The cause goes to the Channel's diagnostics when its pipeline is there to
  record it, and straight to the event log (§118) when it is not.

A stop cannot be taken for a fault. Stop cancels the transport before either
task ends, and the monitor looks at that cancellation only after the transport
has ended.

**Boundary:**

- It detects a task that has ended, not one that is alive and stuck. A pipeline
  blocked forever still reads Running.
- It relies on a panic unwinding one task. A build that aborts on panic ends
  the process instead.
- When the pipeline is the part that ended, the cause is in the event log only.
  A pipeline that is gone returns no final snapshot, so the Channel's retained
  diagnostics do not show it.
- A TCP connection, disabled in this release (§4.1), is supervised separately:
  there `Completed` is still a client that closed. (TCP receive code removed 2026-11-14, ADR-047.)

**Consequences:** `Completed` no longer means "not a fault" on its own; whoever
awaits the transport decides. A Channel with reconnect on now restarts after a
failure inside Listener, with a new pipeline; one with reconnect off stays
Faulted and says why.

## ADR-055 — Console output goes through a bounded queue that drops

**Status:** Accepted 2026-11-12.

**Context:** Listener wrote to the console from the threads doing the work.
Each Channel's pipeline wrote its own log lines, and the CLI's loop, which also
drives reconnect, printed its own. A console can stop taking output: selecting
text pauses a classic Windows console window, and a pipe stalls when its reader
does.

Measured on 2026-11-12, with one UDP Channel at 100 datagrams a second and a
match rule logging a line for each: a text selection held for 118 s stopped
that Channel's processing for 118.1 s. Nothing was lost, but only because the
4 MiB socket buffer and the ingest queue held the 755 kB that arrived
meanwhile. A longer pause, a faster feed or a Serial Channel would lose data.
The natural moment to select console text is when an error appears.

**Decision:** While Channels run, nothing writes to the console directly.

- Standard output and standard error each have a bounded queue
  (`CONSOLE_QUEUE_LINES`, 4,096 lines) and a worker thread that writes it out.
  The stdout log layer and every line the CLI prints go through them.
- A full queue drops the line and counts it. Nothing waits.
- The closing lines, the summary among them, are printed directly once the
  Channels have stopped and the queues are written out, so they are never among
  the dropped lines. A count of the dropped lines follows them.
- Talker's stdout log goes through a queue of the same kind (talker ADR-064).

The same test afterwards, with a selection held for about a minute, showed no
gap longer than 0.11 s between that Channel's log lines over five minutes, and
all 30,000 datagrams were recorded.

**Boundary:**

- Console output is best-effort. The event log (§118) is the record; it has
  its own worker and is not affected.
- An exit with the console still paused waits for it, as it did before. Every
  Channel has stopped by then.
- The queue is bounded in lines, not bytes.
- The GUI prints nothing of its own. Its stdout log goes through the same
  queue.

**Alternatives considered:** A console sink in `wiredata-log`, which would mark
the loss in the console itself with a gap line. Not built: that crate owns only
the log-file mechanism (talker ADR-061), and the count at the end says the same
thing.

**Consequences:** A paused or stalled console costs console lines and nothing
else. Listener now depends directly on `tracing-appender`, which the workspace
already used.

## ADR-056 — Hover text opens with a bold sentence, then short paragraphs

**Status:** Accepted 2026-11-13.

**Context:** Talker adopted a hover format on 13 November (talker ADR-065)
after the user found its hovers hard to read: dense paragraphs in internal
terms, some describing cases not on screen. Listener's hovers had the same
shape, and listener's own ADR-019 asks the two apps to look like one product.

**Decision:** Listener follows talker ADR-065 as written: a bold first
sentence that states the point, short paragraphs in plain, complete sentences
using the names on screen, one plain line for short hints, and no "/" as a
separator. It uses the same `Hover` and `ShowHover` from `wiredata-ui`. What
each hover says stays in listener.

Checking each hover against the code while rewriting it found nine that
described behaviour the code does not have. The largest: the multicast
Binding address hover said the chosen address picks the interface that joins
the group. It does not: the GUI never sets `multicast_interface`, so the group
is joined on the operating system's default interface. The hovers now say
what the code does; the behaviour is unchanged.

**Boundary:** Text only. Whether the GUI should let the user choose the
multicast join interface is a separate, open question.

**Consequences:** Listener's and talker's hovers look and read alike.

## ADR-057 — The Receive diagnostics card has no status badge

**Merged into ADR-033 on 2026-11-14.** The card has no title and no status badge; ADR-033 states it. Text in [REVISIONS.md](REVISIONS.md#retired-decisions).

## ADR-058 — A reconnect sequence ends only after 60 s up

**Status:** Accepted 2026-11-13. Amended 2026-11-19: there is no attempt limit
any more (ADR-072); the 60 s rule is unchanged.

**Context:** A faulted Channel reconnects under its `ReconnectPolicy` (§9.1):
attempts with a growing backoff, up to `max_attempts`. A successful attempt
used to end the sequence at once, so a device that connected and dropped a
moment later started again from attempt 1 and the shortest delay. It was
retried at that rate for ever, `max_attempts` never applied, and each cycle
wrote several event-log lines: the wrong shape for weeks-long logging, where
a misbehaving device should be retried patiently and its noise kept down.

**Decision:** A successful reconnect leaves the attempt count and backoff in
place until the Channel has stayed up for 60 s (`RECONNECT_STABLE_AFTER` in
`runtime/listener.rs`). A fault within that time continues the sequence: the
next attempt number and the next backoff step, counted towards
`max_attempts`. Staying up for 60 s, or a manual stop, starts it over.

**Boundary:** The recorder's own rule for repeated faults (3 within 10 min,
then a retry every 5 min) is separate and unchanged. Reconnect off still
means no automatic reconnect.

**Consequences:** A flapping device is retried at a growing interval and,
with `max_attempts` set, gives up. A device that stays up a minute between
faults is treated as recovered each time.

## ADR-059 — Display recordings mark their gaps; fault-finalization is gone

**Merged into ADR-043 on 2026-11-14.** A Display Recording marks its gaps, and a stop always finalizes; ADR-043 states both. Text in [REVISIONS.md](REVISIONS.md#retired-decisions).

## ADR-060 — One display view, in the profile as well as the runtime

**Status:** Accepted 2026-11-13. Corrects ADR-023.

**Context:** ADR-023 made a Channel show one display view but kept the
profile's `views` list, a `DisplayViewId`, per-view pause handles and a
`Vec` of views in the pipeline, all for a multi-view feature that is deferred
(spec Appendix A). Everything that ran used the first entry; the rest were
read and ignored, which let a profile say things that had no effect.

**Decision:**

- A Channel has exactly one view: `DisplayConfig.view`, saved as the
  `display.view` table. `DisplayViewId`, the view list, the per-view pause
  handles and `PauseDisplay`'s target are removed; pause applies to the
  Channel's display.
- **Old profiles still load.** The strict parse runs first. Only if it fails
  is the parsed TOML rewritten from the old form (`config/legacy.rs`) and
  parsed strictly again; when nothing was rewritten, the original error and
  its line number stand. A `views` list keeps its first view; more than one,
  or a `PauseDisplay` aimed at a later view, gives a warning that names the
  Channel. The CLI prints it; the GUI shows it on the profile status line.
  A saved profile always uses the new form.
- An empty or missing view is the default Raw / Utf8 / Native / NoWrap view,
  in the display and the Display Recording alike; the GUI used a different
  default before.

**Boundary:** Multiple simultaneous views remain deferred (Appendix A); they
would bring back a list, deliberately, with what it needs. The example
profile was removed at the same time, at the user's request.

**Consequences:** A profile can no longer describe views that never show.
A GUI profile load with both upgrade warnings and skipped Channels reports
only the skipped Channels, since both use the one status line.

## ADR-061 — The disk check scans a recording's folder once per segment

**Status:** Accepted 2026-11-14.

**Context:** The disk-space guard (§56.2) checks every 5 s and shows how much
the recording's files take. It found that by scanning the destination folder
on every check. Measured on 2026-11-14 (debug build, `%TEMP%`), a scan over
20,000 matching files took 59 to 80 ms: past the 50 ms set as the point to
act, and a cost that grows for ever on a weeks-long hourly recording.

**Decision:** The folder is scanned when the current segment changes, or
when no segment is open. Each 5 s check then reads free space as before and
adds two file sizes, the current segment and its `.idx`, to the stored scan
of everything else.

**Boundary:** The figures shown are the same. A file that appears in the
folder between segment opens is counted from the next open, not at once.

**Consequences:** A check's cost no longer grows with the number of files.

## ADR-062 — One stuck Channel cannot hold up status or recording begins

**Status:** Accepted 2026-11-14.

**Context:** The third review pass found that a Faulted Channel keeps its
handle while its pipeline task finalizes, with no time limit, and that every
status request to it waited for the reply with no limit either. A recording
close stuck on a removed USB disk or a dead network drive therefore froze the
status of every Channel, since the GUI polls them one at a time, and queued
Stop behind it; the CLI did not see Ctrl-C meanwhile. Separately, a Begin
waited up to 2 s for detached stops before checking whether recording was
already on, so a frequent `Record Begin` rule could hold up reception.

**Decision:**

- A status request waits at most 300 ms (`STATUS_REPLY_WAIT`), send and reply
  together. On a timeout, snapshot and stats serve the last good answer of the
  current run, stamped with the Channel's state, or the retained figures when
  there is none. A stream delta returns nothing, since an old one would repeat
  bytes.
- A Faulted Channel's own finalization is bounded by the stop grace period, as
  a user Stop is, and logs "finalization incomplete" on expiry (§113).
- A Begin on a recorder that is already running returns at once.

**Boundary:** A stuck Channel still costs up to 300 ms of each poll.

**Correction (2026-11-14):** As first built, a Channel that faulted with a
stuck close and was then stopped did not set the CLI's exit code 4. Its
pipeline task had already ended, so the stop's drain finished in time and
counted as complete. The drain now also reports whether the pipeline's own
close gave up, and that counts as finalization incomplete, named once. The run
summary carries the same fact, separately from whether a final snapshot exists.

**Correction (2026-11-14), on the figures served:** The last good answer was
handed out with nothing to say it was old, so a throughput or a stall length
could sit frozen and look live, and a Channel slow to answer could show figures
much older than one poll. Each answer now says whether it is fresh (ADR-068).
The cost of stuck Channels to one poll is also limited (ADR-069).

**Consequences:** Every other Channel's status keeps moving while one is
stuck, and a running Channel slow to answer shows figures at most one poll old.

**Correction (2026-11-15), on a failed pipeline:** A pipeline task that
failed before handing back its close was treated like a Channel with no
pipeline, and counted as complete. It now counts as finalization incomplete,
is named at shutdown, and logs that it failed before confirming its close.

## ADR-063 — Listener follows the workspace time policy

**Status:** Accepted 2026-11-14.

**Context:** Talker ADR-067 sets one policy for every time of day in both
apps: local unless a format, a labelled option or the feature's name says UTC,
with a zone on whatever is written to a file, a log or the wire.

**Decision:** Listener follows it. Two things changed. Gap times, in the
`‹GAP …›` line of a Display Recording and in the gap records of the event log
and diagnostics, are ISO 8601 local time with milliseconds and offset
(`2026-11-13T14:02:04.000-05:00`); they had a space for the `T` and no offset.
The GUI shows a finished run's start and end in local time; it showed UTC.

**Boundary:** Already in line and unchanged: event-log and console lines
(local with offset), plain timestamp marks (local, with an optional offset),
rotating file names (local), the timestamp sidecar (Unix time) and ZDA marks
(UTC fields, NMEA's zone sign, ADR-025). The copied run report keeps its
`started_utc=` and `finished_utc=` keys.

**Consequences:** A time read from any listener file can be placed without
knowing the computer's zone or the season.

## ADR-064 — A Record action takes effect at its match, not at the end of the read

**Status:** Accepted 2026-11-14.

**Context:** Record actions were applied after the whole read that held their
match had been offered to the recordings. A Begin therefore recorded nothing
from that read, not even what followed its trigger, and a Stop recorded all of
it, including what followed its trigger. Where the transport happened to cut
the stream into reads then decided which bytes were recorded. The owner's
choice is that both triggers belong in the recording.

**Decision:** A Begin starts the recording at its match's first byte. A Stop
ends it just after its match's last byte. The same stream therefore records the
same bytes however the transport split it into reads, including a trigger split
across two reads, whose first bytes are taken from the few bytes the matcher
keeps for cross-read matching. Actions inside one read apply in stream order,
not rule order. A Begin while recording and a Stop while not recording do
nothing. This holds for the Raw and the Display recording alike, and the
Display recording keeps its inline Mark timestamps inside the recorded span.
A read that fired Record actions is offered to the recordings those actions
drive only when the actions are applied, sliced at the matches.

**Boundary:** No bytes before the Begin trigger are recorded; pre-trigger
capture stays deferred (§158, Appendix A). An Idle rule's Record action has no
place in the stream and acts between reads.

**Consequences:** A recording made by triggers holds the trigger bytes and what
lies between them, and nothing else.

**Correction (2026-11-15):** The guarantee above did not hold for overlapping
triggers. A Stop whose match ends inside a Begin's match completes first, so
whether the two land in one read decided the result: with Begin `ABCDE` and
Stop `BC`, one read recorded `ABC` and two reads recorded `ABCDE`. Validation
now refuses such a pair when both drive the same recording (§71); every other
arrangement gives one result. A bare Mark is now placed before its match, in
the same recording span as the match's bytes; it was written at once, before
the read's Record actions applied, so one read lost it and two reads kept it.

## ADR-065 — One read fires at most 4,096 matches

**Status:** Accepted 2026-11-14. Corrects ADR-048's memory bound.

**Context:** A rule set can be written so that every byte matches many rules.
64 one-byte rules over a 65,507-byte UDP datagram is 4,192,448 matches, about
288 MiB of working memory, which broke the per-channel memory bound stated in
ADR-048.

**Decision:** Matches fire in stream order of their first byte, and matches
starting on the same byte fire in rule order. One read fires at most 4,096
matches (`MAX_FIRINGS_PER_READ`). Matches later in that read do not fire and
are not acted on. The first time this happens in a run a warning is recorded;
later ones are counted only.

**Boundary:** A read cut off this way is the one case where what a rule set
does depends on read boundaries. Normal rule sets never reach the limit.

**Consequences:** One read's matching work and memory are fixed whatever the
rules and the data.

**Correction (2026-11-15):** Limiting the firings did not fix the work and
memory of one read as stated: each firing copied its rule's whole action list,
and nothing limited how many actions a rule had. 4,096 firings of a rule with
1,024 actions was 224 MiB. A firing now shares its rule's actions, a rule may
have at most 16 actions, and a Mark separator at most 256 bytes (§71). Matches
that began in an earlier read are now put in stream order before the limit is
applied; they were cut in rule order, which could keep later matches and drop
earlier ones.

## ADR-066 — One-time GUI results are never dropped

**Status:** Accepted 2026-11-14. Amends ADR-008.

**Context:** The driver sent the GUI every update through one bounded queue
that drops when full. Most updates are repeated by the next poll, but some are
said once. A lost "Channel added" left a Channel the GUI never showed, a lost
"removed" left a ghost, and a lost "reconfigured" left an old editor baseline
that could later overwrite newer settings.

**Decision:** The driver sends the GUI two kinds of update. Periodic ones
(snapshots, statistics, stream deltas, control lines, and the high-rate
advisory events the snapshot also carries) go through a bounded queue and are
dropped when it is full, since the next poll repeats them. One-time results (a
Channel added, removed, renamed or reconfigured; lifecycle, recording and
reconnect events; profile results; errors) go through an unbounded lane and are
never dropped. The GUI applies every waiting one-time result before any
periodic update.

**Boundary:** Events into the driver from the runtime stay advisory (ADR-006).
The unbounded lane is safe because its volume is bounded by user actions and by
profile size, not by the polling rate.

**Consequences:** The GUI's list of Channels and its editor baselines cannot be
left wrong by load.

**Correction (2026-11-15):** Two parts of the decision above were wrong.
Lifecycle, recording and reconnect events on the reliable lane were applied
ahead of older periodic updates, so a Running status already queued could land
after a newer Stop and show the Channel running. They also need no user
action, so the unbounded lane was not bounded by user actions after all. Every
runtime event now travels on the periodic lane, in order with the snapshots;
the next poll rebuilds the state an event carries. A dropped recording-fault
event can miss its inline notice, but not the recording's state or its
diagnostic. Only results of user actions and profile loads are never dropped.

## ADR-067 — Profile defaults are written into each Channel at load

**Status:** Accepted 2026-11-14.

**Context:** Validation accepted a Channel that left its retention unset by
reading the profile's default, but neither the GUI loader nor the CLI loader
gave that default to the Channel, and the display default was not used at all.
A profile could pass validation and run with other limits than it asked for.
A GUI Save also wrote an empty defaults section.

**Decision:** The defaults are resolved once, at load, by one function that
validation, the GUI loader and the CLI loader all use. Each Channel that leaves
a setting unset takes the profile's default and from then on carries its own
explicit value. A GUI Save therefore writes per-Channel values with an empty
defaults section, and loading that file behaves the same. Retention is
inherited whole when the Channel sets no limit at all; individual limits are
never merged. Display is inherited when the Channel's display equals the
built-in default.

**Boundary:** Display settings have no unset state, so a Channel cannot ask for
the built-in display under a profile whose default differs.

**Consequences:** What the CLI runs is what the GUI showed, and a saved profile
no longer depends on its defaults section.

## ADR-068 — A status answer says whether its figures are fresh

**Status:** Accepted 2026-11-14. Extends ADR-062.

**Context:** ADR-062 serves a Channel's last good answer when it does not
answer a status request in time. That answer carried no time of measurement,
so the GUI showed old figures exactly as it shows live ones.

**Decision:** Every snapshot and statistics answer carries where its figures
came from. Fresh: measured for this request, or the exact final figures of a
Channel that has stopped. Cached: the Channel's last answer in this run, with
its age, worked out when the answer is handed out. Retained: an earlier run's
figures, because the Channel has not answered in this run. When the figures
shown are not fresh, the diagnostics card says so in a sentence and gives the
age. A stopped or faulted Channel's timing window is called its final ten
seconds only when the figures are fresh.

**Boundary:** The figures themselves are shown as measured. A rate or an active
duration is not aged or zeroed; the sentence is what says it may no longer be
true. A single slow answer shows the sentence until the next answer arrives.

**Consequences:** Nobody reads a frozen figure as a live one.

**Correction (2026-11-15):** A stopped Channel's figures were always labelled
fresh. When its latest run ended without a final snapshot, they were the
previous run's figures, or none. They are now labelled as an earlier run's
figures, and the card says the latest run has no status answer to show.

## ADR-069 — The GUI's driver waits on nothing without a limit

**Status:** Accepted 2026-11-14. Extends ADR-052.

**Context:** ADR-052 moved starts and stops off the driver's loop. Three other
waits remained on it: handing a recording command or a serial control-line
command to a Channel whose queue was full, profile file work, and a round of
status requests, which costs 300 ms for each Channel that does not answer.
While the loop waits, status, reconnects and Shutdown stop for every Channel.

**Decision:**

- A recording command or a serial control-line command waits at most 300 ms
  (`COMMAND_HANDOFF_WAIT`) for its Channel to take it. After that it is given
  up and reported; it was not sent and can be tried again.
- Profile reads and writes, and the resume check, run off the loop. They run
  one at a time, in the order asked for. Shutdown does not wait for one.
- A round of status requests ends after 600 ms (`POLL_ROUND_LIMIT`). The next
  round starts with the Channels the last one did not reach, so stuck Channels
  cannot starve the ones after them.

**Boundary:** Other commands are served while profile file work is
outstanding, so a Channel can change before a load lands. A round can overrun
its limit by the requests of the last Channel it visited. A file operation
that never returns leaves its thread behind until the process exits.

**Consequences:** A hung share, a stalled serial reader or several stuck
Channels slow the GUI's figures; they do not freeze it or hold up closing it.

## ADR-070 — Linemon: a window for one stream

**Status:** Accepted 2026-11-15.

**Context:** Listener serves two uses of equal weight: unattended logging on
many Channels for weeks, and looking at one stream to understand it. The first
is well covered. The second was not. Three cases showed it: finding a GPS among
four serial ports and working out its serial settings; clicking through a dozen
consecutive UDP ports to find one instrument; and checking a serial stream for
unexpected characters. In each, the Channel list and the recording controls
were in the way, the display had to be dug for, and it could not be made
larger.

**Decision:** Linemon is a second window for one stream, opened by one button
in the main window (§50.3).

- It reads one serial port or one UDP port, and only that port is open.
- A drop-down and step buttons choose the port. Every port and setting change
  takes effect at once; there is no Apply button.
- The display keeps its data across changes, with a marker line at each one,
  until Clear. A checkbox clears it on a change of port.
- It has Pause and Copy, and the renderings and view controls a Channel's
  display has.
- It remembers its last port, settings and view. It is not a Channel and is
  never in a profile, a recording or a run summary.
- A port held by a running Channel is not opened; Linemon names the Channel.

It is built on what exists. A hidden Channel in the runtime backs the window,
so the transports, the pipeline and the renderer are reused, and a change of
port or setting restarts that Channel. Two things are new: a display buffer in
the window that survives those restarts, and a second live stream to the GUI
beside the selected Channel's.

The window is drawn from the part of the application that runs every frame,
whether or not the main window is visible. eframe calls the main window's
drawing only while that window is visible, and takes down any window that was
not drawn in a frame, so Linemon drawn from there vanished whenever the main
window was minimized.

**Added decision (2026-11-15):** Start and Stop buttons open and close the port,
and Stop keeps the display. A checkbox, Reopen port on start, decides whether
opening the window opens the last port or waits for Start. While Linemon is
stopped, a change waits for Start. A value typed into a field, such as a
multicast group, takes effect only when the field is left, so a half-typed
value never opens a port.

A port held by the other side is refused the same way in both directions, before
anything opens: Linemon names the Channel that has the port, and a Channel names
Linemon. A refused port never opens on its own when it comes free; Start opens
it. Each of the other automatic opens (reconnect, resume, talker's recovery and
its CLI's open retry) is switched on by the user or reopens a port already
started, and this one would have been neither.

**Boundary:** Linemon does no decoding (ADR-010 stands) and shows one view
(ADR-023 stands). It reads serial and UDP only. Viewing a running Channel's
stream, a record button, time stamps on each line, finding the baud rate
automatically, TCP and find are deferred (Appendix A). A counterpart in talker
is undecided.

**Consequences:** Looking at a stream no longer means setting up a Channel.
While the window is open it costs one more Channel's memory, within the same
limits (ADR-048); closed, it costs nothing. A logging run is not disturbed by
it, since it never opens a port a Channel holds.

**Added decision (2026-11-15), Linemon on its own:** Opened from Listener,
Linemon was clutter in the logging tool, so it becomes a program of its own,
`linemon`, built from this crate. The window, its behaviour and its hidden
Channel are unchanged; the program around it holds only that window. What
looking at a running Channel's stream needed moves into Listener instead, as
the pop-out display (ADR-071).

Three parts of the decision above end with the move. The two programs cannot
see each other's ports, so Linemon no longer names a Channel that holds a port,
a Channel no longer names Linemon, and each shows the system's reason when a
port is taken. For the same reason the consequence above that a logging run is
never disturbed no longer holds: Linemon can take a port that a Channel in
Listener is waiting to reconnect to. And Linemon is drawn as its program's own
window, so the reason for drawing it every frame no longer applies. Linemon keeps its own saved settings
and writes no event log. Its deferred list shrinks to a record button, time
stamps on each line, finding the baud rate, TCP and find.

**Added decision (2026-11-19):** The serial port is chosen from the drop-down only;
the step buttons stay for the UDP port number. The serial settings take two lines,
and the second holds flow control and the control lines (§161): RTS and DTR as
buttons, CTS, DSR, DCD and RI as circles. What the outputs show follows ADR-073.

## ADR-071 — A Channel's display pops out into a window of its own

**Status:** Accepted 2026-11-15.

**Context:** Looking closely at one stream also happens with a Channel that is
already running, and Listener's display sits in the main window at the size
the rest of the window leaves it. Linemon looked at such a stream from inside
Listener, which was clutter (ADR-070). What was wanted was the Channel's own
display, larger, with nothing that could change the Channel.

**Decision:** A Pop out button opens a Channel's display in a window of its own
(§50.4).

- One pop-out per Channel; several Channels can each have one open. The main
  window keeps its display.
- It shows the Channel's received bytes with no insertions: no Mark timestamps
  and no other spliced text. It clears when the Channel restarts.
- It has Pause, Clear and the view controls of the Channel's display, apart
  from the scroll-buffer size, each acting on the pop-out alone, and none kept
  after it closes. A paused pop-out keeps its picture through a restart until
  Resume.
- It never changes the Channel's settings, starts or stops it, or records.
- It shows what the Channel's display holds, so pausing that display pauses
  the pop-out too, and the pop-out says so. The Channel's paused state rides in
  the status poll every Channel gets, so a pop-out of a Channel that is not
  selected knows it.

The GUI streams bytes only for the selected Channel today, so each popped-out
Channel gets its stream too. A pop-out reads the Channel's bytes as the GUI
already holds them and keeps only its own pause point, clear point, view
settings and rendering.

**Boundary:** A pop-out is a second rendering of the same bytes on screen, not
a second Display View: the Channel still has one view (ADR-023 stands), and its
display recording follows that view. The pop-out keeps nothing across
launches and is in no profile.

**Consequences:** A running Channel's stream can be read at any size, on any
monitor, without risk to the Channel or its recordings. Each pop-out costs one
more stream to the GUI and its own rendering while it is open.

## ADR-072 — Reconnect is on or off, at one fixed pace

**Status:** Accepted 2026-11-19. Amends ADR-045, ADR-048 and ADR-058.

**Context:** `ReconnectPolicy` held five values: `enabled`, a first and a
longest wait, a multiplier, and an optional attempt limit. The GUI sets only
`enabled` (ADR-045), so the other four could be changed only by editing a
profile. Their limits (ADR-048) accepted a first wait of 0 ms, which made an
attempt at every reconnect check, twice a second, for ever. The pace they
defaulted to, 1 s doubling to 30 s, was also written out separately for a
recording's reopen (§56). Neither primary use needs to tune it: someone
troubleshooting presses Start, and a weeks-long log wants patient retrying that
never stops.

**Decision:**

- `ReconnectPolicy` holds only `enabled`.
- Reconnecting waits 1 s, doubling to 30 s, and goes on until the Channel
  starts. The pace lives in the `retry` module (`RETRY_FIRST`, `RETRY_MAX`),
  which a recording's reopen uses too, so the two cannot drift. Tests shorten
  it through `Backoff`.
- Nothing gives up: the attempt limit, the give-up event and the "gave up
  after N attempts" status are removed.
- The 500 ms at which both front ends look for a due reconnect is one
  constant, `RECONNECT_CHECK_EVERY`.

**Boundary:** ADR-058's 60 s rule stands: a fault within 60 s of reconnecting
carries the attempt count and the wait forward. Talker's retries keep their
own constants. A profile that still carries the removed keys is refused, as
any unknown key is; no profiles are in use, and both schemas return to 1 at
release.

**Alternatives considered:**

- **Bound the four values instead:** Rejected. It keeps settings no screen
  shows, to serve a need nobody has raised.
- **Put the four values in the channel editor:** Rejected. Four number fields
  for a pace nobody needs to change.
- **Keep only the attempt limit:** Deferred. It is the one value with a clear
  use, and one key can be added back if someone needs it.

**Consequences:** Every Channel with reconnect on retries the same way, and
its hover states the pace. A Channel never stops retrying by itself; Stop
ends it. A start fault that keeps repeating is written to the event log once,
then at most once every 10 minutes with how many times it failed meanwhile, so a
Channel that can never start does not fill the log (§118).

## ADR-073 — Serial outputs are set at open, so what is shown is true

**Status:** Accepted 2026-11-20. Amends §14.3, §50.3 and §161.

**Context:** Listener cannot read an output line back: what it shows for RTS and
DTR is what it last set. A line the profile left unset was left at the operating
system's level and reported as low. On Windows the serial library opens with both
low, except RTS under RTS/CTS flow control, which it sets high; on Linux the
system usually raises both at open. So Linemon and a Channel's view could show
RTS or DTR low while it was high.

**Decision:**

- An output line the profile leaves unset is set low at open, in Linemon and in
  every Channel alike, so the level shown is the level set. A profile's own `rts`
  or `dtr` still wins.
- Under RTS/CTS flow control RTS belongs to the flow control. It is left as the
  driver sets it, has no button, and reads "RTS set by flow control".
- The inputs, CTS, DSR, DCD and RI, are read from the port as before.
- The lines are shown only while the port is running. A faulted port's last
  reading is not shown as live, and while no port is running a sentence says why
  the lines are greyed out.

**Boundary:** Nothing changes on Windows, which already opened with both low. On
Linux and macOS, an unset line now starts low instead of the system's usual high.

**Alternatives considered:**

- **Set both high, as most terminal programs do:** Rejected. A board that resets
  on DTR would restart at every open, and Linemon reopens at every setting
  change; some RS-485 adapters stop receiving with RTS high.
- **Leave them and say "set by the system":** Rejected. It is true but tells the
  user less than a known level.

**Consequences:** A device that waits for DTR or RTS stays silent until the line
is clicked high, and the display shows the line low, which says why. A Channel
that needs a line high from the start sets it in its profile.

**Amendment (2026-11-21):** RTS under RTS/CTS is known on Windows. The serial
library opens such a port with RTS switched on and left on (`RTS_CONTROL_ENABLE`,
not handshaking), which an adapter's RTS light confirmed by hand. So on Windows
it is reported high and reads "RTS high, set by flow control"
(`RTS_HIGH_UNDER_RTS_CTS`). On Linux and macOS the driver lowers and raises RTS
to pace the sender, so it still reads "RTS set by flow control" with no level.
Listener still never sets RTS under RTS/CTS.

**Amendment (2026-11-22):** Talker follows this decision for its own serial
channels, and all three programs name flow control None, XON/XOFF and RTS/CTS
(talker ADR-070).

## ADR-074 — Linemon and a Channel share the serial settings lines with talker

**Status:** Accepted 2026-11-23. Amends §14.1, §14.3, §50.3 and ADR-019;
talker ADR-071 records the same decision for talker.

**Context:** A Channel's editor laid out its serial settings as five rows of
radio buttons, Linemon as one line of drop-downs, and the editor offered Mark
and Space parity and 1.5 stop bits, which the serial library refuses at open.

**Decision:** Linemon and a Channel's editor use the two lines in
`wiredata-ui::serial`, as talker does (talker ADR-071): the port with its baud
rate and framing, then Flow with the control lines. Listener maps its config to
and from the shared values. Only what a port opens with is offered; a profile's
Mark, Space or 1.5 is shown as not supported and kept until another is chosen,
never changed unasked. A port the system does not list can be typed by name
(§14.4).

**Boundary:** A Channel's edits still wait for Apply & Restart and Linemon's
still apply at once. The profile keys and their accepted values are unchanged.

**Consequences:** A profile with Mark, Space or 1.5 still cannot start, as
before; the editor now says why instead of offering it as a choice.

## ADR-075 — A new view shows control characters as control pictures

**Status:** Accepted 2026-11-24. Amends §78 and ADR-060's default view.

**Context:** A new Channel's view was Raw with control characters passed
through (`Native`). The ctrl-chars row offers only ␊, [LF] and <0A>, so none
was selected. Passed through, a line feed still broke the line on screen,
in the mode meant to show control characters rather than act on them.

**Decision:** The default view is Raw / Utf8 / Glyph / NoWrap: a new Channel,
and a profile that gives no view, show control characters as control
pictures. `Native` stays a valid profile value.

**Boundary:** A profile that names a character rendering keeps it, `Native`
included, and the ctrl-chars row still has no button for it.

**Consequences:** A new Channel's Display Recording is in the same form, so a
`.disp` recorded with the default view holds ␍␊ instead of line breaks.
Rendered keeps line breaks.

## Open questions

_None open. (OQ-L1 resolved by ADR-004 above.)_
