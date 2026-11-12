//! Teams namespace: the read models of a thread's team board (ADR 0045). The
//! board's configuration is edited elsewhere; nothing here changes it.
use crate::{
    handshake::{Capability, WireLimits},
    ids::{RunId, ThreadId},
    limits::{MAX_NAME_BYTES, MAX_REFERENCE_BYTES},
    queries::ThreadQuery,
    validation::{ValidationError, check_bytes, check_count},
    views::Page,
    wire::Revision,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[];
pub const QUERY_OPERATIONS: &[&str] = &["get_team", "list_team_tasks", "get_team_task"];
pub const EVENT_TYPES: &[&str] = &["team_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most artifact paths one [`TeamTaskView`] lists.
pub const MAX_TEAM_TASK_PATHS: usize = 32;
/// Most members one [`TeamOverview`] lists.
pub const MAX_TEAM_MEMBERS: usize = 64;

no_commands!(TeamsCommand);
no_results!(TeamsResult);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamTaskState {
    Ready,
    Claimed,
    Complete,
}

/// One task of the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamTaskView {
    pub task_id: String,
    pub state: TeamTaskState,
    /// The artifact paths the task claims.
    pub paths: Vec<String>,
    /// Who holds the claim, as the board names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The run behind `owner`, when the board names a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_run: Option<RunId>,
    /// Which claim of the task this is; 0 before the first.
    pub claim_generation: Revision,
    pub revision: Revision,
}

impl TeamTaskView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("task_id", &self.task_id, MAX_NAME_BYTES, false)?;
        check_count("paths", self.paths.len(), MAX_TEAM_TASK_PATHS)?;
        for path in &self.paths {
            check_bytes("path", path, MAX_REFERENCE_BYTES, true)?;
        }
        if let Some(owner) = &self.owner {
            check_bytes("owner", owner, MAX_NAME_BYTES, true)?;
        }
        Ok(())
    }
}

/// Who works on the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamMember {
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    pub claimed_tasks: u32,
}

/// Where the board was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamSource {
    /// The coordinator's run still holds the board.
    Live,
    /// Only the persisted snapshot remains.
    Persisted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOverview {
    pub thread: ThreadId,
    pub source: TeamSource,
    /// The coordinator run of a live board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_run: Option<RunId>,
    pub ready: u32,
    pub claimed: u32,
    pub complete: u32,
    pub members: Vec<TeamMember>,
    pub revision: Revision,
}

impl TeamOverview {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("members", self.members.len(), MAX_TEAM_MEMBERS)?;
        for member in &self.members {
            check_bytes("owner", &member.owner, MAX_NAME_BYTES, true)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamTaskRef {
    pub thread: ThreadId,
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TeamsQuery {
    GetTeam(crate::commands::ThreadTarget),
    ListTeamTasks(ThreadQuery),
    GetTeamTask(TeamTaskRef),
}

impl TeamsQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetTeam(_) => Ok(()),
            Self::ListTeamTasks(q) => q.page.validate(),
            Self::GetTeamTask(task) => check_bytes("task_id", &task.task_id, MAX_NAME_BYTES, false),
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
pub enum TeamsReply {
    Team(TeamOverview),
    Tasks(Page<TeamTaskView>),
    Task(TeamTaskView),
}

impl TeamsReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Team(team) => team.validate(),
            Self::Tasks(page) => page.validate_with(TeamTaskView::validate),
            Self::Task(task) => task.validate(),
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
pub enum TeamsEvent {
    /// The board of a thread changed.
    TeamChanged { team: TeamOverview },
}

impl TeamsEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::TeamChanged { team } => team.validate(),
        }
    }
}
