# TODO — Listener

Work the `listener` code does not do yet. Decisions go in [ADR.md](ADR.md) and
behaviour in the [specification](listener_specification.md), whose Appendix A
lists what is deferred on purpose. Workspace-level tasks live in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md).

Each item names the symbol or section where the work lands, so it can be found
again. Remove an item when it is done: the commit says what changed.

---

## Features

- [ ] **Check Windows logoff while a serial port is slow to open, by hand
  (ADR-049, ADR-052).** While the CLI is still opening a slow serial port, a
  Windows logoff should stop it and finalize the recordings already running.
  An automated test covers the code path; this checks the real OS. Optional:
  do it when convenient.
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

## Reconnect

- [ ] **A give-up limit, only if asked for (ADR-072).** Reconnecting never
  stops by itself now. If someone needs a Channel to give up, add one key to
  `ReconnectPolicy` and one field to the editor.

The retry questions shared with talker are in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md), under Retry and backoff.

## Soak

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
