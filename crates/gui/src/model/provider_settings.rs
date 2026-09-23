use std::collections::BTreeMap;

use super::codex_auth::CodexAuthModel;

#[path = "provider_codex_models.rs"]
mod codex_models;
#[path = "provider_openai_editor.rs"]
mod openai;
pub use codex_models::model_display_label;
pub use codex_models::{CodexFetchedModels, CodexModelsFetch};
pub use openai::ProviderSettingsModel as OpenAiEditorModel;
pub use openai::{CredentialMode, ModelsFetchState, ProviderSettingsTab, provider_status_of};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiCompatible,
    CodexSubscription,
    KimiSubscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub name: String,
    pub kind: ProviderKind,
    pub default_model: String,
    pub models: Vec<String>,
    pub provider_type: config::ProviderTypeConfig,
}

#[derive(Debug)]
pub struct CodexEditorModel {
    pub name: String,
    pub original_name: Option<String>,
    pub account: String,
    pub auth: CodexAuthModel,
    pub models: Vec<String>,
    pub model_entries: BTreeMap<String, config::ModelEntryConfig>,
    pub default_model: String,
    pub fetch: CodexModelsFetch,
}

#[derive(Debug)]
pub enum ProfileEditor {
    OpenAiCompatible(OpenAiEditorModel),
    Codex(CodexEditorModel),
}

#[derive(Debug, Default)]
pub struct ProviderSettingsModel {
    pub open: bool,
    pub profiles: Vec<ProfileSummary>,
    pub editor: Option<ProfileEditor>,
    pub error: Option<String>,
    pub confirm_delete: Option<String>,
    pub model_presets: BTreeMap<String, config::ModelPresetConfig>,
    pub catalog: super::model_catalog::CatalogState,
    entries: BTreeMap<String, config::ProviderProfileConfig>,
}

impl ProviderSettingsModel {
    pub fn seed_from_config(config: &config::Config) -> Self {
        Self {
            profiles: config
                .providers
                .iter()
                .map(|(name, profile)| ProfileSummary {
                    name: name.clone(),
                    kind: match profile.provider_type {
                        config::ProviderTypeConfig::OpenAiCodex => ProviderKind::CodexSubscription,
                        config::ProviderTypeConfig::KimiSubscription => {
                            ProviderKind::KimiSubscription
                        }
                        _ => ProviderKind::OpenAiCompatible,
                    },
                    default_model: profile.default_model.clone(),
                    models: profile
                        .models
                        .iter()
                        .map(|model| model.id.clone())
                        .collect(),
                    provider_type: profile.provider_type,
                })
                .collect(),
            entries: config.providers.clone(),
            model_presets: config.model_presets.clone(),
            ..Self::default()
        }
    }

    pub fn add(&mut self, kind: ProviderKind) {
        let prefix = match kind {
            ProviderKind::OpenAiCompatible => "openai-compat",
            ProviderKind::CodexSubscription => "codex",
            ProviderKind::KimiSubscription => "kimi",
        };
        let mut name = prefix.to_owned();
        let mut suffix = 2;
        while self.entries.contains_key(&name) {
            name = format!("{prefix}-{suffix}");
            suffix += 1;
        }
        self.editor = Some(match kind {
            ProviderKind::OpenAiCompatible => ProfileEditor::OpenAiCompatible(OpenAiEditorModel {
                name,
                ..Default::default()
            }),
            ProviderKind::KimiSubscription => ProfileEditor::OpenAiCompatible(OpenAiEditorModel {
                name,
                provider_type: config::ProviderTypeConfig::KimiSubscription,
                base_url: config::types::provider::KIMI_DEFAULT_BASE_URL.to_owned(),
                models: config::types::provider::KIMI_DEFAULT_MODELS
                    .iter()
                    .map(|id| config::types::provider::ModelEntryConfig::enabled(*id))
                    .collect(),
                default_model: config::types::provider::KIMI_DEFAULT_MODEL.to_owned(),
                ..Default::default()
            }),
            ProviderKind::CodexSubscription => ProfileEditor::Codex(CodexEditorModel {
                original_name: None,
                account: name.clone(),
                auth: CodexAuthModel::for_account(name.clone()),
                name,
                models: config::types::provider::CODEX_DEFAULT_MODELS
                    .iter()
                    .map(|id| (*id).to_owned())
                    .collect(),
                model_entries: BTreeMap::new(),
                default_model: config::types::provider::CODEX_DEFAULT_MODEL.to_owned(),
                fetch: CodexModelsFetch::default(),
            }),
        });
        self.error = None;
        self.confirm_delete = None;
    }

    pub fn edit(&mut self, name: &str) {
        let Some(profile) = self.entries.get(name) else {
            return;
        };
        if !matches!(
            profile.provider_type,
            config::ProviderTypeConfig::OpenAiCompatible
                | config::ProviderTypeConfig::OpenAiCodex
                | config::ProviderTypeConfig::KimiSubscription
        ) {
            self.error = Some(
                "This provider type must be edited in evorch.toml; its configuration is preserved."
                    .into(),
            );
            return;
        }
        self.editor = Some(match profile.provider_type {
            config::ProviderTypeConfig::OpenAiCodex => {
                let account = match &profile.credential {
                    config::CredentialRefConfig::Keyring { account, .. } => account.clone(),
                    config::CredentialRefConfig::Env { .. } => name.into(),
                };
                ProfileEditor::Codex(CodexEditorModel {
                    name: name.into(),
                    original_name: Some(name.into()),
                    auth: CodexAuthModel::for_account(account.clone()),
                    account,
                    models: profile
                        .models
                        .iter()
                        .map(|model| model.id.clone())
                        .collect(),
                    model_entries: profile
                        .models
                        .iter()
                        .map(|model| (model.id.clone(), model.clone()))
                        .collect(),
                    default_model: profile.default_model.clone(),
                    fetch: CodexModelsFetch {
                        base_url: profile.base_url.clone(),
                        ..Default::default()
                    },
                })
            }
            _ => {
                let mut cfg = config::Config::default();
                let profile = profile.clone();
                cfg.providers.insert(name.into(), profile);
                ProfileEditor::OpenAiCompatible(OpenAiEditorModel::seed_from_config(&cfg))
            }
        });
        self.error = None;
        self.confirm_delete = None;
    }

    pub fn credential(&self, name: &str) -> Option<&config::CredentialRefConfig> {
        self.entries.get(name).map(|profile| &profile.credential)
    }

    pub fn model_entry(&self, profile: &str, model: &str) -> Option<&config::ModelEntryConfig> {
        self.entries
            .get(profile)?
            .models
            .iter()
            .find(|entry| entry.id == model)
    }

    pub fn provider_type(&self, profile: Option<&str>) -> Option<config::ProviderTypeConfig> {
        let name = profile?;
        self.profiles
            .iter()
            .find(|profile| profile.name == name)
            .map(|profile| profile.provider_type)
    }

    pub fn openai_mut(&mut self) -> Option<&mut OpenAiEditorModel> {
        match &mut self.editor {
            Some(ProfileEditor::OpenAiCompatible(editor)) => Some(editor),
            Some(ProfileEditor::Codex(_)) | None => None,
        }
    }

    pub fn openai(&self) -> Option<&OpenAiEditorModel> {
        match &self.editor {
            Some(ProfileEditor::OpenAiCompatible(editor)) => Some(editor),
            Some(ProfileEditor::Codex(_)) | None => None,
        }
    }

    pub fn codex_mut(&mut self) -> Option<&mut CodexEditorModel> {
        match &mut self.editor {
            Some(ProfileEditor::Codex(editor)) => Some(editor),
            Some(ProfileEditor::OpenAiCompatible(_)) | None => None,
        }
    }

    pub fn start_models_fetch_with_store(
        &mut self,
        store: Option<std::sync::Arc<dyn sandbox::CredentialStore>>,
    ) {
        match &mut self.editor {
            Some(ProfileEditor::OpenAiCompatible(editor)) => {
                editor.start_models_fetch_with_store(store)
            }
            Some(ProfileEditor::Codex(editor)) => editor.start_models_fetch_with_store(store),
            None => {}
        }
    }

    pub fn poll_models(&mut self) -> bool {
        match &mut self.editor {
            Some(ProfileEditor::OpenAiCompatible(editor)) => editor.poll_models(),
            Some(ProfileEditor::Codex(editor)) => editor.poll_models(),
            None => false,
        }
    }
}
