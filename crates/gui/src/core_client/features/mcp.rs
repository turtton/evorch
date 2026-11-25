//! Desktop feature of the `mcp` area: the project's MCP servers, approvals,
//! connection tests and keychain secrets, over the protocol only.
//!
//! Every action is a typed command with an operation id this feature chose. The
//! core answers at once and reports the end of the work as an event, also after
//! this client was disconnected: after a reconnect every operation still
//! running or of unknown outcome is asked for again with `get_operation`. A
//! command is never sent twice; what is unknown is looked up, never resent.
//!
//! The form is the one of the in-process settings dialog ([`McpServerDraft`]),
//! so a definition is checked the same way before it is sent.
//!
//! A typed secret lives in the input field only. It is moved into the command
//! when sent and the field is cleared at once, whatever the outcome; it is not
//! part of any state this feature keeps, and nothing it displays contains one.
use super::{Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply};
use crate::mcp_settings::{
    DraftTransport, DraftValue, DraftValueKind, McpServerDraft, SecretTarget,
};
use crate::theme::text::muted;
use crate::theme::tokens::palette;
use crate::theme::widgets::{badge, spinner};
use config::mcp::{MCP_ROLE_KEYS, McpServerEntry, McpSource};
use egui::{Color32, RichText, Ui};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use workbench_host::areas::mcp::{config_definition, describe, wire_definition};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::ProjectId,
    namespaces::mcp::{
        CancelOperation, ConfirmApproval, DeleteSecret, McpApprovalPreview, McpApprovalWarning,
        McpCommand, McpErrorCode, McpEvent, McpField, McpMutation, McpOperationId,
        McpOperationKind, McpOperationState, McpOperationView, McpOrigin, McpOutcome, McpQuery,
        McpReply, McpResult, McpRevision, McpScope, McpSecretState, McpSecretValue, McpServerState,
        McpServerStatus, McpServerView, McpSettings, PreviewApproval, Refresh, RevokeApproval,
        SaveOverride, SaveSecret, SetEnabled, TestConnection,
    },
    queries::{Query, QueryReply},
};

const REQUIRED: &[Capability] = &[Capability::Commands, Capability::Queries];

/// What a request in flight stands for. Kept across `reset`: a reply may only
/// arrive after the connection was replaced.
#[derive(Debug, Clone)]
enum Pending {
    /// A command that started `operation`, and what it acts on.
    Command {
        operation: McpOperationId,
        project: ProjectId,
    },
    /// Asking the core to cancel an operation (which keeps running until it says so).
    Cancel,
    /// Reading `operation` again (reconciliation after a reconnect).
    Lookup {
        operation: McpOperationId,
    },
    Settings {
        project: ProjectId,
    },
}

/// A request waiting for the next frame, which owns the context to send it.
enum Outgoing {
    Command(Command, Pending),
    Query(Query, Pending),
}

/// What the user last did to one server row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowNote {
    Working(McpOperationKind),
    Approved,
    Revoked,
    Tested(McpServerStatus),
    Failed(McpErrorCode),
    Cancelled,
    /// The core no longer holds the result of the request: it may or may not
    /// have been applied.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Saved,
    SecretStored,
    SecretDeleted(String),
    /// The settings are saved but the keychain entry may still hold its value.
    SecretDeletePartial {
        name: String,
        reason: McpErrorCode,
    },
    /// The files changed since they were read; nothing was written.
    Conflict,
    Failed(String),
    /// A command's outcome is not known; the core is asked about it.
    Unknown,
    /// The core no longer holds the result of a request; reload to see what
    /// the state is now.
    Expired,
}

enum Review {
    Loading {
        operation: McpOperationId,
    },
    Ready(Box<McpApprovalPreview>),
    Confirming {
        preview: Box<McpApprovalPreview>,
        operation: McpOperationId,
    },
    Stale,
    Failed(McpErrorCode),
}

/// A form being edited. It is tied to the project and the settings revision it
/// was opened on: whatever is selected or reloaded later, a save goes to this
/// project and quotes this revision, and only an explicit action moves it.
struct Editor {
    project: ProjectId,
    revision: McpRevision,
    draft: McpServerDraft,
    secret: Option<SecretEntry>,
    /// The operation that is saving this form. Its end closes this form and
    /// no other.
    saving: Option<McpOperationId>,
}

impl Editor {
    fn new(project: ProjectId, revision: McpRevision, draft: McpServerDraft) -> Self {
        Self {
            project,
            revision,
            draft,
            secret: None,
            saving: None,
        }
    }
}

/// What a request of ours acts on, kept until its end is known.
#[derive(Debug, Clone)]
struct Target {
    project: ProjectId,
    server: Option<String>,
    kind: McpOperationKind,
}

/// The masked value being typed. Deliberately without `Debug`.
struct SecretEntry {
    target: SecretTarget,
    value: String,
}

#[derive(Default)]
struct ProjectState {
    settings: Option<McpSettings>,
    /// A refresh was asked for and has not answered.
    loading: bool,
}

#[derive(Default)]
pub struct McpFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    outbox: Vec<Outgoing>,
    projects: HashMap<String, ProjectState>,
    /// Operations this desktop started (their notes are for this user).
    mine: HashSet<McpOperationId>,
    operations: BTreeMap<McpOperationId, McpOperationView>,
    /// Commands whose reply was lost; asked for after the next snapshot.
    unknown: HashSet<McpOperationId>,
    /// What each operation of `mine` that has not ended acts on.
    targets: HashMap<McpOperationId, Target>,
    /// The connection test started from each row (project, server).
    tests: HashMap<(String, String), McpOperationId>,
    notes: HashMap<(String, String), RowNote>,
    notice: Option<Notice>,
    editor: Option<Editor>,
    review: Option<(String, Review)>,
    session: u64,
}

fn operation_id(session: u64, counter: u64) -> McpOperationId {
    McpOperationId::new(format!("gui-{}-{session}-{counter}", uuid::Uuid::new_v4()))
        .expect("a generated operation id is valid")
}

fn origin(view: &McpServerView) -> (McpOrigin, bool) {
    (view.origin, view.imported_exists)
}

/// Row actions of an entry, from where it is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowActions {
    pub edit: bool,
    pub set_enabled: Option<bool>,
    pub disable: bool,
    pub remove_override: bool,
    pub delete: bool,
}

pub fn row_actions(view: &McpServerView) -> RowActions {
    let enabled = view.definition.as_ref().map(|d| d.enabled);
    match origin(view) {
        (McpOrigin::ImportedJson, _) => RowActions {
            edit: true,
            set_enabled: (enabled == Some(false)).then_some(true),
            disable: enabled == Some(true),
            ..RowActions::default()
        },
        (McpOrigin::ProjectToml, true) => RowActions {
            edit: !view.tombstone,
            set_enabled: enabled.map(|e| !e),
            disable: !view.tombstone,
            remove_override: true,
            delete: false,
        },
        (McpOrigin::ProjectToml, false) => RowActions {
            edit: !view.tombstone,
            set_enabled: enabled.map(|e| !e),
            disable: false,
            remove_override: false,
            delete: true,
        },
    }
}

/// The draft form's entry for `view` (the form works on the runtime's types).
fn entry_of(view: &McpServerView) -> McpServerEntry {
    McpServerEntry {
        source: match view.origin {
            McpOrigin::ImportedJson => McpSource::ImportedJson,
            McpOrigin::ProjectToml => McpSource::ProjectToml,
        },
        imported_exists: view.imported_exists,
        definition: view.definition.clone().map(config_definition),
        tombstone: view.tombstone,
        diagnostics: Vec::new(),
    }
}

pub fn error_text(code: McpErrorCode) -> String {
    match code {
        McpErrorCode::ConfigConflict => {
            "The files changed since they were read. Nothing was written; the list is reloaded.".into()
        }
        McpErrorCode::StalePreview => {
            "The configuration changed after the preview, so it cannot be approved. Review it again.".into()
        }
        McpErrorCode::NeedsApproval => {
            "Not started: it is not approved. Approve it to use it from the next new run.".into()
        }
        McpErrorCode::SecretUnavailable => {
            "The OS keychain is not available. The value may not have been stored.".into()
        }
        McpErrorCode::SecretLocked => {
            "The OS keychain is locked. Unlock it and try again; the value may not have been stored.".into()
        }
        McpErrorCode::SecretDenied => {
            "Access to the OS keychain was denied. The value may not have been stored.".into()
        }
        McpErrorCode::OutcomeUnknown => {
            "The result is not known: the keychain entry or the settings may have been changed. The list is reloaded; check it before trying again.".into()
        }
        McpErrorCode::SecretMissing => "The keychain entry has no stored value yet.".into(),
        McpErrorCode::Timeout => "The server did not answer in time.".into(),
        McpErrorCode::Transport => {
            "The server could not be reached or stopped unexpectedly.".into()
        }
        McpErrorCode::HttpStatus { status } => format!("The server answered with HTTP {status}."),
        McpErrorCode::Protocol => "The server's response was not valid MCP.".into(),
        McpErrorCode::ServerRejected => "The server rejected the request.".into(),
        McpErrorCode::ToolRejected => "The tool reported an error.".into(),
        McpErrorCode::Configuration => "The server definition is invalid.".into(),
        McpErrorCode::ApprovalRevoked => {
            "Approval was revoked; calls to this server are refused.".into()
        }
        McpErrorCode::RoleNotAllowed => {
            "This server is not shared with this role; calls to it are refused.".into()
        }
        McpErrorCode::MissingEnvironment { field } => format!(
            "An environment variable used in {} is not set in evorch's environment.",
            field_label(field)
        ),
        McpErrorCode::ProcessLimitReached => {
            "Too many local MCP servers are running for this project; it was not started.".into()
        }
        McpErrorCode::PreparationLimitReached => {
            "Too many MCP connections are being prepared for this project; this server is unavailable for this run.".into()
        }
        McpErrorCode::Unavailable => "The server is unavailable.".into(),
        McpErrorCode::Cancelled => "Cancelled.".into(),
        McpErrorCode::InputRequired => {
            "The server asked for interactive input, which evorch does not support.".into()
        }
        McpErrorCode::LimitExceeded => "The server's response exceeded a size limit.".into(),
        McpErrorCode::DefinitionChanged => {
            "The server's tool definitions changed since this conversation started.".into()
        }
        McpErrorCode::UnknownProject => "The core does not know this project.".into(),
    }
}

fn field_label(field: McpField) -> &'static str {
    match field {
        McpField::Command => "command",
        McpField::Args => "arguments",
        McpField::Cwd => "working directory",
        McpField::Env => "environment",
        McpField::Url => "URL",
        McpField::Headers => "headers",
        McpField::Token => "token",
    }
}

fn status_badge(status: McpServerStatus) -> (&'static str, Color32) {
    match status {
        McpServerStatus::Disabled => ("off", palette().TEXT_MUTED),
        McpServerStatus::Invalid => ("invalid", palette().ERROR_FG),
        McpServerStatus::NeedsApproval => ("needs approval", palette().WARNING),
        McpServerStatus::SecretUnavailable => ("secret unavailable", palette().WARNING),
        McpServerStatus::Connecting => ("connecting", palette().INFO),
        McpServerStatus::Ready => ("ready", palette().SUCCESS),
        McpServerStatus::Unavailable => ("unavailable", palette().ERROR_FG),
        McpServerStatus::ProcessLimitReached => ("process limit", palette().WARNING),
    }
}

fn status_reason(state: &McpServerState) -> Option<McpErrorCode> {
    match (state.reason, state.status) {
        (Some(reason), _) => Some(reason),
        (None, McpServerStatus::NeedsApproval) => Some(McpErrorCode::NeedsApproval),
        (None, McpServerStatus::ProcessLimitReached) => Some(McpErrorCode::ProcessLimitReached),
        (None, McpServerStatus::SecretUnavailable) => Some(McpErrorCode::SecretUnavailable),
        (None, McpServerStatus::Invalid) => Some(McpErrorCode::Configuration),
        (None, _) => None,
    }
}

/// `error_text` describes a failed operation; a server's state is not one, so
/// the codes whose operation wording would claim a write or a reload differ.
fn state_reason_text(code: McpErrorCode) -> String {
    match code {
        McpErrorCode::SecretUnavailable => "The OS keychain is not available. Nothing was read or stored, and evorch does not fall back to a file.".into(),
        McpErrorCode::SecretLocked => "The OS keychain is locked. Unlock it and try again.".into(),
        McpErrorCode::SecretDenied => {
            "Access to the OS keychain was denied or cancelled. Nothing was stored.".into()
        }
        McpErrorCode::SecretMissing => {
            "The keychain entry has no stored value yet. Edit the server to set it.".into()
        }
        McpErrorCode::ConfigConflict => {
            "The MCP configuration changed on disk. Reload and try again.".into()
        }
        McpErrorCode::StalePreview => "The MCP configuration changed after this preview, so nothing was approved. Review the current settings again.".into(),
        McpErrorCode::OutcomeUnknown => {
            "The connection ended before the result arrived; it is not retried.".into()
        }
        other => error_text(other),
    }
}

fn operation_text(kind: McpOperationKind) -> &'static str {
    match kind {
        McpOperationKind::Refresh => "Reloading…",
        McpOperationKind::SaveOverride => "Saving…",
        McpOperationKind::SetEnabled => "Updating…",
        McpOperationKind::PreviewApproval => "Reading what approving would allow…",
        McpOperationKind::ConfirmApproval => "Approving…",
        McpOperationKind::RevokeApproval => "Revoking approval…",
        McpOperationKind::TestConnection => "Testing the connection…",
        McpOperationKind::SaveSecret => "Storing the secret…",
        McpOperationKind::DeleteSecret => "Deleting the secret…",
    }
}

fn delete_partial_text(name: &str, reason: McpErrorCode) -> String {
    let cause = match reason {
        McpErrorCode::SecretUnavailable => "the OS keychain is not available",
        McpErrorCode::SecretLocked => "the OS keychain is locked",
        McpErrorCode::SecretDenied => "access to the OS keychain was denied or cancelled",
        McpErrorCode::ConfigConflict => "the MCP configuration changed on disk meanwhile",
        _ => "the OS keychain reported an error",
    };
    format!(
        "The references of {name} are removed, but the keychain entry was not deleted because {cause}. Its value may still be stored in the OS keychain."
    )
}

fn warning_text(warning: McpApprovalWarning, preview: &McpApprovalPreview) -> (String, Color32) {
    match warning {
        McpApprovalWarning::HostProcessOutsideSandbox => (
            "This starts a program on this computer outside the sandbox. It can run any command with your user permissions.".into(),
            palette().WARNING_FG,
        ),
        McpApprovalWarning::PrivateDestination => (
            "This is an address on this computer or the local network. evorch connects only to the addresses listed above; a new address needs approval again.".into(),
            palette().WARNING_FG,
        ),
        McpApprovalWarning::PlaintextHttp => (
            "http:// is not encrypted: requests, headers and any secrets are sent in plain text on the network.".into(),
            palette().ERROR_FG,
        ),
        McpApprovalWarning::SendsSecretReferences => (
            format!(
                "These secrets are sent to this server with every request: {}.",
                preview.summary.secret_references.join(", ")
            ),
            palette().WARNING_FG,
        ),
    }
}

impl McpFeature {
    fn tag(&mut self) -> u64 {
        self.next_tag += 1;
        self.next_tag
    }

    fn fresh_operation(&mut self) -> McpOperationId {
        self.next_tag += 1;
        let id = operation_id(self.session, self.next_tag);
        self.mine.insert(id.clone());
        id
    }

    fn settings_of(&self, project: &ProjectId) -> Option<&McpSettings> {
        self.projects
            .get(project.as_str())
            .and_then(|state| state.settings.as_ref())
    }

    fn queue_command(&mut self, command: McpCommand, operation: McpOperationId) {
        let pending = match &command {
            McpCommand::CancelOperation(_) => Pending::Cancel,
            other => {
                let (project, kind, server) = describe(other);
                let project = project.expect("every command but a cancel names a project");
                self.targets.insert(
                    operation.clone(),
                    Target {
                        project: project.clone(),
                        server,
                        kind,
                    },
                );
                Pending::Command { operation, project }
            }
        };
        self.outbox
            .push(Outgoing::Command(Command::Mcp(command), pending));
    }

    /// Release what a request that ended without an effect we know of was
    /// holding: the row's "working" note, the review waiting for it, the
    /// loading flag, the connection test it was and the editor it was saving.
    fn unwind(&mut self, operation: &McpOperationId) {
        let Some(target) = self.targets.remove(operation) else {
            return;
        };
        let server = target.server.clone().unwrap_or_default();
        let key = (target.project.as_str().to_owned(), server);
        if matches!(self.notes.get(&key), Some(RowNote::Working(_))) {
            self.notes.remove(&key);
        }
        if self.tests.get(&key) == Some(operation) {
            self.tests.remove(&key);
        }
        if target.kind == McpOperationKind::Refresh
            && let Some(state) = self.projects.get_mut(target.project.as_str())
        {
            state.loading = false;
        }
        let waiting = match &self.review {
            Some((_, Review::Loading { operation: waits })) => waits == operation,
            Some((
                _,
                Review::Confirming {
                    operation: waits, ..
                },
            )) => waits == operation,
            _ => false,
        };
        if waiting {
            self.review = None;
        }
        if let Some(editor) = &mut self.editor
            && editor.saving.as_ref() == Some(operation)
        {
            editor.saving = None;
        }
    }

    /// The end of the operation that was saving the form: that form closes
    /// (when the save is what the user asked for) or is released to be edited
    /// again; a form opened since is never touched.
    fn editor_settled(
        &mut self,
        operation: &McpOperationId,
        close: bool,
        revision: Option<&McpRevision>,
    ) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.saving.as_ref() != Some(operation) {
            return;
        }
        if close {
            self.editor = None;
        } else {
            editor.saving = None;
            if let Some(revision) = revision {
                editor.revision = revision.clone();
            }
        }
    }

    /// The core no longer holds `operation` (or it was lost on the way): what
    /// happened is not known, and it is not claimed either way.
    fn settle_unknown(&mut self, operation: &McpOperationId) {
        self.unknown.remove(operation);
        self.mine.remove(operation);
        self.operations.remove(operation);
        let target = self.targets.get(operation).cloned();
        self.unwind(operation);
        if let Some(target) = target {
            if let Some(server) = &target.server
                && matches!(
                    target.kind,
                    McpOperationKind::SetEnabled
                        | McpOperationKind::RevokeApproval
                        | McpOperationKind::TestConnection
                        | McpOperationKind::SaveSecret
                        | McpOperationKind::DeleteSecret
                )
            {
                self.notes.insert(
                    (target.project.as_str().to_owned(), server.clone()),
                    RowNote::Unknown,
                );
            }
            self.refresh(&target.project);
        }
        self.notice = Some(Notice::Expired);
    }

    /// Ask the core to cancel the connection test started from this row.
    /// Another row's (or project's) test is never touched.
    fn cancel_test(&mut self, project: &ProjectId, server: &str) -> bool {
        let key = (project.as_str().to_owned(), server.to_owned());
        let Some(operation) = self.tests.get(&key).cloned() else {
            return false;
        };
        self.queue_command(
            McpCommand::CancelOperation(CancelOperation {
                operation_id: operation.clone(),
            }),
            operation,
        );
        true
    }

    fn start_test(&mut self, project: &ProjectId, revision: &McpRevision, server: &str) {
        let operation = self.fresh_operation();
        self.tests.insert(
            (project.as_str().to_owned(), server.to_owned()),
            operation.clone(),
        );
        self.queue_command(
            McpCommand::TestConnection(TestConnection {
                operation_id: operation.clone(),
                scope: Self::scope(project, server),
                expected_revision: revision.clone(),
            }),
            operation,
        );
        self.working(project, server, McpOperationKind::TestConnection);
    }

    fn queue_settings(&mut self, project: &ProjectId) {
        self.outbox.push(Outgoing::Query(
            Query::Mcp(McpQuery::GetSettings {
                project: project.clone(),
            }),
            Pending::Settings {
                project: project.clone(),
            },
        ));
    }

    fn refresh(&mut self, project: &ProjectId) {
        self.projects
            .entry(project.as_str().to_owned())
            .or_default()
            .loading = true;
        let operation = self.fresh_operation();
        self.queue_command(
            McpCommand::Refresh(Refresh {
                operation_id: operation.clone(),
                project: project.clone(),
            }),
            operation,
        );
    }

    fn scope(project: &ProjectId, server: &str) -> McpScope {
        McpScope {
            project: project.clone(),
            server_id: server.to_owned(),
        }
    }

    fn working(&mut self, project: &ProjectId, server: &str, kind: McpOperationKind) {
        self.notes.insert(
            (project.as_str().to_owned(), server.to_owned()),
            RowNote::Working(kind),
        );
    }

    /// An operation's state arrived (from a reply, an event or a lookup).
    fn apply_operation(&mut self, view: McpOperationView) {
        let project = view.project.clone();
        let mine = self.mine.contains(&view.operation_id);
        self.unknown.remove(&view.operation_id);
        let finished = view.state.is_finished();
        self.operations
            .insert(view.operation_id.clone(), view.clone());
        if !finished || !mine {
            return;
        }
        let server = view.server_id.clone().unwrap_or_default();
        let key = (project.as_str().to_owned(), server.clone());
        self.targets.remove(&view.operation_id);
        if self.tests.get(&key) == Some(&view.operation_id) {
            self.tests.remove(&key);
        }
        match (&view.state, view.kind) {
            (McpOperationState::Succeeded { outcome }, kind) => match outcome {
                McpOutcome::Settings { revision } => {
                    let known = self.settings_of(&project).map(|s| s.revision.clone());
                    // A reload and a secret change what the settings show (which
                    // entries hold a value) without a new revision.
                    if known.as_ref() != Some(revision)
                        || matches!(
                            kind,
                            McpOperationKind::Refresh
                                | McpOperationKind::SaveSecret
                                | McpOperationKind::DeleteSecret
                        )
                    {
                        self.queue_settings(&project);
                    }
                    if let Some(state) = self.projects.get_mut(project.as_str()) {
                        state.loading = false;
                    }
                    match kind {
                        McpOperationKind::SaveSecret => self.notice = Some(Notice::SecretStored),
                        McpOperationKind::DeleteSecret => {
                            self.notice = Some(Notice::SecretDeleted(server.clone()))
                        }
                        McpOperationKind::Refresh => {}
                        _ => self.notice = Some(Notice::Saved),
                    }
                    self.notes.remove(&key);
                    let close = matches!(
                        kind,
                        McpOperationKind::SaveOverride | McpOperationKind::SaveSecret
                    );
                    self.editor_settled(&view.operation_id, close, Some(revision));
                }
                McpOutcome::ApprovalPreview { preview } => {
                    if let Some((_, Review::Loading { operation, .. })) = &self.review
                        && operation == &view.operation_id
                    {
                        self.review = Some((server, Review::Ready(preview.clone())));
                    }
                    self.notes.remove(&key);
                }
                McpOutcome::ApprovalConfirmed => {
                    self.review = None;
                    self.notes.insert(key, RowNote::Approved);
                }
                McpOutcome::ApprovalRevoked => {
                    self.notes.insert(key, RowNote::Revoked);
                }
                McpOutcome::Tested { status } => {
                    self.notes.insert(key, RowNote::Tested(*status));
                }
            },
            (
                McpOperationState::Partial {
                    outcome,
                    keychain_error,
                },
                _,
            ) => {
                let revision = match outcome {
                    McpOutcome::Settings { revision } => Some(revision),
                    _ => None,
                };
                self.editor_settled(&view.operation_id, false, revision);
                self.notice = Some(Notice::SecretDeletePartial {
                    name: server.clone(),
                    reason: *keychain_error,
                });
                self.queue_settings(&project);
                self.notes.remove(&key);
            }
            (McpOperationState::Cancelled, _) => {
                self.editor_settled(&view.operation_id, false, None);
                self.notes.insert(key, RowNote::Cancelled);
            }
            (McpOperationState::Failed { error }, kind) => {
                let key2 = key.clone();
                match (*error, kind) {
                    (McpErrorCode::StalePreview, McpOperationKind::ConfirmApproval) => {
                        self.review = Some((server.clone(), Review::Stale));
                    }
                    (
                        error,
                        McpOperationKind::PreviewApproval | McpOperationKind::ConfirmApproval,
                    ) => {
                        self.review = Some((server.clone(), Review::Failed(error)));
                    }
                    (McpErrorCode::OutcomeUnknown, kind) => {
                        // The keychain or the settings may have changed: say so, read
                        // the files again (the core's copy may be older than them),
                        // and show what is there when that read has ended.
                        let _ = kind;
                        self.notice =
                            Some(Notice::Failed(error_text(McpErrorCode::OutcomeUnknown)));
                        self.refresh(&project);
                    }
                    (McpErrorCode::ConfigConflict, _) => {
                        self.notice = Some(Notice::Conflict);
                        self.refresh(&project);
                    }
                    (
                        error,
                        McpOperationKind::TestConnection | McpOperationKind::RevokeApproval,
                    ) => {
                        self.notes.insert(key, RowNote::Failed(error));
                    }
                    (error, _) => self.notice = Some(Notice::Failed(error_text(error))),
                }
                self.editor_settled(&view.operation_id, false, None);
                if matches!(self.notes.get(&key2), Some(RowNote::Working(_))) {
                    self.notes.remove(&key2);
                }
                if let Some(state) = self.projects.get_mut(project.as_str()) {
                    state.loading = false;
                }
            }
            (McpOperationState::Running, _) => {}
        }
    }

    fn flush(&mut self, ctx: &mut FeatureContext<'_>) {
        for outgoing in std::mem::take(&mut self.outbox) {
            let tag = self.tag();
            match outgoing {
                Outgoing::Command(command, pending) => {
                    self.pending.insert(tag, pending);
                    ctx.command(tag, command);
                }
                Outgoing::Query(query, pending) => {
                    self.pending.insert(tag, pending);
                    ctx.query(tag, query);
                }
            }
        }
    }

    /// What a command outcome of unknown result is looked up with.
    fn reconcile(&mut self) {
        let ids: Vec<McpOperationId> = self
            .unknown
            .iter()
            .cloned()
            .chain(
                self.operations
                    .values()
                    .filter(|op| !op.state.is_finished() && self.mine.contains(&op.operation_id))
                    .map(|op| op.operation_id.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for operation in ids {
            self.outbox.push(Outgoing::Query(
                Query::Mcp(McpQuery::GetOperation {
                    operation_id: operation.clone(),
                }),
                Pending::Lookup { operation },
            ));
        }
    }
}

impl Feature for McpFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Mcp
    }

    fn title(&self) -> &'static str {
        "MCP"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented { required: REQUIRED }
    }

    fn reset(&mut self) {
        // What came from the core is dropped; what ties a request in flight to
        // its meaning stays (`pending`, `mine`, `operations`, `unknown`).
        self.session += 1;
        for state in self.projects.values_mut() {
            state.settings = None;
            state.loading = false;
        }
        self.reconcile();
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Mcp(event) = event else {
            return;
        };
        match event {
            McpEvent::SettingsChanged { project, revision } => {
                let known = self.settings_of(project).map(|s| &s.revision);
                if self.projects.contains_key(project.as_str()) && known != Some(revision) {
                    self.queue_settings(project);
                }
            }
            McpEvent::ServerStatusChanged { scope, state } => {
                if let Some(settings) = self
                    .projects
                    .get_mut(scope.project.as_str())
                    .and_then(|p| p.settings.as_mut())
                    && let Some(server) = settings
                        .servers
                        .iter_mut()
                        .find(|s| s.server_id == scope.server_id)
                {
                    server.state = *state;
                }
            }
            McpEvent::OperationCompleted { operation } => self.apply_operation(operation.clone()),
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.get(&tag).cloned() else {
            return;
        };
        match reply {
            FeatureReply::Command(Ok(reply)) => {
                self.pending.remove(&tag);
                if let Some(result) = reply.result
                    && let CommandResult::Mcp(McpResult::Operation(view)) = *result
                {
                    self.apply_operation(view);
                }
            }
            FeatureReply::Command(Err(FeatureError {
                outcome_unknown: true,
                ..
            })) => {
                // The tag stays: the resolved reply arrives for it later, and
                // the operation is looked up by id after the next snapshot.
                if let Pending::Command { operation, .. } = pending {
                    self.unknown.insert(operation);
                    self.notice = Some(Notice::Unknown);
                }
            }
            FeatureReply::Command(Err(error)) => {
                // The core refused the command at once: nothing runs, so what
                // it held (working notes, a waiting review, the loading flag,
                // the form being saved) is released now.
                self.pending.remove(&tag);
                if let Pending::Command {
                    operation, project, ..
                } = pending
                {
                    self.mine.remove(&operation);
                    self.operations.remove(&operation);
                    self.unknown.remove(&operation);
                    self.unwind(&operation);
                    if matches!(
                        error.code,
                        Some(
                            ErrorCode::Conflict
                                | ErrorCode::StaleRevision
                                | ErrorCode::ResyncRequired
                        )
                    ) {
                        self.queue_settings(&project);
                    }
                }
                self.notice = Some(Notice::Failed(error.message));
            }
            FeatureReply::Query(Ok(QueryReply::Mcp(reply))) => {
                self.pending.remove(&tag);
                match reply {
                    McpReply::Settings(settings) => {
                        let slot = self
                            .projects
                            .entry(settings.project.as_str().to_owned())
                            .or_default();
                        slot.loading = false;
                        slot.settings = Some(*settings);
                    }
                    McpReply::Operation(view) => self.apply_operation(view),
                    McpReply::Operations { .. } => {}
                }
            }
            FeatureReply::Query(Ok(_)) => {
                self.pending.remove(&tag);
            }
            FeatureReply::Query(Err(error)) => {
                self.pending.remove(&tag);
                match pending {
                    Pending::Lookup { operation } => {
                        if error.code == Some(ErrorCode::NotFound) {
                            // The core holds no such operation: it may be evicted
                            // or never have arrived. That is not proof the request
                            // was not applied, so it is not reported as such.
                            self.settle_unknown(&operation);
                        } else {
                            // The lookup itself failed: the result is still not
                            // known, so it stays tracked and is asked for again.
                            self.unknown.insert(operation);
                            self.notice = Some(Notice::Unknown);
                        }
                    }
                    Pending::Settings { project } => {
                        if let Some(state) = self.projects.get_mut(project.as_str()) {
                            state.loading = false;
                        }
                        self.notice = Some(Notice::Failed(error.message));
                    }
                    Pending::Command { .. } | Pending::Cancel => {}
                }
            }
        }
    }

    fn render(&mut self, ui: &mut Ui, ctx: &mut FeatureContext<'_>) {
        let Some(project) = ctx.selection().project.clone() else {
            ui.label("Select a project to manage its MCP servers.");
            return;
        };
        let known = self.projects.get(project.as_str());
        if known.is_none_or(|p| p.settings.is_none() && !p.loading) {
            self.refresh(&project);
        }
        self.draw(ui, &project);
        self.flush(ctx);
    }

    fn unsupported(&self) -> Option<&'static str> {
        None
    }
}

fn secret_state_label(state: McpSecretState) -> &'static str {
    match state {
        McpSecretState::Present => "a value is stored",
        McpSecretState::Missing => "no value is stored yet",
        McpSecretState::Locked => "the keychain is locked",
        McpSecretState::Unavailable => "the keychain is not available",
        McpSecretState::Denied => "access to the keychain was denied",
    }
}

fn row_note(ui: &mut Ui, note: &RowNote) {
    match note {
        RowNote::Working(kind) => {
            ui.horizontal(|ui| {
                spinner(ui);
                ui.label(muted(operation_text(*kind)));
            });
        }
        RowNote::Approved => {
            ui.colored_label(
                palette().SUCCESS,
                "Approved. It is used from the next new run.",
            );
        }
        RowNote::Revoked => {
            ui.colored_label(palette().WARNING_FG, "Approval revoked.");
        }
        RowNote::Tested(status) => {
            let (label, color) = status_badge(*status);
            ui.colored_label(color, format!("Connection test: {label}."));
        }
        // Row failures come only from a connection test or a revoke, which
        // never store a secret, so the write-claiming operation wording is wrong.
        RowNote::Failed(error) => {
            ui.colored_label(palette().ERROR_FG, state_reason_text(*error));
        }
        RowNote::Cancelled => {
            ui.label("Cancelled.");
        }
        RowNote::Unknown => {
            ui.label(
                "The result is not known (the core no longer holds it). Check the state before trying again.",
            );
        }
    }
}

impl McpFeature {
    fn draw(&mut self, ui: &mut Ui, project: &ProjectId) {
        if let Some(notice) = &self.notice {
            let text = match notice {
                Notice::Saved => "Saved. Changes apply from the next new run.".to_owned(),
                Notice::SecretStored => "The secret is stored in the OS keychain.".to_owned(),
                Notice::SecretDeleted(name) => format!("The secret of {name} was deleted."),
                Notice::SecretDeletePartial { name, reason } => delete_partial_text(name, *reason),
                Notice::Conflict => error_text(McpErrorCode::ConfigConflict),
                Notice::Failed(message) => message.clone(),
                Notice::Unknown => {
                    "The result of a request is not known yet; it is being looked up.".to_owned()
                }
                Notice::Expired => {
                    "The core no longer holds the result of a request, so it is not known whether it was applied. The list is reloaded; check it before trying again.".to_owned()
                }
            };
            let unknown = matches!(notice, Notice::Unknown);
            ui.horizontal(|ui| {
                ui.label(text);
                if unknown && ui.small_button("Look up again").clicked() {
                    self.reconcile();
                }
                if ui.small_button("Dismiss").clicked() {
                    self.notice = None;
                }
            });
        }
        let Some(settings) = self.settings_of(project).cloned() else {
            ui.label("Loading MCP servers…");
            return;
        };
        ui.horizontal(|ui| {
            if ui.button("Reload").clicked() {
                self.refresh(project);
            }
            if ui.button("Add MCP server").clicked() {
                self.editor = Some(Editor::new(
                    project.clone(),
                    settings.revision.clone(),
                    McpServerDraft::new_server(),
                ));
            }
        });
        if settings.preparation_blocked {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "mcp.toml cannot be read: no new run uses MCP servers until it is fixed.",
            );
        }
        for server in &settings.servers {
            self.server_row(ui, project, &settings, server);
        }
        self.review_dialog(ui, project);
        self.editor_dialog(ui, project);
    }

    fn server_row(
        &mut self,
        ui: &mut Ui,
        project: &ProjectId,
        settings: &McpSettings,
        server: &McpServerView,
    ) {
        let id = server.server_id.clone();
        let actions = row_actions(server);
        let note = self
            .notes
            .get(&(project.as_str().to_owned(), id.clone()))
            .cloned();
        let busy = matches!(note, Some(RowNote::Working(_)));
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&id).strong());
                let (label, color) = status_badge(server.state.status);
                badge(ui, label, color, palette().SURFACE);
            });
            let note_cause = match &note {
                Some(RowNote::Failed(code)) => Some(*code),
                Some(RowNote::Cancelled) => Some(McpErrorCode::Cancelled),
                _ => None,
            };
            if let Some(reason) = status_reason(&server.state)
                && note_cause != Some(reason)
            {
                ui.label(muted(state_reason_text(reason)));
            }
            for (name, state) in &server.secret_states {
                ui.label(format!(
                    "Keychain entry {name}: {}",
                    secret_state_label(*state)
                ));
            }
            if let Some(note) = &note {
                row_note(ui, note);
            }
            ui.horizontal_wrapped(|ui| {
                if actions.edit && ui.button("Edit").clicked() {
                    self.editor = Some(Editor::new(
                        project.clone(),
                        settings.revision.clone(),
                        McpServerDraft::from_entry(&id, &entry_of(server)),
                    ));
                }
                if let Some(enabled) = actions.set_enabled
                    && ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(if enabled { "Enable" } else { "Disable" }),
                        )
                        .clicked()
                {
                    let operation = self.fresh_operation();
                    self.queue_command(
                        McpCommand::SetEnabled(SetEnabled {
                            operation_id: operation.clone(),
                            scope: Self::scope(project, &id),
                            expected_revision: settings.revision.clone(),
                            enabled,
                        }),
                        operation,
                    );
                    self.working(project, &id, McpOperationKind::SetEnabled);
                }
                if actions.disable && ui.button("Hide (tombstone)").clicked() {
                    self.save_mutation(
                        project,
                        settings,
                        McpMutation::Disable {
                            server_id: id.clone(),
                        },
                    );
                }
                if actions.remove_override && ui.button("Remove override").clicked() {
                    self.save_mutation(
                        project,
                        settings,
                        McpMutation::RemoveOverride {
                            server_id: id.clone(),
                        },
                    );
                }
                if actions.delete && ui.button("Delete").clicked() {
                    self.save_mutation(
                        project,
                        settings,
                        McpMutation::RemoveOverride {
                            server_id: id.clone(),
                        },
                    );
                }
                self.approval_buttons(ui, project, settings, server, busy);
            });
        });
    }

    fn approval_buttons(
        &mut self,
        ui: &mut Ui,
        project: &ProjectId,
        settings: &McpSettings,
        server: &McpServerView,
        busy: bool,
    ) {
        let id = &server.server_id;
        if server.definition.is_none() {
            return;
        }
        if ui
            .add_enabled(!busy, egui::Button::new("Review approval"))
            .clicked()
        {
            let operation = self.fresh_operation();
            self.review = Some((
                id.clone(),
                Review::Loading {
                    operation: operation.clone(),
                },
            ));
            self.queue_command(
                McpCommand::PreviewApproval(PreviewApproval {
                    operation_id: operation.clone(),
                    scope: Self::scope(project, id),
                    expected_revision: settings.revision.clone(),
                }),
                operation,
            );
        }
        if ui.add_enabled(!busy, egui::Button::new("Revoke")).clicked() {
            let operation = self.fresh_operation();
            self.queue_command(
                McpCommand::RevokeApproval(RevokeApproval {
                    operation_id: operation.clone(),
                    scope: Self::scope(project, id),
                }),
                operation,
            );
            self.working(project, id, McpOperationKind::RevokeApproval);
        }
        if busy {
            if self
                .tests
                .contains_key(&(project.as_str().to_owned(), id.clone()))
                && ui.button("Cancel test").clicked()
            {
                self.cancel_test(project, id);
            }
        } else if ui.button("Test connection").clicked() {
            self.start_test(project, &settings.revision, id);
        }
    }

    fn save_mutation(
        &mut self,
        project: &ProjectId,
        settings: &McpSettings,
        mutation: McpMutation,
    ) {
        let operation = self.fresh_operation();
        self.queue_command(
            McpCommand::SaveOverride(Box::new(SaveOverride {
                operation_id: operation.clone(),
                project: project.clone(),
                expected_revision: settings.revision.clone(),
                mutation,
            })),
            operation,
        );
    }

    fn review_dialog(&mut self, ui: &mut Ui, project: &ProjectId) {
        let Some((server, review)) = &self.review else {
            return;
        };
        let server = server.clone();
        let mut action = None;
        ui.group(|ui| match review {
            Review::Loading { .. } => {
                ui.label("Reading what approving would allow…");
            }
            Review::Ready(preview) | Review::Confirming { preview, .. } => {
                ui.label(RichText::new(format!("Approve {server}?")).strong());
                if let Some(destination) = &preview.summary.destination {
                    ui.label(format!(
                        "Destination: {}://{}:{}",
                        destination.scheme, destination.host, destination.port
                    ));
                }
                if let Some(executable) = &preview.summary.executable {
                    ui.label(format!("Runs: {executable}"));
                }
                if !preview.approved_ips.is_empty() {
                    ui.label(format!(
                        "Allowed addresses: {}",
                        preview.approved_ips.join(", ")
                    ));
                }
                for warning in &preview.warnings {
                    let (text, color) = warning_text(*warning, preview);
                    ui.colored_label(color, text);
                }
                if !preview.summary.secret_references.is_empty() {
                    ui.label(format!(
                        "Keychain references: {}",
                        preview.summary.secret_references.join(", ")
                    ));
                }
                let confirming = matches!(review, Review::Confirming { .. });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!confirming, egui::Button::new("Approve"))
                        .clicked()
                    {
                        action = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(false);
                    }
                });
            }
            Review::Stale => {
                ui.label(error_text(McpErrorCode::StalePreview));
                if ui.button("Review again").clicked() {
                    action = Some(false);
                }
            }
            Review::Failed(error) => {
                ui.label(error_text(*error));
                if ui.button("Close").clicked() {
                    action = Some(false);
                }
            }
        });
        match action {
            Some(true) => {
                if let Some((_, Review::Ready(preview))) = self.review.take() {
                    let operation = self.fresh_operation();
                    let confirm_operation = operation.clone();
                    self.queue_command(
                        McpCommand::ConfirmApproval(ConfirmApproval {
                            operation_id: operation.clone(),
                            preview_id: preview.preview_id.clone(),
                            scope: Self::scope(project, &server),
                            expected_revision: preview.source_revision.clone(),
                            expected_fingerprint: preview.fingerprint.clone(),
                        }),
                        operation,
                    );
                    self.review = Some((
                        server,
                        Review::Confirming {
                            preview,
                            operation: confirm_operation,
                        },
                    ));
                }
            }
            Some(false) => self.review = None,
            None => {}
        }
    }

    fn editor_dialog(&mut self, ui: &mut Ui, current: &ProjectId) {
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.project != *current {
            // The form belongs to another project and stays there.
            ui.label(format!(
                "A form for project {} is open there; it is saved to that project only.",
                editor.project.as_str()
            ));
            return;
        }
        let Some(settings) = self.settings_of(&editor.project).cloned() else {
            return;
        };
        let project = editor.project.clone();
        let stale = editor.revision != settings.revision;
        let can_write = !stale && editor.saving.is_none();
        let saved_states = editor
            .draft
            .existing_id
            .as_ref()
            .and_then(|id| settings.servers.iter().find(|s| &s.server_id == id))
            .map(|server| server.secret_states.clone());
        let Some(editor) = &mut self.editor else {
            return;
        };
        let taken: BTreeSet<String> = settings
            .servers
            .iter()
            .map(|s| s.server_id.clone())
            .collect();
        let check = editor.draft.check(&taken);
        let mut rebase = false;
        let mut save = false;
        let mut cancel = false;
        let mut secret_request: Option<(SecretTarget, String, String)> = None;
        let mut delete_target: Option<SecretTarget> = None;
        let mut start_secret: Option<SecretTarget> = None;
        // What was saved is what is closed at the end of the save: while it
        // saves, the form takes no further input (nothing typed meanwhile is lost).
        let editing_locked = editor.saving.is_some();
        ui.group(|ui| {
            let draft = &mut editor.draft;
            if stale {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "The settings changed since this form was opened, so it cannot be saved as it is.",
                );
                if ui.button("Keep my edit on the latest settings").clicked() {
                    rebase = true;
                }
            }
            ui.label(
                RichText::new(if draft.is_new() {
                    "Add MCP server"
                } else {
                    "Edit MCP server"
                })
                .strong(),
            );
            ui.add_enabled_ui(!editing_locked, |ui| {
            ui.add_enabled_ui(draft.is_new(), |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut draft.id);
                });
            });
            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut draft.transport,
                    DraftTransport::Stdio,
                    "Command (stdio)",
                );
                ui.radio_value(&mut draft.transport, DraftTransport::Http, "HTTP");
                ui.checkbox(&mut draft.enabled, "Enabled");
            });
            ui.horizontal_wrapped(|ui| {
                for role in MCP_ROLE_KEYS {
                    let mut on = draft.roles.contains(*role);
                    if ui.checkbox(&mut on, *role).changed() {
                        if on {
                            draft.roles.insert((*role).to_owned());
                        } else {
                            draft.roles.remove(*role);
                        }
                    }
                }
            });
            match draft.transport {
                DraftTransport::Stdio => {
                    ui.horizontal(|ui| {
                        ui.label("Command");
                        ui.text_edit_singleline(&mut draft.command);
                    });
                    let mut args = draft.args.join("\n");
                    ui.label("Arguments (one per line)");
                    if ui.text_edit_multiline(&mut args).changed() {
                        draft.args = args.lines().map(str::to_owned).collect();
                    }
                    ui.horizontal(|ui| {
                        ui.label("Working directory");
                        ui.text_edit_singleline(&mut draft.cwd);
                    });
                    pairs(ui, "Environment", &mut draft.env);
                }
                DraftTransport::Http => {
                    ui.horizontal(|ui| {
                        ui.label("URL");
                        ui.text_edit_singleline(&mut draft.url);
                    });
                    let mut bearer = draft.token.is_some();
                    if ui.checkbox(&mut bearer, "Bearer token").changed() {
                        draft.token = bearer.then(|| DraftValue::text(""));
                    }
                    if let Some(token) = &mut draft.token {
                        value_row(ui, token);
                    }
                    pairs(ui, "Headers", &mut draft.headers);
                }
            }
            ui.horizontal(|ui| {
                ui.label("Startup timeout (ms)");
                ui.text_edit_singleline(&mut draft.startup_timeout_ms);
                ui.label("Request timeout (ms)");
                ui.text_edit_singleline(&mut draft.request_timeout_ms);
            });
            for message in check.errors.values() {
                ui.colored_label(ui.visuals().error_fg_color, message);
            }
            for warning in &check.warnings {
                ui.colored_label(ui.visuals().warn_fg_color, warning.message());
            }
            for (target, name) in draft.keychain_references() {
                ui.horizontal(|ui| {
                    let stored = saved_states
                        .as_ref()
                        .and_then(|states| states.get(&name))
                        .map(|state| format!(" ({})", secret_state_label(*state)))
                        .unwrap_or_default();
                    ui.label(format!(
                        "{} → keychain entry {name}{stored}",
                        target.label()
                    ));
                    if ui.small_button("Set value…").clicked() {
                        start_secret = Some(target.clone());
                    }
                    if ui
                        .add_enabled(can_write, egui::Button::new("Delete entry").small())
                        .clicked()
                    {
                        delete_target = Some(target.clone());
                    }
                });
            }
            if let Some(entry) = &mut editor.secret {
                ui.horizontal(|ui| {
                    ui.label(format!("Value for {}", entry.target.label()));
                    ui.add(egui::TextEdit::singleline(&mut entry.value).password(true));
                    let name = draft.keychain_name(&entry.target);
                    let ready = !entry.value.is_empty()
                        && name.is_some()
                        && check.is_valid()
                        && can_write;
                    if ui
                        .add_enabled(ready, egui::Button::new("Store secret"))
                        .clicked()
                        && let Some(name) = name
                    {
                        // Moved out of the field at once: nothing keeps it after this.
                        secret_request =
                            Some((entry.target.clone(), name, std::mem::take(&mut entry.value)));
                    }
                    if ui.button("Cancel").clicked() {
                        entry.value.clear();
                        start_secret = None;
                    }
                });
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        check.is_valid() && can_write,
                        egui::Button::new("Save server"),
                    )
                    .clicked()
                {
                    save = true;
                }
                if ui.button("Cancel edit").clicked() {
                    cancel = true;
                }
            });
            });
        });
        if let Some(target) = start_secret {
            editor.secret = Some(SecretEntry {
                target,
                value: String::new(),
            });
        }
        if rebase {
            editor.revision = settings.revision.clone();
        }
        let mutation = check
            .definition()
            .map(|(id, definition)| McpMutation::Upsert {
                server_id: id.clone(),
                definition: wire_definition(definition),
            });
        if let Some((_, name, value)) = secret_request {
            editor.secret = None;
            if let (Some(mutation), Some(definition)) = (mutation.clone(), check.definition()) {
                self.editor_store_secret(&definition.0, name, value, mutation);
            }
        } else if save && let Some(mutation) = mutation {
            self.editor_save(mutation);
        }
        if let Some(target) = delete_target {
            self.delete_secret(&target);
        }
        if cancel {
            self.editor = None;
        }
        let _ = project;
    }

    /// Save the form: to the project it was opened on, quoting the revision it
    /// was opened on, whatever is selected or reloaded since.
    fn editor_save(&mut self, mutation: McpMutation) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.saving.is_some() {
            return;
        }
        let (project, revision) = (editor.project.clone(), editor.revision.clone());
        let operation = self.fresh_operation();
        if let Some(editor) = &mut self.editor {
            editor.saving = Some(operation.clone());
        }
        self.queue_command(
            McpCommand::SaveOverride(Box::new(SaveOverride {
                operation_id: operation.clone(),
                project,
                expected_revision: revision,
                mutation,
            })),
            operation,
        );
    }

    fn editor_store_secret(
        &mut self,
        server: &str,
        logical_name: String,
        value: String,
        mutation: McpMutation,
    ) {
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.saving.is_some() {
            return;
        }
        let (project, revision) = (editor.project.clone(), editor.revision.clone());
        let operation = self.fresh_operation();
        if let Some(editor) = &mut self.editor {
            editor.saving = Some(operation.clone());
        }
        self.queue_command(
            McpCommand::SaveSecret(Box::new(SaveSecret {
                operation_id: operation.clone(),
                scope: Self::scope(&project, server),
                logical_name,
                value: McpSecretValue::new(value),
                expected_revision: revision,
                mutation,
            })),
            operation,
        );
        self.working(&project, server, McpOperationKind::SaveSecret);
    }

    /// Drop every reference to the entry from the form; an entry the saved
    /// server already uses is also deleted from the keychain (which saves).
    fn delete_secret(&mut self, target: &SecretTarget) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(name) = editor.draft.keychain_name(target) else {
            return;
        };
        editor.draft.remove_secret(target);
        let project = editor.project.clone();
        let revision = editor.revision.clone();
        let busy = editor.saving.is_some();
        let still_used = editor
            .draft
            .keychain_references()
            .iter()
            .any(|(_, other)| *other == name);
        let existing = editor.draft.existing_id.clone();
        let Some(settings) = self.settings_of(&project).cloned() else {
            return;
        };
        let saved_uses = existing
            .as_ref()
            .and_then(|id| settings.servers.iter().find(|s| &s.server_id == id))
            .and_then(|s| s.definition.as_ref())
            .is_some_and(|d| d.keychain_references().contains(&name));
        if still_used || !saved_uses || busy {
            return;
        }
        let taken: BTreeSet<String> = settings
            .servers
            .iter()
            .map(|s| s.server_id.clone())
            .filter(|id| Some(id) != existing.as_ref())
            .collect();
        let Some(editor) = &self.editor else {
            return;
        };
        let Some((server, definition)) = editor.draft.check(&taken).into_definition() else {
            return;
        };
        let operation = self.fresh_operation();
        if let Some(editor) = &mut self.editor {
            editor.saving = Some(operation.clone());
        }
        self.queue_command(
            McpCommand::DeleteSecret(Box::new(DeleteSecret {
                operation_id: operation.clone(),
                scope: Self::scope(&project, &server),
                logical_name: name,
                expected_revision: revision,
                mutation: McpMutation::Upsert {
                    server_id: server.clone(),
                    definition: wire_definition(&definition),
                },
            })),
            operation,
        );
        self.working(&project, &server, McpOperationKind::DeleteSecret);
    }
}

fn value_row(ui: &mut Ui, value: &mut DraftValue) {
    ui.horizontal(|ui| {
        let mut keychain = value.kind == DraftValueKind::Keychain;
        if ui.checkbox(&mut keychain, "Keychain reference").changed() {
            let kind = if keychain {
                DraftValueKind::Keychain
            } else {
                DraftValueKind::Text
            };
            if value.kind != kind {
                value.text.clear();
                value.kind = kind;
            }
        }
        ui.text_edit_singleline(&mut value.text);
    });
}

fn pairs(ui: &mut Ui, label: &str, rows: &mut Vec<crate::mcp_settings::DraftPair>) {
    ui.label(label);
    let mut remove = None;
    for (index, row) in rows.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut row.name);
            value_row(ui, &mut row.value);
            if ui.small_button("Remove").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        rows.remove(index);
    }
    if ui.small_button(format!("Add to {label}")).clicked() {
        rows.push(crate::mcp_settings::DraftPair::empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::commands::CommandReply;

    fn project() -> ProjectId {
        ProjectId::new("p").unwrap()
    }

    fn view(
        id: &McpOperationId,
        kind: McpOperationKind,
        state: McpOperationState,
    ) -> McpOperationView {
        McpOperationView {
            operation_id: id.clone(),
            project: project(),
            kind,
            server_id: Some("fixture".into()),
            state,
        }
    }

    fn completed(view: McpOperationView) -> UiEvent {
        UiEvent::Mcp(McpEvent::OperationCompleted { operation: view })
    }

    fn started(feature: &mut McpFeature, kind: McpOperationKind) -> McpOperationId {
        let id = feature.fresh_operation();
        feature
            .operations
            .insert(id.clone(), view(&id, kind, McpOperationState::Running));
        id
    }

    fn note(feature: &McpFeature) -> Option<&RowNote> {
        feature.notes.get(&("p".to_owned(), "fixture".to_owned()))
    }

    #[test]
    fn a_completion_event_of_my_operation_sets_the_row_note_and_others_are_ignored() {
        let mut feature = McpFeature::default();
        let mine = started(&mut feature, McpOperationKind::TestConnection);
        let foreign = McpOperationId::new("someone-else").unwrap();
        feature.event(&completed(view(
            &foreign,
            McpOperationKind::TestConnection,
            McpOperationState::Succeeded {
                outcome: McpOutcome::Tested {
                    status: McpServerStatus::Ready,
                },
            },
        )));
        assert_eq!(note(&feature), None);
        feature.event(&completed(view(
            &mine,
            McpOperationKind::TestConnection,
            McpOperationState::Succeeded {
                outcome: McpOutcome::Tested {
                    status: McpServerStatus::Ready,
                },
            },
        )));
        assert_eq!(
            note(&feature),
            Some(&RowNote::Tested(McpServerStatus::Ready))
        );
    }

    #[test]
    fn a_stale_confirmation_asks_for_a_new_review_and_a_conflict_reloads() {
        let mut feature = McpFeature::default();
        let confirm = started(&mut feature, McpOperationKind::ConfirmApproval);
        feature.event(&completed(view(
            &confirm,
            McpOperationKind::ConfirmApproval,
            McpOperationState::Failed {
                error: McpErrorCode::StalePreview,
            },
        )));
        assert!(matches!(feature.review, Some((_, Review::Stale))));
        let save = started(&mut feature, McpOperationKind::SaveOverride);
        feature.event(&completed(view(
            &save,
            McpOperationKind::SaveOverride,
            McpOperationState::Failed {
                error: McpErrorCode::ConfigConflict,
            },
        )));
        assert_eq!(feature.notice, Some(Notice::Conflict));
        assert!(
            feature.outbox.iter().any(|out| matches!(
                out,
                Outgoing::Command(Command::Mcp(McpCommand::Refresh(_)), _)
            )),
            "the list is read again, nothing is resent"
        );
    }

    #[test]
    fn a_partial_secret_delete_is_reported_as_partial_and_never_as_success() {
        let mut feature = McpFeature::default();
        let id = started(&mut feature, McpOperationKind::DeleteSecret);
        feature.event(&completed(view(
            &id,
            McpOperationKind::DeleteSecret,
            McpOperationState::Partial {
                outcome: McpOutcome::Settings {
                    revision: workbench_protocol::namespaces::mcp::McpRevision::new("0".repeat(64))
                        .unwrap(),
                },
                keychain_error: McpErrorCode::SecretDenied,
            },
        )));
        assert_eq!(
            feature.notice,
            Some(Notice::SecretDeletePartial {
                name: "fixture".into(),
                reason: McpErrorCode::SecretDenied
            })
        );
    }

    #[test]
    fn a_command_with_an_unknown_outcome_is_looked_up_after_a_reconnect_and_never_resent() {
        let mut feature = McpFeature::default();
        let id = started(&mut feature, McpOperationKind::SaveOverride);
        feature.pending.insert(
            7,
            Pending::Command {
                operation: id.clone(),
                project: project(),
            },
        );
        feature.reply(
            7,
            FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
        );
        assert_eq!(feature.notice, Some(Notice::Unknown));
        feature.reset();
        assert_eq!(feature.outbox.len(), 1);
        let Outgoing::Query(Query::Mcp(McpQuery::GetOperation { operation_id }), _) =
            &feature.outbox[0]
        else {
            panic!("the operation is asked for, not the command resent");
        };
        assert_eq!(operation_id, &id);
        // The core answers with the finished operation: the lookup is settled.
        let tag = feature.tag();
        feature.pending.insert(
            tag,
            Pending::Lookup {
                operation: id.clone(),
            },
        );
        feature.reply(
            tag,
            FeatureReply::Query(Ok(QueryReply::Mcp(McpReply::Operation(view(
                &id,
                McpOperationKind::SaveOverride,
                McpOperationState::Failed {
                    error: McpErrorCode::ConfigConflict,
                },
            ))))),
        );
        assert!(!feature.unknown.contains(&id));
        assert_eq!(feature.notice, Some(Notice::Conflict));
    }

    fn revision(c: char) -> McpRevision {
        McpRevision::new(c.to_string().repeat(64)).unwrap()
    }

    fn settings_of(project: &str, revision: McpRevision) -> McpSettings {
        McpSettings {
            project: ProjectId::new(project).unwrap(),
            revision,
            preparation_blocked: false,
            diagnostics: vec![],
            servers: vec![],
        }
    }

    fn lookup(feature: &mut McpFeature, id: &McpOperationId) -> u64 {
        let tag = feature.tag();
        feature.pending.insert(
            tag,
            Pending::Lookup {
                operation: id.clone(),
            },
        );
        tag
    }

    fn queued_commands(feature: &McpFeature) -> Vec<&McpCommand> {
        feature
            .outbox
            .iter()
            .filter_map(|out| match out {
                Outgoing::Command(Command::Mcp(command), _) => Some(command),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_lookup_the_core_does_not_hold_is_not_reported_as_not_applied() {
        let mut feature = McpFeature::default();
        let id = started(&mut feature, McpOperationKind::SetEnabled);
        feature.queue_command(
            McpCommand::SetEnabled(SetEnabled {
                operation_id: id.clone(),
                scope: McpFeature::scope(&project(), "fixture"),
                expected_revision: revision('a'),
                enabled: true,
            }),
            id.clone(),
        );
        feature.working(&project(), "fixture", McpOperationKind::SetEnabled);
        feature.unknown.insert(id.clone());
        let tag = lookup(&mut feature, &id);
        feature.reply(
            tag,
            FeatureReply::Query(Err(
                FeatureError::new("unknown operation").with_code(ErrorCode::NotFound)
            )),
        );
        assert_eq!(feature.notice, Some(Notice::Expired));
        assert_eq!(note(&feature), Some(&RowNote::Unknown));
        assert!(
            !feature.unknown.contains(&id),
            "nothing more can be learned"
        );
        assert!(!matches!(feature.notice, Some(Notice::Failed(_))));
    }

    #[test]
    fn a_failed_lookup_keeps_the_operation_tracked_as_unknown() {
        let mut feature = McpFeature::default();
        let id = started(&mut feature, McpOperationKind::SaveOverride);
        feature.unknown.insert(id.clone());
        let tag = lookup(&mut feature, &id);
        feature.reply(
            tag,
            FeatureReply::Query(Err(FeatureError::new("connection reset"))),
        );
        assert!(feature.unknown.contains(&id));
        assert!(feature.mine.contains(&id));
        assert_eq!(feature.notice, Some(Notice::Unknown));
        feature.reset();
        assert!(
            feature.outbox.iter().any(|out| matches!(
                out,
                Outgoing::Query(Query::Mcp(McpQuery::GetOperation { .. }), _)
            )),
            "it is asked for again, the command is never resent"
        );
        assert!(queued_commands(&feature).is_empty());
    }

    #[test]
    fn a_command_refused_at_once_releases_what_it_held_and_a_conflict_reloads() {
        let mut feature = McpFeature::default();
        let id = feature.fresh_operation();
        feature.queue_command(
            McpCommand::PreviewApproval(PreviewApproval {
                operation_id: id.clone(),
                scope: McpFeature::scope(&project(), "fixture"),
                expected_revision: revision('a'),
            }),
            id.clone(),
        );
        feature.working(&project(), "fixture", McpOperationKind::PreviewApproval);
        feature.review = Some((
            "fixture".into(),
            Review::Loading {
                operation: id.clone(),
            },
        ));
        let Some(Outgoing::Command(_, pending)) = feature.outbox.pop() else {
            panic!("the command is queued");
        };
        feature.pending.insert(4, pending);
        feature.reply(
            4,
            FeatureReply::Command(Err(
                FeatureError::new("settings changed").with_code(ErrorCode::Conflict)
            )),
        );
        assert!(feature.review.is_none(), "no preview is waited for");
        assert_eq!(note(&feature), None);
        assert!(!feature.mine.contains(&id));
        assert!(
            feature.outbox.iter().any(|out| matches!(
                out,
                Outgoing::Query(Query::Mcp(McpQuery::GetSettings { .. }), _)
            )),
            "the settings are read again after a conflict"
        );
    }

    #[test]
    fn a_refused_refresh_stops_the_loading_state() {
        let mut feature = McpFeature::default();
        feature.refresh(&project());
        assert!(feature.projects["p"].loading);
        let Some(Outgoing::Command(_, pending)) = feature.outbox.pop() else {
            panic!("the refresh is queued");
        };
        feature.pending.insert(2, pending);
        feature.reply(2, FeatureReply::Command(Err(FeatureError::new("busy"))));
        assert!(!feature.projects["p"].loading);
    }

    fn open_editor(feature: &mut McpFeature, project: &str, revision: McpRevision) {
        feature.editor = Some(Editor::new(
            ProjectId::new(project).unwrap(),
            revision,
            McpServerDraft::new_server(),
        ));
    }

    fn disable() -> McpMutation {
        McpMutation::Disable {
            server_id: "fixture".into(),
        }
    }

    #[test]
    fn an_edit_is_saved_to_its_own_project_and_its_own_revision() {
        let mut feature = McpFeature::default();
        for (name, rev) in [("a", 'a'), ("b", 'b')] {
            feature.projects.entry(name.into()).or_default().settings =
                Some(settings_of(name, revision(rev)));
        }
        open_editor(&mut feature, "a", revision('a'));
        // Project b is selected and reloaded to a newer revision meanwhile.
        feature.projects.get_mut("b").unwrap().settings = Some(settings_of("b", revision('c')));
        feature.projects.get_mut("a").unwrap().settings = Some(settings_of("a", revision('d')));
        feature.editor_save(disable());
        let commands = queued_commands(&feature);
        let [McpCommand::SaveOverride(save)] = commands.as_slice() else {
            panic!("one save is queued");
        };
        assert_eq!(save.project.as_str(), "a");
        assert_eq!(save.expected_revision, revision('a'));
    }

    #[test]
    fn a_secret_is_stored_and_deleted_against_the_edit_it_started_from() {
        let mut feature = McpFeature::default();
        feature.projects.entry("a".into()).or_default().settings =
            Some(settings_of("a", revision('b')));
        open_editor(&mut feature, "a", revision('a'));
        feature.editor_store_secret("fixture", "token".into(), "value".into(), disable());
        let commands = queued_commands(&feature);
        let [McpCommand::SaveSecret(save)] = commands.as_slice() else {
            panic!("one secret save is queued");
        };
        assert_eq!(save.scope.project.as_str(), "a");
        assert_eq!(save.expected_revision, revision('a'));
    }

    #[test]
    fn a_connection_test_is_cancelled_only_from_its_own_row() {
        let mut feature = McpFeature::default();
        let first = ProjectId::new("p1").unwrap();
        let second = ProjectId::new("p2").unwrap();
        feature.start_test(&first, &revision('a'), "fixture");
        feature.outbox.clear();
        assert!(!feature.cancel_test(&second, "fixture"));
        assert!(!feature.cancel_test(&first, "other"));
        assert!(queued_commands(&feature).is_empty());
        assert!(feature.cancel_test(&first, "fixture"));
        let commands = queued_commands(&feature);
        let [McpCommand::CancelOperation(cancel)] = commands.as_slice() else {
            panic!("a cancel is queued");
        };
        let started = feature.tests[&("p1".to_owned(), "fixture".to_owned())].clone();
        assert_eq!(cancel.operation_id, started);
    }

    #[test]
    fn the_end_of_a_save_closes_only_the_form_that_was_saving() {
        let mut feature = McpFeature::default();
        open_editor(&mut feature, "p", revision('a'));
        feature.editor_save(disable());
        let saving = feature.editor.as_ref().unwrap().saving.clone().unwrap();
        // The user abandons that form and opens another one while it saves.
        open_editor(&mut feature, "p", revision('a'));
        feature.operations.insert(
            saving.clone(),
            view(
                &saving,
                McpOperationKind::SaveOverride,
                McpOperationState::Running,
            ),
        );
        feature.event(&completed(view(
            &saving,
            McpOperationKind::SaveOverride,
            McpOperationState::Succeeded {
                outcome: McpOutcome::Settings {
                    revision: revision('b'),
                },
            },
        )));
        assert!(feature.editor.is_some(), "the newer draft survives");
        // The form that is saving is closed by its own operation.
        feature.editor_save(disable());
        let own = feature.editor.as_ref().unwrap().saving.clone().unwrap();
        feature.mine.insert(own.clone());
        feature.event(&completed(view(
            &own,
            McpOperationKind::SaveOverride,
            McpOperationState::Succeeded {
                outcome: McpOutcome::Settings {
                    revision: revision('c'),
                },
            },
        )));
        assert!(feature.editor.is_none());
    }

    #[test]
    fn a_failed_save_releases_its_form_for_another_try() {
        let mut feature = McpFeature::default();
        open_editor(&mut feature, "p", revision('a'));
        feature.editor_save(disable());
        let own = feature.editor.as_ref().unwrap().saving.clone().unwrap();
        feature.event(&completed(view(
            &own,
            McpOperationKind::SaveOverride,
            McpOperationState::Failed {
                error: McpErrorCode::OutcomeUnknown,
            },
        )));
        assert!(feature.editor.as_ref().unwrap().saving.is_none());
        assert!(matches!(feature.notice, Some(Notice::Failed(_))));
    }

    /// The feature drawn by egui, as the shared client draws it.
    struct Window {
        feature: McpFeature,
        cache: crate::core_client::cache::Cache,
        selection: crate::core_client::features::Selection,
        frame: crate::core_client::features::FeatureFrame,
    }

    fn window(feature: McpFeature) -> egui_kittest::Harness<'static, Window> {
        egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 3000.0))
            .build_ui_state(
                |ui, window: &mut Window| {
                    let Window {
                        feature,
                        cache,
                        selection,
                        frame,
                    } = window;
                    let mut context = FeatureContext {
                        data: &mut crate::core_client::features::data::DataInbox::default(),
                        feature: FeatureId::Mcp,
                        view: crate::core_client::features::FeatureView {
                            connected: true,
                            capabilities: &[],
                            cache,
                            selection,
                        },
                        frame,
                    };
                    feature.render(ui, &mut context);
                },
                Window {
                    feature,
                    cache: Default::default(),
                    selection: crate::core_client::features::Selection {
                        project: Some(project()),
                        thread: None,
                    },
                    frame: Default::default(),
                },
            )
    }

    #[test]
    fn a_form_that_is_saving_takes_no_input_and_what_was_typed_before_is_what_closes() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let mut feature = McpFeature::default();
        feature.projects.entry("p".into()).or_default().settings =
            Some(settings_of("p", revision('a')));
        let mut draft = McpServerDraft::new_server();
        draft.id = "x".into();
        draft.command = "cmd".into();
        feature.editor = Some(Editor::new(project(), revision('a'), draft));
        let mut window = window(feature);
        let name = |window: &egui_kittest::Harness<'static, Window>| {
            window
                .state()
                .feature
                .editor
                .as_ref()
                .unwrap()
                .draft
                .id
                .clone()
        };
        let type_name = |window: &mut egui_kittest::Harness<'static, Window>, text: &str| {
            window
                .get_all_by_role(Role::TextInput)
                .next()
                .unwrap()
                .click();
            window.run_steps(4);
            window
                .get_all_by_role(Role::TextInput)
                .next()
                .unwrap()
                .type_text(text);
            window.run_steps(4);
        };
        window.run_steps(4);
        type_name(&mut window, "y");
        assert_eq!(name(&window), "xy", "an idle form takes input");
        window.state_mut().feature.editor_save(disable());
        window.run_steps(4);
        type_name(&mut window, "z");
        assert_eq!(name(&window), "xy", "a saving form takes none");
        let saving = window
            .state()
            .feature
            .editor
            .as_ref()
            .unwrap()
            .saving
            .clone()
            .unwrap();
        window.state_mut().feature.event(&completed(view(
            &saving,
            McpOperationKind::SaveOverride,
            McpOperationState::Succeeded {
                outcome: McpOutcome::Settings {
                    revision: revision('b'),
                },
            },
        )));
        assert!(window.state().feature.editor.is_none());
    }

    #[test]
    fn after_an_unknown_outcome_the_files_are_read_again_before_the_settings_are_asked_for() {
        let mut feature = McpFeature::default();
        feature.projects.entry("p".into()).or_default().settings =
            Some(settings_of("p", revision('a')));
        open_editor(&mut feature, "p", revision('a'));
        feature.editor_save(disable());
        let own = feature.editor.as_ref().unwrap().saving.clone().unwrap();
        feature.outbox.clear();
        feature.event(&completed(view(
            &own,
            McpOperationKind::SaveOverride,
            McpOperationState::Failed {
                error: McpErrorCode::OutcomeUnknown,
            },
        )));
        assert!(
            queued_commands(&feature)
                .iter()
                .any(|command| matches!(command, McpCommand::Refresh(_))),
            "the core reads the files again"
        );
        assert!(
            !feature.outbox.iter().any(|out| matches!(
                out,
                Outgoing::Query(Query::Mcp(McpQuery::GetSettings { .. }), _)
            )),
            "its older copy is not what is asked for"
        );
        assert!(
            !queued_commands(&feature)
                .iter()
                .any(|command| matches!(command, McpCommand::SaveOverride(_))),
            "the save is never sent again"
        );
        // The read has ended (at the revision the screen already knows): only
        // now are the settings asked for.
        let refresh = feature
            .targets
            .iter()
            .find(|(_, target)| target.kind == McpOperationKind::Refresh)
            .map(|(id, _)| id.clone())
            .unwrap();
        feature.mine.insert(refresh.clone());
        feature.outbox.clear();
        feature.event(&completed(view(
            &refresh,
            McpOperationKind::Refresh,
            McpOperationState::Succeeded {
                outcome: McpOutcome::Settings {
                    revision: revision('a'),
                },
            },
        )));
        assert!(feature.outbox.iter().any(|out| matches!(
            out,
            Outgoing::Query(Query::Mcp(McpQuery::GetSettings { .. }), _)
        )));
    }

    #[test]
    fn a_finished_reload_always_asks_for_the_settings_even_at_the_same_revision() {
        let mut feature = McpFeature::default();
        feature.projects.entry("p".into()).or_default().settings =
            Some(settings_of("p", revision('a')));
        let reload = started(&mut feature, McpOperationKind::Refresh);
        feature.event(&completed(view(
            &reload,
            McpOperationKind::Refresh,
            McpOperationState::Succeeded {
                outcome: McpOutcome::Settings {
                    revision: revision('a'),
                },
            },
        )));
        assert!(feature.outbox.iter().any(|out| matches!(
            out,
            Outgoing::Query(Query::Mcp(McpQuery::GetSettings { .. }), _)
        )));
    }

    #[test]
    fn an_accepted_command_reply_records_the_running_operation() {
        let mut feature = McpFeature::default();
        let id = feature.fresh_operation();
        feature.pending.insert(
            1,
            Pending::Command {
                operation: id.clone(),
                project: project(),
            },
        );
        let running = view(&id, McpOperationKind::Refresh, McpOperationState::Running);
        feature.reply(
            1,
            FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::Mcp(
                McpResult::Operation(running.clone()),
            )))),
        );
        assert_eq!(feature.operations.get(&id), Some(&running));
        assert!(!feature.pending.contains_key(&1));
    }

    const ALL_STATUSES: [McpServerStatus; 8] = [
        McpServerStatus::Disabled,
        McpServerStatus::Invalid,
        McpServerStatus::NeedsApproval,
        McpServerStatus::SecretUnavailable,
        McpServerStatus::Connecting,
        McpServerStatus::Ready,
        McpServerStatus::Unavailable,
        McpServerStatus::ProcessLimitReached,
    ];

    #[test]
    fn server_states_read_as_the_legacy_badges() {
        let labels: Vec<_> = ALL_STATUSES.iter().map(|s| status_badge(*s).0).collect();
        assert_eq!(
            labels,
            [
                "off",
                "invalid",
                "needs approval",
                "secret unavailable",
                "connecting",
                "ready",
                "unavailable",
                "process limit",
            ]
        );
    }

    #[test]
    fn no_failure_is_shown_as_a_type_name() {
        let fields = [
            McpField::Command,
            McpField::Args,
            McpField::Cwd,
            McpField::Env,
            McpField::Url,
            McpField::Headers,
            McpField::Token,
        ];
        let mut codes = vec![
            McpErrorCode::Timeout,
            McpErrorCode::Transport,
            McpErrorCode::HttpStatus { status: 502 },
            McpErrorCode::Protocol,
            McpErrorCode::ServerRejected,
            McpErrorCode::ToolRejected,
            McpErrorCode::Configuration,
            McpErrorCode::ConfigConflict,
            McpErrorCode::NeedsApproval,
            McpErrorCode::ApprovalRevoked,
            McpErrorCode::RoleNotAllowed,
            McpErrorCode::StalePreview,
            McpErrorCode::SecretUnavailable,
            McpErrorCode::SecretLocked,
            McpErrorCode::SecretDenied,
            McpErrorCode::SecretMissing,
            McpErrorCode::ProcessLimitReached,
            McpErrorCode::PreparationLimitReached,
            McpErrorCode::Unavailable,
            McpErrorCode::Cancelled,
            McpErrorCode::OutcomeUnknown,
            McpErrorCode::InputRequired,
            McpErrorCode::LimitExceeded,
            McpErrorCode::DefinitionChanged,
            McpErrorCode::UnknownProject,
        ];
        codes.extend(
            fields
                .iter()
                .map(|field| McpErrorCode::MissingEnvironment { field: *field }),
        );
        for code in codes {
            let text = error_text(code);
            let name = format!("{code:?}");
            let name = name.split([' ', '{']).next().unwrap();
            assert!(
                !text.contains(name) || name.chars().filter(char::is_ascii_uppercase).count() < 2,
                "{text}"
            );
            assert!(!text.contains('{') && !text.contains("::"), "{text}");
        }
        assert_eq!(
            error_text(McpErrorCode::HttpStatus { status: 502 }),
            "The server answered with HTTP 502."
        );
        assert!(
            error_text(McpErrorCode::MissingEnvironment {
                field: McpField::Cwd
            })
            .contains("working directory")
        );
    }

    #[test]
    fn a_revoked_server_row_shows_its_badge_reason_and_test_result_in_words() {
        use egui_kittest::kittest::Queryable;
        let mut feature = McpFeature::default();
        let mut settings = settings_of("p", revision('a'));
        settings.servers.push(McpServerView {
            server_id: "fixture".into(),
            origin: McpOrigin::ProjectToml,
            imported_exists: false,
            tombstone: false,
            definition: None,
            diagnostics: vec![],
            state: McpServerState {
                status: McpServerStatus::NeedsApproval,
                reason: Some(McpErrorCode::ApprovalRevoked),
            },
            secret_references: vec![],
            secret_states: BTreeMap::new(),
        });
        feature.projects.entry("p".into()).or_default().settings = Some(settings);
        feature.notes.insert(
            ("p".to_owned(), "fixture".to_owned()),
            RowNote::Tested(McpServerStatus::Ready),
        );
        let mut window = window(feature);
        window.run_steps(2);
        assert!(window.query_by_label("fixture").is_some());
        assert!(window.query_by_label("needs approval").is_some());
        assert!(
            window
                .query_by_label("Approval was revoked; calls to this server are refused.")
                .is_some()
        );
        assert!(window.query_by_label("Connection test: ready.").is_some());
        assert!(window.query_by_label_contains("NeedsApproval").is_none());
        assert!(window.query_by_label_contains("ApprovalRevoked").is_none());
    }

    #[test]
    fn a_failed_operation_whose_reason_is_the_server_state_is_shown_once() {
        use egui_kittest::kittest::Queryable;
        let mut feature = McpFeature::default();
        let mut settings = settings_of("p", revision('a'));
        settings.servers.push(McpServerView {
            server_id: "fixture".into(),
            origin: McpOrigin::ProjectToml,
            imported_exists: false,
            tombstone: false,
            definition: None,
            diagnostics: vec![],
            state: McpServerState {
                status: McpServerStatus::NeedsApproval,
                reason: Some(McpErrorCode::NeedsApproval),
            },
            secret_references: vec![],
            secret_states: BTreeMap::new(),
        });
        feature.projects.entry("p".into()).or_default().settings = Some(settings);
        feature.notes.insert(
            ("p".to_owned(), "fixture".to_owned()),
            RowNote::Failed(McpErrorCode::NeedsApproval),
        );
        let mut window = window(feature);
        window.run_steps(2);
        let text = error_text(McpErrorCode::NeedsApproval);
        assert_eq!(window.query_all_by_label(&text).count(), 1);
    }

    #[test]
    fn a_cancelled_or_keychain_failed_test_shows_its_cause_once() {
        use egui_kittest::kittest::Queryable;
        for (reason, note, text) in [
            (McpErrorCode::Cancelled, RowNote::Cancelled, "Cancelled."),
            (
                McpErrorCode::SecretLocked,
                RowNote::Failed(McpErrorCode::SecretLocked),
                "The OS keychain is locked. Unlock it and try again.",
            ),
        ] {
            let mut feature = McpFeature::default();
            let mut settings = settings_of("p", revision('a'));
            settings.servers.push(McpServerView {
                server_id: "fixture".into(),
                origin: McpOrigin::ProjectToml,
                imported_exists: false,
                tombstone: false,
                definition: None,
                diagnostics: vec![],
                state: McpServerState {
                    status: McpServerStatus::Unavailable,
                    reason: Some(reason),
                },
                secret_references: vec![],
                secret_states: BTreeMap::new(),
            });
            feature.projects.entry("p".into()).or_default().settings = Some(settings);
            feature
                .notes
                .insert(("p".to_owned(), "fixture".to_owned()), note);
            let mut window = window(feature);
            window.run_steps(2);
            assert_eq!(window.query_all_by_label(text).count(), 1, "{text}");
            assert_eq!(
                window
                    .query_all_by_label_contains("keychain is locked")
                    .count(),
                usize::from(reason == McpErrorCode::SecretLocked)
            );
            assert!(
                window
                    .query_by_label_contains("may not have been stored")
                    .is_none()
            );
        }
    }

    #[test]
    fn a_server_state_reason_never_claims_a_failed_write_or_a_reload() {
        for (status, reason) in [
            (McpServerStatus::SecretUnavailable, None),
            (
                McpServerStatus::SecretUnavailable,
                Some(McpErrorCode::SecretLocked),
            ),
            (
                McpServerStatus::SecretUnavailable,
                Some(McpErrorCode::SecretDenied),
            ),
            (
                McpServerStatus::Unavailable,
                Some(McpErrorCode::OutcomeUnknown),
            ),
            (
                McpServerStatus::Unavailable,
                Some(McpErrorCode::ConfigConflict),
            ),
        ] {
            let text = status_reason(&McpServerState { status, reason })
                .map(state_reason_text)
                .unwrap();
            assert!(!text.contains("may not have been stored"), "{text}");
            assert!(!text.contains("is reloaded"), "{text}");
        }
        assert_eq!(
            status_reason(&McpServerState {
                status: McpServerStatus::SecretUnavailable,
                reason: None,
            })
            .map(state_reason_text)
            .unwrap(),
            "The OS keychain is not available. Nothing was read or stored, and evorch does not fall back to a file."
        );
    }

    #[test]
    fn a_private_destination_review_lists_the_addresses_its_warning_refers_to() {
        use egui_kittest::kittest::Queryable;
        use workbench_protocol::namespaces::mcp::{
            McpApprovalSummary, McpDestination, McpFingerprint, McpPreviewId,
        };
        let mut feature = McpFeature::default();
        let mut settings = settings_of("p", revision('a'));
        settings.servers.push(McpServerView {
            server_id: "fixture".into(),
            origin: McpOrigin::ProjectToml,
            imported_exists: false,
            tombstone: false,
            definition: None,
            diagnostics: vec![],
            state: McpServerState {
                status: McpServerStatus::NeedsApproval,
                reason: None,
            },
            secret_references: vec![],
            secret_states: BTreeMap::new(),
        });
        feature.projects.entry("p".into()).or_default().settings = Some(settings);
        feature.review = Some((
            "fixture".into(),
            Review::Ready(Box::new(McpApprovalPreview {
                preview_id: McpPreviewId::from_bytes(&[1; 16]),
                scope: McpScope {
                    project: project(),
                    server_id: "fixture".into(),
                },
                source_revision: revision('a'),
                fingerprint: McpFingerprint::from_bytes(&[2; 32]),
                warnings: vec![McpApprovalWarning::PrivateDestination],
                previous_fingerprint: None,
                previous_summary: None,
                summary: McpApprovalSummary {
                    destination: Some(McpDestination {
                        scheme: "http".into(),
                        host: "localhost".into(),
                        port: 8080,
                    }),
                    ..Default::default()
                },
                approved_ips: vec!["127.0.0.1".into(), "::1".into()],
            })),
        ));
        let mut window = window(feature);
        window.run_steps(2);
        assert!(
            window
                .query_by_label("Allowed addresses: 127.0.0.1, ::1")
                .is_some()
        );
        assert!(
            window
                .query_by_label_contains("connects only to the addresses listed above")
                .is_some()
        );
    }
}
