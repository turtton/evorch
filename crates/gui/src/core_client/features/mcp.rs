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
use config::mcp::{MCP_ROLE_KEYS, McpServerEntry, McpSource};
use egui::{RichText, Ui};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use workbench_host::areas::mcp::{config_definition, wire_definition};
use workbench_protocol::{
    commands::{Command, CommandResult},
    events::UiEvent,
    handshake::Capability,
    ids::ProjectId,
    namespaces::mcp::{
        CancelOperation, ConfirmApproval, DeleteSecret, McpApprovalPreview, McpCommand,
        McpErrorCode, McpEvent, McpMutation, McpOperationId, McpOperationKind, McpOperationState,
        McpOperationView, McpOrigin, McpOutcome, McpQuery, McpReply, McpResult, McpScope,
        McpSecretValue, McpServerStatus, McpServerView, McpSettings, PreviewApproval, Refresh,
        RevokeApproval, SaveOverride, SaveSecret, SetEnabled, TestConnection,
    },
    queries::{Query, QueryReply},
};

const REQUIRED: &[Capability] = &[Capability::Commands, Capability::Queries];

/// What a request in flight stands for. Kept across `reset`: a reply may only
/// arrive after the connection was replaced.
#[derive(Debug, Clone)]
enum Pending {
    /// A command that started `operation`.
    Command {
        operation: McpOperationId,
    },
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
}

enum Review {
    Loading { operation: McpOperationId },
    Ready(Box<McpApprovalPreview>),
    Confirming(Box<McpApprovalPreview>),
    Stale,
    Failed(McpErrorCode),
}

struct Editor {
    draft: McpServerDraft,
    secret: Option<SecretEntry>,
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
        McpErrorCode::SecretUnavailable => "The OS keychain is not available. Nothing was stored.".into(),
        McpErrorCode::SecretLocked => "The OS keychain is locked. Unlock it and try again.".into(),
        McpErrorCode::SecretDenied => "Access to the OS keychain was denied. Nothing was stored.".into(),
        McpErrorCode::SecretMissing => "The keychain entry has no stored value yet.".into(),
        other => format!("Failed: {other:?}"),
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
        self.outbox.push(Outgoing::Command(
            Command::Mcp(command),
            Pending::Command { operation },
        ));
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
        match (&view.state, view.kind) {
            (McpOperationState::Succeeded { outcome }, kind) => match outcome {
                McpOutcome::Settings { revision } => {
                    let known = self.settings_of(&project).map(|s| s.revision.clone());
                    if known.as_ref() != Some(revision) {
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
                    if matches!(
                        kind,
                        McpOperationKind::SaveOverride | McpOperationKind::SaveSecret
                    ) && self.editor.is_some()
                    {
                        self.editor = None;
                    }
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
            (McpOperationState::Partial { keychain_error, .. }, _) => {
                self.notice = Some(Notice::SecretDeletePartial {
                    name: server.clone(),
                    reason: *keychain_error,
                });
                self.queue_settings(&project);
                self.notes.remove(&key);
            }
            (McpOperationState::Cancelled, _) => {
                self.notes.insert(key, RowNote::Cancelled);
            }
            (McpOperationState::Failed { error }, kind) => {
                match (*error, kind) {
                    (McpErrorCode::StalePreview, McpOperationKind::ConfirmApproval) => {
                        self.review = Some((server.clone(), Review::Stale));
                    }
                    (error, McpOperationKind::PreviewApproval) => {
                        self.review = Some((server.clone(), Review::Failed(error)));
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
                if let Pending::Command { operation } = pending {
                    self.unknown.insert(operation);
                    self.notice = Some(Notice::Unknown);
                }
            }
            FeatureReply::Command(Err(error)) => {
                self.pending.remove(&tag);
                if let Pending::Command { operation } = pending {
                    self.mine.remove(&operation);
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
                    // The core has no such operation: the command never took
                    // effect (an applied command is always remembered).
                    Pending::Lookup { operation } => {
                        self.unknown.remove(&operation);
                        self.mine.remove(&operation);
                        self.notice = Some(Notice::Failed("The request was not applied.".into()));
                    }
                    Pending::Settings { project } => {
                        if let Some(state) = self.projects.get_mut(project.as_str()) {
                            state.loading = false;
                        }
                        self.notice = Some(Notice::Failed(error.message));
                    }
                    Pending::Command { .. } => {}
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

fn status_label(view: &McpServerView) -> String {
    match view.state.reason {
        Some(reason) => format!("{:?} ({reason:?})", view.state.status),
        None => format!("{:?}", view.state.status),
    }
}

impl McpFeature {
    fn draw(&mut self, ui: &mut Ui, project: &ProjectId) {
        if let Some(notice) = &self.notice {
            let text = match notice {
                Notice::Saved => "Saved. Changes apply from the next new run.".to_owned(),
                Notice::SecretStored => "The secret is stored in the OS keychain.".to_owned(),
                Notice::SecretDeleted(name) => format!("The secret of {name} was deleted."),
                Notice::SecretDeletePartial { name, reason } => format!(
                    "The references of {name} are removed, but deleting the keychain entry failed ({reason:?}): its value may still be stored."
                ),
                Notice::Conflict => error_text(McpErrorCode::ConfigConflict),
                Notice::Failed(message) => message.clone(),
                Notice::Unknown => {
                    "The result of a request is not known yet; it is being looked up.".to_owned()
                }
            };
            ui.horizontal(|ui| {
                ui.label(text);
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
                self.editor = Some(Editor {
                    draft: McpServerDraft::new_server(),
                    secret: None,
                });
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
        self.review_dialog(ui, project, &settings);
        self.editor_dialog(ui, project, &settings);
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
                ui.label(status_label(server));
            });
            if let Some(note) = &note {
                ui.label(match note {
                    RowNote::Working(kind) => format!("Working: {kind:?}"),
                    RowNote::Approved => "Approved. It is used from the next new run.".into(),
                    RowNote::Revoked => "Approval revoked.".into(),
                    RowNote::Tested(status) => format!("Test result: {status:?}"),
                    RowNote::Failed(error) => error_text(*error),
                    RowNote::Cancelled => "Cancelled.".into(),
                });
            }
            ui.horizontal_wrapped(|ui| {
                if actions.edit && ui.button("Edit").clicked() {
                    self.editor = Some(Editor {
                        draft: McpServerDraft::from_entry(&id, &entry_of(server)),
                        secret: None,
                    });
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
            if let Some(operation) = self
                .operations
                .values()
                .find(|op| {
                    op.kind == McpOperationKind::TestConnection
                        && op.server_id.as_deref() == Some(id)
                        && !op.state.is_finished()
                })
                .map(|op| op.operation_id.clone())
                && ui.button("Cancel test").clicked()
            {
                self.queue_command(
                    McpCommand::CancelOperation(CancelOperation {
                        operation_id: operation.clone(),
                    }),
                    operation,
                );
            }
        } else if ui.button("Test connection").clicked() {
            let operation = self.fresh_operation();
            self.queue_command(
                McpCommand::TestConnection(TestConnection {
                    operation_id: operation.clone(),
                    scope: Self::scope(project, id),
                    expected_revision: settings.revision.clone(),
                }),
                operation,
            );
            self.working(project, id, McpOperationKind::TestConnection);
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

    fn review_dialog(&mut self, ui: &mut Ui, project: &ProjectId, settings: &McpSettings) {
        let Some((server, review)) = &self.review else {
            return;
        };
        let server = server.clone();
        let mut action = None;
        ui.group(|ui| match review {
            Review::Loading { .. } => {
                ui.label("Reading what approving would allow…");
            }
            Review::Ready(preview) | Review::Confirming(preview) => {
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
                for warning in &preview.warnings {
                    ui.colored_label(ui.visuals().warn_fg_color, format!("{warning:?}"));
                }
                if !preview.summary.secret_references.is_empty() {
                    ui.label(format!(
                        "Keychain references: {}",
                        preview.summary.secret_references.join(", ")
                    ));
                }
                let confirming = matches!(review, Review::Confirming(_));
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
                    self.review = Some((server, Review::Confirming(preview)));
                }
            }
            Some(false) => self.review = None,
            None => {}
        }
        let _ = settings;
    }

    fn editor_dialog(&mut self, ui: &mut Ui, project: &ProjectId, settings: &McpSettings) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let taken: BTreeSet<String> = settings
            .servers
            .iter()
            .map(|s| s.server_id.clone())
            .collect();
        let check = editor.draft.check(&taken);
        let mut save = false;
        let mut cancel = false;
        let mut secret_request: Option<(SecretTarget, String, String)> = None;
        let mut delete_target: Option<SecretTarget> = None;
        let mut start_secret: Option<SecretTarget> = None;
        ui.group(|ui| {
            let draft = &mut editor.draft;
            ui.label(
                RichText::new(if draft.is_new() {
                    "Add MCP server"
                } else {
                    "Edit MCP server"
                })
                .strong(),
            );
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
                    ui.label(format!("{} → keychain entry {name}", target.label()));
                    if ui.small_button("Set value…").clicked() {
                        start_secret = Some(target.clone());
                    }
                    if ui.small_button("Delete entry").clicked() {
                        delete_target = Some(target.clone());
                    }
                });
            }
            if let Some(entry) = &mut editor.secret {
                ui.horizontal(|ui| {
                    ui.label(format!("Value for {}", entry.target.label()));
                    ui.add(egui::TextEdit::singleline(&mut entry.value).password(true));
                    let name = draft.keychain_name(&entry.target);
                    let ready = !entry.value.is_empty() && name.is_some() && check.is_valid();
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
                    .add_enabled(check.is_valid(), egui::Button::new("Save server"))
                    .clicked()
                {
                    save = true;
                }
                if ui.button("Cancel edit").clicked() {
                    cancel = true;
                }
            });
        });
        if let Some(target) = start_secret {
            editor.secret = Some(SecretEntry {
                target,
                value: String::new(),
            });
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
                let operation = self.fresh_operation();
                let server = definition.0.clone();
                self.queue_command(
                    McpCommand::SaveSecret(Box::new(SaveSecret {
                        operation_id: operation.clone(),
                        scope: Self::scope(project, &server),
                        logical_name: name,
                        value: McpSecretValue::new(value),
                        expected_revision: settings.revision.clone(),
                        mutation,
                    })),
                    operation,
                );
                self.working(project, &server, McpOperationKind::SaveSecret);
            }
        } else if save && let Some(mutation) = mutation {
            self.save_mutation(project, settings, mutation);
        }
        if let Some(target) = delete_target {
            self.delete_secret(project, settings, &target);
        }
        if cancel {
            self.editor = None;
        }
    }

    /// Drop every reference to the entry from the form; an entry the saved
    /// server already uses is also deleted from the keychain (which saves).
    fn delete_secret(
        &mut self,
        project: &ProjectId,
        settings: &McpSettings,
        target: &SecretTarget,
    ) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(name) = editor.draft.keychain_name(target) else {
            return;
        };
        editor.draft.remove_secret(target);
        let still_used = editor
            .draft
            .keychain_references()
            .iter()
            .any(|(_, other)| *other == name);
        let saved_uses = editor
            .draft
            .existing_id
            .as_ref()
            .and_then(|id| settings.servers.iter().find(|s| &s.server_id == id))
            .and_then(|s| s.definition.as_ref())
            .is_some_and(|d| d.keychain_references().contains(&name));
        if still_used || !saved_uses {
            return;
        }
        let taken: BTreeSet<String> = settings
            .servers
            .iter()
            .map(|s| s.server_id.clone())
            .filter(|id| Some(id) != editor.draft.existing_id.as_ref())
            .collect();
        let Some((server, definition)) = editor.draft.check(&taken).into_definition() else {
            return;
        };
        let operation = self.fresh_operation();
        self.queue_command(
            McpCommand::DeleteSecret(Box::new(DeleteSecret {
                operation_id: operation.clone(),
                scope: Self::scope(project, &server),
                logical_name: name,
                expected_revision: settings.revision.clone(),
                mutation: McpMutation::Upsert {
                    server_id: server.clone(),
                    definition: wire_definition(&definition),
                },
            })),
            operation,
        );
        self.working(project, &server, McpOperationKind::DeleteSecret);
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

    #[test]
    fn a_lookup_the_core_does_not_know_means_the_request_was_not_applied() {
        let mut feature = McpFeature::default();
        let id = started(&mut feature, McpOperationKind::SaveOverride);
        feature.unknown.insert(id.clone());
        feature.pending.insert(
            3,
            Pending::Lookup {
                operation: id.clone(),
            },
        );
        feature.reply(
            3,
            FeatureReply::Query(Err(FeatureError::new("unknown operation"))),
        );
        assert!(!feature.unknown.contains(&id));
        assert!(matches!(feature.notice, Some(Notice::Failed(_))));
    }

    #[test]
    fn an_accepted_command_reply_records_the_running_operation() {
        let mut feature = McpFeature::default();
        let id = feature.fresh_operation();
        feature.pending.insert(
            1,
            Pending::Command {
                operation: id.clone(),
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
}
