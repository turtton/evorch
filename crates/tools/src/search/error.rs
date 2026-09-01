//! keyless 検索プロバイダ層の error 分類。

/// MCP transport・検索 provider が返す error。
#[derive(Debug, Clone, thiserror::Error)]
pub enum SearchError {
    /// provider が異常な HTTP status を返した。
    #[error("provider が異常な HTTP status を返しました: {0}")]
    HttpStatus(u16),
    /// request が timeout した。
    #[error("provider への request が timeout しました")]
    Timeout,
    /// transport 層で通信が失敗した。
    #[error("provider への通信に失敗しました: {0}")]
    Transport(String),
    /// 応答 envelope が MCP JSON-RPC の契約どおりでない。
    #[error("provider の応答 envelope が不正です: {0}")]
    Protocol(String),
    /// provider が application level で拒否した。
    #[error("provider が拒否しました: {0}")]
    ProviderRejected(String),
}

impl SearchError {
    /// 別 provider への fallback を発火させる error か。
    ///
    /// interview Q3 の design lock: 429・5xx・timeout のみが fallback 対象。
    pub const fn is_fallback_trigger(&self) -> bool {
        matches!(
            self,
            Self::HttpStatus(429) | Self::HttpStatus(500..=599) | Self::Timeout
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: fallback 対象と非対象の error 群 / When: is_fallback_trigger を呼ぶ / Then: 429・5xx・timeout だけが true になる
    #[test]
    fn classifies_fallback_triggers() {
        let triggers = [
            SearchError::HttpStatus(429),
            SearchError::HttpStatus(500),
            SearchError::HttpStatus(599),
            SearchError::Timeout,
        ];
        let non_triggers = [
            SearchError::HttpStatus(400),
            SearchError::HttpStatus(499),
            SearchError::Transport("x".to_owned()),
            SearchError::Protocol("x".to_owned()),
            SearchError::ProviderRejected("x".to_owned()),
        ];

        for error in triggers {
            assert!(error.is_fallback_trigger(), "{error:?}");
        }
        for error in non_triggers {
            assert!(!error.is_fallback_trigger(), "{error:?}");
        }
    }
}
