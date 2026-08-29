//! イベントから読み取りモデルへの投影を管理します。

use std::collections::BTreeMap;
use std::time::SystemTime;

use event_bus::{EventKind, LifecycleEvent, MessageEvent, ToolEvent};
use rusqlite::{Connection, params};

use crate::StorageError;
use crate::db::system_time_to_ns;
use crate::entity::{SessionStatus, TaskStatus};
use crate::repo::event::{self, StoredEvent};

/// イベントログを畳み込んだセッション復元状態です。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub status: SessionStatus,
    pub failure_reason: Option<String>,
    pub delegated_to: Option<String>,
    /// セッションに蓄積されたメッセージ差分です。
    /// イベント語彙に finalize イベントがないため、完了後も全文を保持します。
    /// 「保留」は中断されたセッションを復元する場合に限り末尾の未完了分として解釈してください。
    pub pending_message: String,
    /// セッションに蓄積された推論差分です。
    /// イベント語彙に finalize イベントがないため、完了後も全文を保持します。
    /// 「保留」は中断されたセッションを復元する場合に限り末尾の未完了分として解釈してください。
    pub pending_reasoning: String,
    pub open_tool_calls: Vec<(String, String)>,
    pub task_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionProjection {
    snapshot: SessionSnapshot,
    first_seen: SystemTime,
    last_seen: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskProjection {
    session_id: Option<String>,
    status: TaskStatus,
    first_seen: SystemTime,
    last_seen: SystemTime,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ProjectionState {
    sessions: BTreeMap<String, SessionProjection>,
    tasks: BTreeMap<String, TaskProjection>,
}

impl ProjectionState {
    fn session(&mut self, stored: &StoredEvent) -> Option<&mut SessionProjection> {
        let id = stored.session_id.as_ref()?;
        let time = stored.event.meta.wall_clock;
        let session = self
            .sessions
            .entry(id.clone())
            .or_insert_with(|| SessionProjection {
                snapshot: SessionSnapshot {
                    session_id: id.clone(),
                    status: SessionStatus::Running,
                    failure_reason: None,
                    delegated_to: None,
                    pending_message: String::new(),
                    pending_reasoning: String::new(),
                    open_tool_calls: Vec::new(),
                    task_ids: Vec::new(),
                },
                first_seen: time,
                last_seen: time,
            });
        session.last_seen = time;
        Some(session)
    }

    fn task(&mut self, stored: &StoredEvent, id: &str) -> &mut TaskProjection {
        let time = stored.event.meta.wall_clock;
        let task = self
            .tasks
            .entry(id.to_owned())
            .or_insert_with(|| TaskProjection {
                session_id: stored.session_id.clone(),
                status: TaskStatus::Running,
                first_seen: time,
                last_seen: time,
            });
        if stored.session_id.is_some() {
            task.session_id.clone_from(&stored.session_id);
        }
        task.last_seen = time;
        task
    }
}

pub(crate) fn apply_event(state: &mut ProjectionState, stored: &StoredEvent) {
    match &stored.event.kind {
        EventKind::Lifecycle(event) => match event {
            LifecycleEvent::Started { .. } => set_status(state, stored, SessionStatus::Running),
            LifecycleEvent::Delegated { target, .. } => {
                if let Some(session) = state.session(stored) {
                    session.snapshot.status = SessionStatus::Delegated;
                    session.snapshot.delegated_to = Some(target.clone());
                }
            }
            LifecycleEvent::BackgroundTaskStarted { task_id } => {
                state.task(stored, task_id).status = TaskStatus::Running;
                if let Some(session) = state.session(stored) {
                    session.snapshot.task_ids.push(task_id.clone());
                }
            }
            LifecycleEvent::BackgroundTaskCompleted { task_id } => {
                state.task(stored, task_id).status = TaskStatus::Completed;
                let _ = state.session(stored);
            }
            LifecycleEvent::Completed { .. } => {
                set_status(state, stored, SessionStatus::Completed);
            }
            LifecycleEvent::Failed { reason, .. } => {
                if let Some(session) = state.session(stored) {
                    session.snapshot.status = SessionStatus::Failed;
                    session.snapshot.failure_reason = Some(reason.clone());
                }
            }
        },
        EventKind::Message(MessageEvent::MessageDelta { delta }) => {
            if let Some(session) = state.session(stored) {
                session.snapshot.pending_message.push_str(delta);
            }
        }
        EventKind::Message(MessageEvent::ReasoningDelta { delta }) => {
            if let Some(session) = state.session(stored) {
                session.snapshot.pending_reasoning.push_str(delta);
            }
        }
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id }) => {
            if let Some(session) = state.session(stored) {
                session
                    .snapshot
                    .open_tool_calls
                    .push((tool_name.clone(), call_id.clone()));
            }
        }
        EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. }) => {
            if let Some(session) = state.session(stored) {
                session
                    .snapshot
                    .open_tool_calls
                    .retain(|(_, id)| id != call_id);
            }
        }
        EventKind::Usage(_) | EventKind::Provider(_) | EventKind::Fault(_) => {}
    }
}

fn set_status(state: &mut ProjectionState, stored: &StoredEvent, status: SessionStatus) {
    if let Some(session) = state.session(stored) {
        session.snapshot.status = status;
    }
}

fn fold(events: &[StoredEvent]) -> ProjectionState {
    let mut state = ProjectionState::default();
    events
        .iter()
        .for_each(|stored| apply_event(&mut state, stored));
    state
}

/// 全イベントを採番順で畳み込み、セッション ID 順の復元状態を返します。
pub fn restore_sessions(conn: &Connection) -> Result<Vec<SessionSnapshot>, StorageError> {
    Ok(fold(&event::list_all_ordered(conn)?)
        .sessions
        .into_values()
        .map(|value| value.snapshot)
        .collect())
}

/// 指定セッションのイベントを採番順で畳み込み、復元状態を返します。
pub fn restore_session(
    conn: &Connection,
    id: &str,
) -> Result<Option<SessionSnapshot>, StorageError> {
    Ok(fold(&event::list_by_session(conn, id)?)
        .sessions
        .remove(id)
        .map(|value| value.snapshot))
}

/// 再調整で upsert した行数です。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileSummary {
    pub sessions_upserted: u64,
    pub tasks_upserted: u64,
}

/// イベントログを正としてセッションとタスクの行を一トランザクションで再調整します。
///
/// 帰属にはライフサイクル payload 内の ID ではなく envelope の `session_id` のみを使います。
pub fn reconcile(conn: &Connection) -> Result<ReconcileSummary, StorageError> {
    let state = fold(&event::list_all_ordered(conn)?);
    let summary = ReconcileSummary {
        sessions_upserted: u64::try_from(state.sessions.len())
            .map_err(|_| StorageError::OutOfRange("reconciled session count"))?,
        tasks_upserted: u64::try_from(state.tasks.len())
            .map_err(|_| StorageError::OutOfRange("reconciled task count"))?,
    };
    let tx = conn.unchecked_transaction()?;
    for value in state.sessions.into_values() {
        // イベント語彙は親セッションを持たないため parent_id は常に NULL です。
        tx.execute(
            "INSERT INTO sessions \
             (id,parent_id,status,failure_reason,delegated_to,total_event_bytes,created_at_ns,updated_at_ns) \
             VALUES (?1,NULL,?2,?3,?4,(SELECT COALESCE(SUM(OCTET_LENGTH(payload)),0) FROM events WHERE session_id = ?1),?5,?6) \
             ON CONFLICT(id) DO UPDATE SET status=excluded.status,failure_reason=excluded.failure_reason,delegated_to=excluded.delegated_to,total_event_bytes=excluded.total_event_bytes,updated_at_ns=excluded.updated_at_ns",
            params![value.snapshot.session_id, value.snapshot.status.as_str(), value.snapshot.failure_reason, value.snapshot.delegated_to, system_time_to_ns(value.first_seen)?, system_time_to_ns(value.last_seen)?],
        )?;
    }
    for (id, value) in state.tasks {
        tx.execute(
            "INSERT INTO tasks (id,session_id,status,created_at_ns,updated_at_ns) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,status=excluded.status,updated_at_ns=excluded.updated_at_ns",
            params![id, value.session_id, value.status.as_str(), system_time_to_ns(value.first_seen)?, system_time_to_ns(value.last_seen)?],
        )?;
    }
    tx.commit()?;
    Ok(summary)
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;
    use event_bus::{Event, EventMeta, FaultEvent, ProviderEvent, UsageEvent};
    use std::time::{Duration, UNIX_EPOCH};

    fn apply(kind: impl Into<EventKind>, session: Option<&str>) -> ProjectionState {
        let mut state = ProjectionState::default();
        apply_event(&mut state, &stored(kind, session));
        state
    }
    fn stored(kind: impl Into<EventKind>, session: Option<&str>) -> StoredEvent { StoredEvent { id: 1, session_id: session.map(String::from), event: Event { meta: EventMeta { schema_version: event_bus::SCHEMA_VERSION, monotonic: Duration::ZERO, wall_clock: UNIX_EPOCH }, kind: kind.into() } } }
    fn session(state: &ProjectionState) -> Option<&SessionSnapshot> {
        state.sessions.get("s1").map(|value| &value.snapshot)
    }
    macro_rules! status_test { ($name:ident, $event:expr, $status:expr) => { #[test] fn $name() { /* Given/When: 状態イベントを適用する */ let state = apply($event, Some("s1")); /* Then: 対応状態へ写像される */ assert_eq!(session(&state).map(|value| value.status), Some($status)); } }; }
    macro_rules! noop_test { ($name:ident, $event:expr) => { #[test] fn $name() { /* Given/When: 非投影イベントを適用する */ let state = apply($event, Some("s1")); /* Then: 状態を変更しない */ assert_eq!(state, ProjectionState::default()); } }; }

    status_test!(started_maps_running, LifecycleEvent::Started { session_id: "p".into() }, SessionStatus::Running);
    status_test!(completed_maps_completed, LifecycleEvent::Completed { session_id: "p".into() }, SessionStatus::Completed);
    #[test] fn delegated_maps_target() { /* Given/When: 委譲イベントを適用する */ let state = apply(LifecycleEvent::Delegated { session_id: "p".into(), target: "a".into() }, Some("s1")); /* Then: 状態と委譲先を写像する */ assert_eq!(session(&state).map(|v| (v.status, v.delegated_to.as_deref())), Some((SessionStatus::Delegated, Some("a")))); }
    #[test] fn failed_maps_reason() { /* Given/When: 失敗イベントを適用する */ let state = apply(LifecycleEvent::Failed { session_id: "p".into(), reason: "b".into() }, Some("s1")); /* Then: 状態と理由を写像する */ assert_eq!(session(&state).map(|v| (v.status, v.failure_reason.as_deref())), Some((SessionStatus::Failed, Some("b")))); }
    #[test] fn task_started_maps_running() { /* Given/When: タスク開始を適用する */ let state = apply(LifecycleEvent::BackgroundTaskStarted { task_id: "t".into() }, Some("s1")); /* Then: 状態と帰属を写像する */ assert_eq!(state.tasks.get("t").map(|v| (v.status, v.session_id.as_deref())), Some((TaskStatus::Running, Some("s1")))); assert_eq!(session(&state).map(|v| v.task_ids.as_slice()), Some(["t".into()].as_slice())); }
    #[test] fn task_completed_maps_completed() { /* Given/When: detached タスク完了を適用する */ let state = apply(LifecycleEvent::BackgroundTaskCompleted { task_id: "t".into() }, None); /* Then: 完了状態を保持する */ assert_eq!(state.tasks.get("t").map(|v| v.status), Some(TaskStatus::Completed)); }
    #[test] fn message_delta_appends() { /* Given/When: メッセージ差分を適用する */ let state = apply(MessageEvent::MessageDelta { delta: "m".into() }, Some("s1")); /* Then: 保留本文へ追加する */ assert_eq!(session(&state).map(|v| v.pending_message.as_str()), Some("m")); }
    #[test] fn reasoning_delta_appends() { /* Given/When: 推論差分を適用する */ let state = apply(MessageEvent::ReasoningDelta { delta: "r".into() }, Some("s1")); /* Then: 保留推論へ追加する */ assert_eq!(session(&state).map(|v| v.pending_reasoning.as_str()), Some("r")); }
    #[test] fn tool_started_opens_call() { /* Given/When: ツール開始を適用する */ let state = apply(ToolEvent::ToolStarted { tool_name: "x".into(), call_id: "c".into() }, Some("s1")); /* Then: 未完了呼び出しへ追加する */ assert_eq!(session(&state).map(|v| v.open_tool_calls.clone()), Some(vec![("x".into(), "c".into())])); }
    #[test] fn tool_completed_closes_call() { /* Given: 開いている呼び出し */ let mut state = apply(ToolEvent::ToolStarted { tool_name: "x".into(), call_id: "c".into() }, Some("s1")); /* When: 完了を適用する */ apply_event(&mut state, &stored(ToolEvent::ToolCompleted { tool_name: "x".into(), call_id: "c".into(), is_error: false }, Some("s1"))); /* Then: 未完了一覧から除く */ assert_eq!(session(&state).map(|v| v.open_tool_calls.as_slice()), Some([].as_slice())); }
    noop_test!(usage_does_not_mutate, UsageEvent::Usage { provider: "p".into(), model: "m".into(), input_tokens: 1, output_tokens: 2, cache_read_tokens: 3, cache_write_tokens: 4 });
    noop_test!(cache_stats_does_not_mutate, UsageEvent::CacheStats { provider: "p".into(), model: "m".into(), cache_hits: 1, cache_misses: 2 });
    noop_test!(provider_does_not_mutate, ProviderEvent::ProviderFallback { from_provider: "a".into(), to_provider: "b".into(), reason: "r".into() });
    noop_test!(fault_does_not_mutate, FaultEvent::SubscriberLagged { subscriber_id: 1, skipped: 2 });
}
