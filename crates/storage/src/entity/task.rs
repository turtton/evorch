use std::time::SystemTime;

/// タスクの永続化状態です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Queued,
    Blocked,
    Retrying,
    Cancelled,
    Running,
    Completed,
    Failed,
}

string_enum!(TaskStatus {
    Pending => "pending",
    Queued => "queued",
    Blocked => "blocked",
    Retrying => "retrying",
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
    pub resume_cursor: Option<String>,
    pub attempts: u32,
    pub heartbeat_at: Option<SystemTime>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}
