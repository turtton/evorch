#!/usr/bin/env bash
set -euo pipefail
cd "${1:?usage}"
git checkout 4391a7e5 -- crates/apply-patch/src/seek_sequence.rs crates/apply-patch/src/text_file.rs
git reset -q
printf '%s\n' \
  'seek_sequence.rs: EOF 検索の開始位置が max ではなく min になっていた。' \
  'text_file.rs: 挿入行の改行は最初の改行を採るのに、最後の改行で上書きしていた。' > answers/t6.md
