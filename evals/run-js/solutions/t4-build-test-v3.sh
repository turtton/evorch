#!/usr/bin/env bash
# 参照解（agents / apply-patch / event-bus の 3 つの作業）。WRONG=<kind> を環境変数で与えると部分誤答の版を作る
# （solutions/*.wrong-*.sh が呼ぶ）。
#   WRONG=delete-tests   agents の壊れた既存テストを消して通す
#   WRONG=sorted-roles   roles_allowing が宣言順ではなく名前順
#   WRONG=keep-map-failure  event-bus の map_failure を残す（error_type を足すだけ）
#   WRONG=label          ChangeKind::label の Moved を "renamed" にする
set -euo pipefail
cd "${1:?usage}"
WRONG=${WRONG:-}
passed() { { cargo test "$@" --offline 2>&1 || true; } | awk '/^test result: ok\./ { s += $4 } END { print s + 0 }'; }
b_agents=$(passed -p agents)
b_patch=$(passed -p apply-patch)
b_bus=$(passed -p event-bus --features otel-exporter)

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

# ---- apply-patch: ChangeKind::label / ChangeCounts / Applied::counts
python3 - "$WRONG" <<'PY'
import sys
wrong = sys.argv[1]
p = "crates/apply-patch/src/apply.rs"
s = open(p).read()
moved = "renamed" if wrong == "label" else "moved"
add = f'''impl ChangeKind {{
    /// 変更の種類を表す短い英語のラベル（`added` / `updated` / `moved` / `deleted`）。
    pub const fn label(&self) -> &'static str {{
        match self {{
            Self::Added {{ .. }} => "added",
            Self::Updated {{ .. }} => "updated",
            Self::Moved {{ .. }} => "{moved}",
            Self::Deleted {{ .. }} => "deleted",
        }}
    }}
}}

/// 変更の種類ごとの件数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {{
    pub added: usize,
    pub updated: usize,
    pub moved: usize,
    pub deleted: usize,
}}

impl ChangeCounts {{
    /// 4 種類の合計。
    pub const fn total(&self) -> usize {{
        self.added + self.updated + self.moved + self.deleted
    }}
}}

impl Applied {{
    /// `changes` を種類ごとに数える。
    pub fn counts(&self) -> ChangeCounts {{
        let mut counts = ChangeCounts::default();
        for change in &self.changes {{
            match change.change {{
                ChangeKind::Added {{ .. }} => counts.added += 1,
                ChangeKind::Updated {{ .. }} => counts.updated += 1,
                ChangeKind::Moved {{ .. }} => counts.moved += 1,
                ChangeKind::Deleted {{ .. }} => counts.deleted += 1,
            }}
        }}
        counts
    }}
}}

'''
anchor = "impl AppliedChange {\n"
assert anchor in s
s = s.replace(anchor, add + anchor, 1)
open(p, "w").write(s)
p = "crates/apply-patch/src/lib.rs"
s = open(p).read()
s = s.replace("Applied, AppliedChange, ApplyError, ChangeKind, CommitFailure, Plan, apply_patch, plan,", "Applied, AppliedChange, ApplyError, ChangeCounts, ChangeKind, CommitFailure, Plan, apply_patch, plan,")
open(p, "w").write(s)
PY
cat > crates/apply-patch/tests/change_counts.rs <<'RS'
use apply_patch::{ChangeCounts, ChangeKind, apply_patch};

#[test]
fn labels_cover_every_kind() {
    let kinds = [
        (ChangeKind::Added { content: String::new(), overwritten: None }, "added"),
        (ChangeKind::Updated { old: String::new(), new: String::new() }, "updated"),
        (ChangeKind::Deleted { old: String::new() }, "deleted"),
    ];
    for (kind, label) in kinds {
        assert_eq!(kind.label(), label);
    }
}

#[test]
fn counts_group_a_real_patch_by_kind() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("update.txt"), "a\nb\n").unwrap();
    std::fs::write(dir.path().join("delete.txt"), "gone\n").unwrap();
    std::fs::write(dir.path().join("from.txt"), "x\n").unwrap();
    let patch = "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** Update File: update.txt\n@@\n a\n-b\n+c\n*** Delete File: delete.txt\n*** Update File: from.txt\n*** Move to: to.txt\n@@\n-x\n+y\n*** End Patch";
    let applied = apply_patch(patch, Some(dir.path())).unwrap();
    let counts = applied.counts();
    assert_eq!(counts, ChangeCounts { added: 1, updated: 1, moved: 1, deleted: 1 });
    assert_eq!(counts.total(), 4);
    assert_eq!(applied.changes[0].change.label(), "added");
}

#[test]
fn default_counts_are_zero() {
    assert_eq!(ChangeCounts::default().total(), 0);
}
RS
cargo fmt -p apply-patch

# ---- event-bus: ProviderFailureKind::error_type と map_failure の置き換え
python3 - "$WRONG" <<'PY'
import sys
wrong = sys.argv[1]
p = "crates/event-bus/src/event.rs"
s = open(p).read()
anchor = "/// プロバイダ呼び出しの目的。usage 集計で本処理と付随コストを区別する。"
assert anchor in s
method = '''impl ProviderFailureKind {
    /// OpenTelemetry の `error.type` に載せる、低カーディナリティな値。`Http` の status は捨てる。
    pub const fn error_type(&self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Http { .. } => "http",
            Self::Timeout => "timeout",
            Self::InvalidResponse => "invalid_response",
            Self::Transport => "transport",
            Self::Server => "server",
            Self::Quota => "quota",
            Self::Auth => "auth",
            Self::Other => "other",
        }
    }
}

'''
s = s.replace(anchor, method + anchor, 1)
open(p, "w").write(s)
p = "crates/event-bus/src/otel/mod.rs"
s = open(p).read()
if wrong != "keep-map-failure":
    a = s.index("/// [`ProviderFailureKind`] を低カーディナリティな `error.type` 値へ正規化する。")
    b = s.index("/// attribute キーごとの値検査方式。")
    s = s[:a] + s[b:]
    s = s.replace("Some(map_failure(failure)),", "Some(failure.error_type()),")
    s = s.replace("assert_eq!(map_failure(&failure), expected,", "assert_eq!(failure.error_type(), expected,")
    # トップレベルでは使わなくなる import をテストモジュールへ移す（clippy の unused import 対策）
    s = s.replace("use crate::event::{Event, EventKind, ProviderEvent, ProviderFailureKind, UsageEvent};", "use crate::event::{Event, EventKind, ProviderEvent, UsageEvent};")
    s = s.replace("mod tests {\n    use super::*;\n", "mod tests {\n    use super::*;\n    use crate::event::ProviderFailureKind;\n", 1)
open(p, "w").write(s)
PY
cat > crates/event-bus/tests/failure_error_type.rs <<'RS'
use event_bus::event::ProviderFailureKind;

fn all() -> [ProviderFailureKind; 9] {
    [
        ProviderFailureKind::RateLimited,
        ProviderFailureKind::Http { status: 500 },
        ProviderFailureKind::Timeout,
        ProviderFailureKind::InvalidResponse,
        ProviderFailureKind::Transport,
        ProviderFailureKind::Server,
        ProviderFailureKind::Quota,
        ProviderFailureKind::Auth,
        ProviderFailureKind::Other,
    ]
}

#[test]
fn error_types_are_the_closed_domain_in_declaration_order() {
    let types: Vec<&str> = all().iter().map(ProviderFailureKind::error_type).collect();
    assert_eq!(
        types,
        ["rate_limited", "http", "timeout", "invalid_response", "transport", "server", "quota", "auth", "other"]
    );
}

#[test]
fn http_status_is_dropped_from_the_error_type() {
    assert_eq!(ProviderFailureKind::Http { status: 404 }.error_type(), ProviderFailureKind::Http { status: 503 }.error_type());
}
RS
cargo fmt -p event-bus

cargo fmt -p agents -p apply-patch -p event-bus
f_agents=$(passed -p agents)
f_patch=$(passed -p apply-patch)
f_bus=$(passed -p event-bus --features otel-exporter)
printf '{"baseline_passed": {"agents": %s, "apply-patch": %s, "event-bus": %s}, "final_passed": {"agents": %s, "apply-patch": %s, "event-bus": %s}, "all_len": 9, "skill_load_roles": ["Orchestrator", "Explorer", "Worker", "Planner"], "git_diff_roles": ["Orchestrator", "Explorer", "Worker", "Reviewer", "Planner", "Advisor"], "error_types": ["rate_limited", "http", "timeout", "invalid_response", "transport", "server", "quota", "auth", "other"]}\n' \
  "$b_agents" "$b_patch" "$b_bus" "$f_agents" "$f_patch" "$f_bus" > answers/t4.json
