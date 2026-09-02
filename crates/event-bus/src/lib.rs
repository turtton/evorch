//! 型付きイベントストリームの内部配信基盤であり、tokio broadcast ベースで ADR 0012 の計測収集層の土台となります。

pub mod bus;
pub mod event;
pub mod otel;
pub mod ring;
pub mod usage;

pub use bus::{EventBus, EventReceiver, RecvError};
pub use event::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, DeliveryDisposition, Event,
    EventKind, EventMeta, FallbackAxis, FaultEvent, LifecycleEvent, MessageEvent, ProviderEvent,
    ProviderFailureKind, SCHEMA_VERSION, SkillDiagnosticKind, ToolEvent, UsageEvent,
};
pub use otel::{
    ATTRIBUTE_WHITELIST, CardinalityViolation, MetricAttribute, MetricMeasurement, MetricValue,
    SEMCONV_PIN, map_event, validate_metric_attributes,
};
pub use ring::RingBuffer;
pub use usage::{BucketKey, UsageAggregator, UsageBucket, UsageSink};
