use std::time::Duration;

use reqwest::Response;
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::http::{map_request_error, map_response_error};
use crate::provider::codex::tokens::TokenBundle;

/// Codex OAuth の公開クライアント ID。
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Codex OAuth が要求する scope。
pub const CODEX_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// ユーザーが device code を入力する URL。
pub const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
/// device code の authorization code exchange に使う redirect URI。
pub const DEVICE_REDIRECT_URI: &str = DEVICE_VERIFICATION_URL;

/// Codex device OAuth HTTP クライアント。
pub struct DeviceAuthClient {
    auth_base_url: String,
    http: reqwest::Client,
}

/// device authorization のユーザーコード応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCodeResponse {
    /// polling に使う device authorization ID。
    pub device_auth_id: String,
    /// ユーザーが verification URL へ入力するコード。
    pub user_code: String,
    /// サーバー指定の polling 間隔。
    pub interval: Duration,
    /// ユーザーが開く固定 verification URL。
    pub verification_url: &'static str,
}

/// device polling で得た authorization code とサーバー発行 verifier。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCodeBundle {
    /// token exchange に使う authorization code。
    pub authorization_code: String,
    /// token exchange に使うサーバー発行 PKCE verifier。
    pub code_verifier: String,
}

/// device authorization polling の時間設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollOptions {
    /// サーバー指定間隔を上書きする polling 間隔。
    pub interval_override: Option<Duration>,
    /// polling 全体の上限時間。
    pub timeout: Duration,
}

#[derive(Serialize)]
struct UserCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Deserialize)]
struct RawUserCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: String,
}

#[derive(Serialize)]
struct AgentCodeRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct RawAgentCodeResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct ExchangeRequest<'a> {
    grant_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

impl DeviceAuthClient {
    /// 指定 OAuth base URL と構築済み HTTP client から生成する。
    pub fn new(auth_base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            auth_base_url: auth_base_url.into().trim_end_matches('/').to_owned(),
            http,
        }
    }

    /// device authorization 用のユーザーコードを要求する。
    ///
    /// # Errors
    /// transport、HTTP status、JSON、または interval の解析失敗を返す。
    pub async fn request_user_code(&self) -> Result<UserCodeResponse, ProviderError> {
        let response = self
            .http
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.auth_base_url
            ))
            .json(&UserCodeRequest {
                client_id: CODEX_CLIENT_ID,
            })
            .send()
            .await
            .map_err(map_request_error)?;
        let raw: RawUserCodeResponse = parse_success_json(response).await?;
        let seconds = raw
            .interval
            .parse::<u64>()
            .map_err(|error| ProviderError::InvalidJson {
                detail: format!("device authorization interval の解析に失敗しました: {error}"),
            })?;
        Ok(UserCodeResponse {
            device_auth_id: raw.device_auth_id,
            user_code: raw.user_code,
            interval: Duration::from_secs(seconds),
            verification_url: DEVICE_VERIFICATION_URL,
        })
    }

    /// authorization code が発行されるまで device endpoint を polling する。
    ///
    /// # Errors
    /// timeout、transport、pending 以外の HTTP status、または JSON 解析失敗を返す。
    pub async fn poll_agent_code(
        &self,
        resp: &UserCodeResponse,
        opts: &PollOptions,
    ) -> Result<AgentCodeBundle, ProviderError> {
        let interval = opts.interval_override.unwrap_or(resp.interval);
        tokio::time::timeout(opts.timeout, async {
            loop {
                let response = self
                    .http
                    .post(format!(
                        "{}/api/accounts/deviceauth/token",
                        self.auth_base_url
                    ))
                    .json(&AgentCodeRequest {
                        device_auth_id: &resp.device_auth_id,
                        user_code: &resp.user_code,
                    })
                    .send()
                    .await
                    .map_err(map_request_error)?;
                let status = response.status();
                if status.is_success() {
                    let raw: RawAgentCodeResponse = parse_json(response).await?;
                    return Ok(AgentCodeBundle {
                        authorization_code: raw.authorization_code,
                        code_verifier: raw.code_verifier,
                    });
                }
                if status.as_u16() == 403 {
                    let body = response.text().await.map_err(map_request_error)?;
                    match serde_json::from_str::<OAuthErrorResponse>(&body) {
                        Ok(error) if error.error == "authorization_pending" => {
                            tokio::time::sleep(interval).await;
                            continue;
                        }
                        Ok(_) | Err(_) => {
                            return Err(ProviderError::Http { status: 403, body });
                        }
                    }
                }
                return Err(map_response_error(response).await);
            }
        })
        .await
        .map_err(|_| ProviderError::Timeout)?
    }

    /// authorization code を token bundle へ交換する。
    ///
    /// # Errors
    /// transport、HTTP status、または JSON 解析失敗を返す。
    pub async fn exchange_code(
        &self,
        code: &AgentCodeBundle,
    ) -> Result<TokenBundle, ProviderError> {
        let response = self
            .http
            .post(format!("{}/oauth/token", self.auth_base_url))
            .form(&ExchangeRequest {
                grant_type: "authorization_code",
                code: &code.authorization_code,
                redirect_uri: DEVICE_REDIRECT_URI,
                client_id: CODEX_CLIENT_ID,
                code_verifier: &code.code_verifier,
            })
            .send()
            .await
            .map_err(map_request_error)?;
        parse_success_json(response).await
    }

    /// refresh token を使って token bundle を更新する。
    ///
    /// 応答で省略された token は現在値を維持する。
    ///
    /// # Errors
    /// transport、HTTP status、または JSON 解析失敗を返す。
    pub async fn refresh(&self, current: &TokenBundle) -> Result<TokenBundle, ProviderError> {
        let response = self
            .http
            .post(format!("{}/oauth/token", self.auth_base_url))
            .json(&RefreshRequest {
                client_id: CODEX_CLIENT_ID,
                grant_type: "refresh_token",
                refresh_token: &current.refresh_token,
            })
            .send()
            .await
            .map_err(map_request_error)?;
        let rotated: RefreshResponse = parse_success_json(response).await?;
        Ok(TokenBundle {
            access_token: rotated
                .access_token
                .unwrap_or_else(|| current.access_token.clone()),
            refresh_token: rotated
                .refresh_token
                .unwrap_or_else(|| current.refresh_token.clone()),
            id_token: rotated.id_token.unwrap_or_else(|| current.id_token.clone()),
        })
    }
}

async fn parse_success_json<T: for<'de> Deserialize<'de>>(
    response: Response,
) -> Result<T, ProviderError> {
    if response.status().is_success() {
        parse_json(response).await
    } else {
        Err(map_response_error(response).await)
    }
}

async fn parse_json<T: for<'de> Deserialize<'de>>(response: Response) -> Result<T, ProviderError> {
    let bytes = response.bytes().await.map_err(map_request_error)?;
    serde_json::from_slice(&bytes).map_err(|error| ProviderError::InvalidJson {
        detail: error.to_string(),
    })
}
