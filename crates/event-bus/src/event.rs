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
    /// エージェント間メッセージ関連のイベント。
    AgentMessage(AgentMessageEvent),
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

impl From<AgentMessageEvent> for EventKind {
    fn from(event: AgentMessageEvent) -> Self {
        Self::AgentMessage(event)
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
        /// ツールが添えたメタデータ (request_id や取得 URL 等の補助情報)。
        ///
        /// v0.1 で保存された旧形式ペイロードはこのフィールドを持たないため、
        /// 欠落時は `None` として読む。
        #[serde(default)]
        detail: Option<serde_json::Value>,
    },
    /// ツール実行の承認が要求された。
    ApprovalRequested { tool_name: String, call_id: String },
    /// 承認要求への応答（承認 UI / CLI 側が emit する）。
    ApprovalResolved { call_id: String, approved: bool },
    /// ポリシーまたは承認結果によりツール実行が拒否された。
    ExecutionDenied {
        tool_name: String,
        call_id: String,
        reason: String,
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

/// プロバイダリクエスト attempt の失敗を型付きで分類する。
///
/// 観測イベント用の分類であり、エラーレスポンス本文・詳細メッセージ・
/// credential は含めない (イベントは bus と storage に流れるため)。
/// `#[serde(tag = "kind")]` の内部タグ形式で、unit バリアントは
/// `{"kind": "Timeout"}`、データ付きバリアントは
/// `{"kind": "Http", "status": 500}` と一貫した形状になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ProviderFailureKind {
    /// HTTP 429 (レート制限)。
    RateLimited,
    /// 429 以外の HTTP エラーステータスをプロバイダから観測した。
    Http {
        /// HTTP ステータスコード。
        status: u16,
    },
    /// リクエストのタイムアウト。
    Timeout,
    /// レスポンス形式が不正 (不正な JSON / SSE / canonical 変換の失敗)。
    InvalidResponse,
    /// トランスポート層の失敗 (接続エラー / ストリームの中途 EOF 等)。
    Transport,
    /// プロバイダ側サーバーエラー (routing の FailureKind::Server 分類由来)。
    Server,
    /// 支払いまたはクォータ上限 (routing の FailureKind::Quota 分類由来)。
    Quota,
    /// 認証または認可の失敗 (routing の FailureKind::Auth 分類由来)。
    Auth,
    /// 上記に分類できない失敗。
    Other,
}

/// プロバイダ切替とリクエスト attempt 観測に関するイベント。
///
/// attempt 観測イベント (`RequestStarted` / `FirstTokenObserved` /
/// `RequestCompleted` / `RequestFailed`) は attempt ごとに一意な
/// `request_id` を共有し、これで相互に相関する。`request_id` は
/// `req-{プロセス起動時刻ミリ秒}-{プロセス内単調カウンタ}` 形式で、
/// 同一プロセス内での一意性を保証し、プロセス再起動をまたいだ衝突も
/// 起動時刻成分で実用上避ける。attempt 開始イベントは HTTP request を
/// 送信する直前に、終端イベントは成功・失敗を問わず attempt 終了時に
/// ちょうど 1 回発行される (start ⇒ terminal の対応が常に取れる)。
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
    /// プロバイダへのリクエスト attempt を開始した。
    RequestStarted {
        /// attempt 相関用の request ID (同一 attempt の全イベントで同一)。
        request_id: String,
        /// プロバイダ識別子 (usage イベントと同じラベル)。
        provider: String,
        /// routing の provider profile 名。profile に紐付けられていない
        /// 構築経路では `None`。
        profile: Option<String>,
        /// wire プロトコル識別子 (例: `openai-chat-completions` /
        /// `anthropic-messages`)。
        protocol: String,
        /// モデル識別子。
        model: String,
        /// ストリーミング attempt かどうか。
        streaming: bool,
    },
    /// ストリーミング attempt で最初の user-visible delta を観測した (TTFT)。
    ///
    /// TTFT 測定契約:
    /// - 開始点: HTTP request attempt を**送信する直前**。
    /// - 終了点: provider wire stream から**最初の user-visible content
    ///   delta (空でないテキスト差分) または tool-call delta を正常に解釈
    ///   した瞬間**。
    /// - **first token に数えないもの**: HTTP headers 到着、usage-only
    ///   frame、keepalive、空 delta、reasoning-only delta。
    /// - 1 回の streaming attempt で**高々 1 回**だけ発行される。
    /// - 非ストリーミング (`send`) attempt では発行されない。
    FirstTokenObserved {
        /// attempt 相関用の request ID。
        request_id: String,
        /// プロバイダ識別子。
        provider: String,
        /// routing の provider profile 名 (不明なら `None`)。
        profile: Option<String>,
        /// wire プロトコル識別子。
        protocol: String,
        /// モデル識別子。
        model: String,
        /// time-to-first-token (ミリ秒)。
        ttft_ms: u64,
    },
    /// リクエスト attempt が成功して完了した。
    ///
    /// token accounting 契約: 本イベントの token counts は、**同じ
    /// attempt について発行される [`UsageEvent::Usage`] の値をそのまま
    /// 写した観測用の複製**である。トークン消費の集計は常に
    /// [`UsageEvent::Usage`] だけを canonical な集計入力とし、本イベントの
    /// counts と合算してはならない (二重計上になる)。wire 上の相関は
    /// 「同一 provider / model で、同一 request の bus 順序が
    /// `RequestStarted` → [`UsageEvent::Usage`] → `RequestCompleted` と
    /// なる」ことで担保される ([`UsageEvent`] に request ID は持たせない:
    /// wire format 不変制約のため)。
    RequestCompleted {
        /// attempt 相関用の request ID。
        request_id: String,
        /// プロバイダ識別子。
        provider: String,
        /// routing の provider profile 名 (不明なら `None`)。
        profile: Option<String>,
        /// wire プロトコル識別子。
        protocol: String,
        /// モデル識別子。
        model: String,
        /// ストリーミング attempt かどうか。
        streaming: bool,
        /// attempt 開始 (送信直前) からの経過時間 (ミリ秒)。
        duration_ms: u64,
        /// 入力トークン数 ([`UsageEvent::Usage`] と同値)。
        input_tokens: u64,
        /// 出力トークン数 ([`UsageEvent::Usage`] と同値)。
        output_tokens: u64,
        /// キャッシュ読み取りトークン数 ([`UsageEvent::Usage`] と同値)。
        cache_read_tokens: u64,
        /// キャッシュ書き込みトークン数 ([`UsageEvent::Usage`] と同値)。
        cache_write_tokens: u64,
        /// canonical finish reason (snake_case: `stop` / `length` /
        /// `tool_use` / `content_filter`)。
        finish_reason: String,
    },
    /// リクエスト attempt が失敗して終了した。
    ///
    /// 失敗の詳細は型付き分類のみを保持し、エラーレスポンス本文・
    /// 詳細メッセージ・credential は**含めない**。
    RequestFailed {
        /// attempt 相関用の request ID。
        request_id: String,
        /// プロバイダ識別子。
        provider: String,
        /// routing の provider profile 名 (不明なら `None`)。
        profile: Option<String>,
        /// wire プロトコル識別子。
        protocol: String,
        /// モデル識別子。
        model: String,
        /// ストリーミング attempt かどうか。
        streaming: bool,
        /// attempt 開始 (送信直前) から失敗までの経過時間 (ミリ秒)。
        duration_ms: u64,
        /// 型付き失敗分類。
        failure: ProviderFailureKind,
    },
    /// routing の fallback 選択境界でフォールバック先が選択された。
    ///
    /// 候補順序や retry/fallback policy 自体は変化させず、選択の観測のみを
    /// 行う。失敗した元 attempt との相関は `request_id` (attempt の request
    /// ID を呼び出し側が把握している場合) と `session_id` / `logical_model`
    /// で保持する。
    FallbackTriggered {
        /// 失敗したプロバイダプロファイル名。
        from_provider: String,
        /// 選択されたフォールバック先プロバイダプロファイル名。
        to_provider: String,
        /// フォールバック先の実モデル ID。
        to_model: String,
        /// 元の論理モデル名。
        logical_model: String,
        /// 障害が発生したセッションの ID。
        session_id: String,
        /// 元 attempt の失敗分類。
        failure: ProviderFailureKind,
        /// 失敗した元 attempt の request ID (把握している場合のみ)。
        request_id: Option<String>,
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

/// エージェント間で配送されるメッセージ封筒です。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMessage {
    /// メッセージ識別子。
    pub message_id: String,
    /// 送信側エージェント実行の識別子。
    pub sender_run_id: String,
    /// 受信側エージェント実行の識別子。
    pub recipient_run_id: String,
    /// メッセージ種別。`Reply` は `reply_to` による相関を意味します。
    pub kind: AgentMessageKind,
    /// メッセージ本文。
    pub content: String,
    /// `kind` が [`AgentMessageKind::Reply`] のときの相関元メッセージ識別子。
    pub reply_to: Option<String>,
}

/// エージェント間メッセージの種別です。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    /// 相関しない一方向の送信です。
    Send,
    /// [`AgentMessage::reply_to`] で先行メッセージと相関する返信です。
    Reply,
    /// 実行中の受信者へ割り込む指示です。
    Steering,
}

/// 配信時点で確定する受信側での扱いです。
///
/// 受信者の位相と送受信者の関係から配信時に決定されます。`Wake` は受信者が
/// 待機中（[`AgentRunPhase::Waiting`]）のため実行を再開させる扱い、`Steering`
/// は送信者が受信者の親であり受信者がターン中のため次回 loop-top で注入する
/// 扱い、`Aside` は受信者のステップ境界まで保留する扱いを意味します。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryDisposition {
    /// 受信者がターン中のため次回 loop-top で注入します。
    Steering,
    /// 受信者のステップ境界まで保留します。
    Aside,
    /// 受信者が待機中のため実行を再開させます。
    Wake,
}

/// エージェント間メッセージの配送に関するイベントです。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum AgentMessageEvent {
    /// メッセージが受信者へ配送された。
    Delivered {
        /// 配送されたメッセージ封筒。
        message: AgentMessage,
        /// 配信時に決定された受信側での扱い。
        disposition: DeliveryDisposition,
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
                    detail: None,
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
    fn approval_requested_uses_nested_adjacent_tags() {
        let event = Event::new(ToolEvent::ApprovalRequested {
            tool_name: "shell".into(),
            call_id: "c1".into(),
        });
        let json = serde_json::to_string(&event).expect("JSONへ変換できる");
        let restored: Event = serde_json::from_str(&json).expect("JSONから復元できる");
        assert_eq!(event, restored);
        let value: serde_json::Value = serde_json::from_str(&json).expect("JSONを読み取れる");

        assert_eq!(
            value["kind"],
            serde_json::json!({
                "kind": "Tool",
                "payload": {
                    "kind": "ApprovalRequested",
                    "payload": {"tool_name": "shell", "call_id": "c1"}
                }
            })
        );
    }

    #[test]
    fn approval_resolved_round_trips_with_nested_adjacent_tags() {
        let event = Event::new(ToolEvent::ApprovalResolved {
            call_id: "c1".into(),
            approved: true,
        });
        let json = serde_json::to_string(&event).expect("JSONへ変換できる");

        let restored: Event = serde_json::from_str(&json).expect("JSONから復元できる");

        assert_eq!(event, restored);
        let value: serde_json::Value = serde_json::from_str(&json).expect("JSONを読み取れる");
        assert_eq!(
            value["kind"]["payload"],
            serde_json::json!({
                "kind": "ApprovalResolved",
                "payload": {"call_id": "c1", "approved": true}
            })
        );
    }

    #[test]
    fn execution_denied_uses_nested_adjacent_tags() {
        let event = Event::new(ToolEvent::ExecutionDenied {
            tool_name: "shell".into(),
            call_id: "c1".into(),
            reason: "policy".into(),
        });
        let json = serde_json::to_string(&event).expect("JSONへ変換できる");
        let restored: Event = serde_json::from_str(&json).expect("JSONから復元できる");
        assert_eq!(event, restored);
        let value: serde_json::Value = serde_json::from_str(&json).expect("JSONを読み取れる");

        assert_eq!(
            value["kind"]["payload"],
            serde_json::json!({
                "kind": "ExecutionDenied",
                "payload": {
                    "tool_name": "shell",
                    "call_id": "c1",
                    "reason": "policy"
                }
            })
        );
    }

    // Given: detail メタデータ付きと detail なしの ToolCompleted イベント。
    // When: Event を JSON 文字列へシリアライズして復元する。
    // Then: いずれも往復前後で等しく、detail の有無が保存される。
    #[test]
    fn tool_completed_round_trips_with_and_without_detail() {
        let cases = [
            Event::new(ToolEvent::ToolCompleted {
                tool_name: "read".into(),
                call_id: "call-1".into(),
                is_error: false,
                detail: Some(serde_json::json!({ "request_id": "req-1" })),
            }),
            Event::new(ToolEvent::ToolCompleted {
                tool_name: "read".into(),
                call_id: "call-1".into(),
                is_error: true,
                detail: None,
            }),
        ];

        for event in cases {
            let json = serde_json::to_string(&event).expect("JSONへ変換できる");
            let restored: Event = serde_json::from_str(&json).expect("JSONから復元できる");
            assert_eq!(event, restored, "round-trip mismatch: {json}");
        }
    }

    // Given: detail フィールドを含まない旧形式の ToolCompleted ペイロード (v0.1 で保存された JSON)。
    // When: 旧形式 JSON をデシリアライズする。
    // Then: detail が None として復元され、SCHEMA_VERSION 1 のまま読み続けられる。
    #[test]
    fn tool_completed_legacy_payload_without_detail_deserializes() {
        let legacy = r#"{
            "meta": {
                "schema_version": 1,
                "monotonic": {"secs": 0, "nanos": 0},
                "wall_clock": {"secs_since_epoch": 0, "nanos_since_epoch": 0}
            },
            "kind": {
                "kind": "Tool",
                "payload": {
                    "kind": "ToolCompleted",
                    "payload": {
                        "tool_name": "read",
                        "call_id": "call-1",
                        "is_error": false
                    }
                }
            }
        }"#;

        let restored: Event = serde_json::from_str(legacy).expect("旧形式 JSON から復元できる");

        assert_eq!(
            restored.kind,
            EventKind::Tool(ToolEvent::ToolCompleted {
                tool_name: "read".into(),
                call_id: "call-1".into(),
                is_error: false,
                detail: None,
            })
        );
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

    // Given: 観測系として追加する全 ProviderEvent バリアント。
    // When: それぞれ Event として JSON 往復する。
    // Then: 値が保存され、内側タグ名が期待どおりになる (request ID 相関の土台)。
    #[test]
    fn provider_observation_variants_round_trip() {
        let cases: Vec<(&'static str, ProviderEvent)> = vec![
            (
                "RequestStarted",
                ProviderEvent::RequestStarted {
                    request_id: "req-1700000000000-1".into(),
                    provider: "openai".into(),
                    profile: Some("primary".into()),
                    protocol: "openai-chat-completions".into(),
                    model: "gpt-contract".into(),
                    streaming: false,
                },
            ),
            (
                "FirstTokenObserved",
                ProviderEvent::FirstTokenObserved {
                    request_id: "req-1700000000000-2".into(),
                    provider: "anthropic".into(),
                    profile: None,
                    protocol: "anthropic-messages".into(),
                    model: "claude-contract".into(),
                    ttft_ms: 42,
                },
            ),
            (
                "RequestCompleted",
                ProviderEvent::RequestCompleted {
                    request_id: "req-1700000000000-3".into(),
                    provider: "openai-compatible".into(),
                    profile: Some("secondary".into()),
                    protocol: "openai-chat-completions".into(),
                    model: "local-model".into(),
                    streaming: true,
                    duration_ms: 500,
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_read_tokens: 3,
                    cache_write_tokens: 1,
                    finish_reason: "stop".into(),
                },
            ),
            (
                "RequestFailed",
                ProviderEvent::RequestFailed {
                    request_id: "req-1700000000000-4".into(),
                    provider: "anthropic".into(),
                    profile: None,
                    protocol: "anthropic-messages".into(),
                    model: "claude-contract".into(),
                    streaming: true,
                    duration_ms: 120,
                    failure: ProviderFailureKind::Http { status: 500 },
                },
            ),
            (
                "FallbackTriggered",
                ProviderEvent::FallbackTriggered {
                    from_provider: "primary".into(),
                    to_provider: "secondary".into(),
                    to_model: "model-b".into(),
                    logical_model: "summary".into(),
                    session_id: "session-1".into(),
                    failure: ProviderFailureKind::Timeout,
                    request_id: Some("req-1700000000000-5".into()),
                },
            ),
        ];

        for (inner_tag, event) in cases {
            let outer = Event::new(event);
            let json = serde_json::to_string(&outer).expect("serialize Event");
            let restored: Event = serde_json::from_str(&json).expect("deserialize Event");
            assert_eq!(outer, restored, "round-trip mismatch: {inner_tag}");

            let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
            assert_eq!(value["kind"]["kind"], "Provider", "outer tag mismatch");
            assert_eq!(
                value["kind"]["payload"]["kind"], inner_tag,
                "inner tag mismatch"
            );
        }
    }

    // Given: 失敗分類の全 ProviderFailureKind バリアント。
    // When: それぞれ JSON 往復する。
    // Then: 内部タグ形式の一貫した形状が保たれる (Http は status を保持する)。
    #[test]
    fn provider_failure_kind_round_trips_with_pinned_shapes() {
        let http = ProviderFailureKind::Http { status: 429 };
        let value = serde_json::to_value(http).expect("serialize Http failure");
        assert_eq!(value, serde_json::json!({"kind": "Http", "status": 429}));
        let restored: ProviderFailureKind =
            serde_json::from_value(value).expect("deserialize Http failure");
        assert_eq!(restored, http);

        let unit_variants = [
            (ProviderFailureKind::RateLimited, "RateLimited"),
            (ProviderFailureKind::Timeout, "Timeout"),
            (ProviderFailureKind::InvalidResponse, "InvalidResponse"),
            (ProviderFailureKind::Transport, "Transport"),
            (ProviderFailureKind::Server, "Server"),
            (ProviderFailureKind::Quota, "Quota"),
            (ProviderFailureKind::Auth, "Auth"),
            (ProviderFailureKind::Other, "Other"),
        ];
        for (failure, tag) in unit_variants {
            let value = serde_json::to_value(failure).expect("serialize unit failure");
            assert_eq!(value, serde_json::json!({"kind": tag}), "shape: {tag}");
            let restored: ProviderFailureKind =
                serde_json::from_value(value).expect("deserialize unit failure");
            assert_eq!(restored, failure, "round-trip: {tag}");
        }
    }

    // Given: 既存 ProviderFallback の schema_version 1 wire スナップショット。
    // When: 現在の Event として deserialize する。
    // Then: 既存 variant の wire format は不変で復元でき、バージョンは 1 のまま。
    #[test]
    fn legacy_provider_fallback_snapshot_still_deserializes_and_schema_version_is_one() {
        let snapshot = r#"{"meta":{"schema_version":1,"monotonic":{"secs":1,"nanos":0},"wall_clock":{"secs_since_epoch":1700000000,"nanos_since_epoch":0}},"kind":{"kind":"Provider","payload":{"kind":"ProviderFallback","payload":{"from_provider":"anthropic","to_provider":"openai","reason":"timeout"}}}}"#;

        let event: Event = serde_json::from_str(snapshot).expect("legacy snapshot を復元できる");

        assert_eq!(event.meta.schema_version, 1);
        assert!(matches!(
            event.kind,
            EventKind::Provider(ProviderEvent::ProviderFallback { .. })
        ));
        assert_eq!(
            SCHEMA_VERSION, 1,
            "schema_version は追加のみで 1 を維持する"
        );
    }

    // Given: reply_to と disposition を持つ完全な AgentMessage 封筒。
    // When: EventKind::AgentMessage として JSON 文字列へシリアライズして復元する。
    // Then: EventKind 全体が等しく、封筒の全フィールドと disposition が保存される。
    #[test]
    fn agent_message_envelope_round_trips_through_event_kind() {
        let kind = EventKind::AgentMessage(AgentMessageEvent::Delivered {
            message: AgentMessage {
                message_id: "msg-2".into(),
                sender_run_id: "run-1".into(),
                recipient_run_id: "run-2".into(),
                kind: AgentMessageKind::Reply,
                content: "result".into(),
                reply_to: Some("msg-1".into()),
            },
            disposition: DeliveryDisposition::Steering,
        });

        let json = serde_json::to_string(&kind).expect("serialize EventKind::AgentMessage");
        let restored: EventKind = serde_json::from_str(&json).expect("deserialize EventKind");
        assert_eq!(kind, restored);

        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["kind"], "AgentMessage", "outer tag mismatch");
        // AgentMessageKind / DeliveryDisposition は snake_case で送出される。
        assert_eq!(
            value["payload"]["payload"]["message"]["kind"], "reply",
            "message kind tag mismatch"
        );
        assert_eq!(
            value["payload"]["payload"]["disposition"], "steering",
            "disposition tag mismatch"
        );
    }

    // Given: AgentMessage イベントと Lifecycle イベント。
    // When: それぞれ JSON 文字列へシリアライズして復元する。
    // Then: AgentMessage の大分類タグは Lifecycle と異なり、Lifecycle 側は
    //       従来どおり不変で往復する。
    #[test]
    fn agent_message_event_kind_tag_is_distinct_from_lifecycle() {
        let agent = EventKind::AgentMessage(AgentMessageEvent::Delivered {
            message: AgentMessage {
                message_id: "msg-1".into(),
                sender_run_id: "run-1".into(),
                recipient_run_id: "run-2".into(),
                kind: AgentMessageKind::Send,
                content: "ping".into(),
                reply_to: None,
            },
            disposition: DeliveryDisposition::Wake,
        });
        let lifecycle = EventKind::Lifecycle(LifecycleEvent::Started {
            session_id: "session-1".into(),
        });

        let agent_json = serde_json::to_string(&agent).expect("serialize AgentMessage kind");
        let agent_value: serde_json::Value = serde_json::from_str(&agent_json).expect("valid JSON");
        assert_eq!(agent_value["kind"], "AgentMessage");
        assert_ne!(agent_value["kind"], "Lifecycle");

        let lifecycle_json = serde_json::to_string(&lifecycle).expect("serialize Lifecycle kind");
        let restored: EventKind =
            serde_json::from_str(&lifecycle_json).expect("deserialize Lifecycle kind");
        assert_eq!(
            lifecycle, restored,
            "lifecycle round-trip must be unchanged"
        );
    }
}
