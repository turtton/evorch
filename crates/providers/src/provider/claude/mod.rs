//! Claude subscription OAuth, token refresh and quota. API keys use AnthropicClient.

mod fingerprint;
pub(super) mod oauth;
mod profile;
mod quota;
mod request;
mod tokens;
mod tool_adapter;

pub use oauth::{
    CLAUDE_CLIENT_ID, CLAUDE_SCOPE, ClaudeLoginRequest, ClaudeOAuthClient, ClaudeOAuthConfig,
    ClaudeOAuthError,
};
pub use profile::{CLAUDE_INFERENCE_BETAS, CLAUDE_SYSTEM_IDENTITY, CLAUDE_USER_AGENT};
pub(super) use profile::{ClaudeReasoningControls, ClaudeWireProfile};
pub use quota::{ClaudeExtraUsage, ClaudeQuotaClient, ClaudeQuotaSnapshot, ClaudeQuotaWindow};
pub(super) use request::serialize_request;
pub use tokens::{ClaudeTokenBundle, ClaudeTokenStore, InMemoryClaudeTokenStore};
pub(super) use tool_adapter::ClaudeToolAdapter;

use super::anthropic::{AnthropicClient, AnthropicConfig};
use crate::stream::DeltaStream;
use crate::wire::anthropic::{WireContentBlock, WireMessagesRequest};
use crate::{
    ChatRequest, ChatResponse, ProviderAuth, ProviderCapabilities, ProviderClient, ProviderError,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// Account-scoped subscription client. The token store owns all persistence.
pub struct ClaudeClient {
    inner: AnthropicClient,
    store: Arc<dyn ClaudeTokenStore>,
    oauth: ClaudeOAuthClient,
}

impl ClaudeClient {
    pub fn new(
        config: AnthropicConfig,
        store: Arc<dyn ClaudeTokenStore>,
    ) -> Result<Self, ProviderError> {
        // A broken credential must not prevent composing other usable profiles.
        // Authentication reports the stored-credential error when this client is used.
        let account_id = store
            .load()
            .ok()
            .flatten()
            .and_then(|bundle| bundle.account_id);
        Ok(Self {
            inner: AnthropicClient::new(config)?
                .with_claude_oauth()?
                .with_claude_account_id(account_id),
            store,
            oauth: ClaudeOAuthClient::new(ClaudeOAuthConfig::default())
                .map_err(provider_auth_error)?,
        })
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.inner = self.inner.with_profile(profile);
        self
    }

    pub fn with_oauth_config(mut self, config: ClaudeOAuthConfig) -> Result<Self, ProviderError> {
        self.oauth = ClaudeOAuthClient::new(config).map_err(provider_auth_error)?;
        Ok(self)
    }

    pub async fn access_token(&self) -> Result<ClaudeTokenBundle, ProviderError> {
        ensure_fresh(self.store.as_ref(), &self.oauth).await
    }
}

/// JSON identity follows omp's anthropic-identity.ts. No credential participates in it.
/// Device identity is stable for a configured account/profile; session identity follows the run.
pub(super) fn request_metadata(
    account_id: Option<&str>,
    profile: Option<&str>,
    base_url: &str,
    fallback_session: &str,
    run_id: Option<&str>,
) -> serde_json::Value {
    let mut device_hash = Sha256::new();
    for part in [
        "evorch-claude-device-v1",
        base_url,
        profile.unwrap_or(""),
        account_id.unwrap_or(""),
    ] {
        device_hash.update(part.as_bytes());
        device_hash.update([0]);
    }
    let device_id = device_hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let session_id = run_id.filter(|id| !id.is_empty()).map_or_else(
        || fallback_session.into(),
        |id| {
            let hash = Sha256::digest(format!("evorch-claude-session-v1\0{id}"));
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&hash[..16]);
            bytes[6] = (bytes[6] & 0x0f) | 0x40;
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            uuid::Uuid::from_bytes(bytes).to_string()
        },
    );
    let mut identity = serde_json::json!({"device_id": device_id, "session_id": session_id});
    if let Some(account_id) = account_id.filter(|id| !id.trim().is_empty()) {
        identity["account_uuid"] = account_id.into();
    }
    serde_json::json!({"user_id":identity.to_string()})
}

#[async_trait]
impl ProviderClient for ClaudeClient {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let bundle = self.access_token().await?;
        self.inner
            .send(&ProviderAuth::new(bundle.access_token), request)
            .await
    }
    async fn stream(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        let bundle = self.access_token().await?;
        self.inner
            .stream(&ProviderAuth::new(bundle.access_token), request)
            .await
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
    let bundle = store
        .load()?
        .ok_or_else(|| ProviderError::Request("Claude login required".into()))?;
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
