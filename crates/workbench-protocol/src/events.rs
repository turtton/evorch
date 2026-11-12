//! Subscriptions and the events pushed on them.
//!
//! A subscription starts from a snapshot and then follows events. Every event
//! carries an [`EventCursor`]; the snapshot's `high_water` cursor is the
//! boundary: events at or before it are already reflected in the snapshot.
//! Delivery may repeat events (ADR 0045 allows SSE duplicates) but never skips
//! silently: a subscriber that cannot be served receives
//! [`ProjectionEvent::ResyncRequired`] and must take a new snapshot. Token deltas are
//! not events; only committed messages are.

use crate::handshake::CoreStatus;
use crate::ids::{ProjectId, RequestId, SnapshotId, SubscriptionId, ThreadId};
use crate::namespaces::{
    approvals::ApprovalsEvent, artifacts::ArtifactsEvent, auth::AuthEvent, browser::BrowserEvent,
    conversation::ConversationEvent, diagnostics::DiagnosticsEvent, evaluation::EvaluationEvent,
    execution::ExecutionEvent, improvement::ImprovementEvent, lanes::LanesEvent,
    lifecycle::LifecycleEvent, maintenance::MaintenanceEvent, mcp::McpEvent, memory::MemoryEvent,
    projection::ProjectionEvent, pull_requests::PullRequestsEvent, questions::QuestionsEvent,
    settings::SettingsEvent, teams::TeamsEvent, workspace::WorkspaceEvent,
};
use crate::validation::ValidationError;
use crate::views::{ApprovalView, Page, ProjectSummary, QuestionView, RunSummary, ThreadSummary};
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
    /// Unique within one connection and never reused after the subscription
    /// ends: a core numbers subscriptions monotonically per connection. A
    /// client still tolerates a non-conforming peer that reuses an id (it
    /// registers the new subscription and never lets a finished one release
    /// it), but a reused id is a core bug, not something to rely on.
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

/// An event on a subscription: one namespace, then one of its events. Only the
/// projection namespace's control notices end a subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum UiEvent {
    Conversation(ConversationEvent),
    Workspace(WorkspaceEvent),
    Execution(ExecutionEvent),
    Questions(QuestionsEvent),
    Approvals(ApprovalsEvent),
    Lanes(LanesEvent),
    Lifecycle(LifecycleEvent),
    Projection(ProjectionEvent),
    Settings(SettingsEvent),
    Auth(AuthEvent),
    Mcp(McpEvent),
    Teams(TeamsEvent),
    Diagnostics(DiagnosticsEvent),
    Memory(MemoryEvent),
    Evaluation(EvaluationEvent),
    Improvement(ImprovementEvent),
    PullRequests(PullRequestsEvent),
    Browser(BrowserEvent),
    Artifacts(ArtifactsEvent),
    Maintenance(MaintenanceEvent),
}

impl UiEvent {
    /// Wire names of every namespace.
    pub const TYPES: [&'static str; 20] = crate::namespaces::NAMESPACE_NAMES;

    /// Whether the subscription is over after this event. Only the projection
    /// namespace's control notices end one; no other namespace may, so a
    /// namespace gaining events never changes this.
    pub fn ends_subscription(&self) -> bool {
        match self {
            Self::Projection(e) => e.ends_subscription(),
            _ => false,
        }
    }

    /// The wire name of the namespace this event belongs to.
    pub fn namespace(&self) -> &'static str {
        crate::namespaces::NAMESPACE_NAMES[self.namespace_index()]
    }

    fn namespace_index(&self) -> usize {
        match self {
            Self::Conversation(_) => 0,
            Self::Workspace(_) => 1,
            Self::Execution(_) => 2,
            Self::Questions(_) => 3,
            Self::Approvals(_) => 4,
            Self::Lanes(_) => 5,
            Self::Lifecycle(_) => 6,
            Self::Projection(_) => 7,
            Self::Settings(_) => 8,
            Self::Auth(_) => 9,
            Self::Mcp(_) => 10,
            Self::Teams(_) => 11,
            Self::Diagnostics(_) => 12,
            Self::Memory(_) => 13,
            Self::Evaluation(_) => 14,
            Self::Improvement(_) => 15,
            Self::PullRequests(_) => 16,
            Self::Browser(_) => 17,
            Self::Artifacts(_) => 18,
            Self::Maintenance(_) => 19,
        }
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
            Self::Conversation(e) => e.validate(),
            Self::Workspace(e) => e.validate(),
            Self::Execution(e) => e.validate(),
            Self::Questions(e) => e.validate(),
            Self::Approvals(e) => e.validate(),
            Self::Lanes(e) => e.validate(),
            Self::Lifecycle(e) => e.validate(),
            Self::Projection(e) => e.validate(),
            Self::Settings(e) => e.validate(),
            Self::Auth(e) => e.validate(),
            Self::Mcp(e) => e.validate(),
            Self::Teams(e) => e.validate(),
            Self::Diagnostics(e) => e.validate(),
            Self::Memory(e) => e.validate(),
            Self::Evaluation(e) => e.validate(),
            Self::Improvement(e) => e.validate(),
            Self::PullRequests(e) => e.validate(),
            Self::Browser(e) => e.validate(),
            Self::Artifacts(e) => e.validate(),
            Self::Maintenance(e) => e.validate(),
        }
    }
}

impl EventEnvelope {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.event.validate()
    }
}
