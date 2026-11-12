//! Read-only requests and their answers.

use crate::commands::CommandReply;
use crate::errors::WorkbenchError;
use crate::events::{EventCursor, SnapshotItem};
use crate::handshake::{Capability, WireLimits};
use crate::ids::{
    ApprovalId, BodyId, PageCursor, ProjectId, RequestId, RunId, SnapshotId, ThreadId,
};
use crate::limits::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};
use crate::namespaces::{
    approvals::{ApprovalsQuery, ApprovalsReply},
    artifacts::{ArtifactsQuery, ArtifactsReply},
    auth::{AuthQuery, AuthReply},
    browser::{BrowserQuery, BrowserReply},
    conversation::{ConversationQuery, ConversationReply},
    diagnostics::{DiagnosticsQuery, DiagnosticsReply},
    evaluation::{EvaluationQuery, EvaluationReply},
    execution::{ExecutionQuery, ExecutionReply},
    improvement::{ImprovementQuery, ImprovementReply},
    lanes::{LanesQuery, LanesReply},
    lifecycle::{LifecycleQuery, LifecycleReply},
    maintenance::{MaintenanceQuery, MaintenanceReply},
    mcp::{McpQuery, McpReply},
    memory::{MemoryQuery, MemoryReply},
    projection::{ProjectionQuery, ProjectionReply},
    pull_requests::{PullRequestsQuery, PullRequestsReply},
    questions::{QuestionsQuery, QuestionsReply},
    settings::{SettingsQuery, SettingsReply},
    teams::{TeamsQuery, TeamsReply},
    workspace::{WorkspaceQuery, WorkspaceReply},
};
use crate::validation::ValidationError;
use crate::views::Page;
use crate::wire::{ByteOffset, InstanceEpoch, Revision};
use serde::{Deserialize, Serialize};

fn default_limit() -> u32 {
    DEFAULT_PAGE_SIZE
}

/// Which page of a listing to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    /// From a previous page of the same query; absent for the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<PageCursor>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: DEFAULT_PAGE_SIZE,
        }
    }
}

impl PageRequest {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if !(1..=MAX_PAGE_SIZE).contains(&self.limit) {
            return Err(ValidationError::OutOfRange {
                field: "limit",
                reason: "must be 1..=200",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListThreads {
    pub project: ProjectId,
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadQuery {
    pub thread: ThreadId,
    #[serde(default)]
    pub page: PageRequest,
}

/// Questions or approvals, for one thread or for all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadId>,
    /// Only items still waiting for a decision.
    #[serde(default)]
    pub pending_only: bool,
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadCommittedHistory {
    /// Backward starts at the tail; each page remains chronological and its
    /// next cursor addresses older messages. The direction binds the cursor.
    #[serde(default)]
    pub direction: HistoryDirection,
    pub thread: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadBody {
    pub body_id: BodyId,
    pub offset: ByteOffset,
    /// At most the negotiated `max_body_chunk_bytes`.
    pub max_bytes: u32,
}

/// Paging direction, independent of the chronological order within a page.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDirection {
    #[default]
    Forward,
    Backward,
}

/// Read the complete, secret-masked JSON input while this approval is pending.
/// Each chunk revalidates the registry identity; it is not a durable body ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetApprovalInput {
    pub approval_id: ApprovalId,
    pub core_epoch: InstanceEpoch,
    pub approval_revision: Revision,
    pub offset: ByteOffset,
    pub max_bytes: u32,
}

/// An approval input has the same UTF-8 byte-offset rules as ReadBody.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalInputChunk {
    pub approval_id: ApprovalId,
    pub core_epoch: InstanceEpoch,
    pub approval_revision: Revision,
    pub offset: ByteOffset,
    pub text: String,
    /// Size of the complete secret-masked input (before wire JSON escaping).
    pub total_bytes: ByteOffset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<ByteOffset>,
}

impl ApprovalInputChunk {
    pub fn slice(request: &GetApprovalInput, input: &str) -> Result<Self, ValidationError> {
        if input.len() > crate::limits::MAX_APPROVAL_INPUT_BYTES {
            return Err(ValidationError::TooLong {
                field: "input",
                max: crate::limits::MAX_APPROVAL_INPUT_BYTES,
                actual: input.len(),
            });
        }
        let range = crate::views::chunk_range(input, request.offset, request.max_bytes)?;
        Ok(Self {
            approval_id: request.approval_id.clone(),
            core_epoch: request.core_epoch,
            approval_revision: request.approval_revision,
            offset: request.offset,
            text: input[range.clone()].to_owned(),
            total_bytes: ByteOffset(input.len() as u64),
            next_offset: (range.end < input.len()).then_some(ByteOffset(range.end as u64)),
        })
    }
    pub fn validate(&self, max_bytes: u32) -> Result<(), ValidationError> {
        if self.total_bytes.get() > crate::limits::MAX_APPROVAL_INPUT_BYTES as u64 {
            return Err(ValidationError::OutOfRange {
                field: "total_bytes",
                reason: "approval input exceeds 1 MiB",
            });
        }
        crate::views::validate_text_chunk(
            self.offset,
            &self.text,
            self.total_bytes,
            self.next_offset,
            max_bytes,
        )
    }
}

/// Continue a frozen subscription snapshot without a live subscription or SSE
/// connection. An expired or epoch-invalidated snapshot requires resync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadSnapshot {
    pub snapshot_id: SnapshotId,
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPage {
    pub snapshot_id: SnapshotId,
    /// The original snapshot cut, not the current live event cursor.
    pub high_water: EventCursor,
    pub page: Page<SnapshotItem>,
}

/// Every read-only request a client may send: one namespace, then one of its
/// operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Query {
    Conversation(ConversationQuery),
    Workspace(WorkspaceQuery),
    Execution(ExecutionQuery),
    Questions(QuestionsQuery),
    Approvals(ApprovalsQuery),
    Lanes(LanesQuery),
    Lifecycle(LifecycleQuery),
    Projection(ProjectionQuery),
    Settings(SettingsQuery),
    Auth(AuthQuery),
    Mcp(McpQuery),
    Teams(TeamsQuery),
    Diagnostics(DiagnosticsQuery),
    Memory(MemoryQuery),
    Evaluation(EvaluationQuery),
    Improvement(ImprovementQuery),
    PullRequests(PullRequestsQuery),
    Browser(BrowserQuery),
    Artifacts(ArtifactsQuery),
    Maintenance(MaintenanceQuery),
}

impl Query {
    /// Wire names of every namespace, for rejecting unknown ones precisely.
    pub const TYPES: [&'static str; 20] = crate::namespaces::NAMESPACE_NAMES;

    pub fn required_capability(&self) -> Capability {
        match self {
            Self::Conversation(q) => q.required_capability(),
            Self::Workspace(q) => q.required_capability(),
            Self::Execution(q) => q.required_capability(),
            Self::Questions(q) => q.required_capability(),
            Self::Approvals(q) => q.required_capability(),
            Self::Lanes(q) => q.required_capability(),
            Self::Lifecycle(q) => q.required_capability(),
            Self::Projection(q) => q.required_capability(),
            Self::Settings(q) => q.required_capability(),
            Self::Auth(q) => q.required_capability(),
            Self::Mcp(q) => q.required_capability(),
            Self::Teams(q) => q.required_capability(),
            Self::Diagnostics(q) => q.required_capability(),
            Self::Memory(q) => q.required_capability(),
            Self::Evaluation(q) => q.required_capability(),
            Self::Improvement(q) => q.required_capability(),
            Self::PullRequests(q) => q.required_capability(),
            Self::Browser(q) => q.required_capability(),
            Self::Artifacts(q) => q.required_capability(),
            Self::Maintenance(q) => q.required_capability(),
        }
    }

    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Conversation(q) => q.validate(limits),
            Self::Workspace(q) => q.validate(limits),
            Self::Execution(q) => q.validate(limits),
            Self::Questions(q) => q.validate(limits),
            Self::Approvals(q) => q.validate(limits),
            Self::Lanes(q) => q.validate(limits),
            Self::Lifecycle(q) => q.validate(limits),
            Self::Projection(q) => q.validate(limits),
            Self::Settings(q) => q.validate(limits),
            Self::Auth(q) => q.validate(limits),
            Self::Mcp(q) => q.validate(limits),
            Self::Teams(q) => q.validate(limits),
            Self::Diagnostics(q) => q.validate(limits),
            Self::Memory(q) => q.validate(limits),
            Self::Evaluation(q) => q.validate(limits),
            Self::Improvement(q) => q.validate(limits),
            Self::PullRequests(q) => q.validate(limits),
            Self::Browser(q) => q.validate(limits),
            Self::Artifacts(q) => q.validate(limits),
            Self::Maintenance(q) => q.validate(limits),
        }
    }
}

/// What the core knows about an earlier command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum CommandReceiptState {
    /// The core has no retained record. Eviction or restart can lose a receipt,
    /// so this is not proof of non-execution and must not trigger a blind resend.
    Unknown,
    Applied {
        reply: CommandReply,
    },
    Rejected {
        error: WorkbenchError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReceipt {
    pub request_id: RequestId,
    pub state: CommandReceiptState,
}

/// Success answer to a query, from the same namespace the query named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QueryReply {
    Conversation(ConversationReply),
    Workspace(WorkspaceReply),
    Execution(ExecutionReply),
    Questions(QuestionsReply),
    Approvals(ApprovalsReply),
    Lanes(LanesReply),
    Lifecycle(LifecycleReply),
    Projection(ProjectionReply),
    Settings(SettingsReply),
    Auth(AuthReply),
    Mcp(McpReply),
    Teams(TeamsReply),
    Diagnostics(DiagnosticsReply),
    Memory(MemoryReply),
    Evaluation(EvaluationReply),
    Improvement(ImprovementReply),
    PullRequests(PullRequestsReply),
    Browser(BrowserReply),
    Artifacts(ArtifactsReply),
    Maintenance(MaintenanceReply),
}

impl CommandReceipt {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match &self.state {
            CommandReceiptState::Rejected { error } => error.validate(),
            CommandReceiptState::Unknown | CommandReceiptState::Applied { .. } => Ok(()),
        }
    }
}

impl QueryReply {
    /// Check a reply received from a peer: every page against the page limit
    /// and every item, string and chunk against its bound. A reply that fails
    /// is never handed to application code.
    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Conversation(r) => r.validate(limits),
            Self::Workspace(r) => r.validate(limits),
            Self::Execution(r) => r.validate(limits),
            Self::Questions(r) => r.validate(limits),
            Self::Approvals(r) => r.validate(limits),
            Self::Lanes(r) => r.validate(limits),
            Self::Lifecycle(r) => r.validate(limits),
            Self::Projection(r) => r.validate(limits),
            Self::Settings(r) => r.validate(limits),
            Self::Auth(r) => r.validate(limits),
            Self::Mcp(r) => r.validate(limits),
            Self::Teams(r) => r.validate(limits),
            Self::Diagnostics(r) => r.validate(limits),
            Self::Memory(r) => r.validate(limits),
            Self::Evaluation(r) => r.validate(limits),
            Self::Improvement(r) => r.validate(limits),
            Self::PullRequests(r) => r.validate(limits),
            Self::Browser(r) => r.validate(limits),
            Self::Artifacts(r) => r.validate(limits),
            Self::Maintenance(r) => r.validate(limits),
        }
    }
}
