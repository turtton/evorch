//! The configuration file behind the `settings` area: how a snapshot is read
//! and how a save is validated and written. Everything here blocks on the file
//! system, so callers run it on the blocking pool, never in a handler.
use config::{
    CodexModelIntent, CodexProviderInput, Config, ConfigError, CredentialRefConfig, LoadOptions,
    ModelEntryConfig, OpenAiCompatibleProviderInput, ProviderCredentialInput,
    ProviderProfileConfig, ProviderTypeConfig, SubscriptionProviderInput,
};
use sha2::{Digest, Sha256};
use std::{
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use workbench_protocol::namespaces::settings::{
    CredentialRef, MAX_DOCUMENT_BYTES, ModelInput, ModelView, OpenaiCompatibleKind, ProviderInput,
    ProviderKind, ProviderView, SettingsProblem, SettingsRevision, SettingsSnapshot,
    SubscriptionKind,
};

pub(super) const CONFIG_FILE: &str = "config.toml";

/// A directory that never exists. A text composed there is the only layer, so
/// two such views differ exactly where the files' own text differs, whatever
/// drop-ins the real directory has.
const LOCAL_VIEW_DIR: &str = "/nonexistent/evorch-settings-local-view";

/// Why a save did not write.
#[derive(Debug)]
pub(super) enum SaveError {
    /// The file is not the revision the client quoted.
    Stale,
    /// The text or input is not acceptable. The message is secret-free.
    Invalid(String),
    /// The restart targets would change.
    RestartTargets,
    /// The file could not be read or written.
    Io(String),
}

impl From<ConfigError> for SaveError {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::Io(_) => Self::Io("the configuration file could not be accessed".into()),
            other => Self::Invalid(other.to_string()),
        }
    }
}

pub(super) struct ConfigStore {
    dir: PathBuf,
    /// The configuration the core started with: the restart-target baseline
    /// when the file on disk does not load.
    startup: Arc<Config>,
}

pub(super) fn revision_of(bytes: &[u8]) -> SettingsRevision {
    SettingsRevision::from_bytes(&Sha256::digest(bytes).into())
}

fn local_options() -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(PathBuf::from(LOCAL_VIEW_DIR)),
        read_env: false,
        ..Default::default()
    }
}

fn local_view(text: &str) -> Result<Config, ConfigError> {
    Config::load_unresolved_with_file_text(
        &local_options(),
        &Path::new(LOCAL_VIEW_DIR).join(CONFIG_FILE),
        text,
    )
}

impl ConfigStore {
    pub(super) fn new(dir: PathBuf, startup: Arc<Config>) -> Self {
        Self { dir, startup }
    }

    fn path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    fn options(&self) -> LoadOptions {
        LoadOptions {
            user_config_dir: Some(self.dir.clone()),
            read_env: false,
            ..Default::default()
        }
    }

    /// The bytes of the file; an absent file is empty.
    fn read(&self) -> Result<Vec<u8>, SaveError> {
        match std::fs::read(self.path()) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(_) => Err(SaveError::Io(
                "the configuration file could not be read".into(),
            )),
        }
    }

    pub(super) fn revision(&self) -> Result<SettingsRevision, SaveError> {
        Ok(revision_of(&self.read()?))
    }

    /// What a client reads now. Never fails: a file that does not read or load
    /// is a `problem` of the snapshot.
    pub(super) fn snapshot(&self, applied: &SettingsRevision) -> SettingsSnapshot {
        let (bytes, mut problem) = match std::fs::read(self.path()) {
            Ok(bytes) => (bytes, None),
            Err(error) if error.kind() == ErrorKind::NotFound => (Vec::new(), None),
            Err(_) => (Vec::new(), Some(SettingsProblem::Unreadable)),
        };
        let revision = revision_of(&bytes);
        let mut document = String::new();
        if bytes.len() > MAX_DOCUMENT_BYTES {
            problem = Some(SettingsProblem::DocumentTooLarge);
        } else if problem.is_none() {
            match String::from_utf8(bytes) {
                Ok(text) => document = text,
                Err(_) => problem = Some(SettingsProblem::Unreadable),
            }
        }
        let mut providers = Vec::new();
        if problem.is_none() {
            match Config::load_unresolved(&self.options()) {
                Ok(config) => providers = config.providers.iter().map(provider_view).collect(),
                Err(_) => problem = Some(SettingsProblem::InvalidConfig),
            }
        }
        SettingsSnapshot {
            revision,
            applied_revision: applied.clone(),
            document,
            providers,
            problem,
        }
    }

    /// Replace the whole file with `text`.
    ///
    /// The caller holds the writer lock, so the revision compare, the checks
    /// and the write are one step with respect to every other settings save.
    pub(super) fn save_document(
        &self,
        expected: &SettingsRevision,
        text: &str,
    ) -> Result<SettingsRevision, SaveError> {
        let current = self.read()?;
        if &revision_of(&current) != expected {
            return Err(SaveError::Stale);
        }
        let current_text = String::from_utf8(current)
            .map_err(|_| SaveError::Io("the configuration file is not text".into()))?;
        let effective = self.compose(text)?;
        self.guard_restart_targets(&current_text, text, &effective)?;
        self.write(text)?;
        Ok(revision_of(text.as_bytes()))
    }

    /// Write one provider through the configuration crate's typed save.
    pub(super) fn save_provider(
        &self,
        expected: &SettingsRevision,
        original_name: Option<&str>,
        input: &ProviderInput,
    ) -> Result<SettingsRevision, SaveError> {
        self.check_revision(expected)?;
        let options = self.options();
        let context = config::LayeredSaveContext::new(&options);
        let path = self.path();
        match input {
            ProviderInput::OpenaiCompatible {
                name,
                kind,
                base_url,
                credential,
                models,
                excluded_models,
                default_model,
            } => context.save_openai_compatible_provider_edit(
                &path,
                &OpenAiCompatibleProviderInput {
                    name: name.clone(),
                    provider_type: match kind {
                        OpenaiCompatibleKind::OpenaiCompatible => {
                            ProviderTypeConfig::OpenAiCompatible
                        }
                        OpenaiCompatibleKind::KimiSubscription => {
                            ProviderTypeConfig::KimiSubscription
                        }
                    },
                    base_url: base_url.clone(),
                    credential: match credential {
                        CredentialRef::Env { var } => {
                            ProviderCredentialInput::Env { var: var.clone() }
                        }
                        CredentialRef::Keyring { service, account } => {
                            ProviderCredentialInput::Keyring {
                                service: service.clone(),
                                account: account.clone(),
                            }
                        }
                    },
                    models: model_entries(models),
                    excluded_models: excluded_models.clone(),
                    default_model: default_model.clone(),
                },
                original_name,
            )?,
            ProviderInput::Codex {
                name,
                account,
                base_url,
                models,
                default_model,
            } => context.save_codex_provider_edit(
                &path,
                &CodexProviderInput {
                    name: name.clone(),
                    account: account.clone(),
                    base_url: base_url.clone(),
                    models: model_entries(models),
                    default_model: default_model.clone(),
                },
                original_name,
                if models.is_empty() {
                    CodexModelIntent::BuiltinDefaults
                } else {
                    CodexModelIntent::ExplicitModels
                },
            )?,
            ProviderInput::Subscription {
                name,
                kind,
                account,
                base_url,
                models,
                excluded_models,
                default_model,
            } => context.save_subscription_provider_edit(
                &path,
                &SubscriptionProviderInput {
                    name: name.clone(),
                    provider_type: match kind {
                        SubscriptionKind::AnthropicSubscription => {
                            ProviderTypeConfig::AnthropicSubscription
                        }
                        SubscriptionKind::Cursor => ProviderTypeConfig::Cursor,
                    },
                    account: account.clone(),
                    base_url: base_url.clone(),
                    models: model_entries(models),
                    excluded_models: excluded_models.clone(),
                    default_model: default_model.clone(),
                },
                original_name,
            )?,
        }
        self.revision()
    }

    pub(super) fn delete_provider(
        &self,
        expected: &SettingsRevision,
        name: &str,
    ) -> Result<SettingsRevision, SaveError> {
        self.check_revision(expected)?;
        let options = self.options();
        config::LayeredSaveContext::new(&options).delete_provider(&self.path(), name)?;
        self.revision()
    }

    fn check_revision(&self, expected: &SettingsRevision) -> Result<(), SaveError> {
        if &self.revision()? == expected {
            Ok(())
        } else {
            Err(SaveError::Stale)
        }
    }

    /// `text` composed as the file in the real layers, with the checks every
    /// typed save makes.
    fn compose(&self, text: &str) -> Result<Config, SaveError> {
        let candidate =
            Config::load_unresolved_with_file_text(&self.options(), &self.path(), text)?;
        if let Some(problem) = candidate.model_kind_problems().into_iter().next() {
            return Err(problem.into());
        }
        Ok(candidate)
    }

    /// The restart targets of the file are the one thing a client may not
    /// change: a target is an executable the core may start. Judged on both the
    /// file's own text and the effective configuration, so neither a key moved
    /// between layers nor a spelling the parser reads differently gets through.
    /// An omission counts as a removal.
    fn guard_restart_targets(
        &self,
        current_text: &str,
        candidate_text: &str,
        candidate_effective: &Config,
    ) -> Result<(), SaveError> {
        let candidate_local = local_view(candidate_text)?;
        let baseline_local = local_view(current_text).unwrap_or_else(|_| (*self.startup).clone());
        let baseline_effective =
            Config::load_unresolved(&self.options()).unwrap_or_else(|_| (*self.startup).clone());
        config::reject_restart_target_changes(&baseline_local, &candidate_local)
            .and(config::reject_restart_target_changes(
                &baseline_effective,
                candidate_effective,
            ))
            .map_err(|_| SaveError::RestartTargets)
    }

    /// Atomic replacement. A leftover temporary file is never overwritten.
    fn write(&self, text: &str) -> Result<(), SaveError> {
        let path = self.path();
        let mut temporary = path.as_os_str().to_os_string();
        temporary.push(".tmp");
        let temporary = Path::new(&temporary);
        let io = |_| SaveError::Io("the configuration file could not be written".into());
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary)
            .map_err(io)?;
        let written = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                drop(file);
                std::fs::rename(temporary, &path)
            });
        if written.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        written.map_err(io)
    }
}

fn model_entries(models: &[ModelInput]) -> Vec<ModelEntryConfig> {
    models
        .iter()
        .map(|model| ModelEntryConfig {
            enabled: model.enabled,
            ..ModelEntryConfig::enabled_with_inferred_kind(model.id.clone())
        })
        .collect()
}

fn provider_view((name, profile): (&String, &ProviderProfileConfig)) -> ProviderView {
    ProviderView {
        name: name.clone(),
        kind: provider_kind(profile.provider_type),
        base_url: profile.base_url.clone(),
        credential: credential_ref(&profile.credential),
        models: profile
            .models
            .iter()
            .map(|model| ModelView {
                id: model.id.clone(),
                enabled: model.enabled,
            })
            .collect(),
        excluded_models: profile.excluded_models.clone(),
        default_model: profile.default_model.clone(),
    }
}

pub(in crate::areas) fn provider_kind(kind: ProviderTypeConfig) -> ProviderKind {
    match kind {
        ProviderTypeConfig::Anthropic => ProviderKind::Anthropic,
        ProviderTypeConfig::AnthropicSubscription => ProviderKind::AnthropicSubscription,
        ProviderTypeConfig::OpenAi => ProviderKind::Openai,
        ProviderTypeConfig::OpenAiCodex => ProviderKind::OpenaiCodex,
        ProviderTypeConfig::GithubCopilot => ProviderKind::GithubCopilot,
        ProviderTypeConfig::Openrouter => ProviderKind::Openrouter,
        ProviderTypeConfig::OpenAiCompatible => ProviderKind::OpenaiCompatible,
        ProviderTypeConfig::KimiSubscription => ProviderKind::KimiSubscription,
        ProviderTypeConfig::Cursor => ProviderKind::Cursor,
    }
}

pub(in crate::areas) fn credential_ref(credential: &CredentialRefConfig) -> CredentialRef {
    match credential {
        CredentialRefConfig::Env { var } => CredentialRef::Env { var: var.clone() },
        CredentialRefConfig::Keyring { service, account } => CredentialRef::Keyring {
            service: service.clone(),
            account: account.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> ConfigStore {
        ConfigStore::new(dir.to_path_buf(), Arc::new(Config::default()))
    }

    const WITH_TARGET: &str = "version = 2\n[core.restart.targets.web]\npath = \"/usr/bin/true\"\n";

    #[test]
    fn the_local_view_never_reads_a_neighbour_of_the_real_directory() {
        let a = local_view("[core]\nidle_grace_secs = 7\n").unwrap();
        assert_eq!(a.core.idle_grace_secs, 7);
        assert!(local_view("this is not toml").is_err());
    }

    #[test]
    fn a_save_changes_the_revision_and_a_stale_one_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let empty = store.revision().unwrap();
        let first = store
            .save_document(&empty, "version = 2\n[core]\nidle_grace_secs = 5\n")
            .unwrap();
        assert_ne!(first, empty);
        assert!(matches!(
            store.save_document(&empty, "version = 2\n"),
            Err(SaveError::Stale)
        ));
        assert_eq!(store.revision().unwrap(), first);
    }

    #[test]
    fn restart_targets_are_not_changed_by_a_whole_text_save() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), WITH_TARGET).unwrap();
        let store = store(dir.path());
        let revision = store.revision().unwrap();
        for text in [
            "version = 2\n",
            "version = 2\n[core.restart.targets.web]\npath = \"/bin/sh\"\n",
            "version = 2\n[core.restart.targets.other]\npath = \"/usr/bin/true\"\n",
            "version = 2\n[core.restart.targets.web]\npath = \"/usr/bin/true\"\n[core.restart.targets.extra]\npath = \"/bin/ls\"\n",
            "version = 2\ncore = { restart = { targets = {} } }\n",
        ] {
            assert!(
                matches!(
                    store.save_document(&revision, text),
                    Err(SaveError::RestartTargets)
                ),
                "{text}"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(),
                WITH_TARGET
            );
        }
        // Keeping them as they are, in another spelling, is allowed.
        let kept = "version = 2\n[core]\nidle_grace_secs = 9\n[core.restart.targets.web]\npath = \"/usr/bin/true\"\n";
        assert!(store.save_document(&revision, kept).is_ok());
    }

    #[test]
    fn a_text_that_does_not_load_is_refused_and_the_file_stays() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let revision = store.revision().unwrap();
        assert!(matches!(
            store.save_document(&revision, "version = 2\n[core\n"),
            Err(SaveError::Invalid(_))
        ));
        assert!(!dir.path().join(CONFIG_FILE).exists());
    }
}
