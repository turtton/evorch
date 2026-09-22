//! 永続化するエンティティを定義します。
use std::time::SystemTime;

use event_bus::{
    AgentMessageEvent, CompactionEvent, EventKind, LifecycleEvent, MessageEvent, ProviderEvent,
    ToolEvent,
};

use crate::error::StorageError;

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        impl $name {
            /// SQLite に保存する文字列表現を返します。
            #[must_use]
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $value,)+
                }
            }

            /// SQLite の文字列表現から値を復元します。
            #[must_use]
            #[allow(
                clippy::should_implement_trait,
                reason = "the storage contract requires this Option-returning inherent method"
            )]
            pub fn from_str(value: &str) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = ();

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_str(value).ok_or(())
            }
        }
    };
}

/// セッションの永続化状態です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// 実行中です。
    Running,
    /// 別エージェントへ委譲済みです。
    Delegated,
    /// 正常に完了しました。
    Completed,
    /// 失敗しました。
    Failed,
}

string_enum!(SessionStatus {
    Running => "running",
    Delegated => "delegated",
    Completed => "completed",
    Failed => "failed",
});

#[path = "entity/task.rs"]
mod task;
pub use task::{TaskContinuation, TaskRecord, TaskStatus};

/// エージェント実行の永続化状態です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunStatus {
    /// 実行中です。
    Running,
    /// 正常に完了しました。
    Completed,
    /// 失敗しました。
    Failed,
}

string_enum!(AgentRunStatus {
    Running => "running",
    Completed => "completed",
    Failed => "failed",
});

/// メッセージの送信主体です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// ユーザーからのメッセージです。
    User,
    /// アシスタントからのメッセージです。
    Assistant,
    /// システムからのメッセージです。
    System,
    /// ツールからのメッセージです。
    Tool,
}

string_enum!(MessageRole {
    User => "user",
    Assistant => "assistant",
    System => "system",
    Tool => "tool",
});

/// セッションの永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// セッション識別子です。
    pub id: String,
    /// 親セッション識別子です。
    pub parent_id: Option<String>,
    /// セッション状態です。
    pub status: SessionStatus,
    /// 失敗理由です。
    pub failure_reason: Option<String>,
    /// 委譲先です。
    pub delegated_to: Option<String>,
    /// 保存済みイベントの累積バイト数です。
    pub total_event_bytes: u64,
    /// 作成日時です。
    pub created_at: SystemTime,
    /// 更新日時です。
    pub updated_at: SystemTime,
}

/// メッセージの永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    /// メッセージ識別子です。
    pub id: String,
    /// 所属セッション識別子です。
    pub session_id: String,
    /// 送信主体です。
    pub role: MessageRole,
    /// メッセージ本文です。
    pub content: String,
    /// 推論内容です。
    pub reasoning: Option<String>,
    /// 作成日時です。
    pub created_at: SystemTime,
    /// 更新日時です。
    pub updated_at: SystemTime,
}

/// エージェント実行の永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunRecord {
    /// 実行識別子です。
    pub id: String,
    /// 所属セッション識別子です。
    pub session_id: String,
    /// プロバイダー名です。
    pub provider: String,
    /// モデル名です。
    pub model: String,
    /// 実行状態です。
    pub status: AgentRunStatus,
    /// 開始日時です。
    pub started_at: SystemTime,
    /// 終了日時です。
    pub finished_at: Option<SystemTime>,
}

/// カタログ更新の永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogUpdateRecord {
    /// 更新元を識別する文字列です。
    pub source: String,
    /// 更新後のモデル数です。
    pub model_count: u32,
    /// 更新の詳細です。
    pub detail: String,
    /// 更新を記録した Unix epoch ナノ秒です。
    pub recorded_at_ns: i64,
}

/// Sanitize persisted tool payloads without changing the live event or protocol identifiers.
pub(crate) fn redact_tool_event(event: &event_bus::Event) -> event_bus::Event {
    let mut persisted = event.clone();
    let redactor = secret_guard::SecretRedactor::from_env();
    match &mut persisted.kind {
        EventKind::Tool(
            ToolEvent::ToolStarted { input, .. } | ToolEvent::ApprovalRequested { input, .. },
        ) => {
            if let Some(input) = input {
                redactor.redact_json(input);
            }
        }
        EventKind::Tool(ToolEvent::ToolCompleted { output, detail, .. }) => {
            if let Some(output) = output {
                *output = redactor.redact(output).text;
            }
            if let Some(detail) = detail {
                redactor.redact_json(detail);
            }
        }
        _ => {}
    }
    persisted
}

/// Fail-closed checks for entities that do not support redacted persistence.
#[derive(Debug)]
pub(crate) struct SecretGuard {
    redactor: secret_guard::SecretRedactor,
}

impl SecretGuard {
    pub(crate) fn from_env() -> Self {
        Self {
            redactor: secret_guard::SecretRedactor::from_env(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_known_values(values: impl IntoIterator<Item = String>) -> Self {
        Self {
            redactor: secret_guard::SecretRedactor::with_known_values(values),
        }
    }

    /// メッセージレコードの `content` / `reasoning` が永続化可能か検査します。
    pub(crate) fn check_message_record(&self, record: &MessageRecord) -> Result<(), StorageError> {
        self.check_text("message", "content", &record.content)?;
        if let Some(reasoning) = &record.reasoning {
            self.check_text("message", "reasoning", reasoning)?;
        }
        Ok(())
    }

    /// 永続化対象イベントの human-readable text（`MessageDelta` / `ReasoningDelta` の
    /// `delta` と `reason` 系 field）が serialize / INSERT 可能か検査します。
    pub(crate) fn check_event_kind(&self, kind: &EventKind) -> Result<(), StorageError> {
        // reason / delta 系の自由文字列 field を明示列挙する。新しい text field を持つ
        // variant が event-bus へ追加されたらここへも検査を追加すること。
        match kind {
            EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) => {
                let payload = serde_json::to_string(question)
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                self.check_text("event", "UserQuestionUpdated", &payload)
            }
            EventKind::Ledger(event_bus::LedgerEvent::RunLedgerAppended { body, .. }) => {
                self.check_text("event", "RunLedgerAppended.body", body)
            }
            EventKind::Ownership(event) => {
                let payload = serde_json::to_string(event)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?;
                self.check_text("event", "Ownership.payload", &payload)
            }
            EventKind::Snapshot(event) => {
                let payload = serde_json::to_string(event)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?;
                self.check_text("event", "Snapshot.payload", &payload)
            }
            EventKind::Diagnostic(event) => {
                let payload = serde_json::to_string(event)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?;
                self.check_text("event", "Diagnostic.payload", &payload)
            }
            EventKind::Lifecycle(LifecycleEvent::Failed { reason, .. }) => {
                self.check_text("event", "Failed.reason", reason)
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                reason: Some(reason),
                ..
            }) => self.check_text("event", "AgentRunStateChanged.reason", reason),
            EventKind::Lifecycle(LifecycleEvent::RoutingDecision { reason, .. }) => {
                self.check_text("event", "RoutingDecision.reason", reason)
            }
            EventKind::Message(MessageEvent::MessageDelta { delta, .. }) => {
                self.check_text("event", "MessageDelta.delta", delta)
            }
            EventKind::Message(MessageEvent::ReasoningDelta { delta, .. }) => {
                self.check_text("event", "ReasoningDelta.delta", delta)
            }
            EventKind::Tool(ToolEvent::ExecutionDenied { reason, .. }) => {
                self.check_text("event", "ExecutionDenied.reason", reason)
            }
            EventKind::Provider(ProviderEvent::ProviderFallback { reason, .. }) => {
                self.check_text("event", "ProviderFallback.reason", reason)
            }
            EventKind::Provider(ProviderEvent::RequestCompleted { finish_reason, .. }) => {
                // FinishReason::Other は provider 由来の任意文字列を保持し得る
                // （providers::observe::emit_completed）。
                self.check_text("event", "RequestCompleted.finish_reason", finish_reason)
            }
            // AgentMessage の本文は MessageDelta と同様の自由文字列であり、
            // bus / storage へ流れるため fail-closed で走査する。
            EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => {
                self.check_text("event", "AgentMessage.content", &message.content)
            }
            // Compaction の summary は会話由来の自由文字列であり、
            // checkpoint / run の識別子も同様に fail-closed で走査する。
            EventKind::Compaction(CompactionEvent::Compacted {
                run_id,
                checkpoint_id,
                summary,
                ..
            }) => {
                self.check_text("event", "Compacted.summary", summary)?;
                self.check_text("event", "Compacted.checkpoint_id", checkpoint_id)?;
                self.check_text("event", "Compacted.run_id", run_id)
            }
            EventKind::Lifecycle(_)
            | EventKind::Tool(_)
            | EventKind::Provider(_)
            | EventKind::Usage(_)
            | EventKind::Fault(_) => Ok(()),
            // Orchestrator は goal 本文・findings・detail 等の自由文字列を
            // payload 全体で保持するため、serialize 結果ごと fail-closed で
            // 走査する。
            EventKind::Orchestrator(event) => {
                let payload = serde_json::to_string(event)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?;
                self.check_text("event", "Orchestrator.payload", &payload)
            }
        }
    }

    pub(crate) fn check_text(
        &self,
        entity: &'static str,
        field: &'static str,
        text: &str,
    ) -> Result<(), StorageError> {
        match self.redactor.detect(text) {
            Some(rule) => Err(StorageError::SecretDetected {
                entity,
                field,
                rule,
            }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{CompactionReason, FaultEvent};

    const KNOWN_VALUE: &str = "evorch-known-credential-fixture-value-0123456789";

    #[test]
    fn event_check_covers_reason_and_delta_fields_and_skips_typed_only_variants() {
        // Given: 既知値を注入した guard と各イベント variant
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned()]);
        let cases: [(&str, EventKind); 7] = [
            (
                "Failed.reason",
                LifecycleEvent::Failed {
                    session_id: "s".into(),
                    reason: format!("boom {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "AgentRunStateChanged.reason",
                LifecycleEvent::AgentRunStateChanged {
                    run_id: "r".into(),
                    from: event_bus::AgentRunPhase::Running,
                    to: event_bus::AgentRunPhase::Error,
                    reason: Some(format!("die {KNOWN_VALUE}")),
                }
                .into(),
            ),
            (
                "MessageDelta.delta",
                MessageEvent::MessageDelta {
                    delta: format!("out {KNOWN_VALUE}"),
                    run_id: None,
                }
                .into(),
            ),
            (
                "ReasoningDelta.delta",
                MessageEvent::ReasoningDelta {
                    delta: format!("think {KNOWN_VALUE}"),
                    run_id: None,
                }
                .into(),
            ),
            (
                "ExecutionDenied.reason",
                ToolEvent::ExecutionDenied {
                    tool_name: "t".into(),
                    call_id: "c".into(),
                    reason: format!("deny {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "ProviderFallback.reason",
                ProviderEvent::ProviderFallback {
                    from_provider: "a".into(),
                    to_provider: "b".into(),
                    reason: format!("flip {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "RequestCompleted.finish_reason",
                ProviderEvent::RequestCompleted {
                    request_id: "q".into(),
                    provider: "p".into(),
                    profile: None,
                    protocol: "proto".into(),
                    model: "m".into(),
                    streaming: true,
                    duration_ms: 1,
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_tokens: 3,
                    cache_write_tokens: 4,
                    finish_reason: format!("other {KNOWN_VALUE}"),
                    run_id: None,
                }
                .into(),
            ),
        ];

        // When / Then: 各 field 名付きで拒否される
        for (field, kind) in cases {
            let Err(StorageError::SecretDetected {
                entity,
                field: actual,
                ..
            }) = guard.check_event_kind(&kind)
            else {
                panic!("field {field} must be rejected");
            };
            assert_eq!(entity, "event");
            assert_eq!(actual, field);
        }

        // And: 型付き分類のみの variant と reason 未設定の variant は検査対象外
        let skipped: [EventKind; 2] = [
            FaultEvent::SubscriberLagged {
                subscriber_id: 1,
                skipped: 2,
            }
            .into(),
            LifecycleEvent::AgentRunStateChanged {
                run_id: "r".into(),
                from: event_bus::AgentRunPhase::Running,
                to: event_bus::AgentRunPhase::Running,
                reason: None,
            }
            .into(),
        ];
        for kind in skipped {
            guard
                .check_event_kind(&kind)
                .expect("typed-only variant must pass");
        }
    }

    #[test]
    fn event_check_rejects_secret_in_compaction_fields() {
        // Given: 既知値を注入した guard と Compaction イベント
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned()]);
        let cases: [(&str, EventKind); 3] = [
            (
                "Compacted.summary",
                CompactionEvent::Compacted {
                    run_id: "r".into(),
                    reason: CompactionReason::Automatic,
                    threshold: 0.8,
                    context_window_tokens: 100,
                    window_source: event_bus::WindowSource::Default,
                    estimated_tokens_before: 90,
                    estimated_tokens_after: 30,
                    compacted_range_start: 0,
                    compacted_range_end: 3,
                    checkpoint_id: "cp".into(),
                    summary: format!("chat {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "Compacted.checkpoint_id",
                CompactionEvent::Compacted {
                    run_id: "r".into(),
                    reason: CompactionReason::Automatic,
                    threshold: 0.8,
                    context_window_tokens: 100,
                    window_source: event_bus::WindowSource::Default,
                    estimated_tokens_before: 90,
                    estimated_tokens_after: 30,
                    compacted_range_start: 0,
                    compacted_range_end: 3,
                    checkpoint_id: format!("cp {KNOWN_VALUE}"),
                    summary: "safe".into(),
                }
                .into(),
            ),
            (
                "Compacted.run_id",
                CompactionEvent::Compacted {
                    run_id: format!("run {KNOWN_VALUE}"),
                    reason: CompactionReason::Automatic,
                    threshold: 0.8,
                    context_window_tokens: 100,
                    window_source: event_bus::WindowSource::Default,
                    estimated_tokens_before: 90,
                    estimated_tokens_after: 30,
                    compacted_range_start: 0,
                    compacted_range_end: 3,
                    checkpoint_id: "cp".into(),
                    summary: "safe".into(),
                }
                .into(),
            ),
        ];

        // When / Then: 各 field 名付きで拒否される
        for (field, kind) in cases {
            let Err(StorageError::SecretDetected {
                entity,
                field: actual,
                ..
            }) = guard.check_event_kind(&kind)
            else {
                panic!("field {field} must be rejected");
            };
            assert_eq!(entity, "event");
            assert_eq!(actual, field);
        }
    }

    #[test]
    fn routing_decision_reason_is_secret_guarded() {
        // Given: 既知値を注入した guard と reason へ既知 credential 値を含む
        //        RoutingDecision イベント
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned()]);
        let kind = EventKind::Lifecycle(LifecycleEvent::RoutingDecision {
            shape: "Direct".into(),
            reason: format!("matched {KNOWN_VALUE}"),
            source: event_bus::RoutingSource::LocalRule {
                rule: "direct-keyword:direct".into(),
            },
        });

        // When / Then: "RoutingDecision.reason" の field 名付きで拒否される
        let Err(StorageError::SecretDetected {
            entity,
            field: actual,
            ..
        }) = guard.check_event_kind(&kind)
        else {
            panic!("RoutingDecision.reason must be rejected");
        };
        assert_eq!(entity, "event");
        assert_eq!(actual, "RoutingDecision.reason");
    }

    #[test]
    fn message_record_check_reports_content_and_reasoning_fields() {
        // Given: guard と本文/推論へ既知値を含むレコード
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned()]);
        let base = MessageRecord {
            id: "m".into(),
            session_id: "s".into(),
            role: MessageRole::Assistant,
            content: "safe".into(),
            reasoning: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        };

        // When / Then: content / reasoning それぞれの field 名で拒否される
        let bad_content = MessageRecord {
            content: format!("say {KNOWN_VALUE}"),
            ..base.clone()
        };
        let Err(StorageError::SecretDetected { entity, field, .. }) =
            guard.check_message_record(&bad_content)
        else {
            panic!("content must be rejected");
        };
        assert_eq!((entity, field), ("message", "content"));

        let bad_reasoning = MessageRecord {
            reasoning: Some(format!("why {KNOWN_VALUE}")),
            ..base
        };
        let Err(StorageError::SecretDetected { entity, field, .. }) =
            guard.check_message_record(&bad_reasoning)
        else {
            panic!("reasoning must be rejected");
        };
        assert_eq!((entity, field), ("message", "reasoning"));
    }
}
