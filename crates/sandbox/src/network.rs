//! ネットワーク接続先の許可表現です。
//!
//! bubblewrap のネットワーク名前空間は全許可か全拒否のため、v0.1 の OS レベル
//! 強制は既定で全拒否です。この型は設定と将来のプロキシが利用する許可リストです。

use std::collections::HashSet;

/// 接続を許可するホスト名の集合。
#[derive(Debug, Clone, Default)]
pub struct NetworkPolicy {
    hosts: HashSet<String>,
}

impl NetworkPolicy {
    pub fn deny_all() -> Self {
        Self::default()
    }

    pub fn providers_only() -> Self {
        Self::deny_all()
            .with_host("api.openai.com")
            .with_host("api.anthropic.com")
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.hosts.insert(host.into());
        self
    }

    pub fn is_allowed(&self, host: &str) -> bool {
        self.hosts.contains(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 全拒否方針 / When: 任意ホストを確認 / Then: 拒否される
    #[test]
    fn deny_all_rejects_hosts() {
        assert!(!NetworkPolicy::deny_all().is_allowed("api.openai.com"));
    }

    // Given: プロバイダ限定方針 / When: 既定ホストを確認 / Then: 二つだけ許可される
    #[test]
    fn providers_only_allows_known_hosts() {
        let policy = NetworkPolicy::providers_only();
        assert!(policy.is_allowed("api.openai.com"));
        assert!(policy.is_allowed("api.anthropic.com"));
        assert!(!policy.is_allowed("example.com"));
    }

    // Given: 追加ホスト / When: 許可状態を確認 / Then: 追加したホストが許可される
    #[test]
    fn with_host_adds_allowlist_entry() {
        assert!(
            NetworkPolicy::deny_all()
                .with_host("example.com")
                .is_allowed("example.com")
        );
    }
}
