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
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Directories that hold the core's own configuration and databases.
    protected: Vec<PathBuf>,
    source: Option<ProjectionSource>,
    tombstones: Box<Tombstones>,
    pager: Pager,
    changes: AtomicU64,
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
                protected: protected.iter().map(|p| canonical(p)).collect(),
                source,
                tombstones: Box::new(move || Ok(Arc::new(deleted.clone()))),
                pager: Pager::default(),
                changes: AtomicU64::new(0),
            }),
        }
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
    fn deleted(&self) -> Result<Deleted, WorkspaceRejection> {
        (self.tombstones)().map_err(|reason| WorkspaceRejection::StorageFailed { reason })
    }

    fn read(&self) -> Result<SidebarDocument, String> {
        let mut document = self.store.read_unlocked().map_err(|e| e.to_string())?;
        let deleted = (self.tombstones)()?;
        document.prune_threads(&*deleted);
        Ok(document)
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
                let current = document.sidebar.resolve_states();
                let actual = current.get(&id).map_or(0, |s| s.archive_revision);
                check_expected(c.expected_revision, Revision(actual))?;
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
                let current = document.sidebar.resolve_states();
                let actual = current.get(&id).map_or(0, |s| s.pin_revision);
                check_expected(c.expected_revision, Revision(actual))?;
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

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}
