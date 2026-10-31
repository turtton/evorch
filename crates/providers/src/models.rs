//! Model discovery for OpenAI-compatible endpoints.

use crate::http::{build_http_client, map_request_error, map_response_error};
use crate::wire::openai::WireModelList;
use crate::{ProviderAuth, ProviderError};
use serde::{Deserialize, de::DeserializeOwned};

/// Verify authenticated catalog connectivity without issuing a completion.
///
/// # Errors
/// Returns the same transport, HTTP and decoding errors as [`list_models`].
pub async fn verify_connectivity(base_url: &str, auth: &ProviderAuth) -> Result<(), ProviderError> {
    list_models(base_url, auth).await.map(|_| ())
}

/// Fetch model identifiers from `{base_url}/models` using Bearer authentication.
///
/// Catalog requests have a ten-second whole-request timeout.
///
/// # Errors
/// Returns the shared HTTP/transport error or [`ProviderError::InvalidJson`]
/// when the response cannot be decoded as a model list.
pub async fn list_models(
    base_url: &str,
    auth: &ProviderAuth,
) -> Result<Vec<String>, ProviderError> {
    let request = build_http_client(Some(std::time::Duration::from_secs(10)))?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .bearer_auth(&auth.api_key);
    let models: WireModelList = fetch_list(request).await?;
    Ok(models.data.into_iter().map(|model| model.id).collect())
}

#[derive(Deserialize)]
struct CodexModelList {
    models: Vec<CodexModel>,
}

/// 新しい能力フィールドは型が想定と違っても catalog 全体を拒否せず、既定値に倒す。
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

fn default_true() -> bool {
    true
}

/// `{"effort": "low", ...}` の preset と、素の文字列の双方から effort 名を取り出す。
#[derive(Deserialize)]
#[serde(untagged)]
enum ReasoningLevelEntry {
    Preset { effort: String },
    Name(String),
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    max_context_window: Option<u64>,
    #[serde(default)]
    service_tiers: Option<Vec<CodexServiceTierEntry>>,
    #[serde(default)]
    additional_speed_tiers: Option<Vec<String>>,
    #[serde(default, deserialize_with = "lenient")]
    use_responses_lite: bool,
    #[serde(default, deserialize_with = "lenient")]
    default_reasoning_level: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    supported_reasoning_levels: Vec<ReasoningLevelEntry>,
    #[serde(default = "default_true", deserialize_with = "lenient_true")]
    supports_reasoning_summary_parameter: bool,
    #[serde(default, deserialize_with = "lenient")]
    support_verbosity: bool,
    #[serde(default, deserialize_with = "lenient")]
    default_verbosity: Option<String>,
}

fn lenient_true<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(true))
}

#[derive(Deserialize)]
struct CodexServiceTierEntry {
    id: String,
}

/// Codex catalog が広告するモデルごとの能力。要求形状の決定に使い、wire 変換は
/// これを参照して Responses Lite・reasoning・verbosity・service tier を切り替える。
/// [`Default`] は能力不明のモデルで、従来の（Lite でない）要求形状になる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexModelCapabilities {
    /// Responses Lite 形式（additional_tools・tools 省略・並列 tool 無効）を要求するか。
    pub use_responses_lite: bool,
    /// effort 未指定時にモデルが使う既定の reasoning effort。
    pub default_reasoning_level: Option<String>,
    /// モデルが広告する reasoning effort の一覧。
    pub supported_reasoning_levels: Vec<String>,
    /// `reasoning.summary` を受け付けるか。広告が無いモデルは受け付けるとみなす。
    pub supports_reasoning_summary_parameter: bool,
    /// `text.verbosity` を受け付けるか。
    pub support_verbosity: bool,
    /// `support_verbosity` が真のモデルの既定 verbosity。
    pub default_verbosity: Option<String>,
    /// priority tier の広告。`None` は catalog が tier を広告していない（検証しない）。
    pub priority_tier: Option<bool>,
}

impl Default for CodexModelCapabilities {
    fn default() -> Self {
        Self {
            use_responses_lite: false,
            default_reasoning_level: None,
            supported_reasoning_levels: Vec::new(),
            supports_reasoning_summary_parameter: true,
            support_verbosity: false,
            default_verbosity: None,
            priority_tier: None,
        }
    }
}

/// Codexカタログに広告されたモデルとfast対応情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexModelInfo {
    /// APIへ送信する実モデル識別子。
    pub slug: String,
    /// Context window advertised by this Codex backend, when available.
    pub context_window: Option<u64>,
    /// priority tierまたは旧fast tierの対応が広告されているか。
    pub supports_fast: bool,
    /// 要求形状を決める能力情報。
    pub capabilities: CodexModelCapabilities,
}

/// Codex backend へ名乗る Codex CLI のバージョン。
/// 推論リクエストのヘッダーとモデルカタログ取得の双方で使う。実行時に最新版を
/// 問い合わせず、リリースごとに更新する（Sol/Luna は 0.155.0 以上が必要）。
pub const CODEX_CLIENT_VERSION: &str = "0.166.0";

/// OAuth の Bearer トークンとアカウント ID で Codex のモデルカタログを取得する。
/// [`CODEX_CLIENT_VERSION`] を `client_version` として送る。
///
/// # Errors
/// HTTP・通信エラー、または応答形式が不正な場合の JSON エラーを返す。
pub async fn list_codex_models(
    base_url: &str,
    auth: &ProviderAuth,
    account_id: &str,
) -> Result<Vec<CodexModelInfo>, ProviderError> {
    let request = build_http_client(Some(std::time::Duration::from_secs(10)))?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .query(&[("client_version", CODEX_CLIENT_VERSION)])
        .header("version", CODEX_CLIENT_VERSION)
        .header("chatgpt-account-id", account_id)
        .header("originator", "codex_cli_rs")
        .header(
            reqwest::header::USER_AGENT,
            format!("codex_cli_rs/{CODEX_CLIENT_VERSION}"),
        )
        .bearer_auth(&auth.api_key);
    let models: CodexModelList = fetch_list(request).await?;
    Ok(models.models.into_iter().map(codex_model_info).collect())
}

fn codex_model_info(model: CodexModel) -> CodexModelInfo {
    let supports_fast = model
        .service_tiers
        .iter()
        .flatten()
        .any(|tier| tier.id == "priority")
        || model
            .additional_speed_tiers
            .iter()
            .flatten()
            .any(|tier| tier == "fast");
    let capabilities = CodexModelCapabilities {
        use_responses_lite: model.use_responses_lite,
        default_reasoning_level: model
            .default_reasoning_level
            .filter(|level| !level.is_empty()),
        supported_reasoning_levels: model
            .supported_reasoning_levels
            .into_iter()
            .map(|entry| match entry {
                ReasoningLevelEntry::Preset { effort } | ReasoningLevelEntry::Name(effort) => {
                    effort
                }
            })
            .collect(),
        supports_reasoning_summary_parameter: model.supports_reasoning_summary_parameter,
        support_verbosity: model.support_verbosity,
        default_verbosity: model.default_verbosity.filter(|level| !level.is_empty()),
        priority_tier: if supports_fast {
            Some(true)
        } else {
            model.service_tiers.is_some().then_some(false)
        },
    };
    CodexModelInfo {
        slug: model.slug,
        context_window: model
            .context_window
            .filter(|window| *window > 0)
            .or(model.max_context_window.filter(|window| *window > 0)),
        supports_fast,
        capabilities,
    }
}

async fn fetch_list<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, ProviderError> {
    let mut response = request.send().await.map_err(map_request_error)?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_request_error)? {
        if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err(ProviderError::Request("model catalog exceeds 1 MiB".into()));
        }
        body.extend_from_slice(&chunk);
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        if status == 429 {
            return Err(map_response_error(response).await);
        }
        return Err(ProviderError::Http {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
    }
    serde_json::from_slice(&body).map_err(|error| ProviderError::InvalidJson {
        detail: error.to_string(),
    })
}
