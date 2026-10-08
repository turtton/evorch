//! Claude subscription OAuth, token refresh and quota. API keys use AnthropicClient.

pub(super) mod oauth;
mod quota;
mod tokens;

pub use oauth::{
    CLAUDE_CLIENT_ID, CLAUDE_SCOPE, ClaudeLoginRequest, ClaudeOAuthClient, ClaudeOAuthConfig,
    ClaudeOAuthError,
};
pub use quota::{ClaudeExtraUsage, ClaudeQuotaClient, ClaudeQuotaSnapshot, ClaudeQuotaWindow};
pub use tokens::{ClaudeTokenBundle, ClaudeTokenStore, InMemoryClaudeTokenStore};

use super::anthropic::{AnthropicClient, AnthropicConfig};
use crate::stream::DeltaStream;
use crate::wire::anthropic::{WireContentBlock, WireMessagesRequest};
use crate::{
    ChatRequest, ChatResponse, ContentBlock, ProviderAuth, ProviderCapabilities, ProviderClient,
    ProviderError, StreamEvent,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub const CLAUDE_USER_AGENT: &str = "claude-cli/2.1.280 (external, cli)";
pub const CLAUDE_INFERENCE_BETAS: &str =
    "claude-code-20250219,oauth-2025-04-20,prompt-caching-scope-2026-01-05";
pub const CLAUDE_SYSTEM_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

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
                .with_claude_oauth()
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
        reject_reasoning_controls(request)?;
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
        reject_reasoning_controls(request)?;
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

fn reject_reasoning_controls(request: &ChatRequest) -> Result<(), ProviderError> {
    if request.reasoning_effort.is_some() {
        Err(ProviderError::Request(
            "Claude OAuth reasoning controls require signed thinking support".into(),
        ))
    } else {
        Ok(())
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

fn prefix_tool(name: &str) -> String {
    if matches!(
        name,
        "web_search" | "code_execution" | "text_editor" | "computer"
    ) {
        name.into()
    } else {
        format!("_{name}")
    }
}

pub(super) fn shape_oauth_request(wire: &mut WireMessagesRequest) {
    let system = wire.system.get_or_insert_with(Vec::new);
    system.insert(
        0,
        WireContentBlock::Text {
            text: CLAUDE_SYSTEM_IDENTITY.into(),
            cache_control: None,
        },
    );
    for tool in &mut wire.tools {
        tool.name = prefix_tool(&tool.name);
    }
    for message in &mut wire.messages {
        for block in &mut message.content {
            if let WireContentBlock::ToolUse { name, .. } = block {
                *name = prefix_tool(name);
            }
        }
    }
}

pub(super) fn restore_response_tool_names(response: &mut ChatResponse) {
    for block in &mut response.message.content {
        if let ContentBlock::ToolUse { name, .. } = block {
            strip_tool_prefix(name);
        }
    }
}
fn strip_tool_prefix(name: &mut String) {
    if let Some(unprefixed) = name.strip_prefix('_') {
        *name = unprefixed.into();
    }
}
pub(super) fn restore_event_tool_names(event: &mut StreamEvent) {
    match event {
        StreamEvent::ToolCallDelta {
            name: Some(name), ..
        } => strip_tool_prefix(name),
        StreamEvent::Completed { response } => restore_response_tool_names(response),
        _ => {}
    }
}
