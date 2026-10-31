//! Codex subscription のセッショントークン解決を提供します。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use super::oauth::{DeviceAuthClient, REFRESH_REASON_PROACTIVE, REFRESH_REASON_UNAUTHORIZED};
use super::tokens::{CodexTokenStore, TokenBundle, needs_refresh, parse_jwt_claims, parse_jwt_exp};
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
    /// 更新要否は access token の `exp` で判断し、JWT として読めないときだけ
    /// ID token の `exp` に戻します。account id は常に ID token から取ります。
    ///
    /// # Errors
    /// token bundle の欠落、ID token の不正、更新、永続化、時計取得失敗時に返します。
    pub async fn current(&self) -> Result<AuthorizedCodexToken, ProviderError> {
        let now = current_unix_time()?;
        {
            let cache = self.cache.lock().await;
            if let Some(bundle) = cache.as_ref() {
                let (account_id, expiring) = inspect(bundle, now)?;
                if !expiring {
                    return Ok(authorized_token(bundle, account_id));
                }
            }
        }

        let mut cache = self.cache.lock().await;
        let bundle = self.load_bundle()?;
        let (_, expiring) = inspect(&bundle, current_unix_time()?)?;
        let current = if expiring {
            let refreshed = self
                .device_auth
                .refresh_with_reason(&bundle, REFRESH_REASON_PROACTIVE)
                .await?;
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

    /// backend が 401 で拒否した認証情報から回復します。
    ///
    /// cache の Mutex を保持したまま実行するため、同時に 401 を受けた複数の
    /// リクエストがあっても refresh は 1 回だけです。store の access token が
    /// `rejected` と異なれば (他のリクエスト・プロセスが更新済み) ネットワークを
    /// 使わずにそれを返します。
    ///
    /// # Errors
    /// token bundle の欠落、更新・永続化の失敗、および refresh token が拒否され
    /// store にも新しい認証情報がない (再認証が必要な) 場合に返します。
    pub async fn refresh_after_unauthorized(
        &self,
        rejected: &AuthorizedCodexToken,
    ) -> Result<AuthorizedCodexToken, ProviderError> {
        let mut cache = self.cache.lock().await;
        let bundle = self.load_bundle()?;
        if bundle.access_token != rejected.access_token {
            return self.adopt(&mut cache, bundle);
        }
        match self
            .device_auth
            .refresh_with_reason(&bundle, REFRESH_REASON_UNAUTHORIZED)
            .await
        {
            Ok(refreshed) => {
                self.store.save(&refreshed)?;
                self.adopt(&mut cache, refreshed)
            }
            Err(ProviderError::Http { status, .. }) if (400..500).contains(&status) => {
                match self.store.load()? {
                    Some(stored) if stored != bundle => self.adopt(&mut cache, stored),
                    _ => Err(ProviderError::Http {
                        status: 401,
                        body: "codex refresh token was rejected; re-authenticate the Codex subscription"
                            .to_string(),
                    }),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn load_bundle(&self) -> Result<TokenBundle, ProviderError> {
        self.store.load()?.ok_or_else(|| {
            ProviderError::Request(
                "codex token bundle missing; authenticate the Codex subscription first".to_string(),
            )
        })
    }

    fn adopt(
        &self,
        cache: &mut Option<TokenBundle>,
        bundle: TokenBundle,
    ) -> Result<AuthorizedCodexToken, ProviderError> {
        let account_id = parse_jwt_claims(&bundle.id_token)?.chatgpt_account_id;
        let token = authorized_token(&bundle, account_id);
        *cache = Some(bundle);
        Ok(token)
    }
}

/// bundle の account id と、事前 refresh が必要かを返す。
///
/// access token の `exp` を優先する。access token が JWT として読めないとき
/// (不透明 token・exp 欠落・復号失敗) だけ ID token の `exp` に戻す。
fn inspect(bundle: &TokenBundle, now: u64) -> Result<(String, bool), ProviderError> {
    let claims = parse_jwt_claims(&bundle.id_token)?;
    let exp = parse_jwt_exp(&bundle.access_token)
        .unwrap_or(None)
        .unwrap_or(claims.exp);
    Ok((claims.chatgpt_account_id, needs_refresh(now, exp)))
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

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    const NOW: u64 = 1_000_000;

    fn jwt(exp: u64) -> String {
        let payload = serde_json::json!({
            "exp": exp,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc"}
        });
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(payload.to_string()))
    }

    fn bundle(access_token: String, id_exp: u64) -> TokenBundle {
        TokenBundle {
            access_token,
            refresh_token: "refresh".into(),
            id_token: jwt(id_exp),
        }
    }

    fn expiring(access_token: String, id_exp: u64) -> bool {
        inspect(&bundle(access_token, id_exp), NOW)
            .expect("bundle is readable")
            .1
    }

    // Given: access / ID token の期限の組 / When: inspect / Then: 判断は access token の exp に従う
    #[test]
    fn refresh_decision_follows_the_access_token_expiry() {
        let (fresh, stale) = (NOW + 3_600, NOW + 60);

        assert!(!expiring(jwt(fresh), fresh));
        assert!(
            expiring(jwt(stale), fresh),
            "ID token が有効でも access token が失効間近なら更新"
        );
        assert!(
            !expiring(jwt(fresh), stale),
            "access token が有効なら ID token の期限は無視"
        );
        assert!(expiring(jwt(stale), stale));
    }

    // Given: access token の exp が now+300 / now+301 / When: inspect / Then: 300 秒は更新、301 秒は更新しない
    #[test]
    fn access_token_refresh_window_boundary() {
        let id_exp = NOW + 3_600;

        assert!(expiring(jwt(NOW + 300), id_exp));
        assert!(!expiring(jwt(NOW + 301), id_exp));
    }

    // Given: JWT として読めない access token / When: inspect / Then: ID token の exp (300 / 301 秒) に戻る
    #[test]
    fn unreadable_access_token_falls_back_to_the_id_token_expiry() {
        for opaque in ["opaque-token", "a.!!!!.c", "a.e30.c"] {
            assert!(expiring(opaque.into(), NOW + 300), "{opaque}");
            assert!(!expiring(opaque.into(), NOW + 301), "{opaque}");
        }
    }

    // Given: 有効な access token と不正な ID token / When: inspect / Then: account id が取れずエラー
    #[test]
    fn account_id_is_always_read_from_the_id_token() {
        let broken = TokenBundle {
            access_token: jwt(NOW + 3_600),
            refresh_token: "refresh".into(),
            id_token: "garbage".into(),
        };

        assert!(inspect(&broken, NOW).is_err());
    }
}
