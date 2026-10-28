#!/usr/bin/env bash
# t5-single-edit: grep.rs の 2 行だけが期待どおり変わっている。
source "$(dirname "$0")/lib/common.sh"

f=crates/tools/src/tools/grep.rs
[ "$(changed_files)" = "$f" ] || fail "変更ファイルが $f だけではない: $(changed_files | tr '\n' ' ')"
[ "$(git diff --numstat "$BASE" -- "$f" | cut -f1,2)" = "$(printf '2\t2')" ] || fail "差分が 2 行追加・2 行削除ではない"
grep -qx 'const MAX_LINES: usize = 150;' "$f" || fail "MAX_LINES が 150 になっていない"
sed -n '4p' "$f" | grep -q '最大 150 行 / 8 KiB' || fail "4 行目の doc コメントが 150 になっていない"
git diff "$BASE" -- "$f" | grep -E '^[-+][^-+]' | grep -vE '200|150' && fail "MAX_LINES 以外の行が変わっている" || true
ok "grep.rs の MAX_LINES と doc コメントだけが 200 → 150"
