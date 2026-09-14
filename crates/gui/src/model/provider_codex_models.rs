use super::{CodexEditorModel, ModelsFetchState};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use providers::provider::codex::tokens::{CodexTokenStore, parse_jwt_claims};
use std::collections::BTreeSet;
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
#[path = "provider_codex_models_tests.rs"]
mod tests;

#[derive(Debug)]
pub struct CodexModelsFetch {
    pub base_url: String,
    pub models_fetch_state: ModelsFetchState,
    pub available_models: Option<Vec<String>>,
    pub fetch_selected: BTreeSet<String>,
    pub models_rx: Option<mpsc::Receiver<Result<Vec<String>, String>>>,
    pub(super) source: Option<(String, String)>,
}

impl Default for CodexModelsFetch {
    fn default() -> Self {
        Self {
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            models_fetch_state: ModelsFetchState::Idle,
            available_models: None,
            fetch_selected: BTreeSet::new(),
            models_rx: None,
            source: None,
        }
    }
}

impl CodexEditorModel {
    pub fn start_models_fetch_with_store(
        &mut self,
        store: Option<Arc<dyn sandbox::CredentialStore>>,
    ) {
        if self.fetch.models_rx.is_some() {
            return;
        }
        self.fetch.models_fetch_state = ModelsFetchState::Loading;
        self.fetch.available_models = None;
        self.fetch.fetch_selected.clear();
        let account = self.account.clone();
        let base_url = self.fetch.base_url.clone();
        self.fetch.source = Some((account.clone(), base_url.clone()));
        let (tx, rx) = mpsc::channel();
        self.fetch.models_rx = Some(rx);
        std::thread::spawn(move || {
            let result = access_token(store, account).and_then(|credentials| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime
                    .block_on(providers::list_codex_models(
                        &base_url,
                        &providers::ProviderAuth::new(credentials.access_token),
                        &credentials.account_id,
                    ))
                    .map(expand_fetched_models)
                    .map_err(|error| map_fetch_error(&error))
            });
            let _ = tx.send(result);
        });
    }

    pub fn poll_models(&mut self) -> bool {
        let Some(rx) = self.fetch.models_rx.take() else {
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => {
                self.fetch.models_rx = Some(rx);
                return false;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Model fetch finished without result".into())
            }
        };
        let result = if self
            .fetch
            .source
            .as_ref()
            .is_some_and(|(account, base_url)| {
                account != &self.account || base_url != &self.fetch.base_url
            }) {
            Err("Account or base URL changed during fetch; result discarded".into())
        } else {
            result
        };
        self.fetch.source = None;
        match result {
            Ok(models) if models.is_empty() => {
                self.fetch.available_models = None;
                self.fetch.models_fetch_state = ModelsFetchState::Failed(format!(
                    "Codex backend returned 0 models; pinned client version {} may have been rejected",
                    providers::CODEX_MODELS_CLIENT_VERSION,
                ));
            }
            Ok(models) => {
                let mut seen = BTreeSet::new();
                self.fetch.available_models = Some(
                    models
                        .into_iter()
                        .filter(|id| !id.trim().is_empty() && seen.insert(id.clone()))
                        .collect(),
                );
                self.fetch.models_fetch_state = ModelsFetchState::Loaded;
            }
            Err(error) => {
                self.fetch.available_models = None;
                self.fetch.models_fetch_state = ModelsFetchState::Failed(error);
            }
        }
        true
    }

    pub fn apply_fetched_selection(&mut self) {
        if let Some(models) = &self.fetch.available_models {
            for id in models {
                if self.fetch.fetch_selected.contains(id) && !self.models.contains(id) {
                    self.models.push(id.clone());
                    if self.default_model.is_empty() {
                        self.default_model.clone_from(id);
                    }
                }
            }
        }
        self.fetch.fetch_selected.clear();
    }
}

struct CatalogCredentials {
    access_token: String,
    account_id: String,
}

fn map_fetch_error(error: &providers::ProviderError) -> String {
    use providers::ProviderError;
    match error {
        ProviderError::Http {
            status: 401 | 403, ..
        } => "Codex authorization rejected; Sign in again".into(),
        ProviderError::Http { status, body } => format!(
            "Could not fetch Codex models: HTTP {status}: {}",
            body.chars().take(300).collect::<String>(),
        ),
        ProviderError::Timeout
        | ProviderError::Request(_)
        | ProviderError::Transport { .. }
        | ProviderError::InvalidJson { .. }
        | ProviderError::InvalidSse { .. }
        | ProviderError::RateLimited { .. }
        | ProviderError::RetriesExhausted { .. } => {
            format!("Could not fetch Codex models: {error}")
        }
    }
}

/// 取得したカタログを選択肢 ID へ展開する。fast 対応モデルは通常版の直後に `+fast` 版を並べる。
fn expand_fetched_models(models: Vec<providers::CodexModelInfo>) -> Vec<String> {
    models
        .into_iter()
        .flat_map(|info| {
            let mut ids = vec![info.slug.clone()];
            if info.supports_fast {
                ids.push(config::types::provider::fast_variant_id(&info.slug));
            }
            ids
        })
        .collect()
}

/// モデル選択肢の表示名を返す。fast 版は `<base> (fast)` と表示する。
pub fn model_display_label(id: &str) -> String {
    match config::types::provider::parse_model_speed(id) {
        (base, config::types::provider::ModelSpeed::Fast) => format!("{base} (fast)"),
        _ => id.to_owned(),
    }
}

fn access_token(
    store: Option<Arc<dyn sandbox::CredentialStore>>,
    account: String,
) -> Result<CatalogCredentials, String> {    let store = store.ok_or("Credential store unavailable; restart with keyring access")?;
    let bundle = routing::factory::CredentialStoreTokenStore::new(store, account)
        .load()
        .map_err(|_| "Could not read Codex credentials; Sign in again")?
        .ok_or("No stored Codex credentials; Sign in first")?;
    #[derive(serde::Deserialize)]
    struct Expiry {
        exp: u64,
    }
    let mut segments = bundle.access_token.split('.');
    let payload = match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (Some(_), Some(payload), Some(_), None) => payload,
        _ => return Err("Invalid Codex access token; Sign in again".into()),
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| "Invalid Codex access token; Sign in again")?;
    let expiry: Expiry =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid Codex access token; Sign in again")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock is invalid")?
        .as_secs();
    if expiry.exp <= now {
        return Err("Codex access token expired; Sign in again".into());
    }
    let claims =
        parse_jwt_claims(&bundle.id_token).map_err(|_| "Invalid Codex ID token; Sign in again")?;
    Ok(CatalogCredentials {
        access_token: bundle.access_token,
        account_id: claims.chatgpt_account_id,
    })
}
