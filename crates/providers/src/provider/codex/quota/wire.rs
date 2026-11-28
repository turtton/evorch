use super::super::{
    session::AuthorizedCodexToken,
    tokens::{CodexTokenStore, parse_jwt_claims},
};
use super::{CodexQuota, QuotaConfig, QuotaError, QuotaWindow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::time::{Duration, SystemTime};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RpcWindow {
    #[serde(deserialize_with = "percentage")]
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

// Percentage anomalies are not structural window failures. Preserve the source
// value for diagnostics to mark unknown without changing the public f64 model.
fn percentage<'de, D: serde::Deserializer<'de>>(input: D) -> Result<f64, D::Error> {
    let value = serde_json::Value::deserialize(input)?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .ok_or_else(|| serde::de::Error::custom("invalid quota percentage"))
}

fn window(used: f64, seconds: u64, reset: i64) -> Result<QuotaWindow, QuotaError> {
    if seconds == 0 {
        return Err(QuotaError::Protocol("invalid quota window"));
    }
    Ok(QuotaWindow {
        used_percent: used,
        // Legacy callers require a bounded number; diagnostics reads used_percent.
        remaining_percent: if used.is_finite() && (0.0..=100.0).contains(&used) {
            100.0 - used
        } else {
            0.0
        },
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
    #[serde(deserialize_with = "percentage")]
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
    fetch_authorized(
        http,
        config,
        &AuthorizedCodexToken {
            access_token: token.access_token,
            chatgpt_account_id: claims.chatgpt_account_id,
        },
    )
    .await
}

pub(super) async fn fetch_authorized(
    http: &reqwest::Client,
    config: &QuotaConfig,
    token: &AuthorizedCodexToken,
) -> Result<CodexQuota, QuotaError> {
    let response = http
        .get(&config.wham_endpoint)
        .bearer_auth(&token.access_token)
        .header("chatgpt-account-id", &token.chatgpt_account_id)
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
