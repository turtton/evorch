#!/usr/bin/env bash
# t6-debug-fix-v3: 仕込んだ 8 つの退行が直り、新機能（ChangeKind::label）が残り、テストが無改変で通り、報告に 7 ファイルが挙がっている。
source "$(dirname "$0")/lib/common.sh"

FILES="crates/apply-patch/src/seek_sequence.rs crates/apply-patch/src/text_file.rs crates/apply-patch/src/targets.rs crates/apply-patch/src/apply.rs crates/event-bus/src/ring.rs crates/event-bus/src/otel/span/span_budget.rs crates/agents/src/role.rs"
KEPT_NEW="crates/apply-patch/tests/label.rs"   # 新機能のテスト（コミット c で追加。変更ファイルに現れてよい）

# 1. 変更が 7 ファイルと新機能のテストに限られ、各ファイルのインライン・テスト部（#[cfg(test)] 以降）が無改変。
allowed=$(printf '%s\n' $FILES $KEPT_NEW | paste -sd'|')
extra=$(changed_files | grep -vE "^($allowed)\$" || true)
[ -z "$extra" ] || fail "範囲外の変更: $extra"
tests_of() { sed -n '/#\[cfg(test)\]/,$p'; }
for f in $FILES; do
  diff <(git show "$BASE:$f" | tests_of) <(tests_of <"$f") >/dev/null || fail "$f のテスト部が変更されている"
done
ok "変更範囲とテストの無改変"

# 2. 3 crate のテストが全部通る（失敗した binary で止まらないよう --no-fail-fast）。event-bus は feature 付き。
out=$(cargo test -p apply-patch -p agents -p event-bus --features event-bus/otel-exporter --offline --no-fail-fast 2>&1) || { echo "$out" | tail -30 >&2; fail "cargo test -p apply-patch -p agents -p event-bus が失敗"; }
ok "cargo test passed=$(echo "$out" | sum_passed)"

# 3. 新機能が残っている（隠しテスト。検証後に必ず削除する）。
hidden=crates/apply-patch/tests/zz_eval_hidden.rs
trap 'rm -f "$hidden"' EXIT
cp "$HERE/hidden/apply_patch_label_check.rs" "$hidden"
out=$(cargo test -p apply-patch --offline --test zz_eval_hidden 2>&1) || { echo "$out" | tail -25 >&2; fail "新機能 ChangeKind::label が残っていない（隠しテストが失敗）"; }
ok "新機能 ChangeKind::label"

# 4. 原因の報告: 7 ファイルが全部、1 ファイル 1 行以上で挙がっている。
[ -f answers/t6.md ] || fail "answers/t6.md がない"
for name in seek_sequence.rs text_file.rs targets.rs apply.rs ring.rs span_budget.rs role.rs; do
  grep -qF "$name" answers/t6.md || fail "answers/t6.md に $name の記述がない"
done
[ "$(grep -c . answers/t6.md)" -ge 7 ] || fail "answers/t6.md が 7 行未満"
ok "answers/t6.md"
