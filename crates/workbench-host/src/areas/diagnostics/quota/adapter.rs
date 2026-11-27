//! Native clients only. Discard error text at the boundary; never log it.
use super::{Backend, Profile};
use config::CredentialRefConfig;
use providers::{
    ProviderAuth, ProviderError,
    provider::{
        claude::{ClaudeOAuthClient, ClaudeOAuthConfig, ClaudeQuotaClient, ClaudeTokenStore},
        codex::quota::{CodexQuotaClient, QuotaConfig, QuotaError},
        cursor::{
            CursorOAuthClient, CursorOAuthConfig, CursorQuotaClient, CursorQuotaConfig,
            CursorTokenStore,
        },
        kimi_quota::KimiQuotaClient,
    },
};
use std::{sync::Arc, time::Duration};
use workbench_protocol::namespaces::diagnostics::*;

enum Native {
    Codex(Box<CodexQuotaClient>),
    Claude(
        ClaudeQuotaClient,
        ClaudeOAuthClient,
        Arc<dyn ClaudeTokenStore>,
    ),
    Cursor(
        CursorQuotaClient,
        CursorOAuthClient,
        Arc<dyn CursorTokenStore>,
    ),
    Kimi(
        KimiQuotaClient,
        CredentialRefConfig,
        Arc<dyn sandbox::CredentialStore>,
    ),
}
fn issue(code: QuotaIssueCode) -> QuotaIssue {
    QuotaIssue {
        code,
        http_status: None,
    }
}
fn classify(error: QuotaError) -> QuotaIssue {
    match error {
        QuotaError::Timeout => issue(QuotaIssueCode::Timeout),
        QuotaError::Credentials => issue(QuotaIssueCode::Credentials),
        QuotaError::ReauthenticationRequired | QuotaError::HttpStatus(401 | 403) => {
            issue(QuotaIssueCode::ReauthenticationRequired)
        }
        QuotaError::HttpStatus(status) if (100..=599).contains(&status) => QuotaIssue {
            code: QuotaIssueCode::Http,
            http_status: Some(status),
        },
        QuotaError::HttpTransport => issue(QuotaIssueCode::Network),
        _ => issue(QuotaIssueCode::Protocol),
    }
}
fn provider_issue(error: ProviderError) -> QuotaIssue {
    match error {
        ProviderError::Timeout => issue(QuotaIssueCode::Timeout),
        ProviderError::Http {
            status: 401 | 403, ..
        } => issue(QuotaIssueCode::ReauthenticationRequired),
        ProviderError::Http { status, .. } if (100..=599).contains(&status) => QuotaIssue {
            code: QuotaIssueCode::Http,
            http_status: Some(status),
        },
        ProviderError::Request(_) => issue(QuotaIssueCode::Credentials),
        ProviderError::InvalidJson { .. } => issue(QuotaIssueCode::Protocol),
        _ => issue(QuotaIssueCode::Network),
    }
}
fn account(profile: &Profile) -> Result<String, QuotaIssue> {
    match &profile.credential {
        CredentialRefConfig::Keyring { account, .. } => Ok(account.clone()),
        _ => Err(issue(QuotaIssueCode::Credentials)),
    }
}
fn base<'a>(profile: &'a Profile, default: &'a str) -> &'a str {
    if profile.base_url.is_empty() {
        default
    } else {
        &profile.base_url
    }
}
pub(super) fn create(
    profile: &Profile,
    store: Arc<dyn sandbox::CredentialStore>,
) -> Result<Box<dyn Backend>, QuotaIssue> {
    use config::types::provider::{
        CLAUDE_DEFAULT_BASE_URL, CURSOR_DEFAULT_BASE_URL, KIMI_DEFAULT_BASE_URL,
    };
    let native = match profile.kind {
        QuotaProvider::Codex => Native::Codex(Box::new(
            CodexQuotaClient::new_for_account(
                QuotaConfig::default(),
                Arc::new(routing::factory::CredentialStoreTokenStore::new(
                    store,
                    account(profile)?,
                )),
            )
            .map_err(classify)?,
        )),
        QuotaProvider::Claude => Native::Claude(
            ClaudeQuotaClient::new(
                base(profile, CLAUDE_DEFAULT_BASE_URL),
                Duration::from_secs(10),
            )
            .map_err(classify)?,
            ClaudeOAuthClient::new(ClaudeOAuthConfig::default())
                .map_err(|_| issue(QuotaIssueCode::Protocol))?,
            Arc::new(routing::factory::CredentialStoreClaudeTokenStore::new(
                store,
                account(profile)?,
            )),
        ),
        QuotaProvider::Cursor => Native::Cursor(
            CursorQuotaClient::new(CursorQuotaConfig {
                base_url: base(profile, CURSOR_DEFAULT_BASE_URL).into(),
                ..Default::default()
            })
            .map_err(provider_issue)?,
            CursorOAuthClient::new(CursorOAuthConfig::default()).map_err(provider_issue)?,
            Arc::new(routing::factory::CredentialStoreCursorTokenStore::new(
                store,
                account(profile)?,
            )),
        ),
        QuotaProvider::Kimi => Native::Kimi(
            KimiQuotaClient::new(
                base(profile, KIMI_DEFAULT_BASE_URL),
                Duration::from_secs(10),
            )
            .map_err(classify)?,
            profile.credential.clone(),
            store,
        ),
    };
    Ok(Box::new(native))
}
fn percent(value: f64) -> Option<u16> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then(|| (value * 100.0).round() as u16)
}
fn text(value: &str) -> String {
    let mut result = String::new();
    for ch in value.chars().filter(|ch| !ch.is_control()) {
        if result.len() + ch.len_utf8() > MAX_QUOTA_LABEL_BYTES {
            break;
        }
        result.push(ch);
    }
    if result.is_empty() {
        "Quota".into()
    } else {
        result
    }
}
fn window(
    id: String,
    label: &str,
    remaining: Option<f64>,
    resets_at_ms: Option<u64>,
) -> QuotaWindow {
    QuotaWindow {
        id,
        label: text(label),
        remaining_basis_points: remaining.and_then(percent),
        resets_at_ms,
        duration_seconds: None,
        usage_label: None,
    }
}
fn millis(seconds: i64) -> Option<u64> {
    u64::try_from(seconds).ok()?.checked_mul(1000)
}
fn kimi_auth(
    credential: &CredentialRefConfig,
    store: &dyn sandbox::CredentialStore,
) -> Result<ProviderAuth, QuotaIssue> {
    let key = match credential {
        CredentialRefConfig::Env { var } => std::env::var(var).ok(),
        CredentialRefConfig::Keyring { account, .. } => store
            .get(account)
            .map_err(|_| issue(QuotaIssueCode::Credentials))?
            .map(|s| s.expose().to_owned()),
    }
    .filter(|key| !key.trim().is_empty())
    .ok_or_else(|| issue(QuotaIssueCode::Credentials))?;
    Ok(ProviderAuth::new(key))
}
#[async_trait::async_trait]
impl Backend for Native {
    async fn fetch(&mut self) -> Result<Vec<QuotaWindow>, QuotaIssue> {
        let windows = match self {
            Self::Codex(client) => {
                let snapshot = client.fetch_quota().await.map_err(classify)?;
                if let Some(error) = snapshot.last_error {
                    return Err(classify(error));
                }
                [
                    ("primary", snapshot.quota.primary),
                    ("secondary", snapshot.quota.secondary),
                    ("code_review", snapshot.quota.code_review),
                ]
                .into_iter()
                .filter_map(|(id, w)| {
                    w.map(|w| {
                        let mut out = window(
                            id.into(),
                            &w.duration_label(),
                            percent(w.used_percent).map(|_| 100.0 - w.used_percent),
                            millis(w.resets_at.timestamp()),
                        );
                        out.duration_seconds = (w.window_duration.as_secs() > 0)
                            .then_some(w.window_duration.as_secs());
                        out
                    })
                })
                .collect()
            }
            Self::Claude(client, oauth, store) => {
                let snapshot = client
                    .fetch_quota_for_store(store.as_ref(), oauth)
                    .await
                    .map_err(classify)?;
                if let Some(error) = snapshot.last_error {
                    return Err(classify(error));
                }
                let mut out: Vec<_> = snapshot
                    .windows
                    .into_iter()
                    .take(MAX_QUOTA_WINDOWS - 1)
                    .enumerate()
                    .map(|(i, w)| {
                        window(
                            format!("window-{i}"),
                            &w.label,
                            percent(w.used_percent).map(|_| 100.0 - w.used_percent),
                            w.resets_at.and_then(|t| millis(t.timestamp())),
                        )
                    })
                    .collect();
                if let Some(extra) = snapshot.extra_usage {
                    let cap = extra.limit_usd.filter(|v| v.is_finite() && *v > 0.0);
                    let mut w = window(
                        "extra".into(),
                        "Extra usage",
                        cap.map(|cap| 100.0 - extra.used_usd / cap * 100.0),
                        None,
                    );
                    if extra.used_usd.is_finite() {
                        w.usage_label = Some(text(&cap.map_or_else(
                            || format!("$ {:.2} used; cap unavailable", extra.used_usd),
                            |cap| format!("$ {:.2} / $ {cap:.2}", extra.used_usd),
                        )));
                    }
                    out.push(w);
                }
                out
            }
            Self::Cursor(client, oauth, store) => client
                .fetch_for_store(store.as_ref(), oauth)
                .await
                .map_err(provider_issue)?
                .windows
                .into_iter()
                .take(MAX_QUOTA_WINDOWS)
                .enumerate()
                .map(|(i, w)| {
                    let mut out = window(
                        format!("window-{i}"),
                        &w.label,
                        w.used_percent.map(|used| 100.0 - used),
                        w.resets_at.and_then(|s| s.checked_mul(1000)),
                    );
                    if let Some(used) = w.used.filter(|v| v.is_finite()) {
                        out.usage_label = Some(text(
                            &w.limit.filter(|v| v.is_finite() && *v > 0.0).map_or_else(
                                || format!("{used:.2} used; limit unavailable"),
                                |limit| format!("{used:.2} / {limit:.2}"),
                            ),
                        ));
                    }
                    out
                })
                .collect(),
            Self::Kimi(client, credential, store) => {
                let auth = kimi_auth(credential, store.as_ref())?;
                let snapshot = client.fetch_quota(&auth).await.map_err(classify)?;
                if let Some(error) = snapshot.last_error {
                    return Err(classify(error));
                }
                snapshot
                    .windows
                    .into_iter()
                    .take(MAX_QUOTA_WINDOWS)
                    .enumerate()
                    .map(|(i, w)| {
                        window(
                            format!("window-{i}"),
                            &w.label,
                            percent(w.used_percent).map(|_| 100.0 - w.used_percent),
                            w.resets_at.and_then(|t| millis(t.timestamp())),
                        )
                    })
                    .collect()
            }
        };
        Ok(windows)
    }
    fn interval(&self, failures: u32) -> Duration {
        if let Self::Codex(client) = self {
            client.poll_interval()
        } else {
            Duration::from_secs(60)
                .saturating_mul(2_u32.saturating_pow(failures.min(4)))
                .min(Duration::from_secs(600))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_percent_is_unknown_not_clamped() {
        for invalid in [f64::NAN, f64::INFINITY, -0.1, 100.1] {
            assert_eq!(percent(invalid), None);
        }
        assert_eq!(percent(0.0), Some(0));
        assert_eq!(percent(100.0), Some(10000));
    }
    #[test]
    fn errors_discard_body_and_credentials() {
        let issue = provider_issue(ProviderError::Http {
            status: 500,
            body: "secret-token raw body".into(),
        });
        assert_eq!(
            serde_json::to_string(&issue).unwrap(),
            r#"{"code":"http","http_status":500}"#
        );
        assert_eq!(
            classify(QuotaError::Protocol("secret raw body")).code,
            QuotaIssueCode::Protocol
        );
    }
}
