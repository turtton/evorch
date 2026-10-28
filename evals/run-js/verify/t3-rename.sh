#!/usr/bin/env bash
# t3-rename: 改名の網羅性・過剰置換の無さ・変更範囲・テストの合格を確かめる。
source "$(dirname "$0")/lib/common.sh"

# 1. 旧名が crates/ に残っていない。
old=$(git grep -nwE 'UpdateFileChunk|is_end_of_file' -- crates || true)
if [ -n "$old" ]; then
  echo "$old" | head -5 >&2
  fail "旧名が残っている"
fi
ok "旧名なし"

# 2. 新名の出現数が元の出現数と一致する（31 箇所と 20 箇所）。
n_chunk=$(git grep -nw 'UpdateChunk' -- crates | wc -l | tr -d ' ')
n_eof=$(git grep -nw 'at_eof' -- crates | wc -l | tr -d ' ')
[ "$n_chunk" = 31 ] || fail "UpdateChunk の出現行数が $n_chunk（期待 31）"
[ "$n_eof" = 20 ] || fail "at_eof の出現行数が $n_eof（期待 20）"
ok "新名の出現数 UpdateChunk=$n_chunk at_eof=$n_eof"

# 3. パッチ文法のマーカー文字列を壊していない。
before=$(git grep -c 'End of File' "$BASE" -- crates/apply-patch | sed "s/^$BASE://" | sort)
after=$(git grep -c 'End of File' -- crates/apply-patch | sort)
[ "$before" = "$after" ] || fail "'End of File' の出現が変わった"
ok "'End of File' は不変"

# 4. 変更が crates/apply-patch の *.rs に限られ、5 ファイル以内（改名対象は 5 ファイル）。
outside=$(changed_files | grep -v '^crates/apply-patch/.*\.rs$' || true)
[ -z "$outside" ] || fail "範囲外の変更: $outside"
[ "$(changed_files | wc -l)" -le 5 ] || fail "変更ファイルが多すぎる: $(changed_files | tr '\n' ' ')"
ok "変更範囲: $(changed_files | wc -l) ファイル"

# 5. ビルドとテスト。
out=$(cargo test -p apply-patch --offline 2>&1) || { echo "$out" | tail -20 >&2; fail "cargo test -p apply-patch が失敗"; }
ok "cargo test -p apply-patch"
