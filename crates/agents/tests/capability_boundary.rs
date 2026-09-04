//! ADR 0002 ケイパビリティ境界の完全マトリックステスト。
//!
//! v0.2 の 5 ロール (Orchestrator / Explorer / Worker / Reviewer / Librarian) について
//! 許可ツール集合・拒否ツール・ネットワーク要件・委譲可否を検証する。
//! Librarian は v0.2 で `Role` variant として追加され、
//! Orchestrator は web_fetch を持ちネットワークが OptIn になった
//! (ADR 0002 2026-09-03 補足)。

use std::collections::BTreeSet;

use agents::{CapabilityDecision, NetworkAccess, Role, RoleCapabilities};

/// ADR 0002 が定める Orchestrator の許可ツール集合 (期待値)。
const ORCHESTRATOR_TOOLS: &[&str] = &[
    "delegate",
    "delegate_background",
    "send_message",
    "skill_load",
    "send",
    "wait_reply",
    "inbox",
    "wait",
    "cancel",
    "list_agents",
    "inspect_agent",
    "read",
    "grep",
    "git_diff",
    "compact",
    "finish",
    "web_fetch",
];

/// ADR 0002 が定める Explorer の許可ツール集合 (期待値)。
const EXPLORER_TOOLS: &[&str] = &["read", "grep"];

/// ADR 0002 が定める Worker の許可ツール集合 (期待値)。
const WORKER_TOOLS: &[&str] = &[
    "read",
    "edit",
    "grep",
    "shell",
    "skill_load",
    "git_diff",
    "send",
    "wait_reply",
    "inbox",
];

/// ADR 0002 が定める Reviewer の許可ツール集合 (期待値、詳細はワークスペース決定)。
const REVIEWER_TOOLS: &[&str] = &["read", "grep", "git_diff"];

/// ADR 0002 (2026-09-03 補足) が定める Librarian の許可ツール集合 (期待値)。
const LIBRARIAN_TOOLS: &[&str] = &["read", "grep", "web_search", "web_fetch"];

/// Denied 判定の機械消費フィールド (role_name / tool) と拒否理由の存在を検証する。
fn assert_denied(decision: CapabilityDecision, expected_role: &str, expected_tool: &str) {
    match decision {
        CapabilityDecision::Allowed => {
            panic!(
                "Denied を期待しましたが Allowed が返りました: {expected_role} / {expected_tool}"
            );
        }
        CapabilityDecision::Denied {
            role_name,
            tool,
            reason,
        } => {
            assert_eq!(role_name, expected_role, "role_name が一致しません");
            assert_eq!(tool, expected_tool, "tool が一致しません");
            assert!(!reason.is_empty(), "拒否理由が空です");
        }
    }
}

/// 期待値のツール集合を BTreeSet<String> として構築する。
fn tool_set(tools: &[&str]) -> BTreeSet<String> {
    tools.iter().map(|&tool| tool.to_string()).collect()
}

#[test]
fn role_names_are_stable_identifiers() {
    // Given: v0.2 の 5 ロール
    // When: name() を呼び出す
    // Then: ADR 0002 のロール名識別子が返る
    assert_eq!(Role::Orchestrator.name(), "Orchestrator");
    assert_eq!(Role::Explorer.name(), "Explorer");
    assert_eq!(Role::Worker.name(), "Worker");
    assert_eq!(Role::Reviewer.name(), "Reviewer");
    assert_eq!(Role::Librarian.name(), "Librarian");
}

#[test]
fn orchestrator_allows_exactly_adr_0002_tools() {
    // Given: Orchestrator ロール (委譲と調査のみを担う)
    // When: ケイパビリティのツール集合と全ツールの判定を検査する
    // Then: ADR 0002 のツール集合と完全一致し、全ツールが Allowed になる
    let caps = Role::Orchestrator.capabilities();
    assert_eq!(caps.allowed_tools, tool_set(ORCHESTRATOR_TOOLS));
    for &tool in ORCHESTRATOR_TOOLS {
        assert_eq!(
            caps.check_tool(Role::Orchestrator.name(), tool),
            CapabilityDecision::Allowed
        );
    }
}

#[test]
fn orchestrator_denies_mutation_tools() {
    // Given: Orchestrator ロール (ADR 0002: mutation tool を持たせない)
    // When: edit / shell の使用可否を問い合わせる
    // Then: 両方とも role_name / tool 入りの Denied になる
    let caps = Role::Orchestrator.capabilities();
    for tool in ["edit", "shell"] {
        assert_denied(caps.check_tool("Orchestrator", tool), "Orchestrator", tool);
    }
}

#[test]
fn orchestrator_denies_web_search_but_allows_web_fetch() {
    // Given: Orchestrator ロール (ADR 0002 2026-09-03 補足: web_fetch のみ持ち、
    //        web_search は Librarian 専用、ネットワークは OptIn)
    // When: web_search / web_fetch の使用可否を問い合わせる
    // Then: web_search は Denied、web_fetch は Allowed になる
    let caps = Role::Orchestrator.capabilities();
    assert_denied(
        caps.check_tool("Orchestrator", "web_search"),
        "Orchestrator",
        "web_search",
    );
    assert_eq!(
        caps.check_tool("Orchestrator", "web_fetch"),
        CapabilityDecision::Allowed
    );
}

#[test]
fn explorer_allows_exactly_adr_0002_tools() {
    // Given: Explorer ロール (読み取り専用の調査役)
    // When: ケイパビリティのツール集合と全ツールの判定を検査する
    // Then: ADR 0002 のツール集合 (read / grep) と完全一致し、全ツールが Allowed になる
    let caps = Role::Explorer.capabilities();
    assert_eq!(caps.allowed_tools, tool_set(EXPLORER_TOOLS));
    for &tool in EXPLORER_TOOLS {
        assert_eq!(
            caps.check_tool(Role::Explorer.name(), tool),
            CapabilityDecision::Allowed
        );
    }
}

#[test]
fn explorer_denies_mutation_delegation_and_messaging_tools() {
    // Given: Explorer ロール (read / search のみ、write / edit / delegate / messaging は拒否)
    // When: edit / shell / delegate_background / send / wait_reply / inbox の使用可否を問い合わせる
    // Then: すべて Denied になる
    let caps = Role::Explorer.capabilities();
    for tool in [
        "edit",
        "shell",
        "delegate_background",
        "send",
        "wait_reply",
        "inbox",
    ] {
        assert_denied(caps.check_tool("Explorer", tool), "Explorer", tool);
    }
}

#[test]
fn explorer_denies_skill_load() {
    // Given: Explorer ロール
    // When: skill_load の使用可否を問い合わせる
    // Then: Denied になる
    let caps = Role::Explorer.capabilities();

    assert_denied(
        caps.check_tool("Explorer", "skill_load"),
        "Explorer",
        "skill_load",
    );
}

#[test]
fn worker_allows_exactly_adr_0002_tools() {
    // Given: Worker ロール (ワークスペース read-write の実装役)
    // When: ケイパビリティのツール集合と全ツールの判定を検査する
    // Then: ADR 0002 のツール集合と完全一致し、全ツールが Allowed になる
    let caps = Role::Worker.capabilities();
    assert_eq!(caps.allowed_tools, tool_set(WORKER_TOOLS));
    for &tool in WORKER_TOOLS {
        assert_eq!(
            caps.check_tool(Role::Worker.name(), tool),
            CapabilityDecision::Allowed
        );
    }
}

#[test]
fn worker_allows_mutation_tools() {
    // Given: Worker ロール (mutation を担う唯一のロール)
    // When: edit / shell の使用可否を問い合わせる
    // Then: 両方とも Allowed になる
    let caps = Role::Worker.capabilities();
    for tool in ["edit", "shell"] {
        assert_eq!(caps.check_tool("Worker", tool), CapabilityDecision::Allowed);
    }
}

#[test]
fn worker_denies_delegation_tools() {
    // Given: Worker ロール (委譲は Orchestrator のみが持つ)
    // When: delegate_background の使用可否を問い合わせる
    // Then: Denied になる
    let caps = Role::Worker.capabilities();
    assert_denied(
        caps.check_tool("Worker", "delegate_background"),
        "Worker",
        "delegate_background",
    );
}

#[test]
fn reviewer_allows_exactly_adr_0002_tools() {
    // Given: Reviewer ロール (生成と独立したレビュー役)
    // When: ケイパビリティのツール集合と全ツールの判定を検査する
    // Then: ツール集合 (read / grep / git_diff) と完全一致し、全ツールが Allowed になる
    let caps = Role::Reviewer.capabilities();
    assert_eq!(caps.allowed_tools, tool_set(REVIEWER_TOOLS));
    for &tool in REVIEWER_TOOLS {
        assert_eq!(
            caps.check_tool(Role::Reviewer.name(), tool),
            CapabilityDecision::Allowed
        );
    }
}

#[test]
fn reviewer_allows_git_diff() {
    // Given: Reviewer ロール (差分を読んでレビューする)
    // When: git_diff の使用可否を問い合わせる
    // Then: Allowed になる
    let caps = Role::Reviewer.capabilities();
    assert_eq!(
        caps.check_tool("Reviewer", "git_diff"),
        CapabilityDecision::Allowed
    );
}

#[test]
fn reviewer_denies_mutation_and_messaging_tools() {
    // Given: Reviewer ロール (レビュー対象を自分で書き換えず、メッセージ交換もしない)
    // When: edit / send / wait_reply / inbox の使用可否を問い合わせる
    // Then: すべて Denied になる
    let caps = Role::Reviewer.capabilities();
    for tool in ["edit", "send", "wait_reply", "inbox"] {
        assert_denied(caps.check_tool("Reviewer", tool), "Reviewer", tool);
    }
}

#[test]
fn reviewer_denies_skill_load() {
    // Given: Reviewer ロール
    // When: skill_load の使用可否を問い合わせる
    // Then: Denied になる
    let caps = Role::Reviewer.capabilities();

    assert_denied(
        caps.check_tool("Reviewer", "skill_load"),
        "Reviewer",
        "skill_load",
    );
}

#[test]
fn librarian_allows_exactly_adr_0002_tools() {
    // Given: Librarian ロール (v0.2 の調査役、web_search / web_fetch を持つ)
    // When: ケイパビリティのツール集合と全ツールの判定を検査する
    // Then: ADR 0002 (2026-09-03 補足) のツール集合と完全一致し、全ツールが Allowed になる
    let caps = Role::Librarian.capabilities();
    assert_eq!(caps.allowed_tools, tool_set(LIBRARIAN_TOOLS));
    for &tool in LIBRARIAN_TOOLS {
        assert_eq!(
            caps.check_tool(Role::Librarian.name(), tool),
            CapabilityDecision::Allowed
        );
    }
}

#[test]
fn librarian_denies_mutation_and_delegation_tools() {
    // Given: Librarian ロール (read / grep と web_search / web_fetch のみ、
    //        mutation / 委譲 / messaging は拒否)
    // When: edit / shell / delegate_background / send の使用可否を問い合わせる
    // Then: すべて Denied になる
    let caps = Role::Librarian.capabilities();
    for tool in ["edit", "shell", "delegate_background", "send"] {
        assert_denied(caps.check_tool("Librarian", tool), "Librarian", tool);
    }
}

#[test]
fn network_access_defaults_match_adr_matrix() {
    // Given: v0.2 の 5 ロール
    // When: 各ロールのネットワーク要件を参照する
    // Then: Worker / Reviewer は Denied (ADR 0008 default-deny)、
    //       Explorer / Orchestrator は OptIn、Librarian は Allowed になる
    //       (Orchestrator の OptIn は web_fetch のみを対象とする ADR 0002 2026-09-03 補足)
    assert_eq!(
        Role::Orchestrator.capabilities().network,
        NetworkAccess::OptIn
    );
    assert_eq!(Role::Explorer.capabilities().network, NetworkAccess::OptIn);
    assert_eq!(Role::Worker.capabilities().network, NetworkAccess::Denied);
    assert_eq!(Role::Reviewer.capabilities().network, NetworkAccess::Denied);
    assert_eq!(
        Role::Librarian.capabilities().network,
        NetworkAccess::Allowed
    );
}

#[test]
fn only_orchestrator_can_delegate() {
    // Given: v0.2 の 5 ロール
    // When: 各ロールの委譲可否を参照する
    // Then: Orchestrator のみ true になる (ADR 0002)
    assert!(Role::Orchestrator.capabilities().can_delegate);
    assert!(!Role::Explorer.capabilities().can_delegate);
    assert!(!Role::Worker.capabilities().can_delegate);
    assert!(!Role::Reviewer.capabilities().can_delegate);
    assert!(!Role::Librarian.capabilities().can_delegate);
}

#[test]
fn role_capabilities_new_collects_tools_into_a_btreeset() {
    // Given: &str ツール名のイテレータとネットワーク要件
    // When: RoleCapabilities::new でケイパビリティを構築する
    // Then: ツールは BTreeSet<String> に収集され、network / can_delegate が反映される
    let caps = RoleCapabilities::new(["read", "grep"], NetworkAccess::OptIn, false);
    assert_eq!(caps.allowed_tools, tool_set(&["read", "grep"]));
    assert_eq!(caps.network, NetworkAccess::OptIn);
    assert!(!caps.can_delegate);
}

#[test]
fn librarian_role_is_defined_via_capabilities_only() {
    // Given: v0.2 の Librarian ロール (ADR 0002 2026-09-03 補足: read / grep と
    //        web_search / web_fetch を持ち、network は Allowed、委譲は不可)
    // When: Role::Librarian のケイパビリティを参照する
    // Then: 境界チェックは RoleCapabilities 経由で機能し、network は Allowed、
    //       委譲は不可になる (ランタイムの強制は RoleCapabilities のみを消費する)
    let caps = Role::Librarian.capabilities();
    assert_eq!(
        caps.check_tool("Librarian", "read"),
        CapabilityDecision::Allowed
    );
    assert_denied(caps.check_tool("Librarian", "edit"), "Librarian", "edit");
    assert_eq!(caps.network, NetworkAccess::Allowed);
    assert!(!caps.can_delegate);
}
