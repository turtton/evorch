//! ADR 0044 Phase 3 durable management boundary. The authenticated caller is not
//! part of the deserializable request. Host family checks must be held across the
//! call and projection save; SQL cannot validate sidebar.json's family revision.
use crate::{
    StorageError,
    lanes::{LanePolicy, LaneRecord},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedLanePolicy {
    Attached,
    Summary,
}
impl From<ManagedLanePolicy> for LanePolicy {
    fn from(p: ManagedLanePolicy) -> Self {
        match p {
            ManagedLanePolicy::Attached => Self::Attached,
            ManagedLanePolicy::Summary => Self::Summary,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneManagementRequest {
    pub operation_id: String,
    pub lane_id: String,
    pub expected_management_revision: u64,
    pub action: LaneManagementAction,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LaneManagementAction {
    SetPolicy {
        policy: ManagedLanePolicy,
    },
    Detach,
    Reattach {
        lead_thread_id: String,
        expected_lead_root_run_id: String,
    },
}
/// Supplied by trusted ingress, never decoded from model/client JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneManagementActor {
    User,
    Lead {
        thread_id: String,
        root_run_id: String,
    },
}
/// Authoritative independent family state, checked by the host under its lock.
/// No family revision in SQL: the host validates it before making this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneAttachGuard {
    pub pinned: bool,
    pub archived: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneManagementResult {
    pub operation_id: String,
    /// Snapshot at this operation's commit, including on a replay after later edits.
    pub lane: LaneRecord,
    pub changed: bool,
    pub replayed: bool,
    pub family_topology_revision: u64,
    /// Projection is a separate durable step, not part of management success.
    pub projection_pending: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneManagementError {
    StaleManagementRevision,
    OperationIdConflict,
    NotCurrentLead,
    HumanActionRequired,
    InvalidPolicyTransition,
    InvalidAttachTarget,
    LanePinned,
    LaneArchived,
    StaleFamilyRevision,
    OwnershipLost,
    StorageUnavailable,
    ProjectionPending,
    InvalidInput,
}
impl LaneManagementError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaleManagementRevision => "stale_management_revision",
            Self::OperationIdConflict => "operation_id_conflict",
            Self::NotCurrentLead => "not_current_lead",
            Self::HumanActionRequired => "human_action_required",
            Self::InvalidPolicyTransition => "invalid_policy_transition",
            Self::InvalidAttachTarget => "invalid_attach_target",
            Self::LanePinned => "lane_pinned",
            Self::LaneArchived => "lane_archived",
            Self::StaleFamilyRevision => "stale_family_revision",
            Self::OwnershipLost => "ownership_lost",
            Self::StorageUnavailable => "storage_unavailable",
            Self::ProjectionPending => "projection_pending",
            Self::InvalidInput => "invalid_input",
        }
    }
}
/// Storage-side CAS for a monitor's synthetic Failed. A superseded observation
/// is a no-op (None), not a failure report. Runtime wiring belongs to Phase 3B.
#[derive(Debug, Clone)]
pub struct LaneMonitorFailure {
    pub observation: crate::lane_control::ObserveLaneStall,
    pub episode_id: String,
    pub expected_episode_revision: u64,
    pub expected_report_seq: u64,
    pub report: crate::lanes::LaneReportInput,
}
impl crate::Database {
    pub fn lane_family_topology_revision(&self) -> Result<u64, StorageError> {
        crate::repo::lane_management::topology_revision(&self.conn)
    }
}
