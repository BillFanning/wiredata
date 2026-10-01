# TODO — Listener

Implementation reminders for the `listener` crate. Not architectural decisions
(those go in [`ADR.md`](ADR.md) or the spec). Workspace-level tasks live in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md).

**Reset for spec v2.0 (stream-only, ADR-010).** The v1 message-era checklist is
archived in git history (`git show 5a8f491:listener/docs/TODO.md`). Everything
below tracks the v1→v2 strip and the v2 feature set. Do not resurrect items from
the old list — Messages, extraction, decoding, and subsampling are gone.

Cross off items as they are completed. Add new ones inline as they come up.

---

## Spec 2.4.0 requirements not yet built (2026-09-30)

The spec states these; the code does not do them yet.

- [ ] **Unattended GUI (ADR-045, §9.1, §70).** No reconnect checkbox or prompt,
  and no resume registration.
- [ ] **Unattended CLI (ADR-046, §3.1).** The CLI still bails with "no
  channels started", stops on Ctrl-C only, and has no `--require-all`. Of the
  exit codes it sets only 0 and 4 (finalization incomplete); 2 and 3 are not
  yet distinguished, and an internal error exits 1.
- [ ] **TCP Listener disabled (ADR-047, §4.1, §16).** `tcp_listener_template` is
  still offered, and validation accepts the kind.
- [ ] **UDP bind scope and shared port (ADR-047, §15, §75).** No `shared_port`
  field, and the bind scope is not shown in words. `recv_buffer_bytes` has no
  4 MiB default, and the granted size is not reported.
- [ ] **Strict profiles and limits (ADR-048, §71, §72.1).** Unknown keys are
  ignored, and a missing `schema_version` loads as current. There are no upper
  limits, and `PipelineCapacities` is not validated.

## v1 → v2 strip (ADR-010) — DONE

The strip is complete and the workspace builds clean (`cargo test -p listener`,
`clippy -D warnings`, `fmt --check` all pass). Landed across commits `5aea4dc`
(decoder + `nmea0183` removal), `44008ed` (extract removal, pipeline collapse,
Message-model removal). Everything below this block is verified done:

- [x] Write the v2 invariant tests first (spec §150):
  - [x] byte-pattern find across receive-chunk boundaries (§50.2) — cross-chunk carry
        is now wired (see the feature-gaps entry below); a pattern split across two
        reads matches.
  - [x] idle-rule firing and re-arming
  - [x] UDP datagram boundary preserved as a reception/recording detail only
  - [x] raw recording byte-exactness (`.raw`)
  - [x] display recording reflects the rendered view (`.disp`)
  - [x] pause affects neither reception nor recording
  - [x] fan-out overflow: single backpressure edge (Transport→Pipeline), consumers drop/fault
  - [x] older-schema profile refused (`CURRENT_VERSION = 3` — bumped to 2 by the v2
        strip, then to 3 by the Raw/Display split, ADR-013)
- [x] Remove `extract/` (and the `MessageExtractor` seam in the pipeline)
- [x] Remove `decode/`; drop the `nmea0183` dependency from `listener/Cargo.toml`
- [x] Collapse the pipeline: transport → bounded queue → non-blocking fan-out
      (raw recorder, scrollback, display recorder, find/triggers, diagnostics) — §102
- [x] Replace message retention with **byte-bounded** stream scrollback (§80, §88);
      `retention/` keeps only `CountBounded` for events/warnings/errors
- [x] Remove subsampling and `.ssdat`; rename raw extension `.dat` → `.raw` (§53, §59)
- [x] Config schema: drop `extraction`/`decoder`/`subsample`/view `source`/`annotations`;
      byte-based `RetentionConfig`; **bump `schema_version`**, refuse v1 (§72)
- [x] Re-root Find & Triggers on the stream: `BytePattern` + `Idle`;
      matches anchored on `byte_offset` (§50.2). Cross-chunk carry now wired (a
      pattern split across two reads matches; see below).
- [x] GUI: remove framing selector, NMEA-decode toggle, Stream↔Messages source switch,
      per-message #/timestamp toggles; the Stream viewer is the only viewer
- [x] Events: drop `MessageReceived`; liveness stays byte-based (§137, §166)
- [x] Test cleanup: deleted extraction/decoder/NMEA/subsample tests; kept transports,
      recording, rotation, backpressure, control lines, reconnect, liveness. Also
      removed the `Message`/`MessageBytes`/`MessageMetadata` model and
      `MessageRetention`; renamed `core/message.rs` → `core/timing.rs` (keeps the
      still-needed `ChunkTime` / `ChunkTimestamp`).

## v2 feature gaps (after the strip)

### Timing and telemetry plan (2026-07-19)

- [x] **Post-read-to-pipeline timing (ADR-026).** UDP, TCP, and Serial capture
      `ChunkTime` before payload copying; the pipeline records a cumulative,
      fixed-size handoff-delay histogram and exposes it through stats/snapshots and
      the selected detail pane. The completed run remains visible after Stop.
- [x] **Measure total ingest processing duration (ADR-028).** Add one bounded
      histogram around `ChannelPipeline::ingest`; correlate it with handoff delay and
      ingest-queue peak. Split match/render/record stages only if the total proves
      material, avoiding a clock read around every minor operation by default.
- [x] **Recent handoff-delay window (ADR-027).** Ten fixed one-second histogram
      segments feed the labeled recent p99 while the cumulative run maximum and final
      recent state remain available at rest; no per-chunk event traffic. ADR-032 now
      gives this bounded numeric primitive one shared implementation while Listener's
      receive aggregates and measurement boundaries stay local.
- [x] **Processing-window and chunk-shape telemetry (ADR-028).** Total ingest
      processing now has the same rotating recent window as handoff timing. Chunk
      count, size distribution, and inter-read gaps remain cumulative at-rest truth
      and reuse the post-read monotonic capture.
- [x] **Transport-specific loss and stall context (ADR-029).** Serial accumulates
      backpressure episode count/duration; Linux exposes per-socket `SO_RXQ_OVFL`;
      unsupported counters remain distinct from a measured zero.
- [x] **Timing rule precision (ADR-030).** Idle rules use exact monotonic deadlines
      and cumulative/recent firing lateness. Windows requests 1 ms resolution only in
      the final 32 ms; Linux/macOS use native waits. Receive timing remains event-
      driven and unaffected.
- [x] **Advanced arrival timestamps (ADR-029).** UDP optionally requests Linux
      `SO_TIMESTAMPNS`; actual kernel and post-read fallback samples are counted
      separately. Serial/TCP and unsupported platforms retain explicit post-read time.
- [x] **Run summary and export (ADR-031).** The newest completed run retains exact
      bytes/chunks, diagnostics, queue peaks, timing/timer policy, transport health,
      and platform/build facts in an on-click versioned clipboard report.
- [x] **Decision-oriented diagnostics summary (ADR-033).** The selected-channel
      view leads with compact Transport, Pressure, and Pipeline rows plus
      exception-only Attention, while complete telemetry and caveats remain under
      collapsed details. Unsupported never becomes zero, no row claims no loss,
      and derived Attention creates no runtime `Diagnostic`; Listener owns all
      classification policy while `wiredata-ui` supplies only shared egui chrome.

- [x] Find & Triggers runtime: cross-chunk `BytePattern` scanner **with carry** —
      a pattern split across reads now matches, however many short reads it spans
      (`MatchRuleSet` keeps the newest bytes of the stream and scans
      `carry ++ chunk`, reporting only matches ending in the new chunk). Rules fire
      **per occurrence** (a chunk holding three `$GPGGA`s fires a GGA rule three
      times, each at its own `match_offset`) — pinned by
      `every_occurrence_in_a_chunk_fires`. **Measurement:** every boundary-split
      firing increments `match_boundary_saves`, surfaced in both `ChannelStats` and
      `ChannelSnapshot` (the how-often); the first split of a run also records a
      where/why event diagnostic. The carry dies with the pipeline on Stop/Start
      (pipelines are rebuilt per run).
- [x] Independent Raw/Display recording config (ADR-013, spec §79 → schema v3).
      `RecordingConfig`/`RecordingMode` split into `RawRecordingConfig` +
      `DisplayRecordingConfig`, each with its own destination/rotation/overwrite/
      timestamps; a channel can run both at once. GUI: Raw panel above Configure
      (live on/off + setup), Display under Configure. Pinned by
      `raw_and_display_recording_run_to_independent_destinations`.
- [x] Inline **`Mark` arrival annotations** in the live view and `.disp` (§50.2,
      ADR-016/ADR-025). A `Mark { timestamp: Some(..) }` splices compact local time
      or a checksum-bearing NMEA ZDA sentence (before/after) into the rendered text via
      `render_text_annotated`; the live viewer rebases the snapshot's `TriggeredMatch`
      onto the scrollback window via the Mark's **`view_offset`** (view/scrollback space —
      not `byte_offset`, which counts bytes a paused view skipped; ADR-017) and
      splices the same `MarkRender` text, so display and `.disp` match. `.raw` is
      untouched. `After` anchors on the complete match's final byte; CR/LF separators
      reset renderer continuation state. A minimal `BytePattern → Mark(+ts)` editor
      lives under Configure (Apply & Restart). Pinned by
      `timestamped_mark_splices_inline_time_into_disp_not_raw`,
      `zda_mark_splices_valid_custom_sentence_and_newline_only_into_display`,
      `after_mark_splices_after_the_complete_multibyte_match`, and multiline renderer
      tests. (On-screen **Highlight** byte-range styling was **dropped** as too
      complex — see ADR-015; ADR-016 explains why the inline timestamp does *not*
      inherit that cost.)
- [ ] (Optional) Draw a glyph for a **bare** `Mark` (no timestamp) in the live stream
      view. A bare `Mark` still only writes the `‹MARK …›` line into `.disp`; the live
      viewer doesn't render a marker for it. Low priority — kept simple deliberately.
- [x] Match-`Record` to a Display/`Both` target: `apply_pending_records` now routes
      `Display`/`Both` through the same lazy begin / clean finalize path as the live
      Display toggle (ADR-012's `set_display_recording`), with spawn-time
      `DisplayRecordingSettings` built from the channel's display-recording config +
      its primary view renderer. Pinned by
      `record_action_display_target_begins_and_stops_display_recording`,
      `record_action_both_target_drives_raw_and_display_together`, and the loopback
      `set_display_recording_toggles_display_recording_live_through_the_orchestrator`.
- [x] `DisplayViewConfig.hex_grouping` schema field (spec §45/§78) — `HexGrouping
      { bytes_per_group, groups_per_line }` added with `#[serde(default)]`, so
      profiles round-trip it (`b8960a1`). Pinned by `hex_grouping_round_trips_through_
      a_profile` + `a_profile_without_hex_grouping_loads_with_the_default`.
- [x] Consume `hex_grouping` in the Hex renderer (ADR-038, 2026-08-05). Both halves
      are wired, and they land in different places by design: `bytes_per_group` is
      **rendering** (`DisplayView::hex_bytes_per_group` → `render_hex` runs that many
      bytes together), so it reaches the `.disp`; `groups_per_line` is **layout**, so
      it sets the viewer's line length and `build_display_view` passes 0 — no hard
      wraps in a recording (ADR-018). `0` groups stays "fit to the display width"
      (§45). A `HexCursor` carries group position across chunks so read sizes are not
      visible in the spacing, and a Mark takes a row of its own so it cannot shift
      every column below it (ADR-042). GUI control under Configure display, in the
      schema's own units. Pinned by `hex_groups_bytes_between_separators`,
      `hex_grouping_is_chunking_invariant`, and
      `hex_lines_wrap_between_groups_not_inside_them`.
- [x] **Marks spliced twice on a delta boundary** — found while testing the above.
      `rebuild_rows` admitted a mark at one *past* the window end (a `Before` whose
      byte had not arrived), so it rendered ahead of its byte and again when that
      byte arrived. The bound is now half-open, matching `delta_annotations`. Pinned
      by `a_mark_on_a_delta_boundary_splices_once` across all three modes.
- [x] Live `Record` begin/stop without a restart (ADR-012). `Listener::set_recording`
      → `PipelineRequest::SetRecording` into `run_channel` → the pipeline's lazy
      begin / clean finalize path (shared with the match-rule `Record` action); GUI
      Record/Stop-recording button on the detail pane (`UiCommand::SetRecording`).
      A channel with a destination but `raw_recording.enabled = false` is armed but
      not auto-recording, so the toggle controls it. Pinned by
      `set_recording_begins_and_stops_raw_recording_live` (pipeline) +
      `set_recording_toggles_raw_recording_live_through_the_orchestrator` (loopback).
- [x] Expose the `.raw` timestamp sidecar in the UI (§57) — done 2026-08-05. A
      **Timestamp sidecar** checkbox in the Raw recording editor sets
      `RawRecordingConfig.timestamp_enabled`, which was previously config-only, so
      the writer never ran in practice. Raw-only by design: the sidecar keys times to
      byte offsets, which a rendered `.disp` has no stable offsets for. When on, the
      resulting file name is shown beside the checkbox (or `<recording>.idx` under
      rotation, where the name is minted per period). Pinned by
      `the_timestamp_sidecar_flag_reaches_the_recording_settings`.
      **Read-side tooling: decided against, for now.** The sidecar is plain text
      (`<offset>,<wall_nanos>` per line), so every tool already reads it; the help
      text now states the format, the per-block (not per-byte) capture point, and
      that a nanosecond field is not nanosecond accuracy. A bundled viewer pairing
      `.raw.idx` offsets against `.raw` bytes is a real feature with no spec section
      behind it — revisit with an amendment, not as a follow-up to this control.
- [x] **Appended sidecars indexed from zero** (ADR-039, external review 2026-08-05).
      `RawFileRecorder` counted from construction, so under `AppendIfExists` the
      index claimed offsets the bytes did not occupy. Append is the default, rotation
      is the default, and rotation coerces Refuse to Append — so a restart inside the
      current period hit it every time, while the `.raw` stayed byte-exact and hid it.
      Now `stream_offset`, seeded from the opened file's length. Pinned by three tests
      confirmed to fail against the old behaviour first.
- [ ] General match-rule editor UI: the `Idle`/`Record`/`Notify`/`PauseDisplay`
      conditions + actions. The `BytePattern → Mark(+timestamp)` subset now has a
      minimal editor (ADR-016); the rest still arrive only via profiles.
- [ ] Periodic `Every { interval }` match condition. Keep it out of the byte hot path:
      evaluate from the existing idle timer and define whether it fires while a
      channel is paused/stopped before adding it to profiles or the general editor.
- [x] `RuntimeCommand`'s role (§136) — resolved by ADR-012: removed the vestigial
      `core::RuntimeCommand` enum; the command surface is the `Listener` async method
      API (the GUI's `UiCommand` is the bridge transport). Spec §136 rewritten to
      match (v2.0.1).
- [x] Profiles: save/load wired end-to-end against the v2 schema (§67–§71)
      (`6b17021`). The GUI Profile menu (Save / Save As… / Load) routes through
      `UiCommand::{SaveProfile,LoadProfile}`; the driver gathers configs from the
      authoritative `Listener` for save, and on load parses+validates first then
      swaps the channel set (§70 — channels restore Stopped). Pinned by
      `save_then_load_round_trips_the_workspace_through_the_driver` +
      `loading_a_missing_profile_errors_without_touching_the_workspace`.
- [ ] Export (stream scrollback → file, §60–§63) — after GUI settles
- [ ] (YAGNI for now) Diagnostic ordering within one timer tick. `Diagnostic` carries
      only `SystemTime`; on a coarse clock (Windows ~15 ms) two entries can share a
      timestamp and the headline (newest) then orders by severity bucket, not insertion.
      Not observable in practice (lifecycle/recording entries are I/O-separated by ≫ a
      tick). If it ever bites, add a monotonic per-channel sequence to `Diagnostic` and
      sort by it.
- [x] Recording destination uniqueness (ADR-014, spec §6/§55/§71/§121). Two layers:
      (1) **unique Channel Names** — `Profile::validate` flags duplicates
      (`DuplicateChannelName`); GUI add uses a per-kind monotonic, never-reused suffix
      (`UDP_Channel1`…); rename won't commit a duplicate (inline warning); profile load
      rejects duplicate-named channels via the per-channel validation. (2) **advisory
      lock** — `lock_recording_destination` takes std's `File::try_lock` on a `<path>.lock`
      companion (a *sync* std file, so the lock releases deterministically on drop, unlike
      a tokio file), held by `RawFileRecorder`/`DisplayFileRecorder` for the recording's
      lifetime; a conflict is `RecordError::DestinationInUse` → `RecordingFaulted`, channel
      stays Running. An in-process named pre-check at Start was prototyped and removed (it
      faulted the whole channel + showed the bind/port recourse). Tests: name-uniqueness
      validation; recorder lock-conflict; orchestrator two-channel stays-Running.

## Carried over (still valid under v2)

- [ ] Disk-guard GUI exposure (§56.2): neither recording editor sets
      `disk_guard` yet, so a guard comes only from a profile.
- [ ] **TCP connection channels are currently unobservable** — now NORMATIVE
      deferred scope (spec v2.1 §16.2 / ADR-024, 2026-07-16; parked by user
      decision 2026-07-11 — stays deferred until a real TCP-inspection need
      shows up; UC1 today is serial/UDP). Design when promoted lives in ADR-024:
      per-connection handle registry keyed by the minted `ChannelId`,
      `Listener::snapshot`/`stream_delta` routed through it, and a GUI decision
      (sub-tabs vs. dynamic top-level channels).
- [ ] Per-connection recording for TCP connection channels (§16.2, deferred §59 naming)
- [ ] RS-422/485 phases (§14.4)
- [ ] CLI parity with GUI for the v2 surface
- [ ] TCP `recv_buffer_bytes` is persisted + specified (§76) but **ignored at runtime**
      — now normatively deferred with the rest of the connection-channel surfacing
      (spec v2.1 §16.2 / ADR-024 / Appendix A). UDP maps it (`build_udp` →
      `with_recv_buffer`, `build.rs`); `build_tcp_listener` passes only the address and
      `TcpListenerTransport` stores no buffer, so SO_RCVBUF is never applied to accepted
      connections. When promoted: wire it through accept (socket2), like UDP.
- [x] Multi-view pause ambiguity — DECIDED (spec v2.1 §48 / ADR-023,
      2026-07-16): one logical Display View per Channel is normative, with
      Raw/Rendered/Hex as its modes; pause is that view's pause, full stop.
      Multiple simultaneous views moved to Appendix A (reviving them needs
      per-view render/pause state). The schema's `views` list stays; entries
      beyond the first are ignored. Internal `Vec<PipelineDisplayView>` may be
      simplified opportunistically.
- [x] `.disp` per-chunk rendering garbled a multi-byte character split across two
      reads, and injected a newline per read chunk — both fixed by the streaming
      renderer (ADR-018): the `.disp` is now the exact rendered stream (no hard
      wraps ever; soft wrap is the viewer's job). Pinned by
      `stream_renderer_is_chunking_invariant` +
      `disp_is_the_exact_rendered_stream_across_read_boundaries`.

## Robustness & performance (external review, 2026-07-11)

- [x] **Stopped channels keep their exact byte totals at rest** (live-testing
      find, 2026-07-11). `finish_stop` now retains the final snapshot's
      liveness (`retained_activity`, rate zeroed; + `retained_boundary_saves`)
      alongside the diagnostics, and the stopped-channel `retained_snapshot`
      serves them — previously it served zeroed defaults, so the GUI's next
      poll wiped the byte total exactly when the user cross-checks it against
      the sender. GUI side: totals now also reset on *Start* (talker
      semantics), not on Stop. Pinned by
      `stopped_channel_retains_exact_totals_at_rest` +
      `totals_survive_stop_and_reset_on_start`.

- [x] **Command acks** — DONE (`6d4da3b`). A dropped `UiCommand` now raises the
      dismissable top banner (`show_command_drop_banner`, auto-expires after
      `COMMAND_DROP_NOTICE_TTL`) in addition to the tracing warning — the
      user sees that the last click did nothing. Talker's sibling landed in
      the same commit (talker TODO).
- [x] **Scrollback-eviction churn — MEASURED 2026-07-12 (re-measured after a
      bench fix), KILLED.** The first "floor" bench reused one pipeline until
      it filled to the cap, so it compared the capped state to itself
      (external review round 2 caught it). Fixed floor (fresh warmed pipeline
      per iteration): below-cap 460 ns vs at-cap 436 ns per 64-byte chunk,
      same-run, overlapping CIs — eviction is genuinely free. Absolute
      numbers swing ~3× with ambient load; only same-run comparisons count.
- [ ] **Match-scanning cost — MEASURED 2026-07-12, PARKED with threshold.**
      Linear at ~195 ns/rule/chunk (400 ns @ 1 rule, 6.4 µs @ 32, 64-byte
      chunks): 32 rules at 1k chunks/s is 0.6% of a core. Aho–Corasick (plus a
      separate idle-rule list) is only worth building if rule counts pass ~50
      or sustained chunk rates pass ~2k/s — revisit against baseline `main` if
      either becomes real. **Dense-match addendum (2026-07-12): the parking
      survives firing costs.** Same-run: 1 rule firing every chunk ~420 ns vs
      ~336 ns never-matching (+~85 ns/firing); 8 rules firing ~1.84 µs vs
      ~1.52 µs (+~40 ns/firing amortized); a `Notify` action adds ~330 ns per
      firing (its diagnostic record). Even an every-chunk Notify at the 2k
      chunks/s threshold is ~0.15% of a core — firing overhead is the same
      order as the scan it rides on and cannot move the threshold.
- [x] **Selected-channel diagnostics clone+sort — MEASURED 2026-07-12,
      KILLED.** At the retained cap (seeded past every per-severity bound):
      `snapshot/full-at-diagnostics-cap` ~82 µs, the GUI's per-arrival
      `clone` + `into_sorted_vec` ~18 µs. At the 5 Hz selected-channel poll
      that is ~0.5 ms/s ≈ 0.05% of a core — half the filed estimate and ~20×
      under the ~1% action rule. Sequence-numbers/deltas are not justified;
      the once-per-arrival `Rc` cache already keeps it off the frame path.
- [ ] **One round-trip for the on-screen channel** — a request-count nicety,
      not a measured cost: fold into whatever next touches `PipelineRequest`;
      don't do standalone.
- [ ] **Doc-drift batch (fold into the next spec/doc pass, no bump alone):**
      removed on-screen Highlight still appears in requirements prose (ADR-015
      dropped it); stale scrollback-size claims vs. the current default; ADR
      text still says the app lacks a dark theme (dark mode landed, Phase 7,
      `66cb1f1`).

## Robustness (external review round 2, 2026-07-12)

- [x] **Sidecar opens before the main destination** — a failed Raw begin can
      no longer have created/truncated the main `.raw` (only the derived
      `.idx` is at risk on the inverse edge).
- [x] **Transport fault cause is no longer discarded** —
      `TransportNotice::TransportFaulted { channel_id, cause }` carries the
      `TransportOutcome::Faulted` string from the fault monitor (and the TCP
      supervisor, per connection) into the pipeline's diagnostics as an error;
      `run_channel` exits only when BOTH its ingest and notices channels close,
      so the cause can't race the pipeline's drain. Unlike live advisory stall
      notices, terminal delivery awaits bounded queue capacity after reception
      ends, so saturation cannot discard the reason. Pinned by
      `spontaneous_transport_fault_emits_channel_faulted` (cause reaches retained
      diagnostics) + `terminal_fault_waits_for_space_in_a_full_notice_queue`.
- [x] **Lifecycle state self-corrects via the polled surfaces** —
      `ChannelStats`/`ChannelSnapshot` now carry `state` (orchestrator-stamped
      `effective_state()`) + `reconnect_pending`, and `snapshot`/`channel_stats`
      always serve for a known channel (live when running, synthesized from the
      retained diagnostics/activity otherwise) — so both the overview (stats)
      and detail (snapshot) lanes re-derive status every poll. The reducer
      reconciles on every polled update (transitional Starting/Stopping left to
      settle). ADR-006 carries the dated correction (its claim was aspirational
      until now). Pinned by
      `a_dropped_lifecycle_event_self_corrects_on_the_next_poll`,
      `polled_fault_with_reconnect_pending_reads_reconnecting`,
      `transitional_polled_states_do_not_flap_the_row`, and the retained-stats
      assertions in `stopped_channel_retains_exact_totals_at_rest`.
- [x] **Typed per-tap recording events** — `RecordingStarted/Faulted` carry
      `RecordingTap::{Raw, Display}`; the reducer tracks which lane
      `last_error` refers to (`ChannelView::recording_fault`) and both clear
      paths (same-lane `RecordingStarted`, same-lane polled Enabled) are
      tap-aware — a Raw start/health no longer clears a Display fault, and a
      command error is never cleared by recording health at all. Pinned by
      `recording_fault_clearing_is_tap_aware` +
      `polled_recording_state_clears_only_the_faulted_lane`.
- [x] **Serial control-line poll cadence** — input lines (CTS/DSR/DCD/RI) now
      poll at `CONTROL_LINE_POLL_INTERVAL` (100 ms) instead of four driver
      ioctls before every read; RTS/DTR commands still apply every pass and
      re-poll immediately. Done without a dedicated bench: the change is a
      strict bounded reduction (~40 ioctls/s worst case vs. 4×chunk-rate).
      Pinned by `input_line_polling_is_throttled_not_per_read`.
- [x] **Dense-match bench case** — landed (`ingest/64B/{1,8}-rules-firing-
      every-chunk`, `1-rule-notify-every-chunk`); numbers and the confirmed
      parking verdict are in the match-scanning item above.
- [ ] **Frame-time items** (with the talker row-ring methodology): mark-string
      hashing per repaint and repeated `make_contiguous` in
      `gui/detail/stream_view.rs` — generation/dirty-offset tracking instead.
- [ ] **Soak the Serial stall cell under sustained backpressure**
      (`SerialStallState` / `retry_stalled_send`, ADR-034). The episode edges,
      poison recovery, and the unwind guard have unit tests; a full window with
      the Transport→Pipeline queue held full does not. Tracked with the rest of
      the soak suite in
      [`talker/docs/TODO.md`](../../talker/docs/TODO.md) — the `#[ignore]`d
      tests in `listener/tests/soak.rs` are its home.

## Future work — deferred (spec Appendix A)

- Protocol decoders / field extraction (any return would be a new ADR)
- CSV / JSON / structured-semantic export; session replay
- Plugin architecture; TCP client mode; distributed operation
- Size-based file rotation / old-file pruning / filename templating
- Pre-trigger / pre-match recording capture (§50.2)
- Persistent diagnostic log rotation
- Hard real-time guarantees
