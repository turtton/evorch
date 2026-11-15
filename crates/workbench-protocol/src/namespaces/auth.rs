//! Auth namespace: credentials of the provider profiles and the browser logins
//! that create them (ADR 0045, PR 10).
//!
//! ## Shape
//!
//! * A *slot* is the credential of one provider profile of the configuration.
//!   `get_credential_status` answers from the status the core holds (`refresh_credentials`
//!   reads the keychain again); a status never contains a secret, only whether
//!   one is present and when a login expires.
//! * Every state-changing command is an operation with a client-chosen
//!   [`OperationId`](crate::ids::OperationId), answered at once in state
//!   `pending` (see [`crate::operation`]). Keychain access runs on a worker.
//! * A secret is input only: [`SecretValue`] appears in exactly two commands
//!   (`save_api_key` and `submit_login_input`), serializes only there, and has
//!   no `Debug` output. No reply, event, query result, operation view, receipt
//!   or log line carries one.
//! * **A login waits for a person.** `start_login` begins it and returns the
//!   operation `pending`; the address to open is read with `get_login` (it is
//!   not part of any event). The core listens for the browser's redirect on the
//!   loopback address of the host it runs on. A browser on another host (the
//!   web client, say) cannot reach that address, so the redirect address it
//!   ends up on (or the code) is pasted with `submit_login_input`. The first of
//!   the two that completes wins; the other is refused as used. A pasted value
//!   must carry the `state` of this very login, and is refused after use or
//!   after the login expired. While the login waits, **it holds no restart
//!   permit**: a planned restart or a shutdown ends the wait and the operation
//!   settles as `Cancelled` with `restart` or `shutdown`; the credential is
//!   written only after the operation entered the restart gate again.
use crate::{
    handshake::{Capability, WireLimits},
    ids::OperationId,
    limits::MAX_NAME_BYTES,
    namespaces::settings::{CredentialRef, ProviderKind},
    operation::OperationView,
    validation::{ValidationError, check_count, check_line, check_text},
    wire::Revision,
};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const COMMAND_OPERATIONS: &[&str] = &[
    "refresh_credentials",
    "save_api_key",
    "delete_credential",
    "start_login",
    "submit_login_input",
    "cancel_login",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "get_credential_status",
    "get_login",
    "get_auth_operation",
    "list_auth_operations",
];
pub const EVENT_TYPES: &[&str] = &[
    "credential_status_changed",
    "login_changed",
    "auth_operation_completed",
];
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most slots a status lists.
pub const MAX_SLOTS: usize = 64;
/// Most operations a core keeps and lists.
pub const MAX_OPERATIONS: usize = 64;
/// Longest secret or pasted login input, in bytes.
pub const MAX_SECRET_BYTES: usize = 16 * 1024;
/// Longest authorize address.
pub const MAX_URL_BYTES: usize = 4096;
/// Longest text of an operation's message.
pub const MAX_MESSAGE_BYTES: usize = 512;

/// A secret typed or pasted by a person. Input only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The typed text; the only way to read it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    fn validate(&self, field: &'static str) -> Result<(), ValidationError> {
        if self.0.trim().is_empty() {
            return Err(ValidationError::Empty { field });
        }
        if self.0.len() > MAX_SECRET_BYTES {
            return Err(ValidationError::TooLong {
                field,
                max: MAX_SECRET_BYTES,
                actual: self.0.len(),
            });
        }
        if self.0.chars().any(char::is_control) {
            return Err(ValidationError::InvalidChars { field });
        }
        Ok(())
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

/// How a profile logs in, when it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginMethod {
    Codex,
    Claude,
    Cursor,
}

/// What the core knows about a credential. Never the credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialState {
    /// Not read yet.
    Unknown,
    Missing,
    /// Present; a login also tells when it expires (Unix seconds).
    Present {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_unix: Option<u64>,
    },
    /// The store could not be read (a locked keychain, say).
    Unavailable,
}

/// The credential of one provider profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSlot {
    pub profile: String,
    pub kind: ProviderKind,
    pub storage: CredentialRef,
    pub state: CredentialState,
    /// The way to create it by logging in; none for a key a person types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<LoginMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthStatus {
    /// Changes whenever the status does; compare for equality only.
    pub revision: Revision,
    pub slots: Vec<CredentialSlot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthOperationKind {
    RefreshStatus,
    SaveApiKey,
    DeleteCredential,
    Login,
}

/// An operation with what an auth client needs to explain its end. The
/// `message` is one of a few fixed, secret-free sentences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthOperationView {
    pub operation: OperationView,
    pub kind: AuthOperationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl AuthOperationView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.operation.validate()?;
        if let Some(message) = &self.message {
            check_text("message", message, MAX_MESSAGE_BYTES, true)?;
        }
        Ok(())
    }
}

/// Where a login stands. The operation's own state says how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginPhase {
    /// Waiting for a person; no restart permit is held.
    Waiting,
    /// The redirect arrived; the code is being exchanged and saved.
    Completing,
    Settled,
}

/// What a person needs to finish a login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginView {
    pub login_id: OperationId,
    pub profile: String,
    pub method: LoginMethod,
    pub phase: LoginPhase,
    /// The address to open in a browser; only while the login waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<String>,
    /// Whether a pasted redirect address or code finishes this login.
    pub accepts_input: bool,
}

impl LoginView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("profile", &self.profile, MAX_NAME_BYTES, false)?;
        if let Some(url) = &self.authorize_url {
            check_line("authorize_url", url, MAX_URL_BYTES, false)?;
        }
        Ok(())
    }
}

/// Why a pasted value was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginInputRejection {
    /// No such login.
    UnknownLogin,
    /// The login is not waiting for input (it ended, or takes none).
    NotWaiting,
    /// Another input or the redirect already completed this login.
    AlreadyUsed,
    /// The login is past its deadline.
    Expired,
    /// The value names another login (its `state` differs).
    StateMismatch,
    /// Neither a redirect address nor a code.
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginInputOutcome {
    /// The login takes it; its end is the operation's.
    Accepted,
    Rejected {
        reason: LoginInputRejection,
    },
}

// ---- commands -------------------------------------------------------------

/// Read every credential again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshCredentials {
    pub operation_id: OperationId,
}

/// Store `value` in the keychain entry of `profile`. The value is never repeated
/// anywhere, and the same operation id with another value does not run again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveApiKey {
    pub operation_id: OperationId,
    pub profile: String,
    pub value: SecretValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteCredential {
    pub operation_id: OperationId,
    pub profile: String,
}

/// Begin a browser login for `profile`. `operation_id` names the login too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartLogin {
    pub operation_id: OperationId,
    pub profile: String,
}

/// Finish the login `login_id` with the redirect address the browser ended on,
/// or with the code it shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitLoginInput {
    pub login_id: OperationId,
    pub input: SecretValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelLogin {
    pub login_id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AuthCommand {
    RefreshCredentials(RefreshCredentials),
    SaveApiKey(SaveApiKey),
    DeleteCredential(DeleteCredential),
    StartLogin(StartLogin),
    SubmitLoginInput(SubmitLoginInput),
    CancelLogin(CancelLogin),
}

impl AuthCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::RefreshCredentials(_) | Self::CancelLogin(_) => Ok(()),
            Self::SaveApiKey(c) => {
                check_line("profile", &c.profile, MAX_NAME_BYTES, false)?;
                c.value.validate("value")
            }
            Self::DeleteCredential(c) => check_line("profile", &c.profile, MAX_NAME_BYTES, false),
            Self::StartLogin(c) => check_line("profile", &c.profile, MAX_NAME_BYTES, false),
            Self::SubmitLoginInput(c) => c.input.validate("input"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AuthResult {
    /// The operation a command started (or, for `cancel_login`, ended).
    Operation(AuthOperationView),
    /// The answer to `submit_login_input`.
    LoginInput(LoginInputOutcome),
}

// ---- queries --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AuthQuery {
    GetCredentialStatus {},
    GetLogin {
        login_id: OperationId,
    },
    GetAuthOperation {
        operation_id: OperationId,
    },
    /// The retained operations, newest first.
    ListAuthOperations {},
}

impl AuthQuery {
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
pub enum AuthReply {
    Status(AuthStatus),
    Login(LoginView),
    Operation(AuthOperationView),
    Operations { operations: Vec<AuthOperationView> },
}

impl AuthReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Status(status) => check_count("slots", status.slots.len(), MAX_SLOTS),
            Self::Login(login) => login.validate(),
            Self::Operation(operation) => operation.validate(),
            Self::Operations { operations } => {
                check_count("operations", operations.len(), MAX_OPERATIONS)?;
                operations.iter().try_for_each(AuthOperationView::validate)
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
pub enum AuthEvent {
    /// The status has a new revision; query it.
    CredentialStatusChanged { revision: Revision },
    /// A login started, changed phase or ended; query it. The address to open
    /// is never part of the event.
    LoginChanged { login_id: OperationId },
    /// An operation ended; also readable with `get_operation`.
    AuthOperationCompleted { operation: AuthOperationView },
}

impl AuthEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::AuthOperationCompleted { operation } => operation.validate(),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_has_no_readable_debug_output_anywhere_it_is_embedded() {
        let command = AuthCommand::SubmitLoginInput(SubmitLoginInput {
            login_id: OperationId::new("login").unwrap(),
            input: SecretValue::new("sentinel-secret-value"),
        });
        assert!(!format!("{command:?}").contains("sentinel"));
        let command = AuthCommand::SaveApiKey(SaveApiKey {
            operation_id: OperationId::new("op").unwrap(),
            profile: "p".into(),
            value: SecretValue::new("sentinel-secret-value"),
        });
        assert!(!format!("{command:?}").contains("sentinel"));
    }

    #[test]
    fn a_secret_must_be_one_non_empty_line_within_the_limit() {
        for bad in ["", "   ", "a\nb", &"x".repeat(MAX_SECRET_BYTES + 1)] {
            let command = AuthCommand::SubmitLoginInput(SubmitLoginInput {
                login_id: OperationId::new("login").unwrap(),
                input: SecretValue::new(bad),
            });
            assert!(command.validate().is_err(), "{bad:?}");
        }
    }
}
