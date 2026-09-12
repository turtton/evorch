use super::super::tokens::{CodexTokenStore, parse_jwt_claims};
use super::{CodexQuota, QuotaConfig, QuotaError, QuotaWindow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RpcWindow {
    used_percent: f64,
    window_duration_mins: Option<i64>,
    resets_at: Option<i64>,
}

impl RpcWindow {
    fn convert(self) -> Result<Option<QuotaWindow>, QuotaError> {
        let (Some(minutes), Some(reset)) = (self.window_duration_mins, self.resets_at) else {
            return Ok(None);
        };
        let seconds = u64::try_from(minutes)
            .map_err(|_| QuotaError::Protocol("negative window duration"))?
            .checked_mul(60)
            .ok_or(QuotaError::Protocol("window duration overflow"))?;
        window(self.used_percent, seconds, reset).map(Some)
    }
}

fn window(used: f64, seconds: u64, reset: i64) -> Result<QuotaWindow, QuotaError> {
    if !used.is_finite() || used < 0.0 || seconds == 0 {
        return Err(QuotaError::Protocol("invalid quota window"));
    }
    Ok(QuotaWindow {
        used_percent: used,
        remaining_percent: (100.0 - used).clamp(0.0, 100.0),
        window_duration: Duration::from_secs(seconds),
        resets_at: DateTime::from_timestamp(reset, 0)
            .ok_or(QuotaError::Protocol("invalid reset timestamp"))?,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RpcLimits {
    plan_type: Option<String>,
    primary: Option<RpcWindow>,
    secondary: Option<RpcWindow>,
}

impl RpcLimits {
    pub(super) fn convert(self, plan: Option<String>) -> Result<CodexQuota, QuotaError> {
        Ok(CodexQuota {
            plan: self.plan_type.or(plan),
            primary: self.primary.map(RpcWindow::convert).transpose()?.flatten(),
            secondary: self
                .secondary
                .map(RpcWindow::convert)
                .transpose()?
                .flatten(),
            code_review: None,
        })
    }
}

#[derive(Deserialize)]
struct WhamWindow {
    used_percent: f64,
    limit_window_seconds: u64,
    reset_at: Option<i64>,
    reset_after_seconds: Option<i64>,
}

impl WhamWindow {
    fn convert(self) -> Result<QuotaWindow, QuotaError> {
        let now: DateTime<Utc> = SystemTime::now().into();
        let reset = self
            .reset_at
            .or_else(|| {
                self.reset_after_seconds
                    .and_then(|s| now.timestamp().checked_add(s))
            })
            .ok_or(QuotaError::Protocol("missing reset timestamp"))?;
        window(self.used_percent, self.limit_window_seconds, reset)
    }
}

#[derive(Default, Deserialize)]
struct WhamLimits {
    primary_window: Option<WhamWindow>,
    secondary_window: Option<WhamWindow>,
}

#[derive(Deserialize)]
struct WhamResponse {
    plan_type: Option<String>,
    rate_limit: Option<WhamLimits>,
    // Additional limits are intentionally ignored: the public model has only
    // primary/secondary and legacy code review, not arbitrary limit identifiers.
    code_review_rate_limit: Option<WhamWindow>,
}

pub(super) async fn fetch_wham(
    http: &reqwest::Client,
    config: &QuotaConfig,
    store: &dyn CodexTokenStore,
) -> Result<CodexQuota, QuotaError> {
    let token = store
        .load()
        .map_err(|_| QuotaError::Credentials)?
        .ok_or(QuotaError::Credentials)?;
    let claims = parse_jwt_claims(&token.id_token).map_err(|_| QuotaError::Credentials)?;
    // A separate session manager would race the chat manager's refresh-token
    // rotation/cache. Keep this reader passive until a shared manager is injected.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| QuotaError::Credentials)?;
    if claims.exp <= now.as_secs() {
        return Err(QuotaError::ReauthenticationRequired);
    }
    let response = http
        .get(&config.wham_endpoint)
        .bearer_auth(&token.access_token)
        .header("chatgpt-account-id", claims.chatgpt_account_id)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                QuotaError::Timeout
            } else {
                QuotaError::HttpTransport
            }
        })?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(QuotaError::ReauthenticationRequired);
    }
    if !response.status().is_success() {
        return Err(QuotaError::HttpStatus(response.status().as_u16()));
    }
    let raw: WhamResponse = response.json().await.map_err(|error| {
        if error.is_timeout() {
            QuotaError::Timeout
        } else {
            QuotaError::Protocol("invalid WHAM response")
        }
    })?;
    let limits = raw.rate_limit.unwrap_or_default();
    Ok(CodexQuota {
        plan: raw.plan_type,
        primary: limits.primary_window.map(WhamWindow::convert).transpose()?,
        secondary: limits
            .secondary_window
            .map(WhamWindow::convert)
            .transpose()?,
        code_review: raw
            .code_review_rate_limit
            .map(WhamWindow::convert)
            .transpose()?,
    })
}
