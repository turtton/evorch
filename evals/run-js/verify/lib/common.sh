# shellcheck shell=bash
# 各 verify/<id>.sh から source する共通部。
#   verify/<id>.sh <clone-dir>
# 終了コード: 0 = 合格、1 = 不合格、2 = 使い方や環境の誤り。
set -euo pipefail

BASE=4391a7e5
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)   # evals/run-js/verify
CLONE=${1:-}
if [ -z "$CLONE" ] || [ ! -d "$CLONE/.git" ]; then
  echo "usage: $0 <clone-dir>" >&2
  exit 2
fi
CLONE=$(cd "$CLONE" && pwd)
cd "$CLONE"
git rev-parse --verify -q "$BASE^{commit}" >/dev/null || { echo "clone に $BASE がない" >&2; exit 2; }

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

# 追跡ファイルの $BASE からの差分一覧（answers/ などの未追跡ファイルは含まない）。
changed_files() { git diff --name-only "$BASE"; }

# `cargo test` の出力から "N passed" の合計を求める。
sum_passed() { awk '/^test result: ok\./ { s += $4 } END { print s + 0 }'; }
