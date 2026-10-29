#!/usr/bin/env bash
# t4-build-test-v2: Explorer の権限変更、既存テストの更新、Role::ALL / roles_allowing、fmt / clippy / test、報告 JSON。
source "$(dirname "$0")/lib/common.sh"

BASELINE_PASSED=28   # 固定コミットでの `cargo test -p agents` の passed 合計
BASE_EXISTING_TESTS=28   # 固定コミットの tests/*.rs にある #[test] の数（capability_boundary 22 + additional_roles 6）

# 1. 変更範囲: crates/agents の src と tests だけ。新規テストは role_all.rs に 3 件以上。
outside=$(changed_files | grep -vE '^crates/agents/(src|tests)/' || true)
[ -z "$outside" ] || fail "範囲外の変更: $outside"
[ -f crates/agents/tests/role_all.rs ] || fail "crates/agents/tests/role_all.rs がない"
n_new_tests=$(grep -cE '^\s*#\[test\]' crates/agents/tests/role_all.rs || true)
[ "$n_new_tests" -ge 3 ] || fail "role_all.rs のテストが $n_new_tests 件（3 件以上必要）"
ok "変更範囲と新規テスト ${n_new_tests} 件"

# 2. 既存テストを減らしていない・無効化していない（壊れたテストを消して通すのは不合格）。
n_existing=$(cat crates/agents/tests/capability_boundary.rs crates/agents/tests/additional_roles.rs | grep -cE '^\s*#\[test\]' || true)
[ "$n_existing" -ge "$BASE_EXISTING_TESTS" ] || fail "既存テストが減っている（$n_existing 件、元は $BASE_EXISTING_TESTS 件）"
if grep -rnE '#\[ignore' crates/agents/tests/ >/dev/null; then fail "テストに #[ignore] が付いている"; fi
ok "既存テスト $n_existing 件（減っていない）"

# 3. role.rs の doc コメントの表が新しい権限に追従している。
grep -E '^/// \| Explorer \|' crates/agents/src/role.rs | grep -q 'git_diff' || fail "role.rs の doc 表の Explorer に git_diff がない"
grep -E '^/// \| Explorer \|' crates/agents/src/role.rs | grep -q 'skill_load' || fail "role.rs の doc 表の Explorer に skill_load がない"
ok "role.rs の doc 表"

# 4. fmt / clippy。
cargo fmt -p agents -- --check >/dev/null 2>&1 || fail "cargo fmt -p agents -- --check が失敗"
ok "cargo fmt"
out=$(cargo clippy -p agents --all-targets --offline -- -D warnings 2>&1) || { echo "$out" | tail -25 >&2; fail "cargo clippy が失敗"; }
ok "cargo clippy"

# 5. テストを実行し、報告の final_passed と一致する。
out=$(cargo test -p agents --offline 2>&1) || { echo "$out" | tail -25 >&2; fail "cargo test -p agents が失敗"; }
final=$(echo "$out" | sum_passed)
[ "$final" -ge $((BASELINE_PASSED + n_new_tests)) ] || fail "passed 合計 $final が baseline+新規 を下回る"
ok "cargo test passed=$final"

# 6. 報告 JSON。
[ -f answers/t4.json ] || fail "answers/t4.json がない"
node -e '
const [file, baseline, final] = process.argv.slice(1);
const a = JSON.parse(require("fs").readFileSync(file, "utf8"));
const bad = [];
if (a.baseline_passed !== Number(baseline)) bad.push(`baseline_passed=${a.baseline_passed}（期待 ${baseline}）`);
if (a.final_passed !== Number(final)) bad.push(`final_passed=${a.final_passed}（期待 ${final}）`);
if (a.all_len !== 9) bad.push(`all_len=${a.all_len}（期待 9）`);
const same = (x, y) => JSON.stringify(x) === JSON.stringify(y);
if (!same(a.skill_load_roles, ["Orchestrator", "Explorer", "Worker", "Planner"])) bad.push(`skill_load_roles=${JSON.stringify(a.skill_load_roles)}`);
if (!same(a.git_diff_roles, ["Orchestrator", "Explorer", "Worker", "Reviewer", "Planner", "Advisor"])) bad.push(`git_diff_roles=${JSON.stringify(a.git_diff_roles)}`);
if (bad.length) { console.error("FAIL: " + bad.join(", ")); process.exit(1); }
' answers/t4.json "$BASELINE_PASSED" "$final"
ok "answers/t4.json"

# 7. 隠しテスト（検証後に必ず削除する）。
hidden=crates/agents/tests/zz_eval_hidden.rs
trap 'rm -f "$hidden"' EXIT
cp "$HERE/hidden/role_all_v2_check.rs" "$hidden"
out=$(cargo test -p agents --offline --test zz_eval_hidden 2>&1) || { echo "$out" | tail -25 >&2; fail "隠しテスト（Role::ALL の順序、Explorer の権限、roles_allowing）が失敗"; }
ok "隠しテスト"
