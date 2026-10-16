//! Native Cursor subscription provider. No Cursor executable or filesystem tools
//! run here: MCP calls are handed to the canonical evorch tool loop.
//!
//! Protocol adapted from can1357/oh-my-pi at 602b6c8. See OMP-LICENSE.
mod history;
mod models;
mod oauth;
mod quota;
#[cfg(test)]
mod tests;
mod tokens;
mod transport;
mod wire;

pub use oauth::{CURSOR_CLIENT_ID, CursorLoginRequest, CursorOAuthClient, CursorOAuthConfig};
pub use quota::{CursorQuotaClient, CursorQuotaConfig, CursorQuotaSnapshot, CursorQuotaWindow};
pub use tokens::{CursorTokenBundle, CursorTokenStore, InMemoryCursorTokenStore};

use crate::http::map_request_error;
use crate::{
    ChatRequest, ChatResponse, DeltaStream, ProviderAuth, ProviderCapabilities, ProviderClient,
    ProviderError, StreamEvent,
};
use async_trait::async_trait;
use event_bus::EventBus;
use futures_util::StreamExt;
use history::Conversation;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use wire::Proto;

const CLIENT_VERSION: &str = "cli-2026.09.02-c22c1a3";
#[derive(Clone)]
pub struct CursorConfig {
    pub base_url: String,
    pub event_bus: Option<Arc<EventBus>>,
}
impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api2.cursor.sh".into(),
            event_bus: None,
        }
    }
}
pub struct CursorClient {
    http: reqwest::Client,
    config: CursorConfig,
    store: Arc<dyn CursorTokenStore>,
    oauth: CursorOAuthClient,
    profile: Option<String>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Conversation>>>>,
    model_routes: tokio::sync::Mutex<HashMap<String, models::ModelRoute>>,
}
impl CursorClient {
    pub fn new(
        config: CursorConfig,
        store: Arc<dyn CursorTokenStore>,
    ) -> Result<Self, ProviderError> {
        validate_endpoint(&config.base_url)?;
        Ok(Self {
            http: http_client(None)?,
            config,
            store,
            oauth: CursorOAuthClient::new(CursorOAuthConfig::default())?,
            profile: None,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            model_routes: tokio::sync::Mutex::new(HashMap::new()),
        })
    }
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }
    pub fn with_oauth_config(mut self, config: CursorOAuthConfig) -> Result<Self, ProviderError> {
        self.oauth = CursorOAuthClient::new(config)?;
        Ok(self)
    }
    pub async fn access_token(&self) -> Result<CursorTokenBundle, ProviderError> {
        let lock = self.store.refresh_lock();
        let _guard = lock.lock().await;
        let bundle = self
            .store
            .load()?
            .ok_or_else(|| ProviderError::Request("Cursor login required".into()))?;
        if bundle.needs_refresh(unix_now()) {
            let bundle = self.oauth.refresh(&bundle).await?;
            self.store.save(&bundle)?;
            Ok(bundle)
        } else {
            Ok(bundle)
        }
    }
    async fn session(
        &self,
        request: &ChatRequest,
    ) -> Result<tokio::sync::OwnedMutexGuard<Conversation>, ProviderError> {
        let key = history::conversation_key(request);
        let session = {
            let mut sessions = self.sessions.lock().await;
            if !sessions.contains_key(&key) && sessions.len() >= 32 {
                let idle = sessions
                    .iter()
                    .find(|(_, state)| Arc::strong_count(state) == 1)
                    .map(|(key, _)| key.clone());
                if let Some(idle) = idle {
                    sessions.remove(&idle);
                } else {
                    return Err(ProviderError::Request(
                        "Too many concurrent Cursor conversations".into(),
                    ));
                }
            }
            sessions
                .entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Conversation::new())))
                .clone()
        };
        Ok(session.lock_owned().await)
    }
    async fn catalog(
        &self,
        token: &str,
        path: &str,
        body: Proto,
    ) -> Result<Vec<u8>, ProviderError> {
        let response = rpc(
            &self.http,
            &self.config.base_url,
            token,
            path,
            "application/proto",
        )
        .timeout(std::time::Duration::from_secs(15))
        .body(body.0)
        .send()
        .await
        .map_err(map_request_error)?;
        if !response.status().is_success() {
            return Err(status_error(
                response.status().as_u16(),
                "Cursor catalog request failed",
            ));
        }
        bounded_bytes(response, 4 * 1024 * 1024).await
    }
}
#[async_trait]
impl ProviderClient for CursorClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: true,
        }
    }
    async fn list_models(&self, _: &ProviderAuth) -> Result<Option<Vec<String>>, ProviderError> {
        let token = self.access_token().await?;
        let routes = self.discover_models(&token.access_token).await?;
        let mut names: Vec<_> = routes.keys().cloned().collect();
        self.model_routes.lock().await.extend(routes);
        names.sort();
        Ok(Some(names))
    }
    async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let mut stream = self.stream(auth, request).await?;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Completed { response } = event? {
                return Ok(response);
            }
        }
        Err(ProviderError::Request(
            "Cursor stream ended without completion".into(),
        ))
    }
    async fn stream(
        &self,
        _: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        let token = self.access_token().await?;
        let mut state = self.session(request).await?;
        // Pin wire settings for the conversation: refreshing discovery must not
        // silently invalidate the already-sent prompt/cache prefix.
        let route = match &state.model_route {
            Some(route) => route.clone(),
            None => {
                let route = self
                    .model_route(
                        &token.access_token,
                        &request.model,
                        request.reasoning_effort.as_deref(),
                    )
                    .await?;
                state.model_route = Some(route.clone());
                route
            }
        };
        transport::start(
            self.http.clone(),
            self.config.clone(),
            self.profile.clone(),
            token.access_token,
            request.clone(),
            state,
            route,
        )
        .await
    }
}
fn rpc(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    content_type: &str,
) -> reqwest::RequestBuilder {
    http.post(format!("{}{path}", base.trim_end_matches('/')))
        .bearer_auth(token)
        .header("content-type", content_type)
        .header("connect-protocol-version", "1")
        .header("x-cursor-client-type", "cli")
        .header("x-cursor-client-version", CLIENT_VERSION)
        .header("x-ghost-mode", "true")
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn validate_endpoint(value: &str) -> Result<(), ProviderError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| ProviderError::Request("Invalid Cursor endpoint URL".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::Request(
            "Cursor endpoint must be an HTTP(S) URL without credentials, query, or fragment".into(),
        ));
    }
    Ok(())
}
fn http_client(timeout: Option<std::time::Duration>) -> Result<reqwest::Client, ProviderError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .read_timeout(std::time::Duration::from_secs(90));
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder.build().map_err(map_request_error)
}
fn status_error(status: u16, message: &str) -> ProviderError {
    if status == 429 {
        ProviderError::RateLimited { retry_after: None }
    } else {
        ProviderError::Http {
            status,
            body: message.into(),
        }
    }
}
async fn bounded_bytes(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(ProviderError::Request(
            "Cursor response exceeds size limit".into(),
        ));
    }
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| map_request_error(error.without_url()))?;
        if output.len().saturating_add(chunk.len()) > limit {
            return Err(ProviderError::Request(
                "Cursor response exceeds size limit".into(),
            ));
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}
