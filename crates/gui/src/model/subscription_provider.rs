//! Native subscription login state. Tokens and authorization codes never enter errors or Debug.
use super::provider_settings::{ModelsFetchState, OpenAiEditorModel, ProviderKind};
use providers::ProviderClient;
use providers::provider::{
    anthropic::AnthropicConfig, claude::ClaudeTokenStore, cursor::CursorTokenStore,
};
use std::{
    sync::{Arc, mpsc},
    time::{SystemTime, UNIX_EPOCH},
};

#[path = "subscription_oauth_backend.rs"]
mod oauth;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionAuthState {
    SignedOut,
    Waiting { manual_code: bool },
    Exchanging,
    SignedIn { expires_at: u64 },
    Failed(&'static str),
}

enum LoginEvent {
    Prompt(String, bool),
    Finished(Result<u64, &'static str>),
}
enum LoginCommand {
    Code(sandbox::Secret),
    Cancel,
}

pub struct SubscriptionEditorModel {
    pub models: OpenAiEditorModel,
    pub account: String,
    pub auth: SubscriptionAuthState,
    pub code_input: String,
    pub authorize_url: Option<String>,
    loaded_account: Option<String>,
    loaded_store_available: bool,
    login_account: Option<String>,
    url_to_open: Option<String>,
    login_rx: Option<mpsc::Receiver<LoginEvent>>,
    login_tx: Option<tokio::sync::mpsc::UnboundedSender<LoginCommand>>,
}
impl std::fmt::Debug for SubscriptionEditorModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionEditorModel")
            .field("models", &self.models)
            .field("account", &self.account)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}
impl Drop for SubscriptionEditorModel {
    fn drop(&mut self) {
        self.cancel_login();
    }
}
impl SubscriptionEditorModel {
    pub fn new(name: String, kind: ProviderKind) -> Self {
        let cursor = kind == ProviderKind::Cursor;
        let (base, ids, default) = if cursor {
            (
                config::types::provider::CURSOR_DEFAULT_BASE_URL,
                config::types::provider::CURSOR_DEFAULT_MODELS,
                config::types::provider::CURSOR_DEFAULT_MODEL,
            )
        } else {
            (
                config::types::provider::CLAUDE_DEFAULT_BASE_URL,
                config::types::provider::CLAUDE_DEFAULT_MODELS,
                config::types::provider::CLAUDE_DEFAULT_MODEL,
            )
        };
        Self {
            account: name.clone(),
            models: OpenAiEditorModel {
                name,
                provider_type: if cursor {
                    config::ProviderTypeConfig::Cursor
                } else {
                    config::ProviderTypeConfig::AnthropicSubscription
                },
                base_url: base.into(),
                models: ids
                    .iter()
                    .map(|id| config::ModelEntryConfig::enabled(*id))
                    .collect(),
                default_model: default.into(),
                ..Default::default()
            },
            auth: SubscriptionAuthState::SignedOut,
            code_input: String::new(),
            authorize_url: None,
            loaded_account: None,
            loaded_store_available: false,
            login_account: None,
            url_to_open: None,
            login_rx: None,
            login_tx: None,
        }
    }
    pub fn from_profile(name: &str, profile: &config::ProviderProfileConfig) -> Self {
        let kind = if profile.provider_type == config::ProviderTypeConfig::Cursor {
            ProviderKind::Cursor
        } else {
            ProviderKind::ClaudeSubscription
        };
        let mut editor = Self::new(name.into(), kind);
        editor.models.original_name = Some(name.into());
        editor.models.base_url.clone_from(&profile.base_url);
        editor.models.models.clone_from(&profile.models);
        editor.models.excluded_models_text = profile.excluded_models.join("\n");
        editor
            .models
            .default_model
            .clone_from(&profile.default_model);
        if let config::CredentialRefConfig::Keyring { account, .. } = &profile.credential {
            editor.account.clone_from(account);
        }
        editor
    }
    pub fn rename_profile(&mut self, previous_name: &str) {
        if self.models.original_name.is_none() && self.account == previous_name {
            self.account.clone_from(&self.models.name);
        }
    }

    pub fn service(&self) -> &'static str {
        if self.models.provider_type == config::ProviderTypeConfig::Cursor {
            "Cursor"
        } else {
            "Claude"
        }
    }
    pub fn busy(&self) -> bool {
        matches!(
            self.auth,
            SubscriptionAuthState::Waiting { .. } | SubscriptionAuthState::Exchanging
        )
    }
    pub fn to_input(&self) -> config::SubscriptionProviderInput {
        config::SubscriptionProviderInput {
            name: self.models.name.clone(),
            provider_type: self.models.provider_type,
            account: self.account.clone(),
            base_url: self.models.base_url.clone(),
            models: self.models.models.clone(),
            default_model: self.models.default_model.clone(),
            excluded_models: self.models.parsed_excluded_models(),
        }
    }
    pub fn load_auth(&mut self, store: Option<Arc<dyn sandbox::CredentialStore>>) {
        if self.loaded_account.as_deref() == Some(&self.account)
            && self.loaded_store_available == store.is_some()
        {
            return;
        }
        self.cancel_login();
        self.loaded_account = Some(self.account.clone());
        self.loaded_store_available = store.is_some();
        self.models.models_rx = None;
        self.models.available_models = None;
        self.models.models_fetch_state = ModelsFetchState::Idle;
        let Some(store) = store else {
            self.auth = SubscriptionAuthState::Failed("Credential store unavailable");
            return;
        };
        let loaded = if self.models.provider_type == config::ProviderTypeConfig::Cursor {
            routing::factory::CredentialStoreCursorTokenStore::new(store, self.account.clone())
                .load()
                .map(|b| b.map(|b| b.expires_at))
        } else {
            routing::factory::CredentialStoreClaudeTokenStore::new(store, self.account.clone())
                .load()
                .map(|b| b.map(|b| b.expires_at))
        };
        self.auth = match loaded {
            Ok(Some(expires_at)) => SubscriptionAuthState::SignedIn { expires_at },
            Ok(None) => SubscriptionAuthState::SignedOut,
            Err(_) => {
                SubscriptionAuthState::Failed("Could not read stored credentials; sign in again")
            }
        };
    }
    pub fn start_login(&mut self, store: Option<Arc<dyn sandbox::CredentialStore>>) {
        if self.busy() {
            return;
        }
        let Some(store) = store else {
            self.auth = SubscriptionAuthState::Failed("Credential store unavailable");
            return;
        };
        self.cancel_login();
        self.loaded_account = Some(self.account.clone());
        self.loaded_store_available = true;
        self.login_account = Some(self.account.clone());
        self.auth = SubscriptionAuthState::Waiting { manual_code: false };
        let (tx, rx) = mpsc::channel();
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        self.login_rx = Some(rx);
        self.login_tx = Some(commands);
        let account = self.account.clone();
        let cursor = self.models.provider_type == config::ProviderTypeConfig::Cursor;
        std::thread::spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(async {
                    if cursor {
                        oauth::cursor_login(store, account, &tx, command_rx).await
                    } else {
                        oauth::claude_login(store, account, &tx, command_rx).await
                    }
                }),
                Err(_) => Err("Could not start login worker"),
            };
            let _ = tx.send(LoginEvent::Finished(result));
        });
    }
    pub fn submit_code(&mut self) {
        let code = std::mem::take(&mut self.code_input);
        if code.trim().is_empty() {
            return;
        }
        if let Some(tx) = &self.login_tx
            && tx
                .send(LoginCommand::Code(sandbox::Secret::from(code)))
                .is_ok()
        {
            self.auth = SubscriptionAuthState::Exchanging;
        }
    }
    pub fn cancel_login(&mut self) {
        if let Some(tx) = self.login_tx.take() {
            let _ = tx.send(LoginCommand::Cancel);
        }
        self.login_rx = None;
        self.login_account = None;
        self.authorize_url = None;
        self.url_to_open = None;
        self.code_input.clear();
        if self.busy() {
            self.auth = SubscriptionAuthState::SignedOut;
        }
    }
    pub fn poll_login(&mut self) -> bool {
        if self
            .login_account
            .as_deref()
            .is_some_and(|account| account != self.account)
        {
            self.cancel_login();
            self.loaded_account = None;
            return true;
        }
        let Some(rx) = self.login_rx.take() else {
            return false;
        };
        let mut changed = false;
        loop {
            match rx.try_recv() {
                Ok(LoginEvent::Prompt(url, manual_code)) => {
                    self.auth = SubscriptionAuthState::Waiting { manual_code };
                    self.authorize_url = Some(url.clone());
                    self.url_to_open = Some(url);
                    changed = true;
                }
                Ok(LoginEvent::Finished(result)) => {
                    self.auth = match result {
                        Ok(expires_at) => SubscriptionAuthState::SignedIn { expires_at },
                        Err(error) => SubscriptionAuthState::Failed(error),
                    };
                    self.login_tx = None;
                    self.login_account = None;
                    self.authorize_url = None;
                    self.url_to_open = None;
                    self.code_input.clear();
                    return true;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.login_rx = Some(rx);
                    return changed;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.auth = SubscriptionAuthState::Failed("Login worker stopped");
                    self.login_tx = None;
                    return true;
                }
            }
        }
    }
    pub fn take_authorize_url(&mut self) -> Option<String> {
        self.url_to_open.take()
    }
    pub fn start_models_fetch(&mut self, store: Option<Arc<dyn sandbox::CredentialStore>>) {
        if self.models.models_rx.is_some() {
            return;
        }
        self.models.models_fetch_state = ModelsFetchState::Loading;
        self.models.available_models = None;
        self.models.fetch_selected.clear();
        let base_url = self.models.base_url.clone();
        self.models.models_fetch_base_url = Some(base_url.clone());
        let account = self.account.clone();
        let cursor = self.models.provider_type == config::ProviderTypeConfig::Cursor;
        let (tx, rx) = mpsc::channel();
        self.models.models_rx = Some(rx);
        std::thread::spawn(move || {
            let result = (|| {
                let store = store.ok_or("Credential store unavailable")?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "Could not start model fetch")?;
                runtime.block_on(async {
                    let models = if cursor {
                        let token_store = Arc::new(
                            routing::factory::CredentialStoreCursorTokenStore::new(store, account),
                        );
                        let client = providers::provider::cursor::CursorClient::new(
                            providers::provider::cursor::CursorConfig {
                                base_url,
                                ..Default::default()
                            },
                            token_store,
                        )
                        .map_err(|_| "Could not initialize Cursor")?;
                        client
                            .list_models(&providers::ProviderAuth::new(String::new()))
                            .await
                    } else {
                        let token_store = Arc::new(
                            routing::factory::CredentialStoreClaudeTokenStore::new(store, account),
                        );
                        let client = providers::provider::claude::ClaudeClient::new(
                            AnthropicConfig {
                                base_url: super::provider_settings::anthropic_base_url(&base_url),
                                ..Default::default()
                            },
                            token_store,
                        )
                        .map_err(|_| "Could not initialize Claude")?;
                        client
                            .list_models(&providers::ProviderAuth::new(String::new()))
                            .await
                    }
                    .map_err(|_| "Could not fetch models; check connection and sign in again")?;
                    models.ok_or("Provider does not expose a model catalog")
                })
            })()
            .map_err(str::to_owned);
            let _ = tx.send(result);
        });
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_only_accepts_matching_loopback_code_and_state() {
        let request =
            b"GET /callback?code=secret&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert!(oauth::callback_url(request, "expected").is_some());
        for invalid in [
            "GET /callback?code=secret&state=wrong HTTP/1.1",
            "GET /callback?code=secret&state=expected&state=expected HTTP/1.1",
            "GET /callback?code=secret&state=expected&error=denied HTTP/1.1",
            "GET /other?code=secret&state=expected HTTP/1.1",
            "POST /callback?code=secret&state=expected HTTP/1.1",
        ] {
            assert!(oauth::callback_url(invalid.as_bytes(), "expected").is_none());
        }
    }
    #[test]
    fn account_change_discards_login_result_and_model_catalog() {
        let mut editor =
            SubscriptionEditorModel::new("old".into(), ProviderKind::ClaudeSubscription);
        let (tx, rx) = mpsc::channel();
        editor.login_rx = Some(rx);
        editor.login_account = Some("old".into());
        editor.auth = SubscriptionAuthState::Waiting { manual_code: true };
        tx.send(LoginEvent::Finished(Ok(42))).unwrap();
        editor.account = "new".into();
        assert!(editor.poll_login());
        assert_eq!(editor.auth, SubscriptionAuthState::SignedOut);
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    #[test]
    fn saved_native_credentials_restore_expiry_and_account_change_clears_models() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            Arc::new(sandbox::credential::FileCredentialStore::open(directory.path()).unwrap());
        let bundle = providers::provider::claude::ClaudeTokenBundle {
            access_token: "private-access".into(),
            refresh_token: "private-refresh".into(),
            expires_at: 1234,
            account_id: Some("principal".into()),
            email: None,
            org_id: None,
            org_name: None,
        };
        routing::factory::CredentialStoreClaudeTokenStore::new(store.clone(), "work".into())
            .save(&bundle)
            .unwrap();
        let mut editor =
            SubscriptionEditorModel::new("work".into(), ProviderKind::ClaudeSubscription);
        editor.load_auth(None);
        assert!(matches!(editor.auth, SubscriptionAuthState::Failed(_)));
        editor.load_auth(Some(store.clone()));
        assert_eq!(
            editor.auth,
            SubscriptionAuthState::SignedIn { expires_at: 1234 }
        );
        editor.models.available_models = Some(vec!["prior-account-model".into()]);
        editor.account = "other".into();
        editor.load_auth(Some(store));
        assert_eq!(editor.auth, SubscriptionAuthState::SignedOut);
        assert!(editor.models.available_models.is_none());
        assert!(!format!("{editor:?}").contains("private-access"));
    }
}
