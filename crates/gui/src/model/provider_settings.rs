use std::collections::BTreeMap;

use super::codex_auth::CodexAuthModel;

#[path = "provider_openai_editor.rs"]
mod openai;
pub use openai::ProviderSettingsModel as OpenAiEditorModel;
pub use openai::{CredentialMode, ModelsFetchState, ProviderSettingsTab, provider_status_of};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiCompatible,
    CodexSubscription,
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
    pub account: String,
    pub auth: CodexAuthModel,
    pub models: Vec<String>,
    pub default_model: String,
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
            ..Self::default()
        }
    }

    pub fn add(&mut self, kind: ProviderKind) {
        let prefix = match kind {
            ProviderKind::OpenAiCompatible => "openai-compat",
            ProviderKind::CodexSubscription => "codex",
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
            ProviderKind::CodexSubscription => ProfileEditor::Codex(CodexEditorModel {
                account: name.clone(),
                auth: CodexAuthModel::for_account(name.clone()),
                name,
                models: Vec::new(),
                default_model: String::new(),
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
            config::ProviderTypeConfig::OpenAiCompatible | config::ProviderTypeConfig::OpenAiCodex
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
                    auth: CodexAuthModel::for_account(account.clone()),
                    account,
                    models: profile
                        .models
                        .iter()
                        .map(|model| model.id.clone())
                        .collect(),
                    default_model: profile.default_model.clone(),
                })
            }
            _ => {
                let mut cfg = config::Config::default();
                let mut profile = profile.clone();
                profile.provider_type = config::ProviderTypeConfig::OpenAiCompatible;
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
        if let Some(editor) = self.openai_mut() {
            editor.start_models_fetch_with_store(store);
        }
    }

    pub fn poll_models(&mut self) -> bool {
        self.openai_mut()
            .is_some_and(OpenAiEditorModel::poll_models)
    }
}
