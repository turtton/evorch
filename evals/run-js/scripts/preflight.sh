#!/usr/bin/env bash
# モデルが provider で使えるかを、本番の run の前に確かめる。
#   Worker をそのモデルに固定した overlay で、極小のプロンプト（ツール不使用で "OK" と答えさせる）を 1 回だけ流す。
#   provider が advertise していないモデルは、モデル呼び出しの前に `model is not advertised by provider` などで
#   失敗するので、費用はかからない。使えるモデルでは極小のリクエストが 1 回出る（入力 数千トークン程度）。
# 使い方: scripts/preflight.sh <model-key> [--outroot <dir>] [--effort <e>] [--timeout <sec>] [--force]
#   --outroot <dir>   成功を <dir>/.preflight/<model>.ok に記録する。記録があれば再実行しない（--force で確かめ直す）
#   --effort <e>      reasoning_effort（既定 $EVAL_EFFORT、なければ models.sh の既定）
#   --timeout <sec>   evorch run のタイムアウト（既定 $EVAL_PREFLIGHT_TIMEOUT_S、なければ 180）
# 環境変数: EVORCH_BIN（必須）、EVORCH_CONFIG_SRC、EVAL_WORK（run-one.sh と同じ）
# 終了コード: 0 = 使える（または記録あり）、1 = 使えない（理由を標準エラーに 1 行）、2 = 使い方や環境の誤り
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=models.sh
. "$here/models.sh"

usage() { sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 2; }
[ $# -ge 1 ] || usage
model=$1
shift
outroot=''
effort=${EVAL_EFFORT:-$EVAL_DEFAULT_EFFORT}
timeout_s=${EVAL_PREFLIGHT_TIMEOUT_S:-180}
force=0
while [ $# -gt 0 ]; do
  case $1 in
    --outroot) outroot=${2:-}; shift 2 || usage ;;
    --effort) effort=${2:-}; shift 2 || usage ;;
    --timeout) timeout_s=${2:-}; shift 2 || usage ;;
    --force) force=1; shift ;;
    *) usage ;;
  esac
done
[[ $timeout_s =~ ^[0-9]+$ ]] || { echo "タイムアウトは秒の整数" >&2; exit 2; }
target=$(eval_target "$model") || { echo "未知のモデルキー: $model（$EVAL_MODELS）" >&2; exit 2; }
read -r profile model_id <<<"$target"
[ -n "${EVORCH_BIN:-}" ] && [ -x "$EVORCH_BIN" ] || { echo "EVORCH_BIN に実行可能な evorch を指定する" >&2; exit 2; }
if command -v timeout >/dev/null 2>&1; then TIMEOUT=timeout
elif command -v gtimeout >/dev/null 2>&1; then TIMEOUT=gtimeout
else echo "timeout (coreutils) がない" >&2; exit 2; fi

mdir=$(eval_model_dir "$model")
marker=''
if [ -n "$outroot" ]; then
  mkdir -p "$outroot/.preflight"
  marker=$(cd "$outroot" && pwd)/.preflight/$mdir.ok
  if [ -f "$marker" ] && [ "$force" -eq 0 ]; then
    echo "preflight ok（記録あり）: $mdir ($profile / $model_id)"
    exit 0
  fi
  rm -f "$marker"
fi

work_root=${EVAL_WORK:-${TMPDIR:-/tmp}/run-js-eval-work}
mkdir -p "$work_root"
work=$(mktemp -d "$work_root/preflight-$mdir.XXXXXX")
trap 'rm -rf "$work"' EXIT
"$here/make-overlay.sh" "$profile" "$model_id" "$effort" "$work/overlay" >/dev/null || { echo "overlay を作れない" >&2; exit 2; }
mkdir "$work/project"
git -C "$work/project" init -q

"$TIMEOUT" --kill-after=15 "$timeout_s" "$EVORCH_BIN" run \
  --project "$work/project" --role worker \
  --prompt 'Reply with the single word OK. Do not use any tools.' \
  --user-config "$work/overlay" --run-js-enabled false --db "$work/run.db" \
  >"$work/stdout.log" 2>"$work/stderr.log" </dev/null
rc=$?
if [ "$rc" -eq 0 ]; then
  [ -z "$marker" ] || printf '%s %s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$profile" "$model_id" >"$marker"
  echo "preflight ok: $mdir ($profile / $model_id)"
  exit 0
fi

# 失敗: ANSI を除いた stderr から理由を 1 行取り出す（reason= 以降、なければ最後の非空行）。
clean=$(sed -e 's/\x1b\[[0-9;]*m//g' "$work/stderr.log")
reason=$(printf '%s\n' "$clean" | grep 'reason=' | tail -n 1 | sed 's/.*reason=//')
[ -n "$reason" ] || reason=$(printf '%s\n' "$clean" | grep -v '^run_id:' | grep . | tail -n 1)
[ "$rc" -ne 124 ] || reason="タイムアウト（${timeout_s}s）"
echo "preflight 失敗: $mdir ($profile / $model_id): ${reason:-exit $rc}" >&2
exit 1
