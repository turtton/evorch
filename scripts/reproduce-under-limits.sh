#!/usr/bin/env bash
# Standard way to reproduce flaky failures and hangs under bounded resources.
#
# Runs a command (and every descendant) inside ONE transient systemd user scope
# with a CPU quota, a memory limit, a task limit and a mandatory wall-clock
# limit. If the limits cannot be applied the command is NOT run unlimited: the
# script exits non-zero instead. Optional CPU stress is generated only inside
# that same scope, never outside the limits.
#
# This needs the host's systemd user manager, so it does NOT work inside the
# evorch sandbox (no systemd-run, no /sys). Run it unsandboxed with approval.
set -euo pipefail

readonly EX_USAGE=2
readonly EX_NO_LIMITS=125
readonly KILL_GRACE=5 # seconds between SIGTERM and SIGKILL when stopping a scope
readonly DEFAULT_CPUS=1
readonly DEFAULT_MEMORY=2G
readonly DEFAULT_TIMEOUT=300
readonly MAX_TIMEOUT=3600
readonly DEFAULT_TASKS=4096
readonly MAX_STRESS_CPUS=64
readonly SCRIPT_PATH=${BASH_SOURCE[0]}

log() { printf '[reproduce-under-limits] %s\n' "$*" >&2; }
die() {
    local code=$1
    shift
    log "ERROR: $*"
    exit "$code"
}

usage() {
    cat <<'EOF'
Usage: scripts/reproduce-under-limits.sh [options] -- <command...>

Run <command> and all of its descendants inside one systemd user scope with
resource limits and a mandatory time limit, to reproduce flaky failures/hangs.

Options:
  --cpus N            CPU limit in cores, may be fractional (CPUQuota=N*100%).
                      Default: 1
  --memory SIZE       Memory limit (MemoryMax, swap disabled), e.g. 512M, 2G.
                      Default: 2G
  --timeout DURATION  Wall-clock limit per run: seconds, or with s/m/h suffix
                      (max 1h). Always applied. Default: 300s
  --repeat N          Run N times and report how many failed. Default: 1
  --stop-on-fail      With --repeat, stop at the first failing run.
  --stress-cpus K     Start K niced CPU stressors (stress-ng, else busy loops)
                      INSIDE the same limited scope while the command runs.
  --tasks N           Task (process/thread) limit, TasksMax. Default: 4096
  -h, --help          Show this help.

Exit status:
  single run          the command's exit status; 124 on timeout; 128+N when
                      interrupted by signal N
  --repeat N (N>1)    0 if every run passed, otherwise 1 (128+N if interrupted)
  2                   usage error
  125                 limits could not be applied; the command was NOT run.
                      Inside the evorch sandbox this always happens: ask for
                      approval to run this script unsandboxed.

On timeout or interruption the whole scope is stopped (SIGTERM, then SIGKILL
after 5s), so no descendant survives.
EOF
}

# ---------------------------------------------------------------- helpers

# Stop a scope and everything in it. Safe to call when it is already gone.
scope_active() { "$systemctl" --user is-active --quiet "$1.scope" 2>/dev/null; }

stop_scope() {
    local unit=$1 i
    [[ -n $unit ]] || return 0
    scope_active "$unit" || return 0
    "$systemctl" --user stop "$unit.scope" >/dev/null 2>&1 || true
    for ((i = 0; i < 50; i++)); do
        scope_active "$unit" || return 0
        sleep 0.1
    done
    "$systemctl" --user kill --signal=SIGKILL "$unit.scope" >/dev/null 2>&1 || true
    for ((i = 0; i < 50; i++)); do
        scope_active "$unit" || return 0
        sleep 0.1
    done
    log "WARNING: scope $unit.scope is still active after SIGKILL"
    return 1
}

now_ms() {
    local t=${EPOCHREALTIME/./}
    printf '%s' "$((t / 1000))"
}

signal_name() { kill -l "$1" 2>/dev/null || printf 'signal %s' "$1"; }

# Internal mode, executed INSIDE the scope: start CPU stressors, run the command,
# then stop the stressors. Refuses to start stressors anywhere but the expected
# scope, so load can never be generated outside the limits.
scope_launcher() {
    local unit=$1 stress=$2 stress_secs=$3 cg rc=0 i
    shift 3
    cg=$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null || true)
    [[ $cg == */"$unit.scope" ]] || die 1 "refusing to start stressors: not inside $unit.scope (cgroup: ${cg:-unknown})"
    local -a pids=()
    if command -v stress-ng >/dev/null 2>&1; then
        nice -n 19 stress-ng --cpu "$stress" --timeout "${stress_secs}s" --quiet &
        pids+=("$!")
    else
        for ((i = 0; i < stress; i++)); do
            nice -n 19 timeout --signal=KILL "${stress_secs}s" bash -c 'while :; do :; done' &
            pids+=("$!")
        done
    fi
    "$@" || rc=$?
    kill "${pids[@]}" 2>/dev/null || true
    wait "${pids[@]}" 2>/dev/null || true
    return "$rc"
}

if [[ ${1:-} == --internal-scope-launcher ]]; then
    shift
    scope_launcher "$@"
    exit $?
fi

# ------------------------------------------------------- argument parsing

cpus=$DEFAULT_CPUS
memory=$DEFAULT_MEMORY
timeout_arg="${DEFAULT_TIMEOUT}s"
repeat=1
stop_on_fail=0
stress=0
tasks=$DEFAULT_TASKS

need_value() { [[ $# -ge 2 && -n $2 ]] || die "$EX_USAGE" "option $1 needs a value"; }

while [[ $# -gt 0 ]]; do
    case $1 in
        --) shift; break ;;
        -h | --help) usage; exit 0 ;;
        --stop-on-fail) stop_on_fail=1; shift ;;
        --cpus | --memory | --timeout | --repeat | --stress-cpus | --tasks)
            need_value "$@"
            case $1 in
                --cpus) cpus=$2 ;;
                --memory) memory=$2 ;;
                --timeout) timeout_arg=$2 ;;
                --repeat) repeat=$2 ;;
                --stress-cpus) stress=$2 ;;
                --tasks) tasks=$2 ;;
            esac
            shift 2
            ;;
        --cpus=* | --memory=* | --timeout=* | --repeat=* | --stress-cpus=* | --tasks=*)
            set -- "${1%%=*}" "${1#*=}" "${@:2}"
            ;;
        *) usage >&2; die "$EX_USAGE" "unknown option or missing '--' before the command: $1" ;;
    esac
done
[[ $# -gt 0 ]] || { usage >&2; die "$EX_USAGE" "no command given (put it after '--')"; }

# Validation; everything is checked before anything is started.
[[ $cpus =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "$EX_USAGE" "--cpus must be a positive number: $cpus"
[[ $memory =~ ^[0-9]+[KkMmGgTt]?$ ]] || die "$EX_USAGE" "--memory must look like 512M or 2G: $memory"
memory=${memory^^}
if [[ ! $repeat =~ ^[0-9]+$ ]] || ((repeat < 1)); then die "$EX_USAGE" "--repeat must be an integer >= 1: $repeat"; fi
if [[ ! $stress =~ ^[0-9]+$ ]] || ((stress > MAX_STRESS_CPUS)); then die "$EX_USAGE" "--stress-cpus must be an integer in 0..$MAX_STRESS_CPUS: $stress"; fi
if [[ ! $tasks =~ ^[0-9]+$ ]] || ((tasks < 1)); then die "$EX_USAGE" "--tasks must be an integer >= 1: $tasks"; fi
[[ $timeout_arg =~ ^([0-9]+)([smh]?)$ ]] || die "$EX_USAGE" "--timeout must look like 180, 180s, 3m or 1h: $timeout_arg"
timeout_secs=${BASH_REMATCH[1]}
case ${BASH_REMATCH[2]} in
    m) timeout_secs=$((timeout_secs * 60)) ;;
    h) timeout_secs=$((timeout_secs * 3600)) ;;
esac
((timeout_secs >= 1 && timeout_secs <= MAX_TIMEOUT)) || die "$EX_USAGE" "--timeout must be between 1s and ${MAX_TIMEOUT}s: $timeout_arg"

((BASH_VERSINFO[0] >= 5)) || die "$EX_USAGE" "bash >= 5 is required"

cpu_int=${cpus%%.*}
cpu_frac=""
if [[ $cpus == *.* ]]; then cpu_frac=${cpus#*.}; fi
cpu_frac="${cpu_frac}00"
cpu_percent=$((10#$cpu_int * 100 + 10#${cpu_frac:0:2}))
((cpu_percent >= 1)) || die "$EX_USAGE" "--cpus is too small (CPUQuota would be 0%): $cpus"

mem_unit=${memory: -1}
mem_num=${memory%"$mem_unit"}
if [[ $mem_unit =~ [0-9] ]]; then
    mem_bytes=$memory
else
    case $mem_unit in
        K) mem_bytes=$((mem_num * 1024)) ;;
        M) mem_bytes=$((mem_num * 1024 * 1024)) ;;
        G) mem_bytes=$((mem_num * 1024 * 1024 * 1024)) ;;
        T) mem_bytes=$((mem_num * 1024 * 1024 * 1024 * 1024)) ;;
    esac
fi
((mem_bytes >= 4 * 1024 * 1024)) || die "$EX_USAGE" "--memory must be at least 4M: $memory"

command -v -- "$1" >/dev/null 2>&1 || die "$EX_USAGE" "command not found: $1"

# ------------------------------------------------- locate systemd and probe

sandbox_hint() {
    log "The command was NOT run (this script never runs without limits)."
    log "Inside the evorch sandbox systemd-run is unavailable. Ask for approval to run this script with sandbox_access=unsandboxed"
    log "(sandbox 外での実行の承認を求めてください), with a justification such as 'reproduce a flaky test under CPU/memory limits'."
}

systemd_run=${REPRO_SYSTEMD_RUN:-}
if [[ -z $systemd_run ]]; then
    systemd_run=$(command -v systemd-run 2>/dev/null || true)
fi
if [[ -z $systemd_run || ! -x $systemd_run ]]; then
    log "ERROR: systemd-run was not found; cannot apply resource limits."
    sandbox_hint
    exit "$EX_NO_LIMITS"
fi
systemctl=${REPRO_SYSTEMCTL:-}
if [[ -z $systemctl ]]; then
    systemctl=$(command -v systemctl 2>/dev/null || true)
fi
if [[ -z $systemctl || ! -x $systemctl ]]; then
    systemctl="$(dirname -- "$systemd_run")/systemctl"
fi
if [[ ! -x $systemctl ]]; then
    log "ERROR: systemctl was not found; cannot manage the scope."
    sandbox_hint
    exit "$EX_NO_LIMITS"
fi

host_cpus=$(nproc 2>/dev/null || echo 0)
if ((host_cpus > 0)) && ((cpu_percent > host_cpus * 100)); then
    log "WARNING: --cpus $cpus exceeds the $host_cpus CPUs of this host; the quota will not limit anything."
fi

run_id="evorch-repro-$$"
limit_props=(
    "-p" "CPUQuota=${cpu_percent}%"
    "-p" "MemoryMax=$memory"
    "-p" "MemorySwapMax=0"
    "-p" "TasksMax=$tasks"
    "-p" "TimeoutStopSec=${KILL_GRACE}s"
    # Backstop: systemd itself stops the scope even if this script is SIGKILLed.
    "-p" "RuntimeMaxSec=$((timeout_secs + 30))"
)

# Probe: create a scope with exactly the same properties and read back what the
# kernel enforces. If any limit is missing, nothing is run.
# shellcheck disable=SC2016 # expanded by the probe's own shell, inside the scope
probe_script='cg=$(sed -n "s/^0:://p" /proc/self/cgroup); d=/sys/fs/cgroup$cg; printf "%s\n%s\n" "$(cat "$d/cpu.max")" "$(cat "$d/memory.max")"'
probe_out=""
if ! probe_out=$("$systemd_run" --user --scope --quiet --collect --unit="$run_id-probe" \
    --description="evorch reproduce-under-limits probe" "${limit_props[@]}" -- sh -c "$probe_script" 2>&1); then
    log "ERROR: could not create a limited systemd user scope:"
    printf '%s\n' "$probe_out" | sed 's/^/    /' >&2
    sandbox_hint
    exit "$EX_NO_LIMITS"
fi
probe_cpu=$(printf '%s\n' "$probe_out" | sed -n '1p')
probe_mem=$(printf '%s\n' "$probe_out" | sed -n '2p')
expected_cpu="$((cpu_percent * 1000)) 100000"
if [[ $probe_cpu != "$expected_cpu" || $probe_mem != "$mem_bytes" ]]; then
    log "ERROR: limits were not enforced by the kernel (cpu.max='$probe_cpu' expected '$expected_cpu'; memory.max='$probe_mem' expected '$mem_bytes')."
    log "The cpu/memory cgroup controllers are probably not delegated to the systemd user manager."
    sandbox_hint
    exit "$EX_NO_LIMITS"
fi

# ----------------------------------------------------------- run and report

stress_desc=none
if ((stress > 0)); then
    if command -v stress-ng >/dev/null 2>&1; then
        stress_desc="$stress CPU stressor(s) via stress-ng (nice 19), inside the scope"
    else
        stress_desc="$stress niced busy loop(s), inside the scope"
    fi
fi
log "limits: cpus=$cpus (CPUQuota=${cpu_percent}%, cpu.max='$probe_cpu'), memory=$memory (memory.max=$probe_mem, swap=0), tasks=$tasks,"
log "        timeout=${timeout_secs}s (SIGTERM, SIGKILL after ${KILL_GRACE}s), repeat=$repeat, stress=$stress_desc"
log "command: $*"

current_unit=""
interrupted=0
# shellcheck disable=SC2329 # invoked through trap
on_signal() {
    interrupted=$1
    stop_scope "$current_unit" || true
}
# shellcheck disable=SC2329 # invoked through trap
on_exit() { stop_scope "$current_unit" || true; }
trap 'on_signal 2' INT
trap 'on_signal 15' TERM
trap 'on_signal 1' HUP
trap on_exit EXIT

passed=0
failed=0
timed_out_runs=0
failed_runs=()
completed=0

run_once() {
    local index=$1 unit rc=0 timed_out=0 start deadline_ms pid reason leftovers=0 i elapsed
    local -a inner
    shift
    unit="$run_id-$index"
    current_unit=$unit
    if ((stress > 0)); then
        inner=("$BASH" "$SCRIPT_PATH" --internal-scope-launcher "$unit" "$stress" "$((timeout_secs + 30))" "$@")
    else
        inner=("$@")
    fi
    start=$(now_ms)
    deadline_ms=$((start + timeout_secs * 1000))
    "$systemd_run" --user --scope --quiet --collect --unit="$unit" \
        --description="evorch reproduce-under-limits run $index/$repeat" "${limit_props[@]}" -- "${inner[@]}" <&0 &
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
        if ((interrupted != 0)); then break; fi
        if (($(now_ms) >= deadline_ms)); then
            timed_out=1
            stop_scope "$unit" || true
            break
        fi
        sleep 0.2
    done
    if ((interrupted != 0)); then
        stop_scope "$unit" || true
    fi
    if kill -0 "$pid" 2>/dev/null; then
        # Scope stop did not end the leader (should not happen); do not hang.
        kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" || rc=$?
    # The leader may have exited while descendants live on: they are still in the scope.
    for ((i = 0; i < 20; i++)); do
        scope_active "$unit" || break
        sleep 0.05
    done
    if scope_active "$unit"; then
        leftovers=1
        stop_scope "$unit" || true
    fi
    current_unit=""
    elapsed=$((($(now_ms) - start) / 1000))

    if ((timed_out)); then
        rc=124
        reason="TIMED OUT after ${timeout_secs}s; scope stopped, all descendants killed"
    elif ((interrupted != 0)); then
        rc=$((128 + interrupted))
        reason="INTERRUPTED by SIG$(signal_name "$interrupted"); scope stopped, all descendants killed"
    elif ((rc == 0)); then
        reason="exited 0"
    elif ((rc > 128)); then
        reason="killed by SIG$(signal_name $((rc - 128))) (exit $rc)"
        if ((rc == 137)); then reason+="; possibly the memory limit (OOM kill)"; fi
    else
        reason="exited $rc"
    fi
    if ((leftovers)); then reason+="; leftover descendants were killed with the scope"; fi
    log "run $index/$repeat: $reason (${elapsed}s)"
    run_rc=$rc
    run_timed_out=$timed_out
}

run_rc=0
run_timed_out=0
for ((n = 1; n <= repeat; n++)); do
    run_once "$n" "$@"
    if ((interrupted != 0)); then break; fi
    completed=$n
    if ((run_rc == 0)); then
        passed=$((passed + 1))
    else
        failed=$((failed + 1))
        failed_runs+=("$n")
        if ((run_timed_out)); then timed_out_runs=$((timed_out_runs + 1)); fi
        if ((stop_on_fail)) && ((n < repeat)); then
            log "stopping early (--stop-on-fail)"
            break
        fi
    fi
done

if ((repeat > 1)); then
    summary="summary: $failed of $completed completed run(s) failed ($passed passed"
    if ((timed_out_runs > 0)); then summary+=", $timed_out_runs timed out"; fi
    summary+="; requested $repeat)"
    if ((failed > 0)); then summary+="; failing runs: ${failed_runs[*]}"; fi
    log "$summary"
fi

if ((interrupted != 0)); then exit $((128 + interrupted)); fi
if ((repeat > 1)); then
    if ((failed > 0)); then exit 1; fi
    exit 0
fi
exit "$run_rc"
