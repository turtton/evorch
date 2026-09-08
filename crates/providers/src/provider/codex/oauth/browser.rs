//! Desktop browser PKCE uses the full Codex CLI scope set (`CODEX_SCOPE`),
//! including connector scopes, to preserve CLI parity rather than a reduced GUI grant.

use std::io;

use crate::ProviderError;
use crate::http::build_http_client;
use crate::provider::codex::tokens::TokenBundle;

use super::callback::CallbackServer;
use super::device::exchange_authorization_code;
use super::{CODEX_CLIENT_ID, CODEX_SCOPE, PKCE_CHALLENGE_METHOD, PkcePair};

#[derive(Debug, thiserror::Error)]
pub enum BrowserAuthError {
    #[error("Codex callback ports are busy")]
    CallbackPortBusy,
    #[error("Codex browser sign-in timed out")]
    Timeout,
    #[error("Codex callback rejected")]
    Rejected,
    #[error("Codex callback I/O failed")]
    Io(#[from] io::Error),
    #[error("Codex authorization URL is invalid")]
    InvalidUrl,
    #[error("Codex OAuth request failed")]
    Provider(#[from] ProviderError),
}

pub struct BrowserAuthClient {
    auth_base_url: String,
    http: reqwest::Client,
}

pub struct BrowserAuthRequest {
    pub authorize_url: String,
    pub state: String,
    redirect_uri: String,
    pkce: PkcePair,
}

impl BrowserAuthRequest {
    pub fn code_verifier(&self) -> &str {
        &self.pkce.verifier
    }
}

impl BrowserAuthClient {
    pub fn with_default_http(auth_base_url: impl Into<String>) -> Result<Self, ProviderError> {
        Ok(Self {
            auth_base_url: auth_base_url.into().trim_end_matches('/').to_owned(),
            http: build_http_client(None)?,
        })
    }

    pub fn begin(&self, callback: &CallbackServer) -> Result<BrowserAuthRequest, BrowserAuthError> {
        let pkce = PkcePair::generate()?;
        let state = PkcePair::generate()?.verifier;
        let redirect_uri = callback.redirect_uri();
        let url = reqwest::Url::parse_with_params(
            &format!("{}/oauth/authorize", self.auth_base_url),
            &[
                ("client_id", CODEX_CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", &redirect_uri),
                ("scope", CODEX_SCOPE),
                ("code_challenge", &pkce.challenge),
                ("code_challenge_method", PKCE_CHALLENGE_METHOD),
                ("state", &state),
                ("codex_cli_simplified_flow", "true"),
                ("id_token_add_organizations", "true"),
                ("originator", "evorch"),
            ],
        )
        .map_err(|_| BrowserAuthError::InvalidUrl)?;
        Ok(BrowserAuthRequest {
            authorize_url: url.into(),
            state,
            redirect_uri,
            pkce,
        })
    }

    pub async fn complete(
        &self,
        request: BrowserAuthRequest,
        code: &str,
    ) -> Result<TokenBundle, BrowserAuthError> {
        Ok(exchange_authorization_code(
            &self.http,
            &self.auth_base_url,
            code,
            &request.redirect_uri,
            &request.pkce.verifier,
        )
        .await?)
    }
}
