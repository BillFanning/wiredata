#!/usr/bin/env bash
# Run one listener soak scenario on Linux and check it (listener ADR-051).
#
#   wiredata-soak/scripts/run-soak.sh -c 4 -r 100 -s 259200 -m raw \
#       -d /srv/soak,/media/usb/soak -o ./soak-run1
#
# The Linux counterpart of run-soak.ps1: writes a profile of UDP Channels
# named soak00, soak01, ... on consecutive ports, starts listener's CLI, sends
# with soak-gen, samples listener's anonymous memory every 10 s, stops it with
# SIGTERM, and verifies the .raw files. With several destinations, Channels
# take them in turn. Everything it measured is in <out>/summary.txt; the exit
# code is 0 only if every check passed. The soak runbook,
# wiredata-soak/README.md, says how to run it.
#
# Build first:  cargo build --release -p listener --bin listener -p wiredata-soak
#
# Options: -c channels (4), -r datagrams per second per channel (100),
# -s seconds (60), -m raw|display|both (raw), -D how many channels also record
# Display (all), -d destinations, comma-separated (<out>/rec), -o out (./soak-out),
# -p first port (20000), -b binaries (./target/release), -B memory budget in MiB (800),
# -x a profile fragment to append, such as a serial Channel to unplug.

set -u

channels=4 rate=100 seconds=60 record=raw display_channels=-1
destinations="" out=./soak-out port=20000 bin=./target/release budget_mib=800 extra=""
while getopts "c:r:s:m:D:d:o:p:b:B:x:" opt; do
    case $opt in
        c) channels=$OPTARG ;;
        r) rate=$OPTARG ;;
        s) seconds=$OPTARG ;;
        m) record=$OPTARG ;;
        D) display_channels=$OPTARG ;;
        d) destinations=$OPTARG ;;
        o) out=$OPTARG ;;
        p) port=$OPTARG ;;
        b) bin=$OPTARG ;;
        B) budget_mib=$OPTARG ;;
        x) extra=$OPTARG ;;
        *) echo "see the options at the top of $0" >&2; exit 2 ;;
    esac
done
case $record in raw | display | both) ;; *) echo "-m is raw, display or both" >&2; exit 2 ;; esac

mkdir -p "$out" && out=$(cd "$out" && pwd)
bin=$(cd "$bin" && pwd)
[ -n "$destinations" ] || destinations="$out/rec"
IFS=, read -r -a dests <<<"$destinations"
for i in "${!dests[@]}"; do
    mkdir -p "${dests[$i]}" && dests[i]=$(cd "${dests[$i]}" && pwd)
done
[ "$display_channels" -ge 0 ] || display_channels=$channels
logs="${XDG_DATA_HOME:-$HOME/.local/share}/listener/logs"
name_of() { printf 'soak%02d' "$1"; }

# ── The profile ───────────────────────────────────────────────────────────────
{
    echo 'schema_version = 3'
    echo 'name = "soak"'
    for ((i = 0; i < channels; i++)); do
        raw=true
        [ "$record" = display ] && raw=false
        display=false
        [ "$record" != raw ] && [ "$i" -lt "$display_channels" ] && display=true
        dest=${dests[$((i % ${#dests[@]}))]}
        cat <<EOF

[[channels]]
name = "$(name_of "$i")"
kind = "Udp"
[channels.interface]
type = "Udp"
bind_address = "127.0.0.1"
port = $((port + i))
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
EOF
    done
    if [ -n "$extra" ]; then
        echo
        cat "$extra"
    fi
} >"$out/profile.toml"

# ── Run ───────────────────────────────────────────────────────────────────────
"$bin/listener" --profile "$out/profile.toml" >"$out/listener.out" 2>"$out/listener.err" &
listener_pid=$!
sleep 3
"$bin/soak-gen" --to "127.0.0.1:$port" --streams "$channels" --rate "$rate" \
    --seconds "$seconds" --manifest "$out/gen.txt" >"$out/gen.out" 2>"$out/gen.err" &
gen_pid=$!

# Anonymous resident memory is what the process alone holds, so it shows a leak.
started=$(date +%s)
echo "seconds,rss_anon_bytes" >"$out/memory.csv"
while kill -0 "$gen_pid" 2>/dev/null; do
    anon=$(awk '/^RssAnon:/ { print $2 * 1024 }' "/proc/$listener_pid/status" 2>/dev/null)
    [ -n "$anon" ] || break
    echo "$(($(date +%s) - started)),$anon" >>"$out/memory.csv"
    sleep 10
done
wait "$gen_pid"
sleep 2

kill -TERM "$listener_pid"
for _ in $(seq 30); do
    kill -0 "$listener_pid" 2>/dev/null || break
    sleep 1
done
if kill -0 "$listener_pid" 2>/dev/null; then
    kill -KILL "$listener_pid"
    listener_code="did not stop within 30 s"
else
    wait "$listener_pid"
    listener_code=$?
fi

# ── Check ─────────────────────────────────────────────────────────────────────
failed=0
results=()
check() { # check <status> <description>
    if [ "$1" = 0 ]; then results+=("PASS  $2"); else results+=("FAIL  $2"); failed=1; fi
}

[ "$listener_code" = 0 ]
check $? "listener exit code 0"

peak=$(awk -F, 'NR > 1 && $2 > m { m = $2 } END { printf "%.1f", m / 1048576 }' "$out/memory.csv")
awk -v p="$peak" -v b="$budget_mib" 'BEGIN { exit !(p <= b) }'
check $? "peak anonymous memory $peak MiB within $budget_mib MiB"

growth=$(awk -F, 'NR > 1 && $1 >= 3600 { if (!s) { s = $1; a = $2 } e = $1; z = $2 }
    END { if (e > s) printf "%.2f", (z - a) / 1048576 / ((e - s) / 3600) }' "$out/memory.csv")
if [ -n "$growth" ]; then
    awk -v g="$growth" 'BEGIN { exit !(g < 1) }'
    check $? "memory growth after the first hour $growth MiB/h under 1"
fi

largest=$(find "${dests[@]}" -maxdepth 1 -type f \( -name '*.raw' -o -name '*.disp' \) \
    -printf '%s\n' | sort -n | tail -1)
[ "${largest:-0}" -le $((2 * 1024 * 1024 * 1024)) ]
check $? "largest file ${largest:-0} bytes within the 2 GiB cap"

if [ "$record" != display ]; then
    : >"$out/verify.txt"
    verified=0
    for d in "${!dests[@]}"; do
        pairs=()
        for ((i = d; i < channels; i += ${#dests[@]})); do pairs+=("$(name_of "$i")=$i"); done
        "$bin/soak-verify" --manifest "$out/gen.txt" --recordings "${dests[$d]}" \
            --logs "$logs" "${pairs[@]}" | tee -a "$out/verify.txt"
        [ "${PIPESTATUS[0]}" = 0 ] || verified=1
    done
    check $verified "every sequence number recorded (soak-verify)"
fi

if [ "$record" != raw ]; then
    with_display=0
    for ((i = 0; i < display_channels; i++)); do
        if find "${dests[@]}" -maxdepth 1 -name "$(name_of "$i")_*.disp" -size +0 | grep -q .; then
            with_display=$((with_display + 1))
        fi
    done
    [ "$with_display" = "$display_channels" ]
    check $? "a non-empty .disp file for each of $display_channels Display Channels"
fi

{
    echo "soak run: $channels channels x $rate/s for $seconds s, recording $record, to ${dests[*]}"
    echo "listener exit code: $listener_code"
    echo
    printf '%s\n' "${results[@]}"
} | tee "$out/summary.txt"
exit $failed
