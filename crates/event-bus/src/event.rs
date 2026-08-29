//! イベント型と serde スキーマを定義するモジュールです。
// allow: SIZE_OK - 本ファイルのみ編集可能というタスク制約と全バリアント網羅の
// 表駆動テスト要件により分割不可能。生産コード単体では約156純LOC。

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

/// イベントスキーマのバージョン。
pub const SCHEMA_VERSION: u32 = 1;

/// プロセス全体で共有する単調時計のアンカー。初回参照時に遅延初期化される。
static MONOTONIC_ANCHOR: OnceLock<Instant> = OnceLock::new();

/// イベント発生時刻に関するメタ情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventMeta {
    /// このイベントが従うスキーマバージョン。
    pub schema_version: u32,
    /// プロセス内アンカーからの経過時間。同一プロセス内での比較のみ意味を持つ。
    pub monotonic: Duration,
    /// 壁時計による発生時刻。
    pub wall_clock: SystemTime,
}

impl EventMeta {
    /// 両方の時計を現在時刻でスタンプする。
    ///
    /// `monotonic` はプロセス全体で共有される `OnceLock<Instant>` アンカーからの
    /// 経過時間（遅延初期化）であり、同一プロセス内でのみ意味を持つ。
    pub fn now() -> Self {
        let anchor = MONOTONIC_ANCHOR.get_or_init(Instant::now);
        Self {
            schema_version: SCHEMA_VERSION,
            monotonic: anchor.elapsed(),
            wall_clock: SystemTime::now(),
        }
    }
}

/// イベントバス上を流れるイベント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// 発生時刻のメタ情報。
    pub meta: EventMeta,
    /// イベントの種別とペイロード。
    pub kind: EventKind,
}

impl Event {
    /// 現在時刻のメタ情報をスタンプしたイベントを生成する。
    pub fn new(kind: impl Into<EventKind>) -> Self {
        Self {
            meta: EventMeta::now(),
            kind: kind.into(),
        }
    }
}

/// イベントの大分類。隣接タグ形式（`kind` / `payload` キー）でシリアライズされる。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum EventKind {
    /// ライフサイクル関連のイベント。
    Lifecycle(LifecycleEvent),
    /// メッセージストリーム関連のイベント。
    Message(MessageEvent),
    /// ツール実行関連のイベント。
    Tool(ToolEvent),
    /// トークン使用量関連のイベント。
    Usage(UsageEvent),
    /// プロバイダ切替関連のイベント。
    Provider(ProviderEvent),
    /// 障害関連のイベント。
    Fault(FaultEvent),
}

impl From<LifecycleEvent> for EventKind {
    fn from(event: LifecycleEvent) -> Self {
        Self::Lifecycle(event)
    }
}

impl From<MessageEvent> for EventKind {
    fn from(event: MessageEvent) -> Self {
        Self::Message(event)
    }
}

impl From<ToolEvent> for EventKind {
    fn from(event: ToolEvent) -> Self {
        Self::Tool(event)
    }
}

impl From<UsageEvent> for EventKind {
    fn from(event: UsageEvent) -> Self {
        Self::Usage(event)
    }
}

impl From<ProviderEvent> for EventKind {
    fn from(event: ProviderEvent) -> Self {
        Self::Provider(event)
    }
}

impl From<FaultEvent> for EventKind {
    fn from(event: FaultEvent) -> Self {
        Self::Fault(event)
    }
}

/// エージェント実行のライフサイクル位相。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRunPhase {
    /// 実行開始を待機中です。
    Pending,
    /// 実行中です。
    Running,
    /// 外部要因（ツール結果や入力）の到着を待機中です。
    Waiting,
    /// 正常終了しました。
    Done,
    /// 異常終了しました。
    Error,
}

/// セッションおよびタスクのライフサイクルに関するイベント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum LifecycleEvent {
    /// セッションが開始した。
    Started {
        /// 開始したセッションの ID。
        session_id: String,
    },
    /// 処理を別セッションへ委譲した。
    Delegated {
        /// 委譲元のセッション ID。
        session_id: String,
        /// 委譲先の識別子。
        target: String,
    },
    /// バックグラウンドタスクを開始した。
    BackgroundTaskStarted {
        /// 開始したタスクの ID。
        task_id: String,
    },
    /// バックグラウンドタスクが完了した。
    BackgroundTaskCompleted {
        /// 完了したタスクの ID。
        task_id: String,
    },
    /// バックグラウンドタスクがキャンセルされた。
    BackgroundTaskCancelled {
        /// キャンセルされたタスクの ID。
        task_id: String,
    },
    /// エージェント実行の位相が遷移した。
    AgentRunStateChanged {
        /// 状態が変化した実行の ID。
        run_id: String,
        /// 遷移前の位相。
        from: AgentRunPhase,
        /// 遷移後の位相。
        to: AgentRunPhase,
        /// 遷移理由。`to` が [`AgentRunPhase::Error`] のときに設定されます。
        reason: Option<String>,
    },
    /// セッションが完了した。
    Completed {
        /// 完了したセッションの ID。
        session_id: String,
    },
    /// セッションが失敗した。
    Failed {
        /// 失敗したセッションの ID。
        session_id: String,
        /// 失敗理由。
        reason: String,
    },
}

/// メッセージストリームに関するイベント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum MessageEvent {
    /// 応答テキストの差分。
    MessageDelta {
        /// 追加されたテキスト。
        delta: String,
    },
    /// 推論テキストの差分。
    ReasoningDelta {
        /// 追加された推論テキスト。
        delta: String,
    },
}

/// ツール実行に関するイベント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ToolEvent {
    /// ツール呼び出しが開始した。
    ToolStarted {
        /// 呼び出されたツール名。
        tool_name: String,
        /// ツール呼び出しの ID。
        call_id: String,
    },
    /// ツール呼び出しが完了した。
    ToolCompleted {
        /// 呼び出されたツール名。
        tool_name: String,
        /// ツール呼び出しの ID。
        call_id: String,
        /// ツールがエラーで終了したかどうか。
        is_error: bool,
    },
}

/// トークン使用量に関するイベント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum UsageEvent {
    /// プロバイダ・モデル別のトークン使用量。
    Usage {
        /// プロバイダ識別子。
        provider: String,
        /// モデル識別子。
        model: String,
        /// 入力トークン数。
        input_tokens: u64,
        /// 出力トークン数。
        output_tokens: u64,
        /// キャッシュ読み取りトークン数。
        cache_read_tokens: u64,
        /// キャッシュ書き込みトークン数。
        cache_write_tokens: u64,
    },
    /// プロバイダ・モデル別のキャッシュヒット統計。
    CacheStats {
        /// プロバイダ識別子。
        provider: String,
        /// モデル識別子。
        model: String,
        /// キャッシュヒット回数。
        cache_hits: u64,
        /// キャッシュミス回数。
        cache_misses: u64,
    },
}

/// プロバイダ切替に関するイベント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ProviderEvent {
    /// プロバイダがフォールバックした。
    ProviderFallback {
        /// 切替元のプロバイダ識別子。
        from_provider: String,
        /// 切替先のプロバイダ識別子。
        to_provider: String,
        /// 切替理由。
        reason: String,
    },
}

/// 障害に関するイベント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum FaultEvent {
    /// 購読者が取りこぼした。
    SubscriberLagged {
        /// 取りこぼした購読者の ID。
        subscriber_id: u64,
        /// 取りこぼしたイベント数。
        skipped: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_preserves_every_variant() {
        let cases: Vec<(&'static str, EventKind)> = vec![
            (
                "Lifecycle",
                LifecycleEvent::Started {
                    session_id: "session-1".into(),
                }
                .into(),
            ),
            (
                "Lifecycle",
                LifecycleEvent::Delegated {
                    session_id: "session-1".into(),
                    target: "worker-1".into(),
                }
                .into(),
            ),
            (
                "Lifecycle",
                LifecycleEvent::BackgroundTaskStarted {
                    task_id: "task-1".into(),
                }
                .into(),
            ),
            (
                "Lifecycle",
                LifecycleEvent::BackgroundTaskCompleted {
                    task_id: "task-1".into(),
                }
                .into(),
            ),
            (
                "Lifecycle",
                LifecycleEvent::BackgroundTaskCancelled {
                    task_id: "task-1".into(),
                }
                .into(),
            ),
            (
                "Lifecycle",
                LifecycleEvent::AgentRunStateChanged {
                    run_id: "run-1".into(),
                    from: AgentRunPhase::Running,
                    to: AgentRunPhase::Error,
                    reason: Some("boom".into()),
                }
                .into(),
            ),
            (
                "Lifecycle",
                LifecycleEvent::Completed {
                    session_id: "session-1".into(),
                }
                .into(),
            ),
            (
                "Lifecycle",
                LifecycleEvent::Failed {
                    session_id: "session-1".into(),
                    reason: "boom".into(),
                }
                .into(),
            ),
            (
                "Message",
                MessageEvent::MessageDelta { delta: "he".into() }.into(),
            ),
            (
                "Message",
                MessageEvent::ReasoningDelta {
                    delta: "thinking".into(),
                }
                .into(),
            ),
            (
                "Tool",
                ToolEvent::ToolStarted {
                    tool_name: "read".into(),
                    call_id: "call-1".into(),
                }
                .into(),
            ),
            (
                "Tool",
                ToolEvent::ToolCompleted {
                    tool_name: "read".into(),
                    call_id: "call-1".into(),
                    is_error: true,
                }
                .into(),
            ),
            (
                "Usage",
                UsageEvent::Usage {
                    provider: "anthropic".into(),
                    model: "kimi-k3".into(),
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_read_tokens: 30,
                    cache_write_tokens: 40,
                }
                .into(),
            ),
            (
                "Usage",
                UsageEvent::CacheStats {
                    provider: "anthropic".into(),
                    model: "kimi-k3".into(),
                    cache_hits: 3,
                    cache_misses: 4,
                }
                .into(),
            ),
            (
                "Provider",
                ProviderEvent::ProviderFallback {
                    from_provider: "anthropic".into(),
                    to_provider: "openai".into(),
                    reason: "timeout".into(),
                }
                .into(),
            ),
            (
                "Fault",
                FaultEvent::SubscriberLagged {
                    subscriber_id: 7,
                    skipped: 12,
                }
                .into(),
            ),
        ];

        for (category, kind) in cases {
            let event = Event::new(kind);
            let json = serde_json::to_string(&event).expect("serialize Event");
            let restored: Event = serde_json::from_str(&json).expect("deserialize Event");
            assert_eq!(event, restored, "round-trip mismatch: category={category}");

            let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
            // Event の kind フィールドの中に EventKind の隣接タグ
            // オブジェクト（{"kind", "payload"}）がネストするため、
            // 外側タグは value["kind"]["kind"] となる。
            assert_eq!(
                value["kind"]["kind"], category,
                "outer tag mismatch: category={category}"
            );
            // 内側 enum レベルの隣接タグ（{"kind", "payload"}）
            assert!(
                value["kind"]["payload"]["kind"].is_string(),
                "inner tag missing: category={category}"
            );
            assert!(
                value["kind"]["payload"]["payload"].is_object(),
                "inner payload missing: category={category}"
            );
        }
    }

    // Given: Running から Error へ遷移し理由を持つエージェント実行状態変化イベント。
    // When: Event を JSON 文字列へシリアライズして復元する。
    // Then: 元のイベントと等しく、隣接タグ "AgentRunStateChanged" を保つ。
    #[test]
    fn agent_run_state_changed_round_trips_with_error_reason() {
        let event = Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".to_string(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Error,
            reason: Some("boom".to_string()),
        });

        let json = serde_json::to_string(&event).expect("serialize Event");
        let restored: Event = serde_json::from_str(&json).expect("deserialize Event");
        assert_eq!(event, restored);

        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(
            value["kind"]["payload"]["kind"], "AgentRunStateChanged",
            "inner tag mismatch"
        );
    }

    // Given: 理由を持たないバックグラウンドタスクのキャンセルイベント。
    // When: Event を JSON 文字列へシリアライズして復元する。
    // Then: 元のイベントと等しく、隣接タグ "BackgroundTaskCancelled" を保つ。
    #[test]
    fn background_task_cancelled_round_trips() {
        let event = Event::new(LifecycleEvent::BackgroundTaskCancelled {
            task_id: "task-1".to_string(),
        });

        let json = serde_json::to_string(&event).expect("serialize Event");
        let restored: Event = serde_json::from_str(&json).expect("deserialize Event");
        assert_eq!(event, restored);

        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(
            value["kind"]["payload"]["kind"], "BackgroundTaskCancelled",
            "inner tag mismatch"
        );
    }

    // Given: AgentRunPhase の全 5 位相。
    // When: 各位相を JSON 文字列へシリアライズして復元する。
    // Then: いずれの位相も往復前後で等しい。
    #[test]
    fn agent_run_phase_round_trips_every_variant() {
        let phases = [
            AgentRunPhase::Pending,
            AgentRunPhase::Running,
            AgentRunPhase::Waiting,
            AgentRunPhase::Done,
            AgentRunPhase::Error,
        ];

        for phase in phases {
            let json = serde_json::to_string(&phase).expect("serialize AgentRunPhase");
            let restored: AgentRunPhase =
                serde_json::from_str(&json).expect("deserialize AgentRunPhase");
            assert_eq!(phase, restored, "round-trip mismatch: {json}");
        }
    }

    #[test]
    fn event_meta_now_stamps_both_clocks_when_called_successively() {
        let first = EventMeta::now();
        let second = EventMeta::now();

        assert_eq!(first.schema_version, SCHEMA_VERSION);
        assert_eq!(second.schema_version, SCHEMA_VERSION);
        assert!(
            second.monotonic >= first.monotonic,
            "monotonic must be non-decreasing"
        );
        assert!(
            first.wall_clock >= std::time::UNIX_EPOCH,
            "wall_clock must be at or after UNIX_EPOCH"
        );
        assert!(
            second.wall_clock >= std::time::UNIX_EPOCH,
            "wall_clock must be at or after UNIX_EPOCH"
        );
    }

    #[test]
    fn event_new_stamps_meta_when_created() {
        let event = Event::new(LifecycleEvent::Started {
            session_id: "session-1".to_string(),
        });

        assert_eq!(event.meta.schema_version, SCHEMA_VERSION);
        assert!(
            event.meta.wall_clock >= std::time::UNIX_EPOCH,
            "wall_clock must be at or after UNIX_EPOCH"
        );
        assert!(matches!(
            event.kind,
            EventKind::Lifecycle(LifecycleEvent::Started { .. })
        ));
    }
}
