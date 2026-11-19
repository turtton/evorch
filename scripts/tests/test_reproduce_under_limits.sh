#!/usr/bin/env bash
# Regression tests for scripts/reproduce-under-limits.sh without generating any
# load and without systemd.
#
# The production script has no test hooks. The suite instead
#  - puts mock systemd-run/systemctl, and a recording no-op `nice`, first on PATH
#    (so no stressor can ever be started: every call is recorded and verified),
#  - runs a temporary COPY of the script in which only the two literal kernel
#    paths (/proc/self/cgroup, /sys/fs/cgroup) point into a fake cgroup tree
#    written by the mock systemd-run.
# The unmodified script is used for everything that must hold on a real system.
# Not covered here (needs the host's systemd, see the PR description): that the
# kernel really enforces the limits and that a real scope kills every descendant.
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
stubs=$work/stubs
stubs_ng=$work/stubs-ng
minbin=$work/minbin
mkdir -p "$state/active" "$stubs" "$stubs_ng" "$minbin"

cat >"$stubs/systemd-run" <<'EOF'
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
# A fake cgroup tree for the scope's first process. MOCK_CPU_MAX / MOCK_MEM_MAX
# replace what the "kernel" reports, to model a limit that was not applied.
cgdir=$MOCK_STATE/cgroup/mock/$unit.scope
mkdir -p "$cgdir"
echo "${MOCK_CPU_MAX:-$((cpu * 1000)) 100000}" >"$cgdir/cpu.max"
echo "${MOCK_MEM_MAX:-$n}" >"$cgdir/memory.max"
echo "0::/mock/$unit.scope" >"$MOCK_STATE/proc-self-cgroup"
echo $$ >"$MOCK_STATE/active/$unit"
exec "$@"
EOF
cat >"$stubs/systemctl" <<'EOF'
#!/usr/bin/env bash
# Mock of `systemctl --user is-active|stop|kill|show <unit>.scope`.
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
cat >"$stubs/nice" <<'EOF'
#!/usr/bin/env bash
# Records what would have been started as a stressor; starts nothing.
echo "$*" >>"$MOCK_STATE/nice-calls"
exit 0
EOF
cat >"$stubs_ng/stress-ng" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$stubs/systemd-run" "$stubs/systemctl" "$stubs/nice" "$stubs_ng/stress-ng"

# A PATH without any stress-ng, for the busy-loop branch.
for tool in bash env sed sleep mktemp rm nproc dirname cat cp mkdir touch printf sh true false; do
    ln -s "$(type -P "$tool")" "$minbin/$tool"
done

# The copy of the script that reads the fake cgroup tree of the mock.
mscript=$work/reproduce-under-limits.sh
sed -e "s#/proc/self/cgroup#$state/proc-self-cgroup#g" -e "s#/sys/fs/cgroup#$state/cgroup#g" "$script" >"$mscript"
if ! grep -q "$state/proc-self-cgroup" "$mscript" || ! grep -q "$state/cgroup" "$mscript"; then
    echo "the script no longer reads /proc/self/cgroup and /sys/fs/cgroup literally; update this test" >&2
    exit 1
fi

export MOCK_STATE=$state
export PATH=$stubs:$PATH

failures=0
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    failures=$((failures + 1))
}
reset() {
    rm -rf -- "$state"
    mkdir -p "$state/active"
    unset MOCK_FAIL_REAL MOCK_COLLIDE MOCK_DELAY MOCK_CPU_MAX MOCK_MEM_MAX MOCK_NO_TIME
}
# run ARGS...: runs the patched copy, leaves stdout in $out, stderr in $err, status in $rc.
run() {
    rc=0
    "$BASH" "$mscript" "$@" >"$work/out" 2>"$work/err" || rc=$?
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
nice_calls() {
    if [[ -e $state/nice-calls ]]; then wc -l <"$state/nice-calls"; else echo 0; fi
}

# 0. The production script has no test hooks that could fake the verification.
if grep -n 'REPRO_' "$script"; then fail "hooks: the script reads REPRO_* environment variables"; fi

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
TMPDIR=$scratch_root "$BASH" "$mscript" --timeout 60s -- sh -c 'echo $$ >"$1"; exec sleep 30' _ "$work/pid" >/dev/null 2>"$work/err" &
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
PATH=/nonexistent "$BASH" "$script" -- printf ran >"$work/out" 2>"$work/err" || rc=$?
err=$(<"$work/err")
expect_rc no-systemd-run 125
expect_contains no-systemd-run "$err" 'sandbox_access=unsandboxed'
if [[ -s $work/out ]]; then fail "no-systemd-run: the command ran"; fi

# 9. Neither the command nor any stressor starts unless the scope carries every
#    expected limit: CPU, memory and the time limit are each checked on their own,
#    with and without stress. A refusal is exit 125 with the approval hint.
refused() {
    local name=$1
    shift
    expect_rc "$name" 125
    if [[ -n $out ]]; then fail "$name: the command ran: $out"; fi
    if [[ $(nice_calls) -ne 0 ]]; then fail "$name: started stressors: $(<"$state/nice-calls")"; fi
    expect_contains "$name" "$err" 'sandbox_access=unsandboxed'
    if [[ $# -gt 0 ]]; then expect_contains "$name" "$err" "$1"; fi
}
for stress in 0 1; do
    reset
    export MOCK_CPU_MAX='max 100000' MOCK_MEM_MAX=max
    run --stress-cpus "$stress" -- printf ran
    refused "unlimited-scope stress=$stress" "cpu.max is 'max 100000'"
    reset
    export MOCK_MEM_MAX=max
    run --stress-cpus "$stress" -- printf ran
    refused "memory-unlimited stress=$stress" "memory.max is 'max'"
    reset
    export MOCK_MEM_MAX=1073741824
    run --stress-cpus "$stress" -- printf ran
    refused "memory-mismatch stress=$stress" "memory.max is '1073741824'"
    reset
    export MOCK_CPU_MAX='max 100000'
    run --stress-cpus "$stress" -- printf ran
    refused "cpu-unlimited stress=$stress" "cpu.max is 'max 100000'"
    reset
    export MOCK_CPU_MAX='20000 100000'
    run --stress-cpus "$stress" -- printf ran
    refused "cpu-mismatch stress=$stress" "cpu.max is '20000 100000'"
    reset
    export MOCK_NO_TIME=1
    run --stress-cpus "$stress" -- printf ran
    refused "no-time-limit stress=$stress" 'no time limit'
done
reset

# The stressors start in the background; the command waits until they are recorded.
await_calls='for ((i = 0; i < 100; i++)); do
    if [[ -e $1 ]]; then mapfile -t l <"$1"; if ((${#l[@]} >= $2)); then break; fi; fi
    sleep 0.1
done
printf ran'

# 9b. With every limit in place, stressors are requested only through the
#     recording `nice` (nothing is started): busy loops without stress-ng, one
#     stress-ng with it, none without --stress-cpus.
reset
PATH=$stubs:$minbin run --stress-cpus 3 -- bash -c "$await_calls" _ "$state/nice-calls" 3
expect_rc stress-busy-loops 0
if [[ $out != ran ]]; then fail "stress-busy-loops: the command did not run: $out"; fi
if [[ $(nice_calls) -ne 3 ]]; then fail "stress-busy-loops: expected 3 stressors, got $(nice_calls)"; fi
if [[ $(grep -c -- '^-n 19 timeout --signal=KILL [0-9]*s bash -c ' "$state/nice-calls") -ne 3 ]]; then
    fail "stress-busy-loops: unexpected stressor commands: $(<"$state/nice-calls")"
fi
reset
PATH=$stubs:$stubs_ng:$minbin run --stress-cpus 3 -- bash -c "$await_calls" _ "$state/nice-calls" 1
expect_rc stress-ng 0
if [[ $(nice_calls) -ne 1 ]] || ! grep -q -- '^-n 19 stress-ng --cpu 3 ' "$state/nice-calls"; then
    fail "stress-ng: unexpected stressor commands: $(cat "$state/nice-calls" 2>/dev/null)"
fi
reset
run -- printf ran
expect_rc no-stress 0
if [[ $(nice_calls) -ne 0 ]]; then fail "no-stress: started stressors"; fi

# 10. The unmodified in-scope launcher: refuses outside a scope with the limits,
#     with or without stress, cannot be fooled by environment variables, and
#     rejects malformed arguments, whoever calls it.
unit=evorch-repro-1-abcdefabcdef-1
launcher() { "$BASH" "$script" --internal-scope-run "$@" >"$work/out" 2>"$work/err" && rc=0 || rc=$?; err=$(<"$work/err"); }
reset
rm -f "$work/marker" "$work/stressed"
for stress in 0 2; do
    launcher "$unit" "$work/marker" "$stress" 10 '50000 100000' 268435456 "$stubs/systemctl" -- touch "$work/stressed"
    if [[ $rc -eq 0 ]]; then fail "launcher stress=$stress: ran outside a scope"; fi
    expect_contains "launcher stress=$stress" "$err" 'refusing to run'
    if [[ -e $work/marker || -e $work/stressed ]]; then fail "launcher stress=$stress: marker or command created outside a scope"; fi
done
# A fake cgroup file and tree handed in through the environment change nothing.
mkdir -p "$work/fakecg/x/$unit.scope"
echo "50000 100000" >"$work/fakecg/x/$unit.scope/cpu.max"
echo 268435456 >"$work/fakecg/x/$unit.scope/memory.max"
echo "0::/x/$unit.scope" >"$work/fakecg/proc-cgroup"
rc=0
REPRO_TEST_PROC_CGROUP=$work/fakecg/proc-cgroup REPRO_TEST_CGROUP_ROOT=$work/fakecg \
    "$BASH" "$script" --internal-scope-run "$unit" "$work/marker" 0 10 '50000 100000' 268435456 "$stubs/systemctl" -- touch "$work/stressed" \
    >"$work/out" 2>"$work/err" || rc=$?
if [[ $rc -eq 0 || -e $work/marker || -e $work/stressed ]]; then fail "launcher: environment variables faked the verification"; fi
launcher not-a-unit "$work/marker" 2 10 '50000 100000' 268435456 "$stubs/systemctl" -- touch "$work/stressed"
expect_rc launcher-unit 2
launcher "$unit" "$work/marker" 999 10 '50000 100000' 268435456 "$stubs/systemctl" -- touch "$work/stressed"
expect_rc launcher-count 2
launcher "$unit" "$work/marker" 2 10 'max 100000' 268435456 "$stubs/systemctl" -- touch "$work/stressed"
expect_rc launcher-cpu 2
launcher "$unit" "$work/marker" 2 10 '50000 100000' max "$stubs/systemctl" -- touch "$work/stressed"
expect_rc launcher-mem 2
launcher "$unit" relative-marker 0 10 '50000 100000' 268435456 "$stubs/systemctl" -- touch "$work/stressed"
expect_rc launcher-marker 2
launcher "$unit" "$work/marker" 2 10 '50000 100000' 268435456 "$stubs/systemctl"
expect_rc launcher-nocommand 2
if [[ -e $work/stressed ]]; then fail "launcher: ran the command with invalid arguments"; fi
if [[ $(nice_calls) -ne 0 ]]; then fail "launcher: started stressors"; fi

if ((failures > 0)); then
    printf '%d check(s) failed\n' "$failures" >&2
    exit 1
fi
echo "test_reproduce_under_limits: all checks passed"
