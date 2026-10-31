//! provider 固有の公式 compaction を optional capability として扱います。

use async_trait::async_trait;

use crate::{ChatRequest, ProviderError, Usage};

/// 後続リクエストへ再生する暗号化状態と、その生成に使用したトークン数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub encrypted_content: String,
    /// 生成に使用したトークン数。`usage_reported` が `false` のときは実測値ではなく、
    /// 未報告を表す `Usage::default()` の代替値。
    pub usage: Usage,
    /// provider が usage を報告したか。未報告のゼロを実測ゼロとして集計してはならない。
    pub usage_reported: bool,
}

#[async_trait]
pub trait Compactor: Send + Sync {
    /// 公式 API で会話を圧縮します。暗号化状態は呼び出し元でも解釈しません。
    ///
    /// # Errors
    /// 送信・解析の失敗や、有効な compaction item の欠落を返します。
    async fn compact(&self, request: &ChatRequest) -> Result<CompactionResult, ProviderError>;
}
