//! Credential lifecycle contracts for native subscription settings.
use super::*;

#[cfg(test)]
mod subscription_delete_tests {
    use super::*;
    use sandbox::CredentialStore;
    use std::sync::Arc;

    #[test]
    fn deleting_shared_subscription_profile_preserves_auth_until_last_reference() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let store =
            Arc::new(sandbox::credential::FileCredentialStore::open(directory.path()).unwrap());
        store
            .set(
                "shared-account",
                &sandbox::Secret::from("stored-family".to_owned()),
            )
            .unwrap();
        let mut config = config::Config::default();
        for name in ["first", "second"] {
            let input = config::SubscriptionProviderInput {
                name: name.into(),
                provider_type: config::ProviderTypeConfig::Cursor,
                account: "shared-account".into(),
                base_url: config::types::provider::CURSOR_DEFAULT_BASE_URL.into(),
                models: vec![config::ModelEntryConfig::enabled(
                    config::types::provider::CURSOR_DEFAULT_MODEL,
                )],
                default_model: config::types::provider::CURSOR_DEFAULT_MODEL.into(),
                excluded_models: vec![],
            };
            config::save_subscription_provider_edit(&path, &input, None).unwrap();
            config.providers.insert(
                name.into(),
                config::ProviderProfileConfig {
                    provider_type: input.provider_type,
                    credential: config::CredentialRefConfig::Keyring {
                        service: "evorch".into(),
                        account: input.account,
                    },
                    base_url: input.base_url,
                    models: input.models,
                    default_model: input.default_model,
                    ..Default::default()
                },
            );
        }
        let mut state = WorkbenchState::new(
            crate::fixture::DemoSource(Vec::new()),
            &workspace_ui::UiSettings::default(),
        )
        .unwrap()
        .with_provider_settings(ProviderSettingsModel::seed_from_config(&config))
        .with_provider_settings_path(&path)
        .with_credential_store(store.clone());
        state.delete_provider_settings("first".into());
        // Completion comes from the save worker, independent of runner speed.
        state
            .provider_save_rx
            .take()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(
            store.get("shared-account").unwrap().unwrap().expose(),
            "stored-family"
        );
        config.providers.remove("first");
        state.provider_settings = ProviderSettingsModel::seed_from_config(&config);
        state.delete_provider_settings("second".into());
        state
            .provider_save_rx
            .take()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert!(store.get("shared-account").unwrap().is_none());
    }
}

#[cfg(test)]
mod native_delete_race_tests {
    use super::*;
    use sandbox::CredentialStore;
    use std::sync::{Arc, Mutex, mpsc};

    struct GatedDeletion {
        inner: sandbox::credential::FileCredentialStore,
        provider: &'static str,
        started: mpsc::Sender<bool>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl CredentialStore for GatedDeletion {
        fn get(&self, key: &str) -> Result<Option<sandbox::Secret>, sandbox::CredentialError> {
            self.inner.get(key)
        }
        fn set(&self, key: &str, secret: &sandbox::Secret) -> Result<(), sandbox::CredentialError> {
            self.inner.set(key, secret)
        }
        fn delete(&self, key: &str) -> Result<(), sandbox::CredentialError> {
            // At the credential mutation boundary, competing refresh must be excluded.
            let lock = routing::factory::subscription_refresh_lock(self.provider, key);
            self.started.send(lock.try_lock().is_err()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            self.inner.delete(key)
        }
    }
    #[test]
    fn native_credential_deletion_prevents_refresh_from_resurrecting_tokens() {
        for (provider, provider_type, base_url) in [
            (
                "claude",
                config::ProviderTypeConfig::AnthropicSubscription,
                config::types::provider::CLAUDE_DEFAULT_BASE_URL,
            ),
            (
                "cursor",
                config::ProviderTypeConfig::Cursor,
                config::types::provider::CURSOR_DEFAULT_BASE_URL,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            let (started_tx, started) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            let store = Arc::new(GatedDeletion {
                inner: sandbox::credential::FileCredentialStore::open(directory.path()).unwrap(),
                provider,
                started: started_tx,
                release: Mutex::new(release_rx),
            });
            store
                .set(
                    "native-account",
                    &sandbox::Secret::from("old-token-family".to_owned()),
                )
                .unwrap();
            let input = config::SubscriptionProviderInput {
                name: "native".into(),
                provider_type,
                account: "native-account".into(),
                base_url: base_url.into(),
                models: vec![config::ModelEntryConfig::enabled("test-model")],
                default_model: "test-model".into(),
                excluded_models: vec![],
            };
            config::save_subscription_provider_edit(&path, &input, None).unwrap();
            let mut config = config::Config::default();
            config.providers.insert(
                "native".into(),
                config::ProviderProfileConfig {
                    provider_type,
                    credential: config::CredentialRefConfig::Keyring {
                        service: "evorch".into(),
                        account: "native-account".into(),
                    },
                    base_url: base_url.into(),
                    ..Default::default()
                },
            );
            let mut state = WorkbenchState::new(
                crate::fixture::DemoSource(Vec::new()),
                &workspace_ui::UiSettings::default(),
            )
            .unwrap()
            .with_provider_settings(ProviderSettingsModel::seed_from_config(&config))
            .with_provider_settings_path(&path)
            .with_credential_store(store.clone());
            state.delete_provider_settings("native".into());
            let held_lock = started.recv().unwrap();
            let refresh_store = store.clone();
            let refresher = std::thread::spawn(move || {
                let lock = routing::factory::subscription_refresh_lock(provider, "native-account");
                let _guard = lock.blocking_lock();
                if refresh_store.get("native-account").unwrap().is_some() {
                    refresh_store
                        .set(
                            "native-account",
                            &sandbox::Secret::from("rotated-family".to_owned()),
                        )
                        .unwrap();
                }
            });
            release.send(()).unwrap();
            state
                .provider_save_rx
                .take()
                .unwrap()
                .recv()
                .unwrap()
                .unwrap();
            refresher.join().unwrap();
            assert!(
                held_lock,
                "native credential deletion must hold the shared refresh lock"
            );
            assert!(store.get("native-account").unwrap().is_none());
        }
    }
}
