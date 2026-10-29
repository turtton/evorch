#!/usr/bin/env bash
# tasks/<id>.md の ```prompt ブロックを、そのまま標準出力へ出す（Worker に渡す全文）。
# 使い方: scripts/prompt.sh <task-id>
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
id=${1:?usage: $0 <task-id>}
awk '/^```prompt$/ { on = 1; next } /^```$/ && on { exit } on' "$here/tasks/$id.md"
