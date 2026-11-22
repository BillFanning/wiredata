# TODO — Talker

Work the `talker` code does not do yet, and workspace-level work. Decisions go in
[ADR.md](ADR.md) and behaviour in the [specification](talker_specification.md).
Tasks for the `nmea0183` library live in
[`nmea0183/docs/TODO.md`](../../nmea0183/docs/TODO.md), and listener's in
[`listener/docs/TODO.md`](../../listener/docs/TODO.md).

Each item names the symbol, file or section where the work lands, so it can be
found again. Remove an item when it is done: the commit says what changed.

---

## Features

- [ ] **A user-chosen profile folder.** Profiles default to the OS config folder
  (`core::profile::default_dir`), the safe, always-writable choice. Add an
  override, such as a `--profile-dir` flag, a `TALKER_PROFILE_DIR` variable or a
  GUI setting, so users can point talker elsewhere. That also allows a portable
  "profiles beside the .exe" layout without making it the default.
- [ ] **File-log retention (workspace).** Neither app deletes a log file
  (ADR-069): talker's GUI logs rotate daily, a CLI profile chooses never, hourly
  or daily (`FileLogConfig`), and listener's event log rotates daily. If bounded
  log disk use is wanted, decide who deletes, what limit, and how a failed
  cleanup is reported, before describing file logging as bounded.

## Retry and backoff (workspace)

Found by a survey of every retry in the workspace on 2026-11-19.

- [ ] **Talker's GUI never retries a failed open.** The supervisor calls
  `open_and_run`, which reports the failure once; the CLI calls
  `open_retrying_and_run` (1 s doubling to 30 s, ADR-060). Listener's GUI
  retries when a channel's reconnect is on. Decide whether talker's GUI should
  retry too. Once a channel is open, both talker front ends already recover.
- [ ] **The two CLIs disagree on retrying a failed start (ADR-060).** Talker's
  retries every channel and cannot be turned off (`--require-all` stops the run
  instead). Listener's retries only a channel whose `ReconnectPolicy` is
  enabled, which is off by default, and warns only for recording channels.
  Decide whether the one contract should make them agree.
- [ ] **One answer for a device that keeps dropping.** There are three now:
  listener's reconnect carries the count and wait forward within
  `RECONNECT_STABLE_AFTER` (ADR-058) and never gives up; listener's
  recorder slows to `unstable_retry` after `unstable_after` faults within
  `unstable_window` and never gives up; talker resets to
  `RETRY_BACKOFF_INITIAL` after every successful send, so it probes at the
  fastest rate on each drop. Pick one, or record in an ADR why each differs.
- [ ] **The specs repeat retry numbers, and nothing checks them.** The docs test
  catches a renamed identifier, not a changed value: if `RETRY_MAX` changes,
  listener's §9.1 and §56 still say 30 s and no test fails. For a value a user
  relies on, the spec could name the constant instead of repeating the number,
  or a test could compare the two. Decide with the next spec change.
- [ ] **Say why talker's two retry paces differ.** Send failures retry from
  `RETRY_BACKOFF_INITIAL` to `RETRY_BACKOFF_MAX` (250 ms to 5 s), a failed open
  from `OPEN_RETRY_FIRST` to `OPEN_RETRY_MAX` (1 s to 30 s). The fast pace suits
  a scheduled send stream, but no document sets the two side by side.

## Dependencies (workspace)

- [ ] **Replace chrono (RUSTSEC-2026-0543).** chrono is unmaintained, with no
  safe upgrade, and `deny.toml` ignores the advisory only until chrono is gone.
  talker, listener and `wiredata-log` use it directly, in about 50 places, for
  local times, offsets and formatting under the time policy (ADR-067: local
  unless UTC, ISO 8601 offsets in files and logs, NMEA's own sign inside NMEA
  fields). Candidates: `jiff`, or `time`, which is already in the tree through
  `tracing-appender`. Keep every format on screen, in files and on the wire
  byte for byte the same, with tests that pin them before the switch. Then
  remove the `deny.toml` entry.

## Checks by hand

- [ ] **Serial recovery on real hardware (Windows).** With a schedule running,
  unplug a USB serial adapter and plug it back in under the same COM name, and
  confirm the same run resumes. Separately, hold CTS low through a write timeout
  and confirm that flow control causes no port reopen or DTR-reset churn. Watch
  for a possibly partial write while there (§4.4). The automated tests fix the
  state and accounting contract; this checks real drivers. One adapter with
  nothing on its DB9 is enough: under RTS/CTS, CTS is then never raised, so
  every write waits until it times out. Watch the adapter's DTR light for churn.

## GUI

- [ ] **Cache the Cadence grouping and its tooltip.** `cadence_groups` allocates
  and `cadence_tooltip` builds about 1 kB of text every repaint, hovered or not.
  The better shape is a `wiredata-ui` change so `signal_row` builds its tooltip
  only on hover. That touches shared chrome, so do it with the next chrome pass.
- [ ] **Split `talker/src/gui/widgets.rs`** (about 2,700 lines) into a `widgets/`
  folder mirroring listener's, re-exported flat so call sites keep
  `widgets::<name>`. A move only: its own commit, not bundled with feature work.
- [ ] **Do `running` and `warning` earn two colours? (ADR-050)** They look the
  same to a red-green deficiency. Everywhere they appear, a glyph and a word
  already carry the state, so this is about reinforcement one reader does not
  receive. Revisit if a third state ever wants a colour.

## Soak

- [ ] **Fast-then-dormant snapshot expiry (ADR-042).** `recent_snapshot_state` is
  unit-tested and the GUI is tested against synthetic states, but nothing runs the
  real transition end to end. Run a channel fast enough to warm the recent window
  past `MIN_SERVICE_SAMPLES`, make every message dormant, and hold past
  `RECENT_WINDOW`. Then check that the supervisor's retained snapshot is Expired,
  that headroom falls back to labelled run-wide timing, and that the dormant
  runner sent no further `Counters`. Its home is `talker/tests/soak.rs`.

## macOS (planned)

- [ ] **App Nap opt-out.** macOS timers are fine-grained, but App Nap throttles the
  timers of hidden or occluded apps, the counterpart of the Windows throttling both
  apps opt out of (`keep_timer_resolution_when_minimized`). Hold an `NSProcessInfo`
  `beginActivityWithOptions(NSActivityLatencyCritical | NSActivityUserInitiated,
  reason)` token while high-rate work runs, through `wiredata-timing`'s
  `ResolutionCounter`, which is platform-neutral already. Needs `objc2` and
  `objc2-foundation` as macOS-only dependencies.
- [ ] **Platform pass.** Check `serialport` enumeration on macOS (ports are
  `/dev/cu.*`; prefer `cu` to `tty`), eframe and winit windowing, and that every
  `windows-sys` use stays behind `cfg(windows)`. Fonts are bundled, so no font work
  is expected.
- [ ] **Unattended use on macOS.** Unattended use is supported on Windows and
  Linux (`deploy/README.md`). macOS builds and tests in CI, but nobody has run it
  unattended. After the platform pass:
  - Add a launchd example beside `deploy/linux` and `deploy/windows`.
  - Check that launchd's stop (SIGTERM) reaches `wiredata-stop` and that
    recordings finish their files.
  - Check what `request_receive_buffer` reads back. Only Linux reports
    double the size, which the code halves.
  - Run a soak. `run-soak.sh` reads memory from `/proc`, which macOS lacks.

## Releases (workspace)

- [ ] **Version in the Windows .exe properties.** The window titles and
  `--version` show the Cargo.toml version, but Properties → Details on each
  .exe has no File version or Product version. Embed a Windows version
  resource in the five release programs from a build script in talker and
  listener (the winresource crate is one way, and the same resource could carry
  an icon). It adds a build dependency, so record an ADR and run
  `cargo deny check`.

## Project README

The README covers downloading, the Linux build packages (ADR-003) and building
from source. It does not yet cover:

- [ ] The MSRV policy and `rustup update stable`, per ADR-008.
- [ ] **High-rate timing, for users** (ADR-017 has the rationale):
  - Windows wakes sleeps on a 15.625 ms tick. Talker requests 1 ms resolution
    while any schedule has an interval under 32 ms (`wiredata-timing`). It needs
    no elevation, is released when the last fast channel stops, and is cleaned
    up by the OS even on a kill.
  - **Missed sends:** one missed send is one transmission skipped under the stall
    policy ("fire once, skip the backlog, stay on the grid"). What should have
    gone out is sent plus missed, per channel across its messages.
  - Dropped status and display updates affect only what the Output pane shows;
    sends are never delayed. Missed sends mean the wire cadence itself broke.
  - Practical ceilings: about 100 Hz clean out of the box, and 500 Hz to 1 kHz is
    wake-quantization territory. Measured 2026-07-13 on one Windows 11 machine,
    release build, UDP loopback: 500 Hz missed about 1.5% of grid points, and
    1 kHz missed about a third (about 667 Hz effective), because a 1 ms grid sits
    on the timer's granularity. That is the documented ceiling, not a regression.
    A spin-wait would move it and was considered and not built: one pegged core
    per fast channel.
  - Serial line rate: `payload_bytes × 10 / baud` must fit in the interval. A
    40-byte sentence at 115,200 baud takes about 3.5 ms, so 1 kHz is physically
    impossible whatever the timers do.
  - Minimized windows: both apps opt out of Windows 11's timer throttling at
    startup, so minimized long runs keep cadence. macOS needs App Nap's
    counterpart (above).

## Ideas that need a spec amendment first

- **AIS as a sendable payload.** `nmea0183` builds and parses `!AIVDM`/`!AIVDO` and
  armors the 6-bit payload, but §5.1 lists five message formats and AIS is not one
  of them. Exposing it, as pre-armored bytes or a per-message-type editor, needs an
  amendment (see the ADR-012 context).
- **One-shot manual send.** A troubleshooting workflow wants to fire a payload once
  at a port or endpoint while listener watches the response. Transmit is talker's
  job; an amendment would define how one-shot sends relate to §5.1 and the
  scheduler, and whether it belongs in the CLI, the GUI or both.
