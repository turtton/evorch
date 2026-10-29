#!/usr/bin/env bash
# 参照解。WRONG=<kind> を環境変数で与えると、部分誤答の版を作る（solutions/*.wrong-*.sh が呼ぶ）。
#   WRONG=delete-tests  壊れた既存テストを消して通す
#   WRONG=sorted-roles  roles_allowing が宣言順ではなく名前順
#   WRONG=no-grant      Explorer への付与をしない（ALL / roles_allowing とテストだけ）
set -euo pipefail
cd "${1:?usage}"
WRONG=${WRONG:-}
baseline=$(cargo test -p agents --offline 2>&1 | awk '/^test result: ok\./ { s += $4 } END { print s + 0 }')

python3 - "$WRONG" <<'PY'
import sys
wrong = sys.argv[1]
p = "crates/agents/src/role.rs"
s = open(p).read()
if wrong != "no-grant":
    s = s.replace('Role::Explorer => RoleCapabilities::new(["read", "grep"], false),',
                  'Role::Explorer => RoleCapabilities::new(["read", "grep", "git_diff", "skill_load"], false),')
    s = s.replace('/// | Explorer | read / grep | 不可 |', '/// | Explorer | read / grep / git_diff / skill_load | 不可 |')
body = '''    /// 全ロール。enum の宣言順。
    pub const ALL: [Role; 9] = [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
        Role::WebResearcher,
        Role::Planner,
        Role::Advisor,
        Role::MultimodalLooker,
        Role::Evaluator,
    ];

    /// `tool` を許可しているロール。`Role::ALL` の順（enum の宣言順）で返す。
    pub fn roles_allowing(tool: &str) -> Vec<Role> {
        Self::ALL
            .into_iter()
            .filter(|role| role.capabilities().allowed_tools.contains(tool))
            .collect()
    }

'''
if wrong == "sorted-roles":
    body = body.replace("        Self::ALL\n", "        let mut roles: Vec<Role> = Self::ALL\n").replace("            .collect()\n", "            .collect();\n        roles.sort_by_key(|role| role.name());\n        roles\n")
anchor = "impl Role {\n"
assert anchor in s
s = s.replace(anchor, anchor + body, 1)
open(p, "w").write(s)

if wrong != "no-grant":
    p = "crates/agents/tests/support/capability_tools.rs"
    s = open(p).read()
    old = 'pub const EXPLORER_TOOLS: &[&str] = &[\n    "read",\n    "grep",\n'
    assert old in s
    s = s.replace(old, old + '    "git_diff",\n    "skill_load",\n')
    open(p, "w").write(s)
    p = "crates/agents/tests/capability_boundary.rs"
    s = open(p).read()
    start = s.index("#[test]\nfn explorer_denies_skill_load()")
    end = s.index("\n}\n", start) + 3
    if wrong == "delete-tests":
        s = s[:start] + s[end:].lstrip("\n")
    else:
        new = '''#[test]
fn explorer_allows_skill_load() {
    // Given: Explorer ロール (git_diff / skill_load を許可された調査役)
    // When: skill_load の使用可否を問い合わせる
    // Then: Allowed になる
    let caps = Role::Explorer.capabilities();

    assert_eq!(
        caps.check_tool("Explorer", "skill_load"),
        CapabilityDecision::Allowed
    );
}
'''
        s = s[:start] + new + s[end:]
    open(p, "w").write(s)
PY

cat > crates/agents/tests/role_all.rs <<'RS'
use agents::Role;

#[test]
fn all_has_nine_roles() {
    assert_eq!(Role::ALL.len(), 9);
}

#[test]
fn all_round_trips_through_names() {
    for role in Role::ALL {
        assert_eq!(Role::from_name(role.name()).unwrap(), role);
    }
}

#[test]
fn skill_load_is_allowed_for_four_roles_in_declaration_order() {
    assert_eq!(
        Role::roles_allowing("skill_load"),
        [Role::Orchestrator, Role::Explorer, Role::Worker, Role::Planner]
    );
}

#[test]
fn shell_is_worker_only_and_unknown_tools_have_no_roles() {
    assert_eq!(Role::roles_allowing("shell"), [Role::Worker]);
    assert!(Role::roles_allowing("no_such_tool").is_empty());
}
RS
cargo fmt -p agents
final=$({ cargo test -p agents --offline 2>&1 || true; } | awk '/^test result: ok\./ { s += $4 } END { print s + 0 }')
printf '{"baseline_passed": %s, "final_passed": %s, "all_len": 9, "skill_load_roles": ["Orchestrator", "Explorer", "Worker", "Planner"], "git_diff_roles": ["Orchestrator", "Explorer", "Worker", "Reviewer", "Planner", "Advisor"]}\n' \
  "$baseline" "$final" > answers/t4.json
