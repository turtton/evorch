//! Lane management wire contract. Ingress supplies the actor separately. These
//! values do not change a running run's tools, prompt prefix or ownership.
use crate::validation::ValidationError;
use crate::{
    ids::{RunId, ThreadId},
    lane_attention::LaneId,
    wire::{Revision, Sequence},
};
use serde::{Deserialize, Serialize};
/// Durable, globally unique operation identity, independent of a connection/request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LaneManagementOperationId(String);
impl LaneManagementOperationId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let v = value.into();
        crate::validation::check_line("operation_id", &v, 256, false)?;
        Ok(Self(v))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for LaneManagementOperationId {
    type Error = ValidationError;
    fn try_from(v: String) -> Result<Self, Self::Error> {
        Self::new(v)
    }
}
impl From<LaneManagementOperationId> for String {
    fn from(v: LaneManagementOperationId) -> Self {
        v.0
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneManagementRequest {
    pub operation_id: LaneManagementOperationId,
    pub lane_id: LaneId,
    pub expected_management_revision: Revision,
    pub action: LaneManagementAction,
}
/// A human management intent. Reattach is the sidebar D&D path and requires
/// the independent family's revision observed when the drag began.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManageLane {
    pub request: LaneManagementRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_family_revision: Option<LaneFamilyRevision>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneFamilyRevision {
    pub key: ThreadId,
    pub membership_epoch: Revision,
    pub archive_revision: Revision,
    pub pin_revision: Revision,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneManagementState {
    pub lane_id: LaneId,
    pub thread_id: ThreadId,
    pub lead_thread_id: Option<ThreadId>,
    pub policy: LaneManagementPolicy,
    pub management_revision: Revision,
    pub detached_since_management_revision: Option<Revision>,
    /// Available for a detached lane when authoritative family state is readable.
    pub family_revision: Option<LaneFamilyRevision>,
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
    /// Host validates the sidebar family revision, pin/archive and project under
    /// its authoritative lock. No family revision is sent to SQL storage.
    Reattach {
        lead_thread_id: ThreadId,
        expected_lead_root_run_id: RunId,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedLanePolicy {
    Attached,
    Summary,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneManagementPolicy {
    Attached,
    Summary,
    Detached,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneManagementResult {
    pub operation_id: LaneManagementOperationId,
    pub lane_id: LaneId,
    pub thread_id: ThreadId,
    pub lead_thread_id: Option<ThreadId>,
    pub policy: LaneManagementPolicy,
    pub management_revision: Revision,
    /// Revision at which this independent family began; None while managed.
    pub detached_since_management_revision: Option<Revision>,
    pub family_topology_revision: Revision,
    pub changed: bool,
    pub replayed: bool,
    pub projection_pending: bool,
}
/// How a person ends a lane's ledger entry (ADR 0044 §9). Neither stops a run, deletes a
/// thread or worktree, nor detaches the lane: those are separate operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanLaneCloseKind {
    /// The person judged the lane done, with no verification behind it
    /// (recorded as `human_closed`).
    Complete,
    /// The person ended the lane without claiming it is done (recorded as `discarded`).
    Discard,
}

/// A person's request to end a lane's ledger entry. There is no field for who decided: the
/// core records the surface and client its ingress authenticated, and refuses an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseLane {
    pub lane_id: LaneId,
    pub kind: HumanLaneCloseKind,
    /// The Ready the person looked at, if any. A newer report makes the request stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_seq: Option<Sequence>,
    /// The revisions the person's screen showed; a lane that moved since is refused as
    /// stale and nothing is recorded.
    pub expected_management_revision: Revision,
    pub expected_request_revision: Revision,
    /// Why (required, at most 2 KiB).
    pub note: String,
}

impl CloseLane {
    pub fn validate(&self) -> Result<(), ValidationError> {
        crate::validation::check_text("note", &self.note, MAX_CLOSE_NOTE_BYTES, false)?;
        if self.note.trim().is_empty() {
            return Err(ValidationError::Empty { field: "note" });
        }
        if self.ready_seq == Some(Sequence(0)) {
            return Err(ValidationError::OutOfRange {
                field: "ready_seq",
                reason: "report sequences start at 1",
            });
        }
        Ok(())
    }
}

/// Longest note a close accepts, in bytes.
pub const MAX_CLOSE_NOTE_BYTES: usize = 2048;

/// How a lane's close was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneCloseRecordKind {
    LeadClosed,
    HumanClosed,
    Discarded,
}

/// The close that stands after a [`CloseLane`]. When another close already stood for these
/// revisions, that earlier record is returned and `already_closed` is true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneCloseResult {
    pub lane_id: LaneId,
    pub close_id: String,
    pub kind: LaneCloseRecordKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_seq: Option<Sequence>,
    pub management_revision: Revision,
    pub request_revision: Revision,
    pub already_closed: bool,
}
/// Stable mapping for adapters; no storage/runtime error types cross the wire.
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_and_epoch_round_trip_without_javascript_precision_loss() {
        let result = LaneManagementResult {
            operation_id: LaneManagementOperationId::new("op").unwrap(),
            lane_id: LaneId::new("lane").unwrap(),
            thread_id: ThreadId::new("thread").unwrap(),
            lead_thread_id: None,
            policy: LaneManagementPolicy::Detached,
            management_revision: Revision(u64::MAX),
            detached_since_management_revision: Some(Revision(u64::MAX)),
            family_topology_revision: Revision(u64::MAX),
            changed: true,
            replayed: false,
            projection_pending: true,
        };
        let value = serde_json::to_value(&result).unwrap();
        for key in [
            "management_revision",
            "detached_since_management_revision",
            "family_topology_revision",
        ] {
            assert_eq!(value[key], u64::MAX.to_string());
        }
        assert_eq!(
            serde_json::from_value::<LaneManagementResult>(value).unwrap(),
            result
        );
    }
    #[test]
    fn a_close_request_has_no_field_for_who_decided_and_requires_a_reason() {
        let request = serde_json::json!({"lane_id":"lane","kind":"complete","ready_seq":"2","expected_management_revision":"1","expected_request_revision":"3","note":"done by hand"});
        let close: CloseLane = serde_json::from_value(request.clone()).unwrap();
        assert!(close.validate().is_ok());
        for forged in ["surface", "client_id", "closed_by", "actor", "provenance"] {
            let mut bad = request.clone();
            bad[forged] = serde_json::json!("web");
            assert!(
                serde_json::from_value::<CloseLane>(bad).is_err(),
                "{forged} must be refused, not ignored"
            );
        }
        let mut kind = request.clone();
        kind["kind"] = serde_json::json!("lead_closed");
        assert!(serde_json::from_value::<CloseLane>(kind).is_err());
        for note in ["", "  \n\t", &"x".repeat(MAX_CLOSE_NOTE_BYTES + 1)] {
            let mut bad = close.clone();
            bad.note = note.into();
            assert!(bad.validate().is_err(), "{note:?}");
        }
        let mut zero = close;
        zero.ready_seq = Some(Sequence(0));
        assert!(zero.validate().is_err());
    }
    #[test]
    fn ingress_rejects_actor_injection_detached_policy_and_unbounded_id() {
        let request = serde_json::json!({"operation_id":"op","lane_id":"lane","expected_management_revision":"1","action":{"type":"set_policy","payload":{"policy":"summary"}}});
        assert!(serde_json::from_value::<LaneManagementRequest>(request.clone()).is_ok());
        let mut bad = request.clone();
        bad["actor"] = serde_json::json!("user");
        assert!(serde_json::from_value::<LaneManagementRequest>(bad).is_err());
        let mut bad = request;
        bad["action"]["payload"]["policy"] = serde_json::json!("detached");
        assert!(serde_json::from_value::<LaneManagementRequest>(bad).is_err());
        assert!(LaneManagementOperationId::new("x".repeat(256)).is_ok());
        for bad in ["".into(), "x".repeat(257), "op\n".into()] {
            assert!(LaneManagementOperationId::new(bad).is_err());
        }
    }
}
