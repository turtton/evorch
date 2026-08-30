//! 型付きイベントストリームの内部配信基盤であり、tokio broadcast ベースで ADR 0012 の計測収集層の土台となります。

pub mod bus;
pub mod event;
pub mod ring;
pub mod usage;

pub use bus::{EventBus, EventReceiver, RecvError};
pub use event::{
    AgentRunPhase, Event, EventKind, EventMeta, FaultEvent, LifecycleEvent, MessageEvent,
    ProviderEvent, ProviderFailureKind, SCHEMA_VERSION, ToolEvent, UsageEvent,
};
pub use ring::RingBuffer;
pub use usage::{BucketKey, UsageAggregator, UsageBucket, UsageSink};
