//! Bounded, secret-free snapshots. Queries never await a provider request.
use crate::{
    limits::MAX_NAME_BYTES,
    validation::{ValidationError, check_count, check_line},
};
use serde::{Deserialize, Serialize};

pub const MAX_PROVIDER_QUOTAS: usize = super::super::settings::MAX_PROVIDERS;
pub const MAX_QUOTA_WINDOWS: usize = 8;
pub const MAX_QUOTA_LABEL_BYTES: usize = 96;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetProviderQuotas {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaProvider {
    Codex,
    Claude,
    Cursor,
    Kimi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaState {
    Loading,
    Ready,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaIssueCode {
    Credentials,
    ReauthenticationRequired,
    Timeout,
    Network,
    Http,
    Protocol,
}

/// No provider message, response body or credential may be carried here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaIssue {
    pub code: QuotaIssueCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    /// Unknown/invalid/unlimited is None, not zero. 10000 means unused.
    pub remaining_basis_points: Option<u16>,
    pub resets_at_ms: Option<u64>,
    pub duration_seconds: Option<u64>,
    /// Formatted numeric usage only (e.g. Cursor spend), never a raw response.
    pub usage_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuota {
    pub profile: String,
    pub provider: QuotaProvider,
    pub state: QuotaState,
    pub fetched_at_ms: Option<u64>,
    pub windows: Vec<QuotaWindow>,
    pub issue: Option<QuotaIssue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotas {
    pub profiles: Vec<ProviderQuota>,
}

impl ProviderQuotas {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("profiles", self.profiles.len(), MAX_PROVIDER_QUOTAS)?;
        let mut names = std::collections::BTreeSet::new();
        for quota in &self.profiles {
            check_line("profile", &quota.profile, MAX_NAME_BYTES, false)?;
            if !names.insert(&quota.profile) {
                return inconsistent("duplicate quota profile");
            }
            check_count("windows", quota.windows.len(), MAX_QUOTA_WINDOWS)?;
            match quota.state {
                QuotaState::Loading | QuotaState::Unavailable
                    if !quota.windows.is_empty() || quota.fetched_at_ms.is_some() =>
                {
                    return inconsistent("quota without a successful fetch");
                }
                QuotaState::Ready | QuotaState::Stale if quota.fetched_at_ms.is_none() => {
                    return inconsistent("quota lacks fetch time");
                }
                _ => {}
            }
            if matches!(quota.state, QuotaState::Stale | QuotaState::Unavailable)
                != quota.issue.is_some()
            {
                return inconsistent("quota issue and state disagree");
            }
            if let Some(issue) = quota.issue
                && (issue
                    .http_status
                    .is_some_and(|status| !(100..=599).contains(&status))
                    || (issue.http_status.is_some() && issue.code != QuotaIssueCode::Http))
            {
                return inconsistent("invalid quota HTTP status");
            }
            let mut ids = std::collections::BTreeSet::new();
            for window in &quota.windows {
                check_line("id", &window.id, MAX_NAME_BYTES, false)?;
                check_line("label", &window.label, MAX_QUOTA_LABEL_BYTES, false)?;
                if !ids.insert(&window.id)
                    || window.remaining_basis_points.is_some_and(|p| p > 10000)
                    || window.duration_seconds == Some(0)
                {
                    return inconsistent("invalid quota window");
                }
                if let Some(label) = &window.usage_label {
                    check_line("usage_label", label, MAX_QUOTA_LABEL_BYTES, false)?;
                }
            }
        }
        Ok(())
    }
}
fn inconsistent<T>(reason: &'static str) -> Result<T, ValidationError> {
    Err(ValidationError::Inconsistent {
        field: "provider_quotas",
        reason,
    })
}
