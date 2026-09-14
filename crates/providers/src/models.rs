//! Model discovery for OpenAI-compatible endpoints.

use crate::http::{build_http_client, map_request_error, map_response_error};
use crate::wire::openai::WireModelList;
use crate::{ProviderAuth, ProviderError};
use serde::{Deserialize, de::DeserializeOwned};

/// Fetch model identifiers from `{base_url}/models` using Bearer authentication.
///
/// Uses the shared connection and read timeouts without a whole-request timeout.
///
/// # Errors
/// Returns the shared HTTP/transport error or [`ProviderError::InvalidJson`]
/// when the response cannot be decoded as a model list.
pub async fn list_models(
    base_url: &str,
    auth: &ProviderAuth,
) -> Result<Vec<String>, ProviderError> {
    let request = build_http_client(None)?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .bearer_auth(&auth.api_key);
    let models: WireModelList = fetch_list(request).await?;
    Ok(models.data.into_iter().map(|model| model.id).collect())
}

#[derive(Deserialize)]
struct CodexModelList {
    models: Vec<CodexModel>,
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
    #[serde(default)]
    service_tiers: Option<Vec<CodexServiceTierEntry>>,
    #[serde(default)]
    additional_speed_tiers: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct CodexServiceTierEntry {
    id: String,
}

/// Codexカタログに広告されたモデルとfast対応情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexModelInfo {
    /// APIへ送信する実モデル識別子。
    pub slug: String,
    /// priority tierまたは旧fast tierの対応が広告されているか。
    pub supports_fast: bool,
}

/// カタログの `minimal_client_version` フィルタを満たす Codex CLI 互換バージョン。
///
/// 観測例では 0.7.x→0件、0.147.0→9件で、gpt-6-astra は 0.153.0 必須。
/// アプリ自身のバージョンを送るとモデルが除外されるため、絶対に代用しない。
pub const CODEX_MODELS_CLIENT_VERSION: &str = "0.153.0";

/// OAuth の Bearer トークンとアカウント ID で Codex のモデルカタログを取得する。
///
/// # Errors
/// HTTP・通信エラー、または応答形式が不正な場合の JSON エラーを返す。
pub async fn list_codex_models(
    base_url: &str,
    auth: &ProviderAuth,
    account_id: &str,
) -> Result<Vec<CodexModelInfo>, ProviderError> {
    let request = build_http_client(None)?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .query(&[("client_version", CODEX_MODELS_CLIENT_VERSION)])
        .header("chatgpt-account-id", account_id)
        .header("originator", "codex_cli_rs")
        .header(
            reqwest::header::USER_AGENT,
            format!("codex_cli_rs/{CODEX_MODELS_CLIENT_VERSION}"),
        )
        .bearer_auth(&auth.api_key);
    let models: CodexModelList = fetch_list(request).await?;
    Ok(models.models.into_iter().map(|model| CodexModelInfo {
        supports_fast: model.service_tiers.iter().flatten().any(|tier| tier.id == "priority")
            || model.additional_speed_tiers.iter().flatten().any(|tier| tier == "fast"),
        slug: model.slug,
    }).collect())
}

async fn fetch_list<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, ProviderError> {
    let response = request.send().await.map_err(map_request_error)?;
    if !response.status().is_success() {
        return Err(map_response_error(response).await);
    }
    let body = response.bytes().await.map_err(map_request_error)?;
    serde_json::from_slice(&body).map_err(|error| ProviderError::InvalidJson {
        detail: error.to_string(),
    })
}
