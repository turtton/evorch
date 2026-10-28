# shellcheck shell=bash
# run-one.sh / run-matrix.sh が source する共通定義（モデルキーと既定値）。
# モデルキー → overlay の route 名。キー自体はディレクトリ名に使うため '+' を含めない。

# shellcheck disable=SC2034
EVAL_MODELS="gpt-sol gpt-luna-fast gpt-astra claude-opus claude-sonnet"
# shellcheck disable=SC2034
EVAL_TASKS="t1-survey t2-aggregate t3-rename t4-build-test t5-single-edit t6-debug-fix"
# reasoning_effort の既定値。--effort か EVAL_EFFORT で上書きする（モデルごとの適切な値は実行前に決める）。
EVAL_DEFAULT_EFFORT=${EVAL_DEFAULT_EFFORT:-medium}

# eval_route <model-key>: route 名を標準出力へ。未知のキーは return 1。
eval_route() {
  case $1 in
    gpt-sol) echo gpt-sol ;;                       # gpt-6.1-sol
    gpt-luna-fast | gpt-luna+fast) echo 'gpt-luna+fast' ;;  # gpt-6-luna+fast
    gpt-astra) echo gpt-astra ;;                   # gpt-6-astra
    claude-opus) echo claude-opus ;;               # claude-opus-5-5
    claude-sonnet) echo claude-sonnet ;;           # claude-sonnet-5-5
    *) return 1 ;;
  esac
}

# eval_model_dir <model-key>: 結果ディレクトリ用の名前（'+' を '-' にする）。
eval_model_dir() { printf '%s\n' "${1//+/-}"; }
