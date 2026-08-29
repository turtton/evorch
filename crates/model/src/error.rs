//! モデルカタログ操作で返すエラーを定義します。

/// モデルカタログ関連の操作で発生するエラー。
///
/// [`std::error::Error`] は thiserror により自動実装される。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ModelError {
    /// カタログソース (models.dev 等) の取得に失敗した。
    #[error("catalog fetch failed: {0}")]
    Fetch(String),
    /// 取得したカタログの解析 (JSON デコード・検証) に失敗した。
    #[error("catalog parse failed: {0}")]
    Parse(String),
    /// カタログキャッシュの読み書きに失敗した。
    #[error("catalog cache failed: {0}")]
    Cache(String),
}
