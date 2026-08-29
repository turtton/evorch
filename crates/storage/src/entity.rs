//! 永続化するエンティティを定義します。
use std::time::SystemTime;

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

/// タスクの永続化状態です。
///
/// V1 マイグレーションの `tasks.status` CHECK 制約が
/// `'running','completed','failed'` のみを許容するため `Cancelled` を持たず、
/// キャンセルイベントは射影で [`TaskStatus::Failed`] へ写像されます。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// 実行中です。
    Running,
    /// 正常に完了しました。
    Completed,
    /// 失敗しました。
    Failed,
}

string_enum!(TaskStatus {
    Running => "running",
    Completed => "completed",
    Failed => "failed",
});

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

/// タスクの永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// タスク識別子です。
    pub id: String,
    /// 所属セッション識別子です。
    pub session_id: Option<String>,
    /// タスク状態です。
    pub status: TaskStatus,
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
