#!/usr/bin/env bash
# run-one.sh / run-matrix.sh の流れ確認用のダミー evorch（実モデルを呼ばない）。
#   run:     引数を $DUMMY_LOG に記録し、--db の場所に空ファイルを作って stderr に run_id を出す。
#            DUMMY_SOLUTION=<script> なら "<script> <project>" で課題を解いたことにする。
#            DUMMY_SLEEP=<sec> で眠る（タイムアウト確認）、DUMMY_EXIT=<n> でその終了コードにする。
#   inspect: test/fixture-metrics.json の最初の run の出力を返す（run / events）。
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
case ${1:-} in
  run)
    shift
    project='' db=''
    printf '%s\n' "$*" >>"${DUMMY_LOG:-/dev/null}"
    while [ $# -gt 0 ]; do
      case $1 in
        --project) project=$2 ;;
        --db) db=$2 ;;
      esac
      shift 2
    done
    [ -z "$db" ] || : >"$db"
    echo "run_id: run-fixture-1" >&2
    [ -z "${DUMMY_SLEEP:-}" ] || sleep "$DUMMY_SLEEP"
    [ -z "${DUMMY_SOLUTION:-}" ] || "$DUMMY_SOLUTION" "$project"
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
