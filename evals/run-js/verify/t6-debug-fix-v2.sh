#!/usr/bin/env bash
# t6-debug-fix-v2: 仕込んだ 5 つの退行が直り、テストが無改変で通り、原因の報告に 5 ファイルすべてが挙がっている。
source "$(dirname "$0")/lib/common.sh"

FILES="crates/apply-patch/src/seek_sequence.rs crates/apply-patch/src/text_file.rs crates/apply-patch/src/targets.rs crates/apply-patch/src/apply.rs crates/agents/src/role.rs"

# 1. 変更が 5 ファイルに限られ、インライン・テスト部（#[cfg(test)] 以降）が無改変。
allowed=$(printf '%s\n' $FILES | paste -sd'|')
extra=$(changed_files | grep -vE "^($allowed)\$" || true)
[ -z "$extra" ] || fail "範囲外の変更: $extra"
tests_of() { sed -n '/#\[cfg(test)\]/,$p'; }
for f in $FILES; do
  diff <(git show "$BASE:$f" | tests_of) <(tests_of <"$f") >/dev/null || fail "$f のテスト部が変更されている"
done
ok "変更範囲とテストの無改変"

# 2. 両 crate のテストが全部通る（失敗した binary で止まらないよう --no-fail-fast）。
out=$(cargo test -p apply-patch -p agents --offline --no-fail-fast 2>&1) || { echo "$out" | tail -25 >&2; fail "cargo test -p apply-patch -p agents が失敗"; }
ok "cargo test passed=$(echo "$out" | sum_passed)"

# 3. 原因の報告: 5 ファイルが全部、1 ファイル 1 行以上で挙がっている。
[ -f answers/t6.md ] || fail "answers/t6.md がない"
for name in seek_sequence.rs text_file.rs targets.rs apply.rs role.rs; do
  grep -qF "$name" answers/t6.md || fail "answers/t6.md に $name の記述がない"
done
[ "$(grep -c . answers/t6.md)" -ge 5 ] || fail "answers/t6.md が 5 行未満"
ok "answers/t6.md"
