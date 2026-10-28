#!/usr/bin/env bash
# モデル × 課題 × 回数 のマトリクスを run-one.sh で順次（または並列に）実行する。
# 結果（<outroot>/<model>/<task>/<rep>/result.json）がある組はスキップするので、中断しても同じ
# コマンドで続きから再開できる。
# 使い方: scripts/run-matrix.sh --outroot <dir> [options]
#   --outroot <dir>     結果の置き場（必須。リポジトリの外に置く）
#   --models "<k> ..."  既定は全 5 モデル（gpt-sol gpt-luna-fast gpt-astra claude-opus claude-sonnet）
#   --tasks "<id> ..."  既定は全 6 課題
#   --reps <n>          回数（既定 1。rep は 1..n）
#   --rep-from <m>      開始 rep 番号（既定 1。3 回目だけ追加したいときなど）
#   -j, --jobs <n>      並列度（既定 1）
#   --run-js true|false 既定 true
#   --effort <e>        reasoning_effort（run-one.sh 参照）
#   --timeout <sec>     1 run のタイムアウト（既定 1800）
#   --retry-errors      status が error の結果は消して再実行する
#   --list              実行せず、計画（run / skip）だけを表示する
#   --keep-clone        run-one.sh に渡す
# 順序は rep → 課題 → モデル。EVORCH_BIN などの環境変数は run-one.sh と同じ。
# 終了コード: 0 = 全組が完了、1 = 準備失敗などで完了しなかった組がある、2 = 使い方の誤り
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=models.sh
. "$here/models.sh"

usage() { sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 2; }
outroot='' models=$EVAL_MODELS tasks=$EVAL_TASKS reps=1 rep_from=1 jobs=1
retry=0 list=0
extra=()
while [ $# -gt 0 ]; do
  case $1 in
    --outroot) outroot=${2:-}; shift 2 || usage ;;
    --models) models=${2:-}; shift 2 || usage ;;
    --tasks) tasks=${2:-}; shift 2 || usage ;;
    --reps) reps=${2:-}; shift 2 || usage ;;
    --rep-from) rep_from=${2:-}; shift 2 || usage ;;
    -j | --jobs) jobs=${2:-}; shift 2 || usage ;;
    --run-js | --effort | --timeout) extra+=("$1" "${2:-}"); shift 2 || usage ;;
    --keep-clone) extra+=("$1"); shift ;;
    --retry-errors) retry=1; shift ;;
    --list) list=1; shift ;;
    -h | --help) usage ;;
    *) usage ;;
  esac
done
[ -n "$outroot" ] || usage
[[ $reps =~ ^[0-9]+$ && $rep_from =~ ^[0-9]+$ && $jobs =~ ^[0-9]+$ ]] && [ "$jobs" -ge 1 ] && [ "$rep_from" -ge 1 ] \
  || { echo "--reps / --rep-from / --jobs は正の整数" >&2; exit 2; }
for m in $models; do eval_target "$m" >/dev/null || { echo "未知のモデルキー: $m" >&2; exit 2; }; done
for t in $tasks; do [ -f "$here/../tasks/$t.md" ] || { echo "未知の課題: $t" >&2; exit 2; }; done
if [ "$list" -eq 0 ]; then
  [ -n "${EVORCH_BIN:-}" ] && [ -x "$EVORCH_BIN" ] || { echo "EVORCH_BIN に実行可能な evorch を指定する" >&2; exit 2; }
fi
mkdir -p "$outroot"
outroot=$(cd "$outroot" && pwd)

# 計画を作る。
plan=$(mktemp)
trap 'rm -f "$plan"' EXIT
n_run=0 n_skip=0
for ((rep = rep_from; rep < rep_from + reps; rep++)); do
  for t in $tasks; do
    for m in $models; do
      md=$(eval_model_dir "$m")
      res=$outroot/$md/$t/$rep/result.json
      if [ -f "$res" ] && [ "$retry" -eq 1 ] && grep -q '"status": "error"' "$res"; then
        [ "$list" -eq 1 ] || rm -rf "${res%/result.json}"
      fi
      if [ -f "$res" ]; then
        n_skip=$((n_skip + 1))
        [ "$list" -eq 1 ] && echo "skip $md $t $rep"
      else
        n_run=$((n_run + 1))
        [ "$list" -eq 1 ] && echo "run  $md $t $rep"
        printf '%s %s %s\n' "$m" "$t" "$rep" >>"$plan"
      fi
    done
  done
done
echo "計画: 実行 $n_run / スキップ $n_skip（並列度 $jobs）" >&2
[ "$list" -eq 1 ] && exit 0
[ "$n_run" -gt 0 ] || exit 0

export EVORCH_BIN
EVAL_EXTRA_ARGS="${extra[*]:-}"
export EVAL_EXTRA_ARGS
# 各行 "<model> <task> <rep>" を run-one.sh へ（run-one.sh は 0 = 保存済み / 3 = 準備失敗を返す）。
# EVAL_EXTRA_ARGS は空白を含まない語の列（--run-js false など）で、意図して分割する。
# shellcheck disable=SC2016
xargs -P "$jobs" -L1 bash -c 'one=$1 out=$2 m=$3 t=$4 r=$5
  exec "$one" "$m" "$t" "$r" "$out" $EVAL_EXTRA_ARGS' _ "$here/run-one.sh" "$outroot" <"$plan"
rc=$?
if [ "$rc" -ne 0 ]; then
  echo "完了しなかった組がある（xargs exit=$rc）。同じコマンドで再開できる" >&2
  exit 1
fi
exit 0
