//! ツール権限の承認方針を定義します。

use std::collections::HashMap;

/// ツールが要求する能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub fs_read: bool,
    pub fs_write: bool,
    pub process_spawn: bool,
    /// ネットワークアクセス。
    pub network: bool,
}

/// 方針による分類結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    AutoAllow,
    Ask,
    Deny,
}

/// 利用者へ承認を求める時点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    OnRequest,
    OnFailure,
    Never,
}

/// ツール別の例外を持つ承認方針。
#[derive(Debug, Clone)]
pub struct ApprovalPolicy {
    mode: ApprovalMode,
    overrides: HashMap<String, PolicyDecision>,
}

impl ApprovalPolicy {
    pub fn standard(mode: ApprovalMode) -> Self {
        Self {
            mode,
            overrides: HashMap::new(),
        }
    }

    pub fn allow_all() -> Self {
        Self::standard(ApprovalMode::Never).with_override("*", PolicyDecision::AutoAllow)
    }

    pub fn with_override(mut self, tool_name: impl Into<String>, decision: PolicyDecision) -> Self {
        self.overrides.insert(tool_name.into(), decision);
        self
    }

    pub const fn mode(&self) -> ApprovalMode {
        self.mode
    }

    pub fn classify(&self, tool_name: &str, caps: &Capabilities) -> PolicyDecision {
        self.overrides
            .get(tool_name)
            .or_else(|| self.overrides.get("*"))
            .copied()
            .unwrap_or(if !caps.fs_write && !caps.process_spawn && !caps.network {
                PolicyDecision::AutoAllow
            } else {
                PolicyDecision::Ask
            })
    }
}

/// 実行側が取る操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Proceed,
    AskFirst,
    AskOnFailure,
    Deny,
}

pub const fn resolve(decision: PolicyDecision, mode: ApprovalMode) -> Action {
    match (decision, mode) {
        (
            PolicyDecision::AutoAllow,
            ApprovalMode::OnRequest | ApprovalMode::OnFailure | ApprovalMode::Never,
        ) => Action::Proceed,
        (
            PolicyDecision::Deny,
            ApprovalMode::OnRequest | ApprovalMode::OnFailure | ApprovalMode::Never,
        ) => Action::Deny,
        (PolicyDecision::Ask, ApprovalMode::OnRequest) => Action::AskFirst,
        (PolicyDecision::Ask, ApprovalMode::OnFailure) => Action::AskOnFailure,
        (PolicyDecision::Ask, ApprovalMode::Never) => Action::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(fs_write: bool, process_spawn: bool) -> Capabilities {
        Capabilities {
            fs_read: true,
            fs_write,
            process_spawn,
            network: false,
        }
    }

    // Given: 標準方針と能力の全組合せ / When: 分類 / Then: 読み取り専用だけ自動許可される
    #[test]
    fn standard_classification_table() {
        let policy = ApprovalPolicy::standard(ApprovalMode::OnRequest);
        assert_eq!(
            policy.classify("read", &caps(false, false)),
            PolicyDecision::AutoAllow
        );
        assert_eq!(
            policy.classify("write", &caps(true, false)),
            PolicyDecision::Ask
        );
        assert_eq!(
            policy.classify("spawn", &caps(false, true)),
            PolicyDecision::Ask
        );
        assert_eq!(
            policy.classify("both", &caps(true, true)),
            PolicyDecision::Ask
        );
    }

    // Given: 標準方針と network のみ true の能力 / When: 分類 / Then: 自動許可されず Ask になる (fail-closed)
    #[test]
    fn network_only_capability_is_not_auto_allowed() {
        let policy = ApprovalPolicy::standard(ApprovalMode::OnRequest);
        let caps = Capabilities {
            fs_read: false,
            fs_write: false,
            process_spawn: false,
            network: true,
        };

        assert_eq!(policy.classify("network", &caps), PolicyDecision::Ask);
    }

    // Given: 標準方針と network + fs_read の能力 / When: 分類 / Then: 自動許可されず Ask になる
    #[test]
    fn network_with_read_capability_is_not_auto_allowed() {
        let policy = ApprovalPolicy::standard(ApprovalMode::OnRequest);
        let caps = Capabilities {
            fs_read: true,
            fs_write: false,
            process_spawn: false,
            network: true,
        };

        assert_eq!(policy.classify("web", &caps), PolicyDecision::Ask);
    }

    // Given: 標準分類と異なる明示指定 / When: 分類 / Then: 明示指定が優先される
    #[test]
    fn explicit_override_has_precedence() {
        let policy = ApprovalPolicy::standard(ApprovalMode::OnRequest)
            .with_override("write", PolicyDecision::Deny);
        assert_eq!(
            policy.classify("write", &caps(true, false)),
            PolicyDecision::Deny
        );
    }

    // Given: 全許可方針 / When: 書き込み能力を分類 / Then: 自動許可される
    #[test]
    fn allow_all_overrides_every_tool() {
        assert_eq!(
            ApprovalPolicy::allow_all().classify("write", &caps(true, true)),
            PolicyDecision::AutoAllow
        );
    }

    // Given: 分類と承認時点の全組合せ / When: 操作へ解決 / Then: 契約表どおりになる
    #[test]
    fn resolve_table() {
        for mode in [
            ApprovalMode::OnRequest,
            ApprovalMode::OnFailure,
            ApprovalMode::Never,
        ] {
            assert_eq!(resolve(PolicyDecision::AutoAllow, mode), Action::Proceed);
            assert_eq!(resolve(PolicyDecision::Deny, mode), Action::Deny);
        }
        assert_eq!(
            resolve(PolicyDecision::Ask, ApprovalMode::OnRequest),
            Action::AskFirst
        );
        assert_eq!(
            resolve(PolicyDecision::Ask, ApprovalMode::OnFailure),
            Action::AskOnFailure
        );
        assert_eq!(
            resolve(PolicyDecision::Ask, ApprovalMode::Never),
            Action::Deny
        );
    }
}
