#!/usr/bin/env bash
# Regression tests for scripts/reproduce-under-limits.sh without generating any
# load and without systemd: systemd-run/systemctl are mocks that register a
# "scope" (the pid of the command) and implement is-active/stop/kill on it.
# What is NOT covered here (needs the host's systemd, see the PR description):
# that the kernel really enforces the limits and that a real scope kills
# every descendant.
# shellcheck disable=SC2016 # the commands under test are single-quoted on purpose
set -euo pipefail
cd "$(dirname "$0")/../.."
script=$PWD/scripts/reproduce-under-limits.sh

work=$(mktemp -d "${TMPDIR:-/tmp}/test-reproduce-under-limits.XXXXXXXX")
sleepers=()
cleanup() {
    local p
    for p in "${sleepers[@]}"; do kill "$p" 2>/dev/null || true; done
    rm -rf -- "$work"
}
trap cleanup EXIT

state=$work/state
mkdir -p "$state/active"

cat >"$work/systemd-run" <<'EOF'
#!/usr/bin/env bash
# Mock of `systemd-run --user --scope`: registers the unit with this pid, then execs.
unit="" props=()
ALL_ARGS="$*"
while [[ $# -gt 0 ]]; do
    case $1 in
        --unit=*) unit=${1#--unit=}; shift ;;
        -p) props+=("$2"); shift 2 ;;
        --) shift; break ;;
        *) shift ;;
    esac
done
echo "$unit" >>"$MOCK_STATE/units"
printf '%s ' "$unit" "${props[@]}" >>"$MOCK_STATE/props"
echo >>"$MOCK_STATE/props"
echo "$ALL_ARGS" >>"$MOCK_STATE/argv"
if [[ $unit != *-probe ]]; then
    if [[ ${MOCK_FAIL_REAL:-} == 1 ]]; then
        echo "Failed to start transient scope unit: mock failure" >&2
        exit 1
    fi
    if [[ ${MOCK_COLLIDE:-} == 1 ]]; then
        cp "$MOCK_STATE/other.pid" "$MOCK_STATE/active/$unit"
        if [[ -n ${MOCK_DELAY:-} ]]; then exec sleep "$MOCK_DELAY"; fi
        echo "Failed to start transient scope unit: Unit $unit.scope already exists." >&2
        exit 1
    fi
fi
cpu="" mem=""
for p in "${props[@]}"; do
    case $p in
        CPUQuota=*) cpu=${p#CPUQuota=}; cpu=${cpu%\%} ;;
        MemoryMax=*) mem=${p#MemoryMax=} ;;
    esac
done
n=${mem%[KMGT]}
case $mem in
    *K) n=$((n * 1024)) ;;
    *M) n=$((n * 1024 * 1024)) ;;
    *G) n=$((n * 1024 * 1024 * 1024)) ;;
    *T) n=$((n * 1024 * 1024 * 1024 * 1024)) ;;
esac
if [[ $unit == *-probe ]]; then
    printf '%s 100000\n%s\n' "$((cpu * 1000))" "$n"
    exit 0
fi
# A fake cgroup tree for the scope's first process; MOCK_UNLIMITED leaves the limits out.
cgdir=$MOCK_STATE/cgroup/mock/$unit.scope
mkdir -p "$cgdir"
if [[ ${MOCK_UNLIMITED:-} == 1 ]]; then
    echo "max 100000" >"$cgdir/cpu.max"
    echo max >"$cgdir/memory.max"
else
    echo "$((cpu * 1000)) 100000" >"$cgdir/cpu.max"
    echo "$n" >"$cgdir/memory.max"
fi
echo "0::/mock/$unit.scope" >"$MOCK_STATE/proc-cgroup-$unit"
export REPRO_TEST_PROC_CGROUP=$MOCK_STATE/proc-cgroup-$unit REPRO_TEST_CGROUP_ROOT=$MOCK_STATE/cgroup
echo $$ >"$MOCK_STATE/active/$unit"
exec "$@"
EOF
cat >"$work/systemctl" <<'EOF'
#!/usr/bin/env bash
# Mock of `systemctl --user is-active|stop|kill <unit>.scope`.
args=("$@")
verb=""
unit=""
for a in "${args[@]}"; do
    case $a in
        is-active | stop | kill | show) verb=$a ;;
        *.scope) unit=${a%.scope} ;;
    esac
done
file=$MOCK_STATE/active/$unit
alive() { [[ -f $file ]] && kill -0 "$(cat "$file")" 2>/dev/null; }
case $verb in
    show)
        if [[ ${MOCK_NO_TIME:-} == 1 ]]; then echo infinity; else echo 50s; fi
        exit 0
        ;;
    is-active) alive; exit $? ;;
    stop)
        echo "stop $unit" >>"$MOCK_STATE/stops"
        if alive; then kill -TERM "$(cat "$file")" 2>/dev/null || true; fi
        ;;
    kill)
        echo "kill $unit" >>"$MOCK_STATE/stops"
        if alive; then kill -KILL "$(cat "$file")" 2>/dev/null || true; fi
        ;;
esac
exit 0
EOF
chmod +x "$work/systemd-run" "$work/systemctl"

export MOCK_STATE=$state
export REPRO_SYSTEMD_RUN=$work/systemd-run
export REPRO_SYSTEMCTL=$work/systemctl

failures=0
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    failures=$((failures + 1))
}
reset() {
    rm -rf -- "$state"
    mkdir -p "$state/active"
    unset MOCK_FAIL_REAL MOCK_COLLIDE MOCK_DELAY MOCK_UNLIMITED MOCK_NO_TIME
}
# run NAME ARGS...: runs the script, leaves stdout in $out, stderr in $err, status in $rc.
run() {
    rc=0
    "$BASH" "$script" "$@" >"$work/out" 2>"$work/err" || rc=$?
    out=$(<"$work/out")
    err=$(<"$work/err")
}
expect_rc() {
    local name=$1 want=$2
    if [[ $rc -ne $want ]]; then fail "$name: exit $rc, expected $want; stderr: $err"; fi
}
expect_contains() {
    local name=$1 haystack=$2 needle=$3
    if [[ $haystack != *"$needle"* ]]; then fail "$name: missing '$needle' in: $haystack"; fi
}

# 1. Arguments reach the command untouched, and systemd-run is told not to expand them.
reset
run --timeout 20s -- bash -c 'printf "<%s>\n" "$@"' _ 'a b' '$HOME' '${USER}' '%h' '*' ''
expect_rc passthrough 0
want=$'<a b>\n<$HOME>\n<${USER}>\n<%h>\n<*>\n<>'
if [[ $out != "$want" ]]; then fail "passthrough: arguments changed: $out"; fi
if [[ $(grep -c -- '--expand-environment=no' "$state/argv") -ne $(wc -l <"$state/argv") ]]; then
    fail "passthrough: a systemd-run call lacks --expand-environment=no: $(<"$state/argv")"
fi
expect_contains passthrough-limits "$(<"$state/props")" 'CPUQuota=100%'
expect_contains passthrough-limits "$(<"$state/props")" 'MemoryMax=2G'

# 2. --repeat aggregation.
reset
echo 0 >"$work/counter"
run --repeat 5 --timeout 20s -- bash -c 'n=$(<"$1"); n=$((n + 1)); echo "$n" >"$1"; [[ $n != 2 && $n != 4 ]]' _ "$work/counter"
expect_rc repeat 1
expect_contains repeat "$err" '2 of 5 completed run(s) failed (3 passed'
expect_contains repeat "$err" 'failing runs: 2 4'
reset
run --repeat 3 -- true
expect_rc repeat-pass 0
expect_contains repeat-pass "$err" '0 of 3 completed run(s) failed (3 passed'
reset
run --repeat 5 --stop-on-fail -- false
expect_rc stop-on-fail 1
expect_contains stop-on-fail "$err" '1 of 1 completed run(s) failed'

# 3. Numbers with a leading zero are decimal: --repeat 08 runs eight times.
reset
: >"$work/ran"
run --repeat 08 --cpus 01.50 --memory 0512m --timeout 08s --tasks 0100 -- bash -c 'echo x >>"$1"' _ "$work/ran"
expect_rc leading-zero 0
if [[ $(wc -l <"$work/ran") -ne 8 ]]; then fail "leading-zero: ran $(wc -l <"$work/ran") times, expected 8"; fi
expect_contains leading-zero "$(<"$state/props")" 'CPUQuota=150%'
expect_contains leading-zero "$(<"$state/props")" 'MemoryMax=512M'
expect_contains leading-zero "$(<"$state/props")" 'TasksMax=100'
for bad in '--repeat 0' '--repeat x' '--repeat 1000000' '--cpus 0' '--cpus 0.001' '--memory 1M' '--timeout 0' '--timeout 2h' '--stress-cpus 65' '--tasks 0'; do
    reset
    # shellcheck disable=SC2086 # the option and its value are two words
    run $bad -- true
    expect_rc "reject $bad" 2
    if [[ -e $state/units ]]; then fail "reject $bad: started a scope"; fi
done

# 4. A scope that cannot be started is reported with the approval hint, command not run.
reset
export MOCK_FAIL_REAL=1
rm -f "$work/ran"
run -- touch "$work/ran"
expect_rc start-failure 125
expect_contains start-failure "$err" 'sandbox_access=unsandboxed'
expect_contains start-failure "$err" 'was not run'
if [[ -e $work/ran ]]; then fail "start-failure: the command ran"; fi
reset
export MOCK_FAIL_REAL=1
run --repeat 3 -- true
expect_rc start-failure-repeat 125
if [[ $(grep -c -v -- '-probe$' "$state/units") -ne 1 ]]; then fail "start-failure-repeat: kept going after the failure"; fi

# 5. A name collision never stops somebody else's scope.
reset
sleep 60 &
other=$!
sleepers+=("$other")
echo "$other" >"$state/other.pid"
export MOCK_COLLIDE=1
run -- true
expect_rc collision 125
if ! kill -0 "$other" 2>/dev/null; then fail "collision: the other scope's process was killed"; fi
if [[ -e $state/stops ]]; then fail "collision: stopped a scope: $(<"$state/stops")"; fi
kill "$other" 2>/dev/null || true
unset MOCK_COLLIDE

# 5b. Same, when the time limit hits while systemd-run has not started the scope yet:
#     only the not-yet-started systemd-run is killed, nobody's scope is stopped.
reset
sleep 60 &
other=$!
sleepers+=("$other")
echo "$other" >"$state/other.pid"
export MOCK_COLLIDE=1 MOCK_DELAY=30
run --timeout 1s -- true
expect_rc collision-timeout 124
if ! kill -0 "$other" 2>/dev/null; then fail "collision-timeout: the other scope's process was killed"; fi
if [[ -e $state/stops ]]; then fail "collision-timeout: stopped a scope: $(<"$state/stops")"; fi
kill "$other" 2>/dev/null || true
unset MOCK_COLLIDE MOCK_DELAY

# 6. Timeout stops the scope and reports 124.
reset
start=$SECONDS
run --timeout 1s -- sh -c 'echo $$ >"$1"; exec sleep 30' _ "$work/pid"
expect_rc timeout 124
expect_contains timeout "$err" 'TIMED OUT'
if kill -0 "$(<"$work/pid")" 2>/dev/null; then fail "timeout: the command survived"; fi
if ((SECONDS - start > 15)); then fail "timeout: took $((SECONDS - start))s"; fi
expect_contains timeout "$(<"$state/stops")" stop

# 7. SIGTERM stops the scope, exit 143, no scratch directory left behind.
reset
rm -f "$work/pid"
scratch_root=$work/tmp-int
mkdir -p "$scratch_root"
TMPDIR=$scratch_root "$BASH" "$script" --timeout 60s -- sh -c 'echo $$ >"$1"; exec sleep 30' _ "$work/pid" >/dev/null 2>"$work/err" &
runner=$!
for _ in $(seq 100); do
    [[ -s $work/pid ]] && break
    sleep 0.1
done
[[ -s $work/pid ]] || fail "interrupt: the command did not start"
kill -TERM "$runner"
rc=0
wait "$runner" || rc=$?
expect_rc interrupt 143
if kill -0 "$(<"$work/pid")" 2>/dev/null; then fail "interrupt: the command survived"; fi
if [[ -n $(ls -A "$scratch_root") ]]; then fail "interrupt: left files behind: $(ls -A "$scratch_root")"; fi

# 8. Without systemd-run nothing runs and the exit status is 125 with the hint.
rc=0
env -u REPRO_SYSTEMD_RUN -u REPRO_SYSTEMCTL PATH=/nonexistent "$BASH" "$script" -- printf ran >"$work/out" 2>"$work/err" || rc=$?
err=$(<"$work/err")
expect_rc no-systemd-run 125
expect_contains no-systemd-run "$err" 'sandbox_access=unsandboxed'
if [[ -s $work/out ]]; then fail "no-systemd-run: the command ran"; fi

# 9. The in-scope launcher never runs the command, with or without stress, outside
#    a scope that carries the expected limits, and rejects malformed arguments,
#    whoever calls it.
reset
export MOCK_UNLIMITED=1
run -- printf ran
expect_rc unlimited-scope 125
expect_contains unlimited-scope "$err" "cpu.max is 'max 100000'"
expect_contains unlimited-scope "$err" 'sandbox_access=unsandboxed'
if [[ -n $out ]]; then fail "unlimited-scope: the command ran: $out"; fi
reset
export MOCK_NO_TIME=1
run -- printf ran
expect_rc no-time-limit 125
expect_contains no-time-limit "$err" 'no time limit'
if [[ -n $out ]]; then fail "no-time-limit: the command ran: $out"; fi
reset
run --stress-cpus 1 -- printf ran
expect_rc stress-limited 0
if [[ $out != ran ]]; then fail "stress-limited: expected the command to run inside a verified scope, got: $out"; fi
reset
export MOCK_UNLIMITED=1
run --stress-cpus 1 -- printf ran
expect_rc stress-unlimited 125
if [[ -n $out ]]; then fail "stress-unlimited: the command ran: $out"; fi
unset MOCK_UNLIMITED
unit=evorch-repro-1-abcdefabcdef-1
launcher() { "$BASH" "$script" --internal-scope-run "$@" >"$work/out" 2>"$work/err" && rc=0 || rc=$?; err=$(<"$work/err"); }
rm -f "$work/marker" "$work/stressed"
launcher "$unit" "$work/marker" 2 10 '50000 100000' 268435456 "$work/systemctl" -- touch "$work/stressed"
if [[ $rc -eq 0 ]]; then fail "launcher: started stressors outside a scope"; fi
expect_contains launcher "$err" 'refusing to run'
if [[ -e $work/marker || -e $work/stressed ]]; then fail "launcher: marker or command created outside a scope"; fi
launcher "$unit" "$work/marker" 0 10 '50000 100000' 268435456 "$work/systemctl" -- touch "$work/stressed"
if [[ $rc -eq 0 ]]; then fail "launcher: ran the command without stress outside a scope"; fi
expect_contains launcher-nostress "$err" 'refusing to run'
if [[ -e $work/marker || -e $work/stressed ]]; then fail "launcher: marker or command created outside a scope (no stress)"; fi
launcher not-a-unit "$work/marker" 2 10 '50000 100000' 268435456 "$work/systemctl" -- touch "$work/stressed"
expect_rc launcher-unit 2
launcher "$unit" "$work/marker" 999 10 '50000 100000' 268435456 "$work/systemctl" -- touch "$work/stressed"
expect_rc launcher-count 2
launcher "$unit" "$work/marker" 2 10 'max 100000' 268435456 "$work/systemctl" -- touch "$work/stressed"
expect_rc launcher-cpu 2
launcher "$unit" "$work/marker" 2 10 '50000 100000' max "$work/systemctl" -- touch "$work/stressed"
expect_rc launcher-mem 2
launcher "$unit" relative-marker 0 10 '50000 100000' 268435456 "$work/systemctl" -- touch "$work/stressed"
expect_rc launcher-marker 2
launcher "$unit" "$work/marker" 2 10 '50000 100000' 268435456 "$work/systemctl"
expect_rc launcher-nocommand 2
if [[ -e $work/stressed ]]; then fail "launcher: ran the command with invalid arguments"; fi

if ((failures > 0)); then
    printf '%d check(s) failed\n' "$failures" >&2
    exit 1
fi
echo "test_reproduce_under_limits: all checks passed"
