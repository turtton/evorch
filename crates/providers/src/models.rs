//! Model discovery for OpenAI-compatible endpoints.

use crate::http::{build_http_client, map_request_error, map_response_error};
use crate::wire::openai::WireModelList;
use crate::{ProviderAuth, ProviderError};

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
    let response = build_http_client(None)?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .bearer_auth(&auth.api_key)
        .send()
        .await
        .map_err(map_request_error)?;
    if !response.status().is_success() {
        return Err(map_response_error(response).await);
    }
    let body = response.bytes().await.map_err(map_request_error)?;
    let models: WireModelList =
        serde_json::from_slice(&body).map_err(|error| ProviderError::InvalidJson {
            detail: error.to_string(),
        })?;
    Ok(models.data.into_iter().map(|model| model.id).collect())
}
