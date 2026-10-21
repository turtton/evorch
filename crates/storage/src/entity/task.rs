use std::time::SystemTime;

/// タスクの永続化状態です。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Queued,
    Blocked,
    Retrying,
    /// Operator-stopped; resumable with current authority, never auto-retried.
    Stopped,
    Cancelled,
    Running,
    Completed,
    Failed,
}

/// TaskProgressed の状態 payload。再開用の会話は run_contexts に保存します。
/// input は最初のイベントのみ保持し、attempts は現在の run generation です。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskContinuation {
    pub status: TaskStatus,
    pub input: Option<String>,
    pub last_artifact: Option<String>,
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub heartbeat_at_ns: Option<u64>,
}

string_enum!(TaskStatus {
    Pending => "pending",
    Queued => "queued",
    Blocked => "blocked",
    Retrying => "retrying",
    Stopped => "stopped",
    Cancelled => "cancelled",
    Running => "running",
    Completed => "completed",
    Failed => "failed",
});

/// タスクの永続化レコードです。JSON ペイロードのスキーマは runtime が所有します。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub id: String,
    pub session_id: Option<String>,
    pub status: TaskStatus,
    pub parent_run_id: Option<String>,
    pub input: Option<String>,
    pub progress: Option<String>,
    pub last_artifact: Option<String>,
    pub failure_reason: Option<String>,
    pub attempts: u32,
    pub heartbeat_at: Option<SystemTime>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}
