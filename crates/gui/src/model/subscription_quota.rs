//! Quota uses the same account token refresh boundary as inference and model discovery.
use super::{
    provider_settings::{ProviderKind, ProviderSettingsModel},
    telemetry::quota::{QuotaBackend, QuotaData, QuotaState},
};
use providers::provider::{
    claude::{ClaudeOAuthClient, ClaudeOAuthConfig, ClaudeQuotaClient, ClaudeTokenStore},
    codex::quota::QuotaError,
    cursor::{
        CursorOAuthClient, CursorOAuthConfig, CursorQuotaClient, CursorQuotaConfig,
        CursorTokenStore,
    },
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Debug, Clone)]
pub struct SubscriptionQuotaWindow {
    pub label: String,
    pub remaining_percent: Option<f64>,
    pub used_percent: Option<f64>,
    pub resets_at: String,
    pub usage: Option<String>,
}
#[derive(Debug, Clone)]
pub struct SubscriptionQuotaSnapshot {
    pub windows: Vec<SubscriptionQuotaWindow>,
    pub stale: bool,
    pub last_error: Option<QuotaError>,
}
impl QuotaData for SubscriptionQuotaSnapshot {
    fn last_error(&self) -> Option<QuotaError> {
        self.last_error.clone()
    }
    fn mark_stale(&mut self, error: QuotaError) {
        self.stale = true;
        self.last_error = Some(error);
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Configuration {
    base_url: String,
    account: String,
    kind: ProviderKind,
    store_available: bool,
}
#[derive(Debug, Default)]
pub struct SubscriptionQuotaState {
    pub claude: QuotaState<SubscriptionQuotaSnapshot>,
    pub cursor: QuotaState<SubscriptionQuotaSnapshot>,
    configurations: BTreeMap<String, Configuration>,
}
impl SubscriptionQuotaState {
    pub fn invalidate_account(&mut self, account: &str) {
        let names: Vec<_> = self
            .configurations
            .iter()
            .filter(|(_, c)| c.account == account)
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            self.configurations.remove(&name);
            self.claude.subscriptions.remove(&name);
            self.cursor.subscriptions.remove(&name);
        }
    }

    pub fn configure(
        &mut self,
        settings: &ProviderSettingsModel,
        store: Option<Arc<dyn sandbox::CredentialStore>>,
    ) {
        let next: BTreeMap<_, _> = settings
            .profiles
            .iter()
            .filter_map(|profile| {
                if !matches!(
                    profile.kind,
                    ProviderKind::ClaudeSubscription | ProviderKind::Cursor
                ) {
                    return None;
                }
                let config::CredentialRefConfig::Keyring { account, .. } =
                    settings.credential(&profile.name)?
                else {
                    return None;
                };
                Some((
                    profile.name.clone(),
                    Configuration {
                        base_url: settings.base_url(&profile.name)?.into(),
                        account: account.clone(),
                        kind: profile.kind,
                        store_available: store.is_some(),
                    },
                ))
            })
            .collect();
        self.claude.subscriptions.retain(|name, _| {
            next.get(name)
                .is_some_and(|c| c.kind == ProviderKind::ClaudeSubscription)
        });
        self.cursor.subscriptions.retain(|name, _| {
            next.get(name)
                .is_some_and(|c| c.kind == ProviderKind::Cursor)
        });
        for (name, config) in &next {
            if self.configurations.get(name) == Some(config) {
                continue;
            }
            let result = store
                .clone()
                .ok_or(QuotaError::Credentials)
                .and_then(|store| SubscriptionBackend::new(config, store));
            let state = match result {
                Ok(backend) => QuotaState::with_backend(Box::new(backend)),
                Err(error) => {
                    let mut state = QuotaState::default();
                    state.accept(Err(error));
                    state
                }
            };
            let subscriptions = if config.kind == ProviderKind::Cursor {
                &mut self.cursor.subscriptions
            } else {
                &mut self.claude.subscriptions
            };
            subscriptions.insert(name.clone(), state);
        }
        self.configurations = next;
    }
}
enum NativeQuota {
    Claude {
        store: Arc<dyn ClaudeTokenStore>,
        oauth: ClaudeOAuthClient,
        quota: ClaudeQuotaClient,
    },
    Cursor {
        store: Arc<dyn CursorTokenStore>,
        oauth: CursorOAuthClient,
        quota: CursorQuotaClient,
    },
}
struct SubscriptionBackend {
    native: NativeQuota,
    failures: u32,
}
impl SubscriptionBackend {
    fn new(
        configuration: &Configuration,
        store: Arc<dyn sandbox::CredentialStore>,
    ) -> Result<Self, QuotaError> {
        let native = if configuration.kind == ProviderKind::Cursor {
            let tokens = Arc::new(routing::factory::CredentialStoreCursorTokenStore::new(
                store,
                configuration.account.clone(),
            ));
            NativeQuota::Cursor {
                store: tokens,
                oauth: CursorOAuthClient::new(CursorOAuthConfig::default()).map_err(quota_error)?,
                quota: CursorQuotaClient::new(CursorQuotaConfig {
                    base_url: configuration.base_url.clone(),
                    ..Default::default()
                })
                .map_err(quota_error)?,
            }
        } else {
            let tokens = Arc::new(routing::factory::CredentialStoreClaudeTokenStore::new(
                store,
                configuration.account.clone(),
            ));
            NativeQuota::Claude {
                store: tokens,
                oauth: ClaudeOAuthClient::new(ClaudeOAuthConfig::default())
                    .map_err(|_| QuotaError::Protocol("invalid Claude OAuth configuration"))?,
                quota: ClaudeQuotaClient::new(&configuration.base_url, Duration::from_secs(10))?,
            }
        };
        Ok(Self {
            native,
            failures: 0,
        })
    }
}
#[async_trait::async_trait]
impl QuotaBackend<SubscriptionQuotaSnapshot> for SubscriptionBackend {
    async fn fetch(&mut self) -> Result<SubscriptionQuotaSnapshot, QuotaError> {
        let result = async {
            let windows = match &self.native {
                NativeQuota::Claude {
                    store,
                    oauth,
                    quota,
                } => claude_windows(quota.fetch_quota_for_store(store.as_ref(), oauth).await?),
                NativeQuota::Cursor {
                    store,
                    oauth,
                    quota,
                } => quota
                    .fetch_for_store(store.as_ref(), oauth)
                    .await
                    .map_err(quota_error)?
                    .windows
                    .into_iter()
                    .map(|w| {
                        let used_percent = w
                            .used_percent
                            .filter(|p| p.is_finite())
                            .map(|p| p.clamp(0.0, 100.0));
                        SubscriptionQuotaWindow {
                            label: w.label,
                            remaining_percent: used_percent.map(|p| 100.0 - p),
                            used_percent,
                            resets_at: w
                                .resets_at
                                .and_then(|t| i64::try_from(t).ok())
                                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                                .map_or_else(
                                    || "unknown".into(),
                                    |d| d.format("%Y-%m-%d %H:%M UTC").to_string(),
                                ),
                            usage: w.used.map(|used| match w.limit {
                                Some(limit) => format!("{used:.2} / {limit:.2} {}", w.unit),
                                None => format!("{used:.2} {} used; limit unavailable", w.unit),
                            }),
                        }
                    })
                    .collect(),
            };
            Ok(SubscriptionQuotaSnapshot {
                windows,
                stale: false,
                last_error: None,
            })
        }
        .await;
        self.failures = if result.is_ok() {
            0
        } else {
            self.failures.saturating_add(1)
        };
        result
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
            .saturating_mul(2_u32.saturating_pow(self.failures.min(4)))
            .min(Duration::from_secs(600))
    }
}
fn claude_windows(
    snapshot: providers::provider::claude::ClaudeQuotaSnapshot,
) -> Vec<SubscriptionQuotaWindow> {
    let mut windows: Vec<_> = snapshot
        .windows
        .into_iter()
        .map(|w| SubscriptionQuotaWindow {
            label: w.label,
            remaining_percent: Some(w.remaining_percent),
            used_percent: Some(w.used_percent),
            resets_at: w.resets_at.map_or_else(
                || "unknown".into(),
                |d| d.format("%Y-%m-%d %H:%M UTC").to_string(),
            ),
            usage: None,
        })
        .collect();
    if let Some(extra) = snapshot.extra_usage {
        let limit = extra
            .limit_usd
            .filter(|limit| limit.is_finite() && *limit > 0.0);
        let used_percent = limit.map(|limit| (extra.used_usd / limit * 100.0).clamp(0.0, 100.0));
        windows.push(SubscriptionQuotaWindow {
            label: "Extra usage".into(),
            remaining_percent: used_percent.map(|percent| 100.0 - percent),
            used_percent,
            resets_at: "unknown".into(),
            usage: Some(limit.map_or_else(
                || format!("${:.2} used; spending cap unavailable", extra.used_usd),
                |limit| format!("${:.2} / ${limit:.2}", extra.used_usd),
            )),
        });
    }
    windows
}

fn quota_error(error: providers::ProviderError) -> QuotaError {
    match error {
        providers::ProviderError::Timeout => QuotaError::Timeout,
        providers::ProviderError::Http {
            status: 401 | 403, ..
        } => QuotaError::ReauthenticationRequired,
        providers::ProviderError::Http { status, .. } => QuotaError::HttpStatus(status),
        providers::ProviderError::Request(_) | providers::ProviderError::InvalidJson { .. } => {
            QuotaError::Credentials
        }
        _ => QuotaError::HttpTransport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings(account: &str) -> ProviderSettingsModel {
        let mut config = config::Config::default();
        for (name, provider_type) in [
            (
                "claude-work",
                config::ProviderTypeConfig::AnthropicSubscription,
            ),
            ("cursor-work", config::ProviderTypeConfig::Cursor),
        ] {
            config.providers.insert(
                name.into(),
                config::ProviderProfileConfig {
                    provider_type,
                    base_url: match provider_type {
                        config::ProviderTypeConfig::Cursor => {
                            config::types::provider::CURSOR_DEFAULT_BASE_URL.into()
                        }
                        _ => config::types::provider::CLAUDE_DEFAULT_BASE_URL.into(),
                    },
                    credential: config::CredentialRefConfig::Keyring {
                        service: "evorch".into(),
                        account: account.into(),
                    },
                    ..Default::default()
                },
            );
        }
        ProviderSettingsModel::seed_from_config(&config)
    }
    fn snapshot() -> SubscriptionQuotaSnapshot {
        SubscriptionQuotaSnapshot {
            windows: vec![SubscriptionQuotaWindow {
                label: "monthly".into(),
                remaining_percent: None,
                used_percent: None,
                resets_at: "unknown".into(),
                usage: Some("12 requests used; limit unavailable".into()),
            }],
            stale: false,
            last_error: None,
        }
    }
    #[test]
    fn quota_keeps_unknown_values_and_errors_mark_last_snapshot_stale() {
        let mut state = QuotaState::default();
        state.accept(Ok(snapshot()));
        state.accept(Err(QuotaError::Timeout));
        let current = state.snapshot.as_ref().unwrap();
        assert!(current.stale);
        assert_eq!(current.windows[0].remaining_percent, None);
        assert!(matches!(state.error, Some(QuotaError::Timeout)));
    }
    #[test]
    fn account_change_and_reauthentication_discard_previous_quota() {
        let mut quotas = SubscriptionQuotaState::default();
        quotas.configure(&settings("first"), None);
        quotas
            .claude
            .subscriptions
            .get_mut("claude-work")
            .unwrap()
            .accept(Ok(snapshot()));
        quotas
            .cursor
            .subscriptions
            .get_mut("cursor-work")
            .unwrap()
            .accept(Ok(snapshot()));
        quotas.configure(&settings("second"), None);
        assert!(
            quotas.claude.subscriptions["claude-work"]
                .snapshot
                .is_none()
        );
        assert!(
            quotas.cursor.subscriptions["cursor-work"]
                .snapshot
                .is_none()
        );
        quotas
            .claude
            .subscriptions
            .get_mut("claude-work")
            .unwrap()
            .accept(Ok(snapshot()));
        quotas.invalidate_account("second");
        quotas.configure(&settings("second"), None);
        assert!(
            quotas.claude.subscriptions["claude-work"]
                .snapshot
                .is_none()
        );
        quotas.configure(&ProviderSettingsModel::default(), None);
        assert!(quotas.claude.subscriptions.is_empty());
        assert!(quotas.cursor.subscriptions.is_empty());
    }
}

#[cfg(test)]
mod credential_availability_tests {
    use super::*;
    #[test]
    fn credentials_becoming_available_reconfigure_the_same_profile() {
        let mut config = config::Config::default();
        config.providers.insert(
            "claude".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::AnthropicSubscription,
                base_url: config::types::provider::CLAUDE_DEFAULT_BASE_URL.into(),
                credential: config::CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: "account".into(),
                },
                ..Default::default()
            },
        );
        let settings = ProviderSettingsModel::seed_from_config(&config);
        let mut quotas = SubscriptionQuotaState::default();
        quotas.configure(&settings, None);
        assert!(matches!(
            quotas.claude.subscriptions["claude"].error,
            Some(QuotaError::Credentials)
        ));
        let directory = tempfile::tempdir().unwrap();
        let store =
            Arc::new(sandbox::credential::FileCredentialStore::open(directory.path()).unwrap());
        quotas.configure(&settings, Some(store));
        assert!(quotas.claude.subscriptions["claude"].error.is_none());
    }
}

#[cfg(test)]
mod extra_usage_tests {
    use super::*;
    #[test]
    fn extra_spending_only_has_percentages_when_a_positive_cap_is_available() {
        for (limit_usd, expected) in [(Some(20.0), Some(75.0)), (None, None), (Some(0.0), None)] {
            let snapshot = providers::provider::claude::ClaudeQuotaSnapshot {
                windows: vec![],
                extra_usage: Some(providers::provider::claude::ClaudeExtraUsage {
                    used_usd: 5.0,
                    limit_usd,
                }),
                stale: false,
                fetched_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
                last_error: None,
            };
            let windows = claude_windows(snapshot);
            assert_eq!(windows[0].remaining_percent, expected);
            assert!(windows[0].usage.as_ref().unwrap().contains("$5.00"));
        }
    }
}
