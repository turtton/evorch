//! Subscriptions and the events pushed on them.
//!
//! A subscription starts from a snapshot and then follows events. Every event
//! carries an [`EventCursor`]; the snapshot's `high_water` cursor is the
//! boundary: events at or before it are already reflected in the snapshot.
//! Delivery may repeat events (ADR 0045 allows SSE duplicates) but never skips
//! silently: a subscriber that cannot be served receives
//! [`UiEvent::ResyncRequired`] and must take a new snapshot. Token deltas are
//! not events; only committed messages are.

use crate::handshake::CoreStatus;
use crate::ids::{ProjectId, RequestId, SnapshotId, SubscriptionId, ThreadId};
use crate::validation::ValidationError;
use crate::views::{
    ApprovalView, CommittedMessage, InputReceipt, Page, ProjectSummary, QuestionView, RunSummary,
    ThreadSummary,
};
use crate::wire::{InstanceEpoch, Sequence};
use serde::{Deserialize, Serialize};

/// A position in the event stream: unique only within one core instance epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventCursor {
    pub core_epoch: InstanceEpoch,
    pub sequence: Sequence,
}

/// What a subscription covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum SubscriptionScope {
    /// Core status, projects and pending questions and approvals everywhere.
    All,
    Project {
        project: ProjectId,
    },
    Thread {
        thread: ThreadId,
    },
}

/// Start following events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscribe {
    pub scope: SubscriptionScope,
    /// Resume after this cursor instead of taking a snapshot. If the core
    /// cannot replay from it (too old, another epoch) it answers with a
    /// snapshot instead and the client must discard what it held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<EventCursor>,
}

impl Subscribe {
    pub fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// One entity in an initial snapshot page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum SnapshotItem {
    CoreStatus(CoreStatus),
    Project(ProjectSummary),
    Thread(ThreadSummary),
    Run(RunSummary),
    Question(QuestionView),
    Approval(ApprovalView),
}

/// Answer to a subscribe request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionStarted {
    pub subscription_id: SubscriptionId,
    /// Present when the client must replace its state with the snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<SnapshotId>,
    /// The point the event stream continues from; events at or before it are
    /// never delivered. With a `snapshot_id` it is the last event the snapshot
    /// already reflects (possibly in a new epoch). Without one the core is
    /// replaying: it equals the requested `after` cursor and every later event
    /// follows, so the client keeps the state it already holds.
    pub high_water: EventCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_page: Option<Page<SnapshotItem>>,
}

/// Why a subscriber has to start over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    /// The requested cursor fell out of the replay buffer.
    CursorTooOld,
    /// The core restarted; cursors of the old epoch mean nothing.
    EpochChanged,
    /// The subscriber did not keep up and events were dropped.
    SlowSubscriber,
}

/// Why a subscription ended without a resync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Unsubscribed,
    CoreShuttingDown,
    ScopeRemoved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum UiEvent {
    CoreStatusChanged { status: CoreStatus },
    ThreadChanged { thread: ThreadSummary },
    RunChanged { run: RunSummary },
    InputReceiptChanged { receipt: InputReceipt },
    QuestionChanged { question: QuestionView },
    ApprovalChanged { approval: ApprovalView },
    MessageCommitted { message: CommittedMessage },
    ResyncRequired { reason: ResyncReason },
    SubscriptionClosed { reason: CloseReason },
}

impl UiEvent {
    /// Wire names of every variant.
    pub const TYPES: [&'static str; 9] = [
        "core_status_changed",
        "thread_changed",
        "run_changed",
        "input_receipt_changed",
        "question_changed",
        "approval_changed",
        "message_committed",
        "resync_required",
        "subscription_closed",
    ];

    /// Whether the subscription is over after this event.
    pub fn ends_subscription(&self) -> bool {
        matches!(
            self,
            Self::ResyncRequired { .. } | Self::SubscriptionClosed { .. }
        )
    }
}

/// An event on a subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub subscription_id: SubscriptionId,
    pub cursor: EventCursor,
    /// The request whose effect this event reports, so a client can tell its
    /// own changes from other clients'.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by_request_id: Option<RequestId>,
    pub event: UiEvent,
}

/// What to do with the next event, given what the subscriber has already seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorDecision {
    /// New: apply it.
    Deliver,
    /// At or before the last cursor seen (or the snapshot boundary): skip it.
    Duplicate,
    /// From another epoch: the stream is unusable until a new snapshot.
    EpochMismatch,
}

/// Client-side ordering rule for one subscription. Pure so every client
/// (desktop, CLI, Web) shares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorTracker {
    last: EventCursor,
}

impl CursorTracker {
    /// Start at the snapshot boundary.
    pub fn new(high_water: EventCursor) -> Self {
        Self { last: high_water }
    }

    pub fn last(&self) -> EventCursor {
        self.last
    }

    /// Classify `cursor` and, when it is new, advance past it.
    pub fn observe(&mut self, cursor: EventCursor) -> CursorDecision {
        if cursor.core_epoch != self.last.core_epoch {
            return CursorDecision::EpochMismatch;
        }
        if cursor.sequence <= self.last.sequence {
            return CursorDecision::Duplicate;
        }
        self.last = cursor;
        CursorDecision::Deliver
    }
}

impl SnapshotItem {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::CoreStatus(status) => status.validate(),
            Self::Project(project) => project.validate(),
            Self::Thread(thread) => thread.validate(),
            Self::Run(_) => Ok(()),
            Self::Question(question) => question.validate(),
            Self::Approval(approval) => approval.validate(),
        }
    }
}

impl SubscriptionStarted {
    /// Bound a subscribe answer received from a peer, snapshot page included.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.initial_page
            .as_ref()
            .map_or(Ok(()), |page| page.validate_with(SnapshotItem::validate))
    }
}

impl UiEvent {
    /// Bound an event received from a peer.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::CoreStatusChanged { status } => status.validate(),
            Self::ThreadChanged { thread } => thread.validate(),
            Self::RunChanged { .. }
            | Self::ResyncRequired { .. }
            | Self::SubscriptionClosed { .. } => Ok(()),
            Self::InputReceiptChanged { receipt } => receipt.validate(),
            Self::QuestionChanged { question } => question.validate(),
            Self::ApprovalChanged { approval } => approval.validate(),
            Self::MessageCommitted { message } => message.validate(),
        }
    }
}

impl EventEnvelope {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.event.validate()
    }
}
