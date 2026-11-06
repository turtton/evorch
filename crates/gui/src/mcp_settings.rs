//! Project MCP server settings: list, add, edit, disable, delete and override
//! removal for `.evorch/mcp.toml` (with `.mcp.json` as a read-only import).
//!
//! All loading and saving goes through [`McpGuiService`] on a background
//! executor; the render thread only reads finished results from a channel.
//! Opening or saving settings never launches or connects to a server, and
//! `.mcp.json` is never written: editing an imported server saves a complete
//! override in `mcp.toml`, and disabling it saves a tombstone.

mod draft;
mod pane;

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use config::mcp::{McpOverrideMutation, McpServerEntry, McpSource, ProjectMcpConfig};
use tokio::sync::watch;

use crate::mcp::{McpErrorKind, McpGuiService, McpSettingsView, McpStatusEvent};

pub use draft::{
    DEFAULT_ROLES, DraftCheck, DraftField, DraftPair, DraftTransport, DraftValue, DraftValueKind,
    DraftWarning, McpServerDraft, config_error_text,
};
pub use pane::{
    ADD_SERVER_LABEL, APPLY_NOTICE, CLOSE_MCP_LABEL, MCP_SECTION_LABEL, MCP_SERVERS_BUTTON_LABEL,
    SAVE_SERVER_LABEL, mcp_settings_modal, mcp_settings_section,
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
    /// Leave the MCP dialog; late results of running tasks are ignored.
    Close,
}

/// Outcome of the last save, shown above the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpNotice {
    Saved,
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

type StatusReceiver = watch::Receiver<Vec<McpStatusEvent>>;

enum TaskResult {
    Loaded(Result<(McpSettingsView, StatusReceiver), McpErrorKind>),
    Saved {
        from_editor: bool,
        result: Result<McpSettingsView, McpErrorKind>,
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
        let waker = self.waker.lock().ok().and_then(|waker| waker.clone());
        if let Some(waker) = waker {
            waker();
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(result) = self.fallback.take() {
            self.deliver(result);
        }
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
        self.session = Some(Session {
            root,
            load: McpLoadState::Loading,
            view: None,
            status: None,
            editor: None,
            pending_delete: None,
            notice: None,
            in_flight: false,
        });
        self.start_load();
    }

    /// Forgets the open project; late results of its tasks are ignored.
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

    pub fn pending_delete(&self) -> Option<&str> {
        self.session.as_ref()?.pending_delete.as_deref()
    }

    pub fn notice(&self) -> Option<&McpNotice> {
        self.session.as_ref()?.notice.as_ref()
    }

    /// A load or save is running; row actions and Save wait for it.
    pub fn is_busy(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.in_flight)
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

    /// Checks the open editor against the loaded server IDs.
    pub fn check_editor(&self) -> Option<DraftCheck> {
        let session = self.session.as_ref()?;
        let editor = session.editor.as_ref()?;
        Some(editor.check(&taken_ids(session)))
    }

    /// Applies finished background results. Never blocks.
    pub fn poll(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(message) => self.apply(message),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Blocks until the running load or save finishes. For headless drivers
    /// and tests; the GUI render thread uses [`Self::poll`] instead.
    pub fn wait_idle(&mut self) {
        while self.is_busy() {
            match self.rx.recv() {
                Ok(message) => self.apply(message),
                Err(_) => return,
            }
        }
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
                }
            }
            McpSettingsAction::CancelEdit => session.editor = None,
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
        session.in_flight = false;
        match message.result {
            TaskResult::Loaded(Ok((view, status))) => {
                session.view = Some(view);
                session.load = McpLoadState::Loaded;
                session.status = Some(status);
            }
            TaskResult::Loaded(Err(kind)) => {
                session.load = McpLoadState::Failed(error_text(kind));
            }
            TaskResult::Saved {
                from_editor,
                result: Ok(view),
            } => {
                session.view = Some(view);
                session.load = McpLoadState::Loaded;
                session.notice = Some(McpNotice::Saved);
                if from_editor {
                    session.editor = None;
                }
            }
            // The edit stays open so it can be saved again after a reload.
            TaskResult::Saved {
                result: Err(kind), ..
            } if is_conflict(kind) => session.notice = Some(McpNotice::Conflict),
            TaskResult::Saved {
                result: Err(kind), ..
            } => session.notice = Some(McpNotice::Failed(error_text(kind))),
        }
    }
}

fn taken_ids(session: &Session) -> BTreeSet<String> {
    session
        .view
        .as_ref()
        .map(|view| view.config.servers.keys().cloned().collect())
        .unwrap_or_default()
}

/// A save refused because the files changed after they were loaded.
fn is_conflict(kind: McpErrorKind) -> bool {
    matches!(kind, McpErrorKind::ConfigConflict)
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
