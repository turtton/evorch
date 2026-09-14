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
}

/// OAuth の Bearer トークンで Codex のモデルカタログを取得する。
///
/// # Errors
/// HTTP・通信エラー、または応答形式が不正な場合の JSON エラーを返す。
pub async fn list_codex_models(
    base_url: &str,
    auth: &ProviderAuth,
    client_version: &str,
) -> Result<Vec<String>, ProviderError> {
    let request = build_http_client(None)?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .query(&[("client_version", client_version)])
        .bearer_auth(&auth.api_key);
    let models: CodexModelList = fetch_list(request).await?;
    Ok(models.models.into_iter().map(|model| model.slug).collect())
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
