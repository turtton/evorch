# shellcheck shell=bash
# run-one.sh / run-matrix.sh が source する共通定義（モデルキーと既定値）。
# モデルキー → Worker に固定する (provider profile, モデル ID)。
# 利用者の合意: サブスクリプションの profile だけを使う（codex-personal / claude-personal）。
# *-work の profile や API キーの profile は使わない。キー自体はディレクトリ名に使うため '+' を含めない。

# shellcheck disable=SC2034
EVAL_MODELS="gpt-sol gpt-luna-fast gpt-astra claude-opus claude-sonnet"
# 既定の課題は v3（観察の 2 回目の結果を受けて、さらに重くしたもの）。v2 は EVAL_TASKS_V2、v1 は EVAL_TASKS_V1、
# または --tasks で指定する。
# shellcheck disable=SC2034
EVAL_TASKS="t1-survey-v3 t2-aggregate-v3 t3-rename-v3 t4-build-test-v3 t5-single-edit-v3 t6-debug-fix-v3"
# shellcheck disable=SC2034
EVAL_TASKS_V2="t1-survey-v2 t2-aggregate-v2 t3-rename-v2 t4-build-test-v2 t5-single-edit-v2 t6-debug-fix-v2"
# shellcheck disable=SC2034
EVAL_TASKS_V1="t1-survey t2-aggregate t3-rename t4-build-test t5-single-edit t6-debug-fix"
# reasoning_effort の既定値。--effort か EVAL_EFFORT で上書きする（モデルごとの適切な値は実行前に決める）。
EVAL_DEFAULT_EFFORT=${EVAL_DEFAULT_EFFORT:-medium}

# eval_target <model-key>: "<profile> <model-id>" を標準出力へ。未知のキーは return 1。
eval_target() {
  case $1 in
    gpt-sol) echo 'codex-personal gpt-6.1-sol' ;;
    gpt-luna-fast | gpt-luna+fast) echo 'codex-personal gpt-6-luna+fast' ;;
    gpt-astra) echo 'codex-personal gpt-6-astra' ;;
    claude-opus) echo 'claude-personal claude-opus-5-5' ;;
    claude-sonnet) echo 'claude-personal claude-sonnet-5-5' ;;
    *) return 1 ;;
  esac
}

# eval_model_dir <model-key>: 結果ディレクトリ用の名前（'+' を '-' にする）。
eval_model_dir() { printf '%s\n' "${1//+/-}"; }

# eval_archive_attempt <rundir>: 既存の結果ディレクトリを <rundir>.attempt<N>（N は未使用の最小の番号）へ退避して、そのパスを出す。
# --retry-errors / --force が最初の試行（error の結果など）を消さないために使う。metrics.mjs は既定で .attempt<N> を読まない。
eval_archive_attempt() {
  local dir=$1 n=1
  while [ -e "$dir.attempt$n" ]; do n=$((n + 1)); done
  mv "$dir" "$dir.attempt$n" && printf '%s\n' "$dir.attempt$n"
}
