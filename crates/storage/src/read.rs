//! `Database` の read-only facade を定義します。

use event_bus::{AgentMessage, AgentMessageEvent, DeliveryDisposition, EventKind, UsageBucket};

use crate::entity::{AgentRunRecord, MessageRecord, SessionRecord, TaskRecord};
use crate::projection;
use crate::repo::{agent_run, event, message, metrics, session, task};
use crate::{Database, SessionSnapshot, StorageError, StoredEvent};

/// 永続化済みの AgentMessage 配送です。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredAgentMessage {
    /// 配送されたメッセージ封筒です。
    pub message: AgentMessage,
    /// 配送時に確定した受信側での扱いです。
    pub disposition: DeliveryDisposition,
}

impl Database {
    /// 識別子に一致するセッションを返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn session(&self, id: &str) -> Result<Option<SessionRecord>, StorageError> {
        session::get(&self.conn, id)
    }

    /// 親セッションに属する子セッションを作成日時順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn sessions_by_parent(&self, parent_id: &str) -> Result<Vec<SessionRecord>, StorageError> {
        session::list_by_parent(&self.conn, parent_id)
    }

    /// 識別子に一致するタスクを返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn task(&self, id: &str) -> Result<Option<TaskRecord>, StorageError> {
        task::get(&self.conn, id)
    }

    /// セッションに属するタスクを作成日時順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn tasks_by_session(&self, session_id: &str) -> Result<Vec<TaskRecord>, StorageError> {
        task::list_by_session(&self.conn, session_id)
    }

    /// 識別子に一致するメッセージを返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn message(&self, id: &str) -> Result<Option<MessageRecord>, StorageError> {
        message::get(&self.conn, id)
    }

    /// セッションに属するメッセージを作成日時順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn messages_by_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        message::list_by_session(&self.conn, session_id)
    }

    /// 識別子に一致するエージェント実行記録を返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn agent_run(&self, id: &str) -> Result<Option<AgentRunRecord>, StorageError> {
        agent_run::get(&self.conn, id)
    }

    /// セッションに属するエージェント実行記録を開始日時順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn agent_runs_by_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentRunRecord>, StorageError> {
        agent_run::list_by_session(&self.conn, session_id)
    }

    /// セッションに属するイベントを採番順で返します。
    ///
    /// # Errors
    /// SQLite 操作またはイベントの復元に失敗した場合にエラーを返します。
    pub fn events_by_session(&self, session_id: &str) -> Result<Vec<StoredEvent>, StorageError> {
        event::list_by_session(&self.conn, session_id)
    }

    /// 指定セッションの AgentMessage 配送を insertion 順に復元する。
    ///
    /// # Errors
    /// SQLite 操作または保存済み AgentMessage イベントの復元に失敗した場合にエラーを返します。
    pub fn agent_messages_by_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredAgentMessage>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT id, kind, payload FROM events \
             WHERE session_id = ?1 AND kind = 'AgentMessage' ORDER BY id ASC",
        )?;
        let mut rows = statement.query(rusqlite::params![session_id])?;
        let mut messages = Vec::new();

        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let payload: String = row.get(2)?;
            let event_kind = serde_json::from_str::<EventKind>(&payload).map_err(|error| {
                StorageError::Serialization(format!("event id {id} kind {kind}: {error}"))
            })?;

            match event_kind {
                EventKind::AgentMessage(AgentMessageEvent::Delivered {
                    message,
                    disposition,
                }) => messages.push(StoredAgentMessage {
                    message,
                    disposition,
                }),
                EventKind::Lifecycle(_)
                | EventKind::Message(_)
                | EventKind::Tool(_)
                | EventKind::Usage(_)
                | EventKind::Provider(_)
                | EventKind::Fault(_)
                | EventKind::Compaction(_) => {
                    return Err(StorageError::Serialization(format!(
                        "event id {id} kind {kind}: payload event kind does not match kind column"
                    )));
                }
            }
        }

        Ok(messages)
    }

    /// 全イベントを採番順で返します。
    ///
    /// # Errors
    /// SQLite 操作またはイベントの復元に失敗した場合にエラーを返します。
    pub fn events_all_ordered(&self) -> Result<Vec<StoredEvent>, StorageError> {
        event::list_all_ordered(&self.conn)
    }

    /// inclusive な window_start 範囲の usage バケットをキー順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn metrics_range(
        &self,
        from_window_start: u64,
        to_window_start: u64,
    ) -> Result<Vec<UsageBucket>, StorageError> {
        metrics::list_range(&self.conn, from_window_start, to_window_start)
    }

    /// 指定セッションのイベントを採番順で畳み込み、復元状態を返します。
    ///
    /// # Errors
    /// SQLite 操作またはイベントの復元に失敗した場合にエラーを返します。
    pub fn restore_session(&self, id: &str) -> Result<Option<SessionSnapshot>, StorageError> {
        projection::restore_session(&self.conn, id)
    }

    /// 全イベントを採番順で畳み込み、セッション ID 順の復元状態を返します。
    ///
    /// # Errors
    /// SQLite 操作またはイベントの復元に失敗した場合にエラーを返します。
    pub fn restore_sessions(&self) -> Result<Vec<SessionSnapshot>, StorageError> {
        projection::restore_sessions(&self.conn)
    }
}
