//! Mcp namespace: project MCP settings, approvals, connection tests and the
//! keychain-backed secrets (ADR 0045, PR 11).
//!
//! Everything here mirrors what the privileged GUI service of the core offers
//! (`McpGuiService`), without depending on it: the wire types are plain data and
//! the core translates them into its own types.
//!
//! ## Shape
//!
//! * A project is named by its [`ProjectId`]; the core resolves the directory.
//!   A client never sends a path.
//! * Every state-changing command carries a client-chosen [`McpOperationId`] and
//!   is answered at once with the operation in state `running` (or
//!   `already_applied` when the id was seen). The work runs in the core; its end
//!   arrives as the `operation_completed` event and stays readable with
//!   `get_operation`, also after the client disconnected. A client that lost a
//!   reply never resends: it asks `get_operation` (or the command receipt).
//! * Settings are guarded by a [`McpRevision`] (the optimistic identity of both
//!   source files). A command quoting an old revision fails with
//!   `config_conflict` and writes nothing.
//! * A secret is input only: [`McpSecretValue`] appears in exactly two commands,
//!   serializes only there, and has no `Debug` output. No reply, event, query
//!   result or operation view carries one.
//! * An approval is confirmed with the identity of the preview the user saw
//!   (id, revision, fingerprint); the core re-checks all three.
use crate::{
    handshake::{Capability, WireLimits},
    ids::ProjectId,
    limits::MAX_NAME_BYTES,
    validation::{ValidationError, check_count, check_line},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "refresh",
    "save_override",
    "set_enabled",
    "preview_approval",
    "confirm_approval",
    "revoke_approval",
    "test_connection",
    "cancel_operation",
    "save_secret",
    "delete_secret",
];
pub const QUERY_OPERATIONS: &[&str] = &["get_settings", "get_operation", "list_operations"];
pub const EVENT_TYPES: &[&str] = &[
    "settings_changed",
    "server_status_changed",
    "operation_completed",
];
/// The tagged choice inside `save_override`'s payload that classification names.
pub const MUTATION_KINDS: &[&str] = &["upsert", "disable", "remove_override"];
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[
    super::NestedOperation {
        operation: "save_override",
        pointer: "/mutation/type",
        known: MUTATION_KINDS,
    },
    super::NestedOperation {
        operation: "save_secret",
        pointer: "/mutation/type",
        known: MUTATION_KINDS,
    },
    super::NestedOperation {
        operation: "delete_secret",
        pointer: "/mutation/type",
        known: MUTATION_KINDS,
    },
];

/// Most servers a definition set may name (the core's own limit).
pub const MAX_SERVERS: usize = 64;
/// Longest string inside a definition or a secret, in bytes.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// Most entries in any list or map of a definition.
pub const MAX_ENTRIES: usize = 128;
/// Most template parts of one template.
pub const MAX_TEMPLATE_PARTS: usize = 64;
/// Most operations a core keeps and lists.
pub const MAX_OPERATIONS: usize = 64;
/// Largest timeout a definition may ask for, in milliseconds.
pub const MAX_TIMEOUT_MS: u64 = 600_000;

fn invalid(field: &'static str, reason: &'static str) -> ValidationError {
    ValidationError::OutOfRange { field, reason }
}

macro_rules! hex_id {
    ($(#[$meta:meta])* $name:ident, $bytes:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// The identity of `bytes`, in lowercase hex.
            pub fn from_bytes(bytes: &[u8; $bytes]) -> Self {
                Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
            }

            /// Parse lowercase hex.
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
                let value = value.into();
                if value.len() != $bytes * 2
                    || !value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                {
                    return Err(invalid(stringify!($name), "expected lowercase hex of the fixed length"));
                }
                Ok(Self(value))
            }

            /// The bytes this id spells.
            pub fn to_bytes(&self) -> [u8; $bytes] {
                let mut out = [0u8; $bytes];
                for (slot, pair) in out.iter_mut().zip(self.0.as_bytes().chunks(2)) {
                    // Validated at construction: ASCII hex digits only.
                    *slot = u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("00"), 16)
                        .unwrap_or(0);
                }
                out
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ValidationError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

hex_id!(
    /// The optimistic identity of both MCP source files of a project.
    McpRevision,
    32
);
hex_id!(
    /// What an approval would allow, as the core computed it.
    McpFingerprint,
    32
);
hex_id!(
    /// Names one approval preview the core issued.
    McpPreviewId,
    16
);

/// Names one command's operation. Chosen by the client (any unique id, a UUID
/// say) so that it can find the outcome again without resending.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct McpOperationId(String);

impl McpOperationId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        crate::ids::validate_id("McpOperationId", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for McpOperationId {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<McpOperationId> for String {
    fn from(value: McpOperationId) -> Self {
        value.0
    }
}

/// A secret the user typed. Input only: it serializes so a client can send it,
/// but it has no readable `Debug`, is never part of a reply, event or receipt,
/// and the core hands it to the keychain and drops it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpSecretValue(String);

impl McpSecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The typed text; the only way to read it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), ValidationError> {
        if self.0.is_empty() {
            return Err(ValidationError::Empty { field: "value" });
        }
        if self.0.len() > MAX_VALUE_BYTES {
            return Err(ValidationError::TooLong {
                field: "value",
                max: MAX_VALUE_BYTES,
                actual: self.0.len(),
            });
        }
        Ok(())
    }
}

impl fmt::Debug for McpSecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("McpSecretValue(<redacted>)")
    }
}

/// One server of one project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpScope {
    pub project: ProjectId,
    pub server_id: String,
}

impl McpScope {
    fn validate(&self) -> Result<(), ValidationError> {
        check_line("server_id", &self.server_id, MAX_NAME_BYTES, false)
    }
}

// ---- definitions ----------------------------------------------------------

/// A piece of a template: literal text or an environment reference.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTemplatePart {
    Literal {
        value: String,
    },
    Env {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
}

/// Configuration text that may name environment variables. Its `Debug` hides
/// the content: literal configuration may contain credentials.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpTemplate(pub Vec<McpTemplatePart>);

impl fmt::Debug for McpTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("McpTemplate(<redacted>)")
    }
}

impl fmt::Debug for McpTemplatePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("McpTemplatePart(<redacted>)")
    }
}

impl McpTemplate {
    fn validate(&self, field: &'static str) -> Result<(), ValidationError> {
        check_count(field, self.0.len(), MAX_TEMPLATE_PARTS)?;
        for part in &self.0 {
            let check = |v: &str, allow_empty: bool| {
                if v.len() > MAX_VALUE_BYTES {
                    return Err(ValidationError::TooLong {
                        field,
                        max: MAX_VALUE_BYTES,
                        actual: v.len(),
                    });
                }
                if v.is_empty() && !allow_empty {
                    return Err(ValidationError::Empty { field });
                }
                Ok(())
            };
            match part {
                McpTemplatePart::Literal { value } => check(value, true)?,
                McpTemplatePart::Env { name, default } => {
                    check(name, false)?;
                    if let Some(default) = default {
                        check(default, true)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Where a header, token or environment value comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpValueSource {
    Template {
        template: McpTemplate,
    },
    /// A named OS keychain entry. The name is not secret; the value is never sent.
    Keychain {
        name: String,
    },
}

impl McpValueSource {
    fn validate(&self, field: &'static str) -> Result<(), ValidationError> {
        match self {
            Self::Template { template } => template.validate(field),
            Self::Keychain { name } => check_line(field, name, MAX_NAME_BYTES, false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransport {
    Stdio {
        command: McpTemplate,
        #[serde(default)]
        args: Vec<McpTemplate>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<McpTemplate>,
        #[serde(default)]
        env: BTreeMap<String, McpValueSource>,
    },
    Http {
        url: McpTemplate,
        #[serde(default)]
        headers: BTreeMap<String, McpValueSource>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<McpValueSource>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerDefinition {
    pub enabled: bool,
    /// Role keys that may use the server; empty means unpublished.
    pub roles: Vec<String>,
    pub transport: McpTransport,
    pub startup_timeout_ms: u64,
    pub request_timeout_ms: u64,
}

fn validate_pairs(
    field: &'static str,
    map: &BTreeMap<String, McpValueSource>,
) -> Result<(), ValidationError> {
    check_count(field, map.len(), MAX_ENTRIES)?;
    for (name, source) in map {
        check_line(field, name, MAX_NAME_BYTES, false)?;
        source.validate(field)?;
    }
    Ok(())
}

impl McpServerDefinition {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("roles", self.roles.len(), MAX_ENTRIES)?;
        for role in &self.roles {
            check_line("roles", role, MAX_NAME_BYTES, false)?;
        }
        for (field, timeout) in [
            ("startup_timeout_ms", self.startup_timeout_ms),
            ("request_timeout_ms", self.request_timeout_ms),
        ] {
            if !(1..=MAX_TIMEOUT_MS).contains(&timeout) {
                return Err(invalid(field, "outside the supported range"));
            }
        }
        match &self.transport {
            McpTransport::Stdio {
                command,
                args,
                cwd,
                env,
            } => {
                command.validate("command")?;
                check_count("args", args.len(), MAX_ENTRIES)?;
                for arg in args {
                    arg.validate("args")?;
                }
                if let Some(cwd) = cwd {
                    cwd.validate("cwd")?;
                }
                validate_pairs("env", env)
            }
            McpTransport::Http {
                url,
                headers,
                token,
            } => {
                url.validate("url")?;
                validate_pairs("headers", headers)?;
                token.as_ref().map_or(Ok(()), |t| t.validate("token"))
            }
        }
    }

    /// The keychain names the definition refers to, sorted and distinct.
    pub fn keychain_references(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let mut push = |source: &McpValueSource| {
            if let McpValueSource::Keychain { name } = source {
                names.push(name.clone());
            }
        };
        match &self.transport {
            McpTransport::Stdio { env, .. } => env.values().for_each(&mut push),
            McpTransport::Http { headers, token, .. } => {
                headers.values().for_each(&mut push);
                token.iter().for_each(&mut push);
            }
        }
        names.sort();
        names.dedup();
        names
    }
}

/// A change to the project's override file. A full replacement, never a merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpMutation {
    Upsert {
        server_id: String,
        definition: McpServerDefinition,
    },
    /// Hide an imported server (a tombstone) or switch a defined one off.
    Disable { server_id: String },
    /// Drop the override so the imported definition applies again, or delete a
    /// server that exists only in the override file.
    RemoveOverride { server_id: String },
}

impl McpMutation {
    pub fn server_id(&self) -> &str {
        match self {
            Self::Upsert { server_id, .. }
            | Self::Disable { server_id }
            | Self::RemoveOverride { server_id } => server_id,
        }
    }

    fn validate(&self) -> Result<(), ValidationError> {
        check_line("server_id", self.server_id(), MAX_NAME_BYTES, false)?;
        match self {
            Self::Upsert { definition, .. } => definition.validate(),
            _ => Ok(()),
        }
    }
}

// ---- classified failures --------------------------------------------------

/// Where in a definition a missing environment variable was named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpField {
    Command,
    Args,
    Cwd,
    Env,
    Url,
    Headers,
    Token,
}

/// A classified failure: never a message, URL, header or file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpErrorCode {
    Timeout,
    Transport,
    HttpStatus {
        status: u16,
    },
    Protocol,
    ServerRejected,
    ToolRejected,
    Configuration,
    /// The files changed since the quoted revision; nothing was written.
    ConfigConflict,
    NeedsApproval,
    ApprovalRevoked,
    RoleNotAllowed,
    /// The configuration changed after the preview, so it cannot be approved.
    StalePreview,
    SecretUnavailable,
    SecretLocked,
    SecretDenied,
    SecretMissing,
    MissingEnvironment {
        field: McpField,
    },
    ProcessLimitReached,
    PreparationLimitReached,
    Unavailable,
    Cancelled,
    OutcomeUnknown,
    InputRequired,
    LimitExceeded,
    DefinitionChanged,
    /// The core does not know the project.
    UnknownProject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatus {
    Disabled,
    Invalid,
    NeedsApproval,
    SecretUnavailable,
    Connecting,
    Ready,
    Unavailable,
    ProcessLimitReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerState {
    pub status: McpServerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<McpErrorCode>,
}

// ---- views ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpOrigin {
    ImportedJson,
    ProjectToml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpConfigCode {
    Unavailable,
    Io,
    Syntax,
    Invalid,
    InvalidTemplate,
    MissingEnvironment,
    LimitExceeded,
    Conflict,
    Symlink,
}

/// A problem found while reading the configuration, without its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpDiagnostic {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    pub source: McpOrigin,
    pub code: McpConfigCode,
}

/// One server as the settings list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerView {
    pub server_id: String,
    pub origin: McpOrigin,
    /// An imported definition exists under this id (so an override overrides it).
    pub imported_exists: bool,
    /// The entry only hides an imported definition.
    pub tombstone: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<McpServerDefinition>,
    pub diagnostics: Vec<McpDiagnostic>,
    pub state: McpServerState,
    /// Keychain names the definition refers to. Whether a value is stored shows
    /// in `state` (`secret_missing`, `secret_locked`, ...); a value never does.
    pub secret_references: Vec<String>,
}

/// A project's MCP settings as the core last read them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSettings {
    pub project: ProjectId,
    pub revision: McpRevision,
    /// A broken override file disables all new preparation.
    pub preparation_blocked: bool,
    pub diagnostics: Vec<McpDiagnostic>,
    pub servers: Vec<McpServerView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpDestination {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

/// Safe description of what a server would run or reach: no argument, URL query
/// or header value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<McpDestination>,
    pub environment_keys: Vec<String>,
    pub secret_references: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpApprovalWarning {
    HostProcessOutsideSandbox,
    PrivateDestination,
    PlaintextHttp,
    SendsSecretReferences,
}

/// What approving a server would allow, with the identity the confirmation must quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalPreview {
    pub preview_id: McpPreviewId,
    pub scope: McpScope,
    pub source_revision: McpRevision,
    pub fingerprint: McpFingerprint,
    pub warnings: Vec<McpApprovalWarning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_fingerprint: Option<McpFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_summary: Option<McpApprovalSummary>,
    pub summary: McpApprovalSummary,
    pub approved_ips: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpOperationKind {
    Refresh,
    SaveOverride,
    SetEnabled,
    PreviewApproval,
    ConfirmApproval,
    RevokeApproval,
    TestConnection,
    SaveSecret,
    DeleteSecret,
}

/// What a finished operation produced. Never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpOutcome {
    /// Settings were read or saved; this is their revision now.
    Settings {
        revision: McpRevision,
    },
    ApprovalPreview {
        preview: Box<McpApprovalPreview>,
    },
    ApprovalConfirmed,
    ApprovalRevoked,
    Tested {
        status: McpServerStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpOperationState {
    Running,
    Succeeded {
        outcome: McpOutcome,
    },
    /// The settings were saved, but a keychain step failed: the entry may still
    /// hold its value. Never reported as a plain success or a plain failure.
    Partial {
        outcome: McpOutcome,
        keychain_error: McpErrorCode,
    },
    Failed {
        error: McpErrorCode,
    },
    Cancelled,
}

impl McpOperationState {
    pub fn is_finished(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpOperationView {
    pub operation_id: McpOperationId,
    pub project: ProjectId,
    pub kind: McpOperationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    pub state: McpOperationState,
}

// ---- commands -------------------------------------------------------------

/// Read the project's configuration again and publish what changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refresh {
    pub operation_id: McpOperationId,
    pub project: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveOverride {
    pub operation_id: McpOperationId,
    pub project: ProjectId,
    pub expected_revision: McpRevision,
    pub mutation: McpMutation,
}

/// Flip `enabled` of a defined server (a complete replacement of its definition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetEnabled {
    pub operation_id: McpOperationId,
    pub scope: McpScope,
    pub expected_revision: McpRevision,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewApproval {
    pub operation_id: McpOperationId,
    pub scope: McpScope,
    pub expected_revision: McpRevision,
}

/// Approve exactly the preview the user saw. The core re-reads the sources and
/// compares id, revision and fingerprint before it records anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmApproval {
    pub operation_id: McpOperationId,
    pub preview_id: McpPreviewId,
    pub scope: McpScope,
    pub expected_revision: McpRevision,
    pub expected_fingerprint: McpFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeApproval {
    pub operation_id: McpOperationId,
    pub scope: McpScope,
}

/// Start and stop the server once, with the approval and process limit of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestConnection {
    pub operation_id: McpOperationId,
    pub scope: McpScope,
    pub expected_revision: McpRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelOperation {
    pub operation_id: McpOperationId,
}

/// Store `value` in the keychain under `logical_name`, then save `mutation`,
/// which must reference that name. The value is never repeated anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveSecret {
    pub operation_id: McpOperationId,
    pub scope: McpScope,
    pub logical_name: String,
    pub value: McpSecretValue,
    pub expected_revision: McpRevision,
    pub mutation: McpMutation,
}

/// Save `mutation` (a complete replacement without any reference to
/// `logical_name`), then delete the keychain entry. A keychain failure after the
/// save is a `partial` operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteSecret {
    pub operation_id: McpOperationId,
    pub scope: McpScope,
    pub logical_name: String,
    pub expected_revision: McpRevision,
    pub mutation: McpMutation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum McpCommand {
    Refresh(Refresh),
    SaveOverride(Box<SaveOverride>),
    SetEnabled(SetEnabled),
    PreviewApproval(PreviewApproval),
    ConfirmApproval(ConfirmApproval),
    RevokeApproval(RevokeApproval),
    TestConnection(TestConnection),
    CancelOperation(CancelOperation),
    SaveSecret(Box<SaveSecret>),
    DeleteSecret(Box<DeleteSecret>),
}

impl McpCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    /// The operation this command names.
    pub fn operation_id(&self) -> &McpOperationId {
        match self {
            Self::Refresh(c) => &c.operation_id,
            Self::SaveOverride(c) => &c.operation_id,
            Self::SetEnabled(c) => &c.operation_id,
            Self::PreviewApproval(c) => &c.operation_id,
            Self::ConfirmApproval(c) => &c.operation_id,
            Self::RevokeApproval(c) => &c.operation_id,
            Self::TestConnection(c) => &c.operation_id,
            Self::CancelOperation(c) => &c.operation_id,
            Self::SaveSecret(c) => &c.operation_id,
            Self::DeleteSecret(c) => &c.operation_id,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Refresh(_) | Self::CancelOperation(_) => Ok(()),
            Self::SaveOverride(c) => c.mutation.validate(),
            Self::SetEnabled(c) => c.scope.validate(),
            Self::PreviewApproval(c) => c.scope.validate(),
            Self::ConfirmApproval(c) => c.scope.validate(),
            Self::RevokeApproval(c) => c.scope.validate(),
            Self::TestConnection(c) => c.scope.validate(),
            Self::SaveSecret(c) => {
                c.scope.validate()?;
                check_line("logical_name", &c.logical_name, MAX_NAME_BYTES, false)?;
                c.value.validate()?;
                c.mutation.validate()
            }
            Self::DeleteSecret(c) => {
                c.scope.validate()?;
                check_line("logical_name", &c.logical_name, MAX_NAME_BYTES, false)?;
                c.mutation.validate()
            }
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
pub enum McpResult {
    Operation(McpOperationView),
}

// ---- queries --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum McpQuery {
    /// The settings the core last read for a project (`refresh` reads them again).
    GetSettings {
        project: ProjectId,
    },
    GetOperation {
        operation_id: McpOperationId,
    },
    /// The retained operations, newest first, of one project or of all.
    ListOperations {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<ProjectId>,
    },
}

impl McpQuery {
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
pub enum McpReply {
    Settings(Box<McpSettings>),
    Operation(McpOperationView),
    Operations { operations: Vec<McpOperationView> },
}

impl McpReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Operations { operations } => {
                check_count("operations", operations.len(), MAX_OPERATIONS)
            }
            Self::Settings(settings) => check_count("servers", settings.servers.len(), MAX_SERVERS),
            Self::Operation(_) => Ok(()),
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
pub enum McpEvent {
    /// The settings of a project have a new revision; query them.
    SettingsChanged {
        project: ProjectId,
        revision: McpRevision,
    },
    ServerStatusChanged {
        scope: McpScope,
        state: McpServerState,
    },
    /// An operation ended; also readable with `get_operation`.
    OperationCompleted { operation: McpOperationView },
}

impl McpEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ServerStatusChanged { scope, .. } => scope.validate(),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_has_no_readable_debug_output_anywhere_it_is_embedded() {
        let command = McpCommand::SaveSecret(Box::new(SaveSecret {
            operation_id: McpOperationId::new("op").unwrap(),
            scope: McpScope {
                project: ProjectId::new("p").unwrap(),
                server_id: "s".into(),
            },
            logical_name: "name".into(),
            value: McpSecretValue::new("sentinel-secret-value"),
            expected_revision: McpRevision::from_bytes(&[0; 32]),
            mutation: McpMutation::Disable {
                server_id: "s".into(),
            },
        }));
        assert!(!format!("{command:?}").contains("sentinel-secret-value"));
        assert!(
            !format!("{:?}", McpSecretValue::new("sentinel-secret-value")).contains("sentinel")
        );
    }

    #[test]
    fn every_mutation_kind_round_trips_and_is_listed_for_classification() {
        for mutation in [
            McpMutation::Disable {
                server_id: "s".into(),
            },
            McpMutation::RemoveOverride {
                server_id: "s".into(),
            },
        ] {
            let wire = serde_json::to_value(&mutation).unwrap();
            assert!(MUTATION_KINDS.contains(&wire["type"].as_str().unwrap()));
            assert_eq!(
                serde_json::from_value::<McpMutation>(wire).unwrap(),
                mutation
            );
        }
        assert!(MUTATION_KINDS.contains(&"upsert"));
    }

    #[test]
    fn hex_identities_reject_anything_but_their_fixed_lowercase_form() {
        assert!(McpRevision::new("AB".repeat(32)).is_err());
        assert!(McpRevision::new("ab".repeat(31)).is_err());
        let id = McpPreviewId::from_bytes(&[7; 16]);
        assert_eq!(McpPreviewId::new(id.as_str()).unwrap().to_bytes(), [7; 16]);
    }

    #[test]
    fn a_definition_outside_the_limits_is_refused() {
        let mut definition = McpServerDefinition {
            enabled: true,
            roles: vec![],
            transport: McpTransport::Http {
                url: McpTemplate(vec![McpTemplatePart::Literal { value: "u".into() }]),
                headers: BTreeMap::new(),
                token: None,
            },
            startup_timeout_ms: 1,
            request_timeout_ms: 1,
        };
        assert!(definition.validate().is_ok());
        definition.request_timeout_ms = MAX_TIMEOUT_MS + 1;
        assert!(definition.validate().is_err());
    }
}
