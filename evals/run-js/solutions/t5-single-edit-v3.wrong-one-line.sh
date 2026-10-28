#!/usr/bin/env bash
# 部分誤答: 定数だけ直して doc コメントの数字を直し忘れる。
set -euo pipefail
cd "${1:?usage}"
sed -i 's/^const MAX_LINES: usize = 200;/const MAX_LINES: usize = 150;/' crates/tools/src/tools/grep.rs
