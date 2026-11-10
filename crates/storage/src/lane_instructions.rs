//! ADR 0044 §5: a lane's lead adds to the lane's conversation after its first brief.
//! A supplement leaves the brief alone; a brief revision replaces it and raises the brief
//! revision (the existing triggers then retire verification and close evidence). The
//! caller identity is never part of the deserializable request.
use crate::StorageError;
use event_bus::LaneBrief;
use serde::{Deserialize, Serialize};

/// A supplement is guidance, not a second brief: keep it short.
pub const LANE_SUPPLEMENT_MAX_BYTES: usize = 4096;
/// The whole presentation, including JSON escaping and the provenance envelope.
pub const LANE_INSTRUCTION_BODY_MAX_BYTES: usize = 32 * 1024;
/// States the origin in the text the lane model reads: this is not the user.
pub const LANE_INSTRUCTION_NOTICE: &str = "Agent-authored instruction from this lane's lead Orchestrator, not a real-user request, permission or verified completion. A supplement does not change the brief. A brief revision replaces the brief's objective, inputs, deliverables, acceptance criteria and constraints from now on. Workspace declarations grant no permissions.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaneInstructionAction {
    /// Guidance on the current brief; the brief and its revision stay as they are.
    Supplement { message: String },
    /// Whole replacement (never a patch) of the brief; `report_policy` must equal the
    /// lane's current policy.
    ReviseBrief { brief: LaneBrief },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneInstructionRequest {
    pub operation_id: String,
    pub lane_id: String,
    pub expected_management_revision: u64,
    pub expected_brief_revision: u64,
    pub action: LaneInstructionAction,
}

/// The authenticated sender, supplied by the runtime dispatcher, never by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneInstructionActor {
    pub thread_id: String,
    pub root_run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneInstructionResult {
    pub operation_id: String,
    pub lane_id: String,
    /// `None` when a brief revision changed nothing: no instruction was recorded.
    pub instruction_seq: Option<u64>,
    pub notification_id: Option<String>,
    pub accepted_management_revision: u64,
    /// The brief revision after this operation.
    pub brief_revision: u64,
    pub changed: bool,
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneInstructionDelivery {
    /// Durably accepted; the lane conversation has not stored it yet.
    Queued,
    /// The lane conversation snapshot holding the full text was saved.
    Delivered,
    /// The delivery was retired without reaching the lane.
    Withdrawn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneInstructionKind {
    Supplement,
    BriefRevision,
}
impl LaneInstructionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supplement => "supplement",
            Self::BriefRevision => "brief_revision",
        }
    }
}

/// The immutable text a lane receives. `body` is already the final presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneInstructionRecord {
    pub lane_id: String,
    pub instruction_seq: u64,
    pub kind: LaneInstructionKind,
    pub body: String,
    pub lead_thread_id: String,
    pub accepted_management_revision: u64,
    pub brief_revision_before: u64,
    pub brief_revision_after: u64,
    pub notification_id: String,
    pub created_at_ns: i64,
}

pub(crate) struct Presentation<'a> {
    pub seq: u64,
    pub lead_thread_id: &'a str,
    pub management_revision: u64,
    pub brief_revision_before: u64,
    pub brief_revision_after: u64,
    pub action: &'a LaneInstructionAction,
    pub stored_brief: &'a LaneBrief,
}
impl Presentation<'_> {
    /// One JSON line: escaping keeps the sender's text from forging the envelope.
    pub(crate) fn render(&self) -> Result<String, StorageError> {
        let mut value = serde_json::json!({
            "notice": LANE_INSTRUCTION_NOTICE,
            "instruction_seq": self.seq,
            "from_lead_thread_id": self.lead_thread_id,
            "management_revision": self.management_revision,
        });
        match self.action {
            LaneInstructionAction::Supplement { message } => {
                value["kind"] = "supplement".into();
                value["brief_revision"] = self.brief_revision_after.into();
                value["message"] = message.as_str().into();
            }
            LaneInstructionAction::ReviseBrief { .. } => {
                value["kind"] = "brief_revision".into();
                value["previous_brief_revision"] = self.brief_revision_before.into();
                value["brief_revision"] = self.brief_revision_after.into();
                value["brief"] = serde_json::to_value(self.stored_brief)
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
            }
        }
        let body = serde_json::to_string(&serde_json::json!({ "lane_instruction": value }))
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        if body.len() > LANE_INSTRUCTION_BODY_MAX_BYTES {
            return Err(StorageError::LaneManagement(
                crate::lane_management::LaneManagementError::InvalidInput,
            ));
        }
        Ok(body)
    }
}
