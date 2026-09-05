/// ルーティング設定または候補選択で発生するエラーです。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RoutingError {
    /// 指定されたプロバイダプロファイルが存在しない。
    #[error("unknown profile: {0}")]
    UnknownProfile(String),
    /// 指定された論理モデルが存在しない。
    #[error("unknown logical model: {0}")]
    UnknownLogicalModel(String),
    /// 候補選択後に利用可能なプロバイダがない。
    #[error("no available candidate: {0}")]
    NoAvailableCandidate(String),
    /// プロバイダプロファイルの検証に失敗した。
    #[error("invalid provider profile: {reason}")]
    InvalidProfile {
        /// 検証失敗の理由。
        reason: String,
    },
    /// 指定されたプロバイダ種別の client 構築は未対応。
    #[error("unsupported provider type: {provider_type}")]
    UnsupportedProviderType {
        /// 未対応のプロバイダ種別 (設定上の識別子)。
        provider_type: String,
    },
    /// プロバイダの認証環境変数が設定されていない。
    #[error("environment variable `{var}` for provider `{profile}` is unset")]
    MissingCredential {
        /// 対象プロファイル名。
        profile: String,
        /// 未設定の環境変数名。
        var: String,
    },
    /// プロバイダの認証環境変数が空である。
    #[error("environment variable `{var}` for provider `{profile}` is empty")]
    EmptyCredential {
        /// 対象プロファイル名。
        profile: String,
        /// 空の環境変数名。
        var: String,
    },
    /// 構成対象のプロバイダが一件もない。
    #[error("no providers configured")]
    NoProviders,
}
