#!/usr/bin/env bash
set -euo pipefail
cd "${1:?usage}"
git checkout 4391a7e5 -- crates/apply-patch/src/seek_sequence.rs crates/apply-patch/src/text_file.rs crates/apply-patch/src/targets.rs crates/apply-patch/src/apply.rs crates/agents/src/role.rs
git reset -q
printf '%s\n' \
  'crates/apply-patch/src/seek_sequence.rs: EOF 検索の開始位置が max ではなく min になっていた。' \
  'crates/apply-patch/src/text_file.rs: 挿入行の改行は最初の改行を採るのに、最後の改行で上書きしていた。' \
  'crates/apply-patch/src/targets.rs: 親ディレクトリ（..）の検査が先頭の要素しか見ていなかった。' \
  'crates/apply-patch/src/apply.rs: 削除の報告が modified の枠に入っていた（added/modified/deleted の順が崩れた）。' \
  'crates/agents/src/role.rs: 小文字の "oracle" の別名が from_name から消えていた。' > answers/t6.md
