#!/usr/bin/env bash
# 参照解（検証スクリプトの自己確認用）。使い方: solutions/<id>.sh <clone-dir>
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cp "$here/verify/expected/t1-survey.json" "${1:?usage}/answers/t1.json"
