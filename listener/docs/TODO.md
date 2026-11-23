# TODO — Listener

Work the `listener` code does not do yet. Decisions go in [ADR.md](ADR.md) and
behaviour in the [specification](listener_specification.md), whose Appendix A
lists what is deferred on purpose. Workspace-level tasks live in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md).

Each item names the symbol or section where the work lands, so it can be found
again. Remove an item when it is done: the commit says what changed.

---

## Features

- [ ] **Linemon gives no reason when its serial port is unplugged (§50.3).**
  On 2026-11-23, unplugging COM3 while Linemon ran read "COM3 stopped: no
  reason was reported." `RunFacts::from_snapshot` takes the reason from the
  snapshot's newest error diagnostic, and the faulted snapshot had none. Find
  whether the serial read's failure is not recorded as a diagnostic, or is
  recorded after the last snapshot Linemon receives, and check what a Listener
  Channel shows for the same unplug.
- [ ] **Does XON/XOFF hide 0x11 and 0x13 from the stream? (§14, ADR-010)**
  Listener records the bytes as received, but with XON/XOFF on the driver may
  take the XON and XOFF bytes out of the received data. Check with the
  null-modem pair: talker sends a message with `‹11›` and `‹13›` in it,
  Listener receives with XON/XOFF on. If they vanish, say so where XON/XOFF is
  chosen, or decide whether a receive-only tool should offer it.
- [ ] **Check Windows logoff while a serial port is slow to open, by hand
  (ADR-049, ADR-052).** While the CLI is still opening a slow serial port, a
  Windows logoff should stop it and finalize the recordings already running.
  An automated test covers the code path; this checks the real OS. Optional:
  do it when convenient.
- [ ] **Disk guard in the GUI (§56.2).** Neither recording editor,
  `edit_raw_recording` nor `edit_display_recording`, sets `disk_guard`, so a
  guard comes only from a profile. With log files kept (talker ADR-069) and
  recordings unbounded, a full drive is the main risk of a weeks-long run, so
  this is the disk work to do first.
- [ ] **Decide the wider disk work (needs a spec amendment).** Whether a new
  recording gets a disk guard by default; whether the event log folder
  (`event_log_folder`) is watched too; and how a low drive is shown when no
  recording has a guard. Talker's logs have the same question; see talker's
  File-log retention item.
- [ ] **A general match-rule editor (§50.2).** `edit_mark_rules` edits only a
  `BytePattern` rule with a `Mark` action (ADR-016). `Idle` conditions and the
  `Record`, `Notify` and `PauseDisplay` actions arrive only through profiles.
- [ ] **A glyph for a bare `Mark` (optional).** A `Mark` with no timestamp writes
  its `‹MARK …›` line into the `.disp`, but the live view draws nothing for it.
  Low priority: kept simple on purpose.
- [ ] **RS-422 and RS-485 (§14.5).** They follow RS-232; nothing is specific to
  them yet beyond the RTS line of §14.3.

## UDP modes

- [ ] **Simplify the UDP receive modes (§15, ADR-047).** On receive, Broadcast
  and Unicast differ only in the socket's broadcast option and in whether Request
  shared port is offered (`udp_mode_and_sharing`). A test on 2026-11-19 found a
  socket in Unicast mode bound to All interfaces receives both kinds of broadcast;
  one bound to a single address missed 255.255.255.255, but the sender was the
  same computer, so check with a second device. Options: merge the two and keep
  Multicast; or one mode with an optional Group. Do it in Linemon and the channel
  editor together, not Linemon alone: they share the widget, and Linemon is where
  a shared port is most used. Port sharing is the catch. A unicast datagram
  reaches only one of the programs sharing a port, so a merged mode either drops
  sharing or needs a warning when unicast arrives on a shared port, which means
  reading each datagram's destination address (`IP_PKTINFO`, `WSARecvMsg`, or
  `IP_RECVDSTADDR` on macOS).
- [ ] **Check whether a shared port can take another program's datagrams on
  Windows.** A socket that sets address reuse may be able to bind a port that
  another socket holds without it, unless that socket set exclusive use. If so,
  Linemon in Broadcast with sharing ticked could take unicast datagrams from a
  Listener channel on the same port, and Listener should set exclusive use on
  sockets that do not share. Test before the item above.

## Reconnect

- [ ] **A give-up limit, only if asked for (ADR-072).** Reconnecting never
  stops by itself now. If someone needs a Channel to give up, add one key to
  `ReconnectPolicy` and one field to the editor.

The retry questions shared with talker are in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md), under Retry and backoff.

## Specification

- [ ] **Name the lock files in §121 at the next spec bump.** §121 says each
  recording holds an advisory lock. It doesn't say the lock is a separate
  `.raw.lock` or `.disp.lock` file that stays in the folder after recording stops
  (`lock_file`). `deploy/README.md` tells operators this. A spec change needs a
  version bump, so this waits for the next one.

## Soak

- [ ] **The Serial stall cell under sustained backpressure (ADR-034).**
  `SerialStallState` and `retry_stalled_send` have unit tests for their edges,
  poison recovery and unwind guard, but no run holds the Transport→Pipeline
  queue full for a whole window. The soak harness (ADR-051) sends UDP only, so
  this needs its own test in `listener/tests/soak.rs`: hold a bounded ingest
  receiver without draining it, run the `BlockingReader` seam at rate, and check
  that `completed_episodes` only grows, `completed_total` never exceeds wall
  time, `active_for` is set while stalled and clear within one snapshot of
  resuming, and the final snapshot has no episode open. A missed edge shows up
  as a stall that grows forever, so a long window is the point.
- [ ] **Slow-disk recording with rotation.** Needs an injectable slow writer
  rather than a real disk; do it with whatever next touches the recorder's
  writer seam.
- [ ] **Soak runs 1 and 2 (ADR-051).** The procedure and pass criteria are in
  [the soak runbook](../../wiredata-soak/README.md). The Linux runner has not
  been run yet.
- [ ] **High-rate recording, if run 2 needs it.** `RawFileRecorder` and
  `DisplayFileRecorder` write through a Tokio `BufWriter`. Each write that reaches
  the file goes through Tokio's blocking pool. If run 2 shows recording gaps or
  recorder-queue drops at 16 × 1,000/s, there are two candidates: batching
  writes, or giving each recording its own writer thread. A faster matcher is a
  separate question, settled by ADR-053 and its threshold.
