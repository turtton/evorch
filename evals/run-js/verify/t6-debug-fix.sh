#!/usr/bin/env bash
# t6-debug-fix: 仕込んだ 2 つの退行が直り、テストが無改変で通り、原因の報告に両方のファイルが挙がっている。
source "$(dirname "$0")/lib/common.sh"

# 1. 変更が src の 2 ファイルに限られ、seek_sequence.rs のテスト部も無改変。
extra=$(changed_files | grep -vE '^crates/apply-patch/src/(seek_sequence|text_file)\.rs$' || true)
[ -z "$extra" ] || fail "範囲外の変更: $extra"
tests_of() { sed -n '/#\[cfg(test)\]/,$p'; }
diff <(git show "$BASE:crates/apply-patch/src/seek_sequence.rs" | tests_of) <(tests_of < crates/apply-patch/src/seek_sequence.rs) >/dev/null \
  || fail "seek_sequence.rs のテスト部が変更されている"
ok "変更範囲とテストの無改変"

# 2. テスト全体が通る。
out=$(cargo test -p apply-patch --offline 2>&1) || { echo "$out" | grep -E 'FAILED|failed' | head >&2; fail "cargo test -p apply-patch が失敗"; }
ok "cargo test -p apply-patch passed=$(echo "$out" | sum_passed)"

# 3. 原因の報告。
[ -f answers/t6.md ] || fail "answers/t6.md がない"
grep -q 'seek_sequence' answers/t6.md || fail "answers/t6.md に seek_sequence の記述がない"
grep -q 'text_file' answers/t6.md || fail "answers/t6.md に text_file の記述がない"
ok "answers/t6.md"
