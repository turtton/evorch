#!/usr/bin/env bash
# run-one.sh / run-matrix.sh の流れ確認用のダミー evorch（実モデルを呼ばない）。
#   run:     引数を $DUMMY_LOG に記録し、--db の場所に空ファイルを作って stderr に run_id を出す。
#            DUMMY_SOLUTION=<script> なら "<script> <project>" で課題を解いたことにする。
#            DUMMY_SLEEP=<sec> で眠る（タイムアウト確認）、DUMMY_EXIT=<n> でその終了コードにする。
#            DUMMY_UNAVAILABLE_MODEL=<model-id> なら、--user-config の zz-worker.toml にそのモデルがあるとき
#            実際の evorch と同じ形式のエラー（model is not advertised by provider）で終了コード 1 にする。
#            DUMMY_ERROR_REASON=<text> なら、stderr に `agent run failed reason=<text>` を出す（provider エラーの再現）。
#   inspect: test/fixture-metrics.json の最初の run の出力を返す（run / events）。
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
case ${1:-} in
  run)
    shift
    project='' db='' config=''
    printf '%s\n' "$*" >>"${DUMMY_LOG:-/dev/null}"
    while [ $# -gt 0 ]; do
      case $1 in
        --project) project=$2 ;;
        --db) db=$2 ;;
        --user-config) config=$2 ;;
      esac
      shift 2
    done
    if [ -n "${DUMMY_UNAVAILABLE_MODEL:-}" ] && [ -n "$config" ] \
      && grep -qF "model = \"$DUMMY_UNAVAILABLE_MODEL\"" "$config/config.d/zz-worker.toml" 2>/dev/null; then
      printf 'run_id: run-fixture-unavailable\n' >&2
      printf '\033[31mERROR\033[0m agent run failed reason=モデル呼び出しに失敗しました: profile=codex-personal model=%s: model is not advertised by provider\nrun ended in phase Error\n' "$DUMMY_UNAVAILABLE_MODEL" >&2
      exit 1
    fi
    [ -z "$db" ] || : >"$db"
    echo "run_id: run-fixture-1" >&2
    [ -z "${DUMMY_SLEEP:-}" ] || sleep "$DUMMY_SLEEP"
    [ -z "${DUMMY_SOLUTION:-}" ] || "$DUMMY_SOLUTION" "$project"
    [ -z "${DUMMY_ERROR_REASON:-}" ] || printf '\033[31mERROR\033[0m agent run failed reason=%s\n' "$DUMMY_ERROR_REASON" >&2
    echo "dummy final text"
    exit "${DUMMY_EXIT:-0}"
    ;;
  inspect)
    shift
    [ "$1" = --db ] || { echo "inspect --db <path> が先頭" >&2; exit 2; }
    [ -f "$2" ] || { echo "database not found: $2" >&2; exit 1; }
    cmd=$3
    node -e '
      const f = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).runs[0];
      console.log(JSON.stringify(process.argv[2] === "run" ? f.run : f.events, null, 2));
    ' "$here/fixture-metrics.json" "$cmd"
    ;;
  *) echo "usage: dummy-evorch run|inspect ..." >&2; exit 2 ;;
esac
