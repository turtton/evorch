//! Account usage normalisation, based on oh-my-pi 602b6c8 (OMP-LICENSE).
use super::{
    CursorTokenBundle,
    oauth::{account_id, session_cookie},
};
use crate::{ProviderError, http::map_request_error};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CursorQuotaConfig {
    pub base_url: String,
    pub summary_url: String,
}
impl Default for CursorQuotaConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api2.cursor.sh".into(),
            summary_url: "https://cursor.com/api/usage-summary".into(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorQuotaWindow {
    pub label: String,
    pub used_percent: Option<f64>,
    pub resets_at: Option<u64>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub unit: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorQuotaSnapshot {
    pub windows: Vec<CursorQuotaWindow>,
    pub plan_type: Option<String>,
}
pub struct CursorQuotaClient {
    http: reqwest::Client,
    config: CursorQuotaConfig,
}
impl CursorQuotaClient {
    pub fn new(config: CursorQuotaConfig) -> Result<Self, ProviderError> {
        super::validate_endpoint(&config.base_url)?;
        super::validate_endpoint(&config.summary_url)?;
        Ok(Self {
            http: super::http_client(Some(Duration::from_secs(20)))?,
            config,
        })
    }
    /// Keep token rotation and the ensuing account request under one account lock.
    pub async fn fetch_for_store(
        &self,
        store: &dyn super::CursorTokenStore,
        oauth: &super::CursorOAuthClient,
    ) -> Result<CursorQuotaSnapshot, ProviderError> {
        let lock = store.refresh_lock();
        let _guard = lock.lock().await;
        let mut bundle = store
            .load()?
            .ok_or_else(|| ProviderError::Request("Cursor login required".into()))?;
        if bundle.needs_refresh(super::unix_now()) {
            bundle = oauth.refresh(&bundle).await?;
            store.save(&bundle)?;
        }
        self.fetch(&bundle).await
    }
    pub async fn fetch(
        &self,
        bundle: &CursorTokenBundle,
    ) -> Result<CursorQuotaSnapshot, ProviderError> {
        let legacy = self
            .http
            .get(format!(
                "{}/auth/usage",
                self.config.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&bundle.access_token)
            .header("accept", "application/json");
        let id = account_id(&bundle.access_token).or_else(|| bundle.account_id.clone());
        let summary = async {
            if self.config.base_url.trim_end_matches('/') != "https://api2.cursor.sh"
                && self.config.summary_url == CursorQuotaConfig::default().summary_url
            {
                return Err(ProviderError::Request(
                    "Cursor web usage is unavailable for a custom endpoint".into(),
                ));
            }
            let Some(id) = id else {
                return Err(ProviderError::Request(
                    "Cursor token has no account ID".into(),
                ));
            };
            fetch_json(
                self.http
                    .get(&self.config.summary_url)
                    .header("cookie", session_cookie(&id, &bundle.access_token))
                    .header("accept", "application/json"),
            )
            .await
        };
        let (legacy, summary) = tokio::join!(fetch_json(legacy), summary);
        if legacy.is_err() && summary.is_err() {
            return Err(legacy.expect_err("checked"));
        }
        let mut snapshot = summary
            .as_ref()
            .map(parse_summary)
            .unwrap_or(CursorQuotaSnapshot {
                windows: Vec::new(),
                plan_type: None,
            });
        if let Ok(legacy) = legacy {
            let mut windows = parse_legacy(&legacy);
            // Current subscriptions return a permanent uncapped, unused legacy
            // bucket. It must not replace last-good quota when summary fails.
            windows.retain(|w| w.limit.is_some() || w.used.is_some_and(|used| used > 0.0));
            windows.extend(snapshot.windows);
            snapshot.windows = windows;
        }
        if snapshot.windows.is_empty() {
            summary?;
            return Err(ProviderError::InvalidJson {
                detail: "Cursor usage response contains no usable quota data".into(),
            });
        }
        Ok(snapshot)
    }
}
async fn fetch_json(request: reqwest::RequestBuilder) -> Result<Value, ProviderError> {
    let response = request
        .send()
        .await
        .map_err(|error| map_request_error(error.without_url()))?;
    if !response.status().is_success() {
        return Err(ProviderError::Http {
            status: response.status().as_u16(),
            body: "Cursor usage request failed".into(),
        });
    }
    serde_json::from_slice(&super::bounded_bytes(response, 1024 * 1024).await?).map_err(|_| {
        ProviderError::InvalidJson {
            detail: "Invalid Cursor usage response".into(),
        }
    })
}
fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}
fn reset(value: &Value) -> Option<u64> {
    for key in ["billingCycleEnd", "endOfMonth", "resetsAt", "nextReset"] {
        if let Some(v) = value.get(key).and_then(timestamp) {
            return Some(v);
        }
    }
    for key in ["startOfMonth", "billingCycleStart", "startOfBillingCycle"] {
        if let Some(start) = value
            .get(key)
            .and_then(timestamp)
            .and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
        {
            return start
                .checked_add_months(chrono::Months::new(1))
                .map(|d| d.timestamp() as u64);
        }
    }
    None
}
fn timestamp(v: &Value) -> Option<u64> {
    if let Some(n) = number(v) {
        return Some(if n >= 1e12 { n / 1000.0 } else { n } as u64);
    }
    chrono::DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .and_then(|d| u64::try_from(d.timestamp()).ok())
}
fn cents(value: &Value, label: &str, resets_at: Option<u64>) -> Option<CursorQuotaWindow> {
    if !value.is_object() || value.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let limit = value.get("limit").and_then(number).filter(|v| *v > 0.0);
    // Explicit malformed/zero caps are not an uncapped plan.
    if value.get("limit").is_some_and(|v| !v.is_null()) && limit.is_none() {
        return None;
    }
    let reported = value.get("used").and_then(number);
    let remaining = value.get("remaining").and_then(number);
    let used = reported
        .filter(|v| *v > 0.0)
        .or_else(|| Some((limit? - remaining?).max(0.0)))
        .or(reported)?;
    Some(CursorQuotaWindow {
        label: label.into(),
        used_percent: limit.map(|v| used / v * 100.0),
        resets_at,
        used: Some(used / 100.0),
        limit: limit.map(|v| v / 100.0),
        unit: "usd".into(),
    })
}
fn percent(
    value: f64,
    label: &str,
    limit: Option<f64>,
    resets_at: Option<u64>,
) -> CursorQuotaWindow {
    CursorQuotaWindow {
        label: label.into(),
        used_percent: Some(value),
        resets_at,
        used: Some(limit.map_or(value, |limit| limit * value / 100.0)),
        limit,
        unit: if limit.is_some() { "usd" } else { "percent" }.into(),
    }
}
fn parse_summary(value: &Value) -> CursorQuotaSnapshot {
    let resets_at = reset(value);
    let mut windows = Vec::new();
    let individual = &value["individualUsage"];
    if let Some(overall) = cents(&individual["overall"], "Personal Usage", resets_at) {
        windows.push(overall);
    } else {
        let plan = &individual["plan"];
        if plan.get("enabled").and_then(Value::as_bool) != Some(false) {
            let limit = plan
                .get("limit")
                .and_then(number)
                .filter(|v| *v > 0.0)
                .map(|v| v / 100.0);
            if let Some(auto) = plan.get("autoPercentUsed").and_then(number) {
                windows.push(percent(auto, "Cursor Models", None, resets_at));
            }
            if let Some(api) = plan.get("apiPercentUsed").and_then(number) {
                windows.push(percent(api, "Other Models", limit, resets_at));
            }
            if windows.is_empty() {
                if let Some(total) = plan.get("totalPercentUsed").and_then(number) {
                    windows.push(percent(total, "Personal Usage", limit, resets_at));
                } else if let Some(window) = cents(plan, "Personal Usage", resets_at) {
                    windows.push(window);
                }
            }
        }
    }
    if let Some(window) = cents(&individual["onDemand"], "On-Demand Usage", resets_at) {
        windows.push(window);
    }
    let plan_type = ["membershipType", "planType"]
        .iter()
        .find_map(|key| value.get(key)?.as_str().map(str::to_owned));
    CursorQuotaSnapshot { windows, plan_type }
}
fn parse_legacy(value: &Value) -> Vec<CursorQuotaWindow> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(key, bucket)| {
            let used = ["numRequests", "used", "amountUsed", "usdUsed"]
                .iter()
                .find_map(|field| number(bucket.get(field)?))?;
            let limit = ["maxRequestUsage", "limit", "amountLimit", "usdLimit"]
                .iter()
                .find_map(|field| number(bucket.get(field)?))
                .filter(|n| *n > 0.0);
            let unit = if key == "planUsage"
                || ["usd", "billing", "stripe"]
                    .iter()
                    .any(|needle| key.to_lowercase().contains(needle))
            {
                "usd"
            } else {
                "requests"
            };
            Some(CursorQuotaWindow {
                label: format!("{key} {unit}"),
                used: Some(used),
                limit,
                used_percent: limit.map(|limit| used / limit * 100.0),
                resets_at: reset(value),
                unit: unit.into(),
            })
        })
        .collect()
}
