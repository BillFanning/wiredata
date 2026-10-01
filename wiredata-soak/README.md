# Soak runbook

How to run listener's soak tests and decide whether they passed. The harness is
listener ADR-051; this page is the procedure. Soak runs stay out of CI: they take
hours to days and need real disks and devices.

## What a run proves

A run passes when every one of these holds:

| Check | How it is measured |
|---|---|
| Every sequence number sent is recorded, except inside a gap listener logged, with nothing repeated or out of order | `soak-verify` |
| Peak memory stays within budget: about 800 MiB at 16 Channels (ADR-048) | memory sampled every 10 s |
| After the first hour, memory grows by less than 1 MiB per hour | the same samples |
| Every recording file stays within its size cap (2 GiB by default) | the largest file |
| listener's CLI exits 0: healthy, or every outage recovered (recording gaps alone do not change this) | its exit code |

The runner scripts make all of these checks and write the verdicts to
`<out>/summary.txt`. The scripts exit 0 only if every check passed.

The generator is `soak-gen`, not talker (ADR-051), so talker's CLI is not part
of these runs. Its exit codes are covered by talker's own CLI tests.

## Before you start

1. Build the release binaries from the repository root:

   ```text
   cargo build --release -p listener --bin listener -p wiredata-soak
   ```

2. **Linux only:** let a socket have a 4 MiB receive buffer. Otherwise listener
   warns that it got less, and run 2 is likely to drop datagrams in the kernel:

   ```text
   sudo sysctl -w net.core.rmem_max=8388608
   ```

3. Check the disk space: run 1 writes about 6.5 GB, run 2 about 90 GB.

4. The runs use loopback ports 20000 upward, one per Channel. Nothing else may
   use them.

5. Close any listener GUI that is running. Its event log is the same files the
   verifier reads.

## The runners

- **Windows:** `pwsh -File wiredata-soak/scripts/run-soak.ps1`, with options
  `-Channels`, `-Rate` (datagrams a second per Channel), `-Seconds`,
  `-Record Raw|Display|Both`, `-DisplayChannels`, `-Destination` (one or more
  folders, comma-separated), `-Out`, `-Port`, and `-ExtraProfile`.
- **Linux:** `wiredata-soak/scripts/run-soak.sh`, with the same options as
  `-c -r -s -m raw|display|both -D -d -o -p -x`. **Not yet run on Linux:**
  watch its first short run closely.

Each runner writes a profile of UDP Channels `soak00`, `soak01`, …, starts
listener's CLI, sends with `soak-gen` for the time given, stops listener as an
operator would (Ctrl-C on Windows, SIGTERM on Linux), then checks. Its output
folder holds:

- `summary.txt`: the verdicts.
- `verify.txt`: per Channel, records, files and gaps, or the first findings.
- `memory.csv`: the memory samples.
- `gen.txt`: what was sent.
- `listener.err`: the CLI's warnings and its end-of-run summary.
- `profile.toml`: the profile it ran.

Listener's event log, which shows every gap and recovery, is in
`%LOCALAPPDATA%\listener\logs` on Windows and `~/.local/share/listener/logs` on
Linux.

On Windows, `wiredata-soak/scripts/send-ctrl-c.ps1 -Id <pid>` stops any console
process as Ctrl-C would, if you run the pieces by hand.

## Short runs (PLAN 7.3)

A minute each, to shake out the setup before committing days to it:

```text
pwsh -File wiredata-soak/scripts/run-soak.ps1 -Channels 4 -Rate 100 -Seconds 60 -Record Raw     -Out .\short-raw
pwsh -File wiredata-soak/scripts/run-soak.ps1 -Channels 4 -Rate 100 -Seconds 60 -Record Display -Out .\short-display -Port 20100
pwsh -File wiredata-soak/scripts/run-soak.ps1 -Channels 4 -Rate 100 -Seconds 60 -Record Both    -Out .\short-both -Port 20200
```

Done on Windows 2026-10-01: all three passed. So did one minute at run 2's
rate (16 × 1,000/s, Display on one Channel): 16 × 60,003 records, nothing
missing, 12.7 MiB peak private memory.

## Run 1: expected load (PLAN 7.4)

72 hours, 4 Channels × 100 datagrams a second, about 6.5 GB, on Windows and then
on Linux. Two Channels record to a local disk and two to a USB drive. A serial
Channel runs beside them, so it can be unplugged.

1. **Serial Channel.** Write a fragment for a serial device that sends steadily:
   a GPS, or talker over a null-modem pair. Turn reconnect on so it recovers
   from the unplug. On Linux, use the `/dev/serial/by-id/` path, which survives
   a replug:

   ```toml
   [[channels]]
   name = "serial"
   kind = "Serial"
   [channels.interface]
   type = "Serial"
   port = "COM5"
   baud_rate = 4800
   [channels.raw_recording]
   enabled = true
   destination = "C:/soak"
   file_rotation = "Hourly"
   [channels.retention]
   byte_limit = 65536
   [channels.reconnect]
   enabled = true
   ```

2. **Start the run.** The destinations are a local folder and a folder on the
   USB drive:

   ```text
   pwsh -File wiredata-soak/scripts/run-soak.ps1 -Channels 4 -Rate 100 -Seconds 259200 -Record Raw `
        -Destination C:\soak,E:\soak -ExtraProfile .\serial.toml -Out .\run1
   ./wiredata-soak/scripts/run-soak.sh -c 4 -r 100 -s 259200 -m raw \
        -d /srv/soak,/media/usb/soak -x ./serial.toml -o ./run1
   ```

3. **USB unplug, once, after the first hour.** Pull the USB drive without
   ejecting it, wait two minutes, and plug it back in. It must return at the
   same place: the same drive letter on Windows, the same mount point on Linux
   (mount it again if nothing does that automatically). In the event log,
   expect a gap on `soak01` and `soak03` saying the recording folder is not
   there, then "resumed in a new file" once the drive is back. Recording never
   resumes on the system disk underneath an empty mount point: the destination
   marker prevents it.

4. **Serial unplug, once, at another time.** Unplug the serial adapter for one
   minute and plug it back in. Expect the serial Channel to fault, then
   reconnecting and reconnected lines.

5. **At the end,** the runner stops listener and checks. The serial Channel's
   data carry no sequence numbers, so it is not verified. Its recovery shows in
   the exit code and the event log.

**Expected:** every check PASS. In `verify.txt`, the USB Channels show the gap as
excused, for example `excused by 1 logged gaps`. The local Channels show none.

## Run 2: headroom (PLAN 7.5)

24 hours, 16 Channels × 1,000 datagrams a second, about 90 GB of Raw, with
Display on one Channel, on Windows and then on Linux, to a local disk:

```text
pwsh -File wiredata-soak/scripts/run-soak.ps1 -Channels 16 -Rate 1000 -Seconds 86400 -Record Both `
     -DisplayChannels 1 -Destination D:\soak -Out .\run2
./wiredata-soak/scripts/run-soak.sh -c 16 -r 1000 -s 86400 -m both -D 1 -d /srv/soak -o ./run2
```

**Expected:** every check PASS, no gaps, and peak memory well under 800 MiB.

## When a check fails

- **Missing inside a file:** datagrams that never reached the recording. On
  Linux, the Diagnostics panel's "UDP kernel receive-queue drops" says whether
  the socket dropped them; raise `net.core.rmem_max`. Otherwise it is a
  listener fault.
- **Missing before a file, with no gap logged:** a new file began without a
  logged gap. That is a listener fault, since segments must be contiguous.
- **Duplicate or reordered:** a listener fault.
- **Memory over budget or still growing:** keep `memory.csv` and the profile.
- **Exit code other than 0:** `listener.err` ends with the run summary, which
  names each Channel's outcome.

Keep the whole output folder and the event log files with any report.
