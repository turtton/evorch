//! ADR 0002 ケイパビリティ境界の完全マトリックステスト。
//!
//! v0.1 の 4 ロール (Orchestrator / Explorer / Worker / Reviewer) について
//! 許可ツール集合・拒否ツール・ネットワーク要件・委譲可否を検証し、
//! v0.2 ロール (Librarian) がロール定義 ([`RoleCapabilities`]) の追加だけで
//! 境界チェックに乗ることを実証する。

use std::collections::BTreeSet;

use agents::{CapabilityDecision, NetworkAccess, Role, RoleCapabilities};

/// ADR 0002 が定める Orchestrator の許可ツール集合 (期待値)。
const ORCHESTRATOR_TOOLS: &[&str] = &[
    "delegate",
    "delegate_background",
    "send_message",
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
];

/// ADR 0002 が定める Explorer の許可ツール集合 (期待値)。
const EXPLORER_TOOLS: &[&str] = &["read", "grep"];

/// ADR 0002 が定める Worker の許可ツール集合 (期待値)。
const WORKER_TOOLS: &[&str] = &[
    "read",
    "edit",
    "grep",
    "shell",
    "git_diff",
    "send",
    "wait_reply",
    "inbox",
];

/// ADR 0002 が定める Reviewer の許可ツール集合 (期待値、詳細はワークスペース決定)。
const REVIEWER_TOOLS: &[&str] = &["read", "grep", "git_diff"];

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
    // Given: v0.1 の 4 ロール
    // When: name() を呼び出す
    // Then: ADR 0002 のロール名識別子が返る
    assert_eq!(Role::Orchestrator.name(), "Orchestrator");
    assert_eq!(Role::Explorer.name(), "Explorer");
    assert_eq!(Role::Worker.name(), "Worker");
    assert_eq!(Role::Reviewer.name(), "Reviewer");
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
fn network_access_defaults_match_adr_matrix() {
    // Given: v0.1 の 4 ロール
    // When: 各ロールのネットワーク要件を参照する
    // Then: Orchestrator / Worker / Reviewer は Denied (ADR 0008 default-deny)、
    //       Explorer は OptIn になる
    assert_eq!(
        Role::Orchestrator.capabilities().network,
        NetworkAccess::Denied
    );
    assert_eq!(Role::Explorer.capabilities().network, NetworkAccess::OptIn);
    assert_eq!(Role::Worker.capabilities().network, NetworkAccess::Denied);
    assert_eq!(Role::Reviewer.capabilities().network, NetworkAccess::Denied);
}

#[test]
fn only_orchestrator_can_delegate() {
    // Given: v0.1 の 4 ロール
    // When: 各ロールの委譲可否を参照する
    // Then: Orchestrator のみ true になる (ADR 0002)
    assert!(Role::Orchestrator.capabilities().can_delegate);
    assert!(!Role::Explorer.capabilities().can_delegate);
    assert!(!Role::Worker.capabilities().can_delegate);
    assert!(!Role::Reviewer.capabilities().can_delegate);
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
fn librarian_role_requires_only_a_capability_definition() {
    // Given: v0.2 で追加予定の Librarian ロール定義 (ADR 0002: read / search / network allowed、
    //        write / edit / delegate denied) を RoleCapabilities として直接構築する
    let librarian = RoleCapabilities::new(["read", "grep"], NetworkAccess::Allowed, false);
    // When: チェッカーに read / edit を問い合わせ、ネットワーク要件を参照する
    // Then: read は Allowed、edit は Denied、network は Allowed になる —
    //       Role enum に手を入れずロール定義の追加だけで境界が機能する (v0.2 拡張レシピ)
    assert_eq!(
        librarian.check_tool("Librarian", "read"),
        CapabilityDecision::Allowed
    );
    assert_denied(
        librarian.check_tool("Librarian", "edit"),
        "Librarian",
        "edit",
    );
    assert_eq!(librarian.network, NetworkAccess::Allowed);
    assert!(!librarian.can_delegate);
}
