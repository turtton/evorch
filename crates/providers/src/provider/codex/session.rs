//! Codex subscription のセッショントークン解決を提供します。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use super::oauth::DeviceAuthClient;
use super::tokens::{CodexTokenStore, TokenBundle, needs_refresh, parse_jwt_claims};
use crate::error::ProviderError;

/// Codex backend リクエストに使用できる認証情報。
#[derive(Clone)]
pub struct AuthorizedCodexToken {
    pub(crate) access_token: String,
    pub(crate) chatgpt_account_id: String,
}

/// 永続化された Codex token bundle をリクエスト用認証情報へ解決します。
pub struct CodexSessionManager {
    store: Arc<dyn CodexTokenStore>,
    device_auth: DeviceAuthClient,
    cache: Mutex<Option<TokenBundle>>,
}

impl CodexSessionManager {
    /// token store と OAuth client を使用するセッションマネージャを生成します。
    #[must_use]
    pub fn new(store: Arc<dyn CodexTokenStore>, device_auth: DeviceAuthClient) -> Self {
        Self {
            store,
            device_auth,
            cache: Mutex::new(None),
        }
    }

    /// 現在の token bundle を検証し、Codex backend 用認証情報を返します。
    ///
    /// # Errors
    /// token bundle の欠落、ID token の不正、更新、永続化、時計取得失敗時に返します。
    pub async fn current(&self) -> Result<AuthorizedCodexToken, ProviderError> {
        let now = current_unix_time()?;
        {
            let cache = self.cache.lock().await;
            if let Some(bundle) = cache.as_ref() {
                let claims = parse_jwt_claims(&bundle.id_token)?;
                if !needs_refresh(now, claims.exp) {
                    return Ok(authorized_token(bundle, claims.chatgpt_account_id));
                }
            }
        }

        let mut cache = self.cache.lock().await;
        let bundle = self.store.load()?.ok_or_else(|| {
            ProviderError::Request(
                "codex token bundle missing; authenticate the Codex subscription first".to_string(),
            )
        })?;
        let claims = parse_jwt_claims(&bundle.id_token)?;
        let current = if needs_refresh(current_unix_time()?, claims.exp) {
            let refreshed = self.device_auth.refresh(&bundle).await?;
            self.store.save(&refreshed)?;
            refreshed
        } else {
            bundle
        };
        let account_id = parse_jwt_claims(&current.id_token)?.chatgpt_account_id;
        let token = authorized_token(&current, account_id);
        *cache = Some(current);
        Ok(token)
    }
}

fn current_unix_time() -> Result<u64, ProviderError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ProviderError::Request(format!("system clock is invalid: {error}")))
        .map(|duration| duration.as_secs())
}

fn authorized_token(bundle: &TokenBundle, chatgpt_account_id: String) -> AuthorizedCodexToken {
    AuthorizedCodexToken {
        access_token: bundle.access_token.clone(),
        chatgpt_account_id,
    }
}
