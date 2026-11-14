//! Lanes namespace: lane management (ADR 0044). The actor is attached by the
//! core's ingress, never sent by a client.
use crate::{
    handshake::{Capability, WireLimits},
    ids::{RunId, ThreadId},
    lane_attention::{LaneAttentionAction, LaneId, LaneReadyState},
    lane_management::{
        CloseLane, LaneCloseRecordKind, LaneCloseResult, LaneManagementPolicy,
        LaneManagementResult, LaneManagementState, ManageLane, ManagedLanePolicy,
    },
    limits::MAX_TITLE_BYTES,
    queries::PageRequest,
    validation::{ValidationError, check_bytes, check_count},
    views::Page,
    wire::{Revision, Sequence, UnixMillis},
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &["manage_lane", "close_lane", "resume_lane_deliveries"];
/// `manage_lane`'s `request.action.type` values: the wire names of
/// [`LaneManagementAction`](crate::lane_management::LaneManagementAction)'s
/// variants (a test keeps the two in step).
pub const MANAGE_LANE_ACTIONS: &[&str] = &["set_policy", "detach", "reattach"];
/// The tagged choice inside a command's payload that classification names.
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[super::NestedOperation {
    operation: "manage_lane",
    pointer: "/request/action/type",
    known: MANAGE_LANE_ACTIONS,
}];
pub const QUERY_OPERATIONS: &[&str] = &[
    "get_lane_management",
    "list_lanes",
    "get_lane",
    "get_lane_close_preview",
];
pub const EVENT_TYPES: &[&str] = &["lane_changed"];

/// Most attention entries one [`LaneDetail`] shows.
pub const MAX_LANE_DETAIL_ATTENTION: usize = 16;
/// Most acceptance criteria a [`LaneClosePreview`] shows.
pub const MAX_CLOSE_PREVIEW_CRITERIA: usize = 16;
/// Most artifact references a [`LaneClosePreviewReady`] lists; the rest are counted.
pub const MAX_CLOSE_PREVIEW_ARTIFACT_REFS: usize = 8;
/// Longest brief objective a [`LaneClosePreview`] carries, in bytes (cut, not refused).
pub const MAX_CLOSE_PREVIEW_OBJECTIVE_BYTES: usize = 4096;
/// Longest criterion or artifact reference in a [`LaneClosePreview`], in bytes.
pub const MAX_CLOSE_PREVIEW_ITEM_BYTES: usize = 512;
/// The most threads a lane change names besides its lane thread and current lead.
pub const MAX_LANE_EVENT_THREADS: usize = 4;

/// Whether a lane's root run was ever started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneStartState {
    NotStarted,
    /// A cancelled or locally failed creation; never recovered automatically.
    StartBlocked,
    /// Stopped by the operator before the root started; resumable.
    OperatorStopped,
    Started,
}

/// What the lane itself reported last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneReportStatus {
    Working,
    Waiting,
    Blocked,
    Ready,
    Failed,
}

/// The close that stands for a managed lane now, as the ledger records it. It is a record of
/// a judgement, never a state of any run: a later Report lifts it, and the lane then reads as
/// active again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneCurrentClose {
    pub close_id: String,
    pub kind: LaneCloseRecordKind,
    /// The Ready the close was about; none for a human close without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_seq: Option<Sequence>,
}

/// One lane as a listing shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneSummary {
    pub lane_id: LaneId,
    pub thread_id: ThreadId,
    /// The lead that manages the lane now; absent once detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_thread_id: Option<ThreadId>,
    pub policy: LaneManagementPolicy,
    /// The revision a management action must name: an older one is refused.
    pub management_revision: Revision,
    pub brief_revision: Revision,
    /// The start of the objective: a preview, not the brief.
    pub objective_preview: String,
    /// The current root run; absent until the first start. A run that is not
    /// this one is an older incarnation and is never a target of an action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_run: Option<RunId>,
    /// 0 until the root started, then raised by every start or restart.
    pub generation: Revision,
    pub start: LaneStartState,
    pub last_activity_at: UnixMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_report_status: Option<LaneReportStatus>,
    /// The close that stands now; absent when none, once a Report lifted it, and for a
    /// detached lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_close: Option<LaneCurrentClose>,
    /// Changes with anything above; compare for equality only.
    pub revision: Revision,
}

impl LaneSummary {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes(
            "objective_preview",
            &self.objective_preview,
            MAX_TITLE_BYTES,
            true,
        )
    }
}

/// The latest report of a lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneReportView {
    pub seq: Sequence,
    pub status: LaneReportStatus,
    pub summary: String,
    pub needs_human: bool,
    /// The management revision the lane had when it reported.
    pub management_revision: Revision,
    /// The brief revision the lane had *received* when it reported. A Ready
    /// refers to this one, not to the lane's current brief.
    pub brief_revision: Revision,
    pub created_at: UnixMillis,
}

/// What the lane's last Ready claims and where its verification stands. It never
/// stops the lead from closing: closing has no gate of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneReadyView {
    pub report_seq: Sequence,
    /// The brief revision the Ready was reported against.
    pub brief_revision: Revision,
    /// The brief changed after that Ready.
    pub brief_changed_since: bool,
    pub readiness: LaneReadyState,
}

/// Why a lane wants its lead's attention, in short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneAttentionTag {
    QuestionPending,
    Blocked,
    Failed,
    RestoreFailed,
    ReadyUnclosed,
    QuestionCapacityBlocked,
    WakeSuppressed,
    StopPending,
    Stall,
}

/// One entry of a lane's attention list. The full item is the attention
/// read model's; this is what a detail shows beside the lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneAttentionBrief {
    pub tag: LaneAttentionTag,
    pub summary: String,
    pub revision: Revision,
    pub since: UnixMillis,
    pub available_actions: Vec<LaneAttentionAction>,
}

/// Notices held for the lane's lead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneDeliveryView {
    /// A Stop on the lead holds notices that are not lost; resuming delivers them.
    pub held: bool,
    /// How many notices are held (saturating); 0 when none or unknown.
    pub pending_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneDetail {
    pub summary: LaneSummary,
    /// The management revision at which the lane became independent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_since_management_revision: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_report: Option<LaneReportView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<LaneReadyView>,
    pub delivery: LaneDeliveryView,
    pub attention: Vec<LaneAttentionBrief>,
}

impl LaneDetail {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.summary.validate()?;
        if let Some(report) = &self.latest_report {
            check_bytes("summary", &report.summary, MAX_TITLE_BYTES, true)?;
        }
        check_count("attention", self.attention.len(), MAX_LANE_DETAIL_ATTENTION)?;
        for item in &self.attention {
            check_bytes("summary", &item.summary, MAX_TITLE_BYTES, true)?;
        }
        Ok(())
    }
}

/// How a Ready's fixed artifact revision is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneArtifactRevisionKind {
    CommitSha,
    Sha256,
}

/// The fixed artifact revision a Ready names, exactly as the lane reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneArtifactRevisionView {
    pub kind: LaneArtifactRevisionKind,
    pub value: String,
}

/// The Ready a close would be about, as the lane reported it. Nothing here is checked
/// against Git, GitHub or the artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneClosePreviewReady {
    pub report_seq: Sequence,
    /// The brief revision the Ready was reported for. A Ready for an earlier brief than
    /// [`LaneClosePreview::brief_revision`] is for reference only.
    pub brief_revision: Revision,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_revision: Option<LaneArtifactRevisionView>,
    /// At most [`MAX_CLOSE_PREVIEW_ARTIFACT_REFS`] of the references, in the order reported.
    pub artifact_refs: Vec<String>,
    /// How many references the Ready names in all.
    pub artifact_ref_count: u32,
}

/// What a person reads before ending a managed lane, from one snapshot of the ledger, with
/// the revisions a [`CloseLane`] must name. Read only: no part of it is a condition of the
/// close, and a close stops no run and deletes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneClosePreview {
    pub lane_id: LaneId,
    pub thread_id: ThreadId,
    /// The start of the brief's objective, cut at [`MAX_CLOSE_PREVIEW_OBJECTIVE_BYTES`].
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub brief_revision: Revision,
    /// The revisions `CloseLane` compares; send these, not newer ones.
    pub management_revision: Revision,
    pub request_revision: Revision,
    /// The latest report when it is a Ready for the current management.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<LaneClosePreviewReady>,
    /// Questions raised by the lane's own runs that nobody has answered (saturating). A close
    /// leaves them open.
    pub open_question_count: u32,
    /// A run of the lane's thread is working now (not idle between turns, not finished). A
    /// close does not stop it.
    pub running: bool,
    /// The close that stands now, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_close: Option<LaneCurrentClose>,
}

impl LaneClosePreview {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes(
            "objective",
            &self.objective,
            MAX_CLOSE_PREVIEW_OBJECTIVE_BYTES,
            true,
        )?;
        check_count(
            "acceptance_criteria",
            self.acceptance_criteria.len(),
            MAX_CLOSE_PREVIEW_CRITERIA,
        )?;
        for item in &self.acceptance_criteria {
            check_bytes(
                "acceptance_criteria",
                item,
                MAX_CLOSE_PREVIEW_ITEM_BYTES,
                true,
            )?;
        }
        if let Some(ready) = &self.ready {
            check_bytes("summary", &ready.summary, MAX_TITLE_BYTES, true)?;
            check_count(
                "artifact_refs",
                ready.artifact_refs.len(),
                MAX_CLOSE_PREVIEW_ARTIFACT_REFS,
            )?;
            for item in &ready.artifact_refs {
                check_bytes("artifact_refs", item, MAX_CLOSE_PREVIEW_ITEM_BYTES, true)?;
            }
            if let Some(revision) = &ready.artifact_revision {
                check_bytes(
                    "artifact_revision",
                    &revision.value,
                    MAX_CLOSE_PREVIEW_ITEM_BYTES,
                    true,
                )?;
            }
        }
        Ok(())
    }
}

/// The lanes a lead manages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListLanes {
    pub lead_thread: ThreadId,
    /// Only lanes under this policy; absent for every managed lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ManagedLanePolicy>,
    /// List the detached lanes (no lead manages them) that `lead_thread` can find again,
    /// instead of the lanes it manages: those opened from it and those it managed last
    /// (the lead of the lane's latest reattach). `policy` does not apply: every one of them
    /// is detached. A detached lane is in no lead's managed list, so this is how a client
    /// finds it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    #[serde(default)]
    pub page: PageRequest,
}

/// Deliver the notices held for a lane's lead again. A human action; both
/// expectations are checked under the current state, so a request made against
/// an older lead, root run or management revision is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeLaneDeliveries {
    pub lane_id: LaneId,
    pub expected_management_revision: Revision,
    pub lead_thread_id: ThreadId,
    pub expected_lead_root_run_id: RunId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneDeliveriesResumed {
    pub lane_id: LaneId,
    pub lead_thread_id: ThreadId,
    pub root_run_id: RunId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LanesCommand {
    ManageLane(ManageLane),
    CloseLane(CloseLane),
    ResumeLaneDeliveries(ResumeLaneDeliveries),
}

impl LanesCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ManageLane(_) | Self::ResumeLaneDeliveries(_) => Ok(()),
            Self::CloseLane(close) => close.validate(),
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
pub enum LanesResult {
    Managed(LaneManagementResult),
    Closed(LaneCloseResult),
    DeliveriesResumed(LaneDeliveriesResumed),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LanesQuery {
    GetLaneManagement {
        lane_id: LaneId,
    },
    ListLanes(ListLanes),
    GetLane {
        lane_id: LaneId,
    },
    /// What a person reads before closing or discarding a managed lane, with the revisions
    /// the `close_lane` command must name.
    GetLaneClosePreview {
        lane_id: LaneId,
    },
}

impl LanesQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetLaneManagement { .. }
            | Self::GetLane { .. }
            | Self::GetLaneClosePreview { .. } => Ok(()),
            Self::ListLanes(q) => q.page.validate(),
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
pub enum LanesReply {
    LaneManagement(LaneManagementState),
    Lanes(Page<LaneSummary>),
    Lane(Box<LaneDetail>),
    LaneClosePreview(Box<LaneClosePreview>),
}

impl LanesReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::LaneManagement(_) => Ok(()),
            Self::Lanes(page) => page.validate_with(LaneSummary::validate),
            Self::Lane(lane) => lane.validate(),
            Self::LaneClosePreview(preview) => preview.validate(),
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
pub enum LanesEvent {
    /// A lane changed (its management, brief, root run or reports). `also_for` names the
    /// threads that show the lane without being its lane thread or current lead (the leads
    /// that find it again once detached, including the lead that just detached it), so a
    /// scoped subscription to any of them receives the change.
    LaneChanged {
        lane: LaneSummary,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        also_for: Vec<ThreadId>,
    },
}

impl LanesEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::LaneChanged { lane, also_for } => {
                check_count("also_for", also_for.len(), MAX_LANE_EVENT_THREADS)?;
                lane.validate()
            }
        }
    }
}
