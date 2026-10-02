# Talker — Program Specification
**Version:** 2.5.2
**Language:** Rust
**Target Platforms:** Windows, macOS, Linux

Revision note (2026-10-01) — a TCP write that timed out:

- **§4.4 / §4.5 possibly partial (ADR-059, amended)** — on Windows, a TCP write
  that times out counts as possibly partial: Windows reports nothing sent, but
  part of the message may have gone. Its error says the peer took no data for
  5 s, in place of Windows' text about a failed connection attempt.

Where a requirement leads the implementation, [TODO.md](TODO.md) lists what is
not yet built.

Earlier revisions are in [REVISIONS.md](REVISIONS.md). They live there rather
than here for two reasons: a document's version number belongs only in its own
header (AGENTS.md §2), and a reader opening a specification should reach §1
without first crossing every change it has ever had.

---

## 1. Purpose and Goals

`talker` is a production-quality utility for sending byte-oriented data from a host computer to external devices via serial (RS-232) or network connections. Its primary use case is testing and validating receiving devices. It is designed for a small technical team with the intention of broader distribution if the tool proves successful.

---

## 2. Architecture

### 2.1 Crate Structure

`wiredata` is a Cargo workspace with six crates:

- **`talker`** — the binary crate containing the CLI, GUI, and all application logic
- **`nmea0183`** — a standalone library crate containing all NMEA 0183 support, with no dependency on `talker`
- **`listener`** — the receive-side stream counterpart to `talker`, with its own spec, ADR, and TODO
- **`wiredata-ui`** — internal shared GUI chrome for the two apps (fonts, color palette, base widget style, formatting helpers); egui-only, never published (talker ADR-016 / listener ADR-019)
- **`wiredata-timing`** — internal shared OS timing mechanics: the refcounted
  Windows 1 ms timer-resolution guard and minimized-window throttling opt-out;
  cadence, scheduling, and timer policy remain in the applications (ADR-038)
- **`wiredata-telemetry`** — internal dependency-free bounded telemetry primitives:
  fixed duration-histogram buckets and the ten-segment recent-window engine;
  application aggregates, measurement boundaries, policy, retention, and
  presentation remain in Talker and Listener (ADR-039 / listener ADR-032)

The application/library crates keep their own `docs/` folders (spec, ADR, TODO).
The three small internal shared crates have no docs folder; their scope decisions live
in the app ADRs. There is no workspace-root docs directory.

```
wiredata/                        # workspace root
├── Cargo.toml                   # workspace manifest
│
├── talker/                      # binary crate
│   ├── Cargo.toml
│   ├── docs/
│   │   ├── ADR.md              # workspace- and talker-level decisions
│   │   ├── TODO.md
│   │   └── talker_specification.md
│   ├── src/
│   │   ├── main.rs              # entry point; dispatches to CLI or GUI
│   │   ├── cli/                 # CLI interface module
│   │   ├── gui/                 # egui GUI module
│   │   └── core/                # all core logic
│   │       ├── channel/         # serial, UDP, TCP abstractions; channel collection
│   │       ├── message/         # message formats, encoding, timestamp, checksum
│   │       ├── scheduler/       # priority-queue send loop
│   │       ├── profile/         # profile management; schema v3 clean break
│   │       └── logging/         # logging subsystem
│   └── tests/                   # integration tests (Rust convention)
│
├── nmea0183/                    # library crate (publishable independently)
│   ├── Cargo.toml
│   ├── docs/
│   │   ├── ADR.md              # nmea0183-specific decisions (ADR-009, OQ-4)
│   │   ├── TODO.md
│   │   └── nmea0183_specification.md
│   ├── src/
│   │   ├── lib.rs               # public API surface
│   │   ├── sentence/            # sentence types, construction, parsing
│   │   ├── talker_id.rs         # talker ID enum and custom variant
│   │   ├── checksum.rs          # XOR checksum computation and verification
│   │   └── proprietary/         # $PRDID, $PASHR, and arbitrary $P builder
│   └── tests/                   # integration tests for nmea0183
│
├── listener/                    # receive-side stream utility crate
│   ├── Cargo.toml
│   ├── docs/
│   │   ├── ADR.md              # listener decisions (own ADR numbering)
│   │   ├── TODO.md
│   │   └── listener_specification.md
│   └── src/
│
├── wiredata-ui/                 # internal shared egui chrome
│   ├── Cargo.toml
│   └── src/
│
├── wiredata-timing/             # internal shared OS timing mechanics
│   ├── Cargo.toml
│   └── src/
│
└── wiredata-telemetry/          # internal shared bounded telemetry primitives
    ├── Cargo.toml
    └── src/
```

### 2.2 Interface Separation

The CLI and GUI are thin interface layers only. All logic — channel management, message construction, scheduling, encoding, profile handling, logging — lives in `core`. Neither interface layer contains business logic.

### 2.3 GUI Framework

The GUI is built with **egui** (via `eframe`). The aesthetic is utilitarian and functional; not a consumer-style app. Clarity and density of information are prioritized over visual polish.

### 2.4 Multithreading Architecture

`talker` uses OS threads (`std::thread`) with `crossbeam` channels for inter-thread communication. Tokio or other async runtimes are not used; the workload (a bounded number of channels, synchronous serial I/O, a single egui UI thread) does not benefit from async and would be complicated by it. Note that "no async runtime" and "multiple OS threads" are independent concepts — `talker` uses multiple OS threads throughout.

#### Multi-Channel as a First-Class Design Goal

Sending simultaneously on multiple channels is a primary use case, not a future option. A user may need to simultaneously drive a GPS simulator on a serial port, a depth sounder on UDP, and an ADCP on a second serial port — all from one `talker` instance with one GUI.

The correct model is one application instance, one GUI, and one talker thread per active channel. `core::channel` manages a *collection* of channels from the initial implementation.

#### Thread Model

| Thread | Responsibility |
|--------|----------------|
| **UI thread** | Runs the egui/eframe event loop; handles all user interaction; never blocks |
| **Talker thread (one per active channel)** | Runs the priority-queue scheduler for that channel; owns the interface handle; handles open/close/reopen |
| **Logger thread** | Receives log messages via channel; writes to file and/or stdout without blocking talker threads |

#### GUI Multi-Channel Layout (master–detail)

The GUI uses a **master–detail** layout:

- **Channel list** (left, resizable, collapsible to a thin status strip): one row per
  channel showing a status glyph (running / fault / stopped), the channel's display
  name (or the positional "Channel N" fallback), a one-line interface summary with
  unfilled fields flagged, the running send count with a msgs/s estimate, and the most
  recent error. The list header holds `+ Add` (per interface kind: UDP / TCP / Serial)
  and Start all / Stop all.
- **Detail pane** (centre): everything about the **selected** channel — a header with
  the editable name, interface summary, drift badge, and per-channel actions (start /
  stop / restart / remove); the Connection editor for the transport selected at
  creation; the Messages editor (one or more messages, each independently configured);
  and the real-time outbound display pane (configurable view — see Section 5.7).

Channels can be added, renamed, reconfigured, removed, started, and stopped
independently at runtime. `+ Add` chooses a channel's transport kind; normal
configuration edits only that kind's parameters, matching listener's model. Channel
names are cosmetic (channels are positional); empty names fall back to "Channel N",
and duplicate names are allowed but flagged inline.

#### Communication

All cross-thread communication uses `crossbeam` channels. Each talker thread has its own dedicated channel pair with the UI:

- **UI → Talker (per channel):** commands (start, stop, parameter change, profile load, message interval update)
- **Talker → UI (per channel):** status updates (channel state, bytes sent, errors, display data)
- **Any thread → Logger:** log messages (level + text)

#### Design Rules

- The UI thread never performs I/O and never blocks.
- Each talker thread exclusively owns its interface handle; handles are never shared across threads.
- Shared configuration is passed by value through channels, not via shared memory or `Mutex` where avoidable.
- The number of simultaneous channels is bounded by available system resources (serial ports, network sockets), not by any artificial limit in the software.

---

## 3. Interfaces

### 3.1 CLI

The CLI uses structured argument parsing (`clap`). All features available in the GUI are also available from the CLI, except window management, layout state, and the data display pane (which are inherently GUI concepts). This includes:

- Selecting channel type and parameters
- Selecting message format, encoding, and data
- Loading profiles (single or multi-channel)
- Enabling stdout echo
- Selecting log destinations (stdout, file, or both)
- Real-time parameter adjustment is not applicable in CLI mode; parameters are set at launch

A `--gui` flag launches the GUI from the CLI.

#### Multi-Channel in CLI Mode

CLI mode supports multiple simultaneous channels identically to GUI mode — one `talker` process, one talker thread per channel, all running in parallel. The primary way to launch multiple channels from the CLI is via a multi-channel profile:

```
talker --profile full_bridge_sim
```

This loads the profile, spawns a talker thread for each channel defined in it, and runs until interrupted. *(Planned)* Single-channel ad-hoc invocation without a profile — a fast path for simple cases — is not yet implemented; today the CLI requires `--profile` / `--profile-path`.

#### Unattended Operation (ADR-060)

The CLI must survive running headless, under systemd or Task Scheduler, where
nobody watches the terminal:

- **Start what can start.** A channel whose interface fails to open is retried
  with backoff, 1 s doubling to 30 s, while the others send. `--require-all`
  instead stops everything and exits when any channel fails to open.
- **Warn loudly.** A WARNING banner on stderr, not hidden by `--quiet`, names each
  channel that did not open and why, using the word rather than colour. A reminder
  repeats every 5 minutes while any channel is down, giving how long it has been
  down and the latest reason. A line reports each recovery.
- **Final summary.** At exit the CLI prints each channel's outcome, its final send
  counters (§4.4), and the number of `--echo` lines dropped (§5.8).
- **Stop signals.** Ctrl-C, SIGTERM on Linux, and console close, logoff and
  shutdown on Windows all stop every channel gracefully, bounded by a shutdown
  time limit.
- **Exit codes:**

  | Code | Meaning |
  |---|---|
  | 0 | Healthy, or every outage recovered (the summary lists them) |
  | 1 | Internal error |
  | 2 | Invalid profile, nothing could start, or `--require-all` failed |
  | 3 | Degraded: a channel never started, its retries ran out, or it was down at shutdown |
  | 4 | Shutdown did not finish within its time limit |

The CLI does not install itself as a service; `deploy/` at the workspace root
holds example definitions.

#### Profile Compatibility

Channel and message configuration is fully compatible between CLI and GUI. A
profile saved from the GUI loads correctly in the CLI and vice versa. A profile
written by hand in a text editor (valid TOML matching the profile schema) works
in both.

The profile's `[logging]` table is CLI launch policy. GUI **Detail**, **Show in
pane**, and **Log file** controls are session-local: loading a profile neither
changes GUI collection or display nor enables file logging. Persisted GUI state
is described below and remains separate from the profile.

**Example invocation sketch** (illustrative, not final):
```
talker --profile gps_sim
talker --profile full_bridge_sim
talker --gui
talker --channel serial --port COM3 --baud 9600 --format nmea0183
```

### 3.2 GUI

The GUI is built with egui/eframe and supports:

- Standard window operations: resize, minimize, maximize
- All core functionality exposed via controls
- Real-time status display (channel state, data sent, errors)
- Profile save/load/switch

#### Master–detail layout

One channel is on screen at a time. The collapsible **channel list** (left) shows
per-row: status glyph + name, the one-line interface summary (unfilled fields as
red `?` pills), `Sent: N · msg/s` while running, per-severity log counts
(info · warn · err, since the channel's last start), and any current unresolved
fault. DEBUG and TRACE entries never inflate the INFO count. Rows carry a ✕
remove overlay; `+ Add` (per transport kind), Start all / Stop all, and the
**Profile menu** (Recent / Save / Save As… / Load… / New; renaming = Save As…) live
in the list header. Collapsed, the list becomes a mini-strip of status glyphs.

The **detail pane** (right) shows the selected channel:

- **Header** — status glyph + editable name (duplicates allowed but hinted);
  a `status · interface summary` row; then the readouts, grouped by subsystem:
  - *Send outcomes:* `Send outcomes: <scheduled> scheduled - <f> failed -
    <p> possibly partial - <s> suppressed - <m> missed = <sent> sent`, shown
    **always** and directly
    beneath the `status · interface` row — amber when any deduction is nonzero,
    red when any write failed. The line is stated as visible arithmetic over the
    schedule's own cadence points, so the successful remainder is defined by the
    equation rather than by a noun asserting something the application cannot
    observe. One tooltip defines each term and where in the send path its
    deduction happened (§8.1). These are totals for the current run and remain
    after sending recovers and after Stop. The outcomes are rendered in exactly
    one place: the diagnostics card neither repeats them nor raises a separate
    unsent callout, though their tone still escalates its badge.
  - *Sent:* `Sent: <total> total · <byte rate> · <msg/s> (~5 s)` — the cumulative
    byte total, retained after Stop, beside the rolling five-second rates, which
    decay to `0.0` at rest while the total stands still. Byte values scale by SI
    unit (kB → MB → GB) rather than carrying digit separators.
  - **Sent** throughout the GUI means the configured-interface write returned
    success. It never asserts that bytes reached the wire or that a peer received
    them; the tooltips carry that boundary. The aggregate `unsent`
    (`failed + possibly partial + suppressed + missed`) remains in the
    completed-run summary and in
    `RunSummary::unsent_sends`, where a single shortfall number is still useful.
  - *Capacity:* aggregate `msg/s` and wire `B/s` **for the configuration that is
    actually sending** — each message's wire size and interval as reported by the
    running schedule, and for Serial the framing and baud the runner confirmed
    open (ADR-035, amended). A channel that is not running has no such
    configuration, so it is projected from the settings shown and labelled a
    projection; that preflight is the feature's original purpose and is retained.
    Unapplied edits therefore no longer make this row describe a schedule that is
    not running; for Serial it reports UART line utilization and headroom against
    that configuration. A second line shows measured application
    headroom after warm-up from eligible timing covering the last ~10 seconds or
    from cumulative run-wide timing. Recent timing whose update is ten seconds
    old cannot supply this estimate; a warmed run-wide fallback is labelled
    explicitly. Low margin and physical serial oversubscription are amber.
  - *Cadence:* leads with the schedule — how many messages are sending and at
    which intervals, grouped by distinct interval (`3 messages: 2 at 50 ms, 1 at
    15 s`) and summarized past three groups. A lateness figure never appears
    without that schedule beside it, because the measurement pools every active
    message's deadlines and reads as one message's behaviour otherwise. The main
    figure is `worst <duration> behind schedule`; its percentage of the shortest
    interval and a distinct `99% within <duration>` follow together in one
    parenthesis, separated as distinct statistics, so the worst figure does not
    move as a percentile appears or disappears. `last ~10 s` states that the
    figure describes current behaviour; update age says when that period ended.
    The tooltip pairs its sample count with whichever period supplied the
    displayed figure: current, final-before-stop, or the whole run for a labelled
    fallback. There is **no warm-up state** (ADR-046): one measured form serves
    every sample count. Interval detail comes from the running schedule's own
    per-message intervals once the channel has reported any, and from the
    on-screen draft otherwise — never both in one render.
  - *Missed-send routing (ADR-045, amended by ADR-051):* when any scheduled send
    has been skipped, one callout says where to look rather than restating the
    count — a **live** interface fault to fix first, then a serial line that
    cannot carry the schedule, then a message whose send was in progress when
    misses occurred, then a message whose sends delayed the others, then
    estimated application headroom. With only one active message, it instead
    directs the technician to compare that message's longest send call with its
    interval or consider a late channel wake-up. An earlier failed-send total is
    only an uncorrelated historical clue and follows those stronger findings.
    Other unmatched cases direct them to the render and send-call timing. Only
    the observed-overlap branch states an
    amount: it is recorded when each miss happens and does not depend on the
    delayed send later starting. A message offered with “check message #…” is a
    weaker, delay-based lead rather than proof of those misses. A live failure is
    an action priority, not proof that it caused the misses; an impossible serial
    schedule and an observed send overlap are direct findings. Earlier failure
    counts may describe a problem that has cleared, and application headroom
    remains guidance rather than proof.

    The amount-stating branch closes its arithmetic with the total for which
    Talker was waiting for another message's serial or network send to finish,
    the largest single share, and the remainder. For the remainder, the callout
    says no serial or network send was recorded as being in progress. It never
    calls the channel idle: a late wake, other work, and a genuinely free thread
    all leave the same gap in this measurement. A message is not counted against
    its own missed points; a send lasting longer than that message's interval is
    compared through **Send call** instead.

    **Timing & runtime details** presents those limits as four labelled points,
    using the same message, interval, send-call, failure, and capacity vocabulary
    as the screen. The retained-history sizing that proves an eligible send was
    not merely forgotten remains in ADR-051, not in the technician-facing help.
    The callout carries a **Dismiss** button, on the rule in §3.2 *Dismissible
    warnings* below. It is advice rather than a count: the misses stay on the
    send-outcomes line and keep the card's badge raised, so dismissing hides
    where to look and never what happened.
  - *Per-message timing (ADR-045, amended by ADR-051):* a collapsed table, one
    row per message numbered as in the Messages editor, with seven columns —
    **Msg**, **Interval**, **Sends**, **Late**, **Send call**, **Longest
    block**, and **Cost to others**. The last two are distinct quantities:
    longest block is the longest single send of that message which delayed
    another, and is the only elapsed hold among them. Cost to others carries the
    two currencies a message can spend on the rest, each with its noun attached
    (`430 ms late · 37 missed`): the waiting it imposed summed across every
    message displaced — which can exceed the send that caused it and must never
    be presented as a duration the channel was held — and the cadence points
    others lost outright while it was sending. Neither converts into the other.
    All three are attributed by matching each deadline and skipped point to the
    send that was holding the channel when it passed. Because a channel handles
    its messages one at a time, the message
    recording the lateness and the message causing it are routinely different
    rows; the table exists so that comparison is a single read across a row. An
    interval that changed mid-run is marked, because the timing beside it is
    cumulative and therefore spans more than one cadence. There is deliberately
    **no render column**: payload construction is a clock read, an allocation and
    a memcpy — typically 1–3 µs, below the 50 µs first bucket of the histogram
    that would report it — so it stays in the clipboard report and the
    channel-wide work line rather than taking a column that reads the same
    forever. A **victim-side** miss count is still not offered at any
    granularity: skips accrue as `late / interval + 1`, so they concentrate on
    the shortest interval and would name the message that suffered. The count
    that is offered belongs to the message whose send was in progress when each
    point was skipped, which is why it stays attributable under the heavy
    overload that starves the delay figures beside it (ADR-051). Per-message
    histograms are cumulative-only; the rolling window stays channel-wide.
  - *Work per send:* the two stages of the work itself — render and the
    synchronous send call — over the approximate last ten seconds, beside the
    cumulative run maximum lateness. Deadline lateness is deliberately **absent**:
    the Cadence row renders the same recent measurement with the schedule context
    that makes it readable, so repeating it here was one fact in two places. This
    line is the only channel-wide view of the two stages *as they are now*, since
    the per-message table is cumulative — which makes it the "is it Talker or the
    link" check. The boundaries read `longest render` and `longest send call`;
    where a percentile differs it follows the longest observation rather than
    moving it. Timing with no capture reads `awaiting timing data`; usable timing
    names `last ~10 s` and adds update age after one second; stale timing reads
    `recent timing unavailable`; and a clean run end retains `final ~10 s before
    stop`. Capacity, Cadence, and this line use the same classification. The timer
    readout names the active wait policy, shortest active interval, cadence
    alignment, wall-clock re-alignment count, and any Windows 1 ms request failure
    (ADR-032 through ADR-042, ADR-047).
  - *Sample counts are stated once with their context.* The Cadence line puts its
    count in the tooltip beside the current, final, or run-wide period it
    qualifies. A readout that already names a count — the per-message table's
    Sends column, or the work line's own total — does not repeat it beside every
    figure. A boundary states its own count only where its population differs,
    which is exactly where it carries information: lateness is sampled before
    retry backoff can withhold a scheduled send, and the send call is timed for
    writes that failed, so a gap is evidence rather than noise.
- **Dismissible warnings** (ADR-052) — the missed-send routing callout and the
  Output pane's dropped-update warning each carry a **Dismiss** button. Both
  stand on a per-run counter that only grows, so dismissal records that counter
  and the warning returns when it is **exceeded**: the reader is told once per
  occurrence, and a fault still spreading is never silent. A counter below the
  recorded value can only mean a new run, which discards the record rather than
  reading a fresh fault against a number from a previous run. Dismissal is
  presentation state — process-local, per channel, never saved to a profile, and
  it changes nothing that is counted or logged. No other callout is dismissible:
  a live interface fault, an impossible serial schedule, and a failed timer
  request each describe a condition that is still true while it is on screen.
- **Lifecycle buttons** — the labeled pair (shared with listener):
  [Start Channel / Apply & Restart / Retry Channel] + [Stop Channel]; the Start
  side's label and enabled state derive from run state, drift, and draft
  validity (a disabled Start's tooltip lists the exact blockers).
- **Last completed run** — one collapsed process-local summary retained for the
  channel's newest completed runner. Its heading shows elapsed, sent, and unsent;
  expansion shows wall-clock start/finish, exact final outcomes, cumulative timing,
  final timer/cadence policy, and build/platform facts. **Copy summary** places the
  versioned line-oriented report on the clipboard; formatting is performed only on
  click. The report carries one flat `per_message_<name>=` line per fact —
  interval, lateness p99/max, render p99, send p99, `blocked_others_us`, and
  `blocking_sends` — each positionally aligned with `per_message_sent`, so
  column *N* of every line describes the same message and one fact can be diffed
  across runs without re-assembling a block per message.
- **Configure interface** / **Configure messages** sections (the editors), and
  the **Output** display pane, headed by the three live-update and sampling lines
  described in §5.7.

The deadline-wait policy is derived from the shortest active interval —
continuous below 32 ms, a bounded window at or above it — and is **not
configurable and not previewed** (ADR-047). The former Standard/Precise choice
did nothing below 32 ms or on platforms without a timer-resolution request, and
could not be evaluated by the person asked; a read-only preview of the derived
policy was then rejected in turn, because its outcome is the same in both bands
and a line stating it would read identically on every look. What a running
channel actually got appears in the diagnostics card's timer readout.
**Align sends to UTC interval boundaries** remains an
independent choice and part of the run configuration, so changing it on an active
channel is applied by **Apply & Restart**, together with the rest of the prepared
replacement.

#### GUI State Persistence

GUI state and profile data are stored separately and serve different purposes:

- **Profiles** (channel config, message config, checksum settings) are TOML files shared between CLI and GUI. They live in the profile directory and are the primary unit of saved work.
- **GUI state** explicitly stores theme and the current/recent profile paths;
  egui's own presentation memory restores zoom. It uses `eframe`'s persistence
  mechanism, is never loaded by the CLI, and never conflicts with profile data.
- **Logging controls** are neither profile data nor persisted GUI state. Detail
  returns to INFO, all five pane visibility switches return on, and Log file
  returns off whenever Talker starts.

On exit, the GUI saves its state automatically. On next launch, the GUI reopens the last active profile and restores zoom and theme. Window geometry is deliberately **not** persisted: eframe restores it only after the window is first shown, which produced a visible double frame/title-bar flash on every launch (and could resurrect a broken tiny geometry) — the window always opens at its default size instead.

GUI state is stored in a platform-appropriate config directory (via the `dirs` crate or `eframe`'s default storage path).

---

## 4. Channels

### 4.1 Supported Interface Types

Each channel has exactly one interface port. The supported interface types are:

| Type | Notes |
|------|-------|
| RS-232 Serial | Full parameter configuration (see 4.2) |
| UDP Unicast | Host and port configurable |
| UDP Broadcast | Broadcast address and port configurable |
| UDP Multicast | Group address, port, outgoing interface, and TTL configurable |
| TCP Client | Connect to a remote host/port; reconnects after a failure (see 4.5) |

The channel abstraction in `core::channel` is designed for easy addition of future interface types (e.g., WebSocket, raw socket) without changes to the interface layers or scheduler.

### 4.2 Serial Configuration

All standard RS-232 parameters are user-configurable:

- Port (e.g., COM3, /dev/ttyUSB0)
- Baud rate: common standard rates are selectable from a list (110, 300, 1200, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600); a free-entry field allows specifying any rate outside this range
- Data bits (5, 6, 7, 8)
- Parity (None, Even, Odd)
- Stop bits (1, 2)
- Hardware flow control (RTS/CTS, None)

If Windows describes an absent port with its known filesystem-oriented file- or
path-not-found wording, the error instead reads `no device is present at
<port>`. The dependency's broader unavailable-device category also covers busy
and access-denied ports, so those and all unfamiliar or localized
operating-system messages retain their original detail.

After a run has successfully opened a serial port, timeout, would-block,
interrupted, and zero-write results can describe transient flow control and are
retried on the existing handle. Every other write error, including one the
operating system cannot classify more narrowly, makes that handle unusable.
Talker closes it before attempting to reopen the same configured port. If the
device returns under that name and no other process owns it, the same GUI or CLI
run resumes without user action. On Windows, an operation-aborted result still
replaces the handle even when the operating system categorizes it as a timeout.

Automatic replacement begins only after the run opened successfully. An initial
open failure still ends that run; the GUI uses its existing **Retry Channel**
action, while CLI mode must start a new run. Recovery does not search for a
renamed port: if the replacement appears under a different name, the configured
port must be changed. A failed write may have transferred a prefix before the
operating system reported the error, so serial recovery cannot promise
exactly-once delivery. When the write reported part of the message transferred
before failing, the send counts as **possibly partial** (§4.4) rather than
failed; the message is never resent. Disconnecting only the far-end RS-232 cable or device may
produce no operating-system write error at all; automatic recovery starts only
when the owned port handle reports a failure.

### 4.3 Real-Time Parameter Changes

Interface parameters (port, baud rate, UDP port, host address, etc.) are adjustable while the program is running in GUI mode. When a change requires closing and reopening the interface:

- Output pauses briefly
- The user is notified visibly in the GUI (status indicator)
- The interface is automatically closed, reconfigured, and reopened
- Output resumes without user action

If a serial channel is already recovering from an unusable handle, a successful
live update opens the new settings and clears the pending replacement. If that
update fails, the previous settings remain confirmed and automatic reopen
attempts continue with them.

The transport kind itself is selected when the channel is created and is not an
in-place interface parameter. A different transport is represented by a newly added
channel, as in listener.

### 4.4 Per-Channel Monitoring

Each channel maintains and displays:

- **Send outcomes** — cumulative scheduled, failed, possibly partial, suppressed,
  missed, and sent totals for the current run; they remain visible after recovery
  and after Stop. **Possibly partial** is a write that failed after transferring
  part of the message: the receiver may hold a fragment. On Windows a TCP write
  that timed out counts too, though Windows reports nothing transferred (§4.5).
  Wire bytes are counted separately from whole messages
- **Peer replies** — for a TCP client, "peer sent N bytes" (§4.5)
- **Log counts** — channel-attributed INFO, WARN, and ERROR entries delivered
  to the global Log pane since Start; DEBUG and TRACE remain available there
  when Detail admits them, but do not inflate INFO. While **log entries not
  shown this session** is present, those three tallies may be lower than what
  occurred
- **Status indicator** — the channel's current lifecycle state
- **Current fault** — the unresolved command or interface problem shown inline;
  a command failure takes precedence, retry attempts refresh the current
  interface explanation, and a successful send clears the interface fault;
  command faults clear only through their own successful command path

Event history lives in the global Log pane. There is no separate clickable
per-channel error log. A recovered channel can therefore have no current fault
while its current-run Send outcomes and log counts still show earlier failures.

### 4.5 TCP Client (ADR-059)

- **Reconnect.** After a failed write, at each retry point the backoff allows
  (§9.2), the client closes the failed stream and connects again with a 5 s
  timeout. The failed message is never resent; the next due message goes out on
  the new connection. The recovery line names the address.
- **Replies are drained.** Talker is a sender, but a device may answer. Before
  each write the client reads whatever the peer has sent, without blocking,
  discards it and counts it: "peer sent N bytes". Leaving it unread would fill the
  receive buffer, and closing a socket with unread data makes most stacks reset
  the connection, which can discard the peer's own in-flight data. When a drain
  finds that the peer has closed the connection, that send fails with nothing
  written, and the next retry point reconnects.
- **A write that times out.** A write the peer takes nothing of for 5 s fails
  with an error that says so, and the next retry point reconnects.
  On Windows it counts as possibly partial (§4.4): Windows reports a timed-out
  write as sending nothing, but leaves the connection in an undetermined state,
  so the peer may hold part of the message.
- There is no acknowledgement protocol and no server mode. Viewing replies is
  listener's job.

---

## 5. Messages

Each channel has one or more messages. Messages within a channel share the same interface port but are otherwise independent — each has its own format, encoding, payload, timing, timestamp configuration, and checksum configuration.

### 5.1 Message Formats

| Format | Description |
|--------|-------------|
| Hex | Arbitrary byte sequence entered as hexadecimal pairs; spaces and hyphens allowed as separators |
| UTF-8 | Unicode text encoded as UTF-8 |
| UTF-16 | Unicode text encoded as UTF-16; byte order (LE or BE) and optional BOM are user-configurable |
| ASCII | Text encoded through a selected code page (see 5.2); characters the code page cannot encode compile to a visible `?` fallback byte (ADR-023) |
| NMEA 0183 | Constructed by `nmea0183`; known UTC time/date fields may optionally refresh at every send (see §6.4) |

All format selections are saved as part of a profile.

### 5.2 ASCII Code Pages

When the message format is ASCII, the user selects a code page. This determines how characters with values 128–255 are encoded in the outgoing byte stream. The supported code pages are:

| Code Page | Common Use |
|-----------|-----------|
| CP437 | IBM PC / DOS (original IBM PC character set) |
| Windows-1252 | Windows Western European (ANSI) |
| Mac OS Roman | Classic Mac OS Western European |
| ISO-8859-1 | Latin-1; standard on Linux/Unix systems |

All four code pages are available regardless of the host operating system. This allows `talker` running on any platform to generate byte streams matching the expectations of a device or system built for a specific OS.

The editor accepts any Unicode text; a character the selected code page cannot encode is **not** an error. Each such character compiles to a single ASCII `?` (`0x3F`) replacement byte (ADR-023). The GUI surfaces the lossiness rather than hiding it: an amber count and a UTF-8 recommendation appear beside the code-page selector, and the wire preview / Output pane give the resulting `?` bytes a contrast-aware amber background (literal question marks are unmarked). Valid `‹XX›` byte markers (§5.3) still emit exact bytes; malformed marker syntax remains an error.

### 5.3 Character Entry

For ASCII and UTF-8 messages, the user types printable characters directly into a text field.

**Non-printable and extended characters** are entered using an **Insert Byte** dialog: a button opens a small dialog where the user enters a single byte value as two hex digits. The inserted byte is shown inline in the text field as a marker (e.g., `‹1B›` for ESC, `‹0D›` for CR). The marker is rendered distinctly from surrounding printable text so the message structure is clear at a glance.

For messages that are predominantly binary (many non-printable bytes), the **Hex format** is the appropriate choice and provides a more efficient entry experience than the Insert Byte dialog.

### 5.4 Timestamp

Each message optionally prepends an ISO 8601 timestamp to the wire output. The timestamp is generated at the moment the message is sent.

Timestamp configuration is per-message and includes independently toggleable components:

- **Date** (YYYY-MM-DD) — include or exclude
- **Time-of-day** (HH:MM:SS) — always included when the timestamp is enabled
- **Milliseconds** (.mmm) — include or exclude
- **Timezone designation** (e.g., `Z` for UTC) — include or exclude

Example forms (all representing the same moment):
```
2026-05-21T14:30:45.123Z     (date + time + ms + timezone)
2026-05-21T14:30:45Z         (date + time + timezone, no ms)
14:30:45.123                 (time + ms only, no date or timezone)
14:30:45                     (time only)
```

The timestamp is prepended to the payload. The outer checksum (Section 5.5), if enabled, covers the complete wire output including the prepended timestamp.

### 5.5 Checksum

Each message optionally appends a checksum to the complete wire output. This checksum is computed over the entire outgoing byte sequence — including the prepended timestamp if present — and appended after the payload.

This outer checksum is independent of and does not replace any checksum that is part of the message protocol itself. For example, an NMEA 0183 sentence includes its own `*XX` checksum inside the payload; the outer checksum wraps the entire output including that internal checksum.

Wire format when both timestamp and outer checksum are enabled:

```
[timestamp][payload][outer_checksum]
```

Checksum configuration is per-message and includes:

- **Algorithm** — selected from the supported list (see Section 7)
- **Intentionally wrong checksum** — option to send a deliberately incorrect value for negative testing

Checksum configuration is saved as part of a profile.

### 5.6 Send Interval

Each message has a send interval in milliseconds. An interval of zero means the message is dormant and is excluded from the send queue. There is no upper bound on the interval.

When an interval is changed while the channel is running:

- **Changed to zero** — the message is removed from the send queue immediately; other messages are unaffected; the channel continues running
- **Changed to a non-zero value** — the message is removed from the queue and re-inserted with its next fire time set to `now + new_interval`; other messages are unaffected

### 5.7 Data Display Pane

Each channel includes a real-time display pane showing outgoing data as it is sent. The view is configurable between:

- **Hex** — each byte shown as two uppercase hex digits (e.g., `0D 0A`)
- **ASCII** — printable characters shown as-is; control characters rendered as replacement symbols
- **Decoded text** — valid UTF-8 sequences rendered as Unicode characters; invalid bytes shown as `U+FFFD`

The display mode is a GUI-only setting and is not saved in the profile.

#### Completeness notices

Three lines head the pane above the view controls. The first two explain the
shared live-update queue and its warning; the third qualifies Output sampling.
The queue can also affect live diagnostic readouts, as its tooltip states. It is
shown here because the warning needs one visible home and discarded payload
updates land in Output, not because the queue is Output-only (ADR-052):

1. **Dropped live updates** — `<n> live updates dropped; Output may omit lines
   and live readouts may lag`, an attention callout shown when the runner has
   discarded any update because the UI queue was full. It can cost a payload
   line, a counter update, a timer change, or an interface error notice; sends
   are never delayed by it. Later counters repair cumulative totals, the
   send-failure episode count, and whether a send/reopen fault is currently
   active; the final run totals stay exact. Dismissible, per §3.2.
2. **Live-update queue** — `live update queue at last screen check:
   <len>/<capacity> · peak <peak> · <drops> dropped`, shown in small, quiet text.
   It is the gauge behind the warning above and lives beside it rather than under
   Timing & runtime details. Queue pressure never delays a send; later cumulative
   updates correct the live totals and current send-failure state, and the final
   run totals remain exact.
3. **Sampled output** — `sampled output · not every sent payload is shown ·
   limit ~<n>/s`, shown once the cumulative accepted total proves an omitted
   payload update, and then for the rest of the run. A standing statement about
   how the pane works rather than a state to act on, so it takes no accent and
   no ⚠. It uses the same small, quiet treatment as the queue gauge; the active
   dropped-update callout alone carries urgency. It is not dismissible because
   it describes a condition that stays true.

While the dropped-update warning is unacknowledged, the **Output** section
header itself reads `Output ⚠ <n> dropped` in the warning accent. The pane is
collapsed by default and this warning no longer raises the diagnostics card's
badge, so without the marked header a reader could sit in front of the condition
and never see it. The marker and the count carry it; the accent only reinforces
them.

#### Control Character Rendering

In ASCII display mode, control characters (bytes 0x00–0x1F and 0x7F) are rendered as visible symbols in one of three user-selectable styles:

| Style | Name | Example (LF / CR / ESC) |
|-------|------|--------------------------|
| A | Unicode control pictures (U+2400 block) | `␊` / `␍` / `␛` |
| B | Bracketed abbreviations | `[LF]` / `[CR]` / `[ESC]` |
| C | Hex escape codes | `<0x0A>` / `<0x0D>` / `<0x1B>` |

Style A is the default. All styles cover the full C0 range (0x00–0x1F) and DEL (0x7F).

Display panes are available in the GUI only. In CLI mode, stdout echo (Section 5.8) serves as the data visibility mechanism.

### 5.8 Standard Output Echo

Output data is optionally echoed to stdout. This is toggled by the user and is available in both CLI and GUI modes.

---

## 6. NMEA 0183 — `nmea0183` Crate

The `nmea0183` library crate handles all NMEA 0183 formatted data. It is:

- A standalone library crate at `nmea0183/` in the workspace
- Written with no dependencies on the `talker` crate
- Designed to be published to crates.io independently for use in other Rust projects

The crate includes:

- Sentence parsing (input validation and field extraction)
- Sentence construction (building valid sentences with correct checksum)
- Sentence type identification
- Checksum computation (XOR of all bytes between `$` and `*`) and verification
- Talker ID selection and validation
- Arbitrary/proprietary sentence construction (see 6.3)

---

### 6.1 Talker IDs

All standard NMEA 0183 talker identifiers are supported as selectable options. The user selects the talker ID when constructing a sentence; any valid two-character ID is accepted. The following are the defined standard talker IDs:

| ID | Device / System |
|----|-----------------|
| AG | Autopilot — General |
| AP | Autopilot — Magnetic |
| CD | Communications — Digital Selective Calling (DSC) |
| CR | Communications — Receiver / Beacon Receiver |
| CS | Communications — Satellite |
| CT | Communications — Radio-Telephone (MF/HF) |
| CV | Communications — Radio-Telephone (VHF) |
| CX | Communications — Scanning Receiver |
| DF | Direction Finder |
| EC | Electronic Chart Display & Information System (ECDIS) |
| EP | Emergency Position Indicating Beacon (EPIRB) |
| ER | Engine Room Monitoring Systems |
| GA | Galileo receiver |
| GB | BeiDou (BDS) receiver |
| GL | GLONASS receiver |
| GN | Combined / multi-constellation GNSS |
| GP | Global Positioning System (GPS) |
| GQ | QZSS receiver |
| HC | Heading — Magnetic Compass |
| HE | Heading — North Seeking Gyro |
| HN | Heading — Non North Seeking Gyro |
| II | Integrated Instrumentation |
| IN | Integrated Navigation |
| LC | Loran-C receiver |
| RA | RADAR / ARPA |
| SD | Sounder — Depth |
| SN | Electronic Positioning System, other/general |
| SS | Sounder — Scanning |
| TI | Turn Rate Indicator |
| VD | Velocity Sensor — Doppler |
| VW | Velocity Sensor — Speed Log, Water, Mechanical |
| WI | Weather Instruments |
| YX | Transducer |
| ZA | Timekeeper — Atomic Clock |
| ZC | Timekeeper — Chronometer |
| ZQ | Timekeeper — Quartz |
| ZV | Timekeeper — Radio Update (WWV/WWVH) |

In addition to these, the user may enter any arbitrary two-character talker ID to accommodate non-standard or future devices.

---

### 6.2 Supported Sentence Types

The following sentence types are supported for construction and parsing. Any talker ID from Section 6.1 may be combined with any applicable sentence type.

| Category | Sentences | Description |
|----------|-----------|-------------|
| GNSS / Position | GGA | GPS fix data — position, time, quality |
| | GLL | Geographic position — latitude/longitude |
| | GNS | Fix data — multi-constellation |
| | RMA | Recommended minimum — Loran-C data |
| | RMB | Recommended minimum — navigation data |
| | RMC | Recommended minimum — specific GPS/transit |
| | VTG | Track made good and ground speed |
| | ZDA | Date and time (UTC + local zone) |
| Satellite Data | GSA | DOP and active satellites |
| | GSV | Satellites in view |
| Heading | HDG | Heading — magnetic, deviation, variation |
| | HDM | Heading — magnetic |
| | HDT | Heading — true |
| Speed / Water Log | VHW | Water speed and heading |
| | VBW | Dual ground/water speed |
| | VLW | Distance traveled through water |
| Depth | DBT | Depth below transducer |
| | DPT | Depth of water (with keel offset) |
| Wind | MWD | Wind direction and speed — true |
| | MWV | Wind speed and angle — apparent or true |
| Water Temperature | MTW | Mean temperature of water |
| Navigation / Autopilot | APB | Autopilot sentence B |
| | BOD | Bearing — origin to destination |
| | XTE | Cross-track error |
| Set and Drift | VDR | Set and drift |
| Transducer / Environment | XDR | Transducer measurement (generic — pressure, temp, humidity, etc.) |
| AIS | AIVDM | AIS VHF data-link message (received from other vessels) |
| | AIVDO | AIS VHF data-link own-vessel report |

Note: AIS sentences use `!` as the start character and the `AI` talker ID regardless of source. The 6-bit ASCII payload armoring is handled by the module.

---

### 6.3 Proprietary Sentence Support

NMEA 0183 proprietary sentences begin with `$P` followed by a manufacturer code and data fields. The `nmea0183` crate supports these in two ways:

**Named proprietary sentences** — fully implemented with field-by-field construction and validation for known formats:

| Sentence | Manufacturer / Origin | Fields |
|----------|----------------------|--------|
| `$PRDID` | Teledyne RDI (ADCP) | Pitch (sddd.dd), Roll (sddd.dd), Heading true (ddd.dd) |
| `$PASHR` | Ashtech / RT300; also output by OxTS, Applanix, SBG, Trimble, Novatel | UTC time, Heading true (hhh.hh), T flag, Roll (rrr.rr), Pitch (ppp.pp), Heave (xxx.xx), Roll accuracy (a.aaa), Pitch accuracy (b.bbb), Heading accuracy (c.ccc), Aiding status, IMU status |

`$PRDID` does not include a checksum by convention. `$PASHR` includes a standard NMEA checksum. The `$PASHR` GNSS quality field (field 10) is exposed as a raw `u8` because Trimble and Novatel define the values differently.

**Arbitrary proprietary sentence builder** — for any `$P` sentence not in the named list, the user can enter the manufacturer code, data fields, and opt to append a standard NMEA checksum or omit it.

Additional named proprietary sentences may be added as requirements are identified.

---

### 6.4 Live UTC Field Substitution

An NMEA message may enable **Live time (UTC)**. At every send, Talker replaces only
the positions declared by `SentenceType::time_fields()` and rebuilds the sentence's
protocol checksum. The prepended message timestamp (§5.4), when enabled, and these
NMEA fields use the same captured UTC instant.

| Sentence types | Replaced fields (zero-based within the NMEA field list) |
|----------------|---------------------------------------------------------|
| `ASHR`, `BWC`, `BWR`, `GBS`, `GGA`, `GNS`, `GRS`, `GST`, `ZFO`, `ZTG` | 0 = UTC time |
| `GLL` | 4 = UTC time |
| `RMC` | 0 = UTC time; 8 = UTC date (`ddmmyy`) |
| `ZDA` | 0 = UTC time; 1 = day; 2 = month; 3 = four-digit year |

UTC time is `hhmmss` by default and `hhmmss.sss` when **Milliseconds** is enabled.
Day and month are two digits. Typed values remain in the profile but are overridden
on wire; if the typed list is short, empty fields are appended through the final
mapped position. Correct/Omit/Wrong NMEA checksum modes retain their meaning after
substitution. Enabling Live time for any standard or custom sentence with no mapping
is a validation error and blocks Start/Apply.

---

## 7. Checksum and CRC Support

Checksums are configured per message (see Section 5.5). They are optional; the default is no checksum. This is entirely separate from the NMEA protocol checksum, which is handled by the `nmea0183` crate.

### 7.1 Supported Algorithms

At minimum the following are supported:

| Algorithm | Common uses |
|-----------|-------------|
| XOR (1-byte) | NMEA-style, simple device protocols |
| CRC-8 | Sensor buses, simple embedded protocols |
| CRC-16/KERMIT | Serial protocols; some devices call it "CCITT" |
| CRC-16/MODBUS | MODBUS RTU |
| CRC-32 | File integrity, Ethernet |

Additional algorithms may be added as specific device requirements are identified.

### 7.2 Behavior

The checksum is computed over the complete wire output for the message — including the prepended timestamp if present. The result is appended after the payload, high byte first, except CRC-16/MODBUS, which MODBUS RTU carries low byte first (ADR-057). The option to intentionally send an incorrect checksum (for negative testing) is supported; it alters the last appended byte.

"CCITT" names several different CRC-16 variants. The one talker computes is CRC-16/KERMIT (reflected, initial value 0, check value 0x2189 for `123456789`), stored in profiles as `crc16_kermit`. Other variants, such as CCITT-FALSE and XMODEM, and a byte-order setting are not provided until a device needs one.

Checksum configuration is saved as part of a profile.

---

## 8. Scheduling and Profiles

### 8.1 Scheduling

Each channel runs a **priority-queue scheduler**. Each message within the channel has its own independent send interval and is scheduled independently of all other messages. ("Priority queue" describes the model; the implementation is a linear next-fire scan over the channel's messages, which is equivalent and faster at the small message counts talker handles.)

**Queue model:**

- The scheduler tracks each message's next-fire-time (conceptually a priority
  queue; see the model note above).
- With the default **Immediate** cadence alignment, when a channel starts all enabled
  messages (interval > 0) have next-fire-time = now and fire immediately at t=0.
- With **UTC phase** alignment, each enabled message instead waits for the strict next
  boundary where Unix-epoch time is an integer multiple of that message's interval.
  Exactly on a boundary means waiting one full interval. Intervals need not divide a
  day; phase is against the Unix epoch, not local midnight.
- The scheduler picks the message with the earliest next-fire-time, waits until that time, sends the message, then re-inserts it with next-fire-time = previous-fire-time + interval.
- When two messages are due at the same time, they fire in list order (the order in which they appear in the message list for that channel).
- **After a stall** (a blocked send, the machine sleeping), a message that is more than one interval overdue fires **once**, then its next-fire-time jumps forward to the first grid point (`previous-fire-time + k·interval`) still in the future. Missed intervals are skipped, not burst out back-to-back: talker generates test cadence, so a receiver should see the rate resume, not a flood catching up the count.

**Interval = 0 (dormant):** A message with interval = 0 is excluded from the queue and does not send. Changing a message's interval to 0 while the channel is running removes it from the queue immediately; other messages are unaffected and the channel continues running.

**Live interval changes:** When a message's interval is changed to a non-zero value
while the channel is running, it is removed from the queue and re-inserted at
now + new-interval in Immediate mode, or at the new interval's strict next UTC phase
in UTC-phase mode. All other messages continue unaffected. The channel is never
stopped by an interval change.

**Observer-state repair (ADR-018/056):** Periodic and final counter updates carry
the cumulative send totals, a cumulative send-failure episode count, and the
current optional send/reopen error. Immediate failure and recovery updates keep
the screen responsive but are best-effort; a later counter therefore repairs
either edge if the live-update queue discarded it, including multiple complete
episodes between observations. A failed reopen refreshes the current obstacle and
withholds that due send without adding a failed write; a later failed write does
increment the failed-send total. Neither opens another episode. These counters do
not reconstruct a dropped log entry or its per-channel severity tally. No telemetry
heartbeat is added: a dormant schedule still waits indefinitely, and repair occurs
at the next delivered send-path or final counter update.

After startup, both modes advance from monotonic deadlines so ordinary wall-clock
slew cannot accumulate cadence drift. A UTC-phase schedule compares its paired
monotonic/wall anchor with the wall clock at most once per second. A displacement of
at least 250 ms rephases every active message to its next future UTC boundary and
increments a visible re-alignment counter. It never emits catch-up sends or counts
the elapsed wall-clock grid as scheduler misses.

**Deadline waiting (ADR-017 / ADR-034 / ADR-047):**

- Every active runner blocks on an interruptible monotonic deadline wait. It does
  not busy-spin, and queued commands can interrupt the wait.
- The wait policy follows the schedule; there is no user setting (ADR-047). On
  Windows, a shortest active interval below 32 ms holds the process's refcounted
  1 ms timer-resolution request continuously. At 32 ms and for every slower active
  schedule, the runner first waits normally until 32 ms before the next send
  deadline, acquires the 1 ms request for the final wait, and releases it before
  payload rendering and `Interface::send`. Closely spaced deadlines may make these
  windows touch; making every message dormant releases any request before the
  runner's indefinite command wait.
- On macOS and Linux the runner keeps one native deadline wait. It adds no staging
  wake and makes no Windows-style resolution request.

The runner's `TimerReconciler` owns the current timer intent, resolution guard,
derived status, edge notification, and observer-drop accounting. It reconciles
schedule transitions, stages and releases bounded windows, and drops the guard
before windowed rendering/sending and before final blocking status delivery.

The wait policy controls deadline wakes, not timestamp formatting or cadence phase.
Cadence alignment independently chooses Immediate or UTC-phase startup/rebase
deadlines. Neither choice changes `SystemTime` accuracy, compensates for render or
interface time, or establishes when serial/network bytes physically leave the host.
The timing telemetry in §3.2 exposes the configured alignment, wall-clock
re-alignments, and measured application boundaries without claiming a hard real-time
guarantee.

The fixed duration buckets and bounded recent-window aging used by those measurements
come from `wiredata-telemetry`. Talker retains its send-specific timing aggregates,
recorder and measurement boundaries, capacity interpretation, completed-run
retention, and GUI/report presentation; the shared crate defines no application
telemetry schema or runtime policy.

**Pushed timing snapshot freshness (ADR-042):**

- Every periodic counter update carries the exact monotonic instant at which its
  cumulative and recent histograms were computed and marks itself non-final. The
  mandatory exact-at-rest counter update reuses the run's final monotonic instant
  and carries explicit final provenance. Finality is never inferred from UI or
  thread lifecycle.
- A snapshot with no capture instant is **Pending**. A non-final snapshot is
  **Current** while its capture age is less than `RECENT_WINDOW` and **Expired**
  once its age is greater than or equal to that window. A provenance-marked final
  snapshot is **Final** regardless of later display age. An abnormal exit that
  omits the mandatory final update therefore cannot relabel an older periodic
  snapshot as final.
- Capacity, Cadence, and detailed Timing consume that one classification. Expired
  recent histograms are not presented as current and cannot drive measured
  application headroom. Capacity may use a sufficiently warmed cumulative
  run-wide histogram instead and must identify that fallback.
- Snapshot age does not create a runner wake. Counters remain send-path,
  rate-limited updates plus the mandatory final update; an all-dormant schedule
  continues to block indefinitely on its command receiver with zero telemetry
  heartbeat.

**Capacity preflight (ADR-035):**

- Draft demand is the sum, across non-dormant messages, of exact compiled wire
  length divided by interval. Incomplete or invalid messages withhold the estimate
  instead of understating it. UDP/TCP show requested `msg/s` and `B/s`; Talker does
  not invent a network link capacity it cannot know.
- For Serial, each wire byte consumes `1 + data bits + parity bits + stop bits` at
  the configured baud. Aggregate demand above the resulting bit rate cannot be
  sustained physically. Demand from 80% through 100% is valid but low-margin.
  Flow control, USB adapter/driver buffering, operating-system delay, and aligned
  message bursts can reduce practical capacity even below 100%.
- Measured application headroom adds the separate render-p99 and send-call-p99
  histogram upper bounds and compares that service estimate with aggregate draft
  message rate. At least 20 paired samples are required; eligible approximately
  last-ten-second data is preferred, with labelled cumulative run-wide data as the
  slow-schedule or expired-snapshot fallback. The sum is not a joint p99. A
  retained run can describe an older draft, and a send call can return before bytes
  physically leave a driver or kernel buffer.

All capacity findings are advisory. They do not disable Start because deliberately
oversubscribed schedules are useful tests; actual deadline, unsent, and throughput
telemetry shows what the run achieved.

**Completed-run retention (ADR-036):**

- A run begins when its schedule is armed, after interface open and any predecessor
  join. Talker captures a wall-clock start beside that monotonic arm instant.
- On Stop or owner disconnect, the send loop captures wall-clock finish and monotonic
  elapsed duration. It attempts ADR-018's blocking final Counters delivery, then emits
  one self-contained `RunFinished` over the reliable control lane from the same final
  counter/timing snapshot. A failed interface open occurs before arming and does not
  replace the last completed summary.
- The supervisor retains only the greatest process-unique run id for each stable
  channel slot. Apply & Restart discards predecessor sampled telemetry so it cannot
  alter the replacement's counters, but drains the predecessor's reliable completion
  tail. Thus polling order cannot make an older run replace a newer completion.
- Retention is bounded to one summary and one per-message count vector per channel.
  It is not profile data or an on-disk history. Loading a profile creates fresh slots
  and clears it. Wall-clock values can reflect system-clock steps; elapsed duration
  uses the monotonic clock.

### 8.2 Profiles

A profile is a named, saved configuration. A profile defines one or more channels, each with its full configuration. This makes multi-channel operation a natural part of the profile system.

Each channel entry within a profile includes:

- Interface type and all parameters
- Cadence alignment (`immediate` by default, or `utc_phase`)
- One or more message definitions, each containing:
  - Format and encoding (including code page for ASCII)
  - Payload data
  - Timestamp configuration
  - Checksum configuration
  - Send interval

Profiles can be:

- Named, saved, loaded, and deleted
- Switched at runtime in the GUI
- Specified by name at launch in the CLI (`--profile <name>`)
- Stored in TOML so they can be inspected, edited, and version-controlled outside the program

**Example profile:** [`talker/profiles/profile.example.toml`](../profiles/profile.example.toml)
is a complete, working profile: a named UDP broadcast channel with UTC-phase
cadence, an NMEA message with a live time field, and a hex message with a
timestamp and a checksum. A test loads it and checks that every key in it is one
the profile types read back, so it cannot drift from the schema.

#### Profile Schema Versioning

Every profile file includes a `version` integer field in its header. The current schema version is `3` (ADR-057). This field enables `talker` to detect and handle schema changes as the program evolves.

**Loading behavior by version:**

- **Matches current version:** load normally.
- **Older version:** refuse to load and instruct the user to recreate the profile. Nothing was deployed before schema 3, so each schema so far has taken a clean break rather than carrying migration code.
- **Newer version than the running binary understands:** warn the user and refuse to load.
- **No `version`:** refuse to load, with "add `version = 3`" (ADR-062).

A missing field takes its default (`#[serde(default)]`), so additive changes within a schema version load older files cleanly. An **unknown key is refused** (ADR-062), with a message naming the key and where it is: a misspelled key would otherwise silently become a default. The version number only increments when a breaking schema change occurs that defaults cannot absorb; a future breaking version can add migration code at that point.

---

## 9. Error Handling and Logging

### 9.1 Error Handling

All errors are handled explicitly. The program uses Rust's `Result` type throughout and does not panic in production code paths. Errors are surfaced to the user with clear, actionable messages.

### 9.2 Logging

Logging uses the `tracing` facade with `tracing-subscriber` for dispatch. Log levels are consistent across both interfaces:

| Level | Examples |
|-------|---------|
| ERROR | Connection failure, file not found, encoding error |
| WARN | Parameter change caused reconnect, malformed record skipped, a channel falling off its send schedule |
| INFO | Channel opened/closed, profile loaded, send started/stopped, a channel returning to schedule |
| DEBUG | Repeated retry detail useful while diagnosing a persistent fault |
| TRACE | Finest-grained application and dependency diagnostics |

#### Edge-triggered channel conditions

Three conditions can persist for the whole of a long run, and each is logged
**at its edges** rather than per occurrence. A line per event would emit
thousands a second under exactly the fault it describes — and in the GUI the log
pane is itself fed by a queue, so the flood would degrade the display it is
competing with.

| Condition | Opens with | Closes with |
|-----------|-----------|-------------|
| Sends failing | the first failed write, at WARN | the first success, at INFO, carrying the episode's failed and withheld counts |
| Off schedule (ADR-053) | the first skipped send, at WARN | the first cadence point reached at least five seconds after the last skip, at INFO, carrying the episode's total |
| Display updates discarded | the first discarded update, at WARN | nothing — see below |

After a failed write, additional due sends are gated by a bounded retry delay
that starts at 250 ms and doubles to at most five seconds. A retry is considered
only when the schedule next reaches a due send after that delay, so five seconds
caps retry frequency rather than recovery latency; a slower schedule waits for
its next due point. For an unusable serial handle, that point first attempts the
replacement described in §4.2. If reopening fails, the due send is counted as
withheld: no payload is rendered, no interface write is attempted, and no render
or send-call timing or send-overlap attribution is recorded. The original write
failure remains a failed send, and the first later successful write closes the
same failure episode. That single INFO recovery edge names the configured serial
port when sending recovers on the automatically reopened handle; opening the
handle does not add a separate log event. While the episode remains open, the
channel row can refresh its current fault from later write or reopen attempts;
that live explanation does not create additional WARN edges or failed-send
episodes. Periodic counters repeat it so a dropped opening or recovery edge is
eventually repaired.

Two of the three close; the third cannot. Discarded updates are a run total
that never returns to zero, so there is no recovery edge to report. The standing
count is on the Output pane (§5.7), which is where it can be acted on.

Off-schedule recovery is a **settle window**, not the first clean send: a
marginal channel skips intermittently, so closing on the first clean send would
log a start and an end per pair and reproduce the flood. Five seconds is a
**minimum, not a deadline** — the check runs where the skip count arrives, on a
cadence point the channel reaches, so no channel is woken to announce its own
recovery and a slow schedule reports at its next send rather than at five
seconds. A run that stops while off schedule logs the run's own missed total, so
the log never ends on an unanswered warning. A second lapse is a second episode,
counted from zero.

This is the only account of missed sends available in CLI mode, which has no
diagnostics card.

#### Who the log is written for (ADR-054)

A log line states **what happened to the reader's channel**, in the vocabulary
the screen uses — not the mechanism inside talker that carried it. The reader is
a technician with a device on the far end of a wire.

Events carry a structured `channel` field, and the GUI tallies any event carrying
it onto that channel's row in the channel list. **That field is a claim**: this
event is about that channel's operation. A failed send, a clock step, and a
skipped cadence point all qualify.

Talker's own bookkeeping faults do not, and carry no `channel` field — a bug in
command tracking must not raise a warning badge on the reader's serial link,
where it cannot be acted on or cleared. Each names the channel in its text and
follows one shape:

```
internal fault on channel <label> (<n>x): <what talker's own state did>.
<what it costs the reader>. Please report this.
```

They stay at WARN or ERROR rather than DEBUG: a fault recorded only when someone
had already raised the log level is one nobody hears about. They are rate
limited on a decade cadence — the 1st, 10th, 100th … occurrence, with the running
count — so a single lapse is never missed and a wedged state machine cannot flood
the log.

#### CLI Logging

In CLI mode, the profile's `[logging].level` selects the process-wide threshold.
INFO is the default; DEBUG is admitted at Debug or Trace, and TRACE only at
Trace. Talker does not read `RUST_LOG`. Log output destinations are independently
selectable at launch:

- **stdout** — enabled or disabled via flag
- **log file** — enabled or disabled via flag; path is configurable or defaults
  to a platform-appropriate location

The threshold and destinations are fixed for that CLI launch. File writes use a
non-blocking logger thread rather than the channel's send thread.

#### GUI Logging

The GUI includes a global **Log** pane for TRACE, DEBUG, INFO, WARN, and ERROR
events. Its controls are deliberately separate:

- **Detail** is the process-wide threshold for new events eligible for the
  pane, console, and enabled file destination. It defaults to Info each launch.
  Debug includes DEBUG and higher events; Trace includes all five levels. The
  two bounded GUI destinations can independently omit an eligible event if
  they cannot keep up.
- Five independent **Show in pane** switches hide or show retained pane rows
  only. All default on. All five levels share one chronological history of the
  newest 2,000 entries delivered to the pane after Detail is applied; retention
  happens before visibility, so hidden rows use the same capacity and can evict
  older visible rows. The switches do not alter collection, channel
  INFO/WARN/ERROR counts, or file contents; a hidden retained row can reappear
  only while it remains in that shared history.
- **Clear** removes only the on-screen pane rows. Channel counts and saved files
  are unchanged, as are the session's pane-loss and file-loss notices.
- **Log file** is off at every launch and lasts only for the GUI session. Loading
  a profile cannot enable it or change Detail or Show in pane. When enabled, it
  writes under the platform's local-data `talker/logs` directory with the
  `talker.log` prefix and starts a new file each day. That GUI destination is
  fixed. **Open folder** creates it if needed and asks the desktop to open it on
  a separate helper thread; the action neither turns file logging on nor uses
  its file worker. A launch failure remains visible and may be retried. If the
  platform cannot provide a local-data directory, file logging and Open folder
  are unavailable.

Events offered to the on-screen Log cross a bounded, non-blocking handoff. If it
fills, Talker keeps sending and shows a persistent, session-cumulative **log
entries not shown** count. Those entries never enter the pane's retained history
or its derived per-channel INFO/WARN/ERROR tallies, so the latter may be low.
The saved-file destination is independent: loss in one does not establish loss
in the other. Pane processing is bounded per frame, so accepted backlog is
carried across frames.

Opening, writing, flushing, and closing GUI log files belong to a dedicated file
worker; neither the UI nor a channel's send thread waits for disk I/O. New file
entries cross a bounded queue. If that queue fills, Talker keeps sending and
shows a persistent, session-cumulative **log entries not saved** count. After
entries accepted before a known queue loss have drained, the worker attempts to
write a plain gap line for that loss directly in the saved output. A marker is
best-effort: the same destination failure that loses entries can also prevent it
from being written. Disabling or shutting down first drains accepted entries,
then attempts a final gap line and flush.
While the GUI remains open, failure to write either an entry or its gap line
turns file logging off and leaves a Fault callout in the Log pane; the visible
session count remains even when the file cannot record its gap. File-session
boundaries prevent a late entry or loss from an earlier destination being
assigned to a later one.

GUI rotation is time-based only. It starts a new file daily but deletes no old
files and places no bound on total disk use. CLI profiles may instead select no
rotation, hourly rotation, or daily rotation; none supplies retention.

---

## 10. Testing

Testing follows Rust conventions:

- **Unit tests** live in `#[cfg(test)]` modules at the bottom of the file they test.
- **Integration tests** live in the `/tests` directory of each crate.

Coverage requirements:

- All `core` modules have unit tests covering normal operation, edge cases, and error conditions.
- The `nmea0183` crate has thorough unit tests covering parsing, construction, and checksum logic for all supported sentence types.
- Integration tests in `talker/tests/` cover end-to-end flows: profile load → channel open → scheduler run → data send.
- Tests must pass on all three target platforms in CI.

---

## 11. Code Quality

- Production-level code throughout; no prototype or placeholder logic in the final deliverable.
- All public APIs are documented with doc comments (`///`).
- `clippy` warnings are resolved; `rustfmt` formatting is enforced.
- Dependencies are chosen conservatively; each must be justified.

---

## 12. Open Items and Planned Future Features

### 12.1 Open Items

- Additional interface types beyond TCP/UDP/serial (WebSocket, raw socket, etc.)
- Additional CRC/checksum algorithms beyond the initial set, and a per-message
  byte-order setting (§7.2)
- Additional named proprietary NMEA sentences beyond `$PRDID` and `$PASHR`
- Installer/packaging requirements for broader distribution
- Binary field construction (typed fields: u8, u16, u24, u32, u64, i8, i16, i32, i64, f32, f64 with per-field byte order) — deferred; hex format covers the immediate need

### 12.2 Planned Future Features

- **File source** — send data read from a file, either as a raw byte stream or as parsed records (one per send interval)
- **Keyboard injection** — real-time injection of data into an active channel from the keyboard (GUI only)
- **Capture and replay** — record live output to a file, then replay it exactly including original timing
- **Auto-response / triggers** — monitor incoming data and automatically send a configured response when a specific byte pattern is detected
- **Ad-hoc multi-channel CLI flags** — repeated `--channel` flags for launching multiple channels without a profile file

---
