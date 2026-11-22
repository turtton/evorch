#!/usr/bin/env bash
# D-3a2 B3-09 performance check driver (the reduced, order-of-magnitude scope).
# Build both arms first, then, on an idle fixed host:
#
#   scripts/perf-gate.sh conditions DIR  --runner EXE --a-exe X --a-sha S --a-tree T --a-recipe R \
#                                        --b-exe Y --b-sha S --b-tree T --b-recipe R [--a-note N --b-note N]
#   scripts/perf-gate.sh run        DIR  --runner EXE --a-exe X --b-exe Y
#   scripts/perf-gate.sh aggregate  DIR  --runner EXE
#   scripts/perf-gate.sh smoke      DIR  --runner EXE --a-exe X --b-exe Y     (evidence=false)
#
# No cgroup scope, no stress-ng and no load other than the fixture's own core
# state: the host must be otherwise idle, and its state is recorded in
# conditions.json (load average, top CPU users) and again in the observations
# file at the start of the round. The round is measured once and is never
# repeated: a failed or timed out round is recorded as failed and fails the
# check; measuring again is a change of conditions that goes back to review.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
round_timeout=960s

die() { echo "perf-gate: $*" >&2; exit 1; }

[[ $# -ge 2 ]] || die "usage: perf-gate.sh conditions|run|aggregate|smoke DIR [--key value ...]"
mode=$1 dir=$2
shift 2
declare -A opt=()
while [[ $# -gt 0 ]]; do
    [[ $1 == --* && $# -ge 2 ]] || die "bad argument: $1"
    opt[${1#--}]=$2
    shift 2
done
need() { [[ -n ${opt[$1]:-} ]] || die "--$1 is required"; }
runner=${opt[runner]:-}
need runner
[[ -x $runner ]] || die "runner is not executable: $runner"
mkdir -p "$dir"
dir=$(cd "$dir" && pwd)

case $mode in
conditions)
    for k in a-exe a-sha a-tree a-recipe b-exe b-sha b-tree b-recipe; do need "$k"; done
    args=()
    for k in "${!opt[@]}"; do
        [[ $k == runner ]] || args+=("--$k" "${opt[$k]}")
    done
    [[ ! -e $dir/conditions.json ]] || die "$dir/conditions.json exists: conditions are written once, before any sample"
    "$runner" conditions --out "$dir/conditions.json" "${args[@]}" \
        --rustc "$(rustc -V)" --cargo "$(cargo -V)" --profile release \
        --rustflags "${RUSTFLAGS:-}" --runner-sha "$(git -C "$repo" rev-parse HEAD)"
    echo "wrote $dir/conditions.json"
    ;;
run)
    need a-exe
    need b-exe
    [[ -f $dir/conditions.json ]] || die "run '$0 conditions' first: conditions.json must exist before the first sample"
    [[ -z $(find "$dir" -maxdepth 1 -name '*-round-*.json' -print -quit) ]] || die "$dir already holds round files: a round is not repeated"
    : >"$dir/run.log"
    for cell in latency pss; do
        out="$dir/$cell-round-1.json"
        echo "$(date -u +%FT%TZ) $cell: start, loadavg $(cut -d' ' -f1-3 /proc/loadavg)" >>"$dir/run.log"
        rc=0
        timeout -k 10 "$round_timeout" \
            "$runner" "$cell" --round 1 --conditions "$dir/conditions.json" \
            --a-exe "${opt[a-exe]}" --b-exe "${opt[b-exe]}" --out "$out" || rc=$?
        echo "$(date -u +%FT%TZ) $cell: exit $rc" >>"$dir/run.log"
        if [[ ! -f $out ]]; then
            status=failed
            [[ $rc -eq 124 ]] && status=timed_out
            "$runner" stub --kind "$cell" --round 1 --out "$out" --conditions "$dir/conditions.json" \
                --status "$status" --reason "no record written; exit $rc"
        fi
    done
    "$runner" aggregate --dir "$dir"
    ;;
aggregate)
    "$runner" aggregate --dir "$dir"
    ;;
smoke)
    need a-exe
    need b-exe
    "$runner" latency --smoke --round 1 --a-exe "${opt[a-exe]}" --b-exe "${opt[b-exe]}" --out "$dir/latency-round-1.json"
    "$runner" pss --smoke --round 1 --b-exe "${opt[b-exe]}" --out "$dir/pss-round-1.json"
    echo "smoke files in $dir are evidence=false: the aggregator rejects them"
    ;;
*)
    die "unknown mode $mode"
    ;;
esac
