#!/usr/bin/env bash
# run-one.sh / run-matrix.sh の流れ確認用のダミー evorch（実モデルを呼ばない）。
#   run:     引数を $DUMMY_LOG に記録し、--db の場所に空ファイルを作って stderr に run_id を出す。
#            DUMMY_SOLUTION=<script> なら "<script> <project>" で課題を解いたことにする。
#            DUMMY_SLEEP=<sec> で眠る（タイムアウト確認）、DUMMY_EXIT=<n> でその終了コードにする。
#            DUMMY_UNAVAILABLE_MODEL=<model-id> なら、--user-config の zz-worker.toml にそのモデルがあるとき
#            実際の evorch と同じ形式のエラー（model is not advertised by provider）で終了コード 1 にする。
#            DUMMY_ERROR_REASON=<text> なら、stderr に `agent run failed reason=<text>` を出す（provider エラーの再現）。
#            DUMMY_GUARD=requests|cost|repeat なら、暴走の watchdog（リクエスト数・費用・同一応答の上限）を試す run を再現する（`<db>.guard` を作って眠り、inspect が上の値を返す）。
#            DUMMY_STALL=1 なら、入力待ちのまま終了しない run を再現する（`<db>.stall` を作って眠り、inspect events は TurnCompleted が最後の列を返す）。
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
    if [ -n "${DUMMY_GUARD:-}" ] && [ -n "$db" ]; then
      printf '%s\n' "$DUMMY_GUARD" >"$db.guard"
      sleep 600
    fi
    if [ -n "${DUMMY_STALL:-}" ] && [ -n "$db" ]; then
      : >"$db.stall"
      sleep 600
    fi
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
    if [ -f "$2.guard" ]; then
      # DUMMY_GUARD の run: requests = 200 リクエスト・費用小、cost = 20 リクエスト・$10、repeat = 同じ応答を繰り返す。
      node -e '
        const mode = require("fs").readFileSync(process.argv[1] + ".guard", "utf8").trim();
        if (process.argv[2] === "run") {
          const n = mode === "requests" ? 200 : 20;
          const each = mode === "cost" ? 0.5 : 0.001;
          const rows = Array.from({ length: n }, (_, i) => ({ purpose: "agent", request_id: `r${i}`, cost_usd: each }));
          console.log(JSON.stringify({ db: process.argv[1], result: { run_id: "run-fixture-1", usage_requests: rows } }));
        } else {
          const msg = (id, text) => ({ id, kind: "Message", payload: { kind: "Message", payload: { kind: "MessageCompleted", payload: { run_id: "run-fixture-1", text } } }, session_id: "headless", wall_clock_ns: 1e18 + id });
          const progress = { id: 99, kind: "Lifecycle", payload: { kind: "Lifecycle", payload: { kind: "RunProgress", payload: { activity: "Tools" } } }, session_id: "headless", wall_clock_ns: 1e18 + 99 };
          const events = mode === "repeat"
            ? [1, 2, 3, 4, 5, 6].map((i) => msg(i, "同じ通知への同じ返事です。")).concat([progress])
            : [msg(1, "作業中"), msg(2, "別の応答"), progress];
          console.log(JSON.stringify({ db: process.argv[1], result: { events } }));
        }
      ' "$2" "$cmd"
      exit 0
    fi
    if [ "$cmd" = events ] && [ -f "$2.stall" ]; then
      # DUMMY_STALL の run: 質問をして入力待ち（Waiting → TurnCompleted が最後のイベント）の末尾を返す。
      node -e '
        const ev = (id, kind, payload) => ({ id, kind: "Lifecycle", payload: { kind: "Lifecycle", payload: { kind, payload } }, session_id: "headless", wall_clock_ns: 1e18 + id });
        const msg = { id: 1, kind: "Message", payload: { kind: "Message", payload: { kind: "MessageCompleted", payload: { run_id: "run-fixture-1", text: "確認です。この方針で進めてよいですか？" } } }, session_id: "headless", wall_clock_ns: 1e18 };
        console.log(JSON.stringify({ db: process.argv[1], result: { events: [msg, ev(2, "RunProgress", { activity: "User" }), ev(3, "AgentRunStateChanged", { from: "Running", to: "Waiting" }), ev(4, "TurnCompleted", { context_len: 3 })] } }));
      ' "$2"
      exit 0
    fi
    node -e '
      const f = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).runs[0];
      console.log(JSON.stringify(process.argv[2] === "run" ? f.run : f.events, null, 2));
    ' "$here/fixture-metrics.json" "$cmd"
    ;;
  *) echo "usage: dummy-evorch run|inspect ..." >&2; exit 2 ;;
esac
