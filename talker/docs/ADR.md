# Architecture Decision Record — Talker
**Project:** talker  
**Version:** 1.23
**Date:** 2026-10-01
**Status:** Accepted

Revision note (2026-10-01) — a TCP peer that has closed:

- **ADR-059 (amended)** — a reply drain that finds the peer has closed the
  connection fails that send with nothing written, and the client reconnects at
  the next retry point.

Earlier revision notes are in [REVISIONS.md](REVISIONS.md).

---

This file records workspace-level and `talker`-application decisions. Decisions
specific to the `nmea0183` library crate live in
[`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md) (currently ADR-009 and OQ-4).
ADR numbers are shared across both files and never reused, so the cross-references
below (e.g. "see ADR-009") remain valid.

---

## What belongs in an ADR

An ADR captures *why* a significant decision was made, not just *what* was decided. It records the context, the options considered, the choice made, and the consequences — so that anyone joining the project later (or the original author six months later) can understand the reasoning without reconstructing it from scratch. An ADR is not a specification; it complements the spec by explaining the decisions that shaped it.

---

## ADR-001 — Workspace structure: split reusable NMEA support from `talker`

**Context:** The NMEA 0183 module was identified early as reusable across other projects. The question was whether to keep it as a module inside the `talker` binary or make it a separate library crate.

**Decision:** The project is structured as a Cargo workspace, initially with `talker` (binary) and `nmea0183` (library). The `nmea0183` crate has no dependency on `talker` and no knowledge of its internals. The workspace now also contains `listener`, a sibling receive crate; it dropped NMEA decoding in its v2.0 stream-only pivot (listener ADR-010). Listener v2.2 reuses `nmea0183` only to construct presentation-only ZDA Mark annotations (listener ADR-025); received bytes remain undecoded.

**Alternatives considered:**
- Single crate with `nmea0183` as an internal module: simpler initially, but makes future extraction painful — splitting a module into a crate after it has grown requires touching import paths throughout the codebase.
- Separate repository: too much overhead for a project at this stage.

**Consequences:**
- `nmea0183` can be published to crates.io independently when ready.
- `nmea0183` must not depend on application-level crates (`anyhow`, `eframe`, etc.).
- Workspace `Cargo.toml` manages shared dependency versions; member crates reference them with `{ workspace = true }`.

---

## ADR-002 — Async runtime: none (`std::thread` + `crossbeam`)

**Context:** `talker` needs to run a UI thread, one talker thread per active connection, and a logger thread concurrently without any blocking the others. Multi-port simultaneous output is a first-class design goal, not a future option.

**Clarification:** "No async runtime" and "multiple OS threads" are entirely independent concepts. This decision rejects cooperative async scheduling (Tokio tasks); it does not restrict the use of OS threads. `talker` uses multiple OS threads throughout.

**Decision:** Use OS threads (`std::thread`) with `crossbeam-channel` for all inter-thread communication. No async runtime (Tokio, async-std, etc.) is used. Each active connection runs in its own dedicated talker thread.

**Alternatives considered:**
- **Tokio:** The dominant async runtime in Rust. Excellent for high-concurrency network servers. Rejected because: (a) `serialport` is a synchronous, blocking API and integrates poorly with async — calls must be wrapped in `spawn_blocking`, which adds overhead and complexity without benefit; (b) `eframe`/`egui` is synchronous; bridging it to an async executor adds friction; (c) `talker` manages a bounded number of connections — the scalability benefits of async do not apply.
- **Multiple application instances:** Running one `talker` process per output port was considered for simplicity but rejected. Each `eframe` instance carries a full GPU-backed rendering stack; this approach is resource-wasteful and unworkable at any meaningful scale.
- **Rayon:** Work-stealing thread pool, designed for data parallelism. Not appropriate for this use case.

**Consequences:**
- Each talker thread is a plain OS thread, easy to reason about and debug.
- Each talker thread waits without spinning by blocking on its command channel with a deadline: `Receiver::recv_deadline(next_fire)` when a message is scheduled (wakes exactly at the fire time, or immediately for a command), and `recv()` when the schedule is idle. A disconnected command channel ends the loop, so a dropped handle cannot leak the thread. (An earlier draft of this ADR specified the `crossbeam-channel` `select!` macro; the single-command-channel deadline receive is simpler and equivalent — there is only one channel to wait on, since the schedule timer is a computed deadline, not a channel.) The loop lives in `core::runner`, shared by the CLI and GUI (spec §2.2).
- Each active connection has its own dedicated channel pair with the UI thread.
- `core::channel` manages a collection of channel instances from the initial implementation; there is no single-channel shortcut to be refactored later.
- The number of simultaneous connections is bounded by available system resources (serial ports, network sockets), not by any artificial limit in the software.

---

## ADR-003 — GUI framework: egui / eframe

**Context:** `talker` needs a cross-platform GUI (Windows, macOS, Linux) that is utilitarian, data-dense, and maintainable by a small team.

**Decision:** Use `egui` (immediate-mode GUI library) via `eframe` (the official native/web framework wrapper).

**Alternatives considered:**
- **iced:** Elm-architecture (message-passing) GUI. More idiomatic for Rust's ownership model in some ways, but less mature ecosystem and steeper learning curve for a data-heavy control panel UI.
- **Tauri:** Web-based UI layer over a Rust backend. Excellent native look, but introduces a JavaScript/HTML/CSS front-end layer and a more complex build process — unjustified for a utilitarian engineering tool.
- **Native platform bindings (gtk-rs, winapi):** Platform-specific; would require separate implementations per OS.

**Consequences:**
- `egui` does not produce a native-looking UI. This is acceptable for a utilitarian engineering tool where information density and simplicity matter more than visual integration.
- `eframe` has significant compile-time dependencies (wgpu, winit, image crates). Compile times will be longer than a CLI-only binary.
- The `persistence` feature of `eframe` is used for GUI state save/restore.
- Linux users need system packages installed for the graphics stack (`libxcb`, `libxkbcommon`, etc.). This should be documented in the README.

---

## ADR-004 — Error handling strategy: `thiserror` in library, `anyhow` in application

**Context:** Rust requires explicit error handling. Two popular approaches exist for reducing boilerplate.

**Decision:** 
- `nmea0183` uses `thiserror` to define a typed public error enum (`NmeaError`). Callers can match on specific variants.
- `talker` uses `anyhow` for application-level error propagation. Errors are wrapped with `.context()` to produce rich diagnostic messages for logging and display.

**Alternatives considered:**
- `anyhow` everywhere: Loses the ability for callers of `nmea0183` to programmatically distinguish error types (e.g., `ChecksumMismatch` vs `InvalidField`). Not appropriate for a reusable library.
- `thiserror` everywhere: More boilerplate in application code where callers don't need to distinguish error types. Not worth the cost in `talker`'s own modules.
- `Box<dyn Error>`: Lowest common denominator. No structured context, no ergonomic `?` chaining with wrapping. Rejected.

**Consequences:**
- The boundary between `nmea0183` and `talker` is where `NmeaError` gets wrapped into `anyhow::Error` via `?`.
- New error variants in `nmea0183` are a minor breaking change for `nmea0183`'s public API — adding `#[non_exhaustive]` to the error enum is recommended before any external publication.

---

## ADR-005 — Profile format: TOML

**Context:** User profiles (connection params, schedule, data config) must be saved, loaded, edited, and potentially version-controlled outside the program.

**Decision:** Profiles are serialized to TOML using `serde` + the `toml` crate.

**Alternatives considered:**
- **JSON:** Machine-readable but noisy for human editing (mandatory quotes on keys, no comments).
- **YAML:** Human-friendly but has well-known parsing footguns (the Norway problem, implicit type coercion).
- **INI/custom format:** Would require writing a custom parser. No benefit over TOML.
- **Binary (bincode, messagepack):** Not human-readable. Violates the explicit requirement.

**Consequences:**
- TOML files can be commented, diffed, and committed to version control.
- Profile structs must derive `serde::Serialize` and `serde::Deserialize`.
- Adding new fields to a profile struct requires a migration strategy (use `#[serde(default)]` for backwards compatibility).

---

## ADR-006 — Logging: `tracing` + `tracing-subscriber`

**Context:** `talker` needs structured logging to both a rotating file and
stdout (CLI), and to a GUI Log pane plus optional file (GUI).

**Correction (2026-08-11):** The original decision named only three severities
and assigned CLI filtering to `RUST_LOG`; neither describes the implemented
contract. The decision below replaces those two statements.

**Correction (2026-08-12):** The pane's bounded handoff previously discarded
entries silently, and the file's loss count did not leave evidence inside saved
output. The decision now treats pane and file delivery as two independent lossy
observers, makes each loss visible, and attempts file-gap evidence without
sending a log event through the route that is already overloaded. It also fixes
the GUI log directory and separates opening that folder from both the UI and
file worker.

**Decision:** Use the `tracing` facade with `tracing-subscriber` for TRACE,
DEBUG, INFO, WARN, and ERROR dispatch. One reloadable threshold wraps the complete
sink stack, so an admitted event is offered consistently to every installed
destination. Each bounded destination may still omit that event independently
if its receiver cannot keep up. CLI takes the threshold from the profile's
`[logging].level`; it does not read `RUST_LOG`, and its stdout and file
destinations are fixed at launch.

The GUI installs a permanent pane destination and a runtime-controlled file
destination at subscriber initialization. **Detail** changes the shared
collection threshold, including file contents. The five **Show in pane** switches
filter retained pane rows only; they neither reconfigure capture nor affect the
file. These controls are session-local, begin at Info/all shown/file off, and are
not applied from profiles.

The pane uses its own bounded, non-blocking handoff. A full handoff increments a
persistent, session-cumulative loss count and wakes the GUI; normal receiver
teardown during shutdown is not overload and is not counted. Only entries that
arrive through this path enter pane history or its derived per-channel
INFO/WARN/ERROR tallies, so the visible loss count explicitly qualifies both.
All five levels share one chronological newest-2,000-entry history, and the
Show switches apply after retention. Hidden rows therefore consume the same
capacity as visible rows. Pane processing is bounded per frame and carries any
accepted backlog into later frames.

The GUI file destination is switched by control messages, not by rebuilding the
subscriber. One dedicated worker owns file open, write, flush, and close. Event
producers use a separate bounded non-blocking queue. Saturation increments a
session-cumulative visible count. Within each enabled file session, the worker
attempts a direct plain gap marker only after writing entries accepted before
the known loss. The marker never re-enters `tracing`, and no producer waits for
it. Disable and shutdown drain accepted entries, attempt the final marker, then
flush. File-session generation boundaries keep late entries and losses out of a
later destination. A marker remains best-effort because a destination write
failure can prevent the evidence itself; while the GUI remains open, that uses
the ordinary visible file-failure path and the session loss count remains.

When the platform supplies a local-data directory, the GUI file destination is
its fixed `talker/logs` location. Otherwise file logging and **Open folder** are
unavailable. Open folder creates the directory if needed and invokes the
platform folder opener on a separate, single-in-flight helper thread. It waits
there for the opener's result so a non-zero exit remains visible and retryable.
The action never enables file logging and never occupies the UI or file-worker
thread. GUI files rotate daily by time and have no retention or disk-use bound.

**Alternatives considered:**
- **`log` + `env_logger`:** The classic Rust logging pair. Simpler but less flexible — `tracing` supports structured fields and spans, which will be useful for correlating log events with specific connections or send operations.
- **`slog`:** Structured logging with explicit loggers passed through the call stack. More explicit but significantly more verbose.
- **Rebuild or reinitialize the subscriber when the GUI file box changes:** A
  process can have only one global subscriber, and replacing it would race active
  channels. A permanently installed gated destination keeps the control local.
- **Write files on the UI/send threads, or use an unbounded queue:** Direct I/O
  can stall cadence or repaint; an unbounded queue trades that for uncontrolled
  memory growth. A bounded lossy observer preserves application work and makes
  the loss visible.
- **Make either observer reliable or block producers until space exists:** This
  would let diagnostic work delay the send path it is meant to observe. The
  decision instead makes incomplete evidence explicit at each destination.
- **Emit file-gap markers as tracing events:** Such a marker would traverse the
  same bounded file route whose loss it reports, could itself be dropped, and
  would also create an unrelated pane row. The file worker writes it directly.
- **Keep separate pane histories per severity:** Reserved capacity would change
  chronological retention and add policy without measured need. One shared
  history keeps visibility reversible for every row that is still retained.
- **Open or choose the directory on the UI or file-worker thread:** Folder
  creation and desktop launch can block or fail independently of file writes.
  The separate helper preserves both boundaries. Directory choice, retention,
  and size-based rotation remain separate product decisions.

**Consequences:**
- The GUI Log pane is implemented as a `tracing_subscriber::Layer` that captures
  log events and pushes them to the UI through a bounded `crossbeam-channel`.
- GUI file control can change at runtime without file I/O on the UI or channel
  threads. The same Detail threshold governs the pane, console, and file; Show in
  pane cannot be used as an accidental recording filter.
- Pane and file delivery are independently best-effort. Each has its own visible,
  session-cumulative loss count; loss in one does not establish loss in the
  other. Neither waits for queue capacity or file I/O; queue-full bookkeeping
  uses only short-lived in-memory synchronization.
- When its marker write succeeds, saved output identifies known file-delivery
  gaps after the earlier accepted backlog. The visible session count remains
  the persistent in-process disclosure when it does not; enabled-session
  boundaries prevent a stale entry or loss from being reassigned after disable
  and re-enable.
- The fixed folder can be created and opened without enabling logging or doing
  I/O on the UI thread. Time rotation deletes no old files; directory choice,
  retention, size rotation, and a disk-use bound remain separate work.

---

## ADR-007 — Checksum/CRC: `crc` crate

**Context:** `talker` must compute XOR, CRC-8, CRC-16/CCITT, CRC-16/MODBUS, and CRC-32 checksums for outgoing data. The `nmea0183` crate also computes NMEA XOR checksums.

**Decision:** Use the `crc` crate for all CRC computations. Implement the NMEA XOR checksum directly in `nmea0183` (it is a trivial one-line fold, has no external dependency, and keeps the library self-contained).

**Alternatives considered:**
- `crc32fast`: Only CRC-32. Too narrow.
- `crc16`: Unmaintained.
- Rolling our own: Unnecessary given the quality of the `crc` crate.

**Consequences:**
- `nmea0183` has no dependency on the `crc` crate — its checksum is a byte XOR, implemented inline.
- `talker`'s `core::message` module uses `crc` for the general checksum feature.
- The `crc` crate uses a const-generic algorithm table approach; algorithm selection is a compile-time or runtime parameter depending on usage pattern.

---

## ADR-008 — Minimum Supported Rust Version (MSRV): 1.95

**Context:** `talker` depends on several crates whose own MSRV has crept upward over time (`clap` 4.6 requires 1.85, `crc` 3.4 requires 1.83, `eframe`/`egui` 0.34 targets recent stable). Pinning to an older Rust version forces the workspace to also pin older versions of these crates, which compounds with every release. The team has no constraint requiring older toolchains.

**Decision:** MSRV is set to Rust 1.95 (current stable as of April 2026) in `[workspace.package]`. Crates in the workspace track the latest stable Rust release rather than supporting an extended back-compatibility window.

**Alternatives considered:**
- **MSRV 1.75 (the original choice):** Was a reasonable "recent baseline" in 2025 but is now ~2.5 years old. Keeping it forced version pins on `clap`, `crc`, and likely `eframe`/`egui` — a moving maintenance burden that produced no benefit, because no team member or known user requires an older toolchain.
- **MSRV at the oldest version that builds with current crate versions:** Saves nothing in practice; the workspace tracks current stable either way, and a precise "minimum" figure is overhead to maintain.
- **N-2 or N-6 month policy:** Appropriate for widely-published libraries serving cautious downstream users. Unnecessary for a workspace whose sole consumer is its own developers.

**Consequences:**
- Users must run `rustup update stable` before building. This is documented in the README.
- Every dependency can be specified by major version only (`clap = "4"`, `crc = "3"`, etc.) and resolved to the latest compatible release.
- CI tests against stable. The declared MSRV is bumped to match current stable on each Rust release rather than maintained as a separate floor.
- This MSRV applies to the workspace and the `talker` binary. The MSRV policy for the `nmea0183` library — which is intended for crates.io publication and may want a looser, more downstream-friendly MSRV — is deferred to a future ADR when publication approaches.

---

## ADR-009 — Talker ID and sentence type extensibility in `nmea0183`

Moved to [`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md) — it is a decision internal to the `nmea0183` library. The ADR-009 number is retained there.

---

## ADR-010 — Profile and GUI state separation

**Context:** Profiles need to be compatible between CLI and GUI. GUI also needs to save window geometry and layout. The question was whether these should share a format and file, or be kept separate.

**Correction (2026-08-11):** Window geometry was later excluded because restoring
it after first paint caused a visible second frame and could resurrect unusable
dimensions. The GUI does not persist the runtime logging controls added under
ADR-006. The separation decision remains; the exact contents below replace the
original illustrative list.

**Decision:** Profile data and GUI state are strictly separated into two different files with two different purposes:

- **Profiles** — TOML files containing channel, message, schedule, checksum, and
  CLI launch-logging configuration. Channel and message configuration is fully
  compatible between CLI and GUI. Profiles live in a documented directory and
  have a public schema so users can edit them by hand.
- **GUI state** — `eframe`'s built-in persistence mechanism in the platform
  config directory. Talker explicitly stores theme and current/recent profile
  paths; egui retains presentation memory such as zoom. It never contains channel
  or message configuration and is never loaded by the CLI. Window geometry and
  the GUI's Detail, Show in pane, and Log file controls are not retained.

**Alternatives considered:**
- Single file for everything: Simpler on the surface, but means the CLI must parse and ignore GUI-only fields, and GUI-only concepts leak into the profile schema. Rejected.
- TOML for GUI state as well: Would require reimplementing what `eframe` already provides for free. Not justified.

**Consequences:**
- Profile structs must not contain GUI-only fields. Runtime logging controls start
  from their GUI defaults on every launch and profile loading cannot change them.
- Serde's default unknown-field handling lets the CLI ignore unrecognized fields
  in a hand-edited profile. GUI-only state is never written there in the first
  place.

---

## ADR-011 — CLI multi-channel model

**Context:** The GUI supports multiple simultaneous connections as a first-class feature. The question was whether CLI mode should be one connection per process or support multiple connections in one process.

**Decision:** CLI mode supports multiple simultaneous channels in a single process, using the same `core::channel` collection and per-channel talker thread model as the GUI. The primary mechanism is `--profile`, which may define one or many channels. Ad-hoc multi-channel via repeated CLI flags is deferred.

**Alternatives considered:**
- One channel per CLI instance: Simple to implement, but requires users to manage multiple terminal sessions and processes for multi-port work. Inconsistent with the GUI model and defeats the profile system. Rejected.
- Repeated channel flags for ad-hoc multi-channel operation: Desirable long-term but adds CLI parsing complexity. Deferred to a future iteration; `--profile` covers the primary use case.

**Consequences:**
- `talker --profile <name>` is the canonical way to launch multi-channel sessions from the CLI.
- The CLI and GUI share identical `core` behavior for channel management. There is no CLI-specific channel limit or shortcut.
- stdout echo in multi-channel CLI mode outputs data from all channels interleaved. Each line is prefixed with a channel identifier to allow filtering.

---

## ADR-012 — Binary field types

**Status:** Superseded by the spec v2.0 message-format model (see ADR-015 context and the message-format summary in [`AGENTS.md`](../../AGENTS.md)). Spec v2.0 removed the typed-binary-field concept: arbitrary byte sequences are now entered in **Hex** format, and structured data is built through the UTF-8/UTF-16/ASCII/NMEA formats. `core::data` and the `BinaryField` enum were never carried into the v2.0 codebase. This ADR is retained for historical context.

**Context:** The spec originally deferred the exact set of binary field types. At the time, a concrete decision was needed before the planned data-construction module could be implemented.

**Decision:** Binary data is constructed as an ordered sequence of typed fields. Supported types are: `u8`, `u16`, `u24`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`, `f32`, `f64`, and raw bytes (arbitrary hex). Byte order is selectable per field — big-endian (default) or little-endian.

**Rationale for `u24`:** Three-byte unsigned integers appear frequently in sonar, audio, and oceanographic equipment. Without `u24`, users must construct them manually from raw bytes, which is error-prone. The implementation cost is low.

**Rationale for big-endian default:** The majority of marine and survey instruments use big-endian (network byte order). Defaulting to big-endian reduces misconfiguration for the primary target audience.

**Consequences:**
- The planned data-construction module would have implemented a `BinaryField` enum with one variant per type plus `RawBytes(Vec<u8>)`.
- Each field carries a `ByteOrder` enum (`BigEndian` | `LittleEndian`).
- `u24` requires manual encoding (write the 3 most-significant bytes of a `u32`); no standard Rust primitive maps directly to it.
- Binary message definitions are saved in profiles as an ordered list of field descriptors.

---

## ADR-013 — Profile schema versioning and migration strategy

**Context:** Profile structs will gain new fields as `talker` evolves. Old profile files must load cleanly in newer versions of the program, and users must be warned rather than silently harmed when loading a profile from a newer version.

**Decision:** A two-layer strategy:

**Layer 1 — `#[serde(default)]` on all profile fields.** Every field has a sensible default. Old profiles missing newly added optional fields load without error. This handles the common case with zero migration code.

**Layer 2 — `version: u32` in the profile header, starting at `1`.** Load behavior:
- Version matches current: load normally.
- Version is older: run a versioned migration function, fill in defaults, log a warning, optionally rewrite at the new version.
- Version is newer than the binary understands: refuse to load, warn the user.

The version number increments only on breaking schema changes that `serde(default)` cannot handle alone, keeping migration functions minimal.

**Consequences:**
- All profile structs annotated with `#[serde(default)]` from the first commit.
- A `PROFILE_SCHEMA_VERSION: u32` constant is defined in `core::profile` and checked on every load.
- Migration functions live in `core::profile::migration` as a match on `(from_version, current_version)`.
- The `#[non_exhaustive]` attribute is used on profile enums to prevent external code from exhaustively matching on them, enabling future variant addition without breaking changes.

**Update (v2.0, 2026-05-22):** The spec v2.0 upgrade restructured the profile schema (nested channels, each owning an interface and a list of messages). Rather than write a v1→v2 migration, the project chose a **clean break**: `CURRENT_VERSION` is `2`, and `Profile::load` refuses any profile whose version differs from it — newer versions are rejected as unsupported (Layer 2 as designed), and **older versions (v1) are also rejected**, with an error instructing the user to recreate the profile. The reasoning: v1 had no released users, so migration code would have been dead weight maintained forever. Layer 1 (`#[serde(default)]` on every field) still stands and handles all *additive* schema changes within v2. The `core::profile::migration` module was therefore never created; when a breaking v3 change arrives, a migration step and version-downgrade handling are reinstated at that point.

**Update (2026-09-30):** Schema 3 (ADR-057) is again a clean break rather than a migration: nothing is deployed, so there are no version-2 profiles to carry forward. ADR-062 narrows Layer 1: a missing field still takes its default, but an unknown key or a missing `version` is now refused.

---

## ADR-014 — `talker` as a library plus a thin binary

**Context:** The `talker` crate began as a pure binary (`main.rs` and a module tree). All application logic lives in `core`; `cli` and `gui` are thin interface layers. During the spec v2.0 upgrade, removing the workspace-wide `#![allow(dead_code)]` exposed nine false-positive dead-code errors: constructors and helpers in `core` that are exercised by unit tests but not yet called from `main`. In a binary crate, anything not reachable from `fn main` is "dead" — even when it is part of a module's intended public API and is under test.

**Decision:** `talker` is both a library and a binary. `src/lib.rs` declares `pub mod cli; pub mod core; pub mod gui;`. `src/main.rs` is a thin shim that calls into the library. The library's public items are part of an exported API, so the compiler no longer flags tested-but-not-yet-wired `core` code as dead.

**Alternatives considered:**
- **Keep `#![allow(dead_code)]`:** Silences the false positives but also silences *genuine* dead code for the life of the project. Rejected — the lint is worth keeping honest.
- **Per-item `#[allow(dead_code)]`:** Scatters annotations across `core` and requires adding/removing them as `main` wiring catches up. Noisy and easy to leave stale.
- **`#[cfg(test)]`-only constructors:** Would mean test-only code paths diverge from production ones. Rejected — tests should exercise the real API.

**Consequences:**
- `cargo test` can address `core` modules directly through the library crate (`use talker::core::...`), and integration tests in `talker/tests/` link against the library.
- The crate compiles with the default dead-code lint active; genuinely unused code is caught.
- `main.rs` contains no logic beyond argument dispatch — consistent with the existing rule that `cli`/`gui` are thin layers.

---

## ADR-015 — Code pages: hand-written tables, no `encoding_rs`

**Context:** Spec §5.2 requires four single-byte code pages for ASCII-format messages — CP437, Windows-1252, Mac OS Roman, and ISO-8859-1 — available on every host OS regardless of the platform's own locale. Each is a fixed mapping between byte values 128–255 and Unicode scalar values.

**Decision:** Each code page is implemented as a hand-written static table in `core::message::codepage`, generated from the authoritative Unicode Consortium mapping files. No transcoding crate is taken as a dependency.

**Alternatives considered:**
- **`encoding_rs`:** The standard Rust transcoding crate (the encoding engine from Firefox). It is correct and well-maintained, but it is a heavy dependency — it carries the full WHATWG Encoding Standard: every legacy multi-byte CJK encoding, big lookup tables, and a streaming decoder API. `talker` needs four *single-byte* maps and a one-shot encode. Pulling in the whole crate for that is disproportionate: it inflates compile time and binary size and widens the dependency surface for no functional gain.
- **`codepage`/`oem_cp` and similar smaller crates:** Lighter than `encoding_rs`, but still an external dependency and an API to track, for tables that are trivially expressed inline and never change.
- **OS-provided conversion APIs:** Platform-specific, and would violate the spec requirement that all four code pages work identically on every host OS.

**Consequences:**
- `core::message::codepage` owns four `[char; 128]`-style tables; encoding is a direct lookup, decoding (for the display pane) is a reverse search.
- The tables were transcribed from the Unicode Consortium `.TXT` mapping files, not from memory; unit tests pin representative code points so a transcription error cannot pass silently.
- ISO-8859-1's 128–255 range is the identity map onto U+0080–U+00FF, so it needs no table — it is handled as a special case.
- Adding a fifth code page later is a self-contained table addition with no dependency change.

---

## ADR-016 — `wiredata-ui`: a shared GUI-chrome crate (fonts, palette, base style)

**Context:** The GUI merge makes `talker` adopt `listener`'s look and feel (channel list + detail layout, listener's light theme, a dark theme for both). Before this decision each app carried its own copies of the same chrome: overlapping bundled Noto font files, two divergent visual styles, and duplicated formatting helpers. A deliberate "the two apps look identical" goal turns that duplication into guaranteed drift.

**Decision:** A fourth workspace crate, **`wiredata-ui`** (internal, `publish = false`), owns the GUI **chrome** only: the bundled font stack and its fallback chains (`fonts`), the named severity/status color palette with `LIGHT` and `DARK` const instances (`palette`), the base widget visuals for both themes plus the shared non-visual style tweaks (`style`), and small pure formatting helpers (`format::human_bytes`). It depends **only on `egui`** — never on `talker`, `listener`, `eframe`, or any runtime crate. App-specific widgets, layouts, and view-models stay in each app. Listener's counterpart decision is its ADR-019.

**Scope rule (what belongs here):** a piece moves into `wiredata-ui` only when it is (a) purely presentational, (b) meaningfully identical across both apps, and (c) egui-only. Anything entangled with an app's runtime, config schema, or view-model stays out. A shared *runtime* crate (`wiredata-core`) was considered during the talker review and rejected — config/timestamp/profile duplication is small and stable, and the apps' I/O shapes are opposite; the chrome is the one place where duplication would actively grow.

**Consequences:**
- The font assets moved from `listener/assets/fonts/` to `wiredata-ui/assets/fonts/` (one copy; talker's private subset — including the Cascadia control-pictures subset font — was deleted, since the full Cascadia face in the shared stack covers U+2400–U+243F).
- Talker's local visuals/font installers were replaced by `wiredata_ui::{fonts::install_fonts, style::install_visuals, style::apply_style_tweaks}`. Talker's light theme is now listener's grey-backdrop look; talker's original dark values seeded the shared dark theme. Both apps now expose and persist the same dark/light toggle.
- Talker also adopts listener's window-startup lessons: `persist_window: false` (no post-show geometry jump) and a minimum window size.
- Listener's `gui/{fonts,theme}.rs` and `widgets/format.rs` became thin re-exports, so its call sites are unchanged.
- The crate has no `docs/` folder; its decisions live in the two app ADR series (this entry and listener ADR-019).

**Follow-up (selected-channel continuity).** The shared crate also owns the purely
presentational selected-channel card, compact tab, row text and historical-count
emphasis, and the card-to-page connector. Each app supplies only a
selected card rectangle, its actual scroll viewport, and (for an expanded list) the
panel id. One shared painter owns the complete page-edge stroke and mirrors egui's
resize hover/drag stroke; it draws the tab detour only when the card and both turns
fit inside the viewport, otherwise it draws an ordinary straight divider. This
prevents partial off-screen connectors and keeps both apps' selection hierarchy and
resizer feedback identical. Lifecycle glyphs and live faults remain app-owned because
they carry runtime meaning rather than chrome.

**Follow-up (2026-07-16, chrome dedup).** The remaining duplicated accessors merged
into the crate: `palette::active(ui)` is the one theme-aware palette accessor
(replacing talker's `theme_palette`, `selection`'s private copy, and listener's
`gui/theme.rs` process-global mirror — listener's pure helpers now take `&Palette`
like talker's `lifecycle_indicator`), `style::theme_toggle_button` owns the shared
◐/◑ header toggle, `selection::{severity_counts_line, last_error_line}` own the two
value-only channel-row lines, and `install_chrome(ctx)` bundles the three-call
chrome install. Listener's re-export shims (`gui/fonts.rs`, `widgets/format.rs`,
`gui/theme.rs`) were deleted in favor of direct `wiredata_ui` paths — the shims
predated the merge settling and had become pure indirection.

---

## ADR-017 — High-resolution OS timer scope for high-rate schedules

**Context:** The runner waits for its next fire with a deadline-bounded blocking receive (`recv_deadline`, ADR-002). On Windows a parked thread with a timeout wakes on the system scheduler tick — **15.625 ms** by default — so a 10 ms interval (100 Hz) can never be honored: every wake lands more than one interval late, the stall policy (spec §8.1) skips the backlog to stay on grid, and ~36% of sends are skipped with ~15.6 ms observed spacing. 10 Hz absorbs the same jitter invisibly, which is why the problem only appears at high rates.

**Options:** (a) request 1 ms resolution process-wide for the whole app lifetime — simple but pays an idle power cost and is against Microsoft guidance; (b) hybrid wait (sleep short, spin the last ~2 ms) — precise to sub-ms but burns a core per fast channel; (c) request 1 ms resolution **only while a schedule actually needs it**.

**Decision:** (c). Talker's policy remains in `core::timing`; ADR-038 later moves
the cross-application OS mechanism into `wiredata-timing`. A refcounted RAII guard
(`high_resolution()` / `HighResolutionGuard`) wraps
`timeBeginPeriod(1)`/`timeEndPeriod(1)`: the first holder raises the request, the last
drop releases it, and the count and OS call share one lock so concurrent
acquire/release cannot reorder the pair. Each runner re-evaluates
`Schedule::active_cadence()`'s shortest interval against `HIGH_RATE_THRESHOLD`
(32 ms = two default ticks)
every loop pass, so `SetInterval` acquires/releases mid-run. Both entry points (GUI
funnel, CLI run) additionally opt out of Windows 11's minimized-window timer
throttling via `SetProcessInformation(ProcessPowerThrottling,
IGNORE_TIMER_RESOLUTION)` — otherwise a minimized long soak silently falls back to
15.6 ms wakes. Everything is a no-op off Windows.

**Consequences:**
- 100 Hz sends hit cadence (wake jitter ~1–2 ms ≪ 10 ms interval); the practical ceiling moves to roughly 500 Hz–1 kHz, beyond which option (b) would be needed.
- No elevation required; per-process since Windows 10 2004, so other processes are unaffected. The kernel releases the request on process death (any kind), so a leaked guard cannot outlive talker — the RAII release is about dropping the power cost early, not correctness.
- The GUI's "Missed sends" readout is the acceptance signal: it should stay 0 at 100 Hz on an otherwise healthy interface.
- ADR-034 later extends the request to bounded final-deadline windows for an
  explicitly Precise slow schedule. Standard retains the automatic high-rate policy
  decided here.

---

## ADR-018 — Talker telemetry split: counters, sampled payloads, edge-triggered errors

**Status:** Accepted 2026-07-11 (proposed by the 2026-07-11 external review).

**Context:** Every send emitted a payload-bearing `TalkerStatus::Sent` over the
per-channel status channel; the GUI maintains a front-drained 200-item display Vec and
rebuilds the complete output string per repaint. Correctness was protected (bounded
queue, drop-and-count, `dropped_statuses` self-correction — sends are never delayed),
but the per-send CPU/allocation cost scales with rate and becomes load-bearing in the
500 Hz–1 kHz territory ADR-017 opened: at 1 kHz the runner allocated and shipped a
thousand payload Vecs a second that the GUI mostly discarded.

**Decision:** The status protocol has three lanes with different cadences, and the
owner picks the payload policy via a named `ObserverPolicy` passed to the runner:

1. **Periodic counters** — `TalkerStatus::Counters` (total/per-message counts,
   bytes, missed sends, dropped statuses, cumulative send-failure episodes, and
   the current optional send/reopen error; **no payload**), emitted at most once
   per `counter_interval` (default 200 ms ≈ 5 Hz), checked on the send path. A
   final `Counters` is emitted when the runner stops, so totals and final failure
   state are exact at rest. ADR-056 defines the current-state repair boundary.
2. **Sampled display payloads** — `TalkerStatus::SendSample` (payload-bearing),
   newest-per-interval: the first send after `sample_interval` elapses carries its
   payload (default 100 ms ≈ 10 Hz). The Output pane shows a live, bounded sample
   instead of every wire message.
3. **Immediate errors** — `ConnectionError` / `SendRecovered` / `OpenFailed`
   (edge-triggered per ADR-055) are never rate-limited. The running failure and
   recovery edges remain best-effort for low latency; periodic state repairs
   either one if the observer queue discarded it. Initial-open failure occurs
   before an armed run and therefore has no counter repair.

`Sent` is **removed**, replaced by lanes 1+2. CLI `--echo` passes
`ObserverPolicy::every_send()` (every send emits a `SendSample`) — the one consumer
that genuinely wants every payload keeps it, explicitly.

**Consequences:**
- Per-send cost at any rate is a counter bump; allocations for observers happen at
  the sample cadence, not the send cadence. The 200-item display Vec fills at ≤10
  items/s regardless of send rate.
- The self-correcting counter scheme survives: totals, the failure-episode count,
  and current send/reopen state ride in every `Counters`, so a dropped failure or
  recovery edge is corrected by the next one; drop-and-count is unchanged. This
  repairs observer state, not a missing GUI log line or its severity tally.
- Status-queue pressure drops by construction (≤ ~15 statuses/s/channel steady-state
  vs. one per send), making `dropped_statuses` a true anomaly signal.
- The GUI's per-send "Output" completeness is gone by design at high rates — the pane
  is a sample, labeled as such; the wire remains exact (that's what recording and the
  CLI `--echo` are for).
- The scheduler/spec "status" wording needs a spec-pass update (see TODO, no bump
  alone).

---

## ADR-019 — Core `TalkerSupervisor` owning the channel collection

**Status:** Accepted 2026-07-11 (proposed by the 2026-07-11 external review).
Implementation deliberately **sequenced after ADR-018** — the telemetry lanes change
the same status plumbing the supervisor will own, and moving them once is cheaper.
The open points below are settled at implementation time, not re-litigated.

**Context:** The spec places channel collection and management in `core`
(`core::channel` manages a collection from day one), but in practice channel
lifecycle, runner-thread collection, draining, cumulative counters, and observer
policy live in `gui/mod.rs` (~1.5 k lines) — the GUI is not the thin layer §127/AGENTS
call for, and the CLI cannot reach that logic (part of why CLI parity lags).

**Decision (implemented 2026-07-11):** `core::supervisor::TalkerSupervisor` owns
index-stable channel slots — runner threads, command/status channel pairs, draining
buckets, and per-channel `ChannelTelemetry` (counts, queue occupancy, errors, the
error banner). The open points settled as:

- **API shape: blocking methods, no command enum.** `start`/`stop`/
  `update_interface`/`set_interval` are direct calls returning a `CommandOutcome`
  (`Enqueued`/`QueueFull`/`NotRunning`); commands that cannot be enqueued are recorded
  in telemetry. Successful enqueue is not proof of execution; ADR-021 adds correlated
  execution results for live mutations. No supervisor thread, matching
  ADR-002 — everything runs on the caller's thread and never blocks (`join_all` is
  the one deliberate exception, exit-path only).
- **Drain cadence: caller-owned poll.** `poll()` non-blockingly folds every runner's
  status lanes (ADR-018) into the telemetry and returns the payload samples; the GUI
  calls it each frame, woken by the notify callback it installs via `set_notify`.
- **Sequencing:** landed after ADR-018 as planned; the lanes moved once.
- **Tests:** the extracted lifecycle is unit-tested in core against mock interfaces
  (start/poll/stop reap + exact totals at rest, command-failure surfacing, restart
  reset + predecessor joins, slot removal), which the GUI-embedded version never was.
- **CLI adoption is deferred to the ad-hoc CLI work.** The current CLI is a one-shot
  headless run whose blocking status funnel is the right shape for `--echo`
  (lowest-latency payload delivery); wrapping it in a poll loop would only add
  latency. The supervisor is the base for the *future* interactive CLI lifecycle
  (the parity item in TODO.md); today both layers consume the same runner API.

**Consequences:**
- The GUI holds pure view-state (drafts, displays, rates, selection, log tallies)
  and reads `telemetry(i)` when rendering; ~10 parallel per-channel `Vec`s deleted.
- Two behaviour improvements fell out: a stopped runner's status receiver is kept
  until its thread exits, so ADR-018's final `Counters` lands and totals read exact
  at rest (previously the tail was dropped with the receiver); removed channels'
  runners land in an orphan bucket and are reaped, not silently detached.
- The status-bar "Errors" tally is now per-run (each channel's count resets when it
  starts, like the send counts and log tallies) instead of per-app-lifetime.
- `STATUS_QUEUE_CAP` moved to `core::supervisor` (re-exported to the GUI for the
  detail-header readout).

## ADR-020 — Stable channel identity (`ChannelId`)

**Status:** Accepted 2026-07-12 (external review round 2, "positional identity").

**Context:** Channels were identified everywhere by **slot index**: the runner
thread captured its index at spawn and stamped it into every `TalkerStatus` and
every structured `channel = n` tracing field; the GUI tallied per-channel log
counts in a positional `Vec` keyed by that field. Slots shift down when a channel
above is removed — but a running runner keeps its captured number, so after a
removal its log events (and its stale text, "channel 3 …") were attributed to
whichever row slid into its old position. The listener solved the same problem
with runtime-minted stable ids from day one; talker inherited the positional
scheme from its single-channel-era plumbing.

**Decision:** Identity and position are separated:

- **`core::channel::ChannelId`** — a process-unique, monotonic id (`u64`,
  1-based), minted by `TalkerSupervisor::push_slot` when the slot is created and
  carried by the slot for its whole life. Runtime-only: never persisted (profiles
  identify channels by position and name), never reused.
- **`RunnerIdentity { id, label }`** is what a runner is started with (ADR-019's
  `start` gained a `label` argument): the id goes into every `TalkerStatus`
  variant and every structured `channel` tracing field; the **label** — the
  custom channel name in quotes, else the 1-based position at start time — is
  used only in log *text*, frozen for the run (a rename shows on the next start).
  Attribution never rides the text; it rides the id.
- **The GUI keys its log tallies by id** (`HashMap<ChannelId, LogCounts>`) and
  maps row → id at render via `TalkerSupervisor::channel_id(i)`; removal drops
  exactly the removed channel's tally.
- **Positional indices remain for "which row right now":** supervisor slot
  methods, display-pane routing (`PayloadSample::slot` — the receivers travel
  with their slots, so the *current* index at drain time is correct), and the
  CLI's `--echo` tag (`ch0:` unchanged, via an id→position map).

**Consequences:**
- Log counts and error/status attribution can no longer land on the wrong row
  after a removal; a runner below a removed channel keeps counting into its own
  row. Pinned by `channel_ids_are_stable_across_slot_removal` and the id
  assertions in the runner status tests.
- Log text is unchanged in the common case ("channel 3 running") and better for
  named channels ("channel 'GPS' running"); supervisor lines before a first
  start fall back to the id form ("channel #4").
- `TalkerStatus` stays self-describing under the CLI's shared funnel with an
  identity that survives any future removal/reorder feature there.

## ADR-021 — Correlated command execution and applied runtime state

**Status:** Accepted 2026-07-12 (deep reliability review).

**Context:** ADR-019 reported whether a live interface or interval command entered
the runner queue, but the GUI immediately copied the requested interface into the
profile and treated it as applied. `UpdateInterface` can still fail while the runner
keeps its previous interface, and `SetInterval` can reject an invalid message index.
The UI could therefore claim runtime state that never took effect. A later unrelated
successful command also cleared the one shared command-error string, hiding the
unresolved failure. Listener's lifecycle work established the applicable rule:
observable state is reconciled from runtime facts, not from submitted intent.

**Decision:** Live mutations carry a process-unique `CommandId` and a scoped
`CommandTarget` (`Interface` or one message interval). The immediate supervisor result
is explicitly an enqueue outcome. After executing a command, the runner emits a
correlated `Applied` or `Failed` result on a dedicated reliable control channel,
separate from ADR-018's sampled/drop-and-count observer queue. The supervisor retains
each pending effect and accepts a completion only when its id and target match.

The start-time interface is likewise not considered applied until the runner reports
that `open` succeeded. A successful interface update replaces the supervisor's
applied-interface baseline; a failed update leaves the previous baseline intact and
states that fact in the channel banner. Command failures are retained independently
per target, and only a later successful command for that same target resolves one.
The supervisor commits an effect to its applied-runtime baseline only after the
correlated success; the profile remains desired/persisted state.

**Consequences:**
- Queue acceptance and runtime application are no longer conflated in APIs, comments,
  telemetry, or drift detection.
- Observer congestion cannot erase configuration truth. The reliable channel is sized
  for the start result plus the bounded command queue; its sends wake the GUI so it is
  drained promptly.
- Failed live interface edits remain visibly dirty against the confirmed live
  interface, even if the draft/profile is saved while the old interface keeps running.
- Results from stopped predecessors are drained only to let them finish; they cannot
  mutate the replacement runner's applied state.
- Tests pin failed-update retention, target-scoped error recovery, and start-time open
  confirmation.

## ADR-022 — Confirm whole-run state and reconfigure owned resources in place

**Status:** Accepted 2026-07-13 (post-ADR-021 reliability review).

**Context:** ADR-021 confirmed only the interface at start, while message drift still
used the profile as an ersatz applied baseline. The GUI consequently wrote message
drafts into the profile immediately after spawning a runner, before `open` succeeded.
The same review found two control/reopen gaps: a command accepted while the initial
open was blocking disappeared if that open failed, and opening a replacement before
dropping the old handle made same-port serial and explicitly bound UDP changes fail on
exclusive resources. These contradicted both ADR-021's runtime-truth rule and spec
§4.3's live parameter-change contract.

**Decision:**
- `TalkerSupervisor` retains the requested interface **and messages** as a pending
  start. `InterfaceOpened` promotes them together in one all-or-nothing supervisor
  state transition; an open failure promotes neither. GUI drift compares drafts with
  this confirmed whole run while live and with the profile only while stopped.
- While the supervisor remains active and its poll loop continues, an `Enqueued`
  command is not silently discarded: execution reports `Applied` or `Failed`, and
  `poll` converts unresolved pending ids to `Failed` when a runner exits or an explicit
  stop tears it down, including the initial-open race. Application teardown may
  abandon unobserved results because no observer remains.
- `Interface::reconfigure` handles resources that cannot be double-opened. Same-port
  serial settings are applied to the owned serial handle with rollback to the prior
  settings on error. UDP changes retaining the same local bind update socket options
  and destination in place, also with rollback. Different serial ports, different UDP
  local ports, and TCP continue to open a replacement first and swap only on success.
  ADR-055 amends the serial case once a write has classified its handle as unusable:
  there is no working handle to roll back, so the stale handle is dropped before the
  next settings are opened; a failed update retains the previous settings and their
  automatic recovery path.
- Successful live interface and interval commands update `AppliedRunConfig` inside the
  supervisor. They do not mutate persisted profile state.

**Consequences:**
- A failed start cannot clear interface or message drift, and Save remains an explicit
  draft-to-profile operation rather than a side effect of runtime reconciliation.
- Same-port serial baud/parity/etc. edits and same-bound-port UDP destination/mode
  edits no longer fail merely because the runner correctly owns the old handle.
- The command-result lane now covers both execution rejection and runner exit before
  execution. Tests pin the exit race and same-bound-port UDP update.

## ADR-023 — Lossy fallback for unsupported code-page text

**Status:** Accepted 2026-07-13 (message-builder usability fix). The editor-geometry
portion is superseded by ADR-024; the lossy code-page fallback remains current.

**Context:** The ASCII payload editor accepts Unicode text, but a selected single-byte
code page cannot represent every Unicode scalar. Compilation previously rejected the
entire schedule when pasted text contained typographic punctuation, arrows, emoji, or
other unsupported characters. The same marker-aware editor also supplied a wrapping
layout job to an egui `TextEdit::singleline`; long messages therefore grew into many
visual rows and displaced neighboring controls.

**Decision:** Each character unsupported by the selected code page encodes as ASCII
`?` (`0x3F`), one replacement byte per Unicode scalar. The GUI shows an amber count
and UTF-8 recommendation beside the code-page selector and lists the affected
characters on hover. Unsupported source characters and only their resulting `?`
bytes in the wire preview and live Output pane receive a contrast-aware amber
background; literal question marks remain unmarked. The compiled message records
replacement byte positions once, and only the already rate-limited Output sample
carries that small provenance list, so the send hot path never rescans source text.
Light mode uses a pale amber background with dark text; dark mode retains the stronger
amber background with black text. Valid `‹XX›` markers continue to emit exact bytes,
while malformed marker syntax remains an error. The initial editor fix
also disabled soft wrapping inside a fixed-width, single-line editor; ADR-024 replaces
that geometry with bounded multiline editing.

**Consequences:** Pasted Unicode no longer prevents a channel from starting merely
because an ASCII code page cannot encode every character. Substitution remains
visible before and after transmission, and users needing exact Unicode or exact
arbitrary bytes can choose UTF-8/UTF-16 or byte markers/Hex. Tests pin the reported
example, both-theme contrast, end-to-end Output provenance, and the visual distinction
between fallback and literal question marks.

## ADR-024 — Bounded multiline message text editors

**Status:** Accepted 2026-07-13 (message-builder usability fix).

**Context:** No supported legacy single-byte code page can represent the full reported
set of typographic punctuation, arrows, and symbols. UTF-8 and UTF-16 can, but users
also need to compose intentional line-oriented payloads in any text format. A
single-line field concealed line feeds, while unbounded vertical growth or soft
wrapping could again displace the channel controls around a large message.

**Decision:** UTF-8 is the recommended format for preserving unrestricted Unicode;
it remains a message format rather than an entry in the ASCII code-page selector.
The UTF-8, UTF-16, and ASCII message editors are multiline and preserve each explicit
line feed in the compiled payload. They never soft-wrap. Each editor grows from three
visible rows up to eight as explicit lines are added, then scrolls vertically. A line
wider than the viewport scrolls horizontally. Both scrollbars appear only when their
axis overflows, and the outer editor width remains bounded so adjacent controls keep
their space. Horizontal extent comes from the same non-wrapping egui galley that the
`TextEdit` consumes; the editor reuses that precomputed galley for its first layout
request instead of separately summing every glyph on every repaint. Marker-aware
editors retain their repair snapshot as `Arc<str>` and replace it only when the text
changes, avoiding an additional full text copy on unchanged repaints.

All other message-derived GUI state is likewise revision-memoized. Every
wire-affecting editor change advances its `ScheduleDraft` revision; one
`MessageDraftAnalysis` build then converts and validates the message, computes ASCII
replacement provenance, and compiles/renders the Wire preview. Start eligibility,
drift detection, replacement warnings, and the preview consume that same analysis
until the revision changes. Hex preview formatting writes once into a pre-sized
string. Unchanged repaints therefore do no payload clone, compile, replacement scan,
or per-byte temporary-string allocation.

**Consequences:** Users can see and edit line-oriented messages without allowing a
large paste to take over the detail pane. Exact Unicode requires UTF-8 or UTF-16;
legacy code pages retain ADR-023's visible `?` substitution. Tests pin explicit line
feed encoding, no-soft-wrap layout, horizontal overflow, vertical growth, and the
eight-row viewport cap for both editor variants. They also pin reuse of the galley
that supplied the width, change-only replacement of marker repair snapshots, and
one analysis rebuild per changed revision rather than per repaint or consumer.

## ADR-025 — Preflight replacements before interrupting an active channel

**Status:** Accepted 2026-07-14 (active-run safety fix).

**Context:** A running channel with edited drafts always enabled `Apply & Restart`,
even when the replacement message could not compile. The GUI then stopped the healthy
runner before compiling its replacement. A transient edit such as an incomplete
`‹XX›` byte marker therefore took an active channel offline and reported the problem
only afterward as a zero-based `message 0` schedule error. The Wire preview reduced
the same useful error to `(message is incomplete)`.

**Decision:** A replacement follows a prepare-then-replace boundary. The candidate
interface, complete message list, and compiled schedule are built before
`TalkerSupervisor::start` performs the first runtime mutation. Any preflight failure
leaves the existing runner, applied configuration, telemetry, and Output state
unchanged. `Apply & Restart` uses complete draft validation and is disabled when that
candidate is invalid; its disabled tooltip and the message's Wire preview expose the
specific compile error. User-facing schedule diagnostics number messages from one.
Malformed byte-marker syntax remains an error rather than silently changing wire
bytes.

**Consequences:** Editing cannot interrupt transmission until a complete replacement
is ready. The supervisor still owns the valid restart and predecessor handoff, while
the GUI owns only draft preparation and presentation. Tests pin the disabled invalid
replacement state, exact one-based marker diagnostics, successful complete preflight,
and preservation of a real active UDP run across a rejected replacement.

## ADR-026 — Preparation has no live-time side effects

**Status:** Accepted 2026-07-15 (profile reliability and schedule timing follow-up).

**Context:** ADR-025 protected one active-channel replacement, but two neighboring
paths still crossed the commit boundary too early. GUI profile loading stopped every
runner before discovering that a hand-edited message could not compile or be
represented by the draft model. Schedule compilation also assigned `Instant`
deadlines during preflight, so time spent joining a predecessor or opening an
interface appeared as missed sends when the new runner finally began.

**Decision:** Preparation constructs inert candidates only. A profile load parses the
file, validates the complete profile, materializes every connection and message draft,
and round-trips those drafts through validated `ChannelConfig` construction. The same
draft-to-core conversions feed Save and Start. Only a successful
`PreparedProfileLoad` may stop runners and replace the workspace. Draft conversion
validates each `MessageConfig`, so malformed marker syntax cannot be saved or flushed
through another GUI path either.

`Schedule::compile_unarmed` compiles wire data and intervals without assigning clock
deadlines. Production entry points use that form; the runner calls `arm(now)` only
after it owns an open interface and immediately before entering the send loop. Arming
makes every active message due at that boundary and starts missed-send accounting
there. The timestamp-taking `Schedule::compile` remains a convenience for deterministic
scheduler tests and is implemented as compile-unarmed plus arm.

**Consequences:** A bad profile leaves the current workspace and active transmissions
untouched. Valid loads remain all-or-nothing at the UI commit point. Slow interface
opens and predecessor handoffs no longer age a candidate schedule, create false missed
sends, or move its first-fire grid. Tests pin complete profile preparation, rejection
before replacement, validation at draft conversion, and a deliberately delayed
unarmed schedule whose first poll sends immediately with zero misses.

## ADR-027 — Channel transport is chosen at creation

**Status:** Accepted 2026-07-15. **Context:** listener's established Add-template
model and the cross-app GUI harmonization following ADR-016 / listener ADR-019.

**Problem:** Talker's `+ Add` menu already chose Serial, UDP, or TCP, but the selected
channel header repeated those choices as three radio buttons. The second control was
not just visual duplication: changing a radio mutated the existing draft and queued
an immediate interface apply. A valid retained target configuration could therefore
replace a running channel's transport after one casual click. Listener instead treats
transport as structural: Add chooses a template and Configure edits its parameters.

**Decision:** Both GUIs use listener's model and one Add-menu order: UDP, TCP, Serial.
For talker, `ConnDraft`'s kind is creation-only and private outside its draft module;
it is set by `ConnDraft::new` or by materializing a profile interface. The detail pane
has no transport selector. Configure Connection renders and edits only the fields for
the existing kind. Runtime parameter reconfiguration remains unchanged.

**Consequences:** The normal workflow has one transport choice, and a running channel
cannot switch transport through an incidental header click. A different transport is
a different channel created through `+ Add`; messages are not silently transferred.
Profiles and the CLI still select a kind while constructing channels. There is no
profile-schema or runtime-protocol change. Focused tests pin the common menu order and
creation-time kind.

## ADR-029 — Live NMEA time is a compiled per-send template

**Status:** Accepted 2026-07-17 (spec v2.2).

**Context:** NMEA payloads were fully serialized during schedule compilation. A typed
GGA/RMC/ZDA time therefore stayed frozen across every send, unlike Talker's optional
prepended timestamp. Rebuilding all messages or moving protocol decisions into the
scheduler would weaken preflight and make the common static path pay for one dynamic
format.

**Decision:** `PayloadConfig::Nmea` gains additive, default-off `live_time` and
`live_time_millis` flags. Compiling a message produces either static bytes or an
immutable live-NMEA template containing parsed identity, typed fields, checksum mode,
and millisecond policy. `CompiledMessage::render_at(now)` uses one UTC instant for the
prepended timestamp and all live NMEA substitutions, then computes the outer checksum
over the resulting message.

The supported positions come only from `SentenceType::time_fields()` (nmea0183
ADR-028). UTC time is `hhmmss` or `hhmmss.sss`; RMC date is `ddmmyy`; ZDA day/month
are two digits and year is four. Typed fields are retained in the profile but replaced
on wire; a short field list is extended with empty fields through the last mapped
position. The NMEA checksum is rebuilt after substitution and still honors Correct,
Omit, and Wrong modes. Enabling live time for an unmapped standard or custom sentence
is a preflight error, never a silent static fallback.

**Consequences:** Static payloads remain pre-encoded and allocation behavior is
unchanged. A live NMEA send clones only that message's field vector, substitutes the
small explicit map, and serializes one sentence; a Criterion live-GGA case tracks this
cost beside an equal-wire-shape static GGA. The 2026-07-17 paired measurement was
~2.21 µs live versus ~140 ns static: below the workspace's action threshold until
roughly 4.5–5 kHz sustained live sends, so direct buffer rendering is deferred.
The GUI disables unsupported new selections, exposes the exact mapped fields, and
leaves an already-invalid selection enabled so the user can turn it off. Old profiles
deserialize with both flags false; the profile schema version is unchanged.

## ADR-031 — Payload compilation has one dynamic-safe public boundary

**Status:** Accepted 2026-07-17.

**Context:** `PayloadConfig::compile()` historically returned final `Vec<u8>` wire
bytes because every payload was static. After ADR-029, preserving that API required
rendering a live NMEA template at `Utc::now()` and returning a byte vector that looked
compiled but was already frozen. No production path used the method, but a future
caller could accidentally restore the stale-time defect ADR-029 removed.

**Decision:** Remove direct byte compilation from `PayloadConfig`. Payload parsing and
encoding stay private to the message module. The public path is
`MessageConfig::compile() -> CompiledMessage`, followed by `render()` for a real send
or `render_at()` for a deterministic preview/test. Static and dynamic payloads now
share one API whose type retains the distinction internally.

**Consequences:** Scheduled sends cannot lose live semantics by choosing an
apparently equivalent payload method. Encoding tests exercise the same message-level
compile/render boundary as production. This removes a pre-1.0 Rust API but changes no
profile field, wire output, GUI behavior, or schema version.

## ADR-032 — Send timing telemetry uses due deadlines and bounded cumulative histograms

**Status:** Accepted 2026-07-19.

**Context:** Missed-send totals identify cadence grid points that were skipped, but
they do not show near misses or distinguish scheduler wake delay from payload-render
and interface-call cost. `Schedule::poll` also rendered a dynamic payload before the
runner's retry-backoff gate, allocating bytes and sampling a timestamp even when that
due fire would be suppressed.

**Decision:** Scheduling and rendering are separate boundaries. `Schedule::poll`
returns `Due { index, scheduled_for }`, advances the same drift-free cadence grid, and
does not render. The runner records monotonic deadline lateness when it handles that
due result, applies retry suppression, and only then renders and calls the interface.
It records render duration and synchronous interface-call duration around those two
operations.

Each boundary uses a fixed-size cumulative histogram: bucket counts, sample count,
sum, and maximum, with no retained samples and no allocation while recording. The
three histograms travel in the existing cumulative `Counters` lane, so dropped
intermediate updates self-correct and the final blocking counters keep the completed
run exact at rest. The GUI shows a warm-up state, then p99 bucket upper bounds and
maximums. It labels those percentile values as `<=`, not exact observations.

**Measurement boundary:** deadline lateness begins at the message's monotonic cadence
deadline and ends after scheduler due selection. Render covers construction of the
wire payload, including dynamic timestamps. Send-call duration ends when the
application-level `Interface::send` returns. It does **not** prove that serial bits
left the adapter, a network packet left the kernel, or a peer received the bytes.

**Consequences:** A suppressed fire still contributes deadline telemetry but performs
no render, timestamp sample, allocation, or send call; tests pin the corresponding
sample-count identities. Per-fire collection remains constant-space and constant-time.
Cumulative p99 is deliberately coarse and can hide a recent change after a long run;
recent-window views, timer-mode reasons, capacity estimates, and export belong to the
staged telemetry follow-ups in `TODO.md`. No profile, wire format, timer-resolution
policy, or spec version changes in this decision.

## ADR-033 — Recent timing and timer policy are explicit bounded telemetry

**Status:** Accepted 2026-07-19.

**Context:** ADR-032's cumulative histograms preserve run-wide truth but eventually
make a recent slowdown almost invisible in the percentile. The GUI also could not say
which deadline-wait policy a runner was using, whether Windows accepted its 1 ms
request, or why that request was held. A queued interval change was applied after the
timer guard was reconciled; making the last fast message dormant could therefore enter
the indefinite idle wait while retaining the high-resolution request.

**Decision:** Each send-path histogram now has a companion recent view made from ten
fixed one-second segments. Recording remains allocation-free and constant-space.
Counter snapshots merge segments younger than ten seconds; no individual samples or
per-send telemetry events are retained. The GUI uses that approximate last-ten-second
view for p99 and keeps the cumulative deadline maximum as the run-wide outlier.

Timer policy is reconciled after queued commands and before schedule polling. The
refcounted Windows request records whether `timeBeginPeriod(1)` succeeded; only a
successful first request is paired with `timeEndPeriod(1)`, and nested holders share
the same outcome. `TimerStatus` reports the shortest active interval and one of
Standard, Windows 1 ms, Windows request failed, or native deadline waits. It travels
in cumulative counters and as a low-frequency edge update so an interval change can
refresh a channel that is about to become idle. Non-Windows targets do not make a
Windows-style resolution request and report their native deadline waits for fast
schedules.

**Consequences:** Current behavior and a past outlier are visible at the same time,
without telemetry work scaling with send rate or run length. A failed Windows request
is no longer presented as success or incorrectly lowered. Making a fast schedule
dormant releases the process request before the zero-wakeup idle block. At this
decision's boundary, high-resolution intent was not extended to slow schedules merely
because their payload printed milliseconds. ADR-034 resolves the follow-up with an
explicit channel mode instead of coupling cadence policy to output formatting. No
profile, wire-format, scheduler-cadence, or spec-version change was introduced by
ADR-033 itself.

## ADR-034 — Explicit Precise mode uses bounded deadline-resolution windows

**Status:** Accepted 2026-07-19 (spec v2.3). **The bounded window stands; the
per-channel choice is superseded by ADR-047 (2026-08-03)** — the mechanism below
is unchanged, but it is now selected from the schedule rather than configured.

**Context:** ADR-017's automatic Windows 1 ms request fixes high-rate schedules, but
a slow schedule can still wake several milliseconds late on the default Windows
timer tick. That jitter is visible when a payload prints milliseconds even though
the cadence is only 1 Hz. Timestamp formatting is not a reliable proxy for user
intent: a millisecond field may be informational, while a static payload may still
need a tighter trigger. Holding 1 ms resolution throughout every slow run would pay
the power cost while almost all of each interval is idle. Wall-clock phase alignment
is a separate semantic choice and must not be implied by a timer-resolution control.

**Decision:** `ChannelConfig` gains an additive, default-Standard `TimingMode` with
Standard and Precise choices. It is a run-fixed setting: GUI edits are prepared and
applied through Apply & Restart, and CLI profile runs pass the same value to the
runner. Standard preserves ADR-017 exactly. Any active schedule whose shortest
interval is below 32 ms continues to hold the Windows request continuously in either
mode.

For a slower Precise schedule on Windows, each runner uses a two-stage interruptible
wait. It first waits until `deadline - 32 ms` without a request, then acquires the
existing process-refcounted RAII guard for the final `recv_deadline`. The guard is
released as soon as the deadline is due, before dynamic rendering or the synchronous
send call. Commands interrupt both stages; changed deadlines are recomputed on the
next loop. Nearby windows naturally overlap through the process refcount, and a
dormant schedule reaches its indefinite command wait with no guard held. On
non-Windows targets, Precise deliberately remains one native deadline wait with no
extra staging wake.

Timer telemetry carries both configured mode and active reason (`HighRate` or
`PrecisionWindow`). A Windows request outcome remains stable between precision
windows so the GUI does not flicker back to Standard. Repeated per-window OS-call
logging is suppressed; a failed request is warned on the state transition and stays
visible in telemetry.

**Consequences:** Slow Precise schedules spend only their final deadline windows at
requested 1 ms resolution; a lone 1 Hz deadline is approximately a 3.2% duty cycle,
while close or interleaved deadlines can make the effective duty cycle higher. There
is no spin wait and no dedicated timing thread. Precise can reduce Windows wake
quantization, but cannot remove scheduler load, rendering cost, blocking interface
cost, or hardware/driver buffering. `TimingMode` does not itself align sends or make
`SystemTime` more accurate; ADR-037 adds alignment as a separate explicit choice.
The new profile field is backward-compatible through `serde(default)`, so profile
schema version 2 remains unchanged.

## ADR-035 — Capacity preflight separates physical proof from measured estimates

**Status:** Accepted 2026-07-19 (spec v2.4).

**Context:** Timing percentiles and observed throughput explain an active run, but a
user should see obvious capacity limits before Start. Serial provides enough facts
for a physical sustained-rate calculation; UDP and TCP do not expose the path's link
capacity. Application render/send timing can estimate runner service headroom, but
the two boundaries are separate histograms and synchronous `send` commonly ends at a
driver or kernel buffer rather than at the wire. Treating either estimate as a hard
real-time admission test would overstate what Talker knows and prevent intentional
overload testing.

**Decision:** `core::capacity` owns pure, GUI-independent calculations. Channel
demand sums each non-dormant message's exact wire bytes divided by interval. If any
message is incomplete or invalid, the aggregate is withheld rather than silently
computed from a partial schedule.

**Amended 2026-08-03 — demand comes from the running configuration.** As first
built, demand always came from the on-screen draft, and a running channel with
unapplied edits showed a capacity verdict about a schedule that was not sending.
Every other figure on that pane is runtime truth, so capacity alone describing a
hypothesis made the pane contradict itself — and it let an unapplied oversubscribed
draft be offered as the explanation for a running channel's missed sends. The
runner now stamps each message's wire size beside its interval on the counters
lane (`CompiledMessage::wire_len`, exact and computed without rendering), so a
live channel's demand is its own, and the serial verdict uses the interface the
runner confirmed open rather than the one on screen.

A channel that is *not* running has no such configuration, and preflight is this
feature's stated purpose, so it still projects from the settings shown — labelled
a projection rather than presented as current. Source therefore follows run
state, which is the distinction the readouts were previously trying to carry with
a label while computing from one source regardless.

The GUI keeps a memoized wire-length beside its fixed-time preview for that
projection path, so selected-channel repaint folds scalar values only; it never
recompiles or rerenders payloads.

For Serial, one byte consumes one frame of `1 start + configured data + parity +
stop` bits. Required bits per second are compared with baud. Greater than 100% is a
physical sustained-rate impossibility; 80% through 100% is highlighted as low
margin. Flow control is not credited as capacity because it can only pause output.
UDP/TCP report requested messages and bytes per second without an invented link
limit.

Measured application headroom requires 20 paired render/send observations. The
separate p99 histogram upper bounds are added and divided into the draft's aggregate
message rate. A sufficiently populated recent ten-second view wins; otherwise the
cumulative run supports slow schedules. The readout calls this a sum of p99 bounds,
not a joint p99, and warns when estimated utilization reaches 80%. Tooltips state the
buffering, aligned-burst, stale-draft, and physical-wire limits.

All findings are advisory and never join Start blockers. A regression test pins that
an obviously oversubscribed serial draft remains startable. Existing draft-interface
materialization is shared by drift and capacity checks, avoiding a second per-frame
string-owning config build.

**Consequences:** Users can distinguish an impossible sustained serial request from
an application-side estimate and see both before and during a run. Actual misses,
failures, suppression, lateness, and throughput remain the verdict. The model is
constant-space, performs no send-path work, and introduces no profile or wire-format
change. A future per-message or phase-aware schedulability analysis may model aligned
bursts more tightly, but it must remain separate from this aggregate capacity view.

## ADR-036 — Completed runs retain one exact, self-contained summary

**Status:** Accepted 2026-07-19 (spec v2.4.1).

**Context:** Live cumulative counters are exact at rest, but a new start deliberately
resets them. That made before/after timing comparisons and issue reports depend on
manual transcription before restarting. A restart can also finish an old runner's
tail after a newer run, so retaining whichever completion happened to be polled last
would be incorrect. Building an export string continuously would add pointless work
to the selected-channel repaint path.

**Decision:** Each runner receives a process-unique, increasing `RunId`. It records a
wall-clock start beside the monotonic instant that arms the schedule, then records a
wall-clock finish and monotonic elapsed duration when its command loop ends. After
attempting delivery of ADR-018's blocking final Counters, the runner emits one
`RunFinished` on the reliable control lane from the same final counter/timing
snapshot. The bounded lane reserves room for start-open truth, every simultaneously
queued command completion, and this one completion record.

`RunSummary` owns the channel/run identity, label, start/finish time, monotonic
elapsed duration, end reason, sent bytes/messages and per-message counts,
failed/suppressed/missed outcomes, observer drops, cumulative/recent bounded timing,
and final timer policy. The supervisor retains only the greatest `RunId` per stable
channel slot. During Apply & Restart, stale sampled status from the predecessor is
discarded so it cannot contaminate replacement telemetry, but its reliable control
tail remains drainable until the self-contained summary arrives. Loading a profile
creates fresh slots and therefore clears retained summaries.

The selected-channel GUI shows one collapsed "Last completed run" block. Its heading
contains elapsed, sent, and unsent totals; expansion shows final times, outcomes,
timer/timing facts, and build/platform facts. `Copy summary` produces a versioned,
line-oriented text report only on click. It does not perform file I/O, and no report
string is built during an ordinary repaint.

**Consequences:** One completed run remains available through normal stop/restart
cycles without unbounded history or per-send work. Retention is process-local and
bounded to one summary plus one per-message count vector per channel. Wall-clock
timestamps remain subject to system clock steps; elapsed duration is monotonic. An
interface-open failure occurs before a send run is armed and therefore does not
replace the last completed-run summary. CLI behavior, profiles, wire bytes, and
scheduler cadence are unchanged.

## ADR-037 — UTC phase alignment anchors once, then advances monotonically

**Status:** Accepted 2026-07-19 (spec v2.4.2).

**Context:** Precise mode improves when a deadline wait wakes, but some test sources
also need their application send calls phased to recognizable UTC boundaries. Making
that behavior implicit in Precise would conflate wake mechanics with schedule
semantics. Scheduling directly from `SystemTime` every interval would also expose
cadence to clock slew and steps, while replaying elapsed boundaries after a forward
step would create the same harmful catch-up bursts the stall policy rejects.

**Decision:** `ChannelConfig` gains an additive, default-Immediate
`CadenceAlignment`. Immediate preserves the existing first-send-at-arm behavior.
`UtcPhase` computes each active message's strict next boundary by Unix-epoch modulo
that message's interval. Exactly on a boundary waits one complete interval. The
first boundary is mapped to `Instant`; subsequent deadlines advance from the prior
monotonic grid, so ordinary wall-clock slew does not accumulate cadence drift and
intervals need not divide a day.

An aligned runner compares a paired wall/monotonic anchor with `SystemTime` at most
once per second. A displacement of at least 250 ms rebases only future deadlines to
their next UTC phases, increments a telemetry counter, and never replays past grid
points or adds scheduler misses. A live nonzero interval change uses the new
interval's strict next phase. Immediate mode performs no periodic wall-clock check.
Checked deadline arithmetic turns an unrepresentable extreme interval into an
unscheduled deadline instead of panicking.

The GUI exposes alignment beside Timing and applies a change through the existing
Apply & Restart path. Timer status and completed-run reports name the configured
alignment and wall-clock rebase count. The setting does not compensate for rendering,
synchronous send, kernel/driver buffering, or physical serialization.

**Consequences:** Independent channels can share a UTC phase without a central clock
thread, spin wait, or per-send wall-clock syscall. Default profiles and immediate
startup are unchanged; `serde(default)` plus omission of Immediate keeps profile
schema version 2. A wall-clock step deliberately moves future aligned sends, while
elapsed timing and each between-step cadence remain monotonic.

## ADR-038 — Process timer-resolution mechanics live in `wiredata-timing`

**Status:** Accepted 2026-07-19 (spec v2.4.2).

**Context:** ADR-017 and ADR-034 originally kept the Windows timer guard inside
Talker. Listener's exact Idle deadlines now need the same refcounted
`timeBeginPeriod(1)` lifetime and minimized-window throttling policy. Copying this
process-wide state into both binaries would duplicate the lock/call ordering and make
the two implementations drift. This is narrower than the general `wiredata-core`
runtime crate rejected in ADR-016: no schedules, config, telemetry, or application
types need to be shared.

**Decision:** Add the internal, non-published `wiredata-timing` crate. It owns only
the refcounted RAII `HighResolutionGuard`, the Windows 1 ms begin/end calls, their
success state, and the one-time process throttling opt-out. Talker retains threshold,
window, intent, and telemetry policy in `core::timing`; Listener retains its Idle
deadline policy in its runtime. On Linux and macOS the guard preserves the same RAII
shape but performs no resolution call and native deadline waits remain unchanged.
Poisoned lock recovery and a defensive holder underflow check keep release behavior
bounded; a failed first Windows request is shared by nested holders and is never
paired with an end call.

**Consequences:** Talker and Listener cannot issue conflicting process timer-period
lifetimes, and Windows feature dependencies have one owner. The crate is intentionally
not a home for histogram helpers, schedulers, wall-clock alignment, or GUI state.
macOS App Nap is a separate activity-policy question, not a timer-resolution analog.

## ADR-039 — Bounded duration telemetry primitives live in `wiredata-telemetry`

**Status:** Accepted 2026-07-20.

**Context:** Talker's send telemetry and Listener's receive telemetry came to use
byte-identical duration buckets, cumulative histogram arithmetic, and ten rotating
one-second recent-window segments. ADR-027 in Listener originally kept that helper
local because there was not yet enough reuse to justify another crate. Once both
applications became durable consumers, retaining two copies made numeric fixes and
segment-aging corrections liable to drift silently. `wiredata-timing` remains the
wrong owner because its scope is process-wide OS timing mechanics, not measurements.

**Decision:** Add the internal, non-published, dependency-free
`wiredata-telemetry` crate. It owns `DurationHistogram`, the fixed duration bucket
boundaries, `RecentDurationHistogram`, its private timed segments, and the bounded
ten-second aging/merge algorithm. Talker and Listener re-export the cumulative type
through their existing telemetry modules so their application-facing paths do not
change.

Only generic numeric storage and aging are shared. Talker retains
`SendTimingTelemetry`, `SendTimingReport`, its recorder and measurement boundaries.
Listener retains byte/chunk histograms, transport and timer summaries, pipeline
measurement boundaries, completed-run retention, and presentation. Timer-resolution
mechanics remain solely in `wiredata-timing`.

**Consequences:** Bucket-boundary and recent-window fixes now have one implementation
and one focused test suite, while recording remains allocation-free and constant
space. The extra crate does not create a general shared runtime or shared application
telemetry schema. This decision supersedes only ADR-038's exclusion of histogram
helpers and the local-implementation portion of Listener ADR-027; their remaining
scope and measurement decisions stand. No profile or wire format changes.

## ADR-040 — Timer lifecycle is explicit; Precise windows stay bounded

**Status:** Accepted 2026-07-20.

**Context:** A review questioned whether ADR-034's final-window policy changes
`timeBeginPeriod`/`timeEndPeriod` too often near the 32 ms high-rate threshold: a
50 ms cadence stages for 18 ms and can make about twenty matched request/release
pairs per second. The proposed remedy was to hold resolution continuously whenever
the 32 ms window occupied at least half the interval. That would replace known time
outside the higher-resolution policy with an unmeasured reduction in API-call churn.
Microsoft documents that multiple matched calls are supported and advises requesting
resolution immediately before timer use and releasing it immediately afterward
([`timeBeginPeriod`](https://learn.microsoft.com/en-us/windows/win32/api/timeapi/nf-timeapi-timebeginperiod));
its
[energy guidance](https://learn.microsoft.com/en-us/windows-hardware/test/assessments/results-for-the-idle-energy-efficiency-assessment)
likewise says to restore the lower frequency when the precise task completes. The
runner separately spread guard acquisition, staging, due-time release, final release,
status derivation, notification, counter invalidation, and observer-drop accounting
across a large send loop and positional helper calls.

**Decision:** Retain ADR-034's bounded policy. Any schedule whose shortest active
interval is strictly below `HIGH_RATE_THRESHOLD` (32 ms) holds the shared request
continuously in either timing mode. A Precise schedule at or above that threshold
uses the final `PRECISION_WINDOW`; Standard uses its native wait. The process-wide
refcount means an overlapping holder naturally prevents redundant underlying OS
transitions. Expanding continuous holding without measured evidence is rejected.

Independently, the runner encapsulates timer lifecycle in a `TimerReconciler`. It
owns the current intent, guard, derived status, timer-edge notification endpoints,
counter refresh instant, and observer-drop count. Schedule reconciliation, wait
preparation, bounded-window due release, and final release are explicit methods. An
interrupted precision-window wait retains its guard until the recomputed wait plan
decides whether the deadline is still inside the window; a transition from continuous
to windowed releases immediately. The guard is always dropped before rendering and
sending for a windowed cadence and before final blocking status delivery. A named
`TimerStatusInput` groups the status derivation context instead of another positional
argument list.

**Consequences:** A 50 ms Precise run deliberately releases its guard for the first
18 ms and holds it for the final 32 ms of each interval when no other runner holds the
process request. This minimizes known high-resolution duty time; API-call overhead can
be revisited only with timing or energy evidence. Dormant schedules still release
before their indefinite wait. Linux and macOS still use one native deadline wait and
make no platform timer request. Isolated tests pin high-rate continuous retention,
near-threshold staging, due/idle release, timer-edge notification, counter
invalidation, and drop accounting. The Talker specification now incorporates the
reconciler without changing profiles, wire formats, or ADR-034's wake policy.

## ADR-041 — Diagnostics lead with decisions without hiding telemetry

**Status:** Accepted 2026-07-20.

**Context:** The selected-channel header exposes exact delivery outcomes, cadence
timing, timer policy, draft capacity, measured service estimates, throughput, and
observer pressure. That evidence is useful for investigation and support reports,
but its flat density makes the first operational question—whether anything currently
needs attention—slower to answer. Replacing the readouts with a generic health score
would be worse: it would conceal which boundary was measured, collapse unavailable
and warm-up states into a number, and overstate what buffered host-side measurements
can prove.

**Decision:** Add a compact, decision-oriented diagnostics summary with three
Talker-owned rows: **Send outcomes**, **Cadence**, and **Capacity**. Each row presents
a short assessment and the most relevant existing evidence. Send outcomes derive from
scheduled, locally accepted, failed, suppressed, and missed outcomes, and explicitly
do not claim physical-wire or peer delivery; Cadence derives from the existing
deadline/timer observations; Capacity derives from the current draft demand, serial
utilization where applicable, and measured service estimate when warmed up. Unknown,
unavailable, stale, and warm-up states remain explicit rather than being treated as
healthy.

An **Attention** callout appears only for derived exceptions that merit operator
notice. It is a presentation of existing state, not a persistent green status and not
an opaque composite health score. Row severity and callout selection may prioritize
the most actionable evidence, but the application owns those rules and their
thresholds, and the absence of a callout means only that no configured exception was
derived from the available evidence. Complete telemetry, measurement boundaries, and
caveats remain available under collapsed details; the compact rows do not replace or
weaken them.

`wiredata-ui` may own only the identical egui card, row, and callout
chrome shared with Listener. Talker owns the row labels, evidence selection,
wording, severity mapping, and thresholds. This is a view-model and layout change
only: runtime behavior, telemetry collection and types, retained summaries, clipboard
reports, wire output, and profiles are unchanged. Listener makes the paired but
receive-specific decision in listener ADR-033.

**Consequences:** The default view answers the three common send-side decisions with
less scanning while every underlying fact remains inspectable and exportable. The
design stays auditable because an operator can expand the evidence behind a derived
exception, and it cannot imply an all-clear from missing data. Pure application-side
classification tests can pin row and attention behavior without coupling shared
chrome to Talker semantics or adding work to the send path.

## ADR-042 — Pushed timing snapshots carry freshness and final provenance

**Status:** Accepted 2026-07-27.

**Context:** Talker's counter lane is deliberately push-based and send-path driven.
At a slow or dormant cadence, the latest collapsed recent-window histogram can remain
unchanged long after its samples would have aged out had the runner computed another
snapshot. The GUI nevertheless preferred a recent histogram after warm-up for
application headroom and displayed it in Cadence and detailed Timing. A retained
sample count alone cannot establish that this evidence is current.

Adding a periodic telemetry heartbeat would refresh aging, but it would also discard
the runner's intentional zero-wakeup dormant behavior. Inferring finality from
whether the UI considered a runner stopped was also insufficient: a final snapshot
can arrive before its thread is reaped, while an abnormal exit can stop without
delivering the mandatory final update.

**Decision:** Every `TalkerStatus::Counters` update carries the exact monotonic
`captured_at` instant passed to the timing snapshot computation. Periodic updates
carry `final_snapshot = false`; the mandatory exact-at-rest update uses the run's
existing final monotonic instant for both snapshot computation and `captured_at` and
carries `final_snapshot = true`. The supervisor retains both values beside the
collapsed recent timing. Apply & Restart resets this telemetry and drops the
predecessor's sampled-status receiver, so an old run's final counter tail cannot
overwrite its replacement; the predecessor's self-contained completion summary
continues on the separate reliable control lane.

Talker classifies the retained pushed snapshot once for all GUI consumers:

- no capture instant is **Pending**;
- a non-final snapshot younger than `RECENT_WINDOW` is **Current**, with its capture
  age available for presentation;
- a non-final snapshot whose age is greater than or equal to `RECENT_WINDOW` is
  **Expired**; and
- an explicitly provenance-marked snapshot is **Final**, regardless of later
  display age.

Presentation names what that provenance lets the reader trust, not how the data
travels internally. Pending reads `awaiting timing data`; Current reads `last
~10 s` and adds its update age once that age is useful; Expired reads `recent
timing unavailable` with the last update age; and Final reads `final ~10 s before
stop`. “Snapshot” remains the internal transport shape, not technician-facing
vocabulary for a window of observations.

Capacity, Cadence, and detailed Timing consume that same classification. Expired
recent histograms are neither presented as current nor used for measured application
headroom. Capacity may fall back to sufficiently warmed cumulative run-wide timing
and labels that source explicitly. Final snapshots remain exact-at-run-end evidence.
If a runner exits abnormally before sending its final update, its last periodic
snapshot remains non-final and can expire rather than being misrepresented as exact
at rest.

No heartbeat is added. Periodic Counters remain rate-limited emissions on the send
path, plus the one mandatory final update. An all-dormant schedule continues to
block indefinitely on its command receiver and performs no telemetry wake.

**Alternatives considered:**

- **Emit a periodic snapshot heartbeat:** rejected because it introduces runner
  wakeups solely for presentation and weakens the dormant zero-wakeup contract.
- **Treat every retained recent histogram as usable until another update arrives:**
  rejected because sample count does not encode freshness and can overstate or
  understate headroom.
- **Infer finality from stopped/draining thread state:** rejected because observer
  lifecycle and measurement provenance can transition at different instants and an
  abnormal exit may have no exact final snapshot.
- **Treat partially aged snapshots as predictably conservative:** rejected because
  retained older samples may raise or lower a percentile; the bias has no guaranteed
  direction. `RECENT_WINDOW` is used only as the exact full-expiry boundary.

**Consequences:** Slow and dormant schedules retain zero-wakeup operation while the
GUI can distinguish current evidence, fully expired pushed evidence, and exact final
evidence. All three timing-driven diagnostic areas agree on the source and state;
run-wide fallback remains historical evidence and is labelled as such. The added
fields change Talker's public Rust observer-protocol shape, although they remain
process-local metadata. There is no persisted or serialized telemetry schema,
profile schema, clipboard-report format, wire-data, cadence, or interface behavior
change.

---

## ADR-043 — Telemetry freshness follows the observation model, not the app

**Status:** Accepted 2026-07-27.

**Context:** ADR-042 gives Talker's pushed counter lane an explicit capture instant
and freshness classification. Listener surfaces comparable timing telemetry built on
the same bounded primitives (ADR-039; listener ADR-032) and carries no such instant.
The difference is deliberate, but nothing recorded it, so it reads as an omission on
one side or the other.

It follows from the two runtime models (ADR-002 versus listener ADR-001). Talker has
no async runtime: its runner owns the send loop and **pushes** collapsed snapshots
from the send path, so a retained snapshot ages between emissions — indefinitely on a
dormant schedule. Listener is Tokio-hybrid: a snapshot request is answered inside the
channel's own task, and every recent window is collapsed at the moment it is served,
so a served window always ends at the request.

**Decision:** Freshness handling belongs to the observation model, not to the
telemetry. Talker retains snapshots across time, so Talker proves their age
(ADR-042). Listener computes on demand, so Listener adds no capture instant — under
pull, a stale window is not representable and the field would always read "now".

Neither mechanism is ported across. In particular, Talker does not adopt
compute-on-demand for its counter lane: answering a poll would require waking a
dormant runner, which is precisely the zero-wakeup contract ADR-042 preserves by
rejecting a heartbeat.

The two panels stay consistent in **vocabulary** rather than mechanism — shared
warm-up wording, bounded-percentile notation, and `RECENT_WINDOW`. Talker names an
*expired* state because it can have one; Listener does not.

The general rule, for any lane added later in either app: whichever side retains a
collapsed snapshot across time owns proving its age.

**Consequences:** The asymmetry is recorded on both sides, so neither app acquires an
always-"now" capture instant nor a telemetry heartbeat by analogy with the other. No
profile schema, clipboard-report format, wire-data, cadence, or interface behavior
changes. See listener ADR-035 for the same decision from Listener's side.

---

## ADR-044 — One vocabulary, and each counted fact rendered once

**Status:** Accepted 2026-07-28.

**Context:** The channel detail pane had accumulated two vocabulary problems and
one structural one.

The section that configures a serial port, UDP socket, or TCP peer was titled
*Configure connection*, while the code type is `InterfaceConfig` and every other
visible string said *interface*. Worse for the shared-chrome goal, Listener uses
"connection" for a distinct first-class concept — an accepted TCP peer session
with its own lifecycle and `max_connections` limit — so the title collided with a
real domain term one pane away. "Connection" is also simply untrue of a
connectionless UDP socket or a serial port.

The send-outcome row was labelled *Interface outcomes*, but it counts scheduled
sends; the interface is only where the write landed.

Structurally, the same outcome counters rendered three times: a card row, an
attention callout that repeated them with the breakdown, and a details line that
repeated them again with the byte total. `unsent` and its component `missed` were
also shown as peers separated by the same divider, which invited reading the
aggregate as a fourth sibling category.

**Decision:** *Interface* is the workspace term for a configured serial/UDP/TCP
endpoint; *connection* is reserved for Listener's accepted TCP peer sessions.
The editor section is **Configure interface** in both applications.

The outcome readout is **Send outcomes**, and the counted facts it reports are
rendered in exactly **one** place: an always-visible line directly beneath the
`status · interface` row, followed by a `Sent:` line carrying the cumulative
byte total and the rolling rates. The diagnostics card keeps only readouts that
require interpretation — Cadence and Capacity — and raises no unsent callout. The
send-outcome tone still feeds the card badge, so a failing interface escalates it
while the adjacent line supplies the reason.

The line is written as **visible arithmetic** rather than as a total with a
name for the successful remainder:

```
Send outcomes: 100 scheduled - 1 failed - 0 suppressed - 1 missed = 98 sent
```

Every candidate noun for that remainder failed review. *Accepted* never says
accepted by what, and is ambiguous about which of the four gates — generated,
handled, rendered, written — did the accepting; a failed send was accepted by
three of them. *Unaccepted* is worse as the complement, because suppressed and
missed were never offered to the interface at all. *Sent* alone invites reading a
successful write as delivery. Stating the subtraction removes the need to choose:
the schedule's own cadence points are the anchor, each deduction names the stage
it happened at, and the remainder is defined by the equation. Because the line
defines its own final term, that term can be the plain word **sent** without
carrying the claim on its own.

The three deductions keep their names — they are load-bearing in `RunSummary`,
the clipboard report keys, and §8.1 — and the aggregate `unsent` remains in the
completed-run summary and `RunSummary::unsent_sends`, where one shortfall number
still earns its place. It is simply not needed on a line that shows the parts.

The shared `signal_row` chrome attaches its tooltip to the **label as well as
the value**, because a reader who does not know what a row measures hovers its
name first. `decision_card` accepts an empty title so a card whose rows name
themselves does not carry a heading that merely repeats a nearby word.

**Consequences:** Three renderings of one fact become one; the card shrinks to
two interpretive rows. Byte rates scale by SI unit through a shared
`human_byte_rate` helper, matching how totals already scale, and read `0.0 B/s`
at rest rather than disappearing. The clipboard report's `key=value` field names
are deliberately untouched — they are a machine-readable format, not prose.
Listener's matching rename and vocabulary pass are a follow-up; the shared chrome
changes already apply to it. No profile schema, wire output, cadence, or
interface behavior changes.

## ADR-045 — Deadline delay is charged to the send that caused it

**Status:** Accepted 2026-07-31.

**Context:** A channel runs every one of its messages on a single thread, so any
send holds that thread against every other message's deadline. Every
cadence measurement Talker had was therefore a *victim* measurement: deadline
lateness records what a message suffered, never what it caused.

The scheduler's own arithmetic makes this actively misleading. Skips are
`late / interval + 1` (`Schedule::poll`), so one stall of length `S` costs a
message with interval `I` about `S / I` grid points. A 500 ms block gives a
10 ms message ~50 misses, a 100 ms message 5, and a 1 s message none at all —
while the message that *caused* the block, precisely because it is slow and
infrequent, records almost no misses of its own. Publishing per-message miss
counts would have pointed a technician at the message with the tightest
interval every time: the victim, essentially never the culprit.

Inferring the culprit from per-message `send_duration` is better but still a
guess — it shows which message *could* block, not which deadline it actually
displaced.

**Decision:** Measure the attribution instead. `MessageTiming` carries the
victim's evidence (`deadline_lateness`) and the culprit's
(`blocked_others`, `blocking_sends`) on the same per-message index basis the
existing `per_message_counts` lane already uses.

A message is charged with another's delay only for the portion of that delay
which elapsed while its own send held the thread: a deadline that passed inside
a send is charged to that send, capped at the overlap. Once the runner starts
working off a late backlog, the charge stays with the send that *opened* the
backlog rather than moving to the quick catch-up sends that happen to precede
each later victim — those sends inherited the lateness, they did not cause it.
Lateness with no send spanning the deadline (an OS wake delay on an idle thread)
is charged to nobody, which keeps the counter honest about the difference
between a blocked channel and a late wake. A send that overruns only its *own*
next deadline is self-inflicted and already visible in its `send_duration`, so
it is excluded from a column that means "cost the others". Failed sends are
recorded: a write that blocked and then errored held the thread just as long.

Per-message histograms are **cumulative-only** (~0.5 kB per message). The
rolling ten-second window stays channel-wide, because "is this happening now" is
the Cadence row's job while "who is responsible" is run-wide by nature. State
is one `Option<SendWindow>` pair and one saturating add per send — no allocation
and no work on the send path beyond the `Instant`s already taken.

Per-message **miss** counts are deliberately not offered at any granularity.

Two quantities, not one. `blocked_others` sums the delay imposed on *every*
message displaced, so a single 412 ms send that leaves four messages waiting
contributes each of their waits and the total can exceed the send that caused
it. It is therefore never presentable as an elapsed hold. `longest_block`
records the longest blocking send and is the figure that may be described as
holding the channel.

**Scope of the claim.** Attribution is measured against deadlines the channel
*reached*. A skipped cadence point is never sampled — the scheduler passes it
before any measurement runs — so per-message blame explains observed **lateness**
directly and observed **misses** only by inference. The two usually share a
cause, but they are different populations, and under heavy overload fewer
deadlines are reached, so blame is measured least well exactly when it matters
most. Surfaces that use this evidence to explain misses must route rather than
convict; see the missed-send guidance in spec §3.2.

**Consequences:** A per-message readout can place what a message suffered next
to what it cost the others, so the diagnosis is reading across one row rather
than interpreting the single-thread model. The runner's counter lane,
`ChannelTelemetry`, and `RunSummary` each widen by one vector, and the clipboard
report gains positionally-aligned `per_message_*` lanes. No profile schema, wire
output, cadence, or interface behavior changes.

## ADR-046 — The warm-up gate is retired; a sample count says it better

**Status:** Accepted 2026-07-31 (Talker; Listener migration pending).

**Context:** Both applications gated percentile readouts on twenty samples in
the recent window: below it, show the observed maximum and label the state
*warming up*; at or above it, show `p99 ≤ X`.

The gate does not do what it appears to. `DurationHistogram::percentile_upper_bound`
computes `rank = ceil(samples × 99 / 100)` and walks to the bucket containing
that rank. For any sample count up to 99, `rank == samples`, so the walk lands
in the bucket holding the last sample — the maximum's own bucket. Below a
hundred samples, p99 *is* the maximum rounded up to a bucket edge. Both branches
of the gate were already showing the same number; only the label changed, and it
changed at 20 while the two statistics actually separate at 100.

A gate that switches labels on a threshold also forces a state into every
readout, every tooltip, and the shared vocabulary — one more thing a technician
must learn that describes the tool rather than the channel.

**Decision:** State the maximum and add the percentile only when it is a
different figure. The maximum is exact (`max_nanos`, not bucketed), meaningful
at one sample, and needs no warm-up disclaimer.

State a sample count once with the context that makes it useful, not beside every
figure. Cadence's main line already states the running schedule and the last
~10-second period; a bare count there invited the reader to weigh a denominator
without saying what period produced it. Its tooltip therefore pairs the count
with the period that supplied the displayed figure: the timing period ending at
the latest update, the final period before stop, or the whole run for a labelled
fallback. A table's own Sends column and the work line's own total keep their
counts on screen, while individual boundaries repeat a count only where their
populations differ. Lateness is sampled before retry backoff can withhold a
scheduled send, and the send call is timed for writes that failed, so such a
difference is evidence rather than noise.

Keep the maximum in one syntactic position. For Cadence it leads as `worst X
behind schedule`; its share of the shortest interval and a distinct percentile
follow together in one parenthesis. Render and send-call boundaries likewise
lead with `longest`. A percentile appearing or disappearing must not move the
primary figure under the reader.

`p99 < max` is exactly the test for "the percentile says something new": within
one bucket the bound is `>=` the maximum, so the comparison is false; it becomes
true only when the p99 bucket sits strictly below the maximum's. The condition is
derived from the data and needs no constant, so it cannot drift out of step with
the histogram it describes.

This retires `TIMING_WARMUP_SAMPLES` in Talker. `MIN_SERVICE_SAMPLES` is
**not** the same gate and stays: it guards the measured-headroom *projection*
(ADR-035), which divides by summed p99 bounds and genuinely needs samples before
it can project. It merely shares the value 20.

**Consequences:** Cadence has one measured state instead of two at every sample
count, and long runs gain a figure they lacked — with real spread the row shows
the percentile *and* the outlier, where before the percentile alone hid a one-off
stall. Its main line stays compact while the tooltip retains the denominator with
its time basis. Listener still carries its own warm-up gate and `p99 ≤ X`
phrasing; the shared vocabulary module records that divergence as deliberate and
tracked rather than leaving it to drift. No measurement, wire output, profile
schema, or cadence behavior changes — this is presentation only.

## ADR-047 — The deadline-wait policy follows the schedule, not a setting

**Status:** Accepted 2026-08-03. Supersedes the per-channel choice in ADR-034;
its bounded-window mechanism is retained unchanged.

**Context:** ADR-034 exposed Standard/Precise per channel. Three things were
wrong with asking.

It did nothing in most configurations. `timer_intent` never consulted the mode
below `HIGH_RATE_THRESHOLD` — those schedules hold the request continuously
either way — and the guard is a no-op on every platform without a Windows-style
resolution request. A control that is inert on two of three target platforms, and
inert again below 32 ms on the third, is worse than no control, because it cannot
say when it is being ignored.

It asked for a decision the user had no way to evaluate. ADR-034 rejected
inferring intent from payload formatting, then substituted a guess made before
Start with no feedback. Talker now measures deadline lateness directly, so the
evidence is better than the guess.

And the answer is not in doubt. The window's cost is its duty cycle,
`PRECISION_WINDOW / interval`; its benefit is roughly constant, about 14 ms of
worst-case wake error removed. Cost falls as schedules slow while benefit holds,
so above the threshold there is no interval where declining is right — at the
threshold the window's duty is 100%, exactly the continuous request it takes
over from, and it only cheapens from there.

**Decision:** Select from the shortest active interval alone:

```
None            -> no request
< 32 ms         -> hold 1 ms continuously
>= 32 ms        -> hold 1 ms for the final 32 ms before each waited deadline
```

Below the threshold there is nothing to window: the coarse wait preceding the
window wakes on the platform tick, so the window cannot be narrower than the two
ticks that make it necessary — which is why `PRECISION_WINDOW` and
`HIGH_RATE_THRESHOLD` are the same constant. An interval shorter than the window
leaves no coarse phase to stage.

`TimingMode`, its profile field, and the radio buttons are removed, and **nothing
replaces them in the editor**.

Two intermediate designs were considered and rejected. Making the radio
auto-update as intervals change gives one value two owners — the user's pick and
the derived value — so it must either discard the first or ignore the second,
with no way to tell the reader which happened. A read-only preview line was then
built and removed: with the policy automatic, the accuracy it produces is the
same in both bands, so the line's only invariant content was a constant, and the
one fact that did vary — continuous versus windowed — changes how much the
process perturbs the machine's timer, not anything the user's output shows. A
readout that says the same thing on every look is decoration.

What a *running* channel actually got is still reported, in the diagnostics
card's timer readout, where it is an observation rather than a prediction.

**Consequences:** A 1 s channel previously defaulted to Standard and made no
request; it now holds 1 ms for 3.2% of the time. That is a real change to
system-wide timer behaviour for schedules nobody opted in, accepted because the
duty is small exactly where the absolute accuracy gain is most visible — a
payload printing milliseconds while the wake is 15 ms off was ADR-034's own
motivating case.

Profiles carrying `timing_mode` still load: serde ignores unknown fields, so the
setting is silently dropped rather than erroring. This is the first *subtractive*
profile change, and schema `version` stays at 2 on the grounds that no reader can
misinterpret data that is simply absent. The clipboard report drops
`timing_mode`; `timer_policy` and `timer_reason` already state which policy
actually applied. `TimerReason::PrecisionWindow` now means "interval at or above
the threshold" rather than "the user chose Precise". Non-Windows behaviour is
unchanged.

**Superseded in part (2026-09-30):** ADR-062 refuses unknown profile keys, so a
profile still carrying `timing_mode` no longer loads. Schema 3 (ADR-057) refuses
every version-2 profile in any case.

---

## ADR-048 — Semantic color has one source; surfaces are derived, not named

**Status:** Accepted 2026-08-05. Completes the chrome rule of ADR-016 /
listener ADR-019 for Talker's remaining bypasses.

**Context:** `wiredata_ui::palette` was introduced to make a restyle a constant
edit rather than a hunt through call sites, and Listener adopted it wholesale.
Talker adopted it partly. What was left behind was not decorative: the log
panel's six severity literals meant an ERROR line was `220,80,80` while a
faulted channel was `fault_red`; the invalid-field outline carried its own red
under a comment calling it "the rest of the GUI's warning red", which it was
not; the status-bar and message-status dots each had their own green and grey.
A user reading a fault in two places saw two reds and had no way to know they
meant the same thing.

The message status strip named four background colors — a green and a grey for
each theme — chosen by hand to sit under body text.

**Decision:** Every color that carries *meaning* comes from the palette. What
"meaning" covers is status (running, idle), severity (error, warning, info,
debug, trace), and destructive intent (the Remove button, an invalid field).

Two things follow that were not just substitutions:

- **Surfaces are derived from accents, not named beside them.** The palette
  gains `tint(ui, accent, alpha)`, which blends an accent into the theme's own
  `panel_fill`. A tinted surface then needs no light/dark pair: it lands pale on
  light and deep on dark by construction, and it cannot drift away from the
  accent it belongs to. The four hand-named status-strip backgrounds and the
  invalid-field wash are now derived this way, as are the three places in the
  shared diagnostics card that were blending a color into the panel by hand. The
  card's `translucent` helper is *not* the same function and remains in use: it
  adjusts a color's alpha, which is what the card's two strokes want.
- **INFO takes no accent.** It was `from_gray(235/20)`, which is body text
  spelled as a literal. It is now `ui.visuals().text_color()`, because INFO is
  the baseline the other severities are read *against*; giving it a palette
  entry would assert it is a status when it is the absence of one.

**Deliberately not moved:** the code-page replacement highlight (ADR-023), the
byte-marker blue, and the `?` fallback background in the Wire preview. These
color *message content*, not chrome — the same distinction the palette's own
module doc already draws when it excludes stream/display content colors as
"user-chosen per view". They are Talker-only, they exist to make one byte
legible against its neighbours rather than to signal channel state, and moving
them into shared chrome would make the palette answerable for Talker's message
editor. The 3 px window-frame trial in `gui/mod.rs` is also untouched: it is an
open experiment, not a settled semantic.

**Consequences:** Some colors shift slightly — the invalid-field red moves from
`220,80,80` to the palette's `fault_red`, and WARN/DEBUG/TRACE land on their
nearest palette equivalents. That is the point: one red, one amber, one idle
grey across both applications. `level_color` now takes `&Ui` rather than a
`dark: bool`, since it reads both the palette and the theme's text color, and it
is pinned in both themes by `log_severity_colors_come_from_the_shared_palette`.
`tint` is pinned by `a_tinted_status_strip_follows_the_theme_from_one_accent`,
which asserts the derived fill differs from both the accent and the panel, and
differs between themes — the three ways a derived surface can be wrong.

`wiredata-ui` still depends on `egui` alone; `tint` reads `Visuals` and nothing
else.

---

## ADR-049 — The fault colour is blue, and palette fields name roles

**Status:** Accepted 2026-08-06.

**Context:** ADR-048 gave every semantic colour one source. It did not ask
whether the colours could be *seen*. Checking the palette against a red-green
colour deficiency answered that: `fault_red` was not distinguishable from
`warning_amber`. The most important signal either application has — a channel
faulted, a recording dead, a field invalid, a destructive button — was the one
that did not arrive, while the less urgent warning did.

This is not a preference. Red against amber is the hardest pair under the most
common deficiency, and the palette had staked its highest-priority meaning on
exactly that pair. The module's own doc claimed `running_green` had been "chosen
to read distinctly from the fault red for red-green color blindness", which
shows the concern was considered — and considered only for green, while red and
amber sat next to each other carrying more.

**Decision — the fault colour is blue.** `rgb(0, 85, 200)` light,
`rgb(95, 165, 255)` dark. Blue is discriminable under every common deficiency,
being the axis red-green deficiency leaves intact; the light value is deep
enough to carry white text on the Remove button's fill, and the dark one light
enough to stay legible as small text. Confirmed visible in both applications
before adoption.

**Decision — palette fields name the role, never the hue.** `fault`, not
`fault_red`. A palette exists precisely so a colour can change; a field name
that encodes the value contradicts the thing the field is for, and the rename is
the proof — `fault_red` became blue, and two more fields are queued to change
for the same accessibility reason. `Color32` in a struct called `Palette`
already says these are colours, so the name only has to say what for. All ten
fields were renamed together rather than only the one that moved, since
otherwise this recurs twice more. 73 call sites, every one compiler-checked.

`box_stroke` was removed: declared in both palettes and read by nothing. It was
also the only field already named for its role rather than its value, which is
not a coincidence — a name describing the job does not rot when the appearance
changes.

**Consequences:** Both applications change appearance wherever a fault is shown.
The doc comment on `fault` says why it is blue and says not to restore red,
because the failure it fixes is invisible to anyone who does not share the
deficiency — a future contributor "correcting" the colour would be undoing a fix
they cannot see.

Two known defects are recorded rather than fixed here (`talker/docs/TODO.md`,
"Colour accessibility"): serial control lines distinguish asserted from low by
colour alone, which is a functional failure in a readout whose only job is
telling those apart; and `running` against `warning` is unchecked. `line_high`
carries a doc comment naming its own defect.

The durable rule is stated there rather than repeated per call site: **anywhere
a state is carried by colour alone is the bug.** Colour may reinforce a
distinction; it may not be the only thing making it. The diagnostics cards
already satisfy this with word badges, and that is the pattern to copy.

Also open, and larger: the palette may simply hold too many colours — five of
ten fields are greys separated by emphasis rather than meaning. The burden is
pairs, not colours, so ten colours is 45 pairs that must stay distinguishable
against four's 6. Deferred to its own pass because collapsing the greys is a
change that must be looked at, not proven.

---

## ADR-050 — Four accents, and colour never carries a state alone

**Status:** Accepted 2026-08-06. Completes the accessibility work of ADR-049.

**Context:** The palette held ten colours. Five were greys — `idle`, `info`,
`event`, `count_info`, `line_low`, at 120/80/60/110/150 in the light theme. Two
were ambers a shade apart (`warning`, `reconnecting`). Two were greens meaning
the same thing in different places (`running`, `line_high`).

None of that was free, and the cost is not the count. Every colour must stay
distinguishable from every other, so the burden is **pairs**: ten colours is
forty-five pairs, four is six. Three of those pairs were checked against a
red-green deficiency during ADR-049 and two failed. The pair count was not a
theoretical budget — it was the surface a real defect had been living on.

**Decision — four accents:** `fault`, `warning`, `running`, `idle`. They are the
same four states `SignalTone { Fault, Warning, Healthy, Neutral }` already names;
the palette and the tone enum had been describing one set of states in two
vocabularies.

What replaced the removed fields is not another colour:

- **The greys became `Visuals::weak_text_color` and `text_color`.** They encoded
  *emphasis*, not meaning, and emphasis is something the theme already defines.
  A palette re-deciding it produces four values that must track two.
- **`reconnecting` folded into `warning`**, `line_high` into `running`,
  `line_low` into `idle`. Each pair meant one thing in two places — an asserted
  line *is* active, a reconnecting channel *does* need attention.

Each collapse was checked against the same test before it was made: **is the
state still legible with the colour removed?** In every case it was, because the
readout already said it in words — a log line is formatted `[time] [LEVEL]
message`, the severity counts read "3 warn", the diagnostics list prefixes
`INFO `/`WARN `/`ERROR `. That is not a coincidence. It is the rule the palette
now states:

> Colour reinforces a state. It never carries one alone.

The readouts that survived ADR-049's review were exactly the ones already obeying
it; the ones that failed were the ones that did not.

**Found during the reduction:** `status_glyph` mapped Running *and* Reconnecting
to `●`. In the detail pane that is harmless — it prints `status_label` beside the
glyph. But the channel list shows the glyph and the channel's *name*, so on the
one surface built for scanning many channels at once, a reconnecting channel and
a healthy one were the same mark separated by green against amber: the exact pair
confirmed indistinguishable a day earlier. Reconnecting now has `◐`, half-filled
between a solid dot and an empty one, which is also what the state is.

**Consequences:** Both applications change appearance in the log panel, the
diagnostics list, and the channel-list counts, where four greys became two theme
emphases. That flattens some hierarchy; it is the one part of this that tests
cannot judge, and it is filed to be looked at.

**Correction (2026-08-06, from external review).** This entry originally cited
two tests as evidence for the decision, and both proved less than it claimed —
one asserted that an array built from four named fields had four elements, the
other that blue was a colour's largest channel. A ratchet over a simulated
red-green deficiency replaced them and was removed in turn: it did not
reproduce the original defect, because red and amber differ in lightness even
where their hue collapses.

The durable policy, which is what this entry should have stated instead of a
test list: **four semantic accents, each distinct from the others, and colour
never carrying a state alone.** The first half is mechanically checkable and is
checked. The second is the one that actually delivers accessibility, and it is
enforced where the states are rendered — every one carries a glyph or a word.
Whether the accents are *comfortable* remains a visual review with a human
looking at both themes; no metric available here substitutes for that, and
pretending otherwise is how the removed tests came to exist.

---

## ADR-051 — A missed send is attributed where it was missed

**Status:** Accepted 2026-08-06. Amends the scope limit in ADR-045.

**Context:** ADR-045 measured which send caused a deadline to be *late*, and was
explicit that this could not explain a send that was *missed*: a skipped cadence
point is passed over by `Schedule::poll` before any measurement runs, so the
only evidence available was inference from the deadlines the channel did reach.

That limit is not uniform — it is worst precisely where the answer matters. Blame
accrues per reached deadline, so a write that holds the thread for ten seconds
against a 10 ms cadence destroys a thousand cadence points and yields exactly
**one** lateness sample. Doubling the stall does not double the evidence; it
halves it. The measurement was thinning out as the fault grew, which is the
opposite of what a diagnostic should do, and the surfaces built on it had to
hedge accordingly (spec §3.2: "it routes, it does not convict").

ADR-045 also refused per-message miss counts, correctly: skips accrue as
`late / interval + 1`, so a *victim-side* count concentrates on the tightest
interval and names the message that suffered. That reasoning rules out one
column. It does not rule out measuring the misses at all.

**Decision:** Attribute misses at the skip, on the culprit side.

`Schedule::poll` already computes how many grid points it is passing over and
knows the interval it is passing them against. It now reports both on
`Tick::Due` (`skipped`, `interval`) instead of only folding them into the
channel-wide `missed_sends` total. The skipped points lie at
`scheduled_for + interval`, `+ 2 × interval`, …, which is enough to place every
one of them on the timeline.

`MessageTiming::missed_others` counts, for each message, the cadence points
**other** messages lost while its own sends held the channel. A point is
attributed to whichever send spanned the moment it passed. Points from a
message's own schedule are not attributed back to that message; an overrun there
is already visible by comparing its longest send call with its interval.

A skipped run is **partitioned** across every retained send window it overlaps.
A stall long enough to skip points is usually long enough to contain several
writes — most commonly at startup, where every message is due at once and the
runner drains them one at a time — so assigning the run to a single send would
credit one message with damage the others did. Partitioning also makes the
backlog rule unnecessary on this path: a quick catch-up send owns only the
points inside its own brief window, which is almost never any, so it cannot
inherit a burst it is clearing.

**Correction (2026-08-06, before release).** This entry first specified the
attribution against a single retained send window and described the shortfall as
points that "passed with the thread free". Both were wrong. The recorder kept
one completed window, so a deadline or a skip behind an *earlier* write in the
same busy stretch matched nothing — the startup case above went entirely
unattributed — and ADR-045's `blocked_others` had carried the same hole silently
since it shipped. Calling the shortfall idle time then converted a gap in the
record into an affirmative claim about the machine. The record now retains
one send window per message and both paths search them, which closes the
delay hole as well; and an unmatched point is reported as **unattributed**, never
as idle, because a late wake, work outside an interface write, and a genuinely
free thread all leave the same gap. (The history was first capped at a constant,
which the same review then pointed out was a number with no matching limit
anywhere in the scheduler; it is now derived as below.)

Two details the arithmetic forced:

- **Closed form, never a loop.** A machine suspended overnight against a 1 ms
  cadence skips tens of millions of points. The count of points falling before
  the blocking send ended is `ceil(span / interval)`, capped at the number
  skipped, so recording a stall costs the same whether it lost one point or
  billions. This runs on the channel thread.
- **The skip lookup must not move the backlog cursor.** Messages are handled
  earliest-first, so a tick's skipped points reach further forward in time than
  deadlines still queued behind it. Letting a skip lookup advance the cursor
  would step over those deadlines, and a real block would go unattributed —
  `blocker_for` is therefore a pure query and only the deadline being handled
  advances the cursor.

**What this does and does not claim.** The attributed share is measured, not
inferred, so the missed-send callout may state an amount and name a message for
it. Its own limits are narrower than the old ones but real: a point is attributed
to whichever message was inside its *interface write* when the point passed. A
message that holds the channel some other way is not matched (render is 1–3 µs
and below the histogram's first bucket, which is why the send call is the only
window measured).

The retained history is sized from the message count rather than capped at a
constant, and the size is a consequence rather than a budget. Once a message's
deadline is overdue, each *other* message can be serviced at most once ahead of
it: only an earlier deadline goes first, and firing advances that message to the
first grid point in the future. No more than one send per message can therefore
separate an overdue deadline from its handling. One retained window per message
is always enough to answer, and a schedule that grows widens its own reach.

The callout must therefore state three quantities: the total observed during
other messages' sends, the largest single share, and the remainder. For the
remainder it says no serial or network send was recorded as being in progress;
it does not call that idle time. Naming only the largest contributor and the
shortfall drops every other matched message out of a sentence whose numbers are
supposed to add up.

**Consequences:** `MessageTiming` gains one `u64`; the recorder gains a ring of
send windows one deep per message (tens of bytes each, alongside the ~0.5 kB of
histograms that message already carries) and one saturating add per overlapping
window on a tick that skipped anything, with no allocation on the send path. The
clipboard report gains a `per_message_missed_others` lane which sums to at most
`missed_sends`, so the unattributed share is a subtraction the reader can do
from two adjacent lines. The per-message table's **Delay caused** column becomes
**Cost to others** and carries both currencies with their nouns attached
(`430 ms late · 37 missed`) rather than taking an eighth column — they answer
one question, and a reader comparing rows should not have to track which of two
columns moved. Neither is convertible into the other: one is time spent waiting,
the other is sends that never happened.

No wire output, cadence, scheduling behaviour, or profile schema changes. The
stall policy is untouched — this measures the skips it was already making.

---

## ADR-052 — A warning may be acknowledged, and it is raised where it lands

**Status:** Accepted 2026-08-09.

**Context:** A warning standing on a run-total counter stays on screen for the
rest of the run, long after the pressure that raised it has passed. A warning
that cannot be cleared is one the reader learns to look past — including on the
occasion it means something.

**Decision:** Three rules.

*A warning has one visible home beside its evidence.* The bounded live-update
queue carries payload samples and diagnostic state alike, so a drop can omit an
Output line or make a live readout lag. It does not affect the wire. The warning
needs one home rather than copies in two sections; it sits above **Output** because
discarded payload updates land there and the section header can keep the count
visible while collapsed. Its tooltip states the broader diagnostic effect.
`diagnostic_card_tone` therefore does not read drops: a badge reading ATTENTION
over otherwise calm rows would imply that one of their underlying measurements
had found a channel problem. The live-update queue gauge belongs beside its
warning, not in a second collapsed section. A warning inside a collapsed section
also marks that section's header, or its chosen home could remain invisible.

*Dismissal is acknowledgement, not deletion.* Dismissal records the counter, and
the warning returns when that counter is **exceeded** — told once per occurrence,
never silent while a fault spreads. The two rejected alternatives are the ends of
one axis: a dismissal lasting the run silences a real escalation, and one
persisted across restarts hides a regression weeks later with no way back. A
counter *below* the record can only mean a new run, so the record is discarded
rather than trusted; without that, a fresh run's first drop would be measured
against a number from a previous one.

*Dismissal never removes a counted fact.* The missed-send routing callout is
advice about where to look; the misses stay on the send-outcomes line and keep
the card's badge raised, so a dismissed routing line cannot make a channel with
skipped sends look clean.

**Boundary.** Only a warning standing on a growing counter is dismissible. A
live interface fault, a serial line that cannot carry the schedule, and a failed
Windows timer request each describe a condition still true while it is on
screen: there is nothing to acknowledge and nothing that would bring them back.
The sampling note is not dismissible for the same reason, but it is set in a
small, quiet weight shared with the queue gauge. It describes standing context,
not a condition to act on; the dropped-update callout alone carries urgency.

**Consequences:** Dismissal is process-local view state, per channel, never
written to a profile, and changes nothing counted, logged, or sent. The shared
chrome gains `dismissible_attention_callout` beside the plain one, sharing its
frame so this is not a second visual language for the same kind of statement.
Timing & runtime details no longer repeats the live-update queue.

---

## ADR-053 — Losing cadence is a logged event, not only a counter

**Status:** Accepted 2026-08-09.

**Context:** Failed sends have been logged since the runner shipped. A *missed*
send — a scheduled point the channel fell more than an interval behind and never
reached — was logged nowhere; it existed only as a live counter. The CLI has no
diagnostics card and prints no run summary, so a channel could skip half its
cadence points with nothing on stdout at all, and in the GUI the log recorded
connection loss but never cadence loss.

**Decision:** Report the edges of an off-cadence episode and nothing between
them. WARN on the first skipped send, INFO once the channel has gone
`MISS_RECOVERY_SETTLE` without another, carrying the episode's total. A run that
stops mid-episode logs the run's missed total, so the log never ends on an
unanswered warning. A later lapse is a new episode counted from zero.

*Not a line per miss.* Skips accrue as `late / interval + 1` and concentrate on
the shortest interval, so heavy overload produces thousands a second — into a log
pane the GUI feeds through a bounded queue, which would discard the very updates
the reader needs. The fault's symptom would degrade its own diagnosis.

*Not the first clean send.* A marginal channel skips intermittently, so closing
on the first clean poll reproduces that flood at two lines instead of one. The
window only has to outlast the skip's cause, and the coarsest ordinary one is an
OS scheduling quantum, so seconds is the right order of magnitude; five is a
settle time chosen within it, not a threshold anyone should tune.

*Not a threshold on the opening edge either.* The first event is the one the
reader could have acted on, and every later one says only "still" — the same rule
the failure path follows. A threshold would need justifying per schedule, since
one skipped send on a 15 s cadence is a fifteen-second hole in the data and one
on a 10 ms cadence is nothing anyone would notice.

**Boundary.** The settle window is a **minimum, not a deadline**, and the line
says so. The check runs where the skip count arrives, on a cadence point the
channel reaches, so nothing is woken to announce its own recovery and a slow
schedule reports at its next send. Adding a timer to the send loop's wait
calculation — on the hot path, to make a notice punctual — was rejected. The line
is scoped to cadence alone: sends can still be failing or withheld by backoff
while every point is reached, so it states what it measured rather than "back to
normal".

**Consequences:** Cadence loss joins connection loss in the same record at the
same two levels, and the CLI blind spot closes. The transition table is a pure
function tested directly, because "sustained overload logs twice, not once per
skipped send" is a property of the table rather than of any timed run; two
timed tests cover the seam to the send loop, including the stop-mid-episode
path and the independence from send failure. Nothing measured, counted,
scheduled, or sent changes; this is reporting only.

---

## ADR-054 — The log is written for the reader, and `channel` is a claim

**Status:** Accepted 2026-08-09.

**Context:** Log strings had been written in the vocabulary of the code that
emitted them rather than of the reader in front of the screen. Worse, six of
them report talker's own state machine disagreeing with itself and each carried
a structured `channel` field — which the GUI log layer turns into a warning badge
on that channel's row. An internal bookkeeping bug summoned the reader to their
serial link over a fault that was not the channel's and that they could neither
act on nor clear.

**Decision:** Three rules.

*A log line states its consequence, in the reader's vocabulary.* The reader is a
technician with a device on the other end of a wire, not the author of the module
that emitted the line. Name what happened to their channel, not the mechanism
that carried it.

*`channel = …` is a claim, not decoration.* It asserts *this event is about that
channel's operation*, and it is what raises the row badge. A failed send, a clock
step, a skipped cadence point all qualify. An internal bookkeeping fault does
not — so those carry no `channel` field and name the channel in their text
instead, which keeps the identity in the record without making a false claim in
the UI. The rule is written down because the field is trivially easy to re-add
out of a wish to be helpful, and nothing in the type system says otherwise.

*An internal fault says it is one, what it cost, and how often.* One shape:
`internal fault on channel <label> (<n>x): <what our state did>. <what it costs
the reader>. Please report this.` They stay at WARN/ERROR rather than dropping to
DEBUG — a fault recorded only when someone had already raised the log level is one
nobody hears about, and these are rare by construction, so their *frequency* is
itself a signal.

**Rate limiting.** Every one of these sits in a loop that repeats, so a wedged
state machine could emit one per poll forever. `InternalFaultTally` reports the
1st, 10th, 100th … occurrence with the running count: a single edge case is never
missed, a billion occurrences cost ten lines, and the count distinguishes those
two shapes where a first-only guard would not. The tally and the sentence shape
live in one module (`core::internal_fault`) that every caller reaches for —
applying the rule was tried by convention first, and one of the six was missed.

**Consequences:** No behaviour, measurement, or wire output changes; this is
reporting only. The tally is per channel slot, and per run for the runner's, so a
wedged slot cannot be diagnosed from a number that pooled every channel. Three
tests hold the rules: the decade cadence and the sentence shape, both properties
of the counter rather than of any run, and one that dispatches a real fault at a
real call site through the GUI log layer and asserts the badge never rises —
because a rule the type system cannot carry needs something other than reading to
enforce it.

---

## ADR-055 — A serial retry replaces an unusable operating-system handle

**Status:** Accepted 2026-08-10.

**Context:** A running serial port owns an operating-system handle to one device
instance. Unplugging a USB serial adapter can leave that handle permanently
unusable even when a replacement later appears under the same COM name. Windows
may report the stale write as access denied or operation aborted. Retrying the
write on that same handle cannot reach the replacement, and opening the name
before releasing the stale handle can fail because serial ports are exclusive.

**Decision:** Serial recovery stays inside the existing edge-triggered send
failure episode. Timeout, would-block, interrupted, and zero-write results keep
the handle because they can describe transient flow control rather than device
replacement. Windows operation-aborted code 995 takes precedence over its
broader timeout category and replaces the handle.

Every other error, including one the standard library cannot classify more
narrowly than *uncategorized*, marks the handle for replacement. That bias is
deliberate: Windows invalid-handle, general-failure, and device-not-connected
results and POSIX I/O, no-device, and no-such-device errors do not share one
portable error category, even though each can describe a removed adapter. A
narrow fatal-error allowlist would therefore restore the permanent stale-handle
loop for real drivers. A new keep-handle exemption requires observed evidence
that the error is transient; an unfamiliar error is not assumed recoverable on
the handle that just returned it.

At the next due send admitted by bounded backoff, the runner gives the transport
a preparation step before it renders the payload or starts send-call timing.
Serial uses that step to drop the stale handle first and then open the same
current port configuration. Other transports do nothing there: UDP socket
replacement would change source-port semantics, and TCP reconnect policy is a
separate decision.

If the serial open fails, no interface write is attempted. That due send counts
as withheld, the backoff deepens, and the failure episode remains open. The open
attempt is outside render and send-call timing and outside per-message send
overlap attribution; those measurements describe payload work and interface
writes, not port discovery. A successful open proceeds through the ordinary
render and write path, and the first successful write closes the episode with
its failed and withheld totals. A successful live serial update during recovery
supplies a fresh handle and clears pending replacement; a failed update keeps the
previous settings and recovery path.

Handle replacement does not create another log edge. The existing INFO line that
closes the failure episode names the configured serial port when sending recovers
on an automatically replaced handle; ordinary transient recovery keeps the
generic wording. A later successful live interface update supersedes the
automatic-replacement marker. This distinguishes the support cases without
logging every open attempt or reporting recovery before a write has proved the
new handle usable.

**Boundary:** This is recovery for a run that opened successfully. An initial
open failure still ends the run; the GUI uses **Retry Channel**, and CLI mode
must start another run. Automatic recovery reopens only the configured port name
and does not search for a replacement that enumerated elsewhere. Backoff grows
from 250 ms to five seconds, but an open is attempted only at the next due send
after the gate, so the cap limits attempt frequency rather than recovery latency.
A failed write may have transferred a prefix before reporting its error; recovery
therefore makes no exactly-once delivery guarantee. Talker remains
running-with-error during the episode rather than adding Listener's reconnecting
lifecycle state. Disconnecting only the remote RS-232 cable or device may leave
writes successful at the operating-system boundary and therefore does not
necessarily trigger this recovery.

**Consequences:** A removed and replaced serial device can resume within the same
GUI or CLI run without a command, while access denied from another process during
recovery keeps retrying at bounded frequency. Failed opens do not masquerade as
failed writes or pollute send-call timing. The transport-specific preparation seam
avoids generic runner reconstruction and leaves UDP and TCP behavior unchanged.
If a built-in transport reaches replacement without confirmed settings, that is
reported as talker's own rate-limited internal fault rather than blamed on the
channel.

---

## ADR-056 — Current channel faults are snapshotted separately from run history

**Status:** Accepted 2026-08-11.

**Context:** Send outcomes and channel-attributed log counts deliberately remain
for the whole run. A failed serial write must still be visible after the adapter
returns, but keeping the channel row faulted after a successful send would turn
history into a false claim about current health. The opposite failure also existed:
the immediate failure and recovery statuses use the best-effort observer lane. A
full queue could discard either edge, leaving a still-broken channel looking clean
or a recovered one looking broken. A cumulative failed-send count repairs history
but cannot distinguish those two current states, and an optional error alone cannot
count two complete episodes that occur between delivered observations.

**Decision:** Current fault state and run history remain separate:

- A healthy-to-failing transition opens one send-failure episode. The runner keeps
  cumulative failed, suppressed, and episode counts while retaining a separate
  current send/reopen explanation. Later failed writes or reopen attempts refresh
  that explanation as the obstacle changes without opening another episode or
  recategorizing a withheld reopen as a failed interface write.
- Immediate `ConnectionError` and `SendRecovered` statuses remain the low-latency
  edge notifications. Every periodic and final `Counters` status repeats both the
  cumulative episode count and the optional current explanation. The supervisor
  treats those snapshot fields as authoritative, repairing the running
  send-failure contribution to the application-wide error tally and either
  current-state edge if an immediate status was dropped.
- A successful send closes the episode and clears the current interface fault. It
  does not reset Send outcomes, channel log counts, or completed-run history. The
  selected diagnostics card also retains ADR-044/052's existing escalation from
  cumulative failed or missed outcomes.
- Command failures remain independent because a working interface does not prove a
  requested edit was applied. The current row banner prefers an unresolved command
  failure over an interface failure and clears each only through its own success
  path.

**Boundary:** Counter snapshots repair observer state; they do not recreate a
dropped GUI log event or its per-severity row tally. They remain send-path updates
plus the final at-rest update, not a heartbeat. An all-dormant schedule therefore
does not wake merely to refresh the row, and an initial interface-open failure
occurs before an armed run has counters available to repair its immediate status.

**Alternatives considered:**

- **Keep a channel faulted whenever the run has any failed send:** Rejected because
  it makes a recovered channel claim a current problem. Run totals already preserve
  that history.
- **Reset failed outcomes on recovery:** Rejected because it destroys the run's
  accounting and hides an intermittent device fault.
- **Make failure edges blocking/reliable:** Rejected because observer backpressure
  must not delay the send cadence. Repeated compact state supplies eventual repair.
- **Add an idle heartbeat:** Rejected because it adds wakes and traffic only to
  restate unchanged state; repair at the next send-path or final counter is enough.

**Consequences:** A channel row answers what is wrong now while Send outcomes and
the global log answer what happened during the run. One later counter corrects a
dropped failure or recovery edge, and the cumulative episode count keeps the
running send-failure contribution to the application-wide error tally exact even
when several complete episodes pass between observations. No second per-channel
error-history view is needed.

---

## ADR-057 — Outer checksums: MODBUS goes low byte first, and "CCITT" is named KERMIT

**Status:** Accepted 2026-09-30.

**Context:** Every outer checksum was appended high byte first. MODBUS RTU carries
its CRC low byte first: the check value of `123456789` is 0x4B37 and belongs on the
wire as `37 4B`. A receiver validating MODBUS frames would therefore reject every
message talker sent with that algorithm. Separately, the algorithm labelled
"CRC-16/CCITT" computes CRC-16/KERMIT (reflected, initial value 0, check value
0x2189). "CCITT" names at least three different CRC-16 variants — CCITT-FALSE
(0x29B1), XMODEM (0x31C3) and KERMIT — so the label could not tell a user which
one their device expects. Nothing is deployed, so no saved profile depends on
either behaviour.

**Decision:**

- CRC-16/MODBUS is appended low byte first. The other multi-byte algorithms keep
  high byte first.
- The KERMIT algorithm is named for what it computes: `Crc16Kermit` in code,
  `crc16_kermit` in profiles, and "CRC-16/KERMIT" in the GUI. The rename lands
  with the specification change to §7.1, which names the algorithm.
- The stored name changes, so the talker profile schema moves from 2 to 3. A
  version-2 profile is refused with the existing "recreate this profile" message;
  there is no migration (ADR-013).

**Boundary:** No other CRC variant is added, and there is no per-message byte-order
setting. Both wait until a real device needs one. `intentionally_wrong` still
corrupts the last appended byte, which for MODBUS is now the high byte.

**Alternatives considered:**

- **Keep storing `crc16_ccitt` and change only the label:** Rejected. With no
  deployed profiles there is nothing to protect, and a stored name that disagrees
  with the label is the same ambiguity moved into the file.
- **A byte-order setting per message:** Rejected for now. It adds a choice most
  users cannot answer, to fix one algorithm whose convention is fixed by its
  protocol.

**Consequences:** MODBUS RTU receivers accept talker's frames. A user choosing a
CRC-16 sees the variant's real name and can match it against a device manual's
check value.

---

## ADR-059 — A TCP client reconnects, counts possibly-partial writes, and drains replies

**Status:** Accepted 2026-09-30. Resolves the §12.1 open item on TCP reconnect.
Amended 2026-10-01: a drain that finds the peer has closed fails that send.

**Context:** The TCP client has no retry preparation. Once the peer closes or
restarts, every later write fails on the dead stream until someone restarts the
channel. §12.1 set two conditions for any fix: failed-write and withheld-send
accounting must survive, and an errored write must not be assumed to have
transferred nothing. Separately, talker never reads the socket. A device that
answers fills talker's receive buffer, and closing a socket with unread data
makes most stacks send a reset, which can discard the peer's own in-flight data.

**Decision:**

- **Reconnect.** At each retry point the runner's backoff allows (250 ms doubling
  to 5 s, §9.2), the client closes the failed stream and connects again with the
  existing 5 s timeout. The failed message is never resent; the next due message
  goes out on the new connection. Recovery closes the failure episode with one
  INFO line naming the address.
- **Possibly partial.** Writes track the bytes transferred. A write that fails
  after transferring some bytes counts as **possibly partial**, its own outcome
  beside failed, suppressed and missed. Serial reports it the same way (§4.2).
  Wire bytes are counted separately from complete messages. The send-outcomes
  line and the run summary show it.
- **Replies are drained.** Before each write, the client reads whatever the peer
  has sent, without blocking, discards it and counts it. Status shows "peer sent
  N bytes".
- **A closed peer fails the send** (amended 2026-10-01). When a drain reads
  end-of-stream, the peer has closed the connection. That send fails with
  nothing written, and the client reconnects at the next retry point. Writing
  anyway would put the message into a connection whose peer has gone and count
  it as sent; usually only a later write would fail. The cost falls on a peer
  that half-closes on purpose but keeps reading, which would see reconnects.

**Boundary:** No resend, no acknowledgement protocol and no server mode. Replies
are counted, not shown; seeing them is listener's job.

**Alternatives considered:**

- **Resend the failed message on the new connection:** Rejected. The receiver
  may already hold part of it, so a resend risks a duplicate that looks like
  valid data.
- **Leave replies unread:** Rejected, because of the reset on close and because a
  device that answers is worth knowing about.

**Consequences:** A TCP test run survives a receiver restart without a click. The
counts never claim a message was delivered whole when it may not have been.

---

## ADR-060 — Both CLIs run unattended under one contract

**Status:** Accepted 2026-09-30. Workspace-level; listener ADR-046 adopts it.

**Context:** Talker's CLI opens every channel or none, so one missing USB adapter
stops the whole run. Listener's gives up only when nothing starts. Neither
survives a device that enumerates late after a reboot, both stop only on Ctrl-C,
and neither exit code says whether a run was healthy. Both apps must run headless
under systemd and Task Scheduler.

**Decision:**

- **Start what can start.** A channel that fails to open is retried with backoff
  while the others run. `--require-all` keeps the old all-or-nothing behaviour.
- **Warn loudly.** "Loudly" means:
  - A WARNING banner on stderr that `--quiet` does not hide, naming the channel
    and the reason. It uses the word, never colour alone.
  - A reminder every 5 minutes while any channel is down, with how long it has
    been down and the latest reason.
  - A line when a channel recovers.
  - A final summary naming each channel's outcome.
- **Exit codes:**

  | Code | Meaning |
  |---|---|
  | 0 | Healthy, or every outage recovered (the summary lists them) |
  | 1 | Internal error |
  | 2 | Invalid profile, cannot start, or `--require-all` failed |
  | 3 | Degraded: a channel never started, its retries ran out, or it was down at shutdown |
  | 4 | Finalization incomplete |

- **Stop on every OS stop signal.** Ctrl-C, SIGTERM on Linux, and console close,
  logoff and shutdown on Windows all get the same graceful stop, bounded by a
  shutdown time limit.

**Boundary:** Each app keeps its own retry details and summary content. Neither
installs itself as a service; `deploy/` holds example definitions.

**Alternatives considered:**

- **Exit when any channel fails:** Rejected. A late USB adapter would then need a
  person to restart the service.
- **Nonzero exit only when nothing ran:** Rejected. A channel down for the whole
  run is a degraded run, and a supervisor should be told.

**Consequences:** A headless run outlives a late device and says what is down
while it is down. Scripts and service managers can act on the exit code.

---

## ADR-061 — One bounded log-file worker, shared as `wiredata-log`

**Status:** Accepted 2026-09-30.

**Context:** Listener needs a persistent event log (listener ADR-044) with the
same properties talker's GUI log file already has (ADR-006): a dedicated worker
behind a bounded, non-blocking handoff, a cumulative loss count, a gap line after
a loss, visible failure, session boundaries and a final flush. That worker is
about 350 production lines in talker's logging module. Two copies would drift on
exactly the details that make it trustworthy.

**Decision:** The worker moves into a new internal crate, `wiredata-log`
(`publish = false`), which both apps use.

- It owns the file worker thread, the bounded handoff, loss accounting, gap
  lines, failure reporting, daily rotation by local date, deletion of log files
  older than a caller-given age, and flush and close.
- Callers own everything else: the `tracing` layers and formatting, the level
  policy, what is logged, the folder and file prefix, and presentation.
- Talker keeps its current behaviour, including deleting no old files. Listener
  keeps 30 days.

**Boundary:** Like `wiredata-timing` and `wiredata-telemetry`, the crate stays
narrow. No logging policy moves into it.

**Alternatives considered:**

- **Copy the worker into listener:** Rejected. Two implementations of loss
  accounting drift, and the drift shows up only when a log is most needed.
- **`tracing-appender`'s non-blocking writer:** Rejected. It drops lines without
  a gap marker or a count anyone sees.

**Consequences:** The workspace gains a seventh crate. AGENTS.md lists it, with
its dependency rule, when it is created.

---

## ADR-062 — Talker profiles load strictly

**Status:** Accepted 2026-09-30. Amends ADR-013 and supersedes ADR-047's note on
ignored fields.

**Context:** Every profile field is `#[serde(default)]` and unknown keys are
ignored. A misspelled key — `include_ms` for `include_millis` — silently becomes a
default, and for an unattended run that is a wrong setting nobody sees. ADR-047
relied on that leniency so profiles still carrying `timing_mode` would load. A
missing `version` also loads as the current one.

**Decision:**

- **Unknown keys are refused.** The message names the key and where it is, for
  example "channel 1, message 2". Every profile struct gets serde's
  deny-unknown-fields attribute. Serde's support is limited with internally
  tagged enums (`tag = "type"`) and `flatten`, so each struct gets a test proving
  that a misspelled key is refused.
- **A missing `version` is refused**, with "add `version = 3`".
- Missing fields still take their defaults, so an additive change within a
  schema version still loads older files.

**Boundary:** No migration. Version-2 profiles are already refused by the move to
schema 3 (ADR-057).

**Alternatives considered:**

- **Warn and ignore unknown keys:** Rejected. In a CLI run the warning scrolls
  away and the wrong setting stays.

**Consequences:** A profile either means what it says or does not load, with a
message that points at the line to fix.

---

## Open questions

The following decisions are deferred until the relevant module is written. They are recorded here so they are not forgotten and so the eventual decision (in a future ADR or commit) can reference the context.

**OQ-1 — `nmea0183` path-vs-version dependency.** The `talker` crate currently depends on `nmea0183` via `{ path = "../nmea0183" }`. When `nmea0183` is published to crates.io (per ADR-001), this should become `{ path = "../nmea0183", version = "0.1" }` so that downstream consumers building against published versions resolve cleanly while in-workspace builds continue to use the local source. Defer until publication is imminent.

**OQ-2 — `toml` 1.x vs 0.8 API. — Resolved (v2.0).** `core::profile` was implemented against `toml = "1"` with no friction: `Profile::load` parses to a `toml::Value` to inspect the schema version before full deserialization, then `toml::from_str` / `toml::to_string_pretty` handle the round trip. The 1.x API surface was sufficient; the fallback to `"0.8"` was not needed. The workspace stays on `toml = "1"`.

**OQ-3 — `nmea0183` serde feature activation in `talker`. — Resolved (v2.0).** `core::profile` uses a **`talker`-side representation**: an NMEA message stores plain-string talker, sentence type, and fields plus Talker-owned checksum/live-time flags, not serialized `nmea0183` types. Those strings are parsed into library types at message compilation; a static sentence is serialized then, while ADR-029's live template serializes per send. The profile schema is therefore decoupled from the library's struct shapes, and the `talker` dependency on `nmea0183` does **not** enable the `serde` feature (`nmea0183 = { path = "../nmea0183" }`). The `serde` feature on `nmea0183` itself still exists and is still verified to compile, for the benefit of other potential consumers.

**OQ-4 — `nmea0183` library MSRV policy.** Moved to [`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md) — it concerns the library's publication policy. See ADR-008 above for the workspace MSRV context it builds on.

**OQ-5 — Theme preference: per-app or shared?** ADR-016 gave both apps the same dark/light toggle, but each persists its own choice through its own `eframe` storage, so putting talker in dark leaves listener light. Whether the two should follow one preference is unresolved, and so is where a shared value would live: `wiredata-ui` is chrome-only and depends on nothing at runtime, so it cannot own persisted state. Raised during the 2026-07-09 GUI merge; no decision has been taken.

New open questions should be added here as they arise during implementation.
