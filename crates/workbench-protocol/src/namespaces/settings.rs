//! Settings namespace: the user configuration file, provider profiles and the
//! reload that applies a saved configuration to the running core (ADR 0045,
//! PR 10).
//!
//! ## Shape
//!
//! * The file is the truth. The core reads `config.toml` of its user
//!   configuration directory and answers with its text, the provider profiles
//!   it holds and two [`SettingsRevision`]s: the one *saved* in the file and the
//!   one *applied* to the models the runs use. They differ between a save and a
//!   `reload_models`.
//! * Every state-changing command carries a client-chosen
//!   [`OperationId`](crate::ids::OperationId) and is answered at once with the
//!   operation in state `pending` (the shared contract of
//!   [`crate::operation`]). The file is read and written on a worker, not in the
//!   handler; the end arrives as `operation_completed` and stays readable with
//!   `get_operation`.
//! * A save quotes the revision the client read ([`SettingsRevision`], the hash
//!   of the file). A save quoting an older one is refused with
//!   `StaleRevision` and writes nothing, so two clients cannot lose each other's
//!   update.
//! * `save_document` replaces the whole file; `save_provider` and
//!   `delete_provider` change one profile and leave the rest of the file as it
//!   is. Both go through the same checks the core applies when it starts, and
//!   **neither can change `core.restart.targets`** (the executables a planned
//!   restart may start): a document that adds, removes, changes or omits a
//!   target is refused whoever sent it. Only a local edit of the file changes
//!   them. The check runs against the file as it is when the revision is
//!   compared, inside the same critical section as the write.
//! * No secret is part of this namespace. A credential is only a reference
//!   (environment variable name, keychain service and account); the secret
//!   itself belongs to the `auth` namespace.
mod feature_options;
pub use feature_options::*;

use crate::{
    handshake::{Capability, WireLimits},
    ids::OperationId,
    limits::MAX_NAME_BYTES,
    operation::OperationView,
    validation::{ValidationError, check_count, check_line, check_text},
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "refresh_settings",
    "save_document",
    "save_provider",
    "delete_provider",
    "reload_models",
    "save_feature_settings",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "model_choices",
    "get_settings_snapshot",
    "get_feature_settings",
    "get_settings_operation",
    "list_settings_operations",
];
pub const EVENT_TYPES: &[&str] = &["settings_snapshot_changed", "settings_operation_completed"];
/// The tagged choice inside `save_provider`'s payload that classification names.
pub const PROVIDER_INPUT_KINDS: &[&str] = &["openai_compatible", "codex", "subscription"];
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[super::NestedOperation {
    operation: "save_provider",
    pointer: "/provider/type",
    known: PROVIDER_INPUT_KINDS,
}];

/// Largest configuration text, in bytes. Larger files are not shown or saved
/// through the wire (the frame budget doubles for JSON escaping).
pub const MAX_DOCUMENT_BYTES: usize = 128 * 1024;
pub const MAX_PROVIDERS: usize = 64;
pub const MAX_MODELS: usize = 256;
/// Most operations a core keeps and lists.
pub const MAX_OPERATIONS: usize = 64;
/// Longest text of an operation's message.
pub const MAX_MESSAGE_BYTES: usize = 1024;
const MAX_VALUE_BYTES: usize = 2048;

fn invalid(field: &'static str, reason: &'static str) -> ValidationError {
    ValidationError::OutOfRange { field, reason }
}

/// The identity of the file's content: the SHA-256 of its bytes, in lowercase
/// hex (an absent file hashes as an empty one). Compare for equality only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SettingsRevision(String);

impl SettingsRevision {
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Parse lowercase hex of the fixed length.
    ///
    /// # Errors
    /// Anything else.
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid(
                "SettingsRevision",
                "expected 64 lowercase hex digits",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SettingsRevision {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SettingsRevision> for String {
    fn from(value: SettingsRevision) -> Self {
        value.0
    }
}

/// The kind of a provider profile, spelled as the configuration spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "anthropic_subscription")]
    AnthropicSubscription,
    #[serde(rename = "openai")]
    Openai,
    #[serde(rename = "openai_codex")]
    OpenaiCodex,
    #[serde(rename = "github_copilot")]
    GithubCopilot,
    #[serde(rename = "openrouter")]
    Openrouter,
    #[serde(rename = "openai_compatible")]
    OpenaiCompatible,
    #[serde(rename = "kimi_subscription")]
    KimiSubscription,
    #[serde(rename = "cursor")]
    Cursor,
}

/// Where a provider's credential comes from. A reference, never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialRef {
    Env { var: String },
    Keyring { service: String, account: String },
}

impl CredentialRef {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Env { var } => check_line("credential.var", var, MAX_NAME_BYTES, false),
            Self::Keyring { service, account } => {
                check_line("credential.service", service, MAX_NAME_BYTES, false)?;
                check_line("credential.account", account, MAX_NAME_BYTES, false)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelView {
    pub id: String,
    pub enabled: bool,
}

/// One provider profile as the effective configuration holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderView {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub credential: CredentialRef,
    pub models: Vec<ModelView>,
    pub excluded_models: Vec<String>,
    pub default_model: String,
}

/// Why a snapshot carries no usable document or providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsProblem {
    /// The file could not be read.
    Unreadable,
    /// The file does not load as a configuration (the text is still shown).
    InvalidConfig,
    /// The file is larger than [`MAX_DOCUMENT_BYTES`]; its text is not shown.
    DocumentTooLarge,
}

/// What a client reads: the file, and how far the running core follows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSnapshot {
    /// Saved in the file now.
    pub revision: SettingsRevision,
    /// Applied to the models the runs use. Differs from `revision` after a
    /// save until `reload_models` succeeded (or the core restarted). `None`
    /// when it is not known: a reload that failed may have replaced some of
    /// the models, so no file is known to be what the runs use (the reason is
    /// `apply_failure`). Saving a file whose hash equals an older one does not
    /// make it applied; only a reload that succeeds does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_revision: Option<SettingsRevision>,
    /// Why the last reload failed, while `applied_revision` is unknown. A
    /// bounded, secret-free sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_failure: Option<String>,
    /// The text of `config.toml`. Empty when the file does not exist.
    pub document: String,
    pub providers: Vec<ProviderView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<SettingsProblem>,
}

impl SettingsSnapshot {
    /// Whether the saved file is not known to be what the runs use: a save is
    /// waiting for `reload_models`, or the last reload failed.
    pub fn pending_apply(&self) -> bool {
        self.applied_revision.as_ref() != Some(&self.revision)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsOperationKind {
    Refresh,
    SaveDocument,
    SaveProvider,
    DeleteProvider,
    ReloadModels,
    SaveFeatureSettings,
}

/// An operation with what a settings client needs to explain its end. The
/// `message` is a bounded, secret-free sentence (a validation failure, say);
/// the state itself carries only the failure class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsOperationView {
    pub operation: OperationView,
    pub kind: SettingsOperationKind,
    /// The revision a `Ready` save wrote (`reload_models`: the one it applied).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<SettingsRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl SettingsOperationView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.operation.validate()?;
        if let Some(message) = &self.message {
            check_text("message", message, MAX_MESSAGE_BYTES, true)?;
        }
        Ok(())
    }
}

// ---- commands -------------------------------------------------------------

/// Read the file and the effective configuration again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshSettings {
    pub operation_id: OperationId,
}

/// Replace the whole file with `text`. The text must load exactly as the core
/// loads its configuration at start, and must keep `core.restart.targets` as it
/// is in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveDocument {
    pub operation_id: OperationId,
    pub expected_revision: SettingsRevision,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInput {
    pub id: String,
    pub enabled: bool,
}

/// The profile a `save_provider` writes. A model that the profile already has
/// keeps its other settings (preset, prices); only `enabled` follows the input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderInput {
    /// An OpenAI-compatible endpoint (or Kimi) with an environment variable or
    /// keychain entry as credential.
    OpenaiCompatible {
        name: String,
        kind: OpenaiCompatibleKind,
        base_url: String,
        credential: CredentialRef,
        models: Vec<ModelInput>,
        excluded_models: Vec<String>,
        default_model: String,
    },
    /// OpenAI Codex (ChatGPT subscription); the keychain account holds the login.
    Codex {
        name: String,
        account: String,
        base_url: String,
        models: Vec<ModelInput>,
        default_model: String,
    },
    /// Claude or Cursor subscription; the keychain account holds the login.
    Subscription {
        name: String,
        kind: SubscriptionKind,
        account: String,
        base_url: String,
        models: Vec<ModelInput>,
        excluded_models: Vec<String>,
        default_model: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenaiCompatibleKind {
    OpenaiCompatible,
    KimiSubscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionKind {
    AnthropicSubscription,
    Cursor,
}

impl ProviderInput {
    /// The profile name the input writes.
    pub fn name(&self) -> &str {
        match self {
            Self::OpenaiCompatible { name, .. }
            | Self::Codex { name, .. }
            | Self::Subscription { name, .. } => name,
        }
    }

    fn validate(&self) -> Result<(), ValidationError> {
        let (name, base_url, models, excluded, default_model) = match self {
            Self::OpenaiCompatible {
                name,
                base_url,
                credential,
                models,
                excluded_models,
                default_model,
                ..
            } => {
                credential.validate()?;
                (
                    name,
                    base_url,
                    models,
                    excluded_models.as_slice(),
                    default_model,
                )
            }
            Self::Codex {
                name,
                account,
                base_url,
                models,
                default_model,
            } => {
                check_line("account", account, MAX_NAME_BYTES, false)?;
                (name, base_url, models, &[][..], default_model)
            }
            Self::Subscription {
                name,
                account,
                base_url,
                models,
                excluded_models,
                default_model,
                ..
            } => {
                check_line("account", account, MAX_NAME_BYTES, false)?;
                (
                    name,
                    base_url,
                    models,
                    excluded_models.as_slice(),
                    default_model,
                )
            }
        };
        check_line("name", name, MAX_NAME_BYTES, false)?;
        check_line("base_url", base_url, MAX_VALUE_BYTES, false)?;
        check_line("default_model", default_model, MAX_NAME_BYTES, true)?;
        check_count("models", models.len(), MAX_MODELS)?;
        for model in models {
            check_line("model", &model.id, MAX_NAME_BYTES, false)?;
        }
        check_count("excluded_models", excluded.len(), MAX_MODELS)?;
        for model in excluded {
            check_line("excluded_model", model, MAX_NAME_BYTES, false)?;
        }
        Ok(())
    }
}

/// Write `provider`, replacing the profile named `original_name` (a rename) or,
/// without it, the profile of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveProvider {
    pub operation_id: OperationId,
    pub expected_revision: SettingsRevision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    pub provider: ProviderInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteProvider {
    pub operation_id: OperationId,
    pub expected_revision: SettingsRevision,
    pub name: String,
}

/// Apply the saved file to the models the requests that follow use, the next
/// request of a running conversation included. What was already sent is kept:
/// the reload never rewrites a provider prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadModels {
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingsCommand {
    RefreshSettings(RefreshSettings),
    SaveDocument(SaveDocument),
    SaveProvider(Box<SaveProvider>),
    DeleteProvider(DeleteProvider),
    ReloadModels(ReloadModels),
    SaveFeatureSettings(SaveFeatureSettings),
}

impl SettingsCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    /// The operation this command names.
    pub fn operation_id(&self) -> &OperationId {
        match self {
            Self::RefreshSettings(c) => &c.operation_id,
            Self::SaveDocument(c) => &c.operation_id,
            Self::SaveProvider(c) => &c.operation_id,
            Self::DeleteProvider(c) => &c.operation_id,
            Self::ReloadModels(c) => &c.operation_id,
            Self::SaveFeatureSettings(c) => &c.operation_id,
        }
    }

    pub fn kind(&self) -> SettingsOperationKind {
        match self {
            Self::RefreshSettings(_) => SettingsOperationKind::Refresh,
            Self::SaveDocument(_) => SettingsOperationKind::SaveDocument,
            Self::SaveProvider(_) => SettingsOperationKind::SaveProvider,
            Self::DeleteProvider(_) => SettingsOperationKind::DeleteProvider,
            Self::ReloadModels(_) => SettingsOperationKind::ReloadModels,
            Self::SaveFeatureSettings(_) => SettingsOperationKind::SaveFeatureSettings,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::RefreshSettings(_) | Self::ReloadModels(_) => Ok(()),
            Self::SaveDocument(c) => {
                check_text("text", &c.text, MAX_DOCUMENT_BYTES, true)?;
                // A NUL byte is a control character check_text already refuses.
                Ok(())
            }
            Self::SaveProvider(c) => {
                if let Some(original) = &c.original_name {
                    check_line("original_name", original, MAX_NAME_BYTES, false)?;
                }
                c.provider.validate()
            }
            Self::DeleteProvider(c) => check_line("name", &c.name, MAX_NAME_BYTES, false),
            Self::SaveFeatureSettings(c) => c.changes.validate(),
        }
    }
}

/// What a command answers: the operation it started, in the state it has now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingsResult {
    Operation(SettingsOperationView),
}

// ---- queries --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingsQuery {
    /// The snapshot the core holds (`refresh` reads the file again).
    GetSettingsSnapshot {},
    /// The Features screen's settings: saved, running and waiting for a restart.
    GetFeatureSettings {},
    ModelChoices {
        project: crate::ids::ProjectId,
    },
    GetSettingsOperation {
        operation_id: OperationId,
    },
    /// The retained operations, newest first.
    ListSettingsOperations {},
}

impl SettingsQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingsReply {
    ModelChoices(ProjectModelChoices),
    Settings(Box<SettingsSnapshot>),
    FeatureSettings(Box<FeatureSettingsView>),
    Operation(SettingsOperationView),
    Operations {
        operations: Vec<SettingsOperationView>,
    },
}

/// Credential-free metadata from one applied project-model snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectModelChoices {
    pub project: crate::ids::ProjectId,
    pub profiles: Vec<ModelProfileChoice>,
    pub worker: RoleModelChoice,
    pub orchestrator: RoleModelChoice,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfileChoice {
    pub name: String,
    pub provider_type: ProviderKind,
    pub models: Vec<String>,
    pub default_model: Option<String>,
    pub effort_levels: std::collections::BTreeMap<String, Vec<String>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleModelChoice {
    pub model: String,
    pub reasoning_effort: Option<String>,
}

impl SettingsReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ModelChoices(catalog) => {
                check_count("profiles", catalog.profiles.len(), MAX_PROVIDERS)?;
                for profile in &catalog.profiles {
                    check_line("profile.name", &profile.name, MAX_NAME_BYTES, false)?;
                    check_count("profile.models", profile.models.len(), MAX_MODELS)?;
                    for model in &profile.models {
                        check_line("profile.model", model, MAX_NAME_BYTES, false)?;
                    }
                    if let Some(model) = &profile.default_model {
                        check_line("profile.default_model", model, MAX_NAME_BYTES, false)?;
                    }
                    check_count(
                        "profile.effort_levels",
                        profile.effort_levels.len(),
                        MAX_MODELS,
                    )?;
                    for (model, efforts) in &profile.effort_levels {
                        check_line("profile.effort_model", model, MAX_NAME_BYTES, false)?;
                        check_count("profile.efforts", efforts.len(), MAX_MODELS)?;
                        for effort in efforts {
                            check_line("profile.effort", effort, MAX_NAME_BYTES, false)?;
                        }
                    }
                }
                for role in [&catalog.worker, &catalog.orchestrator] {
                    check_line("role.model", &role.model, MAX_VALUE_BYTES, true)?;
                    if let Some(effort) = &role.reasoning_effort {
                        check_line("role.reasoning_effort", effort, MAX_NAME_BYTES, false)?;
                    }
                }
                Ok(())
            }
            Self::Settings(snapshot) => {
                check_text("document", &snapshot.document, MAX_DOCUMENT_BYTES, true)?;
                check_count("providers", snapshot.providers.len(), MAX_PROVIDERS)
            }
            Self::FeatureSettings(view) => check_count(
                "restart_pending",
                view.restart_pending.len(),
                MAX_FEATURE_SECTIONS,
            ),
            Self::Operation(operation) => operation.validate(),
            Self::Operations { operations } => {
                check_count("operations", operations.len(), MAX_OPERATIONS)?;
                operations
                    .iter()
                    .try_for_each(SettingsOperationView::validate)
            }
        }
    }
}

// ---- events ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingsEvent {
    /// The saved or the applied revision changed; query the snapshot.
    SettingsSnapshotChanged {
        revision: SettingsRevision,
        /// `None`: not known after a failed reload; see the snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        applied_revision: Option<SettingsRevision>,
    },
    /// An operation ended; also readable with `get_operation`.
    SettingsOperationCompleted { operation: SettingsOperationView },
}

impl SettingsEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::SettingsOperationCompleted { operation } => operation.validate(),
            Self::SettingsSnapshotChanged { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_input_kind_is_listed_for_classification() {
        let credential = CredentialRef::Env { var: "KEY".into() };
        for input in [
            ProviderInput::OpenaiCompatible {
                name: "p".into(),
                kind: OpenaiCompatibleKind::OpenaiCompatible,
                base_url: "https://x.test".into(),
                credential,
                models: vec![],
                excluded_models: vec![],
                default_model: "m".into(),
            },
            ProviderInput::Codex {
                name: "c".into(),
                account: "codex".into(),
                base_url: "https://x.test".into(),
                models: vec![],
                default_model: String::new(),
            },
            ProviderInput::Subscription {
                name: "s".into(),
                kind: SubscriptionKind::Cursor,
                account: "cursor".into(),
                base_url: "https://x.test".into(),
                models: vec![],
                excluded_models: vec![],
                default_model: "m".into(),
            },
        ] {
            let wire = serde_json::to_value(&input).unwrap();
            assert!(PROVIDER_INPUT_KINDS.contains(&wire["type"].as_str().unwrap()));
            assert_eq!(
                serde_json::from_value::<ProviderInput>(wire).unwrap(),
                input
            );
        }
    }

    /// A reload that failed part-way leaves the applied state unknown: a value of
    /// its own on the wire (the field is absent, never an old-looking revision),
    /// which survives a round trip with its reason.
    #[test]
    fn an_unknown_applied_state_is_absent_on_the_wire_and_round_trips() {
        let revision = SettingsRevision::from_bytes(&[0x22; 32]);
        let unknown = SettingsSnapshot {
            revision: revision.clone(),
            applied_revision: None,
            apply_failure: Some("a project did not load".into()),
            document: String::new(),
            providers: vec![],
            problem: None,
        };
        let wire = serde_json::to_value(&unknown).unwrap();
        assert!(wire.get("applied_revision").is_none(), "{wire}");
        assert_eq!(wire["apply_failure"], "a project did not load");
        assert_eq!(
            serde_json::from_value::<SettingsSnapshot>(wire).unwrap(),
            unknown
        );
        assert!(unknown.pending_apply());

        let known = SettingsSnapshot {
            applied_revision: Some(revision.clone()),
            apply_failure: None,
            ..unknown
        };
        let wire = serde_json::to_value(&known).unwrap();
        assert!(wire.get("apply_failure").is_none(), "{wire}");
        assert_eq!(wire["applied_revision"], wire["revision"]);
        assert!(!known.pending_apply());

        let event = SettingsEvent::SettingsSnapshotChanged {
            revision,
            applied_revision: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert!(wire["payload"].get("applied_revision").is_none(), "{wire}");
        assert_eq!(
            serde_json::from_value::<SettingsEvent>(wire).unwrap(),
            event
        );
    }

    #[test]
    fn a_revision_is_fixed_lowercase_hex_and_a_document_is_bounded() {
        assert!(SettingsRevision::new("AB".repeat(32)).is_err());
        assert!(SettingsRevision::new("ab".repeat(31)).is_err());
        assert!(SettingsRevision::new("ab".repeat(32)).is_ok());
        let command = SettingsCommand::SaveDocument(SaveDocument {
            operation_id: OperationId::new("op").unwrap(),
            expected_revision: SettingsRevision::from_bytes(&[0; 32]),
            text: "x".repeat(MAX_DOCUMENT_BYTES + 1),
        });
        assert!(command.validate().is_err());
    }
}
