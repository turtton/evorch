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
printf '[providers.codex-personal]\ntype = "openai-codex"\n\n[providers.claude-personal]\ntype = "anthropic-subscription"\n\n[providers.codex-work]\ntype = "openai-codex"\n' >"$scratch/cfg/config.toml"
export EVORCH_CONFIG_SRC=$scratch/cfg
export EVORCH_BIN=$here/dummy-evorch.sh
export EVAL_WORK=$scratch/work
export DUMMY_LOG=$scratch/dummy.log

# make-overlay
"$ev/scripts/make-overlay.sh" codex-personal 'gpt-6-luna+fast' high "$scratch/ovl" >/dev/null
check "overlay: config.toml コピー" "cmp -s $scratch/cfg/config.toml $scratch/ovl/config.toml"
check "overlay: zz-worker.toml（候補 1 件の eval-pinned route を Worker に割り当てる）" "[ \"\$(cat $scratch/ovl/config.d/zz-worker.toml)\" = \"\$(printf '[[routing.routes.eval-pinned]]\nprofile = \"codex-personal\"\nmodel = \"gpt-6-luna+fast\"\n\n[agents.worker]\nmodels = [{ route = \"eval-pinned\", reasoning_effort = \"high\" }]')\" ]"
check "overlay: リポジトリ内は拒否" "! $ev/scripts/make-overlay.sh codex-personal gpt-sol high $ev/ovl-should-not-exist 2>/dev/null; [ ! -e $ev/ovl-should-not-exist ]"
check "overlay: 不正な model は拒否" "! $ev/scripts/make-overlay.sh codex-personal 'a\"b' high $scratch/ovl2 2>/dev/null"
check "overlay: *-work の profile は拒否" "! $ev/scripts/make-overlay.sh codex-work gpt-sol high $scratch/ovl3 2>/dev/null; [ ! -e $scratch/ovl3/config.d ]"
check "overlay: config.toml に無い profile は拒否" "! EVORCH_CONFIG_SRC=$scratch/nocfg $ev/scripts/make-overlay.sh claude-personal claude-opus-5-5 high $scratch/ovl4 2>/dev/null"
for k in gpt-sol gpt-luna-fast gpt-astra claude-opus claude-sonnet; do
  check "models: $k は personal profile に固定" "(. $ev/scripts/models.sh; eval_target $k | grep -Eq '^(codex|claude)-personal ')"
done

# run-one（t5 を参照解で解いたことにする → verify pass）
out=$scratch/out
DUMMY_SOLUTION=$ev/solutions/t5-single-edit-v2.sh "$ev/scripts/run-one.sh" gpt-sol t5-single-edit-v2 1 "$out" --effort high
d=$out/gpt-sol/t5-single-edit-v2/1
check "run-one: result.json" "[ -f $d/result.json ]"
check "run-one: verify=pass" "grep -q '\"verify\": \"pass\"' $d/result.json"
check "run-one: status=ok / run_id" "grep -q '\"status\": \"ok\"' $d/result.json && grep -q '\"run_id\": \"run-fixture-1\"' $d/result.json"
check "run-one: 引数 (--run-js-enabled true / --db / --user-config)" "grep -q -- '--run-js-enabled true' $scratch/dummy.log && grep -q -- \"--db $d/run.db\" $scratch/dummy.log && grep -q -- '--user-config' $scratch/dummy.log"
check "run-one: inspect 保存" "[ -s $d/inspect-run.json ] && [ -s $d/inspect-events.json ]"
check "run-one: final.diff" "[ -s $d/final.diff ]"
check "run-one: clone と overlay は消えている" "[ -z \"\$(ls -A $scratch/work)\" ]"
check "run-one: overlay の config.toml を結果に残さない" "[ ! -e $d/config.toml ]"
before=$(wc -l <"$scratch/dummy.log")
"$ev/scripts/run-one.sh" gpt-sol t5-single-edit-v2 1 "$out" | grep -q '^skip'
check "run-one: 再実行はスキップ" "[ \"\$(wc -l <$scratch/dummy.log)\" = $before ]"

# タイムアウト（解かない → verify fail、status=timeout）
DUMMY_SLEEP=20 "$ev/scripts/run-one.sh" claude-sonnet t5-single-edit-v2 1 "$out" --timeout 1 --run-js false
d=$out/claude-sonnet/t5-single-edit-v2/1
check "run-one: timeout" "grep -q '\"status\": \"timeout\"' $d/result.json && grep -q '\"timed_out\": true' $d/result.json && grep -q '\"run_js_enabled\": false' $d/result.json"
check "run-one: timeout 後も verify を実行（fail）" "grep -q '\"verify\": \"fail\"' $d/result.json"

# evorch の異常終了 → status=error
DUMMY_EXIT=1 "$ev/scripts/run-one.sh" gpt-astra t1-survey-v2 1 "$out"
check "run-one: error status" "grep -q '\"status\": \"error\"' $out/gpt-astra/t1-survey-v2/1/result.json"

# watchdog: 質問して入力待ちになった run（TurnCompleted が最後のイベントのまま終了しない）を打ち切る
t_start=$(date +%s)
DUMMY_STALL=1 EVAL_WATCHDOG_POLL_S=1 "$ev/scripts/run-one.sh" claude-opus t5-single-edit-v2 7 "$out" --idle-timeout 3 --timeout 120 >/dev/null 2>&1
t_end=$(date +%s)
d=$out/claude-opus/t5-single-edit-v2/7
check "watchdog: stalled run を打ち切る（status=stalled、timeout まで待たない）" "grep -q '\"status\": \"stalled\"' $d/result.json && [ $((t_end - t_start)) -lt 60 ]"
check "watchdog: 最後の発言を error_reason に残す" "grep -q 'error_reason.*TurnCompleted のあと 3s 動かず.*この方針で進めてよいですか' $d/result.json && grep -q '\"idle_timeout_s\": 3' $d/result.json"
check "watchdog: stalled の後も verify を実行する" "grep -q '\"verify\": \"fail\"' $d/result.json"
check "watchdog: evorch は残らない" "! pgrep -f 'dummy-evorch.sh run .*t5-single-edit-v2-7' >/dev/null"
DUMMY_STALL=1 EVAL_WATCHDOG_POLL_S=1 DUMMY_SOLUTION=$ev/solutions/t5-single-edit-v2.sh "$ev/scripts/run-one.sh" claude-opus t5-single-edit-v2 8 "$out" --idle-timeout 0 --timeout 3 >/dev/null 2>&1
check "watchdog: --idle-timeout 0 で無効（timeout まで待つ）" "grep -q '\"status\": \"timeout\"' $out/claude-opus/t5-single-edit-v2/8/result.json"

# 不正な入力
check "run-one: 未知のモデル → 2" "$ev/scripts/run-one.sh nope t5-single-edit-v2 1 $out 2>/dev/null; [ \$? = 2 ]"
check "run-one: EVORCH_BIN 無し → 2" "EVORCH_BIN= $ev/scripts/run-one.sh gpt-sol t5-single-edit-v2 1 $out 2>/dev/null; [ \$? = 2 ]"

# run-matrix: 計画 → 実行 → 再開
m=$scratch/matrix
"$ev/scripts/run-matrix.sh" --outroot "$m" --models "gpt-sol claude-opus" --tasks "t1-survey-v2 t5-single-edit-v2" --reps 2 --list 2>/dev/null >"$scratch/plan.txt"
check "matrix: --list は 8 組" "[ \"\$(grep -c '^run ' $scratch/plan.txt)\" = 8 ]"
"$ev/scripts/run-matrix.sh" --outroot "$m" --models "gpt-sol claude-opus" --tasks "t1-survey-v2 t5-single-edit-v2" --reps 2 -j 2 >/dev/null 2>&1
check "matrix: -j 2 で 8 組完了" "[ \"\$(find $m -name result.json | wc -l)\" = 8 ]"
rm -rf "$m/claude-opus/t1-survey-v2/2"
"$ev/scripts/run-matrix.sh" --outroot "$m" --models "gpt-sol claude-opus" --tasks "t1-survey-v2 t5-single-edit-v2" --reps 2 >"$scratch/resume.out" 2>&1
grep -q '実行 1 / スキップ 7' "$scratch/resume.out"
check "matrix: 欠けた 1 組だけ再開" "[ \"\$(find $m -name result.json | wc -l)\" = 8 ]"
"$ev/scripts/run-matrix.sh" --outroot "$out" --models "gpt-astra" --tasks "t1-survey-v2" --retry-errors --no-preflight >"$scratch/retry.out" 2>&1
grep -q '実行 1 / スキップ 0' "$scratch/retry.out"
check "matrix: --retry-errors で error を再実行" "grep -q '\"status\": \"ok\"' $out/gpt-astra/t1-survey-v2/1/result.json"
check "matrix: --retry-errors は最初の試行を .attempt1 へ退避（消さない）" "grep -q '\"status\": \"error\"' $out/gpt-astra/t1-survey-v2/1.attempt1/result.json"
DUMMY_EXIT=1 "$ev/scripts/run-one.sh" gpt-astra t1-survey-v2 1 "$out" --force >/dev/null 2>&1
DUMMY_EXIT=1 "$ev/scripts/run-one.sh" gpt-astra t1-survey-v2 1 "$out" --force >/dev/null 2>&1
check "run-one: --force は既存の結果を .attempt<N> へ順に退避" "[ -f $out/gpt-astra/t1-survey-v2/1.attempt2/result.json ] && [ -f $out/gpt-astra/t1-survey-v2/1.attempt3/result.json ]"
check "run-one: rep に .attempt<N> は使えない" "$ev/scripts/run-one.sh gpt-astra t1-survey-v2 1.attempt1 $out 2>/dev/null; [ \$? = 2 ]"

# status=error だが成果物は正しい run（provider の SSE エラーなど）
DUMMY_SOLUTION=$ev/solutions/t5-single-edit-v2.sh DUMMY_EXIT=1 DUMMY_ERROR_REASON='モデル呼び出しに失敗しました: invalid SSE stream: missing field message' \
  "$ev/scripts/run-one.sh" claude-opus t5-single-edit-v2 1 "$out" | grep -q 'verify=pass'
d=$out/claude-opus/t5-single-edit-v2/1
check "run-one: error だが verify=pass の run を区別して出力" "grep -q '\"status\": \"error\"' $d/result.json && grep -q '\"verify\": \"pass\"' $d/result.json"
check "run-one: error_reason を result.json に残す（ANSI なし）" "grep -q '\"error_reason\": \"モデル呼び出しに失敗しました: invalid SSE stream: missing field message\"' $d/result.json"

# preflight（ダミー evorch。使えないモデルは provider のエラー形式で失敗する）
: >"$scratch/dummy.log"
check "preflight: 使えるモデルは ok（極小プロンプトを 1 回流す）" "$ev/scripts/preflight.sh gpt-sol >/dev/null && [ \"\$(grep -c 'Reply with the single word OK' $scratch/dummy.log)\" = 1 ] && grep -q -- '--run-js-enabled false' $scratch/dummy.log"
check "preflight: 使えないモデルは exit 1 で理由を出す" "DUMMY_UNAVAILABLE_MODEL=gpt-6.1-sol $ev/scripts/preflight.sh gpt-sol 2>&1 | grep -q 'model is not advertised by provider'; [ \${PIPESTATUS[0]} = 1 ]"
check "preflight: 未知のモデルキー → 2" "$ev/scripts/preflight.sh nope 2>/dev/null; [ \$? = 2 ]"
pf=$scratch/pf
$ev/scripts/preflight.sh claude-sonnet --outroot $pf >/dev/null
n_before=$(grep -c 'Reply with the single word OK' $scratch/dummy.log)
$ev/scripts/preflight.sh claude-sonnet --outroot $pf | grep -q '記録あり'
check "preflight: 成功は .preflight/<model>.ok に記録し、再実行しない" "[ -f $pf/.preflight/claude-sonnet.ok ] && [ \"\$(grep -c 'Reply with the single word OK' $scratch/dummy.log)\" = $n_before ]"

# run-matrix の preflight: 使えないモデルの組は実行せず、使えるモデルは続ける
pm=$scratch/pm
DUMMY_UNAVAILABLE_MODEL=gpt-6.1-sol "$ev/scripts/run-matrix.sh" --outroot "$pm" --models "gpt-sol claude-opus" --tasks "t5-single-edit-v2" >"$scratch/pm.out" 2>&1
pm_rc=$?
check "matrix: 使えないモデルは preflight で外れ、exit 1" "[ $pm_rc = 1 ] && grep -q 'preflight 失敗: gpt-sol' $scratch/pm.out && grep -q '使えないモデルの組は実行しない: gpt-sol' $scratch/pm.out"
check "matrix: 使えないモデルは空振りの結果を作らない" "[ ! -e $pm/gpt-sol ]"
check "matrix: 使えるモデルは実行される" "[ -f $pm/claude-opus/t5-single-edit-v2/1/result.json ] && [ -f $pm/.preflight/claude-opus.ok ]"
: >"$scratch/dummy.log"
"$ev/scripts/run-matrix.sh" --outroot "$scratch/pm2" --models "gpt-astra" --tasks "t5-single-edit-v2" --no-preflight >/dev/null 2>&1
check "matrix: --no-preflight は確認を流さない" "! grep -q 'Reply with the single word OK' $scratch/dummy.log"
"$ev/scripts/run-matrix.sh" --outroot "$scratch/pm3" --models "gpt-astra" --tasks "t5-single-edit-v2" --preflight-only >/dev/null 2>&1
check "matrix: --preflight-only は run を流さない" "[ ! -e $scratch/pm3/gpt-astra ] && [ -f $scratch/pm3/.preflight/gpt-astra.ok ]"

# metrics（保存された inspect 出力から）
node "$ev/scripts/metrics.mjs" --results "$m" --format json >"$scratch/metrics.json"
check "metrics: 8 run を集計" "[ \"\$(node -e 'console.log(JSON.parse(require(\"fs\").readFileSync(process.argv[1])).runs.length)' $scratch/metrics.json)\" = 8 ]"
# 保存済み inspect を消すと DB 経由（dummy の inspect）で読む
rm "$m/gpt-sol/t1-survey-v2/1/inspect-run.json"
node "$ev/scripts/metrics.mjs" --results "$m" --format json >"$scratch/metrics2.json"
check "metrics: inspect 出力が無ければ DB を evorch inspect で読む" "[ \"\$(node -e 'const j=JSON.parse(require(\"fs\").readFileSync(process.argv[1]));console.log(j.runs.find(r=>r.model===\"gpt-sol\"&&r.task===\"t1-survey-v2\"&&r.rep===\"1\").usage.requests)' $scratch/metrics2.json)\" = 6 ]"
check "metrics: --db / --run の直接指定" "node $ev/scripts/metrics.mjs --db $m/gpt-sol/t5-single-edit-v2/1/run.db --run run-fixture-1=gpt-sol/t5-single-edit-v2/9 | grep -q 't5-single-edit-v2 | 9'"

# metrics: 結果ディレクトリ直下の余計なディレクトリと退避された試行は読まない
mkdir -p "$out/timeline" "$out/notes"
echo x >"$out/timeline/gpt-sol-t1.txt"
node "$ev/scripts/metrics.mjs" --results "$out" --format json >"$scratch/metrics3.json" 2>"$scratch/metrics3.err"
n_main=$(find "$out" -name result.json -not -path '*.attempt*' | wc -l)
check "metrics: timeline/ などの余計なディレクトリと .attempt<N> を読まない" "[ \"\$(node -e 'const j=JSON.parse(require(\"fs\").readFileSync(process.argv[1]));console.log(j.runs.length, j.runs.some(r=>String(r.rep).includes(\"attempt\")))' $scratch/metrics3.json)\" = '$n_main false' ]"
node "$ev/scripts/metrics.mjs" --results "$out" --include-attempts --format json >"$scratch/metrics4.json" 2>/dev/null
check "metrics: --include-attempts で退避された試行も読む" "[ \"\$(node -e 'const j=JSON.parse(require(\"fs\").readFileSync(process.argv[1]));console.log(j.runs.filter(r=>String(r.rep).includes(\"attempt\")).length)' $scratch/metrics4.json)\" -ge 3 ]"
check "metrics: error だが verify=pass は outcome=error+pass" "[ \"\$(node -e 'const j=JSON.parse(require(\"fs\").readFileSync(process.argv[1]));console.log(j.runs.find(r=>r.model===\"claude-opus\"&&r.task===\"t5-single-edit-v2\").outcome)' $scratch/metrics3.json)\" = error+pass ]"
check "metrics: 親ディレクトリを指すとエラー（結果が 1 つもない）" "node $ev/scripts/metrics.mjs --results $scratch 2>/dev/null; [ \$? = 1 ]"

echo
[ "$fail" = 0 ] && echo "scripts-test: all ok" || echo "scripts-test: FAILED"
exit $fail
