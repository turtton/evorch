//! provider の device OAuth と資格情報ストアを GUI の要約境界へ接続する。

use std::sync::Arc;
use std::time::Duration;

use config::{CredentialRefConfig, ProviderTypeConfig};
use providers::ProviderError;
use providers::provider::codex::oauth::{DeviceAuthClient, PollOptions, UserCodeResponse};
use providers::provider::codex::tokens::{CodexTokenStore, TokenBundle, parse_jwt_claims};
use routing::factory::CredentialStoreTokenStore;
use sandbox::CredentialStore;

use super::codex_auth::{CodexAuthBackend, CodexAuthError, CodexAuthSummary, CodexUserCodePrompt};

pub const DEFAULT_CODEX_CREDENTIAL_ACCOUNT: &str = "codex";
pub const CODEX_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub struct ProviderCodexAuthBackend {
    client: DeviceAuthClient,
    store: Arc<dyn CodexTokenStore>,
    poll: PollOptions,
}

impl ProviderCodexAuthBackend {
    pub const fn new(client: DeviceAuthClient, store: Arc<dyn CodexTokenStore>) -> Self {
        Self {
            client,
            store,
            poll: PollOptions {
                interval_override: None,
                timeout: CODEX_LOGIN_TIMEOUT,
            },
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
            DeviceAuthClient::with_default_http(auth_base_url).map_err(classify_provider_error)?;
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
        ProviderError::Timeout | ProviderError::Request(_) | ProviderError::RateLimited { .. } => {
            CodexAuthError::Network
        }
        ProviderError::Http { .. } => CodexAuthError::Rejected,
        ProviderError::InvalidJson { .. } => CodexAuthError::StoreUnavailable,
        ProviderError::InvalidSse { .. } => CodexAuthError::Unavailable,
    }
}

fn prompt_of(code: &UserCodeResponse) -> CodexUserCodePrompt {
    CodexUserCodePrompt {
        user_code: code.user_code.clone(),
        verification_url: code.verification_url.to_owned(),
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
        on_prompt: &mut (dyn FnMut(CodexUserCodePrompt) + Send),
    ) -> Result<CodexAuthSummary, CodexAuthError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| CodexAuthError::Unavailable)?;
        let mut forward = |code: &UserCodeResponse| on_prompt(prompt_of(code));
        let bundle = runtime
            .block_on(async {
                let code = self.client.request_user_code().await?;
                forward(&code);
                let agent_code = self.client.poll_agent_code(&code, &self.poll).await?;
                self.client.exchange_code(&agent_code).await
            })
            .map_err(classify_provider_error)?;
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
