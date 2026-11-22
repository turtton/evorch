#!/usr/bin/env bash
# D-3a2 B3-09 performance gate driver. Build both arms first (outside any
# measurement scope), then:
#
#   scripts/perf-gate.sh conditions DIR  --runner EXE --a-exe X --a-sha S --a-tree T --a-recipe R \
#                                        --b-exe Y --b-sha S --b-tree T --b-recipe R [--a-note N --b-note N]
#   scripts/perf-gate.sh run        DIR  --runner EXE --a-exe X --b-exe Y
#   scripts/perf-gate.sh aggregate  DIR  --runner EXE
#   scripts/perf-gate.sh dry-run    DIR  --runner EXE --a-exe X --b-exe X [--c-exe Z]  (evidence=false, noise/time)
#   scripts/perf-gate.sh smoke      DIR  --runner EXE --a-exe X --b-exe Y     (evidence=false, no scope)
#
# `run` needs the host's systemd user scope (scripts/reproduce-under-limits.sh),
# so it cannot run inside the evorch sandbox: it is started unsandboxed with
# approval. stress-ng must be on PATH (for example `nix shell nixpkgs#stress-ng
# --command scripts/perf-gate.sh run ...`): there is no busy-loop fallback.
# A round is never repeated: a failed or timed out round is recorded as failed
# and fails the gate; measuring again is a change of conditions that goes back
# to review.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
limits="$repo/scripts/reproduce-under-limits.sh"
rounds=3

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

stress_ng_record() {
    command -v stress-ng >/dev/null 2>&1 || die "stress-ng not found on PATH; there is no fallback (use nix shell nixpkgs#stress-ng)"
    local path
    path=$(readlink -f "$(command -v stress-ng)")
    printf '%s | %s' "$path" "$(stress-ng --version 2>&1 | head -n 1)"
}

case $mode in
conditions)
    for k in a-exe a-sha a-tree a-recipe b-exe b-sha b-tree b-recipe; do need "$k"; done
    args=()
    for k in "${!opt[@]}"; do
        [[ $k == runner ]] || args+=("--$k" "${opt[$k]}")
    done
    [[ ! -e $dir/conditions.json ]] || die "$dir/conditions.json exists: conditions are written once, before any sample"
    "$runner" conditions --out "$dir/conditions.json" "${args[@]}" \
        --stress-ng "$(stress_ng_record)" \
        --rustc "$(rustc -V)" --cargo "$(cargo -V)" --profile release \
        --rustflags "${RUSTFLAGS:-}" --runner-sha "$(git -C "$repo" rev-parse HEAD)"
    echo "wrote $dir/conditions.json"
    ;;
run)
    need a-exe
    need b-exe
    [[ -f $dir/conditions.json ]] || die "run '$0 conditions' first: conditions.json must exist before the first sample"
    [[ -z $(find "$dir" -maxdepth 1 -name '*-round-*.json' -print -quit) ]] || die "$dir already holds round files: a round is not repeated"
    stress_ng_record >"$dir/stress-ng.txt"
    : >"$dir/run.log"
    for ((round = 1; round <= rounds; round++)); do
        for cell in latency pss; do
            out="$dir/$cell-round-$round.json"
            echo "$(date -u +%FT%TZ) round $round $cell: start" >>"$dir/run.log"
            rc=0
            "$limits" --cpus 2 --memory 2G --tasks 512 --timeout 600s --stress-cpus 1 -- \
                "$runner" "$cell" --round "$round" --conditions "$dir/conditions.json" \
                --a-exe "${opt[a-exe]}" --b-exe "${opt[b-exe]}" ${opt[c-exe]:+--c-exe "${opt[c-exe]}"} \
                --out "$out" || rc=$?
            echo "$(date -u +%FT%TZ) round $round $cell: exit $rc" >>"$dir/run.log"
            if [[ ! -f $out ]]; then
                status=failed
                [[ $rc -eq 124 ]] && status=timed_out
                "$runner" stub --kind "$cell" --round "$round" --out "$out" --conditions "$dir/conditions.json" \
                    --status "$status" --reason "no record written; scope exit $rc"
            fi
        done
    done
    "$runner" aggregate --dir "$dir"
    ;;
aggregate)
    "$runner" aggregate --dir "$dir"
    ;;
dry-run)
    # Noise floor and round duration only: full counts under the real scope and stress, but
    # evidence=false, so the aggregator rejects the file. Pass the same A as --a-exe and
    # --b-exe for an A-versus-A run. No force kill, no PSS, no thresholds are consulted.
    need a-exe
    need b-exe
    [[ ! -e $dir/dry-run-latency.json ]] || die "$dir/dry-run-latency.json exists"
    stress_ng_record >"$dir/stress-ng.txt"
    start=$(date +%s)
    rc=0
    "$limits" --cpus 2 --memory 2G --tasks 512 --timeout 600s --stress-cpus 1 -- \
        "$runner" latency --dry-run --round 1 --a-exe "${opt[a-exe]}" --b-exe "${opt[b-exe]}" \
        ${opt[c-exe]:+--c-exe "${opt[c-exe]}"} --out "$dir/dry-run-latency.json" || rc=$?
    echo "dry-run: exit $rc after $(( $(date +%s) - start )) s (scope timeout 600 s)"
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
