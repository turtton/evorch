#!/usr/bin/env bash
# 1 run（モデル × 課題 × 回）を実行して結果を保存する。
#   prepare-clone → prompt → evorch run（タイムアウト付き）→ verify → 結果の保存
# 使い方: scripts/run-one.sh <model-key> <task-id> <rep> <outroot> [options]
#   --run-js true|false   run_js の有効化（既定 true）
#   --effort <e>          reasoning_effort（既定 $EVAL_EFFORT、なければ models.sh の既定）
#   --timeout <sec>       evorch run のタイムアウト（既定 $EVAL_TIMEOUT_S、なければ 1800）
#   --keep-clone          clone を消さずに残す
#   --force               結果がある組も実行し直す
# 環境変数:
#   EVORCH_BIN            evorch バイナリ（必須）
#   EVORCH_CONFIG_SRC     overlay の元になる config ディレクトリ（make-overlay.sh 参照）
#   EVAL_WORK             clone と overlay の置き場（既定 ${TMPDIR:-/tmp}/run-js-eval-work。run の後に消す）
#   EVAL_SOURCE_REPO      prepare-clone.sh の source-repo（既定は prepare-clone.sh の既定）
#   EVAL_VERIFY_TIMEOUT_S verify のタイムアウト（既定 900）
# 結果: <outroot>/<model>/<task>/<rep>/
#   result.json（最後に書く。あれば完了扱い）, run.db, evorch.stdout.log, evorch.stderr.log,
#   verify.log, final.diff, answers/, inspect-run.json, inspect-events.json, zz-worker.toml
# 終了コード: 0 = run を実行して保存した（verify の合否は問わない）/ 既に結果あり、
#             2 = 使い方や環境の誤り、3 = 準備に失敗（結果は書かない。再実行で作り直す）
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
evals=$(cd "$here/.." && pwd)
# shellcheck source=models.sh
. "$here/models.sh"

usage() { sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 2; }
[ $# -ge 4 ] || usage
model=$1 task=$2 rep=$3 outroot=$4
shift 4
run_js=true
effort=${EVAL_EFFORT:-$EVAL_DEFAULT_EFFORT}
timeout_s=${EVAL_TIMEOUT_S:-1800}
verify_timeout_s=${EVAL_VERIFY_TIMEOUT_S:-900}
keep_clone=0
force=0
while [ $# -gt 0 ]; do
  case $1 in
    --run-js) run_js=${2:-}; shift 2 || usage ;;
    --effort) effort=${2:-}; shift 2 || usage ;;
    --timeout) timeout_s=${2:-}; shift 2 || usage ;;
    --keep-clone) keep_clone=1; shift ;;
    --force) force=1; shift ;;
    *) usage ;;
  esac
done
case $run_js in true | false) ;; *) echo "--run-js は true か false" >&2; exit 2 ;; esac
[[ $timeout_s =~ ^[0-9]+$ && $verify_timeout_s =~ ^[0-9]+$ ]] || { echo "タイムアウトは秒の整数" >&2; exit 2; }
[[ $rep =~ ^[A-Za-z0-9_.-]+$ ]] || { echo "rep が不正: $rep" >&2; exit 2; }
route=$(eval_route "$model") || { echo "未知のモデルキー: $model（$EVAL_MODELS）" >&2; exit 2; }
[ -f "$evals/tasks/$task.md" ] || { echo "未知の課題: $task" >&2; exit 2; }
[ -n "${EVORCH_BIN:-}" ] && [ -x "$EVORCH_BIN" ] || { echo "EVORCH_BIN に実行可能な evorch を指定する" >&2; exit 2; }
if command -v timeout >/dev/null 2>&1; then TIMEOUT=timeout
elif command -v gtimeout >/dev/null 2>&1; then TIMEOUT=gtimeout
else echo "timeout (coreutils) がない" >&2; exit 2; fi

mdir=$(eval_model_dir "$model")
mkdir -p "$outroot"
outroot=$(cd "$outroot" && pwd)
rundir=$outroot/$mdir/$task/$rep
if [ -f "$rundir/result.json" ] && [ "$force" -eq 0 ]; then
  echo "skip $mdir $task $rep（結果あり）"
  exit 0
fi
rm -rf "$rundir"
mkdir -p "$rundir"

work_root=${EVAL_WORK:-${TMPDIR:-/tmp}/run-js-eval-work}
mkdir -p "$work_root"
work=$(mktemp -d "$work_root/$mdir-$task-$rep.XXXXXX")
clone=$work/clone
overlay=$work/overlay
child_pid=
# shellcheck disable=SC2329
cleanup() {
  if [ -n "$child_pid" ]; then kill "$child_pid" 2>/dev/null; wait "$child_pid" 2>/dev/null; fi
  if [ "$keep_clone" -eq 0 ]; then rm -rf "$work"; else echo "clone を残した: $clone" >&2; fi
}
trap cleanup EXIT
trap 'echo "中断: $mdir $task $rep" >&2; rm -rf "$rundir"; exit 130' INT TERM

now_ms() { date +%s%3N; }
json_str() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/\\t/g' | tr -d '\r' | awk 'BEGIN{ORS=""} NR>1{print "\\n"} {print}'; }

prep_fail() { echo "準備に失敗: $1 ($mdir $task $rep)" >&2; rm -rf "$rundir"; exit 3; }

"$here/make-overlay.sh" "$route" "$effort" "$overlay" >/dev/null || prep_fail make-overlay
cp "$overlay/config.d/zz-worker.toml" "$rundir/zz-worker.toml"
src_arg=()
[ -n "${EVAL_SOURCE_REPO:-}" ] && src_arg=("${EVAL_SOURCE_REPO}")
"$here/prepare-clone.sh" "$task" "$clone" "${src_arg[@]}" >"$rundir/prepare.log" 2>&1 || prep_fail prepare-clone
prompt=$("$here/prompt.sh" "$task") || prep_fail prompt
[ -n "$prompt" ] || prep_fail "prompt が空"

db=$rundir/run.db
started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
t0=$(now_ms)
"$TIMEOUT" --kill-after=30 "$timeout_s" "$EVORCH_BIN" run \
  --project "$clone" --role worker --prompt "$prompt" \
  --user-config "$overlay" --run-js-enabled "$run_js" --db "$db" \
  >"$rundir/evorch.stdout.log" 2>"$rundir/evorch.stderr.log" </dev/null &
child_pid=$!
wait "$child_pid"
rc=$?
child_pid=
t1=$(now_ms)
status=ok
timed_out=false
if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then status=timeout; timed_out=true
elif [ "$rc" -ne 0 ]; then status=error; fi

run_id=$(sed -n 's/^run_id: *//p' "$rundir/evorch.stderr.log" | head -n 1)

# 検証（同じ clone に対して）。タイムアウトで中断された run も検証する。
v0=$(now_ms)
"$TIMEOUT" --kill-after=30 "$verify_timeout_s" "$evals/verify/$task.sh" "$clone" >"$rundir/verify.log" 2>&1 </dev/null
verify_rc=$?
v1=$(now_ms)
case $verify_rc in 0) verify=pass ;; 1) verify=fail ;; 124) verify=timeout ;; *) verify=error ;; esac

# 成果物の保存。
git -C "$clone" diff 4391a7e5 >"$rundir/final.diff" 2>/dev/null || true
[ -d "$clone/answers" ] && cp -R "$clone/answers" "$rundir/answers" 2>/dev/null

# inspect の出力を保存する（DB があれば。metrics.mjs は無ければ DB を直接読む）。
if [ -n "$run_id" ] && [ -f "$db" ]; then
  "$EVORCH_BIN" inspect --db "$db" run "$run_id" >"$rundir/inspect-run.json" 2>"$rundir/inspect.err" \
    || rm -f "$rundir/inspect-run.json"
  "$EVORCH_BIN" inspect --db "$db" events --run "$run_id" --limit 10000 >"$rundir/inspect-events.json" 2>>"$rundir/inspect.err" \
    || rm -f "$rundir/inspect-events.json"
  [ -s "$rundir/inspect.err" ] || rm -f "$rundir/inspect.err"
fi

tmp=$rundir/.result.json.tmp
{
  printf '{\n'
  printf '  "model": "%s",\n' "$(json_str "$mdir")"
  printf '  "route": "%s",\n' "$(json_str "$route")"
  printf '  "effort": "%s",\n' "$(json_str "$effort")"
  printf '  "task": "%s",\n' "$(json_str "$task")"
  printf '  "rep": "%s",\n' "$(json_str "$rep")"
  printf '  "run_js_enabled": %s,\n' "$run_js"
  if [ -n "$run_id" ]; then printf '  "run_id": "%s",\n' "$(json_str "$run_id")"; else printf '  "run_id": null,\n'; fi
  printf '  "db": "run.db",\n'
  printf '  "status": "%s",\n' "$status"
  printf '  "evorch_exit": %s,\n' "$rc"
  printf '  "timed_out": %s,\n' "$timed_out"
  printf '  "timeout_s": %s,\n' "$timeout_s"
  printf '  "duration_ms": %s,\n' "$((t1 - t0))"
  printf '  "verify": "%s",\n' "$verify"
  printf '  "verify_exit": %s,\n' "$verify_rc"
  printf '  "verify_duration_ms": %s,\n' "$((v1 - v0))"
  printf '  "started_at": "%s"\n' "$started_at"
  printf '}\n'
} >"$tmp"
mv "$tmp" "$rundir/result.json"

printf '%s %s %s status=%s verify=%s dur=%ss run_id=%s\n' \
  "$mdir" "$task" "$rep" "$status" "$verify" "$(((t1 - t0) / 1000))" "${run_id:-none}"
exit 0
