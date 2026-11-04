//! Read-only requests and their answers.

use crate::commands::CommandReply;
use crate::errors::WorkbenchError;
use crate::handshake::{Capability, CoreStatus, WireLimits};
use crate::ids::{BodyId, PageCursor, ProjectId, RequestId, RunId, ThreadId};
use crate::limits::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, MIN_BODY_CHUNK_BYTES};
use crate::validation::ValidationError;
use crate::views::{
    ApprovalView, BodyChunk, CommittedMessage, InputReceipt, Page, ProjectSummary, QuestionView,
    RunSummary, ThreadSummary, ThreadView,
};
use crate::wire::ByteOffset;
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

/// Every read-only request a client may send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
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
}

impl Query {
    /// Wire names of every variant, for rejecting unknown ones precisely.
    pub const TYPES: [&'static str; 11] = [
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
    ];

    pub fn required_capability(&self) -> Capability {
        match self {
            Self::ListQuestions(_) => Capability::Questions,
            Self::ListApprovals(_) => Capability::Approvals,
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
            Self::ReadBody(q) => {
                if q.max_bytes < MIN_BODY_CHUNK_BYTES || q.max_bytes > limits.max_body_chunk_bytes {
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
}
