use super::oauth::{bounded_json, transport_error, validate_endpoint};
use super::{CLAUDE_USER_AGENT, ClaudeTokenBundle};
use crate::provider::codex::quota::QuotaError;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeQuotaWindow {
    pub label: String,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}
/// Monetary extra usage, including plans with no spending cap.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeExtraUsage {
    pub used_usd: f64,
    pub limit_usd: Option<f64>,
}
#[derive(Debug, Clone)]
pub struct ClaudeQuotaSnapshot {
    pub windows: Vec<ClaudeQuotaWindow>,
    pub extra_usage: Option<ClaudeExtraUsage>,
    pub stale: bool,
    pub fetched_at: DateTime<Utc>,
    pub last_error: Option<QuotaError>,
}
pub struct ClaudeQuotaClient {
    endpoint: String,
    http: reqwest::Client,
}
impl ClaudeQuotaClient {
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, QuotaError> {
        let mut url = validate_endpoint(base_url)
            .map_err(|_| QuotaError::Protocol("invalid Claude base URL"))?;
        let path = url.path().trim_end_matches('/');
        let root = path
            .strip_suffix("/v1")
            .or_else(|| path.strip_suffix("/api/oauth"))
            .unwrap_or(path);
        url.set_path(&format!("{root}/api/oauth/usage"));
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| QuotaError::HttpTransport)?;
        Ok(Self {
            endpoint: url.into(),
            http,
        })
    }
    pub async fn fetch_quota(
        &self,
        bundle: &ClaudeTokenBundle,
    ) -> Result<ClaudeQuotaSnapshot, QuotaError> {
        if bundle.access_token.trim().is_empty() {
            return Err(QuotaError::Credentials);
        }
        let response = self
            .http
            .get(&self.endpoint)
            .bearer_auth(&bundle.access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("user-agent", CLAUDE_USER_AGENT)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| quota_error(transport_error(e)))?;
        if !response.status().is_success() {
            return Err(match response.status().as_u16() {
                401 | 403 => QuotaError::ReauthenticationRequired,
                status => QuotaError::HttpStatus(status),
            });
        }
        let raw = bounded_json(response).await.map_err(quota_error)?;
        let windows = parse_windows(&raw);
        let extra_usage = parse_extra_usage(&raw);
        if windows.is_empty() && extra_usage.is_none() {
            return Err(QuotaError::Protocol("Claude quota windows missing"));
        }
        Ok(ClaudeQuotaSnapshot {
            windows,
            extra_usage,
            stale: false,
            fetched_at: SystemTime::now().into(),
            last_error: None,
        })
    }

    /// Hold the shared account lock through quota acquisition to prevent concurrent token rotation.
    pub async fn fetch_quota_for_store(
        &self,
        store: &dyn super::ClaudeTokenStore,
        oauth: &super::ClaudeOAuthClient,
    ) -> Result<ClaudeQuotaSnapshot, QuotaError> {
        let lock = store.refresh_lock();
        let _guard = lock.lock().await;
        let bundle =
            super::ensure_fresh_locked(store, oauth)
                .await
                .map_err(|error| match error {
                    crate::ProviderError::Http {
                        status: 401 | 403, ..
                    } => QuotaError::ReauthenticationRequired,
                    crate::ProviderError::Http { status, .. } => QuotaError::HttpStatus(status),
                    crate::ProviderError::Timeout => QuotaError::Timeout,
                    crate::ProviderError::Transport { .. } => QuotaError::HttpTransport,
                    _ => QuotaError::Credentials,
                })?;
        self.fetch_quota(&bundle).await
    }
}
fn quota_error(error: super::ClaudeOAuthError) -> QuotaError {
    match error {
        super::ClaudeOAuthError::Timeout => QuotaError::Timeout,
        super::ClaudeOAuthError::Protocol => QuotaError::Protocol("invalid Claude quota response"),
        _ => QuotaError::HttpTransport,
    }
}
fn parse_windows(raw: &Value) -> Vec<ClaudeQuotaWindow> {
    let mut windows = Vec::new();
    for (key, label) in [
        ("five_hour", "5h"),
        ("seven_day", "7d"),
        ("seven_day_opus", "Opus 7d"),
        ("seven_day_sonnet", "Sonnet 7d"),
    ] {
        if let Some(window) = window(label, &raw[key], "utilization") {
            windows.push(window);
        }
    }
    // `is_active` selects the binding cap, not whether a bucket exists.
    for entry in raw["limits"]
        .as_array()
        .or_else(|| raw["api_limits"].as_array())
        .into_iter()
        .flatten()
    {
        let kind = entry["kind"].as_str().unwrap_or("limit");
        let model = entry["scope"]["model"]["display_name"]
            .as_str()
            .filter(|s| !s.trim().is_empty());
        let label = match (kind, model) {
            ("five_hour" | "session", None) => "5h".into(),
            ("weekly" | "weekly_all", None) => "7d".into(),
            ("weekly_scoped", Some(model)) => format!("{model} 7d"),
            (_, Some(model)) => format!("{model} {kind}"),
            _ => kind.into(),
        };
        if let Some(window) = window(&label, entry, "percent") {
            // The legacy bucket is authoritative when both response shapes coexist.
            // Current limit entries fill missing buckets, matching omp's precedence.
            if !windows.iter().any(|w| w.label == window.label) {
                windows.push(window);
            }
        }
    }
    windows
}

// Match omp usage/claude.ts: newer spend is authoritative; only absent/null spend permits legacy fallback.
fn parse_extra_usage(raw: &Value) -> Option<ClaudeExtraUsage> {
    let (used_usd, limit_usd) = if raw.get("spend").is_some_and(|spend| !spend.is_null()) {
        let spend = &raw["spend"];
        if spend["enabled"] != true {
            return None;
        }
        let used_usd = spend_dollars(&spend["used"])?;
        let limit = spend.get("limit")?;
        (
            used_usd,
            if limit.is_null() {
                None
            } else {
                Some(spend_dollars(limit)?)
            },
        )
    } else {
        let extra = &raw["extra_usage"];
        if extra["is_enabled"] != true {
            return None;
        }
        if extra.get("currency").is_some_and(|currency| {
            !currency
                .as_str()
                .is_some_and(|c| c.eq_ignore_ascii_case("USD"))
        }) {
            return None;
        }
        let exponent = extra.get("decimal_places").map_or(Some(2), safe_integer)?;
        let used_usd = dollars(&extra["used_credits"], exponent)?;
        let limit = extra.get("monthly_limit")?;
        (
            used_usd,
            if limit.is_null() {
                None
            } else {
                Some(dollars(limit, exponent)?)
            },
        )
    };
    if limit_usd.is_some_and(|limit| limit <= 0.0 || !(used_usd / limit).is_finite()) {
        return None;
    }
    Some(ClaudeExtraUsage {
        used_usd,
        limit_usd,
    })
}
fn spend_dollars(amount: &Value) -> Option<f64> {
    if !amount["currency"].as_str()?.eq_ignore_ascii_case("USD") {
        return None;
    }
    dollars(&amount["amount_minor"], safe_integer(&amount["exponent"])?)
}
fn dollars(amount: &Value, exponent: u64) -> Option<f64> {
    let minor = safe_integer(amount)?;
    let divisor = 10_f64.powi(i32::try_from(exponent).ok()?);
    if !divisor.is_finite() {
        return None;
    }
    Some(minor as f64 / divisor)
}
fn safe_integer(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    (number.is_finite()
        && (0.0..=9_007_199_254_740_991.0).contains(&number)
        && number.fract() == 0.0)
        .then_some(number as u64)
}
fn window(label: &str, raw: &Value, percentage: &str) -> Option<ClaudeQuotaWindow> {
    let used = raw[percentage]
        .as_f64()
        .or_else(|| raw[percentage].as_str()?.parse().ok())?;
    if !used.is_finite() {
        return None;
    }
    let used_percent = used.clamp(0.0, 100.0);
    let resets_at = raw["resets_at"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    Some(ClaudeQuotaWindow {
        label: label.into(),
        used_percent,
        remaining_percent: 100.0 - used_percent,
        resets_at,
    })
}
