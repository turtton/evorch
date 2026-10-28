#!/usr/bin/env bash
# 1 run（モデル × 課題 × 回）を実行して結果を保存する。
#   prepare-clone → prompt → evorch run（タイムアウト付き）→ verify → 結果の保存
# 使い方: scripts/run-one.sh <model-key> <task-id> <rep> <outroot> [options]
#   --run-js true|false   run_js の有効化（既定 true）
#   --effort <e>          reasoning_effort（既定 $EVAL_EFFORT、なければ models.sh の既定）
#   --timeout <sec>       evorch run のタイムアウト（既定 $EVAL_TIMEOUT_S、なければ 1800）
#   --idle-timeout <sec>  watchdog: run のターンが終わった（TurnCompleted が最後のイベント）のに evorch が終了せず、
#                         この秒数だけ新しいイベントが出なければ打ち切る（既定 $EVAL_IDLE_S、なければ 60。0 で無効）。
#                         モデルが確認の質問をして入力待ちになった run（status=stalled）を、30 分の timeout まで待たない
#   --max-requests <n>    watchdog: run のリクエスト数（usage_requests の agent 行）が n を超えたら打ち切る
#                         （既定 $EVAL_MAX_REQUESTS、なければ 150。0 で無効）。status=aborted（abort_kind=requests）
#   --max-cost <usd>      watchdog: run の費用（usage_requests の cost_usd の合計）が usd を超えたら打ち切る
#                         （既定 $EVAL_MAX_COST_USD、なければ 3。0 で無効）。status=aborted（abort_kind=cost）
#   --repeat-limit <n>    watchdog: 直近 n 件の（空でない）アシスタント応答が同一内容なら打ち切る
#                         （既定 $EVAL_REPEAT_LIMIT、なければ 4。0 で無効）。status=aborted（abort_kind=repeat）。
#                         完了後に同じ通知へ同じ返事を続ける暴走（第 3 回に 487 リクエスト・$12.6）を止める
#   --keep-clone          clone を消さずに残す
#   --force               結果がある組も実行し直す（既存の結果は <rep>.attempt<N> へ退避する）
# 環境変数:
#   EVORCH_BIN            evorch バイナリ（必須）
#   EVORCH_CONFIG_SRC     overlay の元になる config ディレクトリ（make-overlay.sh 参照）
#   EVAL_WORK             clone と overlay の置き場（既定 ${TMPDIR:-/tmp}/run-js-eval-work。run の後に消す）
#   EVAL_SOURCE_REPO      prepare-clone.sh の source-repo（既定は prepare-clone.sh の既定）
#   EVAL_VERIFY_TIMEOUT_S verify のタイムアウト（既定 900）
#   EVAL_IDLE_S / EVAL_WATCHDOG_POLL_S  watchdog の打ち切り秒数（既定 60）/ 確認の間隔（既定 5）
#   EVAL_MAX_REQUESTS / EVAL_MAX_COST_USD / EVAL_REPEAT_LIMIT / EVAL_USAGE_POLL_S  暴走の打ち切り条件（既定 150 / 3 / 4）と、リクエスト数・費用を読む間隔（既定 10 秒）
# 結果: <outroot>/<model>/<task>/<rep>/
#   result.json（最後に書く。あれば完了扱い。status が ok でなければ error_reason も）, run.db, evorch.stdout.log, evorch.stderr.log,
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
idle_s=${EVAL_IDLE_S:-60}
poll_s=${EVAL_WATCHDOG_POLL_S:-5}
max_requests=${EVAL_MAX_REQUESTS:-150}
max_cost=${EVAL_MAX_COST_USD:-3}
repeat_limit=${EVAL_REPEAT_LIMIT:-4}
usage_poll_s=${EVAL_USAGE_POLL_S:-10}
verify_timeout_s=${EVAL_VERIFY_TIMEOUT_S:-900}
keep_clone=0
force=0
while [ $# -gt 0 ]; do
  case $1 in
    --run-js) run_js=${2:-}; shift 2 || usage ;;
    --effort) effort=${2:-}; shift 2 || usage ;;
    --timeout) timeout_s=${2:-}; shift 2 || usage ;;
    --idle-timeout) idle_s=${2:-}; shift 2 || usage ;;
    --max-requests) max_requests=${2:-}; shift 2 || usage ;;
    --max-cost) max_cost=${2:-}; shift 2 || usage ;;
    --repeat-limit) repeat_limit=${2:-}; shift 2 || usage ;;
    --keep-clone) keep_clone=1; shift ;;
    --force) force=1; shift ;;
    *) usage ;;
  esac
done
case $run_js in true | false) ;; *) echo "--run-js は true か false" >&2; exit 2 ;; esac
[[ $timeout_s =~ ^[0-9]+$ && $verify_timeout_s =~ ^[0-9]+$ && $idle_s =~ ^[0-9]+$ && $poll_s =~ ^[0-9]+$ && $poll_s -ge 1 && $usage_poll_s =~ ^[0-9]+$ ]] || { echo "タイムアウトは秒の整数" >&2; exit 2; }
[[ $max_requests =~ ^[0-9]+$ && $repeat_limit =~ ^[0-9]+$ ]] || { echo "--max-requests / --repeat-limit は整数" >&2; exit 2; }
[[ $max_cost =~ ^[0-9]+(\.[0-9]+)?$ ]] || { echo "--max-cost は USD の数値" >&2; exit 2; }
[[ $rep =~ ^[A-Za-z0-9_.-]+$ ]] || { echo "rep が不正: $rep" >&2; exit 2; }
[[ $rep =~ \.attempt[0-9]+$ ]] && { echo "rep が不正（.attempt<N> は退避先の名前）: $rep" >&2; exit 2; }
target=$(eval_target "$model") || { echo "未知のモデルキー: $model（$EVAL_MODELS）" >&2; exit 2; }
read -r profile model_id <<<"$target"
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
if [ -f "$rundir/result.json" ]; then
  archived=$(eval_archive_attempt "$rundir") || { echo "結果の退避に失敗: $rundir" >&2; exit 3; }
  echo "既存の結果を退避した: $archived" >&2
else
  rm -rf "$rundir"
fi
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

"$here/make-overlay.sh" "$profile" "$model_id" "$effort" "$overlay" >/dev/null || prep_fail make-overlay
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

# watchdog: 評価の Worker は対話しないので、次のどれかで evorch を打ち切る（TERM、10 秒後に KILL）。run の DB（--db）を
# `evorch inspect` で読んで判定する（読めない間は何もしない）。
#   stalled : evorch は、モデルが確認の質問などでターンを終えると、入力待ち（AgentRunStateChanged Running→Waiting、続いて
#             TurnCompleted）のまま終了しない。最後のイベントが TurnCompleted のまま idle_s 秒動かなければ打ち切る。
#   aborted : 暴走の防御。requests = リクエスト数が max_requests を超えた、cost = 費用が max_cost を超えた、
#             repeat = 直近 repeat_limit 件の同一のアシスタント応答（完了後の通知に同じ返事を 487 リクエスト続けた run がある）。
stalled=false
stall_info=
aborted=false
abort_kind=
abort_info=
g_requests=0 g_cost=0
kill_child() {
  kill "$child_pid" 2>/dev/null
  for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$child_pid" 2>/dev/null || break; sleep 1; done
  kill -9 "$child_pid" 2>/dev/null
}
if [ "$idle_s" -gt 0 ] || [ "$max_requests" -gt 0 ] || [ "$repeat_limit" -gt 0 ] || awk -v c="$max_cost" 'BEGIN { exit !(c > 0) }'; then
  last_id= seen_at=0 stall_run_id= usage_at=0
  while kill -0 "$child_pid" 2>/dev/null; do
    sleep "$poll_s"
    kill -0 "$child_pid" 2>/dev/null || break
    [ -n "$stall_run_id" ] || stall_run_id=$(sed -n 's/^run_id: *//p' "$rundir/evorch.stderr.log" | head -n 1)
    [ -n "$stall_run_id" ] && [ -f "$db" ] || continue
    now=$(date +%s)
    # 暴走: リクエスト数と費用（usage_poll_s ごと。inspect run は大きな出力なので間引く）
    if [ $((now - usage_at)) -ge "$usage_poll_s" ]; then
      usage_at=$now
      if uline=$("$EVORCH_BIN" inspect --db "$db" run "$stall_run_id" 2>/dev/null | node "$here/run-usage.mjs" 2>/dev/null); then
        IFS=$'\t' read -r g_requests g_cost <<<"$uline"
        if [ "$max_requests" -gt 0 ] && [ "$g_requests" -gt "$max_requests" ]; then
          aborted=true abort_kind=requests abort_info="aborted: リクエスト数 $g_requests が上限 $max_requests を超えた（費用 \$$g_cost）"
        elif awk -v c="$g_cost" -v l="$max_cost" 'BEGIN { exit !(l > 0 && c > l) }'; then
          aborted=true abort_kind=cost abort_info="aborted: 費用 \$$g_cost が上限 \$$max_cost を超えた（リクエスト数 $g_requests）"
        fi
        if [ "$aborted" = true ]; then kill_child; break; fi
      fi
    fi
    line=$("$EVORCH_BIN" inspect --db "$db" events --run "$stall_run_id" --limit 150 2>/dev/null | node "$here/last-events.mjs" 2>/dev/null) || continue
    IFS=$'\t' read -r ev_id ev_kind ev_activity ev_state ev_text ev_repeat <<<"$line"
    if [ "$repeat_limit" -gt 0 ] && [ "${ev_repeat:-0}" -ge "$repeat_limit" ]; then
      aborted=true abort_kind=repeat abort_info="aborted: 直近 $ev_repeat 件のアシスタント応答が同一（リクエスト数 $g_requests、費用 \$$g_cost）。応答: $ev_text"
      kill_child
      break
    fi
    if [ "$ev_id" != "$last_id" ]; then last_id=$ev_id; seen_at=$now; fi
    if [ "$idle_s" -gt 0 ] && [ "$ev_kind" = TurnCompleted ] && [ $((now - seen_at)) -ge "$idle_s" ]; then
      stalled=true
      stall_info="stalled: TurnCompleted のあと ${idle_s}s 動かず（activity=$ev_activity state=$ev_state）。最後の発言: $ev_text"
      kill_child
      break
    fi
  done
fi
wait "$child_pid"
rc=$?
child_pid=
t1=$(now_ms)
status=ok
timed_out=false
if [ "$aborted" = true ]; then status=aborted
elif [ "$stalled" = true ]; then status=stalled
elif [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then status=timeout; timed_out=true
elif [ "$rc" -ne 0 ]; then status=error; fi

run_id=$(sed -n 's/^run_id: *//p' "$rundir/evorch.stderr.log" | head -n 1)

# status が ok でないときの理由（stderr の `reason=` 以降。なければ最後の非空行）。provider のエラーで止まった run が
# 成果物としては正しい（verify=pass）ことがあるので、結果に残して metrics.mjs が区別できるようにする。
error_reason=
if [ "$status" != ok ]; then
  clean=$(sed -e 's/\x1b\[[0-9;]*m//g' "$rundir/evorch.stderr.log")
  error_reason=$(printf '%s\n' "$clean" | grep 'reason=' | tail -n 1 | sed 's/.*reason=//')
  [ -n "$error_reason" ] || error_reason=$(printf '%s\n' "$clean" | grep -v '^run_id:' | grep . | tail -n 1)
  [ "$status" != timeout ] || error_reason="timeout after ${timeout_s}s${error_reason:+: $error_reason}"
  [ "$status" != stalled ] || error_reason=$stall_info
  [ "$status" != aborted ] || error_reason=$abort_info
  error_reason=${error_reason:0:300}
fi

# 検証（同じ clone に対して）。タイムアウトで中断された run も検証する。
v0=$(now_ms)
# RUSTC_WRAPPER（sccache）が clone の cwd で動かない環境がある（長い cwd で `sccache: error: path must be shorter
# than SUN_LEN`。予備実行の 2 回目では t3 / t4 / t6 の検証が cargo ごと全部落ちた）。Worker の shell は環境を
# 絞られるので wrapper を持たない。検証の cargo は、wrapper が動かないときだけ wrapper なしで流す。
verify_env=()
if [ -n "${RUSTC_WRAPPER:-}" ] && ! (cd "$clone" && "$RUSTC_WRAPPER" rustc -vV >/dev/null 2>&1); then
  verify_env=(env RUSTC_WRAPPER=)
  echo "note: RUSTC_WRAPPER=$RUSTC_WRAPPER が clone の cwd で動かないため、検証は RUSTC_WRAPPER なしで流す" >&2
fi
"$TIMEOUT" --kill-after=30 "$verify_timeout_s" "${verify_env[@]}" "$evals/verify/$task.sh" "$clone" >"$rundir/verify.log" 2>&1 </dev/null
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
  printf '  "profile": "%s",\n' "$(json_str "$profile")"
  printf '  "model_id": "%s",\n' "$(json_str "$model_id")"
  printf '  "effort": "%s",\n' "$(json_str "$effort")"
  printf '  "task": "%s",\n' "$(json_str "$task")"
  printf '  "rep": "%s",\n' "$(json_str "$rep")"
  printf '  "run_js_enabled": %s,\n' "$run_js"
  if [ -n "$run_id" ]; then printf '  "run_id": "%s",\n' "$(json_str "$run_id")"; else printf '  "run_id": null,\n'; fi
  printf '  "db": "run.db",\n'
  printf '  "status": "%s",\n' "$status"
  printf '  "evorch_exit": %s,\n' "$rc"
  if [ -n "$error_reason" ]; then printf '  "error_reason": "%s",\n' "$(json_str "$error_reason")"; fi
  printf '  "timed_out": %s,\n' "$timed_out"
  printf '  "timeout_s": %s,\n' "$timeout_s"
  printf '  "idle_timeout_s": %s,\n' "$idle_s"
  printf '  "max_requests": %s,\n  "max_cost_usd": %s,\n  "repeat_limit": %s,\n' "$max_requests" "$max_cost" "$repeat_limit"
  if [ "$aborted" = true ]; then printf '  "abort_kind": "%s",\n' "$abort_kind"; fi
  printf '  "duration_ms": %s,\n' "$((t1 - t0))"
  printf '  "verify": "%s",\n' "$verify"
  printf '  "verify_exit": %s,\n' "$verify_rc"
  printf '  "verify_duration_ms": %s,\n' "$((v1 - v0))"
  printf '  "started_at": "%s"\n' "$started_at"
  printf '}\n'
} >"$tmp"
mv "$tmp" "$rundir/result.json"

note=
[ "$status" != ok ] && [ "$verify" = pass ] && note=" (${status} だが verify=pass)"
printf '%s %s %s status=%s verify=%s dur=%ss run_id=%s%s\n' \
  "$mdir" "$task" "$rep" "$status" "$verify" "$(((t1 - t0) / 1000))" "${run_id:-none}" "$note"
exit 0
