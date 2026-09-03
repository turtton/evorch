//! Codex subscription のセッショントークン解決を提供します。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use super::tokens::{CodexTokenStore, needs_refresh, parse_jwt_claims};
use crate::error::ProviderError;

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    chatgpt_account_id: String,
}

/// Codex backend リクエストに使用できる認証情報。
#[derive(Clone)]
pub struct AuthorizedCodexToken {
    pub(crate) access_token: String,
    pub(crate) chatgpt_account_id: String,
}

/// 永続化された Codex token bundle をリクエスト用認証情報へ解決します。
pub struct CodexSessionManager {
    store: Arc<dyn CodexTokenStore>,
    cache: Mutex<Option<Arc<CachedToken>>>,
}

impl CodexSessionManager {
    /// token store を使用するセッションマネージャを生成します。
    #[must_use]
    pub fn new(store: Arc<dyn CodexTokenStore>) -> Self {
        Self {
            store,
            cache: Mutex::new(None),
        }
    }

    /// 現在の token bundle を検証し、Codex backend 用認証情報を返します。
    ///
    /// T6 で期限切れ token の refresh を接続するまでは、更新が必要な token を
    /// 明示的なエラーとして返します。
    ///
    /// # Errors
    /// token bundle の欠落、ID token の不正、期限切れ、時計取得失敗時に返します。
    pub async fn current(&self) -> Result<AuthorizedCodexToken, ProviderError> {
        let mut cache = self.cache.lock().await;
        if let Some(token) = cache.as_ref() {
            return Ok(AuthorizedCodexToken {
                access_token: token.access_token.clone(),
                chatgpt_account_id: token.chatgpt_account_id.clone(),
            });
        }

        let bundle = self.store.load()?.ok_or_else(|| {
            ProviderError::Request(
                "codex token bundle missing; authenticate the Codex subscription first".to_string(),
            )
        })?;
        let claims = parse_jwt_claims(&bundle.id_token)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| ProviderError::Request(format!("system clock is invalid: {error}")))?
            .as_secs();
        if needs_refresh(now, claims.exp) {
            return Err(ProviderError::Request(
                "codex token expired, refresh required".to_string(),
            ));
        }

        let token = Arc::new(CachedToken {
            access_token: bundle.access_token,
            chatgpt_account_id: claims.chatgpt_account_id,
        });
        *cache = Some(token.clone());
        Ok(AuthorizedCodexToken {
            access_token: token.access_token.clone(),
            chatgpt_account_id: token.chatgpt_account_id.clone(),
        })
    }
}
