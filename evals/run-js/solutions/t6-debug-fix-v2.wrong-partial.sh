#!/usr/bin/env bash
# 部分誤答: agents の role.rs を直し忘れる（apply-patch の 4 つは直し、報告は 5 ファイル分書く）。
set -euo pipefail
cd "${1:?usage}"
git checkout 4391a7e5 -- crates/apply-patch/src/seek_sequence.rs crates/apply-patch/src/text_file.rs crates/apply-patch/src/targets.rs crates/apply-patch/src/apply.rs
git reset -q
printf '%s\n' seek_sequence.rs text_file.rs targets.rs apply.rs role.rs a b > answers/t6.md
