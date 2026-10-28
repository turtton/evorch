#!/usr/bin/env bash
# 部分誤答: event-bus の ring.rs を直し忘れる（ほかは直して新機能も残す）。
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "${1:?usage}"
"$here/t6-debug-fix-v3.sh" "$PWD"
git checkout HEAD -- crates/event-bus/src/ring.rs
git reset -q
