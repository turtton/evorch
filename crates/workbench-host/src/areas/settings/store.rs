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
    CredentialRef, MAX_DOCUMENT_BYTES, ModelInput, ModelKind, ModelView, OpenaiCompatibleKind,
    ProviderInput, ProviderKind, ProviderView, SettingsProblem, SettingsRevision, SettingsSnapshot,
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
    /// The file has no such provider.
    NoSuchProvider,
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

    /// The configuration the core started with.
    pub(super) fn startup(&self) -> &Config {
        &self.startup
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
    pub(super) fn snapshot(&self, applied: Option<&SettingsRevision>) -> SettingsSnapshot {
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
            applied_revision: applied.cloned(),
            apply_failure: None,
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
        // The core starts with the strict load, which refuses what `compose`
        // only drops (an unknown field): a file that passed only the lenient
        // one would be saved and then not start.
        self.strict_check(text)?;
        self.write(text)?;
        Ok(revision_of(text.as_bytes()))
    }

    /// The effective configuration the files hold now (every layer but the project's),
    /// with the revision of the file it was read after.
    pub(super) fn effective(&self) -> Result<(SettingsRevision, Config), SaveError> {
        let revision = self.revision()?;
        Ok((revision, Config::load_unresolved(&self.options())?))
    }

    /// The configuration as the core loads it at start, read again after a write.
    pub(super) fn effective_strict(&self) -> Result<Config, SaveError> {
        Ok(Config::load_strict(&self.options())?)
    }

    /// A typed save made by editing the text of the file.
    ///
    /// The caller holds the writer lock. `edit` gets the current text and the effective
    /// configuration and returns the new text; `verify` judges the effective configuration
    /// the new text composes (a key another layer overrides is refused here). The checks
    /// are those of a whole-text save: the revision, the restart targets and the strict
    /// load. Nothing is written when the text does not change.
    pub(super) fn save_edited(
        &self,
        expected: &SettingsRevision,
        edit: impl FnOnce(&str, &Config) -> Result<String, SaveError>,
        verify: impl FnOnce(&Config) -> Result<(), SaveError>,
    ) -> Result<SettingsRevision, SaveError> {
        let current = self.read()?;
        if &revision_of(&current) != expected {
            return Err(SaveError::Stale);
        }
        let current_text = String::from_utf8(current)
            .map_err(|_| SaveError::Io("the configuration file is not text".into()))?;
        let baseline = Config::load_unresolved(&self.options())?;
        let text = edit(&current_text, &baseline)?;
        let effective = self.compose(&text)?;
        verify(&effective)?;
        self.guard_restart_targets(&current_text, &text, &effective)?;
        self.strict_check(&text)?;
        if text != current_text {
            self.write(&text)?;
        }
        Ok(revision_of(text.as_bytes()))
    }

    /// Write one provider through the configuration crate's typed save.
    pub(super) fn save_provider(
        &self,
        expected: &SettingsRevision,
        original_name: Option<&str>,
        input: &ProviderInput,
    ) -> Result<Saved, SaveError> {
        self.check_revision(expected)?;
        let existing = self.existing_models(original_name.unwrap_or_else(|| input_name(input)))?;
        let before = Config::load_unresolved(&self.options()).ok();
        let effective = before
            .as_ref()
            .and_then(|config| {
                config
                    .providers
                    .get(original_name.unwrap_or_else(|| input_name(input)))
            })
            .map(|profile| profile.models.clone())
            .unwrap_or_default();
        self.typed_save(|| {
            self.save_provider_unchecked(original_name, input, &existing, &effective)
        })?;
        let after = Config::load_unresolved(&self.options()).ok();
        Ok(Saved {
            revision: self.revision()?,
            models_unchanged: before
                .zip(after)
                .is_some_and(|(before, after)| same_but_quota(before, after)),
        })
    }

    /// The provider `name` of the file at `expected`, as the effective configuration holds it.
    pub(super) fn provider_at(
        &self,
        expected: &SettingsRevision,
        name: &str,
    ) -> Result<ProviderProfileConfig, SaveError> {
        self.check_revision(expected)?;
        Config::load_unresolved(&self.options())?
            .providers
            .remove(name)
            .ok_or(SaveError::NoSuchProvider)
    }

    fn save_provider_unchecked(
        &self,
        original_name: Option<&str>,
        input: &ProviderInput,
        existing: &[ModelEntryConfig],
        effective: &[ModelEntryConfig],
    ) -> Result<(), ConfigError> {
        let options = self.options();
        let context = config::LayeredSaveContext::new(&options).with_show_quota(match input {
            ProviderInput::OpenaiCompatible { show_quota, .. }
            | ProviderInput::Codex { show_quota, .. }
            | ProviderInput::Subscription { show_quota, .. } => *show_quota,
        });
        let path = self.path();
        let model_entries = |models: &[ModelInput]| model_entries(models, existing, effective);
        match input {
            ProviderInput::OpenaiCompatible {
                name,
                kind,
                base_url,
                credential,
                models,
                excluded_models,
                default_model,
                ..
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
                ..
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
                ..
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
        Ok(())
    }

    /// The models the file itself (not its drop-ins) lists for provider `name`, read as TOML
    /// without validating any other setting. A file whose models cannot be read is not saved
    /// over: that would replace entries whose detail this save cannot see.
    fn existing_models(&self, name: &str) -> Result<Vec<ModelEntryConfig>, SaveError> {
        let text = String::from_utf8(self.read()?)
            .map_err(|_| SaveError::Io("the configuration file is not text".into()))?;
        // The reason is fixed: a parse error would quote the file, which may hold secrets.
        config::provider_models_in_text(&text, name)
            .map(Option::unwrap_or_default)
            .map_err(|_| {
                SaveError::Invalid(format!(
                    "the models of provider `{name}` in the configuration file could not be read, so the provider was not saved over them"
                ))
            })
    }

    pub(super) fn delete_provider(
        &self,
        expected: &SettingsRevision,
        name: &str,
    ) -> Result<SettingsRevision, SaveError> {
        self.check_revision(expected)?;
        self.typed_save(|| {
            let options = self.options();
            config::LayeredSaveContext::new(&options).delete_provider(&self.path(), name)
        })?;
        self.revision()
    }

    /// A typed save of the configuration crate, held to the strict load the
    /// core starts with. A file that does not pass it already is not edited
    /// (the result would not start either); a result that does not pass is
    /// put back as it was.
    fn typed_save(&self, save: impl FnOnce() -> Result<(), ConfigError>) -> Result<(), SaveError> {
        let original = self.read()?;
        let original_text = String::from_utf8(original)
            .map_err(|_| SaveError::Io("the configuration file is not text".into()))?;
        self.strict_check(&original_text)?;
        save()?;
        let saved = self.read()?;
        let checked = String::from_utf8(saved)
            .map_err(|_| SaveError::Io("the configuration file is not text".into()))
            .and_then(|text| self.strict_check(&text));
        if let Err(error) = checked {
            self.write(&original_text)?;
            return Err(error);
        }
        Ok(())
    }

    /// `text` as the file, loaded the way the core loads its configuration:
    /// strictly, with the drop-ins and the project layer the real directory
    /// has. Composed in a scratch directory so the file itself is not touched.
    fn strict_check(&self, text: &str) -> Result<(), SaveError> {
        let scratch = Scratch::new()?;
        let io = |_| SaveError::Io("the configuration could not be checked".into());
        write_private(&scratch.path().join(CONFIG_FILE), text.as_bytes()).map_err(io)?;
        let drop_ins = self.dir.join("config.d");
        if let Ok(entries) = std::fs::read_dir(&drop_ins) {
            let target = scratch.path().join("config.d");
            create_private_dir(&target).map_err(io)?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "toml") && path.is_file() {
                    let bytes = std::fs::read(&path).map_err(io)?;
                    write_private(&target.join(entry.file_name()), &bytes).map_err(io)?;
                }
            }
        }
        let options = LoadOptions {
            user_config_dir: Some(scratch.path().to_path_buf()),
            ..self.options()
        };
        Config::load_strict(&options)?;
        Ok(())
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
        // The replacement keeps the mode of the file it replaces: a file made
        // readable by its owner only does not become readable by others.
        if let Ok(existing) = std::fs::metadata(&path)
            && let Err(error) = file.set_permissions(existing.permissions())
        {
            drop(file);
            let _ = std::fs::remove_file(temporary);
            return Err(io(error));
        }
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

/// A new directory only its owner can enter (the copy of a configuration is
/// as private as the configuration itself).
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// A new file only its owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

/// A directory for one check, removed afterwards.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, SaveError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "evorch-settings-check-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        create_private_dir(&path)
            .map_err(|_| SaveError::Io("the configuration could not be checked".into()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The entries a client's model list stands for. A client names a model by id and says
/// whether it is enabled, its kind and its context override; whatever else the file already
/// says about that id (preset, prices, effort levels, edit tool) is kept.
///
/// What the client left as the effective configuration (`effective`: the file with its
/// drop-ins and the provider's defaults) already has it is not written over what the file
/// says: a context window that a drop-in overrides stays as the file has it, and a model that
/// only the defaults list is not given an explicit kind, so a save that edits nothing of the
/// models writes the same models back.
fn model_entries(
    models: &[ModelInput],
    existing: &[ModelEntryConfig],
    effective: &[ModelEntryConfig],
) -> Vec<ModelEntryConfig> {
    models
        .iter()
        .map(|model| {
            let in_file = existing.iter().find(|entry| entry.id == model.id);
            let was = effective.iter().find(|entry| entry.id == model.id);
            let mut entry = match (in_file, was) {
                (Some(entry), _) => entry.clone(),
                (None, Some(_)) => ModelEntryConfig::enabled(model.id.clone()),
                (None, None) => ModelEntryConfig::enabled_with_inferred_kind(model.id.clone()),
            };
            entry.enabled = model.enabled;
            if let Some(kind) = model.kind {
                let kind = match kind {
                    ModelKind::Chat => config::ModelKind::Chat,
                    ModelKind::ImageGeneration => config::ModelKind::ImageGeneration,
                };
                if was.is_none_or(|was| was.effective_kind() != kind) {
                    entry.kind = Some(kind);
                }
            }
            match was {
                Some(was) if was.context_window == model.context_override => {}
                _ => entry.context_window = model.context_override,
            }
            entry
        })
        .collect()
}

fn input_name(input: &ProviderInput) -> &str {
    match input {
        ProviderInput::OpenaiCompatible { name, .. }
        | ProviderInput::Codex { name, .. }
        | ProviderInput::Subscription { name, .. } => name,
    }
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
                kind: match model.effective_kind() {
                    config::ModelKind::Chat => ModelKind::Chat,
                    config::ModelKind::ImageGeneration => ModelKind::ImageGeneration,
                },
                context_override: model.context_window,
            })
            .collect(),
        excluded_models: profile.excluded_models.clone(),
        default_model: profile.default_model.clone(),
        show_quota: profile.show_quota_enabled(),
        quota_supported: profile.quota_supported(),
    }
}

/// What a provider save wrote.
pub(super) struct Saved {
    pub revision: SettingsRevision,
    /// The effective configuration is the same but for `show_quota`, so the
    /// models the runs use do not differ from what the file now says.
    pub models_unchanged: bool,
}

fn same_but_quota(mut before: Config, mut after: Config) -> bool {
    for profile in before
        .providers
        .values_mut()
        .chain(after.providers.values_mut())
    {
        profile.show_quota = None;
    }
    before == after
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
    fn the_wire_inference_of_a_model_kind_is_the_one_of_the_configuration() {
        for id in [
            "gpt-image-2",
            "GPT-IMAGE-1",
            " dall-e-3",
            "chatgpt-image-latest",
            "gpt-5",
            "image-gpt",
            "",
        ] {
            let wire = match config::ModelKind::infer(id) {
                config::ModelKind::Chat => ModelKind::Chat,
                config::ModelKind::ImageGeneration => ModelKind::ImageGeneration,
            };
            assert_eq!(ModelKind::infer(id), wire, "{id:?}");
        }
        assert_eq!(
            workbench_protocol::namespaces::settings::IMAGE_GENERATION_MODEL_PREFIXES,
            config::types::provider::IMAGE_GENERATION_MODEL_PREFIXES
        );
    }

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

    #[cfg(unix)]
    /// The copy a strict check makes is as private as the configuration: only
    /// its owner can enter the directory or read the files, whatever the umask.
    #[test]
    fn the_scratch_copy_of_a_configuration_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new().unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(scratch.path()), 0o700);
        let file = scratch.path().join(CONFIG_FILE);
        write_private(&file, b"version = 3\n").unwrap();
        assert_eq!(mode(&file), 0o600);
        let nested = scratch.path().join("config.d");
        create_private_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
    }
}
