//! Native PKCE authorization-code flow, including manual callback input.
use super::{ClaudeTokenBundle, unix_now};
use crate::provider::codex::oauth::PkcePair;
use serde_json::{Value, json};
use std::{fmt, time::Duration};

/// Public client identifier published by Anthropic's Claude Code OAuth client.
pub const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_SCOPE: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

#[derive(Debug, Clone)]
pub struct ClaudeOAuthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub redirect_uri: String,
    pub timeout: Duration,
}
impl Default for ClaudeOAuthConfig {
    fn default() -> Self {
        Self {
            authorize_url: "https://claude.ai/oauth/authorize".into(),
            token_url: "https://api.anthropic.com/v1/oauth/token".into(),
            redirect_uri: "http://localhost:54545/callback".into(),
            timeout: Duration::from_secs(30),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClaudeOAuthError {
    #[error("Claude OAuth configuration is invalid")]
    Configuration,
    #[error("Claude OAuth callback rejected")]
    Callback,
    #[error("Claude OAuth request timed out")]
    Timeout,
    #[error("Claude OAuth transport failed")]
    Transport,
    #[error("Claude authentication rejected; sign in again")]
    ReauthenticationRequired,
    #[error("Claude OAuth HTTP status {0}")]
    HttpStatus(u16),
    #[error("Claude OAuth response is invalid")]
    Protocol,
}
pub struct ClaudeLoginRequest {
    pub authorize_url: String,
    pub state: String,
    redirect_uri: String,
    pkce: PkcePair,
}
impl ClaudeLoginRequest {
    pub fn code_verifier(&self) -> &str {
        &self.pkce.verifier
    }
}
impl fmt::Debug for ClaudeLoginRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeLoginRequest")
            .field("login", &"<redacted>")
            .finish()
    }
}
pub struct ClaudeOAuthClient {
    config: ClaudeOAuthConfig,
    http: reqwest::Client,
}
impl ClaudeOAuthClient {
    pub fn new(config: ClaudeOAuthConfig) -> Result<Self, ClaudeOAuthError> {
        validate_endpoint(&config.authorize_url)?;
        validate_endpoint(&config.token_url)?;
        let redirect = reqwest::Url::parse(&config.redirect_uri)
            .map_err(|_| ClaudeOAuthError::Configuration)?;
        if !matches!(redirect.scheme(), "https" | "http")
            || redirect.query().is_some()
            || redirect.fragment().is_some()
            || !redirect.username().is_empty()
            || redirect.password().is_some()
        {
            return Err(ClaudeOAuthError::Configuration);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.timeout)
            .build()
            .map_err(|_| ClaudeOAuthError::Transport)?;
        Ok(Self { config, http })
    }
    pub fn begin(&self) -> Result<ClaudeLoginRequest, ClaudeOAuthError> {
        let pkce = PkcePair::generate().map_err(|_| ClaudeOAuthError::Transport)?;
        let state = PkcePair::generate()
            .map_err(|_| ClaudeOAuthError::Transport)?
            .verifier;
        let url = reqwest::Url::parse_with_params(
            &self.config.authorize_url,
            &[
                ("client_id", CLAUDE_CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", self.config.redirect_uri.as_str()),
                ("scope", CLAUDE_SCOPE),
                ("code", "true"),
                ("code_challenge", pkce.challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("state", state.as_str()),
            ],
        )
        .map_err(|_| ClaudeOAuthError::Configuration)?;
        Ok(ClaudeLoginRequest {
            authorize_url: url.into(),
            state,
            redirect_uri: self.config.redirect_uri.clone(),
            pkce,
        })
    }
    pub async fn complete(
        &self,
        login: ClaudeLoginRequest,
        input: &str,
    ) -> Result<ClaudeTokenBundle, ClaudeOAuthError> {
        self.complete_at(login, input, unix_now()).await
    }
    /// Explicit clock makes token expiry contracts deterministic.
    pub async fn complete_at(
        &self,
        login: ClaudeLoginRequest,
        input: &str,
        now: u64,
    ) -> Result<ClaudeTokenBundle, ClaudeOAuthError> {
        let code = parse_code(input, &login)?;
        self.exchange(
            json!({ "grant_type": "authorization_code", "client_id": CLAUDE_CLIENT_ID,
            "code": code, "state": login.state, "redirect_uri": login.redirect_uri,
            "code_verifier": login.pkce.verifier }),
            None,
            now,
        )
        .await
    }
    pub async fn refresh(
        &self,
        previous: &ClaudeTokenBundle,
    ) -> Result<ClaudeTokenBundle, ClaudeOAuthError> {
        self.refresh_at(previous, unix_now()).await
    }
    pub async fn refresh_at(
        &self,
        previous: &ClaudeTokenBundle,
        now: u64,
    ) -> Result<ClaudeTokenBundle, ClaudeOAuthError> {
        if previous.refresh_token.trim().is_empty() {
            return Err(ClaudeOAuthError::ReauthenticationRequired);
        }
        self.exchange(
            json!({ "grant_type": "refresh_token", "client_id": CLAUDE_CLIENT_ID,
            "refresh_token": previous.refresh_token }),
            Some(previous),
            now,
        )
        .await
    }
    async fn exchange(
        &self,
        payload: Value,
        previous: Option<&ClaudeTokenBundle>,
        now: u64,
    ) -> Result<ClaudeTokenBundle, ClaudeOAuthError> {
        let mut request = self.http.post(&self.config.token_url).json(&payload);
        if previous.is_some() {
            request = request.header("anthropic-beta", "oauth-2025-04-20").header(
                "user-agent",
                "anthropic-sdk-typescript/0.112.1 userOAuthProvider",
            );
        }
        let response = request.send().await.map_err(transport_error)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(match status {
                400 | 401 | 403 => ClaudeOAuthError::ReauthenticationRequired,
                status => ClaudeOAuthError::HttpStatus(status),
            });
        }
        let body = bounded_json(response).await?;
        if body["token_type"]
            .as_str()
            .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
        {
            return Err(ClaudeOAuthError::Protocol);
        }
        let access_token = nonempty(&body, "access_token").ok_or(ClaudeOAuthError::Protocol)?;
        let refresh_token = nonempty(&body, "refresh_token")
            .or_else(|| previous.map(|p| p.refresh_token.clone()))
            .ok_or(ClaudeOAuthError::Protocol)?;
        let expires = body["expires_in"]
            .as_u64()
            .filter(|v| *v > 0)
            .ok_or(ClaudeOAuthError::Protocol)?;
        Ok(ClaudeTokenBundle {
            access_token,
            refresh_token,
            expires_at: now.saturating_add(expires),
            account_id: nonempty(&body["account"], "uuid")
                .or_else(|| previous.and_then(|p| p.account_id.clone())),
            email: nonempty(&body["account"], "email_address")
                .or_else(|| previous.and_then(|p| p.email.clone())),
            // Refresh must retain the subscription workspace selected at login.
            org_id: match previous {
                Some(p) => p.org_id.clone(),
                None => nonempty(&body["organization"], "uuid"),
            },
            org_name: match previous {
                Some(p) => p.org_name.clone(),
                None => nonempty(&body["organization"], "name"),
            },
        })
    }
}
fn nonempty(body: &Value, field: &str) -> Option<String> {
    body[field]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}
fn parse_code(input: &str, login: &ClaudeLoginRequest) -> Result<String, ClaudeOAuthError> {
    let input = input.trim();
    let (code, state) = if let Ok(url) = reqwest::Url::parse(input) {
        let redirect =
            reqwest::Url::parse(&login.redirect_uri).map_err(|_| ClaudeOAuthError::Callback)?;
        if url.origin() != redirect.origin() || url.path() != redirect.path() {
            return Err(ClaudeOAuthError::Callback);
        }
        let mut code = None;
        let mut state = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" if code.is_none() => code = Some(value.into_owned()),
                "state" if state.is_none() => state = Some(value.into_owned()),
                "code" | "state" | "error" => return Err(ClaudeOAuthError::Callback),
                _ => {}
            }
        }
        (
            code.ok_or(ClaudeOAuthError::Callback)?,
            Some(state.ok_or(ClaudeOAuthError::Callback)?),
        )
    } else if let Some((code, state)) = input.split_once('#') {
        (code.into(), Some(state.into()))
    } else {
        (input.into(), None)
    };
    if code.is_empty()
        || code.chars().any(char::is_whitespace)
        || state.as_deref().is_some_and(|s| s != login.state)
    {
        return Err(ClaudeOAuthError::Callback);
    }
    Ok(code)
}
pub(super) fn validate_endpoint(endpoint: &str) -> Result<reqwest::Url, ClaudeOAuthError> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| ClaudeOAuthError::Configuration)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ClaudeOAuthError::Configuration);
    }
    Ok(url)
}
pub(super) fn transport_error(error: reqwest::Error) -> ClaudeOAuthError {
    if error.is_timeout() {
        ClaudeOAuthError::Timeout
    } else {
        ClaudeOAuthError::Transport
    }
}
pub(crate) async fn bounded_json(
    mut response: reqwest::Response,
) -> Result<Value, ClaudeOAuthError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err(ClaudeOAuthError::Protocol);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ClaudeOAuthError::Protocol)
}
