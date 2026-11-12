//! Workspace namespace: projects, threads, conversation families, trust and
//! the directories a project or thread may reach (ADR 0045, PR 09).
//!
//! The core owns the shared sidebar. Every mutation names the revision of the
//! entity it was decided on (`expected_revision`) and is answered with a typed
//! [`WorkspaceResult`]: either [`WorkspaceResult::Applied`] with the new
//! revision, or [`WorkspaceResult::Rejected`] with a [`WorkspaceRejection`] that
//! says why nothing changed. A rejection is an answer, not a transport error.
//!
//! What stays out of this namespace on purpose: the selected window, scroll
//! position, panel layout and unsent drafts are client state; conversation
//! creation and continuation belong to the `conversation` namespace.
//!
//! Revisions are equality tokens read from the matching query:
//! * a project's or a thread's revision is a digest of its stored record, so a
//!   write by any other writer (another client or a legacy desktop) changes it;
//! * a family's archive and pin revisions are the counters the family state keeps.
use crate::{
    commands::ThreadTarget,
    envelope::Actor,
    handshake::{Capability, WireLimits},
    ids::{ProjectId, RunId, ThreadId},
    limits::{MAX_NAME_BYTES, MAX_TITLE_BYTES},
    queries::{ListThreads, PageRequest},
    validation::{ValidationError, check_all, check_bytes, check_count, check_line},
    views::{Page, ProjectSummary, ThreadSummary, ThreadView},
    wire::Revision,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "register_project",
    "rename_project",
    "set_project_trust",
    "update_allowed_directory",
    "rename_thread",
    "set_family_archived",
    "set_family_pinned",
    "set_thread_extra_read_dirs",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "list_projects",
    "list_threads",
    "get_thread",
    "get_project_access",
    "list_families",
    "get_thread_detail",
    "get_retention",
];
pub const EVENT_TYPES: &[&str] = &["thread_changed", "workspace_changed"];
/// Tagged choices inside a command's payload that classification names.
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[super::NestedOperation {
    operation: "update_allowed_directory",
    pointer: "/action/type",
    known: ALLOWED_DIRECTORY_ACTIONS,
}];
/// `update_allowed_directory`'s `action.type` values.
pub const ALLOWED_DIRECTORY_ACTIONS: &[&str] = &["add", "remove"];

/// Longest path a command or view carries.
pub const MAX_PATH_BYTES: usize = 4096;
/// Most extra read directories one thread may declare.
pub const MAX_EXTRA_READ_DIRS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Approved,
    Unapproved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterProject {
    pub name: String,
    /// An absolute directory; the core canonicalises it.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameProject {
    pub project: ProjectId,
    pub name: String,
    pub expected_revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetProjectTrust {
    pub project: ProjectId,
    pub trust: TrustLevel,
    pub expected_revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AllowedDirectoryAction {
    Add { path: String, trust: TrustLevel },
    Remove { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAllowedDirectory {
    pub project: ProjectId,
    pub action: AllowedDirectoryAction,
    pub expected_revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameThread {
    pub thread: ThreadId,
    pub title: String,
    pub expected_revision: Revision,
}

/// Archive or restore a whole conversation family. The family is the unit:
/// `thread` may be any member and the state is kept once, for the family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFamilyArchived {
    pub thread: ThreadId,
    pub archived: bool,
    /// The family's `archive_revision` as listed by `list_families`.
    pub expected_revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFamilyPinned {
    pub thread: ThreadId,
    pub pinned: bool,
    /// The family's `pin_revision` as listed by `list_families`.
    pub expected_revision: Revision,
}

/// Replace the read-only directories a thread's runs may read (never write).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetThreadExtraReadDirs {
    pub thread: ThreadId,
    pub dirs: Vec<String>,
    pub expected_revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkspaceCommand {
    RegisterProject(RegisterProject),
    RenameProject(RenameProject),
    SetProjectTrust(SetProjectTrust),
    UpdateAllowedDirectory(UpdateAllowedDirectory),
    RenameThread(RenameThread),
    SetFamilyArchived(SetFamilyArchived),
    SetFamilyPinned(SetFamilyPinned),
    SetThreadExtraReadDirs(SetThreadExtraReadDirs),
}

impl WorkspaceCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    /// Whether the command changes what a run may read or load. Only a human
    /// actor may issue it.
    pub fn grants_access(&self) -> bool {
        matches!(
            self,
            Self::SetProjectTrust(_)
                | Self::UpdateAllowedDirectory(_)
                | Self::SetThreadExtraReadDirs(_)
                | Self::RegisterProject(_)
        )
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::RegisterProject(c) => {
                check_line("name", &c.name, MAX_NAME_BYTES, false)?;
                check_line("path", &c.path, MAX_PATH_BYTES, false)
            }
            Self::RenameProject(c) => check_line("name", &c.name, MAX_NAME_BYTES, false),
            Self::SetProjectTrust(_) | Self::SetFamilyArchived(_) | Self::SetFamilyPinned(_) => {
                Ok(())
            }
            Self::UpdateAllowedDirectory(c) => match &c.action {
                AllowedDirectoryAction::Add { path, .. }
                | AllowedDirectoryAction::Remove { path } => {
                    check_line("path", path, MAX_PATH_BYTES, false)
                }
            },
            Self::RenameThread(c) => check_line("title", &c.title, MAX_TITLE_BYTES, false),
            Self::SetThreadExtraReadDirs(c) => {
                check_count("dirs", c.dirs.len(), MAX_EXTRA_READ_DIRS)?;
                check_all(&c.dirs, |d| check_line("dirs", d, MAX_PATH_BYTES, false))
            }
        }
    }
}

/// Why a mutation changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkspaceRejection {
    /// The entity changed since `expected`; read it again and decide again.
    Conflict {
        expected: Revision,
        actual: Revision,
    },
    /// The thread was deleted by retention. A deleted thread never returns,
    /// whatever any client or a saved copy still says.
    Tombstoned,
    /// The entity does not exist.
    NotFound,
    /// A path was refused: not absolute, not a directory, too broad, nested,
    /// protected (the core's own configuration or database) or a symlink loop.
    InvalidPath { reason: String },
    /// The command needs a human actor.
    PermissionDenied { reason: String },
    /// The name or value is not acceptable (empty, duplicate, ...).
    Invalid { reason: String },
    /// Another process holds the shared sidebar; resend the same request.
    Busy,
    /// The shared sidebar could not be read, merged or written. Nothing was
    /// written, and the file is left as it was.
    StorageFailed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applied {
    /// The revision of the entity after the command. For a family command the
    /// matching counter (archive or pin revision).
    pub revision: Revision,
    /// False when the entity already held the requested value.
    pub changed: bool,
    /// The project a registration created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkspaceResult {
    Applied(Applied),
    Rejected(WorkspaceRejection),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectTarget {
    pub project: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkspaceQuery {
    ListProjects(PageRequest),
    ListThreads(ListThreads),
    GetThread(ThreadTarget),
    GetProjectAccess(ProjectTarget),
    ListFamilies(PageRequest),
    GetThreadDetail(ThreadTarget),
    GetRetention(PageRequest),
}

impl WorkspaceQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListProjects(page) | Self::ListFamilies(page) | Self::GetRetention(page) => {
                page.validate()
            }
            Self::ListThreads(q) => q.page.validate(),
            Self::GetThread(_) | Self::GetProjectAccess(_) | Self::GetThreadDetail(_) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedDirectoryView {
    pub path: String,
    pub trust: TrustLevel,
    /// The directory exists now and is a real permission. A stored declaration
    /// whose directory is gone stays listed with `live: false` and grants nothing.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAccessView {
    pub project: ProjectId,
    pub name: String,
    pub root: String,
    pub trust: TrustLevel,
    pub allowed_directories: Vec<AllowedDirectoryView>,
    /// What `rename_project`, `set_project_trust` and `update_allowed_directory` quote.
    pub revision: Revision,
}

/// Why a thread's deletion is held back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RetentionBlock {
    Pinned,
    EvalSample,
    /// Another thread of the family is not due yet.
    FamilyMemberNotDue {
        thread: String,
    },
    FamilyMemberBlocked {
        thread: String,
    },
    FamilyUnresolved {
        thread: String,
    },
    /// A running or recently running conversation holds the thread.
    Owner {
        reason: String,
    },
    /// The database and the sidebar are not the pair they were bound as, so
    /// nothing is deleted.
    BindingMismatch,
    /// The search for what depends on the thread is not finished.
    Exploring,
    OverBudget,
    /// A committed recovery manifest names the thread as a member, or a
    /// prepared claim holds it; a family it belongs to is kept.
    Recovery {
        reason: String,
    },
}

/// Where a thread stands in the retention of archived threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RetentionState {
    NotArchived,
    /// Deleting archived threads is switched off.
    Disabled,
    /// Whole days left, rounded up.
    DueIn {
        days: u32,
    },
    DueNextStartup,
    Blocked {
        block: RetentionBlock,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionEntry {
    pub thread: ThreadId,
    pub state: RetentionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyView {
    /// The family's top ancestor.
    pub key: ThreadId,
    pub members: u32,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    pub archive_revision: Revision,
    pub pinned: bool,
    pub pin_revision: Revision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageType {
    Fork,
    Rewind,
    Escalation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadDetail {
    pub thread: ThreadId,
    pub project: ProjectId,
    pub title: String,
    /// The family the thread belongs to, as the ledger and lineage resolve it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<LineageType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_role: Option<String>,
    pub extra_read_dirs: Vec<AllowedDirectoryView>,
    pub archived: bool,
    pub pinned: bool,
    /// What `rename_thread` and `set_thread_extra_read_dirs` quote.
    pub revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkspaceReply {
    Projects(Page<ProjectSummary>),
    Threads(Page<ThreadSummary>),
    Thread(ThreadView),
    ProjectAccess(Box<ProjectAccessView>),
    Families(Page<FamilyView>),
    Retention(Page<RetentionEntry>),
    ThreadDetail(Box<ThreadDetail>),
}

impl WorkspaceReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Projects(page) => page.validate_with(ProjectSummary::validate),
            Self::Threads(page) => page.validate_with(ThreadSummary::validate),
            Self::Thread(view) => view.validate(),
            Self::ProjectAccess(view) => {
                check_bytes("root", &view.root, MAX_PATH_BYTES, true)?;
                check_count("allowed_directories", view.allowed_directories.len(), 256)
            }
            Self::Families(page) => page.validate(),
            Self::Retention(page) => page.validate(),
            Self::ThreadDetail(detail) => {
                check_count(
                    "extra_read_dirs",
                    detail.extra_read_dirs.len(),
                    MAX_EXTRA_READ_DIRS,
                )?;
                check_bytes("title", &detail.title, MAX_TITLE_BYTES, true)
            }
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
pub enum WorkspaceEvent {
    ThreadChanged {
        thread: ThreadSummary,
    },
    /// The shared sidebar changed (a project, a thread record, a family state or
    /// trust). Clients read what they show again. `revision` only tells two
    /// changes apart.
    WorkspaceChanged {
        revision: Revision,
    },
}

impl WorkspaceEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ThreadChanged { thread } => thread.validate(),
            Self::WorkspaceChanged { .. } => Ok(()),
        }
    }
}

/// Whether `actor` may issue a command that grants access.
pub fn actor_may_grant(actor: Actor) -> bool {
    actor == Actor::Human
}
