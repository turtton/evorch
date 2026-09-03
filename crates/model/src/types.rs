//! モデルカタログのコア型 (プロバイダ種別・機能・価格・カタログ項目) を定義します。

use serde::{Deserialize, Serialize};

/// カタログが扱うプロバイダの種別。
///
/// シリアライズ識別子はケバブケース (例: `anthropic-subscription`) です。
/// `OpenAi` 系は語を分割しない識別子 (`openai`・`openai-codex`・
/// `openai-compatible`) として直列化するため個別に rename します。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderType {
    /// Anthropic API (従量課金)。
    Anthropic,
    /// Anthropic サブスクリプション (Claude Pro / Max)。
    AnthropicSubscription,
    /// OpenAI API。
    #[serde(rename = "openai")]
    OpenAi,
    /// OpenAI Codex (ChatGPT サブスクリプション連携)。
    #[serde(rename = "openai-codex")]
    OpenAiCodex,
    /// GitHub Copilot。
    GithubCopilot,
    /// OpenRouter。
    Openrouter,
    /// OpenAI 互換 API (汎用プレースホルダ)。
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

/// モデルとの通信に用いる API プロトコル。
///
/// シリアライズ識別子はケバブケース (例: `anthropic-messages`) です。
/// `OpenAi` 系は [`ProviderType`] と同様に語を分割しない識別子
/// (`openai-responses`・`openai-completions`・`openai-codex-responses`) として直列化します。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiProtocol {
    /// Anthropic Messages API。
    AnthropicMessages,
    /// OpenAI Responses API。
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// OpenAI Chat Completions API。
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    /// OpenAI Codex Responses API。Codex subscription backend は `store=false` と `stream=true` を強制する。
    #[serde(rename = "openai-codex-responses")]
    OpenAiCodexResponses,
}

/// 論理モデル ID。
///
/// 設定上のモデル参照 (`logical model`) を表す文字列の newtype です。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LogicalModelId(String);

impl LogicalModelId {
    /// ID の文字列表現を返す。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for LogicalModelId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for LogicalModelId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// モデルが備える機能フラグの集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCapabilities {
    /// ツール呼び出し (function calling) 対応。
    pub tool_calling: bool,
    /// 拡張思考などの推論機能対応。
    pub reasoning: bool,
    /// プロンプトキャッシュ対応。
    pub prompt_cache: bool,
}

/// モデルの価格情報 (1M トークンあたり)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// 入力 1M トークンあたりの価格 (USD)。
    pub input_per_million_usd: f64,
    /// 出力 1M トークンあたりの価格 (USD)。
    pub output_per_million_usd: f64,
}

/// モデルの利用可否。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Availability {
    /// 利用可能。
    Available,
    /// 利用不可 (認証状況等で変動)。
    Unavailable,
}

/// カタログ項目の供給源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CatalogSource {
    /// 組み込みデフォルト。
    Builtin,
    /// models.dev 等の外部カタログ。
    ModelsDev,
    /// プロバイダ API から検出されたモデル。
    Discovered,
}

/// カタログに登録される 1 モデル分の情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// モデル ID (プロバイダ上の実モデル ID)。
    pub model_id: String,
    /// 属するプロバイダ種別。
    pub provider: ProviderType,
    /// コンテキストウィンドウサイズ (トークン)。
    pub context_window: u64,
    /// 最大出力トークン数。
    pub max_output_tokens: u64,
    /// 機能フラグ。
    pub capabilities: CatalogCapabilities,
    /// 価格情報。不明な場合は `None`。
    pub price: Option<ModelPrice>,
    /// 利用可否。
    pub availability: Availability,
    /// この項目の供給源。
    pub source: CatalogSource,
    /// 属性 (機能・価格など) が確定済みかどうか。
    ///
    /// プロバイダ検出で追加された属性未確定モデルは `false` です。
    pub attributes_confirmed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: ケバブケースで直列化される ProviderType / When: 各バリアントを JSON 化して読み戻す
    // Then: 指定の識別子と往復する
    #[test]
    fn provider_type_serde_kebab_case_roundtrip() {
        let cases = [
            (ProviderType::Anthropic, "anthropic"),
            (
                ProviderType::AnthropicSubscription,
                "anthropic-subscription",
            ),
            (ProviderType::OpenAi, "openai"),
            (ProviderType::OpenAiCodex, "openai-codex"),
            (ProviderType::GithubCopilot, "github-copilot"),
            (ProviderType::Openrouter, "openrouter"),
            (ProviderType::OpenAiCompatible, "openai-compatible"),
        ];

        for (value, expected) in cases {
            let json = serde_json::to_value(value).expect("直列化に成功する");
            assert_eq!(json, expected, "{value:?} の直列化識別子");
            let parsed: ProviderType = serde_json::from_value(json).expect("逆直列化に成功する");
            assert_eq!(parsed, value, "{value:?} の逆直列化結果");
        }
    }

    // Given: ケバブケースで直列化される ApiProtocol / When: 各バリアントを JSON 化して読み戻す
    // Then: 指定の識別子と往復する
    #[test]
    fn api_protocol_serde_kebab_case_roundtrip() {
        let cases = [
            (ApiProtocol::AnthropicMessages, "anthropic-messages"),
            (ApiProtocol::OpenAiResponses, "openai-responses"),
            (ApiProtocol::OpenAiCompletions, "openai-completions"),
            (ApiProtocol::OpenAiCodexResponses, "openai-codex-responses"),
        ];

        for (value, expected) in cases {
            let json = serde_json::to_value(value).expect("直列化に成功する");
            assert_eq!(json, expected, "{value:?} の直列化識別子");
            let parsed: ApiProtocol = serde_json::from_value(json).expect("逆直列化に成功する");
            assert_eq!(parsed, value, "{value:?} の逆直列化結果");
        }
    }
}
