//! Project MCP server settings: list, add, edit, disable, delete and override
//! removal for `.evorch/mcp.toml` (with `.mcp.json` as a read-only import),
//! plus the privileged actions: approval, revocation, connection tests and
//! storing secrets in the OS keychain.
//!
//! All loading, saving and privileged calls go through [`McpGuiService`] on a
//! background executor; the render thread only reads finished results from a
//! channel. Opening or saving settings never launches or connects to a server,
//! and `.mcp.json` is never written: editing an imported server saves a
//! complete override in `mcp.toml`, and disabling it saves a tombstone.
//!
//! Approving a server never changes the tools of a conversation that is
//! already running; it is used from the next new run.

mod draft;
mod notices;
mod pane;
mod review;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use config::mcp::{McpOverrideMutation, McpServerEntry, McpSource, ProjectMcpConfig};
use runtime::mcp::{ApprovalScope, CancelContext};
use tokio::sync::watch;

use crate::mcp::{
    ConfirmMcpApproval, DeleteMcpSecret, DeleteMcpSecretOutcome, McpApprovalPreview, McpErrorKind,
    McpGuiService, McpServerStatus, McpSettingsView, McpStatusEvent, SaveMcpSecret,
};

pub use draft::{
    DEFAULT_ROLES, DraftCheck, DraftField, DraftPair, DraftTransport, DraftValue, DraftValueKind,
    DraftWarning, McpServerDraft, SecretTarget, config_error_text,
};
pub use notices::{McpApprovalNotice, McpNoticeAction, NOTICE_REVIEW_LABEL, mcp_approval_notices};
pub use pane::{
    ADD_SERVER_LABEL, APPLY_NOTICE, CLOSE_MCP_LABEL, MCP_SECTION_LABEL, MCP_SERVERS_BUTTON_LABEL,
    SAVE_SERVER_LABEL, mcp_settings_modal, mcp_settings_section,
};
pub use review::{
    APPROVE_LABEL, NEXT_RUN_NOTICE, OTHER_CHANGES_TEXT, OTHER_MAYBE_CHANGED_TEXT,
    OUTSIDE_SANDBOX_WARNING, REVIEW_AGAIN_LABEL, STALE_PREVIEW_TEXT, STORE_SECRET_LABEL,
    TEST_REFUSED_TEXT, UNVERIFIED_CONTENT_NOTE, secret_delete_failed_text,
    secret_delete_partial_text, secret_deleted_text, secret_error_text, status_reason_text,
};

/// Asks the UI to draw another frame once background work finishes.
pub type McpWaker = Arc<dyn Fn() + Send + Sync>;

/// What the user asked for in the MCP section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSettingsAction {
    Reload,
    Add,
    Edit(String),
    CancelEdit,
    SaveEdit,
    /// Flip `enabled` of a server defined in `mcp.toml` (complete replacement).
    SetEnabled {
        server_id: String,
        enabled: bool,
    },
    /// Hide an imported server with a `{ enabled = false }` tombstone.
    Disable(String),
    /// Drop the `mcp.toml` table so the `.mcp.json` definition applies again.
    RemoveOverride(String),
    RequestDelete(String),
    ConfirmDelete,
    CancelDelete,
    /// Ask the service what approving this server would allow.
    ReviewApproval(String),
    /// Approve exactly the preview on screen.
    ConfirmApproval,
    CancelApproval,
    RevokeApproval(String),
    /// Start and stop the server once, with the same approval as a run.
    TestConnection(String),
    /// Open the masked input for the keychain value of `target` in the editor.
    EditSecret(SecretTarget),
    CancelSecret,
    /// Store the typed value in the keychain and save the editor with only the reference.
    SaveSecret,
    /// Drop every reference to the keychain entry `target` names. An entry
    /// the saved server already uses is also deleted from the keychain, which
    /// saves the editor; otherwise only the form changes.
    DeleteSecret(SecretTarget),
    /// Leave the MCP dialog; late results of running tasks are ignored.
    Close,
}

/// Outcome of the last save, shown above the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpNotice {
    Saved,
    /// A secret went to the OS keychain and `mcp.toml` got only its name.
    SecretStored,
    /// The references are gone from `mcp.toml` and the keychain entry is deleted.
    SecretDeleted(String),
    /// The references are gone from `mcp.toml`, but deleting the keychain
    /// entry failed: its value may still be stored.
    SecretDeletePartial {
        name: String,
        reason: McpErrorKind,
    },
    /// The files changed on disk since they were loaded; nothing was written.
    Conflict,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpLoadState {
    Loading,
    Loaded,
    Failed(String),
}

/// The approval dialog for one server.
#[derive(Debug, Clone)]
pub struct McpApprovalReview {
    pub server_id: String,
    pub state: McpApprovalState,
}

#[derive(Debug, Clone)]
pub enum McpApprovalState {
    Loading,
    Ready(Box<McpApprovalPreview>),
    Confirming(Box<McpApprovalPreview>),
    /// The configuration changed after the preview, so it cannot be approved.
    Stale,
    Failed(String),
}

/// Result of the last approval, revocation or connection test of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRowNote {
    Approved,
    Revoked,
    Testing,
    Tested(McpServerStatus),
    /// Refused because the server is not approved; nothing was started.
    TestRefused,
    Failed(String),
}

/// The masked keychain value being typed for one reference of the editor.
pub struct SecretInput {
    pub target: SecretTarget,
    /// Cleared as soon as it is handed to the service, whatever the outcome.
    pub value: String,
    pub saving: bool,
}

impl std::fmt::Debug for SecretInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretInput")
            .field("target", &self.target)
            .field("saving", &self.saving)
            .finish_non_exhaustive()
    }
}

/// Whether a server row is shown as coming from `.mcp.json`, from `mcp.toml`
/// alone, or from a `mcp.toml` table overriding `.mcp.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpEntryOrigin {
    ImportedJson,
    ProjectToml,
    Override,
}

impl McpEntryOrigin {
    pub fn of(entry: &McpServerEntry) -> Self {
        match (entry.source, entry.imported_exists) {
            (McpSource::ImportedJson, _) => Self::ImportedJson,
            (McpSource::ProjectToml, true) => Self::Override,
            (McpSource::ProjectToml, false) => Self::ProjectToml,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ImportedJson => ".mcp.json",
            Self::ProjectToml => "mcp.toml",
            Self::Override => "override",
        }
    }
}

/// Row actions available for an entry, derived from where it is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct McpEntryActions {
    pub edit: bool,
    /// `Some(target)` flips `enabled` through a full `mcp.toml` definition.
    pub set_enabled: Option<bool>,
    /// Write a tombstone over the imported definition.
    pub disable: bool,
    pub remove_override: bool,
    /// Remove a server that exists only in `mcp.toml`.
    pub delete: bool,
}

impl McpEntryActions {
    pub fn of(entry: &McpServerEntry) -> Self {
        let origin = McpEntryOrigin::of(entry);
        let enabled = entry
            .definition
            .as_ref()
            .map(|definition| definition.enabled);
        match origin {
            McpEntryOrigin::ImportedJson => Self {
                edit: true,
                // An import that is already off can only be turned on by overriding it.
                set_enabled: (enabled == Some(false)).then_some(true),
                disable: enabled == Some(true),
                ..Self::default()
            },
            McpEntryOrigin::Override => Self {
                edit: !entry.tombstone,
                set_enabled: enabled.map(|enabled| !enabled),
                disable: !entry.tombstone,
                remove_override: true,
                delete: false,
            },
            McpEntryOrigin::ProjectToml => Self {
                edit: !entry.tombstone,
                set_enabled: enabled.map(|enabled| !enabled),
                disable: false,
                remove_override: false,
                delete: true,
            },
        }
    }
}

/// Borrowed parts of the open editor that the secret input needs.
pub struct EditorParts<'a> {
    pub draft: &'a mut McpServerDraft,
    pub secret: &'a mut Option<SecretInput>,
    /// Keychain names stored through this dialog since it was opened.
    pub stored: &'a BTreeSet<String>,
    /// Keychain names the saved definition already referenced.
    pub saved: BTreeSet<String>,
    pub status: Option<McpStatusEvent>,
}

type StatusReceiver = watch::Receiver<Vec<McpStatusEvent>>;

enum TaskResult {
    Loaded(Result<(McpSettingsView, StatusReceiver), McpErrorKind>),
    Saved {
        from_editor: bool,
        result: Result<McpSettingsView, McpErrorKind>,
    },
    SecretSaved {
        logical_name: String,
        result: Result<McpSettingsView, McpErrorKind>,
    },
    SecretDeleted {
        logical_name: String,
        result: Result<Box<DeleteMcpSecretOutcome>, McpErrorKind>,
    },
    Previewed {
        server_id: String,
        result: Result<Box<McpApprovalPreview>, McpErrorKind>,
    },
    Confirmed {
        server_id: String,
        result: Result<(), McpErrorKind>,
    },
    Revoked {
        server_id: String,
        result: Result<(), McpErrorKind>,
    },
    Tested {
        server_id: String,
        result: Result<McpServerStatus, McpErrorKind>,
    },
}

struct TaskMessage {
    generation: u64,
    result: TaskResult,
}

/// Sends exactly one result, even when the task is dropped or panics.
struct Reply {
    tx: Sender<TaskMessage>,
    generation: u64,
    waker: Arc<Mutex<Option<McpWaker>>>,
    fallback: Option<TaskResult>,
}

impl Reply {
    fn send(mut self, result: TaskResult) {
        self.fallback = None;
        self.deliver(result);
    }

    fn deliver(&self, result: TaskResult) {
        let _ = self.tx.send(TaskMessage {
            generation: self.generation,
            result,
        });
        wake(&self.waker);
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(result) = self.fallback.take() {
            self.deliver(result);
        }
    }
}

fn wake(slot: &Mutex<Option<McpWaker>>) {
    let waker = slot.lock().ok().and_then(|waker| waker.clone());
    if let Some(waker) = waker {
        waker();
    }
}

struct Session {
    root: PathBuf,
    load: McpLoadState,
    view: Option<McpSettingsView>,
    status: Option<StatusReceiver>,
    editor: Option<McpServerDraft>,
    pending_delete: Option<String>,
    notice: Option<McpNotice>,
    in_flight: bool,
    approval: Option<McpApprovalReview>,
    /// Review requested before the list finished loading.
    pending_review: Option<String>,
    notes: BTreeMap<String, McpRowNote>,
    /// Rows with a revocation or connection test running.
    row_tasks: BTreeSet<String>,
    tests: Vec<CancelContext>,
    secret: Option<SecretInput>,
    stored_secrets: BTreeSet<String>,
}

impl Session {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            load: McpLoadState::Loading,
            view: None,
            status: None,
            editor: None,
            pending_delete: None,
            notice: None,
            in_flight: false,
            approval: None,
            pending_review: None,
            notes: BTreeMap::new(),
            row_tasks: BTreeSet::new(),
            tests: Vec::new(),
            secret: None,
            stored_secrets: BTreeSet::new(),
        }
    }

    fn scope(&self, server_id: &str) -> Option<(ApprovalScope, config::mcp::SourceRevision)> {
        let view = self.view.as_ref()?;
        Some((
            ApprovalScope {
                canonical_root: view.config.canonical_root.clone(),
                server_id: server_id.to_owned(),
            },
            view.config.revision,
        ))
    }
}

impl Drop for Session {
    // Closing the dialog stops connection tests it started.
    fn drop(&mut self) {
        for cancel in &self.tests {
            cancel.cancel();
        }
    }
}

/// State and background plumbing for the MCP section of project settings.
pub struct McpSettingsModel {
    service: Option<Arc<dyn McpGuiService>>,
    runtime: Option<tokio::runtime::Handle>,
    waker: Arc<Mutex<Option<McpWaker>>>,
    tx: Sender<TaskMessage>,
    rx: Receiver<TaskMessage>,
    generation: u64,
    session: Option<Session>,
    notices: notices::ProjectWatches,
}

impl Default for McpSettingsModel {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            service: None,
            runtime: None,
            waker: Arc::default(),
            tx,
            rx,
            generation: 0,
            session: None,
            notices: notices::ProjectWatches::default(),
        }
    }
}

impl McpSettingsModel {
    /// `runtime` runs service calls; without it each call gets its own thread.
    pub fn new(service: Arc<dyn McpGuiService>, runtime: Option<tokio::runtime::Handle>) -> Self {
        Self {
            service: Some(service),
            runtime,
            ..Self::default()
        }
    }

    pub fn is_available(&self) -> bool {
        self.service.is_some()
    }

    pub fn set_waker(&self, waker: McpWaker) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(waker);
        }
    }

    pub fn has_waker(&self) -> bool {
        self.waker.lock().is_ok_and(|slot| slot.is_some())
    }

    /// Starts loading `root` unless it is already the open project.
    pub fn ensure_open(&mut self, root: &Path) {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.root == root)
        {
            return;
        }
        self.open(root.to_path_buf());
    }

    fn open(&mut self, root: PathBuf) {
        self.generation += 1;
        self.notices.dismiss_project(&root);
        self.session = Some(Session::new(root));
        self.start_load();
    }

    /// Forgets the open project; late results of its tasks are ignored and
    /// its running connection tests are cancelled.
    pub fn close(&mut self) {
        if self.session.take().is_some() {
            self.generation += 1;
        }
    }

    pub fn root(&self) -> Option<&Path> {
        self.session.as_ref().map(|session| session.root.as_path())
    }

    pub fn load_state(&self) -> Option<&McpLoadState> {
        self.session.as_ref().map(|session| &session.load)
    }

    pub fn config(&self) -> Option<&ProjectMcpConfig> {
        self.session
            .as_ref()?
            .view
            .as_ref()
            .map(|view| &view.config)
    }

    pub fn editor(&self) -> Option<&McpServerDraft> {
        self.session.as_ref()?.editor.as_ref()
    }

    pub fn editor_mut(&mut self) -> Option<&mut McpServerDraft> {
        self.session.as_mut()?.editor.as_mut()
    }

    /// The editor with its keychain input, for the secret section.
    pub fn editor_parts(&mut self) -> Option<EditorParts<'_>> {
        let status = self
            .editor()
            .and_then(|draft| draft.existing_id.clone())
            .and_then(|id| self.status(&id));
        let session = self.session.as_mut()?;
        let saved = saved_references(session);
        let draft = session.editor.as_mut()?;
        Some(EditorParts {
            draft,
            secret: &mut session.secret,
            stored: &session.stored_secrets,
            saved,
            status,
        })
    }

    /// The keychain value being typed, if the input is open.
    pub fn secret_input(&self) -> Option<&SecretInput> {
        self.session.as_ref()?.secret.as_ref()
    }

    pub fn secret_input_mut(&mut self) -> Option<&mut SecretInput> {
        self.session.as_mut()?.secret.as_mut()
    }

    pub fn pending_delete(&self) -> Option<&str> {
        self.session.as_ref()?.pending_delete.as_deref()
    }

    pub fn notice(&self) -> Option<&McpNotice> {
        self.session.as_ref()?.notice.as_ref()
    }

    pub fn approval(&self) -> Option<&McpApprovalReview> {
        self.session.as_ref()?.approval.as_ref()
    }

    pub fn row_note(&self, server_id: &str) -> Option<&McpRowNote> {
        self.session.as_ref()?.notes.get(server_id)
    }

    /// A revocation or connection test of this row is running.
    pub fn row_busy(&self, server_id: &str) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.row_tasks.contains(server_id))
    }

    /// A load or save is running; row actions and Save wait for it.
    pub fn is_busy(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.in_flight)
    }

    /// Any background call of the open dialog is still running.
    pub fn has_pending_work(&self) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session.in_flight
                || !session.row_tasks.is_empty()
                || session.approval.as_ref().is_some_and(|review| {
                    matches!(
                        review.state,
                        McpApprovalState::Loading | McpApprovalState::Confirming(_)
                    )
                })
        })
    }

    /// Latest status for `server_id`: live subscription first, then the loaded view.
    pub fn status(&self, server_id: &str) -> Option<McpStatusEvent> {
        let session = self.session.as_ref()?;
        let matches = |event: &&McpStatusEvent| event.scope.server_id == server_id;
        if let Some(status) = &session.status
            && let Some(event) = status.borrow().iter().find(matches)
        {
            return Some(event.clone());
        }
        session
            .view
            .as_ref()?
            .statuses
            .iter()
            .find(matches)
            .cloned()
    }

    /// Whether the server is known to be unapproved. An approval or a
    /// revocation made in this dialog wins over the last reported status.
    pub fn needs_approval(&self, server_id: &str) -> bool {
        match self.row_note(server_id) {
            Some(McpRowNote::Revoked) => true,
            Some(McpRowNote::Approved) => false,
            _ => self
                .status(server_id)
                .is_some_and(|event| event.status == McpServerStatus::NeedsApproval),
        }
    }

    /// Whether a connection test may be started for the row now.
    pub fn can_test(&self, server_id: &str) -> bool {
        let runnable = self
            .config()
            .and_then(|config| config.servers.get(server_id))
            .and_then(|entry| entry.definition.as_ref())
            .is_some_and(|definition| definition.enabled);
        runnable && !self.needs_approval(server_id) && !self.is_busy() && !self.row_busy(server_id)
    }

    /// Checks the open editor against the loaded server IDs.
    pub fn check_editor(&self) -> Option<DraftCheck> {
        let session = self.session.as_ref()?;
        let editor = session.editor.as_ref()?;
        Some(editor.check(&taken_ids(session)))
    }

    /// Run-start approval notices that are waiting for the user.
    pub fn approval_notices(&self) -> &[McpApprovalNotice] {
        self.notices.notices()
    }

    pub fn dismiss_notice(&mut self, notice: &McpApprovalNotice) {
        self.notices.dismiss(notice);
    }

    /// Follows the server status of every project in `roots`, so a run that
    /// skips an unapproved server raises a notice. Subscribing happens off the
    /// render thread; known projects cost only a lookup.
    pub fn watch_projects<'a>(&mut self, roots: impl IntoIterator<Item = &'a Path>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        for root in roots {
            if let Some(task) = self.notices.start(root, &service, &self.waker) {
                self.spawn(task);
            }
        }
    }

    /// Applies finished background results. Never blocks.
    pub fn poll(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.apply(message);
        }
        let open = self.session.as_ref().map(|session| session.root.clone());
        self.notices.poll(open.as_deref());
    }

    /// Blocks until every running call of the dialog finishes. For headless
    /// drivers and tests; the GUI render thread uses [`Self::poll`] instead.
    pub fn wait_idle(&mut self) {
        while self.has_pending_work() {
            match self.rx.recv() {
                Ok(message) => self.apply(message),
                Err(_) => return,
            }
        }
    }

    /// Blocks until the project status subscriptions started so far are in
    /// place. For headless drivers and tests, like [`Self::wait_idle`].
    pub fn wait_project_watches(&mut self) {
        self.notices.wait_subscribed();
    }

    pub fn apply_action(&mut self, action: McpSettingsAction) {
        if action == McpSettingsAction::Close {
            self.close();
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match action {
            McpSettingsAction::Close => {}
            McpSettingsAction::Reload => {
                if session.in_flight {
                    return;
                }
                session.notice = None;
                self.start_load();
            }
            McpSettingsAction::Add => {
                session.pending_delete = None;
                session.secret = None;
                session.editor = Some(McpServerDraft::new_server());
            }
            McpSettingsAction::Edit(id) => {
                let entry = session
                    .view
                    .as_ref()
                    .and_then(|view| view.config.servers.get(&id));
                if let Some(entry) = entry {
                    session.editor = Some(McpServerDraft::from_entry(&id, entry));
                    session.pending_delete = None;
                    session.secret = None;
                }
            }
            McpSettingsAction::CancelEdit => {
                session.editor = None;
                session.secret = None;
            }
            McpSettingsAction::SaveEdit => {
                let Some(check) = session
                    .editor
                    .as_ref()
                    .map(|editor| editor.check(&taken_ids(session)))
                else {
                    return;
                };
                if let Some((server_id, definition)) = check.into_definition() {
                    self.save(
                        McpOverrideMutation::Upsert {
                            server_id,
                            definition,
                        },
                        true,
                    );
                }
            }
            McpSettingsAction::SetEnabled { server_id, enabled } => {
                let definition = session
                    .view
                    .as_ref()
                    .and_then(|view| view.config.servers.get(&server_id))
                    .and_then(|entry| entry.definition.clone());
                if let Some(mut definition) = definition {
                    definition.enabled = enabled;
                    self.save(
                        McpOverrideMutation::Upsert {
                            server_id,
                            definition,
                        },
                        false,
                    );
                }
            }
            McpSettingsAction::Disable(server_id) => {
                self.save(McpOverrideMutation::Disable { server_id }, false);
            }
            McpSettingsAction::RemoveOverride(server_id) => {
                self.save(McpOverrideMutation::RemoveOverride { server_id }, false);
            }
            McpSettingsAction::RequestDelete(id) => session.pending_delete = Some(id),
            McpSettingsAction::CancelDelete => session.pending_delete = None,
            McpSettingsAction::ConfirmDelete => {
                if let Some(server_id) = session.pending_delete.take() {
                    // Only mcp.toml-only servers are deletable; removing the
                    // table deletes them without touching .mcp.json.
                    self.save(McpOverrideMutation::RemoveOverride { server_id }, false);
                }
            }
            McpSettingsAction::ReviewApproval(id) => self.review(id),
            McpSettingsAction::ConfirmApproval => self.confirm(),
            McpSettingsAction::CancelApproval => session.approval = None,
            McpSettingsAction::RevokeApproval(id) => self.revoke(id),
            McpSettingsAction::TestConnection(id) => self.test(id),
            McpSettingsAction::EditSecret(target) => {
                if session.editor.is_some() {
                    session.secret = Some(SecretInput {
                        target,
                        value: String::new(),
                        saving: false,
                    });
                }
            }
            McpSettingsAction::CancelSecret => session.secret = None,
            McpSettingsAction::SaveSecret => self.save_secret(),
            McpSettingsAction::DeleteSecret(target) => self.delete_secret(&target),
        }
    }

    fn start_load(&mut self) {
        let generation = self.generation;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let Some(service) = self.service.clone() else {
            session.load = McpLoadState::Failed(error_text(McpErrorKind::Unavailable));
            return;
        };
        session.in_flight = true;
        if session.view.is_none() {
            session.load = McpLoadState::Loading;
        }
        let root = session.root.clone();
        let reply = self.reply(
            generation,
            TaskResult::Loaded(Err(McpErrorKind::Unavailable)),
        );
        self.spawn(async move {
            let result = service.settings(root.clone()).await.map(|view| {
                let status = service.subscribe_status(view.config.canonical_root.clone());
                (view, status)
            });
            reply.send(TaskResult::Loaded(result));
        });
    }

    fn save(&mut self, mutation: McpOverrideMutation, from_editor: bool) {
        let generation = self.generation;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let (Some(service), Some(view)) = (self.service.clone(), session.view.as_ref()) else {
            return;
        };
        if session.in_flight {
            return;
        }
        session.in_flight = true;
        session.notice = None;
        let root = session.root.clone();
        let revision = view.config.revision;
        let reply = self.reply(
            generation,
            TaskResult::Saved {
                from_editor,
                result: Err(McpErrorKind::Unavailable),
            },
        );
        self.spawn(async move {
            let result = service.save_override(root, revision, mutation).await;
            reply.send(TaskResult::Saved {
                from_editor,
                result,
            });
        });
    }

    /// Keychain first, then `mcp.toml` with only `{ keychain = "<name>" }`;
    /// both happen in the service. The typed value leaves the form here.
    fn save_secret(&mut self) {
        let generation = self.generation;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let Some(service) = self.service.clone() else {
            return;
        };
        if session.in_flight {
            return;
        }
        let (Some(editor), Some(input)) = (session.editor.as_ref(), session.secret.as_ref()) else {
            return;
        };
        let Some(logical_name) = editor.keychain_name(&input.target) else {
            return;
        };
        if input.value.is_empty() {
            return;
        }
        let Some((server_id, definition)) = editor.check(&taken_ids(session)).into_definition()
        else {
            return;
        };
        let Some((scope, expected_revision)) = session.scope(&server_id) else {
            return;
        };
        let Some(input) = session.secret.as_mut() else {
            return;
        };
        let value = sandbox::Secret::from(std::mem::take(&mut input.value));
        input.saving = true;
        session.in_flight = true;
        session.notice = None;
        let request = SaveMcpSecret {
            scope,
            logical_name: logical_name.clone(),
            value,
            expected_revision,
            mutation: McpOverrideMutation::Upsert {
                server_id,
                definition,
            },
        };
        let reply = self.reply(
            generation,
            TaskResult::SecretSaved {
                logical_name: logical_name.clone(),
                result: Err(McpErrorKind::Unavailable),
            },
        );
        self.spawn(async move {
            let result = service.save_secret(request).await;
            reply.send(TaskResult::SecretSaved {
                logical_name,
                result,
            });
        });
    }

    fn delete_secret(&mut self, target: &SecretTarget) {
        let generation = self.generation;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if session.in_flight {
            return;
        }
        let saved = saved_references(session);
        let Some(editor) = session.editor.as_ref() else {
            return;
        };
        let Some(logical_name) = editor.keychain_name(target) else {
            return;
        };
        let mut without = editor.clone();
        for (other, name) in editor.keychain_references() {
            if name == logical_name {
                without.remove_secret(&other);
            }
        }
        let stored = editor.existing_id.is_some()
            && (saved.contains(&logical_name) || session.stored_secrets.contains(&logical_name));
        if !stored {
            // Nothing in the keychain belongs to this reference yet.
            if session
                .secret
                .as_ref()
                .is_some_and(|input| without.keychain_name(&input.target).is_none())
            {
                session.secret = None;
            }
            session.editor = Some(without);
            return;
        }
        let Some(service) = self.service.clone() else {
            return;
        };
        let Some((server_id, definition)) = without.check(&taken_ids(session)).into_definition()
        else {
            return;
        };
        let Some((scope, expected_revision)) = session.scope(&server_id) else {
            return;
        };
        session.in_flight = true;
        session.notice = None;
        let request = DeleteMcpSecret {
            scope,
            logical_name: logical_name.clone(),
            expected_revision,
            mutation: McpOverrideMutation::Upsert {
                server_id,
                definition,
            },
        };
        let reply = self.reply(
            generation,
            TaskResult::SecretDeleted {
                logical_name: logical_name.clone(),
                result: Err(McpErrorKind::Unavailable),
            },
        );
        self.spawn(async move {
            let result = service.delete_secret(request).await.map(Box::new);
            reply.send(TaskResult::SecretDeleted {
                logical_name,
                result,
            });
        });
    }

    fn review(&mut self, server_id: String) {
        let generation = self.generation;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let stale = session
            .approval
            .as_ref()
            .is_some_and(|review| matches!(review.state, McpApprovalState::Stale));
        if session.view.is_none() || stale {
            // A stale preview means the loaded files are stale too: reload
            // first, then ask for a preview of what is on disk now.
            session.pending_review = Some(server_id);
            session.approval = None;
            if !session.in_flight {
                self.start_load();
            }
            return;
        }
        let (Some(service), Some((scope, revision))) =
            (self.service.clone(), session.scope(&server_id))
        else {
            return;
        };
        session.approval = Some(McpApprovalReview {
            server_id: server_id.clone(),
            state: McpApprovalState::Loading,
        });
        let reply = self.reply(
            generation,
            TaskResult::Previewed {
                server_id: server_id.clone(),
                result: Err(McpErrorKind::Unavailable),
            },
        );
        self.spawn(async move {
            let result = service
                .preview_approval(scope, revision)
                .await
                .map(Box::new);
            reply.send(TaskResult::Previewed { server_id, result });
        });
    }

    /// Sends the preview's own identity, fingerprint and revision so the
    /// service refuses it if anything changed since it was shown.
    fn confirm(&mut self) {
        let generation = self.generation;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let Some(service) = self.service.clone() else {
            return;
        };
        let Some(review) = session.approval.as_mut() else {
            return;
        };
        let McpApprovalState::Ready(preview) = &review.state else {
            return;
        };
        let confirmation = ConfirmMcpApproval {
            preview_id: preview.id,
            scope: preview.scope.clone(),
            expected_revision: preview.source_revision,
            expected_fingerprint: preview.fingerprint,
        };
        review.state = McpApprovalState::Confirming(preview.clone());
        let server_id = review.server_id.clone();
        let reply = self.reply(
            generation,
            TaskResult::Confirmed {
                server_id: server_id.clone(),
                result: Err(McpErrorKind::Unavailable),
            },
        );
        self.spawn(async move {
            let result = service.confirm_approval(confirmation).await;
            reply.send(TaskResult::Confirmed { server_id, result });
        });
    }

    fn revoke(&mut self, server_id: String) {
        let generation = self.generation;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let (Some(service), Some((scope, _))) = (self.service.clone(), session.scope(&server_id))
        else {
            return;
        };
        if !session.row_tasks.insert(server_id.clone()) {
            return;
        }
        if session
            .approval
            .as_ref()
            .is_some_and(|review| review.server_id == server_id)
        {
            session.approval = None;
        }
        let reply = self.reply(
            generation,
            TaskResult::Revoked {
                server_id: server_id.clone(),
                result: Err(McpErrorKind::Unavailable),
            },
        );
        self.spawn(async move {
            let result = service.revoke_approval(scope).await;
            reply.send(TaskResult::Revoked { server_id, result });
        });
    }

    fn test(&mut self, server_id: String) {
        let generation = self.generation;
        if self.needs_approval(&server_id) {
            // Refused here as well as by the service: nothing is started.
            if let Some(session) = self.session.as_mut() {
                session.notes.insert(server_id, McpRowNote::TestRefused);
            }
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let (Some(service), Some((scope, revision))) =
            (self.service.clone(), session.scope(&server_id))
        else {
            return;
        };
        if session.in_flight || !session.row_tasks.insert(server_id.clone()) {
            return;
        }
        session.notes.insert(server_id.clone(), McpRowNote::Testing);
        let cancel = CancelContext::default();
        session.tests.push(cancel.clone());
        let reply = self.reply(
            generation,
            TaskResult::Tested {
                server_id: server_id.clone(),
                result: Err(McpErrorKind::Cancelled),
            },
        );
        self.spawn(async move {
            let result = service.test_connection(scope, revision, cancel).await;
            reply.send(TaskResult::Tested { server_id, result });
        });
    }

    fn reply(&self, generation: u64, fallback: TaskResult) -> Reply {
        Reply {
            tx: self.tx.clone(),
            generation,
            waker: Arc::clone(&self.waker),
            fallback: Some(fallback),
        }
    }

    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        if let Some(runtime) = &self.runtime {
            drop(runtime.spawn(task));
            return;
        }
        // A dropped task still answers through its Reply guard.
        let _ = std::thread::Builder::new()
            .name("mcp-settings".into())
            .spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(task);
                }
            });
    }

    fn apply(&mut self, message: TaskMessage) {
        if message.generation != self.generation {
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let mut review_next = None;
        match message.result {
            TaskResult::Loaded(Ok((view, status))) => {
                session.in_flight = false;
                session.view = Some(view);
                session.load = McpLoadState::Loaded;
                session.status = Some(status);
                review_next = session.pending_review.take();
            }
            TaskResult::Loaded(Err(kind)) => {
                session.in_flight = false;
                session.pending_review = None;
                session.load = McpLoadState::Failed(error_text(kind));
            }
            TaskResult::Saved {
                from_editor,
                result: Ok(view),
            } => {
                session.in_flight = false;
                session.view = Some(view);
                session.load = McpLoadState::Loaded;
                session.notice = Some(McpNotice::Saved);
                if from_editor {
                    session.editor = None;
                    session.secret = None;
                }
            }
            // The edit stays open so it can be saved again after a reload.
            TaskResult::Saved {
                result: Err(kind), ..
            } => {
                session.in_flight = false;
                session.notice = Some(save_failure(kind));
            }
            TaskResult::SecretSaved {
                logical_name,
                result: Ok(view),
            } => {
                session.in_flight = false;
                session.view = Some(view);
                session.load = McpLoadState::Loaded;
                session.notice = Some(McpNotice::SecretStored);
                session.stored_secrets.insert(logical_name);
                session.editor = None;
                session.secret = None;
            }
            // Nothing was written in plain text; the value must be typed again.
            TaskResult::SecretSaved {
                result: Err(kind), ..
            } => {
                session.in_flight = false;
                if let Some(input) = session.secret.as_mut() {
                    input.saving = false;
                    input.value.clear();
                }
                session.notice = Some(match kind {
                    McpErrorKind::ConfigConflict => McpNotice::Conflict,
                    other => McpNotice::Failed(secret_error_text(other)),
                });
            }
            // Settings were saved: the form is done, whatever the keychain said.
            TaskResult::SecretDeleted {
                logical_name,
                result: Ok(outcome),
            } => {
                session.in_flight = false;
                session.view = Some(outcome.view);
                session.load = McpLoadState::Loaded;
                session.editor = None;
                session.secret = None;
                session.notice = Some(match outcome.keychain_error {
                    None => {
                        session.stored_secrets.remove(&logical_name);
                        McpNotice::SecretDeleted(logical_name)
                    }
                    Some(reason) => McpNotice::SecretDeletePartial {
                        name: logical_name,
                        reason,
                    },
                });
            }
            // Nothing was saved or deleted; the edit stays open.
            TaskResult::SecretDeleted {
                result: Err(kind), ..
            } => {
                session.in_flight = false;
                session.notice = Some(match kind {
                    McpErrorKind::ConfigConflict => McpNotice::Conflict,
                    other => McpNotice::Failed(secret_delete_failed_text(other)),
                });
            }
            TaskResult::Previewed { server_id, result } => {
                let Some(review) = session.approval.as_mut() else {
                    return;
                };
                if review.server_id != server_id
                    || !matches!(review.state, McpApprovalState::Loading)
                {
                    return;
                }
                review.state = match result {
                    Ok(preview) => McpApprovalState::Ready(preview),
                    Err(kind) if is_stale(kind) => McpApprovalState::Stale,
                    Err(kind) => McpApprovalState::Failed(status_reason_text(kind)),
                };
            }
            TaskResult::Confirmed { server_id, result } => {
                let current = session
                    .approval
                    .as_ref()
                    .is_some_and(|review| review.server_id == server_id);
                match result {
                    Ok(()) => {
                        session.notes.insert(server_id, McpRowNote::Approved);
                        if current {
                            session.approval = None;
                        }
                    }
                    Err(kind) => {
                        if let Some(review) = session.approval.as_mut().filter(|_| current) {
                            review.state = if is_stale(kind) {
                                McpApprovalState::Stale
                            } else {
                                McpApprovalState::Failed(status_reason_text(kind))
                            };
                        }
                    }
                }
            }
            TaskResult::Revoked { server_id, result } => {
                session.row_tasks.remove(&server_id);
                let note = match result {
                    Ok(()) => McpRowNote::Revoked,
                    Err(kind) => McpRowNote::Failed(status_reason_text(kind)),
                };
                session.notes.insert(server_id, note);
            }
            TaskResult::Tested { server_id, result } => {
                session.row_tasks.remove(&server_id);
                let note = match result {
                    Ok(status) => McpRowNote::Tested(status),
                    Err(McpErrorKind::NeedsApproval | McpErrorKind::ApprovalRevoked) => {
                        McpRowNote::TestRefused
                    }
                    Err(kind) => McpRowNote::Failed(status_reason_text(kind)),
                };
                session.notes.insert(server_id, note);
            }
        }
        if let Some(id) = review_next {
            self.review(id);
        }
    }
}

/// Keychain names the saved definition of the edited server references.
fn saved_references(session: &Session) -> BTreeSet<String> {
    session
        .editor
        .as_ref()
        .and_then(|draft| draft.existing_id.as_ref())
        .and_then(|id| session.view.as_ref()?.config.servers.get(id))
        .map(|entry| {
            McpServerDraft::from_entry("", entry)
                .keychain_references()
                .into_iter()
                .map(|(_, name)| name)
                .collect()
        })
        .unwrap_or_default()
}

fn taken_ids(session: &Session) -> BTreeSet<String> {
    session
        .view
        .as_ref()
        .map(|view| view.config.servers.keys().cloned().collect())
        .unwrap_or_default()
}

/// The preview no longer matches the files or the approval being confirmed.
fn is_stale(kind: McpErrorKind) -> bool {
    matches!(
        kind,
        McpErrorKind::StalePreview | McpErrorKind::ConfigConflict
    )
}

fn save_failure(kind: McpErrorKind) -> McpNotice {
    match kind {
        // A save refused because the files changed after they were loaded.
        McpErrorKind::ConfigConflict => McpNotice::Conflict,
        other => McpNotice::Failed(error_text(other)),
    }
}

fn error_text(kind: McpErrorKind) -> String {
    match kind {
        McpErrorKind::Unavailable => "MCP settings are unavailable in this session.".to_owned(),
        McpErrorKind::Configuration => {
            "The MCP configuration could not be read or saved. Check .evorch/mcp.toml.".to_owned()
        }
        McpErrorKind::LimitExceeded => "The MCP configuration exceeds a size limit.".to_owned(),
        McpErrorKind::ConfigConflict => {
            "The MCP configuration changed on disk. Reload before saving.".to_owned()
        }
        other => format!("MCP settings failed ({other})."),
    }
}
