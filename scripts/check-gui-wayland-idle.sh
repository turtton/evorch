#!/usr/bin/env bash
# Native Wayland redraw-wait regression, isolated from the caller's desktop.
# Build first: cargo build -p gui --bin native_qa_window
# Usage: scripts/check-gui-wayland-idle.sh [OUTPUT_DIR]
# Dependencies: Weston (headless + desktop shell), jq, awk, coreutils, Linux /proc.
set -Eeuo pipefail

fail() { printf 'Wayland idle QA: %s\n' "$*" >&2; exit 1; }
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    sed -n '2,5p' "$0"
    exit 0
fi
[[ $# -le 1 ]] || fail 'Expected at most one output directory.'
[[ $(uname -s) == Linux ]] || fail 'This check requires Linux.'
for dependency in weston jq awk timeout mktemp realpath getconf; do
    command -v "$dependency" >/dev/null || fail "Missing dependency: $dependency"
done
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
gui_bin=$(realpath -- "${GUI_BIN:-$repo_root/target/debug/native_qa_window}")
[[ -x "$gui_bin" ]] || fail "GUI_BIN is not executable: $gui_bin"
out=$(realpath -m -- "${1:-${GUI_QA_OUTPUT_DIR:-$repo_root/target/gui-wayland-idle}}")
mkdir -p -- "$out"
out=$(mktemp -d "$out/run-XXXXXXXX")
qa_tmp=$(mktemp -d /tmp/evorch-gui-wayland-idle.XXXXXXXX)
app_monitor="" weston_monitor=""
weston_pid="" weston_start="" weston_stopped=false

# Strip through the LAST closing parenthesis: /proc comm may contain spaces and
# parentheses. Remaining fields start at stat field 3 (state), not field 1.
process_start() {
    awk '{ sub(/^[0-9]+ \(.*\) /, "");
        if (NF < 20 || $20 !~ /^[0-9]+$/) exit 1; print $20 }' "/proc/$1/stat"
}
stop_monitor() {
    local identity=$1 pid start current
    [[ -n "$identity" ]] || return 0
    pid=${identity%%:*} start=${identity#*:}
    current=$(process_start "$pid" 2>/dev/null) || return 0
    [[ "$current" == "$start" ]] || return 0
    # GNU timeout owns a fresh process group and forwards TERM to its child.
    kill -TERM "$pid" 2>/dev/null || true
    for ((attempt=0; attempt<20; attempt++)); do
        current=$(process_start "$pid" 2>/dev/null) || break
        [[ "$current" == "$start" ]] || break
        sleep 0.1
    done
    current=$(process_start "$pid" 2>/dev/null) || current=""
    if [[ "$current" == "$start" ]]; then
        kill -KILL -- "-$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
}
cleanup() {
    local result=$?
    trap - EXIT INT TERM
    # A stopped compositor cannot react to timeout's TERM until resumed.
    if [[ "$weston_stopped" == true && -n "$weston_pid" ]] &&
        [[ $(process_start "$weston_pid" 2>/dev/null) == "$weston_start" ]]; then
        kill -CONT "$weston_pid" 2>/dev/null || true
    fi
    stop_monitor "$app_monitor"
    stop_monitor "$weston_monitor"
    rm -rf -- "$qa_tmp"
    if (( result != 0 )); then
        printf 'FAILED (exit %s). Evidence and logs: %s\n' "$result" "$out" >&2
        tail -n 30 "$out/app.log" "$out/weston.log" >&2 2>/dev/null || true
    fi
    exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'printf "Failed at line %s: %s\n" "$LINENO" "$BASH_COMMAND" >&2' ERR
printf 'Wayland idle QA evidence: %s\n' "$out"

mkdir -p "$qa_tmp/config" "$qa_tmp/data" "$qa_tmp/cache" "$qa_tmp/runtime" "$qa_tmp/work"
chmod 700 "$qa_tmp/runtime"
# env -i excludes the real DISPLAY/WAYLAND_DISPLAY, session bus, credentials and
# user profiles. Both server and client use only this private runtime directory.
qa_env=(env -i "PATH=$PATH" "LANG=C.UTF-8" "LC_ALL=C.UTF-8"
    "XDG_CONFIG_HOME=$qa_tmp/config" "XDG_DATA_HOME=$qa_tmp/data"
    "XDG_CACHE_HOME=$qa_tmp/cache" "XDG_RUNTIME_DIR=$qa_tmp/runtime"
    "TMPDIR=$qa_tmp" "GIT_CONFIG_NOSYSTEM=1" "GIT_CONFIG_GLOBAL=/dev/null"
    "SHELL=/bin/sh" "LIBGL_ALWAYS_SOFTWARE=1" "GALLIUM_DRIVER=llvmpipe"
    "WGPU_BACKEND=vulkan" "WINIT_UNIX_BACKEND=wayland")
for variable in LD_LIBRARY_PATH LIBGL_DRIVERS_PATH VK_ICD_FILENAMES VK_DRIVER_FILES FONTCONFIG_FILE FONTCONFIG_PATH; do
    if [[ -n ${!variable:-} ]]; then qa_env+=("$variable=${!variable}"); fi
done
# Weston 13 (Ubuntu 24.04) and 16 both support these options. Keep the default
# desktop shell: unlike kiosk shells, it implements xdg_toplevel.set_minimized.
# A private headless pixman compositor neither opens a host display nor uses DRM.
timeout --kill-after=2s 5s "${qa_env[@]}" weston --help > "$out/weston-help.txt" 2>&1
weston_args=(--backend=headless --renderer=pixman --no-config --idle-time=0
    --socket=evorch-qa --width=1280 --height=720)
if awk '/--fake-seat/ { found=1 } END { exit !found }' "$out/weston-help.txt"; then
    weston_args+=(--fake-seat)
fi
(
    cd "$qa_tmp/work"
    exec timeout --kill-after=3s 75s "${qa_env[@]}" sh -c \
        'printf "%s\n" "$$" > "$1"; shift; exec "$@"' qa "$qa_tmp/weston.pid" \
        weston "${weston_args[@]}"
) > "$out/weston.log" 2>&1 &
weston_timeout_pid=$!
weston_monitor="$weston_timeout_pid:$(process_start "$weston_timeout_pid")"
for ((attempt=0; attempt<100; attempt++)); do
    [[ -S "$qa_tmp/runtime/evorch-qa" ]] && break
    kill -0 "$weston_timeout_pid" 2>/dev/null || fail 'Weston exited before creating its socket.'
    sleep 0.1
done
[[ -S "$qa_tmp/runtime/evorch-qa" ]] || fail 'Weston did not create its private socket within 10 seconds.'
read -r weston_pid < "$qa_tmp/weston.pid" || fail 'Missing private Weston PID.'
[[ "$weston_pid" =~ ^[0-9]+$ ]] || fail 'Invalid private Weston PID.'
weston_start=$(process_start "$weston_pid") || fail 'Weston exited before GUI startup.'
qa_env+=("WAYLAND_DISPLAY=evorch-qa")

cat > "$out/input-layout.json" <<'JSON'
{
  "version": 3,
  "panels": {
    "agent-main": {"id":"agent-main","kind":"agent","title":"Conversation"},
    "terminal-main": {"id":"terminal-main","kind":"terminal","title":"Terminal"},
    "tasks-main": {"id":"tasks-main","kind":"tasks","title":"Tasks"},
    "notifications-main": {"id":"notifications-main","kind":"notifications","title":"Notifications"},
    "sidebar-main": {"id":"sidebar-main","kind":"sidebar","title":"Projects"},
    "subagents-home": {"id":"subagents-home","kind":"subagent_region","title":"Subagents"},
    "diff-main": {"id":"diff-main","kind":"diff","title":"Diff"}
  },
  "main": {"root":{"type":"tabs","panels":["agent-main","terminal-main","tasks-main","notifications-main","sidebar-main","subagents-home","diff-main"],"active":0},"floating":[]},
  "extra_windows": []
}
JSON
(
    cd "$qa_tmp/work"
    # Record the actual GUI PID before exec, not the timeout wrapper's PID.
    exec timeout --kill-after=3s 60s "${qa_env[@]}" sh -c \
        'printf "%s\n" "$$" > "$1"; shift; exec "$@"' qa "$qa_tmp/app.pid" \
        "$gui_bin" --window-title evorch-wayland-idle-qa \
        --layout "$out/input-layout.json" --save-layout "$qa_tmp/saved-layout.json" \
        --minimize-after-ms 2000
) > "$out/app.log" 2>&1 &
app_timeout_pid=$!
app_monitor="$app_timeout_pid:$(process_start "$app_timeout_pid")"
for ((attempt=0; attempt<250; attempt++)); do
    kill -0 "$app_timeout_pid" 2>/dev/null || fail 'GUI exited before its first redraw.'
    if awk '$0 == "native_qa_window: redraw-ready" { ready=1 }
        END { exit !ready }' "$out/app.log"; then break; fi
    sleep 0.1
done
awk '
    $0 == "native_qa_window: first-frame-ready" { ready++ }
    $0 == "native_qa_window: redraw-ready" && ready == 1 { redraw++ }
    $0 == "native_qa_window: minimize-requested" { minimized++ }
    END { exit !(ready == 1 && redraw == 1 && minimized == 0) }
' "$out/app.log" || fail 'GUI did not complete its first frame before minimization within 25 seconds.'
read -r app_pid < "$qa_tmp/app.pid" || fail 'Missing GUI PID.'
[[ "$app_pid" =~ ^[0-9]+$ ]] || fail 'Invalid GUI PID.'
app_start=$(process_start "$app_pid") || fail 'GUI exited before measurement.'
clock_ticks=$(getconf CLK_TCK)
[[ "$clock_ticks" =~ ^[1-9][0-9]*$ ]] || fail 'Invalid system clock tick rate.'

snapshot() {
    local name=$1 stat_data io_data state user_ticks system_ticks start_ticks syscr uptime unused current
    stat_data=$(cat "/proc/$app_pid/task/$app_pid/stat") || fail 'Cannot read GUI main-thread stat.'
    io_data=$(cat "/proc/$app_pid/task/$app_pid/io") || fail 'Cannot read GUI main-thread I/O counters.'
    printf '%s\n' "$stat_data" > "$out/$name.stat"
    printf '%s\n' "$io_data" > "$out/$name.io"
    stat_data=$(awk '{ sub(/^[0-9]+ \(.*\) /, "");
        if (NF < 20 || $12 !~ /^[0-9]+$/ || $13 !~ /^[0-9]+$/ || $20 !~ /^[0-9]+$/) exit 1;
        print $1, $12, $13, $20 }' <<< "$stat_data") || fail 'Invalid GUI main-thread stat.'
    read -r state user_ticks system_ticks start_ticks <<< "$stat_data"
    [[ "$state" != Z && "$state" != X && "$start_ticks" == "$app_start" ]] \
        || fail 'GUI exited or its PID was reused.'
    syscr=$(awk '$1 == "syscr:" && $2 ~ /^[0-9]+$/ { count++; value=$2 }
        END { if (count != 1) exit 1; print value }' <<< "$io_data") \
        || fail 'Missing/invalid GUI main-thread read syscall counter.'
    current=$(process_start "$app_pid") || fail 'GUI exited during measurement.'
    [[ "$current" == "$app_start" ]] || fail 'GUI PID changed during measurement.'
    read -r uptime unused < /proc/uptime || fail 'Cannot read monotonic uptime.'
    jq -n --argjson pid "$app_pid" --argjson start "$start_ticks" --argjson uptime "$uptime" \
        --argjson syscr "$syscr" --argjson user "$user_ticks" --argjson system "$system_ticks" \
        '{pid: $pid, start_ticks: $start, uptime_seconds: $uptime, read_syscalls: $syscr,
          cpu_ticks: ($user + $system)}' > "$out/$name.json"
}

report_measurement() {
    local scenario=$1 minimum_seconds=$2
    jq -s --arg scenario "$scenario" --argjson ticks "$clock_ticks" --argjson minimum "$minimum_seconds" '
    .[0] as $before | .[1] as $after |
    ($after.uptime_seconds - $before.uptime_seconds) as $seconds |
    if $before.start_ticks != $after.start_ticks or $seconds < $minimum or
       $after.read_syscalls < $before.read_syscalls or $after.cpu_ticks < $before.cpu_ticks
    then error("invalid measurement interval or regressing counters") else
    {scenario: $scenario, pid: $after.pid, duration_seconds: $seconds,
     main_thread_read_syscalls_per_second: (($after.read_syscalls - $before.read_syscalls) / $seconds),
     main_thread_cpu_percent: (100 * ($after.cpu_ticks - $before.cpu_ticks) / $ticks / $seconds),
     max_read_syscalls_per_second: 5000} end
    ' "$out/$scenario-before.json" "$out/$scenario-after.json" > "$out/$scenario-measurement.json"
    cat "$out/$scenario-measurement.json"
    # The observed defect performs ~500,000 read syscalls/s. A wide syscall
    # budget catches that spin without imposing a noisy software-rendering CPU cap.
    jq -e '.main_thread_read_syscalls_per_second < .max_read_syscalls_per_second' \
        "$out/$scenario-measurement.json" >/dev/null || fail "GUI is busy polling during $scenario."
}

# Withhold frame callbacks deterministically, independently of each compositor's
# minimization policy. STOP only the exact compositor launched above; the GUI
# and its unchanged 200 ms Workbench repaint cadence keep running.
[[ $(process_start "$weston_pid") == "$weston_start" ]] || fail 'Private Weston PID changed.'
weston_stopped=true
kill -STOP "$weston_pid"
for ((attempt=0; attempt<20; attempt++)); do
    if awk '{ sub(/^[0-9]+ \(.*\) /, ""); exit !($1 == "T") }' "/proc/$weston_pid/stat"; then
        break
    fi
    sleep 0.1
done
awk '{ sub(/^[0-9]+ \(.*\) /, ""); exit !($1 == "T") }' "/proc/$weston_pid/stat" \
    || fail 'Private Weston did not stop.'
sleep 1
snapshot callback-withheld-before
sleep 5
snapshot callback-withheld-after
awk '$0 == "native_qa_window: minimize-requested" { minimized=1 }
    END { exit minimized }' "$out/app.log" \
    || fail 'GUI advanced to minimization while frame callbacks were withheld.'
[[ $(process_start "$weston_pid") == "$weston_start" ]] || fail 'Private Weston PID changed.'
kill -CONT "$weston_pid"
weston_stopped=false

# The timer starts at the first redraw. Reaching the 2-second minimization
# action after CONT demonstrates that GUI redraws resumed after the withheld
# callback, then also covers actual compositor minimization as a second case.
for ((attempt=0; attempt<150; attempt++)); do
    kill -0 "$app_timeout_pid" 2>/dev/null || fail 'GUI exited after compositor resume.'
    if awk '$0 == "native_qa_window: minimize-requested" { minimized=1 }
        END { exit !minimized }' "$out/app.log"; then break; fi
    sleep 0.1
done
awk '
    $0 == "native_qa_window: first-frame-ready" { ready++ }
    $0 == "native_qa_window: redraw-ready" && ready == 1 { redraw++ }
    $0 == "native_qa_window: minimize-requested" && ready == 1 { minimized++ }
    END { exit !(ready == 1 && redraw == 1 && minimized == 1) }
' "$out/app.log" || fail 'GUI did not resume and request minimization exactly once within 15 seconds.'
report_measurement callback-withheld 4
sleep 2
snapshot minimized-before
sleep 8
snapshot minimized-after
[[ $(process_start "$weston_pid") == "$weston_start" ]] || fail 'Weston exited during measurement.'
report_measurement minimized 7
printf 'PASS: Wayland callback wait and minimized main-thread syscall budgets. Evidence: %s\n' "$out"
