//! Host service of the `workspace` area: the core is the writer of the shared
//! sidebar (`sidebar.json`), of project trust and of the directories a project
//! or thread may reach (ADR 0045, PR 09).
//!
//! Every mutation runs in one order: the command is validated, the lock of the
//! shared file is taken (the one desktops that still write the file take too),
//! the file is **re-read under the lock**, tombstoned threads are pruned from
//! what was read, the expected revision is compared with the entity as read,
//! the edit is applied, the file is written atomically, and only then are the
//! live runtime declarations, the event and the projection updated. A failure
//! at any step leaves the file as it was.
//!
//! Interim state (until PR 19a): desktops that still hold a sidebar in memory
//! merge the file with their own three-way merge, so this writer and a legacy
//! desktop coexist through the file lock only. The write of the file itself is
//! not yet a database table.
//!
//! Handlers run under the host's mutex and never wait: a held lock is answered
//! with [`WorkspaceRejection::Busy`] at once.
use super::AreaHandles;
use crate::areas::ProjectionSource;
use crate::bootstrap::BootstrapError;
use crate::queries::{Pager, revision};
use runtime::AgentRuntime;
use runtime::ownership::Registry;
use runtime::thread_retention::{
    BindingPaths, StartupPaths, ThreadRetention, check_binding, startup_input,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use storage::thread_retention::{BlockReason, RetentionStatus, SidebarThread};
use workbench_protocol::commands::{CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{ProjectId as WireProject, ThreadId as WireThread};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{WorkspaceEvent, WorkspaceReply};
use workbench_protocol::queries::PageRequest;
use workbench_protocol::views::Page;
use workbench_protocol::wire::Revision;
use workspace_state::{
    LineageKind, ProjectError, SidebarDocument, SidebarStore, StoreError, ThreadChatRole,
    ThreadError, ThreadId, ThreadRecord, TrustState,
};

type Deleted = Arc<dyn Fn(&str) -> bool + Send + Sync>;
type Tombstones = dyn Fn() -> Result<Deleted, String> + Send + Sync;

struct Service {
    store: SidebarStore,
    runtime: Option<AgentRuntime>,
    retention: Option<RetentionSource>,
    /// Where the archive and pin state of a family is projected into the
    /// `threads` table. The sidebar is the source; the projection only follows.
    mirror: Option<storage::StorageHandle>,
    /// Directories that hold the core's own configuration and databases.
    protected: Vec<PathBuf>,
    source: Option<ProjectionSource>,
    tombstones: Box<Tombstones>,
    pager: Pager,
    changes: AtomicU64,
    /// Threads the core already registered (or found) in the sidebar, so that
    /// a follow-up message does not take the file lock again.
    registered: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

/// A conversation the core created, to be listed in the sidebar.
pub struct NewThread<'a> {
    pub project: &'a WireProject,
    pub thread: &'a WireThread,
    /// The first line of what the user said; empty keeps a placeholder title.
    pub title_hint: &'a str,
    /// The root directory the project directory knows, for a project that the
    /// sidebar does not list yet.
    pub project_root: Option<PathBuf>,
    pub orchestrator: bool,
    /// The run and message count a fork starts after.
    pub fork: Option<(String, u64)>,
}

/// The `workspace` service, reached from `ingress/namespaces/workspace.rs` as
/// `self.areas.workspace`. Unwired (`Default`) it answers nothing.
#[derive(Default)]
pub struct WorkspaceArea {
    inner: Option<Service>,
}

fn unwired() -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::StorageUnavailable,
        "the workspace service is not wired in this host",
        RetryAdvice::Never,
    )
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn to_trust(trust: TrustLevel) -> TrustState {
    match trust {
        TrustLevel::Approved => TrustState::Approved,
        TrustLevel::Unapproved => TrustState::Unapproved,
    }
}

fn from_trust(trust: TrustState) -> TrustLevel {
    match trust {
        TrustState::Approved => TrustLevel::Approved,
        TrustState::Unapproved => TrustLevel::Unapproved,
    }
}

fn project_revision(project: &workspace_state::ProjectRecord) -> Revision {
    revision(&(
        &project.name,
        &project.repo_root,
        &project.trust,
        &project.allowed_directories,
    ))
}

fn thread_revision(thread: &ThreadRecord) -> Revision {
    revision(&(&thread.title, &thread.extra_read_dirs))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn project_rejection(error: ProjectError) -> WorkspaceRejection {
    match error {
        ProjectError::UnknownProject | ProjectError::UnknownAllowedDirectory => {
            WorkspaceRejection::NotFound
        }
        ProjectError::DuplicateProject
        | ProjectError::DuplicateAllowedDirectory
        | ProjectError::EmptyName => WorkspaceRejection::Invalid {
            reason: error.to_string(),
        },
        other => WorkspaceRejection::InvalidPath {
            reason: other.to_string(),
        },
    }
}

fn thread_rejection(error: ThreadError) -> WorkspaceRejection {
    match error {
        ThreadError::UnknownThread | ThreadError::UnknownProject => WorkspaceRejection::NotFound,
        ThreadError::InvalidReadDirectory(reason) => WorkspaceRejection::InvalidPath { reason },
        other => WorkspaceRejection::Invalid {
            reason: other.to_string(),
        },
    }
}

fn store_rejection(error: StoreError) -> WorkspaceRejection {
    match error {
        StoreError::Busy => WorkspaceRejection::Busy,
        other => WorkspaceRejection::StorageFailed {
            reason: other.to_string(),
        },
    }
}

struct Done {
    applied: Applied,
    /// What a run may read or load changed: the runtime declarations are redone.
    access: bool,
}

fn check_expected(expected: Revision, actual: Revision) -> Result<(), WorkspaceRejection> {
    if expected == actual {
        Ok(())
    } else {
        Err(WorkspaceRejection::Conflict { expected, actual })
    }
}

impl WorkspaceArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let runtime = handles.runtime.clone();
        let reader = runtime.clone();
        let source = handles.source("workspace").ok();
        Ok(Self {
            inner: Some(Service {
                store: SidebarStore::new(handles.config_dir.join("sidebar.json")),
                runtime: Some(runtime),
                mirror: Some(handles.storage.clone()),
                retention: Some(RetentionSource {
                    ownership_root: handles.ownership_root.clone(),
                    sidebar_path: handles.config_dir.join("sidebar.json"),
                    policy: runtime::thread_retention::policy_from_config(&handles.config.storage),
                }),
                protected: vec![canonical(&handles.config_dir)],
                source,
                tombstones: Box::new(move || {
                    let set = reader
                        .with_storage_reader(|db| db.tombstones())
                        .map_err(|error| error.to_string())?
                        .unwrap_or_default();
                    Ok(Arc::new(move |id: &str| set.contains_thread(id)))
                }),
                pager: Pager::default(),
                changes: AtomicU64::new(0),
                registered: Default::default(),
            }),
        })
    }

    /// A service over an explicit sidebar file, for hosts and tests that have
    /// no bootstrap. `deleted` names the tombstoned threads.
    pub fn with_store(
        store: SidebarStore,
        runtime: Option<AgentRuntime>,
        protected: Vec<PathBuf>,
        source: Option<ProjectionSource>,
        deleted: impl Fn(&str) -> bool + Send + Sync + Clone + 'static,
    ) -> Self {
        Self {
            inner: Some(Service {
                store,
                runtime,
                retention: None,
                mirror: None,
                protected: protected.iter().map(|p| canonical(p)).collect(),
                source,
                tombstones: Box::new(move || Ok(Arc::new(deleted.clone()))),
                pager: Pager::default(),
                changes: AtomicU64::new(0),
                registered: Default::default(),
            }),
        }
    }

    /// Lists a conversation the core just created in the sidebar and the
    /// `threads` table, under the same lock and file as every other sidebar
    /// write. A thread that is listed already is left alone, so a follow-up
    /// message, or a desktop that listed it itself, changes nothing.
    ///
    /// # Errors
    /// The sidebar could not be locked, read or written.
    pub fn register_thread(&self, new: &NewThread<'_>) -> Result<(), WorkbenchError> {
        let Some(service) = self.inner.as_ref() else {
            return Ok(());
        };
        service
            .register_thread(new)
            .map_err(|reason| internal(&reason))
    }

    /// The projects of the sidebar: id, root directory and the name saved.
    ///
    /// # Errors
    /// The sidebar could not be read.
    pub fn project_entries(&self) -> Result<Vec<(WireProject, PathBuf, String)>, WorkbenchError> {
        let service = self.inner.as_ref().ok_or_else(unwired)?;
        let document = service.read().map_err(|e| internal(&e))?;
        Ok(document
            .sidebar
            .projects
            .iter()
            .filter_map(|p| {
                Some((
                    WireProject::new(p.id.to_string()).ok()?,
                    p.repo_root.clone(),
                    p.name.clone(),
                ))
            })
            .collect())
    }

    /// Projects the archive and pin state of the family of every thread into
    /// `handle` (the `threads` table).
    #[must_use]
    pub fn with_mirror(mut self, handle: storage::StorageHandle) -> Self {
        if let Some(service) = self.inner.as_mut() {
            service.mirror = Some(handle);
        }
        self
    }

    /// Declares trust, allowed directories and read-only directories of the
    /// saved sidebar to the runtime, as a core does at start. A saved
    /// directory that no longer exists stays declared in the sidebar and is
    /// not made a live permission (the runtime skips it).
    pub fn declare_saved(&self) -> Result<(), WorkbenchError> {
        let service = self.inner.as_ref().ok_or_else(unwired)?;
        let document = service
            .store
            .read_unlocked()
            .map_err(|e| internal(&e.to_string()))?;
        service.declare(&document);
        Ok(())
    }

    pub fn command(
        &self,
        command: WorkspaceCommand,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let service = self.inner.as_ref().ok_or_else(unwired)?;
        let result = match service.run(&command, provenance) {
            Ok(done) => {
                service.publish(&done);
                WorkspaceResult::Applied(done.applied)
            }
            Err(rejection) => WorkspaceResult::Rejected(rejection),
        };
        Ok(CommandReply::with_result(CommandResult::Workspace(result)))
    }

    pub fn query(
        &self,
        query: &workbench_protocol::namespaces::WorkspaceQuery,
    ) -> Result<WorkspaceReply, WorkbenchError> {
        use workbench_protocol::namespaces::WorkspaceQuery as Q;
        let service = self.inner.as_ref().ok_or_else(unwired)?;
        let document = service.read().map_err(|e| internal(&e))?;
        match query {
            Q::GetProjectAccess(target) => service
                .access_view(&document, &target.project)
                .map(|view| WorkspaceReply::ProjectAccess(Box::new(view))),
            Q::ListFamilies(page) => service
                .families(&document, page)
                .map(WorkspaceReply::Families),
            Q::GetRetention(page) => service
                .retention(&document, page)
                .map(WorkspaceReply::Retention),
            Q::GetThreadDetail(target) => service
                .detail(&document, &target.thread)
                .map(|detail| WorkspaceReply::ThreadDetail(Box::new(detail))),
            Q::ListProjects(_) | Q::ListThreads(_) | Q::GetThread(_) => Err(WorkbenchError::new(
                ErrorCode::InvalidRequest,
                "answered by the projection",
                RetryAdvice::Never,
            )),
        }
    }
}

fn internal(text: &str) -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::StorageUnavailable,
        format!("workspace: {text}"),
        RetryAdvice::SameRequest,
    )
}

fn not_found(text: &str) -> WorkbenchError {
    WorkbenchError::new(ErrorCode::NotFound, text, RetryAdvice::Never)
}

impl Service {
    fn register_thread(&self, new: &NewThread<'_>) -> Result<(), String> {
        let id = ThreadId::new(new.thread.as_str());
        if self
            .registered
            .lock()
            .unwrap()
            .contains(new.thread.as_str())
        {
            return Ok(());
        }
        let deleted = (self.tombstones)()?;
        if deleted(new.thread.as_str()) {
            return Ok(());
        }
        let lock = self.store.lock().map_err(|e| e.to_string())?;
        let mut document = self.store.read(&lock).map_err(|e| e.to_string())?;
        document.prune_threads(&*deleted);
        if document.sidebar.threads.iter().any(|t| t.id == id) {
            self.registered
                .lock()
                .unwrap()
                .insert(new.thread.as_str().to_owned());
            return Ok(());
        }
        let project = workspace_state::ProjectId::new(new.project.as_str());
        if !document.sidebar.projects.iter().any(|p| p.id == project) {
            let root = new
                .project_root
                .as_ref()
                .ok_or_else(|| "the project is not in the sidebar".to_owned())?;
            let name = root.file_name().map_or_else(
                || "project".to_owned(),
                |n| n.to_string_lossy().into_owned(),
            );
            document
                .sidebar
                .add_project(project.clone(), &name, root)
                .map_err(|e| e.to_string())?;
        }
        let title = new.title_hint.lines().next().unwrap_or("").trim();
        let title: String = title.chars().take(80).collect();
        document
            .sidebar
            .create_thread(
                id.clone(),
                project,
                if title.is_empty() {
                    "New conversation"
                } else {
                    &title
                },
            )
            .map_err(|e| e.to_string())?;
        let parent = new.fork.as_ref().and_then(|(run, _)| {
            self.runtime
                .as_ref()?
                .with_storage_reader(|db| db.thread_of_run(run))
                .ok()
                .flatten()
                .and_then(|row| row.map(|row| row.thread_id))
        });
        let now = now_seconds();
        if let Some(record) = document.sidebar.threads.iter_mut().find(|t| t.id == id) {
            record.title_is_placeholder = title.is_empty();
            record.created_at = now;
            record.chat_role = Some(if new.orchestrator {
                ThreadChatRole::Orchestrator
            } else {
                ThreadChatRole::Worker
            });
            if let (Some(parent), Some((run, context_len))) = (parent, &new.fork) {
                record.parent_thread_id = Some(ThreadId::new(parent));
                record.lineage = Some(workspace_state::ThreadLineage {
                    kind: LineageKind::Fork,
                    point: Some(workspace_state::ForkPoint {
                        run_id: run.clone(),
                        context_len: *context_len,
                    }),
                });
            }
        }
        document.sidebar.refresh_derived();
        self.store
            .write(&lock, &document)
            .map_err(|e| e.to_string())?;
        drop(lock);
        self.mirror_family(&document, new.thread);
        self.registered
            .lock()
            .unwrap()
            .insert(new.thread.as_str().to_owned());
        self.publish(&Done {
            applied: Applied {
                revision: Revision(0),
                changed: true,
                project: None,
            },
            access: false,
        });
        Ok(())
    }

    fn deleted(&self) -> Result<Deleted, WorkspaceRejection> {
        (self.tombstones)().map_err(|reason| WorkspaceRejection::StorageFailed { reason })
    }

    fn read(&self) -> Result<SidebarDocument, String> {
        let mut document = self.store.read_unlocked().map_err(|e| e.to_string())?;
        let deleted = (self.tombstones)()?;
        document.prune_threads(&*deleted);
        self.repair_membership(&mut document.sidebar)?;
        Ok(document)
    }

    /// Applies the lane ledger to `sidebar` before a family is resolved: the
    /// membership of every lane and the revision floors of the `threads`
    /// table (the ledger is authoritative, a file written before a lane
    /// changed is repaired). A ledger that cannot be read is an error, never a
    /// guess: nothing is edited on top of an unrepaired family.
    fn repair_membership(&self, sidebar: &mut workspace_state::SidebarState) -> Result<(), String> {
        let Some(mirror) = &self.mirror else {
            return Ok(());
        };
        let snapshot = mirror
            .sidebar_projection_snapshot(sidebar.threads.iter().map(|t| t.id.to_string()).collect())
            .map_err(|error| format!("the lane ledger could not be read: {error}"))?;
        for lane in &snapshot.lanes {
            sidebar
                .set_lane_link(&ThreadId::new(&lane.thread_id), lane_link(lane))
                .map_err(|error| format!("the lane ledger could not be applied: {error}"))?;
        }
        // Obsolete membership states go first: an old epoch may carry much
        // higher revisions, but must not mask the reset of a new one.
        sidebar.refresh_derived();
        sidebar.raise_family_revisions(
            &|id| {
                snapshot
                    .revision_floors
                    .get(&id.to_string())
                    .copied()
                    .unwrap_or_default()
            },
            false,
        );
        sidebar.refresh_derived();
        Ok(())
    }

    fn check_family(
        sidebar: &workspace_state::SidebarState,
        id: &ThreadId,
        expected: &FamilyIdentity,
    ) -> Result<workspace_state::lane_management::FamilyRevision, WorkspaceRejection> {
        let current = sidebar.lane_family_revision(id).map_err(|error| {
            WorkspaceRejection::FamilyChanged {
                reason: error.to_string(),
            }
        })?;
        if current.key.to_string() != expected.key.as_str()
            || current.membership_epoch != expected.membership_epoch
        {
            return Err(WorkspaceRejection::FamilyChanged {
                reason: "the family membership changed; read the families again".into(),
            });
        }
        Ok(current)
    }

    fn protected(&self, path: &Path) -> Result<(), WorkspaceRejection> {
        let path = canonical(path);
        for guard in &self.protected {
            if path.starts_with(guard) || guard.starts_with(&path) {
                return Err(WorkspaceRejection::InvalidPath {
                    reason: "the path holds or contains the core's configuration or database"
                        .into(),
                });
            }
        }
        Ok(())
    }

    fn run(
        &self,
        command: &WorkspaceCommand,
        provenance: &Provenance,
    ) -> Result<Done, WorkspaceRejection> {
        if command.grants_access()
            && !workbench_protocol::namespaces::workspace::actor_may_grant(provenance.actor)
        {
            return Err(WorkspaceRejection::PermissionDenied {
                reason: "only a human actor may change trust or reachable directories".into(),
            });
        }
        let deleted = self.deleted()?;
        let lock = self.store.lock().map_err(store_rejection)?;
        let mut document = self.store.read(&lock).map_err(store_rejection)?;
        document.prune_threads(&*deleted);
        let before = document.sidebar.clone();
        if matches!(
            command,
            WorkspaceCommand::SetFamilyArchived(_) | WorkspaceCommand::SetFamilyPinned(_)
        ) {
            self.repair_membership(&mut document.sidebar)
                .map_err(|reason| WorkspaceRejection::StorageFailed { reason })?;
        }
        let done = self.apply(command, &mut document, &*deleted)?;
        if document.sidebar != before {
            self.store
                .write(&lock, &document)
                .map_err(store_rejection)?;
        }
        drop(lock);
        if done.access {
            self.declare(&document);
        }
        if done.applied.changed
            && let WorkspaceCommand::SetFamilyArchived(SetFamilyArchived { thread, .. })
            | WorkspaceCommand::SetFamilyPinned(SetFamilyPinned { thread, .. })
            | WorkspaceCommand::RenameThread(RenameThread { thread, .. }) = command
        {
            self.mirror_family(&document, thread);
        }
        Ok(done)
    }

    fn thread_mut<'a>(
        document: &'a mut SidebarDocument,
        thread: &WireThread,
        deleted: &dyn Fn(&str) -> bool,
    ) -> Result<&'a mut ThreadRecord, WorkspaceRejection> {
        if deleted(thread.as_str()) {
            return Err(WorkspaceRejection::Tombstoned);
        }
        document
            .sidebar
            .threads
            .iter_mut()
            .find(|t| t.id.to_string() == thread.as_str())
            .ok_or(WorkspaceRejection::NotFound)
    }

    fn apply(
        &self,
        command: &WorkspaceCommand,
        document: &mut SidebarDocument,
        deleted: &dyn Fn(&str) -> bool,
    ) -> Result<Done, WorkspaceRejection> {
        let applied = |revision, changed| Applied {
            revision,
            changed,
            project: None,
        };
        match command {
            WorkspaceCommand::RegisterProject(c) => {
                let path = Path::new(&c.path);
                if !path.is_absolute() {
                    return Err(WorkspaceRejection::InvalidPath {
                        reason: ProjectError::NotAbsolute.to_string(),
                    });
                }
                self.protected(path)?;
                let root = path
                    .canonicalize()
                    .map_err(|_| WorkspaceRejection::InvalidPath {
                        reason: ProjectError::NotADirectory.to_string(),
                    })?;
                let id = workspace_state::ProjectId::new(format!(
                    "project-{:016x}",
                    revision(&path_text(&root)).0
                ));
                document
                    .sidebar
                    .add_project(id.clone(), c.name.trim(), &root)
                    .map_err(project_rejection)?;
                let project = document
                    .sidebar
                    .projects
                    .iter()
                    .find(|p| p.id == id)
                    .ok_or(WorkspaceRejection::NotFound)?;
                Ok(Done {
                    applied: Applied {
                        project: WireProject::new(id.to_string()).ok(),
                        ..applied(project_revision(project), true)
                    },
                    access: true,
                })
            }
            WorkspaceCommand::RenameProject(c) => {
                let id = workspace_state::ProjectId::new(c.project.as_str());
                let project = Self::project(document, &id)?;
                check_expected(c.expected_revision, project_revision(project))?;
                let before = project.name.clone();
                document
                    .sidebar
                    .rename_project(&id, &c.name)
                    .map_err(project_rejection)?;
                let project = Self::project(document, &id)?;
                Ok(Done {
                    applied: applied(project_revision(project), project.name != before),
                    access: false,
                })
            }
            WorkspaceCommand::SetProjectTrust(c) => {
                let id = workspace_state::ProjectId::new(c.project.as_str());
                let project = Self::project(document, &id)?;
                check_expected(c.expected_revision, project_revision(project))?;
                let changed = project.trust != to_trust(c.trust);
                document
                    .sidebar
                    .set_project_trust(&id, to_trust(c.trust))
                    .map_err(project_rejection)?;
                Ok(Done {
                    applied: applied(project_revision(Self::project(document, &id)?), changed),
                    access: true,
                })
            }
            WorkspaceCommand::UpdateAllowedDirectory(c) => {
                let id = workspace_state::ProjectId::new(c.project.as_str());
                let project = Self::project(document, &id)?;
                check_expected(c.expected_revision, project_revision(project))?;
                let before = project.allowed_directories.clone();
                match &c.action {
                    AllowedDirectoryAction::Add { path, trust } => {
                        self.protected(Path::new(path))?;
                        document
                            .sidebar
                            .add_allowed_directory(&id, Path::new(path), to_trust(*trust))
                            .map_err(project_rejection)?;
                    }
                    AllowedDirectoryAction::Remove { path } => document
                        .sidebar
                        .remove_allowed_directory(&id, Path::new(path))
                        .map_err(project_rejection)?,
                }
                let project = Self::project(document, &id)?;
                Ok(Done {
                    applied: applied(
                        project_revision(project),
                        project.allowed_directories != before,
                    ),
                    access: true,
                })
            }
            WorkspaceCommand::RenameThread(c) => {
                let title = c.title.trim().to_owned();
                if title.is_empty() {
                    return Err(WorkspaceRejection::Invalid {
                        reason: "the title must not be empty".into(),
                    });
                }
                let thread = Self::thread_mut(document, &c.thread, deleted)?;
                check_expected(c.expected_revision, thread_revision(thread))?;
                let changed = thread.title != title;
                thread.title = title;
                thread.title_is_placeholder = false;
                Ok(Done {
                    applied: applied(thread_revision(thread), changed),
                    access: false,
                })
            }
            WorkspaceCommand::SetFamilyArchived(c) => {
                let id = Self::family_member(document, &c.thread, deleted)?;
                let current = Self::check_family(&document.sidebar, &id, &c.expected_family)?;
                check_expected(c.expected_revision, Revision(current.archive_revision))?;
                let changed = document
                    .sidebar
                    .set_family_archived(&id, c.archived, now_seconds())
                    .map_err(thread_rejection)?;
                let revision = document
                    .sidebar
                    .resolve_states()
                    .get(&id)
                    .map_or(0, |s| s.archive_revision);
                Ok(Done {
                    applied: applied(Revision(revision), changed),
                    access: false,
                })
            }
            WorkspaceCommand::SetFamilyPinned(c) => {
                let id = Self::family_member(document, &c.thread, deleted)?;
                let current = Self::check_family(&document.sidebar, &id, &c.expected_family)?;
                check_expected(c.expected_revision, Revision(current.pin_revision))?;
                let changed = document
                    .sidebar
                    .set_family_pinned(&id, c.pinned)
                    .map_err(thread_rejection)?;
                let revision = document
                    .sidebar
                    .resolve_states()
                    .get(&id)
                    .map_or(0, |s| s.pin_revision);
                Ok(Done {
                    applied: applied(Revision(revision), changed),
                    access: false,
                })
            }
            WorkspaceCommand::SetThreadExtraReadDirs(c) => {
                let id = {
                    let thread = Self::thread_mut(document, &c.thread, deleted)?;
                    check_expected(c.expected_revision, thread_revision(thread))?;
                    thread.id.clone()
                };
                for dir in &c.dirs {
                    self.protected(Path::new(dir))?;
                }
                let before = document
                    .sidebar
                    .threads
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.extra_read_dirs.clone())
                    .unwrap_or_default();
                let dirs: Vec<PathBuf> = c.dirs.iter().map(PathBuf::from).collect();
                document
                    .sidebar
                    .set_thread_extra_read_dirs(&id, &dirs)
                    .map_err(thread_rejection)?;
                let thread = Self::thread_mut(document, &c.thread, deleted)?;
                Ok(Done {
                    applied: applied(thread_revision(thread), thread.extra_read_dirs != before),
                    access: true,
                })
            }
        }
    }

    fn project<'a>(
        document: &'a SidebarDocument,
        id: &workspace_state::ProjectId,
    ) -> Result<&'a workspace_state::ProjectRecord, WorkspaceRejection> {
        document
            .sidebar
            .projects
            .iter()
            .find(|p| &p.id == id)
            .ok_or(WorkspaceRejection::NotFound)
    }

    fn family_member(
        document: &SidebarDocument,
        thread: &WireThread,
        deleted: &dyn Fn(&str) -> bool,
    ) -> Result<ThreadId, WorkspaceRejection> {
        if deleted(thread.as_str()) {
            return Err(WorkspaceRejection::Tombstoned);
        }
        let id = ThreadId::new(thread.as_str());
        if !document.sidebar.threads.iter().any(|t| t.id == id) {
            return Err(WorkspaceRejection::NotFound);
        }
        Ok(id)
    }

    /// Redeclares what runs may read or load to the live runtime. Trust and
    /// directories apply to runs started afterwards (the runtime keeps the
    /// context a running run began with), and a saved directory that does not
    /// exist is not declared.
    fn declare(&self, document: &SidebarDocument) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        for project in &document.sidebar.projects {
            runtime.set_project_trust(
                project.repo_root.clone(),
                match project.trust {
                    TrustState::Approved => runtime::ProjectTrust::Approved,
                    TrustState::Unapproved => runtime::ProjectTrust::Unapproved,
                },
            );
            let approved: Vec<PathBuf> = project
                .allowed_directories
                .iter()
                .filter(|d| d.trust == TrustState::Approved && d.path.is_absolute())
                .map(|d| d.path.clone())
                .collect();
            runtime.set_project_allowed_directories(project.repo_root.clone(), approved);
        }
        for thread in &document.sidebar.threads {
            let root = document
                .sidebar
                .projects
                .iter()
                .find(|p| p.id == thread.project_id)
                .map(|p| canonical(&p.repo_root));
            let dirs: Vec<PathBuf> = thread
                .extra_read_dirs
                .iter()
                .filter(|d| {
                    self.protected(d).is_ok() && !root.as_ref().is_some_and(|r| d.starts_with(r))
                })
                .cloned()
                .collect();
            runtime.set_thread_read_only_directories(&thread.id.to_string(), dirs);
        }
    }

    fn publish(&self, _done: &Done) {
        let Some(source) = &self.source else {
            return;
        };
        let revision = Revision(self.changes.fetch_add(1, Ordering::Relaxed) + 1);
        if let Err(error) = source.publish(UiEvent::Workspace(WorkspaceEvent::WorkspaceChanged {
            revision,
        })) {
            tracing::warn!(?error.code, "workspace change event was not published");
        }
    }

    fn access_view(
        &self,
        document: &SidebarDocument,
        project: &WireProject,
    ) -> Result<ProjectAccessView, WorkbenchError> {
        let id = workspace_state::ProjectId::new(project.as_str());
        let record = Self::project(document, &id).map_err(|_| not_found("unknown project"))?;
        Ok(ProjectAccessView {
            project: project.clone(),
            name: record.name.clone(),
            root: path_text(&record.repo_root),
            trust: from_trust(record.trust),
            allowed_directories: record
                .allowed_directories
                .iter()
                .map(|d| AllowedDirectoryView {
                    path: path_text(&d.path),
                    trust: from_trust(d.trust),
                    live: d.trust == TrustState::Approved && d.path.is_dir(),
                })
                .collect(),
            revision: project_revision(record),
        })
    }

    fn families(
        &self,
        document: &SidebarDocument,
        page: &PageRequest,
    ) -> Result<Page<FamilyView>, WorkbenchError> {
        let states = document.sidebar.resolve_states();
        let mut families: BTreeMap<String, FamilyView> = BTreeMap::new();
        for thread in &document.sidebar.threads {
            let Some(state) = states.get(&thread.id) else {
                continue;
            };
            let Ok(key) = &state.family else {
                continue;
            };
            let entry = families
                .entry(key.to_string())
                .or_insert_with(|| FamilyView {
                    key: WireThread::new(key.to_string()).expect("a stored thread id is valid"),
                    membership_epoch: document
                        .sidebar
                        .lane_family_revision(&thread.id)
                        .map_or(0, |revision| revision.membership_epoch),
                    members: 0,
                    archived: state.archived,
                    archived_at: state.archived_at,
                    archive_revision: Revision(state.archive_revision),
                    pinned: state.pinned,
                    pin_revision: Revision(state.pin_revision),
                });
            entry.members += 1;
        }
        let items: Vec<FamilyView> = families.into_values().collect();
        self.pager
            .page("workspace/list_families", &items, page, revision(&items))
    }

    fn detail(
        &self,
        document: &SidebarDocument,
        thread: &WireThread,
    ) -> Result<ThreadDetail, WorkbenchError> {
        let id = ThreadId::new(thread.as_str());
        let record = document
            .sidebar
            .threads
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| not_found("unknown thread"))?;
        let states = document.sidebar.resolve_states();
        let state = states.get(&id);
        let wire_thread = |id: &ThreadId| WireThread::new(id.to_string()).ok();
        Ok(ThreadDetail {
            thread: thread.clone(),
            project: WireProject::new(record.project_id.to_string())
                .map_err(|_| internal("invalid stored project id"))?,
            title: record.title.clone(),
            family: state
                .and_then(|s| s.family.as_ref().ok())
                .and_then(wire_thread),
            parent_thread: record.parent_thread_id.as_ref().and_then(wire_thread),
            lineage: if record.escalation_source_run_id.is_some() {
                Some(LineageType::Escalation)
            } else {
                record.lineage.as_ref().map(|l| match l.kind {
                    LineageKind::Fork => LineageType::Fork,
                    LineageKind::Rewind => LineageType::Rewind,
                })
            },
            root_run: record
                .root_run_id
                .as_deref()
                .and_then(|r| workbench_protocol::ids::RunId::new(r).ok()),
            active_root: record.active_root.as_deref().map(path_text),
            model_profile: record.model_preference.as_ref().map(|m| m.profile.clone()),
            chat_role: record.chat_role.map(|role| match role {
                ThreadChatRole::Worker => "worker".to_owned(),
                ThreadChatRole::Orchestrator => "orchestrator".to_owned(),
            }),
            extra_read_dirs: record
                .extra_read_dirs
                .iter()
                .map(|d| AllowedDirectoryView {
                    path: path_text(d),
                    trust: TrustLevel::Approved,
                    live: d.is_dir() && self.protected(d).is_ok(),
                })
                .collect(),
            archived: state.is_some_and(|s| s.archived),
            pinned: state.is_some_and(|s| s.pinned),
            revision: thread_revision(record),
        })
    }
}

fn lane_link(record: &storage::lanes::LaneRecord) -> workspace_state::LaneLink {
    use event_bus::lane::LanePolicy as Ledger;
    workspace_state::LaneLink {
        lane_id: record.lane_id.clone(),
        origin_thread_id: ThreadId::new(record.origin_thread_id.clone()),
        lead_thread_id: record.lead_thread_id.clone().map(ThreadId::new),
        policy: match record.policy {
            Ledger::Attached => workspace_state::LanePolicy::Attached,
            Ledger::Summary => workspace_state::LanePolicy::Summary,
            Ledger::Detached => workspace_state::LanePolicy::Detached,
        },
        management_revision: record.management_revision,
        detached_since_management_revision: record.detached_since_management_revision,
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// What the retention view reads besides the sidebar: where the database and
/// the sidebar are paired, and how long an archived thread is kept.
#[derive(Debug, Clone)]
pub struct RetentionSource {
    pub ownership_root: PathBuf,
    pub sidebar_path: PathBuf,
    pub policy: storage::thread_retention::ThreadRetentionPolicy,
}

/// The sidebar's threads as the retention phase reads them, with the archive
/// and pin state their family resolves to. A thread whose family cannot be
/// found is told as not archived: nothing is guessed about it.
pub fn sidebar_threads(sidebar: &workspace_state::SidebarState) -> Vec<SidebarThread> {
    let states = sidebar.resolve_states();
    sidebar
        .threads
        .iter()
        .map(|thread| {
            let state = states.get(&thread.id).filter(|state| state.is_resolved());
            SidebarThread {
                thread_id: thread.id.to_string(),
                archived: state.is_some_and(|state| state.archived),
                archived_at_s: state.and_then(|state| state.archived_at),
                pinned: state.is_some_and(|state| state.pinned),
                updated_at_s: thread.last_activity(),
                created_at_s: thread.created_at,
                archive_revision: state.map(|state| state.archive_revision),
                pin_revision: state.map(|state| state.pin_revision),
            }
        })
        .collect()
}

const RECOVERY_CLAIM_PENDING: &str = "recovery claim pending";

fn retention_block(block: BlockReason) -> RetentionBlock {
    match block {
        BlockReason::Pinned => RetentionBlock::Pinned,
        BlockReason::EvalSample => RetentionBlock::EvalSample,
        BlockReason::FamilyMemberNotDue(thread) => RetentionBlock::FamilyMemberNotDue { thread },
        BlockReason::FamilyMemberBlocked(thread) => RetentionBlock::FamilyMemberBlocked { thread },
        BlockReason::FamilyUnresolved(thread) => RetentionBlock::FamilyUnresolved { thread },
        BlockReason::Owner(reason)
            if reason == runtime::thread_retention::BINDING_MISMATCH_BLOCK =>
        {
            RetentionBlock::BindingMismatch
        }
        BlockReason::Owner(reason) if reason == RECOVERY_CLAIM_PENDING => {
            RetentionBlock::Recovery { reason }
        }
        BlockReason::Owner(reason) => RetentionBlock::Owner { reason },
        BlockReason::Exploring => RetentionBlock::Exploring,
        BlockReason::OverBudget => RetentionBlock::OverBudget,
    }
}

fn retention_state(status: RetentionStatus) -> RetentionState {
    match status {
        RetentionStatus::NotArchived => RetentionState::NotArchived,
        RetentionStatus::Disabled => RetentionState::Disabled,
        RetentionStatus::DueIn { days } => RetentionState::DueIn { days },
        RetentionStatus::DueNextStartup => RetentionState::DueNextStartup,
        RetentionStatus::Blocked(block) => RetentionState::Blocked {
            block: retention_block(block),
        },
    }
}

/// The retention status of every thread of `document`, computed the way the
/// start-up phase will decide (a read only computation: nothing is confirmed
/// or deleted here). The deciding is not tied to any client: a client that is
/// closed, or none at all, changes nothing about it.
///
/// Recovery is taken into account by the storage side: a family whose run is a
/// member of a committed, not superseded recovery manifest is not confirmed,
/// and one with a prepared recovery claim reads as blocked.
///
/// # Errors
/// A message when the database, the ownership registry or the binding cannot
/// be read.
pub fn retention_entries(
    database: &storage::Database,
    document: &SidebarDocument,
    source: &RetentionSource,
    now_ns: i64,
) -> Result<Vec<RetentionEntry>, String> {
    let error = |error: &dyn std::fmt::Display| error.to_string();
    let paths = StartupPaths {
        // The input is built from the sidebar and ownership paths only; the
        // database is the handle already open.
        db_path: PathBuf::new(),
        ownership_root: source.ownership_root.clone(),
        sidebar_path: source.sidebar_path.clone(),
        artifacts_dir: None,
    };
    let input =
        startup_input(&paths, sidebar_threads(&document.sidebar), now_ns).map_err(|e| error(&e))?;
    let binding = check_binding(
        database,
        BindingPaths {
            ownership_root: &source.ownership_root,
            sidebar_path: &source.sidebar_path,
        },
    )
    .map_err(|e| error(&e))?;
    // No registry yet means no owner holds anything.
    let registry = Registry::open_readonly(&source.ownership_root.join("owners.db")).ok();
    let statuses = ThreadRetention::schedule(
        database,
        &input,
        &source.policy,
        &binding,
        registry.as_ref(),
    )
    .map_err(|e| error(&e))?;
    let mut entries = Vec::new();
    for (thread, status) in statuses {
        if let Ok(thread) = WireThread::new(thread) {
            entries.push(RetentionEntry {
                thread,
                state: retention_state(status),
            });
        }
    }
    entries.sort_by(|a, b| a.thread.as_str().cmp(b.thread.as_str()));
    Ok(entries)
}

fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

impl Service {
    fn retention(
        &self,
        document: &SidebarDocument,
        page: &PageRequest,
    ) -> Result<Page<RetentionEntry>, WorkbenchError> {
        let (Some(source), Some(runtime)) = (&self.retention, &self.runtime) else {
            return Err(unwired());
        };
        let entries = runtime
            .with_storage_reader(|database| {
                Ok(retention_entries(database, document, source, now_nanos()))
            })
            .map_err(|e| internal(&e.to_string()))?
            .ok_or_else(|| internal("the database is not open"))?
            .map_err(|e| internal(&e))?;
        self.pager.page(
            "workspace/get_retention",
            &entries,
            page,
            revision(&entries),
        )
    }
}

impl Service {
    /// Projects the family state of `thread` into the `threads` table for
    /// every member. The family state of the sidebar stays the source: a
    /// failure here leaves the file as it was written and is only logged, and
    /// the next archive or pin of the family projects it again.
    fn mirror_family(&self, document: &SidebarDocument, thread: &WireThread) {
        let Some(mirror) = &self.mirror else {
            return;
        };
        let states = document.sidebar.resolve_states();
        let id = ThreadId::new(thread.as_str());
        let Some(family) = states.get(&id).and_then(|s| s.family.as_ref().ok()) else {
            return;
        };
        let upserts: Vec<storage::threads::ThreadUpsert> = document
            .sidebar
            .threads
            .iter()
            .filter(|t| {
                states
                    .get(&t.id)
                    .and_then(|s| s.family.as_ref().ok())
                    .is_some_and(|key| key == family)
            })
            .filter_map(|t| {
                let state = states.get(&t.id).filter(|s| s.is_resolved())?;
                let root = document
                    .sidebar
                    .projects
                    .iter()
                    .find(|p| p.id == t.project_id)
                    .and_then(|p| p.repo_root.canonicalize().ok())
                    .map(|p| path_text(&p));
                Some(storage::threads::ThreadUpsert {
                    thread_id: t.id.to_string(),
                    project_id: Some(t.project_id.to_string()),
                    project_root: root,
                    title: Some(t.title.clone()),
                    archived: Some(state.archived),
                    archived_at_ns: state.archived_at.map(|at| at.saturating_mul(1_000_000_000)),
                    pinned: Some(state.pinned),
                    archive_revision: state.archive_revision,
                    pin_revision: state.pin_revision,
                    created_at_ns: t.created_at.saturating_mul(1_000_000_000),
                    updated_at_ns: t.last_activity().saturating_mul(1_000_000_000),
                    ..storage::threads::ThreadUpsert::default()
                })
            })
            .collect();
        if let Err(error) = mirror.upsert_threads_host(&upserts) {
            tracing::warn!(%error, "the family state was saved but not projected into the threads table");
        }
    }
}
