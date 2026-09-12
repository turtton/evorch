//! provider の browser OAuth と資格情報ストアを GUI の要約境界へ接続する。

use std::sync::Arc;
use std::time::Duration;

use config::{CredentialRefConfig, ProviderTypeConfig};
use providers::ProviderError;
use providers::provider::codex::oauth::{BrowserAuthClient, BrowserAuthError, CallbackServer};
use providers::provider::codex::tokens::{CodexTokenStore, TokenBundle, parse_jwt_claims};
use routing::factory::CredentialStoreTokenStore;
use sandbox::CredentialStore;

use super::codex_auth::{CodexAuthBackend, CodexAuthError, CodexAuthSummary, CodexLoginPrompt};

pub const DEFAULT_CODEX_CREDENTIAL_ACCOUNT: &str = "codex";
pub const CODEX_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub struct ProviderCodexAuthBackend {
    client: BrowserAuthClient,
    store: Arc<dyn CodexTokenStore>,
    callback_ports: Vec<u16>,
}

impl ProviderCodexAuthBackend {
    pub fn new(client: BrowserAuthClient, store: Arc<dyn CodexTokenStore>) -> Self {
        Self {
            client,
            store,
            callback_ports: vec![1455, 1457],
        }
    }

    /// 本番の HTTP クライアントと資格情報ストアを接続する。
    ///
    /// # Errors
    /// HTTP クライアントの構築失敗を返す。
    pub fn production(
        credential_store: Arc<dyn CredentialStore>,
        account: String,
        auth_base_url: &str,
    ) -> Result<Self, CodexAuthError> {
        let client =
            BrowserAuthClient::with_default_http(auth_base_url).map_err(classify_provider_error)?;
        let store = Arc::new(CredentialStoreTokenStore::new(credential_store, account));
        Ok(Self::new(client, store))
    }
}

fn summary_of(bundle: &TokenBundle) -> Result<CodexAuthSummary, CodexAuthError> {
    let claims =
        parse_jwt_claims(&bundle.id_token).map_err(|_| CodexAuthError::StoreUnavailable)?;
    Ok(CodexAuthSummary {
        expires_at_unix: Some(claims.exp),
    })
}

fn classify_provider_error(error: ProviderError) -> CodexAuthError {
    match error {
        ProviderError::Timeout => CodexAuthError::Timeout,
        ProviderError::Request(_)
        | ProviderError::Transport { .. }
        | ProviderError::RetriesExhausted { .. }
        | ProviderError::RateLimited { .. } => CodexAuthError::Network,
        ProviderError::Http { .. } => CodexAuthError::Rejected,
        ProviderError::InvalidJson { .. } => CodexAuthError::StoreUnavailable,
        ProviderError::InvalidSse { .. } => CodexAuthError::Unavailable,
    }
}

fn classify_browser_error(error: BrowserAuthError) -> CodexAuthError {
    match error {
        BrowserAuthError::CallbackPortBusy => CodexAuthError::CallbackPortBusy,
        BrowserAuthError::Timeout => CodexAuthError::Timeout,
        BrowserAuthError::Rejected => CodexAuthError::Rejected,
        BrowserAuthError::Io(_) | BrowserAuthError::InvalidUrl => CodexAuthError::Unavailable,
        BrowserAuthError::Provider(error) => classify_provider_error(error),
    }
}

impl CodexAuthBackend for ProviderCodexAuthBackend {
    fn load_summary(&self) -> Result<Option<CodexAuthSummary>, CodexAuthError> {
        self.store
            .load()
            .map_err(|_| CodexAuthError::StoreUnavailable)?
            .as_ref()
            .map(summary_of)
            .transpose()
    }

    fn authenticate(
        &self,
        on_prompt: &mut (dyn FnMut(CodexLoginPrompt) + Send),
    ) -> Result<CodexAuthSummary, CodexAuthError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| CodexAuthError::Unavailable)?;
        let callback =
            CallbackServer::bind_ports(&self.callback_ports).map_err(classify_browser_error)?;
        let request = self
            .client
            .begin(&callback)
            .map_err(classify_browser_error)?;
        on_prompt(CodexLoginPrompt {
            authorize_url: request.authorize_url.clone(),
        });
        let code = callback
            .wait_for_code(&request.state, CODEX_LOGIN_TIMEOUT)
            .map_err(classify_browser_error)?;
        let bundle = runtime
            .block_on(self.client.complete(request, &code))
            .map_err(classify_browser_error)?;
        let summary = summary_of(&bundle)?;
        self.store
            .save(&bundle)
            .map_err(|_| CodexAuthError::StoreUnavailable)?;
        Ok(summary)
    }
}

/// 最初の Codex keyring プロファイルから保存先アカウントを取得する。
pub fn codex_credential_account(config: &config::Config) -> Option<String> {
    config.providers.values().find_map(|profile| {
        if profile.provider_type != ProviderTypeConfig::OpenAiCodex {
            return None;
        }
        match &profile.credential {
            CredentialRefConfig::Keyring { account, .. } => Some(account.clone()),
            CredentialRefConfig::Env { .. } => None,
        }
    })
}

#[cfg(test)]
#[path = "codex_auth_backend_tests.rs"]
mod tests;
