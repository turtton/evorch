#!/usr/bin/env bash
# t4-build-test: Role::ALL の追加、fmt / clippy / test の通過、報告 JSON の数値を確かめる。
source "$(dirname "$0")/lib/common.sh"

BASELINE_PASSED=28   # 固定コミットでの `cargo test -p agents` の passed 合計

# 1. 変更範囲: crates/agents/src の変更と、新規の crates/agents/tests/role_all.rs だけ。
outside=$(changed_files | grep -v '^crates/agents/src/' || true)
[ -z "$outside" ] || fail "範囲外の変更: $outside"
[ -f crates/agents/tests/role_all.rs ] || fail "crates/agents/tests/role_all.rs がない"
n_new_tests=$(grep -cE '^\s*#\[test\]' crates/agents/tests/role_all.rs || true)
[ "$n_new_tests" -ge 2 ] || fail "role_all.rs のテストが $n_new_tests 件（2 件以上必要）"
ok "変更範囲と新規テスト ${n_new_tests} 件"

# 2. fmt / clippy。
cargo fmt -p agents -- --check >/dev/null 2>&1 || fail "cargo fmt -p agents -- --check が失敗"
ok "cargo fmt"
cargo clippy -p agents --all-targets --offline -- -D warnings >/dev/null 2>&1 || fail "cargo clippy が失敗"
ok "cargo clippy"

# 3. テストを実行し、報告の final_passed と一致する。
out=$(cargo test -p agents --offline 2>&1) || { echo "$out" | tail -20 >&2; fail "cargo test -p agents が失敗"; }
final=$(echo "$out" | sum_passed)
[ "$final" -ge $((BASELINE_PASSED + n_new_tests)) ] || fail "passed 合計 $final が baseline+新規 を下回る"
ok "cargo test passed=$final"

# 4. 報告 JSON。
[ -f answers/t4.json ] || fail "answers/t4.json がない"
node -e '
const [file, baseline, final] = process.argv.slice(1);
const a = JSON.parse(require("fs").readFileSync(file, "utf8"));
const bad = [];
if (a.baseline_passed !== Number(baseline)) bad.push(`baseline_passed=${a.baseline_passed}（期待 ${baseline}）`);
if (a.final_passed !== Number(final)) bad.push(`final_passed=${a.final_passed}（期待 ${final}）`);
if (a.all_len !== 9) bad.push(`all_len=${a.all_len}（期待 9）`);
if (bad.length) { console.error("FAIL: " + bad.join(", ")); process.exit(1); }
' answers/t4.json "$BASELINE_PASSED" "$final"
ok "answers/t4.json"

# 5. 隠しテスト（検証後に必ず削除する）。
hidden=crates/agents/tests/zz_eval_hidden.rs
trap 'rm -f "$hidden"' EXIT
cp "$HERE/hidden/role_all_check.rs" "$hidden"
cargo test -p agents --offline --test zz_eval_hidden >/dev/null 2>&1 || fail "隠しテスト（Role::ALL の順序・往復・型）が失敗"
ok "隠しテスト"
