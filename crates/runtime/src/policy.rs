//! ロールの capability boundary を実行ポリシーへ適用する (ADR 0002)。

use agents::{CapabilityDecision, Role, RoleCapabilities};
use providers::ToolSpec;

use crate::error::RuntimeError;

/// ランタイムが定義するメタ操作 (ツールではない run 制御操作) の正規名集合。
///
/// メタ操作の解決・dispatch は T5 が担う。ここでは名前の正規集合のみを定義する。
pub const META_OPS: &[&str] = &[
    "delegate",
    "delegate_background",
    "send_message",
    "skill_load",
    "wait",
    "cancel",
    "list_agents",
    "inspect_agent",
    "compact",
    "finish",
    "send",
    "wait_reply",
    "inbox",
    "escalate",
];

/// 名前がメタ操作かどうかを判定する。
pub fn is_meta_op(name: &str) -> bool {
    META_OPS.contains(&name)
}

/// ロールの capability boundary を実行時に強制するポリシー。
///
/// ADR 0002 の行列 ([`Role::capabilities`]) を消費し、ツール実行の可否判定と
/// モデルに見せるツール定義のフィルタリングを行う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPolicy {
    /// 強制対象のケイパビリティ集合。
    pub capabilities: RoleCapabilities,
    /// 判定結果に載せるロール名。
    pub role_name: String,
}

impl ExecutionPolicy {
    /// ロールからポリシーを構築する。
    pub fn for_role(role: Role) -> Self {
        Self {
            capabilities: role.capabilities(),
            role_name: role.name().to_owned(),
        }
    }

    /// ツール使用を認可する。
    ///
    /// # Errors
    /// capability boundary の外のツールは [`RuntimeError::CapabilityDenied`] を返す。
    pub fn authorize(&self, tool: &str) -> Result<(), RuntimeError> {
        match self.capabilities.check_tool(&self.role_name, tool) {
            CapabilityDecision::Allowed => Ok(()),
            CapabilityDecision::Denied {
                role_name,
                tool,
                reason,
            } => Err(RuntimeError::CapabilityDenied {
                role: role_name,
                tool,
                reason,
            }),
        }
    }

    /// モデルに見せるツール定義を境界内のものだけにフィルタする。
    ///
    /// モデルは許可されたツールのみを見る (model only sees allowed tools)。
    /// フィルタは入力の順序を保存する。
    pub fn filter_tool_specs(&self, specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
        specs
            .into_iter()
            .filter(|spec| self.capabilities.allowed_tools.contains(&spec.name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: format!("{name} ツール"),
            input_schema: serde_json::json!({ "type": "object" }),
        }
    }

    // Given: Orchestrator のポリシーと read/edit/shell/grep のツール定義
    // When: filter_tool_specs に渡す
    // Then: mutation 系 (edit/shell) が除去され、read/grep だけが順序を保って残る
    #[test]
    fn orchestrator_policy_filters_out_mutation_tool_specs() {
        let policy = ExecutionPolicy::for_role(Role::Orchestrator);
        let specs = vec![spec("read"), spec("edit"), spec("shell"), spec("grep")];

        let filtered = policy.filter_tool_specs(specs);

        let names: Vec<&str> = filtered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["read", "grep"]);
    }

    // Given: Worker のポリシー (read/edit/grep/shell/git_diff を許可)
    // When: edit を authorize する
    // Then: 許可される
    #[test]
    fn worker_authorize_edit_is_allowed() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        assert_eq!(policy.authorize("edit"), Ok(()));
    }

    // Given: Explorer のポリシー (read/grep のみ)
    // When: edit を authorize する
    // Then: CapabilityDenied (role/tool/reason) で拒否される
    #[test]
    fn explorer_authorize_edit_is_denied() {
        let policy = ExecutionPolicy::for_role(Role::Explorer);

        let Err(RuntimeError::CapabilityDenied { role, tool, reason }) = policy.authorize("edit")
        else {
            panic!("Explorer の edit は拒否されるべき");
        };

        assert_eq!(role, "Explorer");
        assert_eq!(tool, "edit");
        assert!(!reason.is_empty());
    }

    // Given: Orchestrator のポリシー (委譲系・メッセージ系メタ操作を許可)
    // When: delegate 系と send/wait_reply/inbox のメタ操作名を authorize する
    // Then: capability 集合に含まれるため許可される
    #[test]
    fn orchestrator_authorizes_delegation_and_messaging_meta_ops() {
        let policy = ExecutionPolicy::for_role(Role::Orchestrator);

        for op in [
            "delegate",
            "delegate_background",
            "send_message",
            "send",
            "wait_reply",
            "inbox",
            "wait",
            "cancel",
        ] {
            assert_eq!(
                policy.authorize(op),
                Ok(()),
                "meta-op {op} は Orchestrator に許可されるべき"
            );
        }
    }

    // Given: Worker のポリシー (委譲不可)
    // When: delegate を authorize する
    // Then: CapabilityDenied で拒否される
    #[test]
    fn worker_authorize_delegate_is_denied() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        assert!(matches!(
            policy.authorize("delegate"),
            Err(RuntimeError::CapabilityDenied { .. })
        ));
    }

    // Given: Worker と各非 Worker ロールのポリシー
    // When: escalate を authorize する
    // Then: Worker のみ許可され、他ロールは CapabilityDenied になる
    #[test]
    fn worker_authorizes_escalate_and_other_roles_deny_it() {
        assert_eq!(
            ExecutionPolicy::for_role(Role::Worker).authorize("escalate"),
            Ok(())
        );

        for role in [
            Role::Orchestrator,
            Role::Explorer,
            Role::Reviewer,
            Role::Librarian,
        ] {
            assert!(matches!(
                ExecutionPolicy::for_role(role).authorize("escalate"),
                Err(RuntimeError::CapabilityDenied { .. })
            ));
        }
    }

    // Given: Worker のポリシーと read/edit/shell/delegate のツール定義
    // When: filter_tool_specs に渡す
    // Then: 境界内 3 つ (read/edit/shell) が残り、境界外の delegate は除去される
    #[test]
    fn worker_policy_keeps_boundary_tools_and_drops_others() {
        let policy = ExecutionPolicy::for_role(Role::Worker);
        let specs = vec![spec("read"), spec("edit"), spec("shell"), spec("delegate")];

        let filtered = policy.filter_tool_specs(specs);

        let names: Vec<&str> = filtered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["read", "edit", "shell"]);
    }

    // Given: META_OPS の正規集合
    // When: is_meta_op を全要素と境界外の名前に適用する
    // Then: 14 操作すべて true、通常ツール・空文字は false
    #[test]
    fn meta_ops_membership_is_exhaustive() {
        let expected = [
            "delegate",
            "delegate_background",
            "send_message",
            "skill_load",
            "wait",
            "cancel",
            "list_agents",
            "inspect_agent",
            "compact",
            "finish",
            "send",
            "wait_reply",
            "inbox",
            "escalate",
        ];

        assert_eq!(META_OPS.len(), 14);
        assert_eq!(META_OPS, expected);
        for &op in META_OPS {
            assert!(is_meta_op(op), "{op} は meta-op であるべき");
        }
        assert!(!is_meta_op("edit"));
        assert!(!is_meta_op("read"));
        assert!(!is_meta_op(""));
    }

    #[test]
    fn orchestrator_authorizes_skill_load() {
        let policy = ExecutionPolicy::for_role(Role::Orchestrator);

        assert_eq!(policy.authorize("skill_load"), Ok(()));
    }

    #[test]
    fn worker_authorizes_skill_load() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        assert_eq!(policy.authorize("skill_load"), Ok(()));
    }

    #[test]
    fn explorer_denies_skill_load() {
        let policy = ExecutionPolicy::for_role(Role::Explorer);

        assert!(matches!(
            policy.authorize("skill_load"),
            Err(RuntimeError::CapabilityDenied { .. })
        ));
    }

    #[test]
    fn reviewer_denies_skill_load() {
        let policy = ExecutionPolicy::for_role(Role::Reviewer);

        assert!(matches!(
            policy.authorize("skill_load"),
            Err(RuntimeError::CapabilityDenied { .. })
        ));
    }

    // Given: Reviewer ロール
    // When: for_role で構築する
    // Then: role_name と capabilities がロール定義どおりになる
    #[test]
    fn for_role_carries_role_name_and_capabilities() {
        let policy = ExecutionPolicy::for_role(Role::Reviewer);

        assert_eq!(policy.role_name, "Reviewer");
        assert_eq!(policy.capabilities, Role::Reviewer.capabilities());
    }
}
