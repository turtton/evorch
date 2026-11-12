//! Workspace namespace: projects and threads, read-only.
use crate::{
    commands::ThreadTarget,
    handshake::{Capability, WireLimits},
    queries::{ListThreads, PageRequest},
    validation::ValidationError,
    views::{Page, ProjectSummary, ThreadSummary, ThreadView},
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[];
pub const QUERY_OPERATIONS: &[&str] = &["list_projects", "list_threads", "get_thread"];
pub const EVENT_TYPES: &[&str] = &["thread_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

no_commands!(WorkspaceCommand);
no_results!(WorkspaceResult);

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
}

impl WorkspaceQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListProjects(page) => page.validate(),
            Self::ListThreads(q) => q.page.validate(),
            Self::GetThread(_) => Ok(()),
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
pub enum WorkspaceReply {
    Projects(Page<ProjectSummary>),
    Threads(Page<ThreadSummary>),
    Thread(ThreadView),
}

impl WorkspaceReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Projects(page) => page.validate_with(ProjectSummary::validate),
            Self::Threads(page) => page.validate_with(ThreadSummary::validate),
            Self::Thread(view) => view.validate(),
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
    ThreadChanged { thread: ThreadSummary },
}

impl WorkspaceEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ThreadChanged { thread } => thread.validate(),
        }
    }
}
