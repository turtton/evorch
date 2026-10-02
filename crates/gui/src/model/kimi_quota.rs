use super::provider_settings::{ProviderKind, ProviderSettingsModel};
use super::telemetry::quota::{QuotaBackend, QuotaData, QuotaState};
use providers::provider::codex::quota::QuotaError;
use providers::provider::kimi_quota::{KimiQuotaClient, KimiQuotaSnapshot};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

impl QuotaData for KimiQuotaSnapshot {
    fn last_error(&self) -> Option<QuotaError> {
        self.last_error.clone()
    }
    fn mark_stale(&mut self, error: QuotaError) {
        self.stale = true;
        self.last_error = Some(error);
    }
}

#[derive(Debug, Default)]
pub struct KimiQuotaState {
    pub state: QuotaState<KimiQuotaSnapshot>,
    configurations: BTreeMap<String, (String, config::CredentialRefConfig)>,
}

impl KimiQuotaState {
    pub fn configured(&self) -> bool {
        !self.configurations.is_empty()
    }

    pub fn configure(
        &mut self,
        settings: &ProviderSettingsModel,
        store: Option<Arc<dyn sandbox::CredentialStore>>,
    ) {
        let next: BTreeMap<_, _> = settings
            .profiles
            .iter()
            .filter(|profile| profile.kind == ProviderKind::KimiSubscription)
            .filter_map(|profile| {
                Some((
                    profile.name.clone(),
                    (
                        settings.base_url(&profile.name)?.to_owned(),
                        settings.credential(&profile.name)?.clone(),
                    ),
                ))
            })
            .collect();
        if self.configurations == next {
            return;
        }
        self.state
            .subscriptions
            .retain(|name, _| next.contains_key(name));
        for (name, (base_url, credential)) in &next {
            if self.configurations.get(name) == next.get(name) {
                continue;
            }
            let base_url = if base_url.is_empty() {
                config::types::provider::KIMI_DEFAULT_BASE_URL
            } else {
                base_url
            };
            let state = match KimiQuotaClient::new(base_url, Duration::from_secs(10)) {
                Ok(client) => QuotaState::with_backend(Box::new(KimiBackend {
                    client,
                    credential: credential.clone(),
                    store: store.clone(),
                    failures: 0,
                })),
                Err(error) => {
                    let mut state = QuotaState::default();
                    state.accept(Err(error));
                    state
                }
            };
            self.state.subscriptions.insert(name.clone(), state);
        }
        self.configurations = next;
    }
}

struct KimiBackend {
    client: KimiQuotaClient,
    credential: config::CredentialRefConfig,
    store: Option<Arc<dyn sandbox::CredentialStore>>,
    failures: u32,
}

impl KimiBackend {
    fn auth(&self) -> Result<providers::ProviderAuth, QuotaError> {
        let key = match &self.credential {
            config::CredentialRefConfig::Env { var } => std::env::var(var).ok(),
            config::CredentialRefConfig::Keyring { account, .. } => self
                .store
                .as_ref()
                .ok_or(QuotaError::Credentials)?
                .get(account)
                .map_err(|_| QuotaError::Credentials)?
                .map(|key| key.expose().to_owned()),
        }
        .filter(|key| !key.trim().is_empty())
        .ok_or(QuotaError::Credentials)?;
        Ok(providers::ProviderAuth::new(key))
    }
}

#[async_trait::async_trait]
impl QuotaBackend<KimiQuotaSnapshot> for KimiBackend {
    async fn fetch(&mut self) -> Result<KimiQuotaSnapshot, QuotaError> {
        let result = match self.auth() {
            Ok(auth) => self.client.fetch_quota(&auth).await,
            Err(error) => Err(error),
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::CredentialStore;

    #[test]
    fn keyring_auth_uses_credential_account_and_observes_key_rotation() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            Arc::new(sandbox::credential::FileCredentialStore::open(directory.path()).unwrap());
        store
            .set(
                "saved-account",
                &sandbox::Secret::from("first-key".to_owned()),
            )
            .unwrap();
        let backend = KimiBackend {
            client: KimiQuotaClient::new("https://api.kimi.ai/coding/v1", Duration::from_secs(1))
                .unwrap(),
            credential: config::CredentialRefConfig::Keyring {
                service: "evorch".into(),
                account: "saved-account".into(),
            },
            store: Some(store.clone()),
            failures: 0,
        };
        assert_eq!(backend.auth().unwrap().api_key, "first-key");
        store
            .set(
                "saved-account",
                &sandbox::Secret::from("second-key".to_owned()),
            )
            .unwrap();
        assert_eq!(backend.auth().unwrap().api_key, "second-key");
        store.delete("saved-account").unwrap();
        assert!(matches!(backend.auth(), Err(QuotaError::Credentials)));
    }
}
