//! Read-only requests and their answers.

use crate::commands::CommandReply;
use crate::errors::WorkbenchError;
use crate::events::{EventCursor, SnapshotItem};
use crate::handshake::{Capability, CoreStatus, WireLimits};
use crate::ids::{
    ApprovalId, BodyId, PageCursor, ProjectId, RequestId, RunId, SnapshotId, ThreadId,
};
use crate::limits::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, MIN_BODY_CHUNK_BYTES};
use crate::validation::ValidationError;
use crate::views::{
    ApprovalView, BodyChunk, CommittedMessage, InputReceipt, Page, ProjectSummary, QuestionView,
    RunSummary, ThreadSummary, ThreadView,
};
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

/// Every read-only request a client may send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Query {
    GetCoreStatus,
    ListProjects(PageRequest),
    ListThreads(ListThreads),
    GetThread(crate::commands::ThreadTarget),
    ListRuns(ThreadQuery),
    ListQuestions(PendingQuery),
    ListApprovals(PendingQuery),
    GetInputReceipts(ThreadQuery),
    /// What became of an earlier command, looked up by its request id.
    GetCommandReceipt {
        request_id: RequestId,
    },
    ReadCommittedHistory(ReadCommittedHistory),
    ReadBody(ReadBody),
    GetApprovalInput(GetApprovalInput),
    ReadSnapshot(ReadSnapshot),
}

impl Query {
    /// Wire names of every variant, for rejecting unknown ones precisely.
    pub const TYPES: [&'static str; 13] = [
        "get_core_status",
        "list_projects",
        "list_threads",
        "get_thread",
        "list_runs",
        "list_questions",
        "list_approvals",
        "get_input_receipts",
        "get_command_receipt",
        "read_committed_history",
        "read_body",
        "get_approval_input",
        "read_snapshot",
    ];

    pub fn required_capability(&self) -> Capability {
        match self {
            Self::ListQuestions(_) => Capability::Questions,
            Self::ListApprovals(_) | Self::GetApprovalInput(_) => Capability::Approvals,
            Self::GetInputReceipts(_) => Capability::InputReceipts,
            Self::GetCommandReceipt { .. } => Capability::Commands,
            Self::ReadCommittedHistory(_) | Self::ReadBody(_) => Capability::CommittedBodies,
            _ => Capability::Queries,
        }
    }

    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetCoreStatus | Self::GetThread(_) | Self::GetCommandReceipt { .. } => Ok(()),
            Self::ListProjects(page) => page.validate(),
            Self::ListThreads(q) => q.page.validate(),
            Self::ListRuns(q) | Self::GetInputReceipts(q) => q.page.validate(),
            Self::ListQuestions(q) | Self::ListApprovals(q) => q.page.validate(),
            Self::ReadCommittedHistory(q) => q.page.validate(),
            Self::ReadSnapshot(q) => q.page.validate(),
            Self::ReadBody(ReadBody { max_bytes, .. })
            | Self::GetApprovalInput(GetApprovalInput { max_bytes, .. }) => {
                if *max_bytes < MIN_BODY_CHUNK_BYTES || *max_bytes > limits.max_body_chunk_bytes {
                    return Err(ValidationError::OutOfRange {
                        field: "max_bytes",
                        reason: "outside the negotiated body chunk range",
                    });
                }
                Ok(())
            }
        }
    }
}

/// What the core knows about an earlier command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum CommandReceiptState {
    /// The core has no record, so the command did not take effect and may be sent again.
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

/// Success answer to a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum QueryReply {
    CoreStatus(CoreStatus),
    Projects(Page<ProjectSummary>),
    Threads(Page<ThreadSummary>),
    Thread(ThreadView),
    Runs(Page<RunSummary>),
    Questions(Page<QuestionView>),
    Approvals(Page<ApprovalView>),
    InputReceipts(Page<InputReceipt>),
    CommandReceipt(CommandReceipt),
    CommittedHistory(Page<CommittedMessage>),
    Body(BodyChunk),
    ApprovalInput(ApprovalInputChunk),
    Snapshot(SnapshotPage),
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
            Self::CoreStatus(status) => status.validate(),
            Self::Projects(page) => page.validate_with(ProjectSummary::validate),
            Self::Threads(page) => page.validate_with(ThreadSummary::validate),
            Self::Thread(view) => view.validate(),
            Self::Runs(page) => page.validate(),
            Self::Questions(page) => page.validate_with(QuestionView::validate),
            Self::Approvals(page) => page.validate_with(ApprovalView::validate),
            Self::InputReceipts(page) => page.validate_with(InputReceipt::validate),
            Self::CommandReceipt(receipt) => receipt.validate(),
            Self::CommittedHistory(page) => page.validate_with(CommittedMessage::validate),
            Self::Body(chunk) => chunk.validate(limits.max_body_chunk_bytes),
            Self::ApprovalInput(chunk) => chunk.validate(limits.max_body_chunk_bytes),
            Self::Snapshot(snapshot) => snapshot.page.validate_with(SnapshotItem::validate),
        }
    }
}
