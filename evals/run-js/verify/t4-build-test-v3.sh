#!/usr/bin/env bash
# t4-build-test-v3: agents（権限変更・Role::ALL・roles_allowing）、apply-patch（ChangeCounts）、event-bus（error_type）の 3 作業。
source "$(dirname "$0")/lib/common.sh"

BASE_PASSED_AGENTS=28 BASE_PASSED_PATCH=94 BASE_PASSED_BUS=175   # 固定コミットでの各 crate のテスト合計（event-bus は --features otel-exporter）
BASE_EXISTING_AGENTS_TESTS=28   # 固定コミットの agents/tests/*.rs の #[test]（capability_boundary 22 + additional_roles 6）
BASE_BUS_MOD_TESTS=$(git show "$BASE:crates/event-bus/src/otel/mod.rs" | grep -cE '^\s*#\[test\]' || true)
BASE_IGNORES=$({ git grep -c '#\[ignore' "$BASE" -- crates/agents crates/apply-patch crates/event-bus || true; } | awk -F: '{s += $NF} END {print s + 0}')

# 1. 変更範囲: 3 crate の src と tests だけ（answers/ は追跡外）。新規テストファイルと件数。
outside=$(changed_files | grep -vE '^crates/(agents|apply-patch|event-bus)/(src|tests)/' || true)
[ -z "$outside" ] || fail "範囲外の変更: $outside"
count_tests() { { grep -cE '^\s*#\[test\]' "$1" 2>/dev/null || true; } | head -n1; }
for spec in "crates/agents/tests/role_all.rs 3" "crates/apply-patch/tests/change_counts.rs 3" "crates/event-bus/tests/failure_error_type.rs 2"; do
  set -- $spec
  [ -f "$1" ] || fail "$1 がない"
  n=$(count_tests "$1")
  [ "$n" -ge "$2" ] || fail "$1 のテストが $n 件（$2 件以上必要）"
done
ok "変更範囲と新規テストファイル"

# 2. 既存テストを減らしていない・無効化していない。map_failure は error_type に置き換えられている。
n_existing=$(cat crates/agents/tests/capability_boundary.rs crates/agents/tests/additional_roles.rs | grep -cE '^\s*#\[test\]' || true)
[ "$n_existing" -ge "$BASE_EXISTING_AGENTS_TESTS" ] || fail "agents の既存テストが減っている（$n_existing 件、元は $BASE_EXISTING_AGENTS_TESTS 件）"
n_bus_mod=$(grep -cE '^\s*#\[test\]' crates/event-bus/src/otel/mod.rs || true)
[ "$n_bus_mod" -ge "$BASE_BUS_MOD_TESTS" ] || fail "event-bus の otel/mod.rs のテストが減っている（$n_bus_mod 件、元は $BASE_BUS_MOD_TESTS 件）"
n_ignore=$({ grep -rE '#\[ignore' crates/agents crates/apply-patch crates/event-bus --include=*.rs || true; } | wc -l | tr -d ' ')
[ "$n_ignore" -le "$BASE_IGNORES" ] || fail "テストに #[ignore] が増えている（$n_ignore 件、元は $BASE_IGNORES 件）"
if grep -qE 'fn map_failure\(' crates/event-bus/src/otel/mod.rs; then fail "event-bus の otel/mod.rs に map_failure が残っている（ProviderFailureKind::error_type に置き換える）"; fi
ok "既存テスト（agents $n_existing 件、otel/mod.rs $n_bus_mod 件）と map_failure の置き換え"

# 3. role.rs の doc コメントの表が新しい権限に追従している。
grep -E '^/// \| Explorer \|' crates/agents/src/role.rs | grep -q 'git_diff' || fail "role.rs の doc 表の Explorer に git_diff がない"
grep -E '^/// \| Explorer \|' crates/agents/src/role.rs | grep -q 'skill_load' || fail "role.rs の doc 表の Explorer に skill_load がない"
ok "role.rs の doc 表"

# 4. fmt / clippy（3 crate。event-bus は feature 付き）。
cargo fmt -p agents -p apply-patch -p event-bus -- --check >/dev/null 2>&1 || fail "cargo fmt -p agents -p apply-patch -p event-bus -- --check が失敗"
ok "cargo fmt"
out=$(cargo clippy -p agents -p apply-patch -p event-bus --all-targets --features event-bus/otel-exporter --offline -- -D warnings 2>&1) || { echo "$out" | tail -30 >&2; fail "cargo clippy が失敗"; }
ok "cargo clippy"

# 5. テストを crate ごとに実行し、報告の final_passed と一致する。
run_tests() {
  local out
  out=$(cargo test "$@" --offline 2>&1) || { echo "$out" | tail -30 >&2; fail "cargo test $* が失敗"; }
  echo "$out" | sum_passed
}
final_agents=$(run_tests -p agents) || exit 1
final_patch=$(run_tests -p apply-patch) || exit 1
final_bus=$(run_tests -p event-bus --features otel-exporter) || exit 1
[ "$final_agents" -ge $((BASE_PASSED_AGENTS + 3)) ] || fail "agents の passed 合計 $final_agents が元 + 新規 3 を下回る"
[ "$final_patch" -ge $((BASE_PASSED_PATCH + 3)) ] || fail "apply-patch の passed 合計 $final_patch が元 + 新規 3 を下回る"
[ "$final_bus" -ge $((BASE_PASSED_BUS + 2)) ] || fail "event-bus の passed 合計 $final_bus が元 + 新規 2 を下回る"
ok "cargo test passed agents=$final_agents apply-patch=$final_patch event-bus=$final_bus"

# 6. 報告 JSON。
[ -f answers/t4.json ] || fail "answers/t4.json がない"
node -e '
const [file, a, p, b] = process.argv.slice(1);
const r = JSON.parse(require("fs").readFileSync(file, "utf8"));
const bad = [];
const same = (x, y) => JSON.stringify(x) === JSON.stringify(y);
const want = { baseline_passed: { agents: 28, "apply-patch": 94, "event-bus": 175 }, final_passed: { agents: +a, "apply-patch": +p, "event-bus": +b } };
if (!same(r.baseline_passed, want.baseline_passed)) bad.push(`baseline_passed=${JSON.stringify(r.baseline_passed)}`);
if (!same(r.final_passed, want.final_passed)) bad.push(`final_passed=${JSON.stringify(r.final_passed)}（期待 ${JSON.stringify(want.final_passed)}）`);
if (r.all_len !== 9) bad.push(`all_len=${r.all_len}`);
if (!same(r.skill_load_roles, ["Orchestrator", "Explorer", "Worker", "Planner"])) bad.push(`skill_load_roles=${JSON.stringify(r.skill_load_roles)}`);
if (!same(r.git_diff_roles, ["Orchestrator", "Explorer", "Worker", "Reviewer", "Planner", "Advisor"])) bad.push(`git_diff_roles=${JSON.stringify(r.git_diff_roles)}`);
if (!same(r.error_types, ["rate_limited", "http", "timeout", "invalid_response", "transport", "server", "quota", "auth", "other"])) bad.push(`error_types=${JSON.stringify(r.error_types)}`);
if (bad.length) { console.error("FAIL: " + bad.join(", ")); process.exit(1); }
' answers/t4.json "$final_agents" "$final_patch" "$final_bus"
ok "answers/t4.json"

# 7. 隠しテスト（検証後に必ず削除する）。
h_agents=crates/agents/tests/zz_eval_hidden.rs
h_patch=crates/apply-patch/tests/zz_eval_hidden.rs
h_bus=crates/event-bus/tests/zz_eval_hidden.rs
trap 'rm -f "$h_agents" "$h_patch" "$h_bus"' EXIT
cp "$HERE/hidden/role_all_v2_check.rs" "$h_agents"
cp "$HERE/hidden/apply_patch_counts_check.rs" "$h_patch"
cp "$HERE/hidden/event_bus_error_type_check.rs" "$h_bus"
for spec in "agents" "apply-patch" "event-bus --features otel-exporter"; do
  # shellcheck disable=SC2086
  out=$(cargo test -p $spec --offline --test zz_eval_hidden 2>&1) || { echo "$out" | tail -25 >&2; fail "隠しテスト（$spec）が失敗"; }
done
ok "隠しテスト（3 crate）"
