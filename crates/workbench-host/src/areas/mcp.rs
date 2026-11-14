//! Host service of the `mcp` area: project MCP settings, approvals, connection
//! tests and keychain-backed secrets (ADR 0045, PR 11).
//!
//! The service is a thin adapter over the privileged `McpGuiService` that the
//! bootstrap already composed (the same instance the runs use, so one approval
//! ledger and one process limiter). It adds what a remote client needs and the
//! in-process GUI never did:
//!
//! * every state-changing command becomes an *operation* with a client-chosen
//!   id. The handler returns at once (handlers must not wait); the work runs as
//!   an [`AreaTasks`](super::AreaTasks) task and its end is published as an
//!   event and kept in a bounded table, so a client that disconnected can read
//!   the outcome later with `get_operation`. The same id with the same request
//!   is answered from the table, never run twice; the same id with another
//!   request is refused;
//! * projects are named by id and resolved to a directory here, never by the
//!   client;
//! * the settings the core last read are cached per project, so a query never
//!   waits and a command quoting an older revision than the core knows is
//!   refused before anything runs (the runtime re-checks against the files);
//! * a secret travels from the command into the keychain call and nowhere else:
//!   it is not part of the request digest, the operation table, any event, or
//!   any error text.
//!
//! Nothing here edits a run's frozen tool catalog: settings, approvals and
//! `listChanged` only matter for the next run the runtime prepares.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;
use config::mcp as cfg;
use runtime::mcp::{
    ApprovalScope, CancelContext, ConfirmMcpApproval, DeleteMcpSecret, McpErrorKind, McpGuiService,
    McpSecretPresence, McpSecretRollback, McpSecretWrite, McpSettingsView, McpStatusEvent,
    SaveMcpSecret, SaveMcpSecretReport,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    pin::pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    task::Poll,
};
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    events::UiEvent,
    ids::ProjectId,
    namespaces::mcp::{self as wire, McpCommand, McpEvent, McpOperationId, McpOperationView},
};

use super::{ProjectionSource, StopSignal};

/// Projects whose server states the area follows with a task of its own. A
/// project beyond this still has every state it reads after an operation.
const MAX_WATCHED_PROJECTS: usize = 8;

/// The `mcp` service, reached from `ingress/namespaces/mcp.rs` as
/// `self.areas.mcp`. Without handles (the unwired form) every operation is
/// refused.
#[derive(Default)]
pub struct McpArea {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    gui: Arc<dyn McpGuiService>,
    tasks: super::AreaTasks,
    source: ProjectionSource,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    operations: HashMap<McpOperationId, Entry>,
    /// Oldest first.
    order: VecDeque<McpOperationId>,
    settings: HashMap<ProjectId, wire::McpSettings>,
    watched: HashSet<PathBuf>,
    /// One writer at a time per project directory, so two commands quoting the
    /// same revision always end as one success and one `config_conflict`
    /// instead of racing on the files.
    writers: HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>,
}

struct Entry {
    view: McpOperationView,
    /// Digest of the request without any secret, to tell a repeat from a
    /// different request under the same id.
    digest: [u8; 32],
    cancel: Option<Arc<CancelContext>>,
}

/// How an accepted command stands.
pub struct Submitted {
    pub view: McpOperationView,
    /// The same operation id and request were accepted before; nothing was started.
    pub already_applied: bool,
}

fn error(code: ErrorCode, message: &str, retry: RetryAdvice) -> WorkbenchError {
    WorkbenchError::new(code, message, retry)
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl McpArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("mcp")
            .map_err(|e| BootstrapError::from(e.message))?;
        Ok(Self {
            inner: Some(Arc::new(Inner {
                gui: handles.mcp.gui.clone(),
                tasks: handles.tasks.clone(),
                source,
                state: Mutex::default(),
            })),
        })
    }

    fn inner(&self) -> Result<&Arc<Inner>, WorkbenchError> {
        self.inner.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "MCP management is not available in this host",
                RetryAdvice::Never,
            )
        })
    }

    /// Accept a command: start its operation, or answer from the table.
    /// `root` is the directory of the command's project, resolved by the
    /// caller from the project id (absent for a command that names none).
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request or a revision older than
    /// the one the core knows, `LimitExceeded` when every retained operation is
    /// still running, `Draining` once the host is stopping.
    pub fn submit(
        &self,
        command: McpCommand,
        root: Option<PathBuf>,
    ) -> Result<Submitted, WorkbenchError> {
        self.submit_tracked(command, root, None)
    }

    pub(crate) fn submit_tracked(
        &self,
        command: McpCommand,
        root: Option<PathBuf>,
        accepted: Option<crate::bootstrap::restart::HostCommandPermit>,
    ) -> Result<Submitted, WorkbenchError> {
        let inner = self.inner()?;
        if let McpCommand::CancelOperation(cancel) = &command {
            return inner.cancel(&cancel.operation_id);
        }
        let id = command.operation_id().clone();
        let digest = request_digest(&command);
        let (project, kind, server_id) = describe(&command);
        {
            let state = lock(&inner.state);
            if let Some(entry) = state.operations.get(&id) {
                return if entry.digest == digest {
                    Ok(Submitted {
                        view: entry.view.clone(),
                        already_applied: true,
                    })
                } else {
                    Err(error(
                        ErrorCode::Conflict,
                        "this operation id was used for a different request",
                        RetryAdvice::Never,
                    ))
                };
            }
        }
        let project = project.ok_or_else(|| {
            error(
                ErrorCode::InvalidRequest,
                "the command names no project",
                RetryAdvice::Never,
            )
        })?;
        let root =
            root.ok_or_else(|| error(ErrorCode::NotFound, "unknown project", RetryAdvice::Never))?;
        let cancel = matches!(command, McpCommand::TestConnection(_))
            .then(|| Arc::new(CancelContext::default()));
        let work = Work::of(command, root, cancel.clone());
        let view = McpOperationView {
            operation_id: id.clone(),
            project: project.clone(),
            kind,
            server_id,
            state: wire::McpOperationState::Running,
        };
        {
            let mut state = lock(&inner.state);
            if let Some(known) = work.expected_revision()
                && let Some(current) = state.settings.get(&project)
                && &current.revision != known
            {
                return Err(error(
                    ErrorCode::Conflict,
                    "the settings changed since the revision quoted; read them again",
                    RetryAdvice::AfterResync,
                ));
            }
            state.make_room()?;
            state.order.push_back(id.clone());
            state.operations.insert(
                id.clone(),
                Entry {
                    view: view.clone(),
                    digest,
                    cancel,
                },
            );
        }
        let task_inner = inner.clone();
        let task_id = id.clone();
        let spawned = inner
            .tasks
            .spawn_counted_tracked("mcp-operation", accepted, move |stop| {
                task_inner.run(task_id, project, work, stop)
            });
        if let Err(refused) = spawned {
            lock(&inner.state).forget(&id);
            return Err(error(
                ErrorCode::Draining,
                &refused.to_string(),
                RetryAdvice::AfterReconnect,
            ));
        }
        Ok(Submitted {
            view,
            already_applied: false,
        })
    }

    /// The settings the core last read for `project`.
    ///
    /// # Errors
    /// `NotFound` before the first `refresh` of the project.
    pub fn settings(&self, project: &ProjectId) -> Result<wire::McpSettings, WorkbenchError> {
        let inner = self.inner()?;
        lock(&inner.state)
            .settings
            .get(project)
            .cloned()
            .ok_or_else(|| {
                error(
                    ErrorCode::NotFound,
                    "the settings are not loaded yet; send refresh",
                    RetryAdvice::Never,
                )
            })
    }

    /// One operation, running or finished.
    ///
    /// # Errors
    /// `NotFound` for an id the core does not hold (never accepted, evicted, or
    /// forgotten with a restart).
    pub fn operation(&self, id: &McpOperationId) -> Result<McpOperationView, WorkbenchError> {
        let inner = self.inner()?;
        lock(&inner.state)
            .operations
            .get(id)
            .map(|entry| entry.view.clone())
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown operation", RetryAdvice::Never))
    }

    /// The retained operations, newest first.
    pub fn operations(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<McpOperationView>, WorkbenchError> {
        let inner = self.inner()?;
        let state = lock(&inner.state);
        Ok(state
            .order
            .iter()
            .rev()
            .filter_map(|id| state.operations.get(id))
            .filter(|entry| project.is_none_or(|p| &entry.view.project == p))
            .map(|entry| entry.view.clone())
            .collect())
    }
}

impl State {
    /// Keep the table within bounds by dropping the oldest finished operation.
    fn make_room(&mut self) -> Result<(), WorkbenchError> {
        if self.operations.len() < wire::MAX_OPERATIONS {
            return Ok(());
        }
        let oldest = self
            .order
            .iter()
            .find(|id| {
                self.operations
                    .get(*id)
                    .is_some_and(|e| e.view.state.is_finished())
            })
            .cloned();
        match oldest {
            Some(id) => {
                self.forget(&id);
                Ok(())
            }
            None => Err(error(
                ErrorCode::LimitExceeded,
                "too many MCP operations are running",
                RetryAdvice::SameRequest,
            )),
        }
    }

    fn forget(&mut self, id: &McpOperationId) {
        self.operations.remove(id);
        self.order.retain(|queued| queued != id);
    }
}

impl Inner {
    fn publish(&self, event: McpEvent) {
        if let Err(error) = self.source.publish(UiEvent::Mcp(event)) {
            tracing::warn!(code = ?error.code, "mcp event could not be published");
        }
    }

    fn cancel(&self, id: &McpOperationId) -> Result<Submitted, WorkbenchError> {
        let state = lock(&self.state);
        let entry = state
            .operations
            .get(id)
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown operation", RetryAdvice::Never))?;
        if entry.view.state.is_finished() {
            return Ok(Submitted {
                view: entry.view.clone(),
                already_applied: true,
            });
        }
        let Some(cancel) = &entry.cancel else {
            return Err(error(
                ErrorCode::InvalidRequest,
                "only a connection test can be cancelled",
                RetryAdvice::Never,
            ));
        };
        cancel.cancel();
        Ok(Submitted {
            view: entry.view.clone(),
            already_applied: false,
        })
    }

    /// Record `view` as the project's settings. Publishes `settings_changed`
    /// when the revision is new to the core.
    fn remember_settings(&self, project: &ProjectId, settings: wire::McpSettings) {
        let revision = settings.revision.clone();
        let changed = {
            let mut state = lock(&self.state);
            let changed = state
                .settings
                .get(project)
                .is_none_or(|known| known.revision != revision);
            state.settings.insert(project.clone(), settings);
            changed
        };
        if changed {
            self.publish(McpEvent::SettingsChanged {
                project: project.clone(),
                revision,
            });
        }
    }

    async fn run(
        self: Arc<Self>,
        id: McpOperationId,
        project: ProjectId,
        work: Work,
        mut stop: StopSignal,
    ) -> Result<(), BootstrapError> {
        let cancel = work.cancel();
        let outcome = {
            let mut execution = pin!(self.execute(work));
            let mut stopped = pin!(stop.stopped());
            let mut cancelled_by_stop = false;
            std::future::poll_fn(|cx| {
                if !cancelled_by_stop && stopped.as_mut().poll(cx).is_ready() {
                    // The host is stopping: end the connection test now and let it
                    // reap its server before this task returns.
                    cancelled_by_stop = true;
                    if let Some(cancel) = &cancel {
                        cancel.cancel();
                    }
                }
                execution.as_mut().poll(cx)
            })
            .await
        };
        let root = outcome.root.clone();
        if let Some(settings) = outcome.settings {
            self.remember_settings(&project, settings);
        }
        if let Some(root) = root {
            self.watch(&project, root);
        }
        let view = {
            let mut state = lock(&self.state);
            state.operations.get_mut(&id).map(|entry| {
                entry.view.state = outcome.state;
                entry.cancel = None;
                entry.view.clone()
            })
        };
        if let Some(operation) = view {
            self.publish(McpEvent::OperationCompleted { operation });
        }
        Ok(())
    }

    /// Follow the server states of `project` with one task, once.
    fn watch(self: &Arc<Self>, project: &ProjectId, root: PathBuf) {
        {
            let mut state = lock(&self.state);
            if state.watched.contains(&root) || state.watched.len() >= MAX_WATCHED_PROJECTS {
                return;
            }
            state.watched.insert(root.clone());
        }
        let inner = self.clone();
        let project = project.clone();
        let started = self.tasks.spawn("mcp-status-watch", {
            let root = root.clone();
            move |mut stop| async move {
                let mut receiver = inner.gui.subscribe_status(root.clone());
                let mut seen: HashMap<String, wire::McpServerState> = HashMap::new();
                for event in receiver.borrow_and_update().iter() {
                    if event.scope.canonical_root == root {
                        seen.insert(event.scope.server_id.clone(), server_state(event));
                    }
                }
                loop {
                    let proceed = {
                        let mut changed = pin!(receiver.changed());
                        let mut stopped = pin!(stop.stopped());
                        std::future::poll_fn(|cx| {
                            if stopped.as_mut().poll(cx).is_ready() {
                                return Poll::Ready(false);
                            }
                            changed.as_mut().poll(cx).map(|r| r.is_ok())
                        })
                        .await
                    };
                    if !proceed {
                        break;
                    }
                    let current: Vec<McpStatusEvent> = receiver.borrow_and_update().clone();
                    for event in current {
                        if event.scope.canonical_root != root {
                            continue;
                        }
                        let state = server_state(&event);
                        let server_id = event.scope.server_id.clone();
                        if seen.get(&server_id) == Some(&state) {
                            continue;
                        }
                        seen.insert(server_id.clone(), state);
                        inner.record_state(&project, &server_id, state);
                    }
                }
                Ok(())
            }
        });
        if started.is_err() {
            lock(&self.state).watched.remove(&root);
        }
    }

    fn record_state(&self, project: &ProjectId, server_id: &str, state: wire::McpServerState) {
        if let Some(settings) = lock(&self.state).settings.get_mut(project)
            && let Some(server) = settings
                .servers
                .iter_mut()
                .find(|s| s.server_id == server_id)
        {
            server.state = state;
        }
        self.publish(McpEvent::ServerStatusChanged {
            scope: wire::McpScope {
                project: project.clone(),
                server_id: server_id.to_owned(),
            },
            state,
        });
    }

    async fn execute(&self, work: Work) -> Done {
        let root = match canonical(work.root()) {
            Ok(root) => root,
            Err(code) => return Done::failed(code),
        };
        let writer = work.writes().then(|| {
            lock(&self.state)
                .writers
                .entry(root.clone())
                .or_default()
                .clone()
        });
        let _turn = match &writer {
            Some(writer) => Some(writer.lock().await),
            None => None,
        };
        let mut done = self.execute_in(&root, work).await;
        if let Some(settings) = &mut done.settings {
            self.fill_secret_states(&root, settings).await;
        }
        done.root = Some(root);
        done
    }

    /// Look up whether each keychain reference holds a value (never which).
    async fn fill_secret_states(&self, root: &Path, settings: &mut wire::McpSettings) {
        for server in &mut settings.servers {
            if server.secret_references.is_empty() {
                continue;
            }
            let scope = ApprovalScope {
                canonical_root: root.to_owned(),
                server_id: server.server_id.clone(),
            };
            server.secret_states = self
                .gui
                .secret_presence(scope, server.secret_references.clone())
                .await
                .into_iter()
                .map(|(name, presence)| (name, secret_state(presence)))
                .collect();
        }
    }

    async fn execute_in(&self, root: &Path, work: Work) -> Done {
        let gui = &self.gui;
        let scope = |server_id: String| ApprovalScope {
            canonical_root: root.to_owned(),
            server_id,
        };
        match work {
            Work::Refresh { project, .. } => {
                settled_view(&project, gui.settings(root.to_owned()).await)
            }
            Work::Save {
                project,
                revision,
                mutation,
                ..
            } => settled_view(
                &project,
                gui.save_override(root.to_owned(), revision, mutation).await,
            ),
            Work::SetEnabled {
                project,
                server_id,
                revision,
                enabled,
                ..
            } => {
                let current = match gui.settings(root.to_owned()).await {
                    Ok(view) => view,
                    Err(kind) => return Done::failed(code(kind)),
                };
                if current.config.revision != revision {
                    return Done::failed(wire::McpErrorCode::ConfigConflict);
                }
                let Some(mut definition) = current
                    .config
                    .servers
                    .get(&server_id)
                    .and_then(|entry| entry.definition.clone())
                else {
                    return Done::failed(wire::McpErrorCode::Configuration);
                };
                definition.enabled = enabled;
                settled_view(
                    &project,
                    gui.save_override(
                        root.to_owned(),
                        revision,
                        cfg::McpOverrideMutation::Upsert {
                            server_id,
                            definition,
                        },
                    )
                    .await,
                )
            }
            Work::Preview {
                project,
                server_id,
                revision,
                ..
            } => match gui.preview_approval(scope(server_id), revision).await {
                Ok(preview) => Done::succeeded(wire::McpOutcome::ApprovalPreview {
                    preview: Box::new(approval_preview(&project, &preview)),
                }),
                Err(kind) => Done::failed(code(kind)),
            },
            Work::Confirm {
                server_id,
                preview_id,
                revision,
                fingerprint,
                project,
                ..
            } => {
                let confirmation = ConfirmMcpApproval {
                    preview_id: tools::mcp::types::ApprovalPreviewId(preview_id.to_bytes()),
                    scope: scope(server_id),
                    expected_revision: revision,
                    expected_fingerprint: tools::mcp::types::ApprovalFingerprint(
                        fingerprint.to_bytes(),
                    ),
                };
                match gui.confirm_approval(confirmation).await {
                    Ok(()) => {
                        let mut done = Done::succeeded(wire::McpOutcome::ApprovalConfirmed);
                        done.settings = gui
                            .settings(root.to_owned())
                            .await
                            .ok()
                            .map(|view| settings_view(&project, &view));
                        done
                    }
                    Err(kind) => Done::failed(code(kind)),
                }
            }
            Work::Revoke {
                project, server_id, ..
            } => match gui.revoke_approval(scope(server_id)).await {
                Ok(()) => {
                    let mut done = Done::succeeded(wire::McpOutcome::ApprovalRevoked);
                    done.settings = gui
                        .settings(root.to_owned())
                        .await
                        .ok()
                        .map(|view| settings_view(&project, &view));
                    done
                }
                Err(kind) => Done::failed(code(kind)),
            },
            Work::Test {
                server_id,
                revision,
                cancel,
                ..
            } => match gui
                .test_connection(scope(server_id), revision, share(&cancel))
                .await
            {
                Ok(status) => Done::succeeded(wire::McpOutcome::Tested {
                    status: server_status(status),
                }),
                Err(kind) => Done::failed(code(kind)),
            },
            Work::SaveSecret {
                project,
                server_id,
                logical_name,
                value,
                revision,
                mutation,
                ..
            } => secret_saved(
                &project,
                gui.save_secret_report(SaveMcpSecret {
                    scope: scope(server_id),
                    logical_name,
                    value,
                    expected_revision: revision,
                    mutation,
                })
                .await,
            ),
            Work::DeleteSecret {
                project,
                server_id,
                logical_name,
                revision,
                mutation,
                ..
            } => match gui
                .delete_secret(DeleteMcpSecret {
                    scope: scope(server_id),
                    logical_name,
                    expected_revision: revision,
                    mutation,
                })
                .await
            {
                Ok(outcome) => {
                    let settings = settings_view(&project, &outcome.view);
                    let saved = wire::McpOutcome::Settings {
                        revision: settings.revision.clone(),
                    };
                    let state = match outcome.keychain_error {
                        // The configuration is saved; only the keychain step
                        // failed, so the entry may still hold its value.
                        Some(kind) => wire::McpOperationState::Partial {
                            outcome: saved,
                            keychain_error: code(kind),
                        },
                        None => wire::McpOperationState::Succeeded { outcome: saved },
                    };
                    Done {
                        state,
                        settings: Some(settings),
                        root: None,
                    }
                }
                Err(kind) => Done::failed(code(kind)),
            },
        }
    }
}

fn share(cancel: &Option<Arc<CancelContext>>) -> CancelContext {
    // `CancelContext` is cloneable and shares its signal with every clone.
    cancel.as_deref().cloned().unwrap_or_default()
}

fn canonical(root: &Path) -> Result<PathBuf, wire::McpErrorCode> {
    std::fs::canonicalize(root).map_err(|_| wire::McpErrorCode::Unavailable)
}

/// What an operation ended with, before it is recorded.
struct Done {
    state: wire::McpOperationState,
    settings: Option<wire::McpSettings>,
    root: Option<PathBuf>,
}

impl Done {
    fn succeeded(outcome: wire::McpOutcome) -> Self {
        Self {
            state: wire::McpOperationState::Succeeded { outcome },
            settings: None,
            root: None,
        }
    }

    fn failed(error: wire::McpErrorCode) -> Self {
        Self {
            state: if error == wire::McpErrorCode::Cancelled {
                wire::McpOperationState::Cancelled
            } else {
                wire::McpOperationState::Failed { error }
            },
            settings: None,
            root: None,
        }
    }
}

fn settled_view(project: &ProjectId, result: Result<McpSettingsView, McpErrorKind>) -> Done {
    match result {
        Ok(view) => {
            let settings = settings_view(project, &view);
            Done {
                state: wire::McpOperationState::Succeeded {
                    outcome: wire::McpOutcome::Settings {
                        revision: settings.revision.clone(),
                    },
                },
                settings: Some(settings),
                root: None,
            }
        }
        Err(kind) => Done::failed(code(kind)),
    }
}

/// The end of a `SaveSecret`. A failure is only reported as such when the
/// keychain is known to be as it was (or the write itself failed); once the
/// entry was written and not taken back, the settings did not save and the
/// keychain may differ: the outcome is unknown, never "nothing was written".
fn secret_saved(project: &ProjectId, report: SaveMcpSecretReport) -> Done {
    match report.result {
        Ok(view) => settled_view(project, Ok(view)),
        Err(kind) => Done::failed(match (report.write, report.rollback) {
            (McpSecretWrite::NotAttempted | McpSecretWrite::Failed(_), _)
            | (McpSecretWrite::Stored, McpSecretRollback::Deleted) => code(kind),
            _ => wire::McpErrorCode::OutcomeUnknown,
        }),
    }
}

fn secret_state(presence: McpSecretPresence) -> wire::McpSecretState {
    match presence {
        McpSecretPresence::Present => wire::McpSecretState::Present,
        McpSecretPresence::Missing => wire::McpSecretState::Missing,
        McpSecretPresence::Locked => wire::McpSecretState::Locked,
        McpSecretPresence::Unavailable => wire::McpSecretState::Unavailable,
        McpSecretPresence::Denied => wire::McpSecretState::Denied,
    }
}

/// A command ready to run: wire values already translated into the runtime's.
enum Work {
    Refresh {
        root: PathBuf,
        project: ProjectId,
    },
    Save {
        root: PathBuf,
        project: ProjectId,
        revision: cfg::SourceRevision,
        wire_revision: wire::McpRevision,
        mutation: cfg::McpOverrideMutation,
    },
    SetEnabled {
        root: PathBuf,
        project: ProjectId,
        server_id: String,
        revision: cfg::SourceRevision,
        wire_revision: wire::McpRevision,
        enabled: bool,
    },
    Preview {
        root: PathBuf,
        project: ProjectId,
        server_id: String,
        revision: cfg::SourceRevision,
        wire_revision: wire::McpRevision,
    },
    Confirm {
        root: PathBuf,
        project: ProjectId,
        server_id: String,
        preview_id: wire::McpPreviewId,
        revision: cfg::SourceRevision,
        wire_revision: wire::McpRevision,
        fingerprint: wire::McpFingerprint,
    },
    Revoke {
        root: PathBuf,
        project: ProjectId,
        server_id: String,
    },
    Test {
        root: PathBuf,
        server_id: String,
        revision: cfg::SourceRevision,
        wire_revision: wire::McpRevision,
        cancel: Option<Arc<CancelContext>>,
    },
    SaveSecret {
        root: PathBuf,
        project: ProjectId,
        server_id: String,
        logical_name: String,
        value: sandbox::Secret,
        revision: cfg::SourceRevision,
        wire_revision: wire::McpRevision,
        mutation: cfg::McpOverrideMutation,
    },
    DeleteSecret {
        root: PathBuf,
        project: ProjectId,
        server_id: String,
        logical_name: String,
        revision: cfg::SourceRevision,
        wire_revision: wire::McpRevision,
        mutation: cfg::McpOverrideMutation,
    },
}

fn revision_of(revision: &wire::McpRevision) -> cfg::SourceRevision {
    cfg::SourceRevision(revision.to_bytes())
}

impl Work {
    fn of(command: McpCommand, root: PathBuf, cancel: Option<Arc<CancelContext>>) -> Self {
        match command {
            McpCommand::Refresh(c) => Self::Refresh {
                root,
                project: c.project,
            },
            McpCommand::SaveOverride(c) => Self::Save {
                root,
                project: c.project,
                revision: revision_of(&c.expected_revision),
                wire_revision: c.expected_revision,
                mutation: config_mutation(c.mutation),
            },
            McpCommand::SetEnabled(c) => Self::SetEnabled {
                root,
                project: c.scope.project,
                server_id: c.scope.server_id,
                revision: revision_of(&c.expected_revision),
                wire_revision: c.expected_revision,
                enabled: c.enabled,
            },
            McpCommand::PreviewApproval(c) => Self::Preview {
                root,
                project: c.scope.project,
                server_id: c.scope.server_id,
                revision: revision_of(&c.expected_revision),
                wire_revision: c.expected_revision,
            },
            McpCommand::ConfirmApproval(c) => Self::Confirm {
                root,
                project: c.scope.project,
                server_id: c.scope.server_id,
                preview_id: c.preview_id,
                revision: revision_of(&c.expected_revision),
                wire_revision: c.expected_revision,
                fingerprint: c.expected_fingerprint,
            },
            McpCommand::RevokeApproval(c) => Self::Revoke {
                root,
                project: c.scope.project,
                server_id: c.scope.server_id,
            },
            McpCommand::TestConnection(c) => Self::Test {
                root,
                server_id: c.scope.server_id,
                revision: revision_of(&c.expected_revision),
                wire_revision: c.expected_revision,
                cancel,
            },
            McpCommand::SaveSecret(c) => Self::SaveSecret {
                root,
                project: c.scope.project,
                server_id: c.scope.server_id,
                logical_name: c.logical_name,
                value: sandbox::Secret::from(c.value.expose().to_owned()),
                revision: revision_of(&c.expected_revision),
                wire_revision: c.expected_revision,
                mutation: config_mutation(c.mutation),
            },
            McpCommand::DeleteSecret(c) => Self::DeleteSecret {
                root,
                project: c.scope.project,
                server_id: c.scope.server_id,
                logical_name: c.logical_name,
                revision: revision_of(&c.expected_revision),
                wire_revision: c.expected_revision,
                mutation: config_mutation(c.mutation),
            },
            // Answered before a work item is made.
            McpCommand::CancelOperation(_) => unreachable!("cancel is not a work item"),
        }
    }

    fn root(&self) -> &Path {
        match self {
            Self::Refresh { root, .. }
            | Self::Save { root, .. }
            | Self::SetEnabled { root, .. }
            | Self::Preview { root, .. }
            | Self::Confirm { root, .. }
            | Self::Revoke { root, .. }
            | Self::Test { root, .. }
            | Self::SaveSecret { root, .. }
            | Self::DeleteSecret { root, .. } => root,
        }
    }

    /// Whether the work edits the project's configuration or its secrets.
    fn writes(&self) -> bool {
        matches!(
            self,
            Self::Save { .. }
                | Self::SetEnabled { .. }
                | Self::SaveSecret { .. }
                | Self::DeleteSecret { .. }
        )
    }

    fn cancel(&self) -> Option<Arc<CancelContext>> {
        match self {
            Self::Test { cancel, .. } => cancel.clone(),
            _ => None,
        }
    }

    /// The revision the command quotes, if it quotes one.
    fn expected_revision(&self) -> Option<&wire::McpRevision> {
        match self {
            Self::Refresh { .. } | Self::Revoke { .. } => None,
            Self::Save { wire_revision, .. }
            | Self::SetEnabled { wire_revision, .. }
            | Self::Preview { wire_revision, .. }
            | Self::Confirm { wire_revision, .. }
            | Self::Test { wire_revision, .. }
            | Self::SaveSecret { wire_revision, .. }
            | Self::DeleteSecret { wire_revision, .. } => Some(wire_revision),
        }
    }
}

/// Project, kind and server of a command, for its operation view (also used by
/// the desktop to know what a request in flight stands for).
pub fn describe(
    command: &McpCommand,
) -> (Option<ProjectId>, wire::McpOperationKind, Option<String>) {
    use wire::McpOperationKind as K;
    match command {
        McpCommand::Refresh(c) => (Some(c.project.clone()), K::Refresh, None),
        McpCommand::SaveOverride(c) => (
            Some(c.project.clone()),
            K::SaveOverride,
            Some(c.mutation.server_id().to_owned()),
        ),
        McpCommand::SetEnabled(c) => (
            Some(c.scope.project.clone()),
            K::SetEnabled,
            Some(c.scope.server_id.clone()),
        ),
        McpCommand::PreviewApproval(c) => (
            Some(c.scope.project.clone()),
            K::PreviewApproval,
            Some(c.scope.server_id.clone()),
        ),
        McpCommand::ConfirmApproval(c) => (
            Some(c.scope.project.clone()),
            K::ConfirmApproval,
            Some(c.scope.server_id.clone()),
        ),
        McpCommand::RevokeApproval(c) => (
            Some(c.scope.project.clone()),
            K::RevokeApproval,
            Some(c.scope.server_id.clone()),
        ),
        McpCommand::TestConnection(c) => (
            Some(c.scope.project.clone()),
            K::TestConnection,
            Some(c.scope.server_id.clone()),
        ),
        McpCommand::SaveSecret(c) => (
            Some(c.scope.project.clone()),
            K::SaveSecret,
            Some(c.scope.server_id.clone()),
        ),
        McpCommand::DeleteSecret(c) => (
            Some(c.scope.project.clone()),
            K::DeleteSecret,
            Some(c.scope.server_id.clone()),
        ),
        // Never started as an operation of its own.
        McpCommand::CancelOperation(_) => (None, K::TestConnection, None),
    }
}

/// The project a command names, for the caller that resolves its directory.
pub fn project_of(command: &McpCommand) -> Option<&ProjectId> {
    match command {
        McpCommand::Refresh(c) => Some(&c.project),
        McpCommand::SaveOverride(c) => Some(&c.project),
        McpCommand::SetEnabled(c) => Some(&c.scope.project),
        McpCommand::PreviewApproval(c) => Some(&c.scope.project),
        McpCommand::ConfirmApproval(c) => Some(&c.scope.project),
        McpCommand::RevokeApproval(c) => Some(&c.scope.project),
        McpCommand::TestConnection(c) => Some(&c.scope.project),
        McpCommand::SaveSecret(c) => Some(&c.scope.project),
        McpCommand::DeleteSecret(c) => Some(&c.scope.project),
        McpCommand::CancelOperation(_) => None,
    }
}

/// A digest of the request that leaves the secret out: enough to tell a repeat
/// from another request, without keeping anything derived from a secret.
fn request_digest(command: &McpCommand) -> [u8; 32] {
    let mut scrubbed = command.clone();
    if let McpCommand::SaveSecret(save) = &mut scrubbed {
        save.value = wire::McpSecretValue::new("-");
    }
    let bytes = serde_json::to_vec(&scrubbed).unwrap_or_default();
    Sha256::digest(bytes).into()
}

// ---- wire <-> runtime translation ------------------------------------------

fn config_template(template: wire::McpTemplate) -> cfg::Template {
    cfg::Template(
        template
            .0
            .into_iter()
            .map(|part| match part {
                wire::McpTemplatePart::Literal { value } => cfg::TemplatePart::Literal { value },
                wire::McpTemplatePart::Env { name, default } => {
                    cfg::TemplatePart::Env { name, default }
                }
            })
            .collect(),
    )
}

fn config_source(source: wire::McpValueSource) -> cfg::ValueSource {
    match source {
        wire::McpValueSource::Template { template } => cfg::ValueSource::Template {
            template: config_template(template),
        },
        wire::McpValueSource::Keychain { name } => cfg::ValueSource::Keychain { name },
    }
}

/// The runtime's definition of a wire definition (also used by the desktop's form).
pub fn config_definition(definition: wire::McpServerDefinition) -> cfg::McpServerDefinition {
    let transport = match definition.transport {
        wire::McpTransport::Stdio {
            command,
            args,
            cwd,
            env,
        } => cfg::McpTransportDefinition::Stdio {
            command: config_template(command),
            args: args.into_iter().map(config_template).collect(),
            cwd: cwd.map(config_template),
            env: env
                .into_iter()
                .map(|(name, source)| (name, config_source(source)))
                .collect(),
        },
        wire::McpTransport::Http {
            url,
            headers,
            token,
        } => cfg::McpTransportDefinition::Http {
            url: config_template(url),
            headers: headers
                .into_iter()
                .map(|(name, source)| (name, config_source(source)))
                .collect(),
            token: token.map(config_source),
        },
    };
    cfg::McpServerDefinition {
        enabled: definition.enabled,
        roles: definition.roles,
        transport,
        startup_timeout_ms: definition.startup_timeout_ms,
        request_timeout_ms: definition.request_timeout_ms,
    }
}

fn config_mutation(mutation: wire::McpMutation) -> cfg::McpOverrideMutation {
    match mutation {
        wire::McpMutation::Upsert {
            server_id,
            definition,
        } => cfg::McpOverrideMutation::Upsert {
            server_id,
            definition: config_definition(definition),
        },
        wire::McpMutation::Disable { server_id } => cfg::McpOverrideMutation::Disable { server_id },
        wire::McpMutation::RemoveOverride { server_id } => {
            cfg::McpOverrideMutation::RemoveOverride { server_id }
        }
    }
}

fn wire_template(template: &cfg::Template) -> wire::McpTemplate {
    wire::McpTemplate(
        template
            .0
            .iter()
            .map(|part| match part {
                cfg::TemplatePart::Literal { value } => wire::McpTemplatePart::Literal {
                    value: value.clone(),
                },
                cfg::TemplatePart::Env { name, default } => wire::McpTemplatePart::Env {
                    name: name.clone(),
                    default: default.clone(),
                },
            })
            .collect(),
    )
}

fn wire_source(source: &cfg::ValueSource) -> wire::McpValueSource {
    match source {
        cfg::ValueSource::Template { template } => wire::McpValueSource::Template {
            template: wire_template(template),
        },
        cfg::ValueSource::Keychain { name } => {
            wire::McpValueSource::Keychain { name: name.clone() }
        }
    }
}

/// The wire form of a runtime definition (also used by the desktop's form).
pub fn wire_definition(definition: &cfg::McpServerDefinition) -> wire::McpServerDefinition {
    let transport = match &definition.transport {
        cfg::McpTransportDefinition::Stdio {
            command,
            args,
            cwd,
            env,
        } => wire::McpTransport::Stdio {
            command: wire_template(command),
            args: args.iter().map(wire_template).collect(),
            cwd: cwd.as_ref().map(wire_template),
            env: env
                .iter()
                .map(|(name, source)| (name.clone(), wire_source(source)))
                .collect(),
        },
        cfg::McpTransportDefinition::Http {
            url,
            headers,
            token,
        } => wire::McpTransport::Http {
            url: wire_template(url),
            headers: headers
                .iter()
                .map(|(name, source)| (name.clone(), wire_source(source)))
                .collect(),
            token: token.as_ref().map(wire_source),
        },
    };
    wire::McpServerDefinition {
        enabled: definition.enabled,
        roles: definition.roles.clone(),
        transport,
        startup_timeout_ms: definition.startup_timeout_ms,
        request_timeout_ms: definition.request_timeout_ms,
    }
}

fn origin(source: cfg::McpSource) -> wire::McpOrigin {
    match source {
        cfg::McpSource::ImportedJson => wire::McpOrigin::ImportedJson,
        cfg::McpSource::ProjectToml => wire::McpOrigin::ProjectToml,
    }
}

fn config_code(code: cfg::McpConfigError) -> wire::McpConfigCode {
    use wire::McpConfigCode as C;
    match code {
        cfg::McpConfigError::Unavailable => C::Unavailable,
        cfg::McpConfigError::Io => C::Io,
        cfg::McpConfigError::Syntax => C::Syntax,
        cfg::McpConfigError::Invalid => C::Invalid,
        cfg::McpConfigError::InvalidTemplate => C::InvalidTemplate,
        cfg::McpConfigError::MissingEnvironment => C::MissingEnvironment,
        cfg::McpConfigError::LimitExceeded => C::LimitExceeded,
        cfg::McpConfigError::Conflict => C::Conflict,
        cfg::McpConfigError::Symlink => C::Symlink,
    }
}

fn diagnostic(diagnostic: &cfg::McpConfigDiagnostic) -> wire::McpDiagnostic {
    wire::McpDiagnostic {
        server_id: diagnostic.server_id.clone(),
        source: origin(diagnostic.source),
        code: config_code(diagnostic.code),
    }
}

/// The classified error as the wire names it.
pub fn code(kind: McpErrorKind) -> wire::McpErrorCode {
    use wire::McpErrorCode as C;
    match kind {
        McpErrorKind::Timeout => C::Timeout,
        McpErrorKind::Transport => C::Transport,
        McpErrorKind::HttpStatus(status) => C::HttpStatus { status },
        McpErrorKind::Protocol => C::Protocol,
        McpErrorKind::ServerRejected => C::ServerRejected,
        McpErrorKind::ToolRejected => C::ToolRejected,
        McpErrorKind::Configuration => C::Configuration,
        McpErrorKind::ConfigConflict => C::ConfigConflict,
        McpErrorKind::NeedsApproval => C::NeedsApproval,
        McpErrorKind::ApprovalRevoked => C::ApprovalRevoked,
        McpErrorKind::RoleNotAllowed => C::RoleNotAllowed,
        McpErrorKind::StalePreview => C::StalePreview,
        McpErrorKind::SecretUnavailable => C::SecretUnavailable,
        McpErrorKind::SecretLocked => C::SecretLocked,
        McpErrorKind::SecretDenied => C::SecretDenied,
        McpErrorKind::SecretMissing => C::SecretMissing,
        McpErrorKind::MissingEnvironment(field) => C::MissingEnvironment {
            field: match field {
                runtime::mcp::McpValueField::Command => wire::McpField::Command,
                runtime::mcp::McpValueField::Args => wire::McpField::Args,
                runtime::mcp::McpValueField::Cwd => wire::McpField::Cwd,
                runtime::mcp::McpValueField::Env => wire::McpField::Env,
                runtime::mcp::McpValueField::Url => wire::McpField::Url,
                runtime::mcp::McpValueField::Headers => wire::McpField::Headers,
                runtime::mcp::McpValueField::Token => wire::McpField::Token,
            },
        },
        McpErrorKind::ProcessLimitReached => C::ProcessLimitReached,
        McpErrorKind::PreparationLimitReached => C::PreparationLimitReached,
        McpErrorKind::Unavailable => C::Unavailable,
        McpErrorKind::Cancelled => C::Cancelled,
        McpErrorKind::OutcomeUnknown => C::OutcomeUnknown,
        McpErrorKind::InputRequired => C::InputRequired,
        McpErrorKind::LimitExceeded => C::LimitExceeded,
        McpErrorKind::DefinitionChanged => C::DefinitionChanged,
    }
}

fn server_status(status: runtime::mcp::McpServerStatus) -> wire::McpServerStatus {
    use runtime::mcp::McpServerStatus as S;
    match status {
        S::Disabled => wire::McpServerStatus::Disabled,
        S::Invalid => wire::McpServerStatus::Invalid,
        S::NeedsApproval => wire::McpServerStatus::NeedsApproval,
        S::SecretUnavailable => wire::McpServerStatus::SecretUnavailable,
        S::Connecting => wire::McpServerStatus::Connecting,
        S::Ready => wire::McpServerStatus::Ready,
        S::Unavailable => wire::McpServerStatus::Unavailable,
        S::ProcessLimitReached => wire::McpServerStatus::ProcessLimitReached,
    }
}

fn server_state(event: &McpStatusEvent) -> wire::McpServerState {
    wire::McpServerState {
        status: server_status(event.status),
        reason: event.reason.map(code),
    }
}

/// A project's settings and server states as the wire carries them.
pub fn settings_view(project: &ProjectId, view: &McpSettingsView) -> wire::McpSettings {
    let config = &view.config;
    let servers = config
        .servers
        .iter()
        .map(|(id, entry)| {
            let definition = entry.definition.as_ref().map(wire_definition);
            let state = view
                .statuses
                .iter()
                .find(|event| event.scope.server_id == *id)
                .map(server_state)
                .unwrap_or(wire::McpServerState {
                    status: wire::McpServerStatus::Unavailable,
                    reason: None,
                });
            wire::McpServerView {
                server_id: id.clone(),
                origin: origin(entry.source),
                imported_exists: entry.imported_exists,
                tombstone: entry.tombstone,
                secret_references: definition
                    .as_ref()
                    .map(wire::McpServerDefinition::keychain_references)
                    .unwrap_or_default(),
                secret_states: Default::default(),
                definition,
                diagnostics: entry.diagnostics.iter().map(diagnostic).collect(),
                state,
            }
        })
        .collect();
    wire::McpSettings {
        project: project.clone(),
        revision: wire::McpRevision::from_bytes(&config.revision.0),
        preparation_blocked: config.preparation_blocked,
        diagnostics: config.diagnostics.iter().map(diagnostic).collect(),
        servers,
    }
}

fn summary(summary: &tools::mcp::types::McpApprovalSummary) -> wire::McpApprovalSummary {
    wire::McpApprovalSummary {
        executable: summary
            .executable
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        canonical_cwd: summary
            .canonical_cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        destination: summary.destination.as_ref().map(|d| wire::McpDestination {
            scheme: d.scheme.clone(),
            host: d.host.clone(),
            port: d.port,
        }),
        environment_keys: summary.environment_keys.clone(),
        secret_references: summary.secret_references.clone(),
    }
}

fn approval_preview(
    project: &ProjectId,
    preview: &tools::mcp::types::McpApprovalPreview,
) -> wire::McpApprovalPreview {
    use tools::mcp::types::ApprovalWarning as W;
    wire::McpApprovalPreview {
        preview_id: wire::McpPreviewId::from_bytes(&preview.id.0),
        scope: wire::McpScope {
            project: project.clone(),
            server_id: preview.scope.server_id.clone(),
        },
        source_revision: wire::McpRevision::from_bytes(&preview.source_revision.0),
        fingerprint: wire::McpFingerprint::from_bytes(&preview.fingerprint.0),
        warnings: preview
            .warnings
            .iter()
            .map(|warning| match warning {
                W::HostProcessOutsideSandbox => wire::McpApprovalWarning::HostProcessOutsideSandbox,
                W::PrivateDestination => wire::McpApprovalWarning::PrivateDestination,
                W::PlaintextHttp => wire::McpApprovalWarning::PlaintextHttp,
                W::SendsSecretReferences => wire::McpApprovalWarning::SendsSecretReferences,
            })
            .collect(),
        previous_fingerprint: preview
            .previous_fingerprint
            .map(|f| wire::McpFingerprint::from_bytes(&f.0)),
        previous_summary: preview.previous_summary.as_ref().map(summary),
        summary: summary(&preview.summary),
        approved_ips: preview
            .approved_ips
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(write: McpSecretWrite, rollback: McpSecretRollback) -> wire::McpOperationState {
        let report = SaveMcpSecretReport {
            result: Err(McpErrorKind::ConfigConflict),
            write,
            rollback,
        };
        secret_saved(&ProjectId::new("p").unwrap(), report).state
    }

    fn error(code: wire::McpErrorCode) -> wire::McpOperationState {
        wire::McpOperationState::Failed { error: code }
    }

    #[test]
    fn a_secret_save_is_a_plain_failure_only_when_the_keychain_is_known_to_be_as_before() {
        use McpSecretRollback as R;
        use McpSecretWrite as W;
        let conflict = wire::McpErrorCode::ConfigConflict;
        assert_eq!(failed(W::NotAttempted, R::NotNeeded), error(conflict));
        assert_eq!(
            failed(W::Failed(McpErrorKind::SecretLocked), R::NotNeeded),
            error(conflict),
            "the failure code is the classified one of the request"
        );
        assert_eq!(failed(W::Stored, R::Deleted), error(conflict));
    }

    #[test]
    fn a_secret_save_that_may_have_changed_the_keychain_is_never_reported_as_nothing_written() {
        use McpSecretRollback as R;
        use McpSecretWrite as W;
        let unknown = error(wire::McpErrorCode::OutcomeUnknown);
        assert_eq!(failed(W::Stored, R::Kept), unknown);
        assert_eq!(
            failed(W::Stored, R::Failed(McpErrorKind::SecretDenied)),
            unknown
        );
        assert_eq!(failed(W::Stored, R::NotNeeded), unknown);
        assert_eq!(failed(W::Unknown, R::Unknown), unknown);
    }
}
