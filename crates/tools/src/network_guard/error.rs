use std::net::IpAddr;

/// NetworkGuard が fail-closed で拒否した理由。
#[derive(Debug, thiserror::Error)]
pub enum NetworkGuardError {
    /// URL を解析できない。
    #[error("URL を解析できません: {0}")]
    InvalidUrl(String),
    /// HTTP(S) 以外の scheme。
    #[error("HTTP(S) 以外の URL scheme は許可されていません: {scheme}")]
    NotHttpScheme { scheme: String },
    /// HTTPS upgrade 後の URL が HTTPS ではない。
    #[error("HTTPS upgrade 後の URL が HTTPS ではありません")]
    NotHttpsAfterUpgrade,
    /// URL に接続先 host がない。
    #[error("URL に接続先 host がありません")]
    MissingHost,
    /// DNS resolver を初期化できない。
    #[error("DNS resolver の初期化に失敗しました: {0}")]
    DnsResolverInitialization(String),
    /// DNS 解決に失敗したか、結果が空だった。
    #[error("DNS 解決に失敗しました: {host}: {detail}")]
    DnsResolutionFailed { host: String, detail: String },
    /// 遮断対象 IP。
    #[error("遮断対象 IP への接続を拒否しました: {addr}")]
    BlockedIp { addr: IpAddr },
    /// HTTPS 接続または TLS handshake に失敗した。
    #[error("HTTPS 接続に失敗しました: {0}")]
    HttpsConnectFailed(reqwest::Error),
    /// redirect 回数が上限を超えた。
    #[error("redirect 回数が上限を超えました")]
    TooManyRedirects,
    /// POST 応答が 3xx redirect を返した（redirect は追従しない）。
    #[error("POST 応答の redirect は追従しません: location = {location:?}")]
    RedirectOnPost {
        /// 応答に含まれていた Location header（存在する場合）。
        location: Option<String>,
    },
    /// redirect の Location が欠落または不正。
    #[error("redirect の Location が不正です: {0}")]
    RedirectLocationInvalid(String),
    /// response がサイズ上限を超えた。
    #[error("response が {check} の上限 {limit} bytes を超えました")]
    ResponseTooLarge {
        /// 超過を検出した段階。
        check: &'static str,
        /// 適用した上限。
        limit: usize,
    },
    /// response の解凍に失敗した。
    #[error("response の解凍に失敗しました: {0}")]
    DecompressionFailed(String),
    /// HTTP request または body stream が失敗した。
    #[error("HTTP 通信に失敗しました: {0}")]
    Http(#[from] reqwest::Error),
}
