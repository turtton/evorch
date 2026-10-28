#!/usr/bin/env bash
# make-overlay / run-one / run-matrix / metrics の流れ確認（ダミー evorch。実モデルは呼ばない）。
# 使い方: test/scripts-test.sh <scratch-dir>   （scratch-dir は存在しない場所。リポジトリの外）
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
ev=$(cd "$here/.." && pwd)
scratch=${1:?usage: $0 <scratch-dir>}
[ ! -e "$scratch" ] || { echo "$scratch は既にある" >&2; exit 2; }
mkdir -p "$scratch"
fail=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }

# 偽の利用者設定
mkdir -p "$scratch/cfg"
printf '[providers.x]\ntype = "openai-compatible"\n' >"$scratch/cfg/config.toml"
export EVORCH_CONFIG_SRC=$scratch/cfg
export EVORCH_BIN=$here/dummy-evorch.sh
export EVAL_WORK=$scratch/work
export DUMMY_LOG=$scratch/dummy.log

# make-overlay
"$ev/scripts/make-overlay.sh" 'gpt-luna+fast' high "$scratch/ovl" >/dev/null
check "overlay: config.toml コピー" "cmp -s $scratch/cfg/config.toml $scratch/ovl/config.toml"
check "overlay: zz-worker.toml" "[ \"\$(cat $scratch/ovl/config.d/zz-worker.toml)\" = \"\$(printf '[agents.worker]\nmodels = [{ route = \"gpt-luna+fast\", reasoning_effort = \"high\" }]')\" ]"
check "overlay: リポジトリ内は拒否" "! $ev/scripts/make-overlay.sh gpt-sol high $ev/ovl-should-not-exist 2>/dev/null; [ ! -e $ev/ovl-should-not-exist ]"
check "overlay: 不正な route は拒否" "! $ev/scripts/make-overlay.sh 'a\"b' high $scratch/ovl2 2>/dev/null"

# run-one（t5 を参照解で解いたことにする → verify pass）
out=$scratch/out
DUMMY_SOLUTION=$ev/solutions/t5-single-edit.sh "$ev/scripts/run-one.sh" gpt-sol t5-single-edit 1 "$out" --effort high
d=$out/gpt-sol/t5-single-edit/1
check "run-one: result.json" "[ -f $d/result.json ]"
check "run-one: verify=pass" "grep -q '\"verify\": \"pass\"' $d/result.json"
check "run-one: status=ok / run_id" "grep -q '\"status\": \"ok\"' $d/result.json && grep -q '\"run_id\": \"run-fixture-1\"' $d/result.json"
check "run-one: 引数 (--run-js-enabled true / --db / --user-config)" "grep -q -- '--run-js-enabled true' $scratch/dummy.log && grep -q -- \"--db $d/run.db\" $scratch/dummy.log && grep -q -- '--user-config' $scratch/dummy.log"
check "run-one: inspect 保存" "[ -s $d/inspect-run.json ] && [ -s $d/inspect-events.json ]"
check "run-one: final.diff" "[ -s $d/final.diff ]"
check "run-one: clone と overlay は消えている" "[ -z \"\$(ls -A $scratch/work)\" ]"
check "run-one: overlay の config.toml を結果に残さない" "[ ! -e $d/config.toml ]"
before=$(wc -l <"$scratch/dummy.log")
"$ev/scripts/run-one.sh" gpt-sol t5-single-edit 1 "$out" | grep -q '^skip'
check "run-one: 再実行はスキップ" "[ \"\$(wc -l <$scratch/dummy.log)\" = $before ]"

# タイムアウト（解かない → verify fail、status=timeout）
DUMMY_SLEEP=20 "$ev/scripts/run-one.sh" claude-sonnet t5-single-edit 1 "$out" --timeout 1 --run-js false
d=$out/claude-sonnet/t5-single-edit/1
check "run-one: timeout" "grep -q '\"status\": \"timeout\"' $d/result.json && grep -q '\"timed_out\": true' $d/result.json && grep -q '\"run_js_enabled\": false' $d/result.json"
check "run-one: timeout 後も verify を実行（fail）" "grep -q '\"verify\": \"fail\"' $d/result.json"

# evorch の異常終了 → status=error
DUMMY_EXIT=1 "$ev/scripts/run-one.sh" gpt-astra t1-survey 1 "$out"
check "run-one: error status" "grep -q '\"status\": \"error\"' $out/gpt-astra/t1-survey/1/result.json"

# 不正な入力
check "run-one: 未知のモデル → 2" "$ev/scripts/run-one.sh nope t5-single-edit 1 $out 2>/dev/null; [ \$? = 2 ]"
check "run-one: EVORCH_BIN 無し → 2" "EVORCH_BIN= $ev/scripts/run-one.sh gpt-sol t5-single-edit 1 $out 2>/dev/null; [ \$? = 2 ]"

# run-matrix: 計画 → 実行 → 再開
m=$scratch/matrix
"$ev/scripts/run-matrix.sh" --outroot "$m" --models "gpt-sol claude-opus" --tasks "t1-survey t5-single-edit" --reps 2 --list 2>/dev/null >"$scratch/plan.txt"
check "matrix: --list は 8 組" "[ \"\$(grep -c '^run ' $scratch/plan.txt)\" = 8 ]"
"$ev/scripts/run-matrix.sh" --outroot "$m" --models "gpt-sol claude-opus" --tasks "t1-survey t5-single-edit" --reps 2 -j 2 >/dev/null 2>&1
check "matrix: -j 2 で 8 組完了" "[ \"\$(find $m -name result.json | wc -l)\" = 8 ]"
rm -rf "$m/claude-opus/t1-survey/2"
"$ev/scripts/run-matrix.sh" --outroot "$m" --models "gpt-sol claude-opus" --tasks "t1-survey t5-single-edit" --reps 2 2>&1 | grep -q '実行 1 / スキップ 7'
check "matrix: 欠けた 1 組だけ再開" "[ \"\$(find $m -name result.json | wc -l)\" = 8 ]"
"$ev/scripts/run-matrix.sh" --outroot "$out" --models "gpt-astra" --tasks "t1-survey" --retry-errors 2>&1 | grep -q '実行 1 / スキップ 0'
check "matrix: --retry-errors で error を再実行" "grep -q '\"status\": \"ok\"' $out/gpt-astra/t1-survey/1/result.json"

# metrics（保存された inspect 出力から）
node "$ev/scripts/metrics.mjs" --results "$m" --format json >"$scratch/metrics.json"
check "metrics: 8 run を集計" "[ \"\$(node -e 'console.log(JSON.parse(require(\"fs\").readFileSync(process.argv[1])).runs.length)' $scratch/metrics.json)\" = 8 ]"
# 保存済み inspect を消すと DB 経由（dummy の inspect）で読む
rm "$m/gpt-sol/t1-survey/1/inspect-run.json"
node "$ev/scripts/metrics.mjs" --results "$m" --format json >"$scratch/metrics2.json"
check "metrics: inspect 出力が無ければ DB を evorch inspect で読む" "[ \"\$(node -e 'const j=JSON.parse(require(\"fs\").readFileSync(process.argv[1]));console.log(j.runs.find(r=>r.model===\"gpt-sol\"&&r.task===\"t1-survey\"&&r.rep===\"1\").usage.requests)' $scratch/metrics2.json)\" = 6 ]"
check "metrics: --db / --run の直接指定" "node $ev/scripts/metrics.mjs --db $m/gpt-sol/t5-single-edit/1/run.db --run run-fixture-1=gpt-sol/t5-single-edit/9 | grep -q 't5-single-edit | 9'"

echo
[ "$fail" = 0 ] && echo "scripts-test: all ok" || echo "scripts-test: FAILED"
exit $fail
