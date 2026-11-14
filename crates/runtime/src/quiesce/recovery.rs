//! Authority-free, versioned graph evidence. Provider inputs remain in resume_intents.
use crate::{RunId, restore::RunRestoreDescriptor};
use serde::{Deserialize, Serialize};
/// Stable selector for CURRENT core configuration; never a stored capability.
/// PR07 capture must use this exact versioned selector.
pub const CORE_RECOVERY_SETTINGS_REFERENCE: &str = "evorch/core/current/v1";
pub use storage::recovery::{
    RecoveryClaim, RecoveryClaimState, RecoveryEdge, RecoveryManifestRecord as RecoveryManifest,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "owner", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResumePolicy {
    CoreOwned,
    CliInvocation {
        invocation_id: String,
        planned_handoff: Option<String>,
    },
    #[default]
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRecipe {
    /// A key for the host's CURRENT configuration, not a path to an old DB/permit.
    pub settings_reference: String,
    pub policy: ResumePolicy,
    pub goal_id: Option<String>,
    pub team: Option<crate::restore::TeamRestoreIdentity>,
    pub lane: Option<event_bus::LaneScope>,
    pub descriptor: Option<RunRestoreDescriptor>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableWaitMode {
    Any,
    All,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedGeneration {
    pub id: String,
    pub generation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitContinuation {
    pub tool_name: String,
    pub legacy_result: bool,
    pub consume_mailbox: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<WaitPayload>,
}
/// Original batch identity; never an instruction to redispatch a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitPayload {
    pub position: usize,
    pub input: serde_json::Value,
    pub kind: WaitKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitKind {
    Runs,
    Delegates {
        calls: Vec<DelegateWaitCall>,
    },
    Reply {
        message_id: String,
        deadline_unix_ms: u64,
        delivered_answer_ids: Vec<String>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateWaitCall {
    pub tool_call_id: String,
    pub child: RunId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableWait {
    pub requester: RunId,
    pub targets: Vec<RunId>,
    pub mode: DurableWaitMode,
    pub tool_call_id: String,
    pub completions: Vec<ObservedGeneration>,
    pub questions: Vec<ObservedGeneration>,
    pub answers: Vec<ObservedGeneration>,
    pub continuation: WaitContinuation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableDeliveryState {
    Held,
    Mailbox,
    Consumed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableNotice {
    Explicit,
    Completion {
        child: RunId,
        recipient: Option<RunId>,
        generation: u64,
        observed: bool,
    },
    Question {
        id: String,
        parent: RunId,
        child: RunId,
        blocking: bool,
        observed: bool,
        admitted: bool,
        paused: bool,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableDelivery {
    pub message_id: String,
    pub sender: RunId,
    pub recipient: RunId,
    pub sequence: u64,
    pub state: DurableDeliveryState,
    pub consumed_receipt: Option<String>,
    pub notice: DurableNotice,
    pub message: Option<event_bus::AgentMessage>,
}
/// A member references the intent row; it must never copy its provider request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecoveryRecord {
    pub version: u32,
    pub run_id: RunId,
    pub old_incarnation: String,
    pub intent_revision: u64,
    pub parent: Option<RunId>,
    pub root: RunId,
    pub thread: Option<String>,
    pub project: Option<std::path::PathBuf>,
    pub role: String,
    pub recipe: RecoveryRecipe,
    pub waits: Vec<DurableWait>,
    pub deliveries: Vec<DurableDelivery>,
    /// Exclusive runtime-wide high-watermark, including completed/expired receipts.
    /// Required: an older record without this evidence cannot be activated.
    pub next_message_id: u64,
    pub completion: Option<ObservedGeneration>,
    #[serde(default)]
    pub completion_generation: Option<u64>,
    pub auto_wakes: u32,
    pub user_reset_generation: u64,
    pub input_ids: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReason {
    PartialManifest,
    SupersededManifest,
    CorruptComponent,
    MissingEdge,
    UnknownSchema,
    NoCommittedManifest,
    PendingClaim,
    IntentRevisionConflict,
    MissingSnapshot,
    SnapshotConsumed,
    OperatorStopped,
    Cancelled,
    CliDisconnected,
    Terminal,
    UnknownOwnership,
    CliContinuityRequired,
    CurrentSettingsRequired,
    UnfinishedWaitUnsupportedIn06a,
    UnconfirmedEffects,
    AuthorityRenewalRequired,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredRecoveryAuthority {
    CurrentOwnership,
    CurrentConfiguration,
    CurrentTeamAndClaimFence,
    CliContinuity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryComponent {
    pub manifest_id: String,
    pub runs: Vec<RunId>,
    pub required_authority: Vec<RequiredRecoveryAuthority>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryOutcome {
    pub manifest_id: Option<String>,
    pub runs: Vec<RunId>,
    pub reason: RecoveryReason,
}
/// Payload-free diagnostics. 06a never populates recovered: activation is disabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecoveryReport {
    pub recovered: Vec<RunId>,
    pub skipped: Vec<RecoveryOutcome>,
    pub blocked: Vec<RecoveryOutcome>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecoveryPlan {
    pub components: Vec<RecoveryComponent>,
    pub report: RecoveryReport,
}
