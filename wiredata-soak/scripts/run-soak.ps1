# Run one listener soak scenario on Windows and check it (listener ADR-051).
#
#   pwsh -File wiredata-soak/scripts/run-soak.ps1 -Channels 4 -Rate 100 -Seconds 259200 `
#        -Record Raw -Destination C:\soak,E:\soak -Out .\soak-run1
#
# Writes a profile of UDP Channels named soak00, soak01, ... on consecutive
# ports, starts listener's CLI, sends with soak-gen, samples listener's
# memory every 10 s, stops listener as Ctrl-C would, and verifies the .raw
# files. With several destinations, Channels take them in turn. Everything it
# measured is in <Out>\summary.txt; the exit code is 0 only if every check
# passed. The soak runbook, wiredata-soak/README.md, says how to run it.
#
# Build first:  cargo build --release -p listener --bin listener -p wiredata-soak

param(
    [int]$Channels = 4,
    [int]$Rate = 100,
    [int]$Seconds = 60,
    # Which recordings each Channel makes.
    [ValidateSet("Raw", "Display", "Both")][string]$Record = "Raw",
    # How many Channels also record Display; all of them unless set.
    [int]$DisplayChannels = -1,
    # Where recordings go, such as a local disk and a USB drive; Channels take
    # them in turn. Default: <Out>\rec.
    [string[]]$Destination = @(),
    [string]$Out = ".\soak-out",
    [int]$Port = 20000,
    [string]$Bin = ".\target\release",
    # A profile fragment to append, such as a serial Channel to unplug.
    [string]$ExtraProfile = "",
    # The soak budget: about 800 MiB at 16 Channels (ADR-048).
    [int]$MemoryBudgetMiB = 800
)
$ErrorActionPreference = "Stop"

New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path $Out).Path
if ($Destination.Count -eq 0) { $Destination = @(Join-Path $Out "rec") }
# Through `pwsh -File`, "C:\soak,E:\soak" arrives as one string.
$Destination = @($Destination | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
foreach ($d in $Destination) { New-Item -ItemType Directory -Force $d | Out-Null }
# Absolute, since listener resolves a relative one from its own directory.
$Destination = @($Destination | ForEach-Object { (Resolve-Path $_).Path })
$Bin = (Resolve-Path $Bin).Path
if ($DisplayChannels -lt 0) { $DisplayChannels = $Channels }
$names = 0..($Channels - 1) | ForEach-Object { "soak{0:D2}" -f $_ }

# ── The profile ───────────────────────────────────────────────────────────────
$profileText = "schema_version = 3`nname = `"soak`"`n"
for ($i = 0; $i -lt $Channels; $i++) {
    $raw = ($Record -ne "Display").ToString().ToLower()
    $display = (($Record -ne "Raw") -and ($i -lt $DisplayChannels)).ToString().ToLower()
    # Channel i records to destination i mod n.
    $dest = $Destination[$i % $Destination.Count] -replace '\\', '/'
    $profileText += @"

[[channels]]
name = "$($names[$i])"
kind = "Udp"
[channels.interface]
type = "Udp"
bind_address = "127.0.0.1"
port = $($Port + $i)
mode = "Unicast"
[channels.raw_recording]
enabled = $raw
destination = "$dest"
overwrite_policy = "AppendIfExists"
file_rotation = "Hourly"
[channels.display_recording]
enabled = $display
destination = "$dest"
overwrite_policy = "AppendIfExists"
file_rotation = "Hourly"
[channels.retention]
byte_limit = 65536

"@
}
if ($ExtraProfile) { $profileText += "`n" + (Get-Content -Raw $ExtraProfile) }
$profilePath = Join-Path $Out "profile.toml"
Set-Content -Path $profilePath -Value $profileText -Encoding utf8NoBOM

# ── Run ───────────────────────────────────────────────────────────────────────
$listener = Start-Process -FilePath "$Bin\listener.exe" -ArgumentList "--profile", $profilePath `
    -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput "$Out\listener.out" -RedirectStandardError "$Out\listener.err"
Start-Sleep -Seconds 3
$gen = Start-Process -FilePath "$Bin\soak-gen.exe" -NoNewWindow -PassThru `
    -ArgumentList "--to", "127.0.0.1:$Port", "--streams", $Channels, "--rate", $Rate, `
    "--seconds", $Seconds, "--manifest", "$Out\gen.txt" `
    -RedirectStandardOutput "$Out\gen.out" -RedirectStandardError "$Out\gen.err"

# Private bytes are what the process alone holds, so they show a leak.
$started = Get-Date
"seconds,private_bytes,working_set_bytes" | Set-Content "$Out\memory.csv"
while (-not $gen.HasExited) {
    $p = Get-Process -Id $listener.Id -ErrorAction SilentlyContinue
    if (-not $p) { break }
    $elapsed = [int]((Get-Date) - $started).TotalSeconds
    "$elapsed,$($p.PrivateMemorySize64),$($p.WorkingSet64)" | Add-Content "$Out\memory.csv"
    Start-Sleep -Seconds 10
}
$gen.WaitForExit()
Start-Sleep -Seconds 2

pwsh -NoProfile -File (Join-Path $PSScriptRoot "send-ctrl-c.ps1") -Id $listener.Id
if (-not $listener.WaitForExit(30000)) {
    $listenerCode = "did not stop within 30 s"
    Stop-Process -Id $listener.Id -Force
} else {
    $listenerCode = $listener.ExitCode
}

# ── Check ─────────────────────────────────────────────────────────────────────
$checks = [ordered]@{}
$checks["listener exit code 0"] = ($listenerCode -eq 0)

$memory = Import-Csv "$Out\memory.csv" | ForEach-Object {
    [pscustomobject]@{ Seconds = [int]$_.seconds; Private = [double]$_.private_bytes }
}
$peakMiB = [math]::Round((($memory | Measure-Object Private -Maximum).Maximum) / 1MB, 1)
$checks["peak private memory $peakMiB MiB within $MemoryBudgetMiB MiB"] = ($peakMiB -le $MemoryBudgetMiB)
$afterHour = @($memory | Where-Object Seconds -ge 3600)
if ($afterHour.Count -ge 2) {
    $hours = ($afterHour[-1].Seconds - $afterHour[0].Seconds) / 3600
    $growth = [math]::Round((($afterHour[-1].Private - $afterHour[0].Private) / 1MB) / $hours, 2)
    $checks["memory growth after the first hour $growth MiB/h under 1"] = ($growth -lt 1)
}

$files = @($Destination | ForEach-Object { Get-ChildItem $_ -File } |
        Where-Object { $_.Extension -in ".raw", ".disp" })
$largest = ($files | Measure-Object Length -Maximum).Maximum
$cap = 2GB
$checks["largest file $largest bytes within the 2 GiB cap"] = ($largest -le $cap)

if ($Record -ne "Display") {
    $verified = $true
    Set-Content "$Out\verify.txt" ""
    for ($d = 0; $d -lt $Destination.Count; $d++) {
        $pairs = for ($i = $d; $i -lt $Channels; $i += $Destination.Count) { "$($names[$i])=$i" }
        & "$Bin\soak-verify.exe" --manifest "$Out\gen.txt" --recordings $Destination[$d] `
            --logs "$env:LOCALAPPDATA\listener\logs" @pairs | Tee-Object "$Out\verify.txt" -Append
        if ($LASTEXITCODE -ne 0) { $verified = $false }
    }
    $checks["every sequence number recorded (soak-verify)"] = $verified
}
if ($Record -ne "Raw") {
    $disp = @($files | Where-Object Extension -eq ".disp")
    $withDisplay = @($names[0..($DisplayChannels - 1)] | Where-Object {
            $n = $_; @($disp | Where-Object { $_.Name.StartsWith("$($n)_") -and $_.Length -gt 0 }).Count -gt 0
        })
    $checks["a non-empty .disp file for each of $DisplayChannels Display Channels"] = ($withDisplay.Count -eq $DisplayChannels)
}

$summary = @(
    "soak run: $Channels channels x $Rate/s for $Seconds s, recording $Record, to $($Destination -join ', ')"
    "listener exit code: $listenerCode"
    "files: $($files.Count), $([math]::Round((($files | Measure-Object Length -Sum).Sum) / 1MB, 1)) MiB"
    ""
)
$failed = 0
foreach ($check in $checks.GetEnumerator()) {
    $word = if ($check.Value) { "PASS" } else { $failed++; "FAIL" }
    $summary += "$word  $($check.Key)"
}
$summary | Tee-Object "$Out\summary.txt"
exit [int]($failed -gt 0)
