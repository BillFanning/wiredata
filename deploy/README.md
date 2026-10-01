# Running listener unattended

These are examples, not an installer. `listener` does not install itself as a
service. Copy the file for your system, change the paths, and register it.

Talker's examples will follow when its CLI adopts the same contract.

## Before you start

- **Use a profile.** Save one from the GUI and run it once by hand with
  `listener --profile <file>` to check that every channel starts.
- **Turn reconnect on** for every channel that records. A channel that fails at
  start, or loses its device, is then retried while the others run. At start,
  listener warns about each recording channel that has reconnect off.
- **Use absolute paths** for recording destinations. The service does not run
  from your working folder.

## Linux: systemd

`linux/listener.service` runs listener as a `listener` user:

```sh
sudo useradd --system --create-home --home-dir /var/lib/listener \
    --groups dialout listener
sudo cp linux/listener.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now listener
```

- **Stop:** `sudo systemctl stop listener` sends SIGTERM. Listener finishes its
  files, prints "stopped" and a summary, and exits.
- **Output:** `journalctl -u listener`.
- **Event log:** `/var/lib/listener/.local/share/listener/logs`, one file per
  day, kept 30 days.
- **Removable drive:** uncomment `RequiresMountsFor=` in the unit, so the
  service waits for the drive.

## Windows: Task Scheduler

`windows/listener-task.xml` starts listener as SYSTEM 30 s after Windows starts,
whether or not anyone signs in. Register it from an elevated PowerShell:

```powershell
Register-ScheduledTask -TaskName "wiredata listener" -Xml (Get-Content -Raw .\windows\listener-task.xml)
```

- **Stop:** a restart or shutdown of Windows asks listener to stop, and it
  finishes its files first. Task Scheduler's **End** command terminates it
  without finishing its files. The files stay consistent up to the last flush
  (at most 1 s), and the next run repairs the timestamp index.
- **Event log:** running as SYSTEM, it is in
  `C:\Windows\System32\config\systemprofile\AppData\Local\listener\logs`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Healthy, or every outage recovered |
| 1 | Internal error |
| 2 | Invalid profile, nothing could start, or `--require-all` failed |
| 3 | Degraded: a channel never started, ran out of retries, or was down at the stop |
| 4 | A recording could not finish its files at the stop |

Codes 3 and 4 appear when the run is stopped. The summary printed just before
says which channel and why.
