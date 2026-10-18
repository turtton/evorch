//! Claude subscription OAuth, token refresh and quota. API keys use AnthropicClient.

mod fingerprint;
mod identity;
pub(super) mod oauth;
mod profile;
mod quota;
mod request;
mod tokens;
mod tool_adapter;
pub(super) mod transport;
mod transport_context;

pub use identity::{CLAUDE_INSTALLATION_ID_FILE, ClaudeInstallationId};
pub(super) use identity::{process_installation_id, request_metadata};
pub use oauth::{
    CLAUDE_CLIENT_ID, CLAUDE_SCOPE, ClaudeLoginRequest, ClaudeOAuthClient, ClaudeOAuthConfig,
    ClaudeOAuthError,
};
pub use profile::{
    CLAUDE_INFERENCE_BETAS, CLAUDE_SYSTEM_IDENTITY, CLAUDE_USER_AGENT, effort_levels,
};
pub(super) use profile::{ClaudeReasoningControls, ClaudeWireProfile};
pub use quota::{ClaudeExtraUsage, ClaudeQuotaClient, ClaudeQuotaSnapshot, ClaudeQuotaWindow};
pub(super) use request::{serialize_request, serialize_request_with_tools};
pub use tokens::{ClaudeTokenBundle, ClaudeTokenStore, InMemoryClaudeTokenStore};
pub(super) use tool_adapter::ClaudeToolAdapter;
pub(super) use transport_context::{ClaudeRequestContext, ClaudeTransportContext};

use super::anthropic::{AnthropicClient, AnthropicConfig};
use crate::stream::DeltaStream;
use crate::wire::anthropic::{WireContentBlock, WireMessagesRequest};
use crate::{
    ChatRequest, ChatResponse, ProviderAuth, ProviderCapabilities, ProviderClient, ProviderError,
};
use async_trait::async_trait;
use std::{
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

/// Account-scoped subscription client. The token store owns all persistence.
pub struct ClaudeClient {
    inner: AnthropicClient,
    store: Arc<dyn ClaudeTokenStore>,
    oauth: ClaudeOAuthClient,
    messages_account: OnceLock<Option<String>>,
}

impl ClaudeClient {
    pub fn new(
        config: AnthropicConfig,
        store: Arc<dyn ClaudeTokenStore>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            inner: AnthropicClient::new(config)?.with_claude_oauth()?,
            store,
            oauth: ClaudeOAuthClient::new(ClaudeOAuthConfig::default())
                .map_err(provider_auth_error)?,
            messages_account: OnceLock::new(),
        })
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.inner = self.inner.with_profile(profile);
        self
    }

    /// Use the application's persistent installation seed for subscription metadata.
    pub fn with_installation_id(mut self, installation_id: ClaudeInstallationId) -> Self {
        self.inner = self.inner.with_claude_installation_id(installation_id);
        self
    }

    pub fn with_oauth_config(mut self, config: ClaudeOAuthConfig) -> Result<Self, ProviderError> {
        self.oauth = ClaudeOAuthClient::new(config).map_err(provider_auth_error)?;
        Ok(self)
    }

    pub async fn access_token(&self) -> Result<ClaudeTokenBundle, ProviderError> {
        ensure_fresh(self.store.as_ref(), &self.oauth).await
    }

    fn messages_client(
        &self,
        bundle: &ClaudeTokenBundle,
    ) -> Result<AnthropicClient, ProviderError> {
        let account = account_identity(bundle).map(str::to_owned);
        let bound = self.messages_account.get_or_init(|| account.clone());
        if bound != &account {
            return Err(account_changed());
        }
        Ok(self.inner.clone().with_claude_account_id(account))
    }

    async fn messages_auth(&self) -> Result<(AnthropicClient, ProviderAuth), ProviderError> {
        let lock = self.store.refresh_lock();
        let _guard = lock.lock().await;
        let bundle = load_bundle(self.store.as_ref())?;
        if self
            .messages_account
            .get()
            .is_some_and(|bound| bound.as_deref() != account_identity(&bundle))
        {
            return Err(account_changed());
        }
        let bundle = freshen_bundle(self.store.as_ref(), &self.oauth, bundle).await?;
        Ok((
            self.messages_client(&bundle)?,
            ProviderAuth::new(bundle.access_token),
        ))
    }
}

#[async_trait]
impl ProviderClient for ClaudeClient {
    fn hosted_web_search(&self) -> Option<&dyn crate::HostedWebSearch> {
        Some(self)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let (client, auth) = self.messages_auth().await?;
        client.send(&auth, request).await
    }
    async fn stream(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        let (client, auth) = self.messages_auth().await?;
        client.stream(&auth, request).await
    }
    async fn list_models(
        &self,
        _auth: &ProviderAuth,
    ) -> Result<Option<Vec<String>>, ProviderError> {
        let bundle = self.access_token().await?;
        self.inner
            .list_models(&ProviderAuth::new(bundle.access_token))
            .await
    }
}

#[async_trait]
impl crate::HostedWebSearch for ClaudeClient {
    fn provider_name(&self) -> &'static str {
        "claude"
    }

    async fn search(
        &self,
        _auth: &ProviderAuth,
        request: &crate::HostedWebSearchRequest,
    ) -> Result<crate::HostedWebSearchResponse, crate::HostedWebSearchError> {
        let (client, auth) = self
            .messages_auth()
            .await
            .map_err(super::anthropic::web_search::sanitize)?;
        crate::HostedWebSearch::search(&client, &auth, request).await
    }
}

/// Resolve and atomically rotate a persisted token family under the account lock.
pub async fn ensure_fresh(
    store: &dyn ClaudeTokenStore,
    oauth: &ClaudeOAuthClient,
) -> Result<ClaudeTokenBundle, ProviderError> {
    let lock = store.refresh_lock();
    let _guard = lock.lock().await;
    ensure_fresh_locked(store, oauth).await
}

pub(super) async fn ensure_fresh_locked(
    store: &dyn ClaudeTokenStore,
    oauth: &ClaudeOAuthClient,
) -> Result<ClaudeTokenBundle, ProviderError> {
    freshen_bundle(store, oauth, load_bundle(store)?).await
}

fn account_identity(bundle: &ClaudeTokenBundle) -> Option<&str> {
    bundle
        .account_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
}

fn account_changed() -> ProviderError {
    ProviderError::Request("Claude account changed; recreate the client and start a new run".into())
}

fn load_bundle(store: &dyn ClaudeTokenStore) -> Result<ClaudeTokenBundle, ProviderError> {
    store
        .load()?
        .ok_or_else(|| ProviderError::Request("Claude login required".into()))
}

async fn freshen_bundle(
    store: &dyn ClaudeTokenStore,
    oauth: &ClaudeOAuthClient,
    bundle: ClaudeTokenBundle,
) -> Result<ClaudeTokenBundle, ProviderError> {
    if bundle.needs_refresh(unix_now()) {
        let refreshed = oauth.refresh(&bundle).await.map_err(provider_auth_error)?;
        store.save(&refreshed)?;
        Ok(refreshed)
    } else {
        Ok(bundle)
    }
}

fn provider_auth_error(error: ClaudeOAuthError) -> ProviderError {
    match error {
        ClaudeOAuthError::ReauthenticationRequired => ProviderError::Http {
            status: 401,
            body: error.to_string(),
        },
        ClaudeOAuthError::HttpStatus(status) => ProviderError::Http {
            status,
            body: "Claude OAuth request rejected".into(),
        },
        ClaudeOAuthError::Timeout => ProviderError::Timeout,
        ClaudeOAuthError::Transport => ProviderError::Transport {
            message: "Claude OAuth transport failed".into(),
        },
        _ => ProviderError::Request(error.to_string()),
    }
}
pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn required_version_error(value: &serde_json::Value) -> Option<String> {
    let error = value.get("error").unwrap_or(value);
    let message = error.get("message")?.as_str()?;
    let code = "claude_code_version_too_old";
    if error.get("code").and_then(serde_json::Value::as_str) != Some(code)
        && error.get("type").and_then(serde_json::Value::as_str) != Some(code)
        && !message.contains(code)
    {
        return None;
    }
    let lower = message.to_ascii_lowercase();
    let (_, tail) = lower.split_once("version ")?;
    let (version, _) = tail.split_once(" or newer is required")?;
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty() || part.len() > 10 || !part.bytes().all(|b| b.is_ascii_digit())
        })
    {
        return None;
    }
    Some(format!(
        "Claude compatibility profile needs an update: Claude Code {version} or newer is required"
    ))
}

pub(super) fn shape_oauth_request(
    wire: &mut WireMessagesRequest,
) -> Result<ClaudeToolAdapter, ProviderError> {
    let adapter = ClaudeToolAdapter::shape_request(wire)?;
    let system = wire.system.get_or_insert_with(Vec::new);
    system.insert(
        0,
        WireContentBlock::Text {
            text: CLAUDE_SYSTEM_IDENTITY.into(),
            cache_control: Some(crate::wire::anthropic::CacheControl::Ephemeral),
        },
    );
    for tool in &mut wire.tools {
        if tool.cache_control.is_some() {
            tool.cache_control = Some(crate::wire::anthropic::CacheControl::EphemeralHour);
        }
    }
    for block in system
        .iter_mut()
        .chain(wire.messages.iter_mut().flat_map(|m| &mut m.content))
    {
        use crate::wire::anthropic::CacheControl;
        match block {
            WireContentBlock::Text { cache_control, .. }
            | WireContentBlock::Image { cache_control, .. }
            | WireContentBlock::ToolUse { cache_control, .. }
            | WireContentBlock::ToolResult { cache_control, .. } => {
                if cache_control.is_some() {
                    *cache_control = Some(CacheControl::EphemeralHour);
                }
            }
            WireContentBlock::Raw { block, .. } => {
                if block.get("cache_control").is_some() {
                    block["cache_control"] = serde_json::json!({"type":"ephemeral", "ttl":"1h"});
                }
            }
            WireContentBlock::Thinking { .. } => {}
        }
    }
    Ok(adapter)
}
