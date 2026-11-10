use crate::RunId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum QuiesceState {
    #[default]
    Running,
    Quiescing,
    WaitingForSafePoints,
    Checkpointed,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    OperatorStopped,
    CliDisconnected,
    Cancelled,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ResumeWait {
    Admission,
    AdmissionQueued,
    Activity { activity: event_bus::RunActivity },
    SafePoint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableInput {
    pub id: String,
    pub text: String,
    pub images: Vec<crate::DelegateImage>,
    pub trusted_user: bool,
}

/// Exact provider input, after route/tool/image selection. Request metadata skipped
/// by ChatRequest's wire serde is stored separately. No credentials or clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInput {
    pub profile: String,
    pub protocol: model::ApiProtocol,
    pub edit_tool: config::EditToolConfig,
    pub affinity_key: String,
    pub request: providers::ChatRequest,
    pub replay: Option<providers::responses_replay::ResponsesReplayContext>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeIntent {
    pub version: u32,
    pub run_id: RunId,
    pub incarnation: String,
    pub parent_run_id: Option<RunId>,
    pub role: String,
    pub wait: ResumeWait,
    /// An unregistered run has not appended its starting input yet.
    pub initial_input: Option<DurableInput>,
    pub inputs: Vec<DurableInput>,
    pub active: bool,
    /// Deferred user Continue evidence, not authority to resume after a restart.
    pub resume_requested: bool,
    pub stop_reason: Option<StopReason>,
    pub checkpoint: Option<crate::checkpoint::RunCheckpoint>,
    pub snapshot: Option<crate::checkpoint::SnapshotReference>,
    pub tool_intent: Vec<crate::restore::InterruptedToolCall>,
    pub provider_input: Option<ProviderInput>,
    /// These are explicit replacement boundaries, never inferred from reconnect.
    pub boundary_pending: bool,
    pub responses_replay: crate::ResponsesReplayState,
    pub compactions: Vec<crate::CompactionCheckpoint>,
    pub route_pins: Vec<crate::RoutePinRecord>,
}

impl ResumeIntent {
    pub(crate) fn decode(row: storage::resume_intent::ResumeIntentRecord) -> Result<Self, String> {
        let intent: Self = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
        if intent.version != 1
            || intent.run_id.to_string() != row.run_id
            || intent.incarnation != row.incarnation
        {
            return Err("invalid resume intent identity/version".into());
        }
        Ok(intent)
    }
}
