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

**Decision:** A hybrid runtime. Tokio owns orchestration — command handling, cancellation, bounded queues, fan-out, async-native network I/O, shutdown. Continuous blocking serial receive loops run on dedicated OS threads that own the interface handle and hand data to the async side through bounded `tokio::sync::mpsc` channels (`try_send`, with a bounded retry loop while the queue is full — see ADR-007's refinement: the retry loop lets the stall notice fire mid-stall and observes cancellation). `spawn_blocking` is reserved for bounded, finite operations (open/close, file create/flush, port enumeration) — never for continuous receive loops. Transports are push-based: data-bearing transports emit `ReceivedData`, TCP listeners emit `NewConnection`.

**Contrast with `talker`:** talker deliberately uses **no** async runtime (talker ADR-002 — `std::thread` + `crossbeam-channel`), because it manages a bounded set of *outbound* senders against synchronous `serialport`/`eframe` APIs. Listener faces the opposite shape: many *inbound* network sources, async-native socket I/O, and dynamic TCP-connection fan-in — where Tokio's orchestration earns its keep. The two crates therefore reach opposite conclusions for the same reason (fit the runtime to the I/O shape), and that divergence is intentional.

**Consequences:**
- `blocking_send` backpressure can stall a serial reader → UART/driver overrun, reported as transport-specific loss (§99, §101).
- Continuous blocking loops need a bounded read timeout so they can observe cancellation; shutdown never relies on interrupting an in-progress blocking read (§111).
- Timing is chunk-granular (`ChunkTime`, §138), not true per-byte hardware timing; recording timestamps and liveness derive from it (§26, §57, §166). *(Originally "per-message timing in the extractor" — both removed by ADR-010.)*

---

## ADR-002 — Messages are immutable; decoders are read-only

> **Superseded by ADR-010 (spec v2.0):** Messages and decoders are removed. The
> surviving principles live on as §103 (received chunks are immutable, shared via
> `Arc`) and §5.4/§5.5 (display/recording never alter received bytes).

**Authoritative context:** spec Part on Messages/Decoders (§131–§135, §140; removed in v2.0).

**Decision:** A `Message` is immutable once emitted by the extractor. Decoders never mutate Messages — they only produce read-only annotations (protocol metadata, integrity metadata). Display formatting never affects what is recorded (Raw Recording is independent of rendering).

**Consequences:**
- Multiple downstream consumers (display, recording, decoding) can share Messages without coordination or copying.
- Raw recording fidelity is guaranteed regardless of decoder or display behavior.

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
  need to surface as listener state.
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
Reconnecting from "gave up / not retrying" = Faulted without depending on the
advisory reconnect events), and `snapshot`/`channel_stats` always serve for a
known channel — live when running, synthesized from the retained
diagnostics/activity otherwise. The GUI reducer reconciles its derived status
from every polled update, leaving the transitional `Starting`/`Stopping` states
to settle on their own. Pinned by
`a_dropped_lifecycle_event_self_corrects_on_the_next_poll`,
`polled_fault_with_reconnect_pending_reads_reconnecting`, and
`transitional_polled_states_do_not_flap_the_row`.

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
   backpressure that drains quickly is normal and must not notify. *(Refined: the
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
reported by runtime monitors outside that transport contract (ADR-020).

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
  state and a slow UI can never stall reception (the driver's pushes are advisory,
  `try_send`/drop-newest like the other observer edges, §99).
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

## ADR-009 — The live viewer is fed from the verbatim pre-extraction byte stream (`DisplaySource::Stream`)

**Status:** Accepted. **Context:** spec §17 (per-Channel Stream vs Message Mode),
§18 (Stream Mode: continuous bytes, no Message Numbers/timestamps, "displayed as a
stream"), §41 (`DisplaySource { Stream, Messages }` — a Display View operates on
either stream data or completed Messages), §24 (in Message Mode with delimiter
exclusion, CRLF "need not remain in the Message payload"), §53/§142 (the
pre-extraction chunk path, `write_chunk` — bytes "exactly as received"), §99.1
(non-blocking distribution order), §88 (retention is Message-**count** based);
ADR-006 (snapshots are the pull surface) and ADR-008 (the GUI↔runtime bridge).

**Problem.** The live viewer was wired only to the **Messages** display source —
extracted, numbered, decoded frames. That is the wrong source for a wire
troubleshooting view, three ways:
- **Delimiter extraction strips the terminator** (§24). For NMEA the `\r\n` that
  *defines* each sentence is consumed by the extractor, so the viewer literally
  cannot show what is on the wire.
- **Stream-Mode channels produce no Messages at all** (`StreamExtractor` emits
  nothing — the generic serial default), so their viewer is empty.
- **Chunk boundaries are arbitrary.** Non-UDP reads return "whatever the OS has,
  when it has it" — capped at a 4 KB (serial) / 8 KB (TCP) buffer, with serial also
  returning empty on a 100 ms cancellation timeout. A read can split a sentence
  anywhere; boundaries track OS buffering and timing, never content. So a Messages-
  or chunk-segmented viewer diverges between serial and UDP for the same data.

The spec already defines the right source — `DisplaySource::Stream` (§18/§41), the
verbatim pre-extraction bytes — but it was never surfaced: the chunk path (§53/§142)
fed only raw *recording*, not the viewer.

**Decision.** Feed the live viewer from the **verbatim pre-extraction byte stream**.
Maintain a bounded per-Channel byte ring, appended in `Pipeline::ingest` *before*
extraction — a second non-blocking pre-extraction tap alongside the raw recorder
(§53/§99.1), **independent of the Channel's extraction config**. Surface its tail in
`ChannelSnapshot` (ADR-006 pull surface), cloned only for the on-screen Channel. The
GUI renders the tail as one continuous, selectable view through the existing
`DisplayView` renderer: Rendered honors the data's real CR/LF (terminal semantics,
§44), Hex is a bytes-per-line dump, Raw shows control pictures. Concatenating chunks
in arrival order reconstructs the exact wire stream regardless of read-chunk
boundaries — the reassembly an extractor must do across buffers (§21) is unnecessary
here because nothing is reframed. This makes serial and UDP behave identically and
gives **every** Channel a working viewer.

The work is phased. Phase 1: the verbatim Stream viewer becomes the default — and,
for now, only — viewer. Phase 2: the per-view `DisplaySource` **Stream ↔ Messages**
switch (§41), which restores the Messages-source view (decoded, numbered,
`[type]`-tagged, match-highlighted). Until then the Messages machinery keeps running
for decoding, Match Rules, diagnostics, and message-framed recording — it is simply
not rendered.

**Why not the alternatives.**
- *Render from extracted Messages (the prior direction).* Loses the delimiters under
  delimiter extraction (§24), shows nothing under Stream extraction, and segments on
  arbitrary frame/chunk boundaries — none of which is the wire.
- *Concatenate the Messages display history into a pseudo-stream (an interim hack).*
  Same source defect: CRLF-stripped frames run together with no breaks. Wrong layer.
- *Reuse the raw-recording path.* That writes to disk; the viewer needs an in-memory,
  bounded, snapshot-cloneable tail. Same data, different sink.

**Consequences.**
- A new bounded byte ring per Channel, capped by **bytes** (~128 KB default) — there
  are no Message boundaries to count, so §88's count-based retention does not apply.
  Trivial CPU/memory; always-on even for Stream-Mode channels.
- Stream Mode has no Message Numbers or timestamps (§18): the Stream viewer drops the
  per-message prefix (timestamp / # / `[type]`), and "Recent messages (N)" becomes a
  byte count. Match-rule highlighting (message-number-keyed, §50.2/§165) and
  Display-View pause move to the deferred Messages view.
- The snapshot grows by the tail clone (≤ cap), polled at the ADR-006 cadence (5 Hz)
  for the selected Channel only. _(Superseded by ADR-011: at the eventual ~1 MB cap
  this 5 Hz full-buffer clone was **not** negligible — it is replaced by incremental
  `StreamDelta` fetches, and the snapshot no longer carries the tail.)_
- The recently built Messages-based viewer (line-virtualized per-message log) is set
  aside, to return behind the §41 source switch.

**Build order (small, reversible first):** (1) byte ring + `ChannelSnapshot` field +
GUI render + revert the interim hack [this step]; (2) the per-view
`DisplaySource::Stream/Messages` switch (§41), restoring the Messages view; (3)
Stream-view pause and optional byte/line gutters.

> **Superseded by ADR-010 (spec v2.0).** ADR-009 phases 1–2 shipped, but v2.0 then
> removed the Message half entirely, so the phase-2 "Messages source" is gone. The
> Stream viewer ADR-009 introduced is now the *only* viewer.

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
- `core::RuntimeCommand` and its `pub use` are gone; `DisplayViewId` is no longer imported by `core::command` (only `RuntimeEvent`'s `MatchRuleId` remains). No functional change — nothing referenced the enum (160 lib + 6 profile + 7 integration tests, clippy `-D warnings`, fmt all unchanged-green after removal).
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

## ADR-019 — GUI chrome moves to the shared `wiredata-ui` crate

**Status:** Accepted. **Context:** the GUI merge — talker adopts listener's look and feel, and both apps will eventually offer the same light/dark themes. Counterpart of talker ADR-016, which holds the full rationale and the scope rule.

**Decision.** Listener's GUI **chrome** — the bundled font stack (`gui/fonts.rs`), the named color palette (`gui/theme.rs`), the base widget visuals and style tweaks (the body of `apply_style`), and `widgets::format::human_bytes` — moved verbatim into the new internal workspace crate **`wiredata-ui`** (egui-only, `publish = false`). The listener modules remain as thin re-exports, so every call site is unchanged. The palette carries both `LIGHT` and `DARK` instances — several light values (e.g. the then-named `WARNING_AMBER`/`INFO_GREY` consts, now `Palette` fields `warning_amber`/`info_grey`) are unreadable on a dark backdrop. *(Follow-up, same series:)* listener since gained the theme toggle: `gui/theme.rs` became theme-aware — same names as small functions that read a UI-thread-written mirror of the active egui theme (set at startup restore and by the ◐ header button; persisted under the same `dark_mode` storage key talker uses), picking `LIGHT` or `DARK` live. The pure color helpers (`status_color` etc.) stay argument-free that way. Stream-view content colors remain user-chosen per view (`ColorScheme` offers both light and dark schemes) — deliberately not coupled to the chrome theme.

**Consequences.**
- `listener/assets/fonts/` moved to `wiredata-ui/assets/fonts/` (README and licenses included); the font rationale doc lives there now.
- Anything app-specific stays put: view-models, widgets with runtime knowledge, the stream-view `ColorScheme` (user-chosen content colors are not chrome).
- A change to the shared look lands in both apps by construction — the drift risk that motivated the crate is gone.

**Follow-up (2026-07-16, chrome dedup — counterpart of the talker ADR-016 follow-up).**
The transitional re-export shims (`gui/fonts.rs`, `gui/widgets/format.rs`) and the
`gui/theme.rs` global theme mirror were retired: call sites import `wiredata_ui`
paths directly, `wiredata_ui::palette::active(ui)` replaces the atomic mirror (the
pure helpers `status_color` / `recording_indicator` now take `&Palette`, keeping
them unit-testable without a `Ui`), and the header theme toggle is the shared
`wiredata_ui::style::theme_toggle_button`.

**Follow-up (selected-channel continuity).** `wiredata-ui::selection` now owns the
selected/full and compact channel tabs, shared row emphasis, and the complete
tab-to-page edge. The apps supply the selected card rectangle and
its real scroll viewport. The shared painter mirrors egui's resizer hover/drag stroke
and detours around a tab only when the whole card and both rounded turns are visible;
a clipped selection gets a straight page divider. Historical warning/error counts
recede identically on background tabs, while app-owned lifecycle glyphs and live
faults remain saturated. This is chrome, not channel state, so both apps use one
implementation and one set of geometry tests.

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

**Consequences.** Reception throughput and cancellation behavior are unchanged:
only a post-reception monitor may wait. A full advisory queue can delay fault
finalization but cannot erase its reason; a pipeline that has already been forcibly
removed simply closes the receiver and releases the send. Pinned by
`terminal_fault_waits_for_space_in_a_full_notice_queue` plus the end-to-end
`spontaneous_transport_fault_emits_channel_faulted` diagnostic assertion.

## ADR-021 — Channel transport is chosen at creation

**Status:** Accepted 2026-07-15. **Context:** the existing Add-template GUI model and
its adoption by talker in talker ADR-027.

**Decision:** Keep transport kind structural in the GUI. `+ Add` chooses the UDP, TCP,
or Serial template, in that shared order. Configure Connection edits the selected
template's parameters and never replaces its `InterfaceConfig` variant. Runtime-created
TCP Connection Channels remain governed by their TCP Listener parent.

**Consequences:** This records existing listener behavior rather than adding a new
runtime path. Both apps now present one ordinary transport choice and the same Add-menu
order. Reconfiguration, profiles, and the schema are unchanged. A focused GUI test
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

**Status:** Accepted 2026-07-16 (spec v2.1). **Context:** spec §48 promised multiple
simultaneous Display Views per Channel, but the runtime and GUI have only ever used
the first view: scrollback pause gating keyed on `display_views.first()`, and the
detail pane surfaced one view. A non-primary view could be *marked* paused but had no
per-view stream state to freeze — the abstraction cost complexity (a `Vec` of views
threaded through the pipeline, snapshots, and config) without delivering the feature.

**Decision:** v2.1 makes one **logical** Display View per Channel normative, with
Raw / Rendered / Hex as that view's switchable modes — matching talker's single
Output pane, so the two apps present the same display model. The profile schema's
`views` list stays (additive forward-compatibility); entries beyond the first are
ignored. Multiple simultaneous views are deferred to Appendix A: reviving them
requires per-view render/pause state, not just config plumbing.

**Consequences:** "The view is paused" and "the Channel's display is paused" are the
same statement, closing the pause ambiguity. Internal `Vec` shapes may simplify
opportunistically; no behavior changes now (the code already lived this way).

## ADR-024 — TCP Connection-Channel surfacing is deferred scope

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

## ADR-025 — NMEA construction returns for presentation-only ZDA Marks

**Status:** Accepted 2026-07-17 (spec v2.2).

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
portable userland receiver can know.

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

**Status:** Accepted 2026-07-19.

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

The implementation remains local to Listener. Talker uses the same deliberately small
algorithm and matching boundary tests, but `wiredata-ui` is presentation-only and
neither existing core crate is a valid owner for runtime telemetry. A new shared crate
for this one helper would add more architecture than reuse; extraction can be revisited
if additional non-GUI consumers emerge.

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

## ADR-030 — Idle rules wait on exact deadlines with bounded Windows precision

**Status:** Accepted 2026-07-19 (spec v2.2.1).

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
recent timing, Idle timer policy, transport health, ingest/Raw-recorder queue peaks,
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
measurement policy local: `ByteHistogram`, `ChunkShape`, transport/timer summaries,
pipeline boundaries, stats/snapshots, completed-run retention, and GUI presentation.
The separate `wiredata-timing` crate continues to own only process timer-resolution
mechanics.

**Consequences:** Listener and Talker share one tested implementation of the numeric
primitive without sharing runtime architecture or telemetry schemas. Hot-path memory,
allocation, measurement boundaries, retained summaries, profiles, and received bytes
are unchanged. This decision supersedes ADR-027's local-implementation paragraph;
its window semantics and all other consequences remain in force.

## ADR-033 — Diagnostics lead with decisions without overstating receive health

**Status:** Accepted 2026-07-20.

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
The card badge distinguishes live monitoring, retained last-run evidence, and a
channel that has not produced run telemetry; it does not turn absent evidence into a
healthy state.

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
describing the channel's configured endpoint.

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
  count chunks; Idle-rule lateness counts firings. The row that reported firings
  as an unlabelled integer now names them.
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

## ADR-038 — Hex grouping is rendering; line length is layout

**Status:** Accepted 2026-08-05.

**Context:** `HexGrouping { bytes_per_group, groups_per_line }` (§45) was
specified, persisted, `#[serde(default)]`-ed, and pinned by a profile round-trip
test — and read by nothing. Both places that built a `DisplayView` hardcoded
`hex_bytes_per_line: 16`, and no GUI control set the field, so a profile could
express a grouping that the viewer and the `.disp` both ignored.

The obvious fix — map the config onto the existing `hex_bytes_per_line` — does
not deliver what §45 asks for. `bytes_per_group` means bytes *run together*
between separators (`41424344` at four), which the renderer could not express:
it emitted one separator between every cell. Line length alone would have left
the setting half-implemented in a way no readout would reveal.

**Decision:** Split the two halves by where they belong.

- **`bytes_per_group` is rendering.** `DisplayView` gains
  `hex_bytes_per_group`; `render_hex` runs that many bytes together and
  separates only groups. It therefore reaches the `.disp` — the recorded stream
  is the rendered stream, and the spacing is part of what was rendered.
- **`groups_per_line` is layout.** It sets the viewer's line length and never
  reaches a recording: ADR-018 holds that `.disp` is never hard-wrapped, so
  `build_display_view` passes `hex_bytes_per_line: 0` unconditionally while the
  GUI passes a real length. The flag is the mechanism, not the code path — a
  `0` means "do not wrap", which is what the recorder's view always carries.
- **`groups_per_line: 0` stays "fit to the display width" (§45)** — a question
  only a viewer that knows its own width can answer, so the renderer is told not
  to wrap and the pane wraps what it gets.

Two rules keep the output stable under things the reader did not choose:

- **A group never breaks at a read boundary.** The `HexCursor` carries group
  position across chunks alongside line position, so a transport's read sizes
  are not observable in the spacing (`hex_grouping_is_chunking_invariant`).
- **An annotation stands apart and resets the group.** Running a Mark into the
  bytes on either side (`4142‹T›4344`) would hide both, and letting it count as
  a group member would shift every column below it.

The line wrap is done by the renderer rather than by splitting rendered text at
a computed column count. The arithmetic works out to the same width, but the
splitter cannot know that the character at the boundary is a separator, so it
stranded one at the head of every continuation line.

*Amended 2026-08-05 (ADR-040): as first written this held only for a configured
group count. Fit-to-width — the default — still passed a zero line length and
let the character splitter cut the row, so the guarantee above described half the
feature. ADR-040 resolves fit-to-width to a byte count before rendering, which
makes the sentence true for both paths.*

**Consequences:** A GUI control under Configure display sets both halves, in the
units §45 and the schema use (bytes per group, groups per line) with the
resulting bytes-per-line shown alongside so the reader does not multiply. The
setting persists through `ViewPrefs` like the other view presentation fields —
the field already round-tripped through TOML, so what this closes is the write
side. A zero `bytes_per_group` resolves to 1 everywhere rather than erroring:
no group can be empty, and the config has no way to mean otherwise.

**Found while testing:** the row cache spliced a Mark twice when its byte landed
exactly on a delta boundary. `rebuild_rows` admitted a mark at one *past* the
window end — a `Before` whose byte had not arrived — so it rendered ahead of its
byte and again when that byte turned up in the next delta. The bound is now
half-open, matching `delta_annotations`, and pinned across all three modes and
four chunk sizes by `a_mark_on_a_delta_boundary_splices_once`.

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

## ADR-041 — A Hex line is bounded by columns, not by cells

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

## ADR-042 — In Hex, a Mark takes a row of its own

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

## ADR-043 — A recording continues across faults as numbered segments with recorded gaps

**Status:** Accepted 2026-09-30. Amends §56.1, §56.2 and §59.

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
  timeout the same way. Gaps go to the event log (ADR-044) and diagnostics only;
  there is no separate gaps file.
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

**Status:** Accepted 2026-09-30. Amends §118.

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
- Log files older than 30 days are deleted at start and at each day's rollover.
  Only log files are deleted, never recordings (ADR-043).
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
Appendix A. Disk use is bounded by the 30-day retention.

## ADR-045 — Unattended GUI: reconnect is an explicit choice, and resume is registered once

**Status:** Accepted 2026-09-30. Amends §9.1 presentation, §70 and §159.

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

## ADR-047 — The TCP Listener is disabled; UDP states its bind scope and can request a shared port

**Status:** Accepted 2026-09-30. Amends §1, §16, §68, §73, §76, §85, §153, §154,
§162 and Appendix A; builds on ADR-024.

**Context:** A TCP Listener accepts connections, but their data cannot be
displayed or recorded (ADR-024). Offering it invites the belief that data is
being captured. It also lacks what a shared network needs: its template binds
all interfaces with no connection cap, there is no keepalive, and any accept
error faults the Channel. For UDP, a second program cannot bind the same
broadcast or multicast port, which users sometimes need, and "0.0.0.0" does not
tell a reader that the socket is reachable from the network.

**Decision:**

- **The TCP Listener is disabled.** It is gone from `+ Add` and `--tcp`, and
  profile validation rejects it: "TCP Listener isn't available in this release:
  received data can't be displayed or recorded yet." The code stays. When it
  returns, alongside ADR-024's surfacing, it needs a default connection cap,
  keepalive with idle, interval and probe count set for each OS, and
  per-connection handling of accept errors with rate-limited logging.
- **Bind scope in words.** The UDP editor lists local addresses, and labels the
  all-interfaces choice "All interfaces (reachable from the network)". The
  Channel header shows the scope.
- **"Request shared port"** is a per-Channel UDP option, off by default and
  offered for broadcast and multicast only. It sets the platform's address-reuse
  option before binding, and status reports whether the OS applied it.

**Boundary:** This targets shared networks, not untrusted ones: there is no
source allow-list. Sharing is not offered for unicast, where the OS delivers
each datagram to only one of the sharing sockets.

**Alternatives considered:**

- **Hide the TCP Listener but keep loading it:** Rejected. Nothing is deployed,
  so there are no profiles to protect, and a loadable but hidden kind is harder
  to explain than an absent one.
- **Build the connection view now:** Out of scope for this remediation.

**Consequences:** Every Channel kind offered can display and record what it
receives. A UDP Channel says in words who can reach it.

## ADR-048 — Profiles load strictly and runtime capacities are bounded

**Status:** Accepted 2026-09-30. Amends §71, §72.1, §80 and §88.

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
  | Reconnect backoff | initial ≤ max, multiplier 1.0–10 |
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

## ADR-049 — OS stop requests come from one shared crate, `wiredata-stop`

**Status:** Accepted 2026-09-30. Implements the stop signals of §3.1 and talker
ADR-060.

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

**Boundary:** The CLI keeps awaiting. An interface open still cannot be cancelled,
so a Stop waits for the open to land. At exit the driver lands opens and drains
in flight, within the stop grace, before stopping what is live.

**Alternatives considered:**

- **Refuse a command for a Channel mid-step:** Rejected. A Stop sent during a
  10 s open would have to be sent again.
- **Cancel the open:** Not possible for a blocking serial open.

**Consequences:** The status keeps updating while any Channel starts or stops,
and a slow Channel delays only its own commands.

## Open questions

_None open. (OQ-L1 resolved by ADR-004 above.)_
