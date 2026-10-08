//! Cursor PKCE browser login. Protocol reference: oh-my-pi 602b6c8, OMP-LICENSE.
use super::{CursorTokenBundle, unix_now};
use crate::{ProviderError, http::map_request_error};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fmt, time::Duration};

pub const CURSOR_CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
#[derive(Debug, Clone)]
pub struct CursorOAuthConfig {
    pub login_url: String,
    pub poll_url: String,
    pub token_url: String,
    pub profile_url: String,
}
impl Default for CursorOAuthConfig {
    fn default() -> Self {
        Self {
            login_url: "https://cursor.com/loginDeepControl".into(),
            poll_url: "https://api2.cursor.sh/auth/poll".into(),
            token_url: "https://api2.cursor.sh/oauth/token".into(),
            profile_url: "https://cursor.com/api/auth/me".into(),
        }
    }
}
pub struct CursorLoginRequest {
    pub authorization_url: String,
    uuid: String,
    verifier: String,
}
impl fmt::Debug for CursorLoginRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CursorLoginRequest")
            .field("pkce", &"<redacted>")
            .finish()
    }
}
pub struct CursorOAuthClient {
    http: reqwest::Client,
    config: CursorOAuthConfig,
}
impl CursorOAuthClient {
    pub fn new(config: CursorOAuthConfig) -> Result<Self, ProviderError> {
        for url in [
            &config.login_url,
            &config.poll_url,
            &config.token_url,
            &config.profile_url,
        ] {
            super::validate_endpoint(url)?;
        }
        Ok(Self {
            http: super::http_client(Some(Duration::from_secs(30)))?,
            config,
        })
    }
    pub fn begin(&self) -> Result<CursorLoginRequest, ProviderError> {
        let mut random = [0; 32];
        getrandom::fill(&mut random)
            .map_err(|_| ProviderError::Request("Cannot generate Cursor PKCE verifier".into()))?;
        let verifier = URL_SAFE_NO_PAD.encode(random);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let uuid = uuid::Uuid::new_v4().to_string();
        let mut url = reqwest::Url::parse(&self.config.login_url)
            .map_err(|_| ProviderError::Request("Invalid Cursor login URL".into()))?;
        url.query_pairs_mut().extend_pairs([
            ("challenge", challenge.as_str()),
            ("uuid", uuid.as_str()),
            ("mode", "login"),
            ("redirectTarget", "cli"),
        ]);
        Ok(CursorLoginRequest {
            authorization_url: url.to_string(),
            uuid,
            verifier,
        })
    }
    /// One polling attempt. `None` means the browser login is still pending.
    pub async fn poll(
        &self,
        request: &CursorLoginRequest,
    ) -> Result<Option<CursorTokenBundle>, ProviderError> {
        let response = self
            .http
            .get(&self.config.poll_url)
            .query(&[("uuid", &request.uuid), ("verifier", &request.verifier)])
            .send()
            .await
            .map_err(|error| map_request_error(error.without_url()))?;
        if response.status() == 404 {
            return Ok(None);
        }
        let payload = auth_json(response).await?;
        let access = nonempty(&payload, "accessToken")
            .ok_or_else(|| auth_error("Cursor login returned no access token"))?;
        let refresh = nonempty(&payload, "refreshToken")
            .ok_or_else(|| auth_error("Cursor login returned no refresh token"))?;
        let mut bundle = bundle(access.into(), refresh.into());
        self.attach_email(&mut bundle).await;
        Ok(Some(bundle))
    }
    /// Dropping this future cancels login, including an outstanding poll.
    pub async fn complete(
        &self,
        request: &CursorLoginRequest,
    ) -> Result<CursorTokenBundle, ProviderError> {
        let mut delay = Duration::from_secs(1);
        let mut transient_errors = 0;
        for _ in 0..150 {
            match self.poll(request).await {
                Ok(Some(bundle)) => return Ok(bundle),
                Ok(None) => transient_errors = 0,
                Err(error)
                    if matches!(
                        error,
                        ProviderError::Transport { .. }
                            | ProviderError::Timeout
                            | ProviderError::Http {
                                status: 500..=599,
                                ..
                            }
                    ) =>
                {
                    transient_errors += 1;
                    if transient_errors >= 3 {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
            tokio::time::sleep(delay).await;
            delay = delay.mul_f64(1.2).min(Duration::from_secs(10));
        }
        Err(auth_error("Cursor browser login timed out"))
    }
    pub async fn refresh(
        &self,
        previous: &CursorTokenBundle,
    ) -> Result<CursorTokenBundle, ProviderError> {
        let response = self.http.post(&self.config.token_url).json(&json!({"grant_type":"refresh_token", "client_id":CURSOR_CLIENT_ID, "refresh_token":previous.refresh_token}))
            .send().await.map_err(|e| map_request_error(e.without_url()))?;
        let payload = auth_json(response).await?;
        if payload.get("shouldLogout").and_then(Value::as_bool) == Some(true) {
            return Err(auth_error("Cursor ended this session; log in again"));
        }
        let access = nonempty(&payload, "access_token")
            .ok_or_else(|| auth_error("Cursor refresh returned no access token"))?;
        let refresh = nonempty(&payload, "refresh_token").unwrap_or(&previous.refresh_token);
        let mut renewed = bundle(access.into(), refresh.into());
        renewed.account_id = renewed.account_id.or_else(|| previous.account_id.clone());
        renewed.email = previous.email.clone();
        if renewed.email.is_none() {
            self.attach_email(&mut renewed).await;
        }
        Ok(renewed)
    }
    async fn attach_email(&self, bundle: &mut CursorTokenBundle) {
        let Some(id) = bundle.account_id.as_deref() else {
            return;
        };
        let Ok(response) = self
            .http
            .get(&self.config.profile_url)
            .header("cookie", session_cookie(id, &bundle.access_token))
            .timeout(Duration::from_secs(3))
            .send()
            .await
        else {
            return;
        };
        if !response.status().is_success() {
            return;
        }
        let Ok(payload) = auth_json(response).await else {
            return;
        };
        if payload.get("sub").and_then(Value::as_str) == Some(id) {
            bundle.email = nonempty(&payload, "email").map(str::to_owned);
        }
    }
}
fn nonempty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
fn auth_error(message: &str) -> ProviderError {
    ProviderError::Request(message.into())
}
async fn auth_json(response: reqwest::Response) -> Result<Value, ProviderError> {
    // OAuth responses may contain credentials: never include their bodies in diagnostics.
    let status = response.status();
    if !status.is_success() {
        return Err(ProviderError::Http {
            status: status.as_u16(),
            body: "Cursor authentication failed".into(),
        });
    }
    serde_json::from_slice(&super::bounded_bytes(response, 1024 * 1024).await?)
        .map_err(|_| auth_error("Invalid Cursor authentication response"))
}
pub(super) fn token_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}
pub(super) fn account_id(token: &str) -> Option<String> {
    let claims = token_claims(token)?;
    let sub = claims.get("sub")?.as_str()?;
    Some(
        sub.split_once('|')
            .map_or(sub, |(_, id)| id)
            .trim()
            .to_owned(),
    )
    .filter(|s| !s.is_empty())
}
fn bundle(access_token: String, refresh_token: String) -> CursorTokenBundle {
    let expires_at = token_claims(&access_token)
        .and_then(|claims| claims.get("exp")?.as_u64())
        .unwrap_or_else(|| unix_now() + 3600);
    CursorTokenBundle {
        account_id: account_id(&access_token),
        access_token,
        refresh_token,
        expires_at,
        email: None,
    }
}
pub(super) fn session_cookie(id: &str, token: &str) -> String {
    // encodeURIComponent's cookie-safe subset, without form-urlencoded '+' semantics.
    let raw = format!("{id}::{token}");
    let mut encoded = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    format!("WorkosCursorSessionToken={encoded}")
}
