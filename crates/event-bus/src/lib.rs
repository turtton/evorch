//! 型付きイベントストリームの内部配信基盤であり、tokio broadcast ベースで ADR 0012 の計測収集層の土台となります。

pub mod bus;
pub mod event;
pub mod orchestrator;
pub mod otel;
pub mod ring;
pub mod usage;

pub use bus::{EventBus, EventReceiver, RecvError};
pub use event::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, CompactionEvent,
    CompactionReason, DeliveryDisposition, EscalationMemoSummary, EscalationTrigger, Event,
    EventKind, EventMeta, FallbackAxis, FaultEvent, LifecycleEvent, MessageEvent, ProviderEvent,
    ProviderFailureKind, RoutingSource, SCHEMA_VERSION, SkillDiagnosticKind, ToolEvent, UsageEvent,
};
pub use orchestrator::{
    ApprovalDecision, CiState, CloseoutStep, CriterionCheck, CriterionStatus, GateEvidence,
    GateRejection, GateSnapshot, GoalReference, GoalStage, GoalState, InvalidationReason,
    MergeBinding, OrchestratorEvent, ReviewVerdict, RunPurpose, StallSignal, SuppressReason,
};
pub use otel::{
    ATTRIBUTE_WHITELIST, CardinalityViolation, MetricAttribute, MetricMeasurement, MetricValue,
    OPERATION_DURATION_METRIC, SECONDS_UNIT, SEMCONV_PIN, TIME_TO_FIRST_TOKEN_METRIC, TOKEN_UNIT,
    TOKEN_USAGE_METRIC, map_event, validate_metric_attributes,
};
pub use ring::RingBuffer;
pub use usage::{BucketKey, UsageAggregator, UsageBucket, UsageSink};
