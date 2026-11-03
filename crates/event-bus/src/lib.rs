//! 型付きイベントストリームの内部配信基盤であり、tokio broadcast ベースで ADR 0012 の計測収集層の土台となります。

pub mod artifact;
pub use artifact::{ArtifactPresentation, PresentedArtifact};
pub mod bus;
mod message_parts;
pub use message_parts::MessagePartAssembler;
mod fencing;
pub use fencing::{
    MutationBatchError, MutationBatchGuard, MutationCheck, MutationGuard, MutationGuardAttempt,
    MutationGuardCheck, MutationGuardTryCheck, MutationValidator,
};
pub mod event;
pub mod lane;
pub use lane::{
    LANE_ARTIFACT_REFS_MAX, LANE_BRIEF_ITEM_MAX_BYTES, LANE_BRIEF_ITEMS_MAX, LANE_BRIEF_MAX_BYTES,
    LANE_LIST_PAGE_MAX, LANE_LIST_PAGE_MAX_BYTES, LANE_OBJECTIVE_MAX_BYTES, LANE_QUESTION_REFS_MAX,
    LANE_REASON_CODE_MAX_BYTES, LANE_REF_MAX_BYTES, LANE_REPORT_MAX_BYTES, LANE_SUMMARY_MAX_BYTES,
    LANE_WORKSPACE_FIELD_MAX_BYTES, LANE_WORKSPACE_NOTES_MAX_BYTES, LaneBrief, LaneInput,
    LaneInputKind, LanePolicy, LaneReport, LaneReportInput, LaneReportProducer, LaneScope,
    LaneStatus, LaneValidationError, LaneValidationKind, LaneWorkspaceDeclaration,
};
pub mod orchestrator;
pub mod ownership;
pub use ownership::{OwnershipAction, OwnershipEvent};
pub mod otel;
pub mod ring;
pub mod thread_goal;
pub mod thread_todo;
pub use thread_todo::{ThreadTodoItem, ThreadTodoSnapshot, ThreadTodoStatus};
pub mod usage;
pub use thread_goal::{ThreadGoalCheck, ThreadGoalPhase, ThreadGoalSnapshot, ThreadGoalUsage};

pub use bus::{EventBus, EventReceiver, RecvError};
pub use event::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, AnswerOrigin,
    CACHE_RETENTION_WARNING_THRESHOLD, CANCELLED_REASON, COMPACTION_DELTA_MAX_BYTES,
    CacheBaselineMissing, CacheComparison, CompactionEvent, CompactionMode, CompactionReason,
    ContextComposition, DeliveryDisposition, DiagnosticEvent, DiagnosticSeverity,
    EscalationMemoSummary, EscalationTrigger, Event, EventKind, EventMeta, FallbackAxis,
    FaultEvent, LedgerEvent, LifecycleEvent, MessageEvent, ProviderEvent, ProviderFailureKind,
    REQUEST_FAILED_DETAIL_MAX_BYTES, RequestPurpose, RoutingSource, RunActivity, SCHEMA_VERSION,
    ServiceTier, SkillDiagnosticKind, ToolEvent, UsageEvent, UserQuestion, WindowSource,
};
pub use orchestrator::OrchestratorEvent;
pub use otel::{
    ATTRIBUTE_WHITELIST, CardinalityViolation, MetricAttribute, MetricMeasurement, MetricValue,
    OPERATION_DURATION_METRIC, SECONDS_UNIT, SEMCONV_PIN, TIME_TO_FIRST_TOKEN_METRIC, TOKEN_UNIT,
    TOKEN_USAGE_METRIC, map_event, validate_metric_attributes,
};
pub use ring::RingBuffer;
pub use usage::{BucketKey, UsageAggregator, UsageBucket, UsageSink};

/// Stable conversation identity shared by the runtime and escalation projections.
pub fn escalation_thread_id(new_run_id: &str) -> String {
    format!("escalation-{new_run_id}")
}
