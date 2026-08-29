//! エージェント実行記録のリポジトリを定義します。
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::StorageError;
use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::entity::{AgentRunRecord, AgentRunStatus};

type AgentRunRow = (String, String, String, String, String, i64, Option<i64>);

/// エージェント実行記録を作成します。
pub fn create(conn: &Connection, record: &AgentRunRecord) -> Result<(), StorageError> {
    let finished_at_ns = record.finished_at.map(system_time_to_ns).transpose()?;
    conn.execute(
        "INSERT INTO agent_runs (id, session_id, provider, model, status, started_at_ns, finished_at_ns) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![record.id, record.session_id, record.provider, record.model, record.status.as_str(), system_time_to_ns(record.started_at)?, finished_at_ns],
    )?;
    Ok(())
}

/// 識別子に一致するエージェント実行記録を返します。
pub fn get(conn: &Connection, id: &str) -> Result<Option<AgentRunRecord>, StorageError> {
    conn.query_row(
        "SELECT id, session_id, provider, model, status, started_at_ns, finished_at_ns FROM agent_runs WHERE id = ?1",
        [id],
        agent_run_row,
    )
    .optional()?
    .map(agent_run_from_row)
    .transpose()
}

/// エージェント実行記録の全フィールドを更新します。
pub fn update(conn: &Connection, record: &AgentRunRecord) -> Result<(), StorageError> {
    let finished_at_ns = record.finished_at.map(system_time_to_ns).transpose()?;
    let changed = conn.execute(
        "UPDATE agent_runs SET session_id = ?2, provider = ?3, model = ?4, status = ?5, started_at_ns = ?6, finished_at_ns = ?7 WHERE id = ?1",
        params![record.id, record.session_id, record.provider, record.model, record.status.as_str(), system_time_to_ns(record.started_at)?, finished_at_ns],
    )?;
    if changed == 0 {
        return Err(StorageError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
    }
    Ok(())
}

/// エージェント実行記録を削除し、削除件数が一件だったかを返します。
pub fn delete(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM agent_runs WHERE id = ?1", [id])? == 1)
}

/// セッションに属する実行記録を開始日時順で返します。
pub fn list_by_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<AgentRunRecord>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT id, session_id, provider, model, status, started_at_ns, finished_at_ns FROM agent_runs WHERE session_id = ?1 ORDER BY started_at_ns, id",
    )?;
    statement
        .query_map([session_id], agent_run_row)?
        .map(|row| agent_run_from_row(row?))
        .collect()
}

fn agent_run_row(row: &Row<'_>) -> rusqlite::Result<AgentRunRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn agent_run_from_row(row: AgentRunRow) -> Result<AgentRunRecord, StorageError> {
    Ok(AgentRunRecord {
        id: row.0,
        session_id: row.1,
        provider: row.2,
        model: row.3,
        status: AgentRunStatus::from_str(&row.4).ok_or_else(|| {
            StorageError::Serialization(format!("invalid agent run status: {}", row.4))
        })?,
        started_at: ns_to_system_time(row.5),
        finished_at: row.6.map(ns_to_system_time),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use crate::entity::{SessionRecord, SessionStatus};
    use crate::repo::session;
    use std::time::{Duration, UNIX_EPOCH};

    fn record(id: &str, seconds: u64) -> AgentRunRecord {
        AgentRunRecord {
            id: id.into(),
            session_id: "session".into(),
            provider: "provider".into(),
            model: "model".into(),
            status: AgentRunStatus::Running,
            started_at: UNIX_EPOCH + Duration::from_secs(seconds),
            finished_at: None,
        }
    }

    #[test]
    fn crud_and_list_by_session_round_trip_all_fields() {
        // Given: セッションと開始日時が異なる三つの実行記録
        let database = Database::open_in_memory().expect("database must open");
        let timestamp = UNIX_EPOCH + Duration::from_secs(1);
        session::create(
            &database.conn,
            &SessionRecord {
                id: "session".into(),
                parent_id: None,
                status: SessionStatus::Running,
                failure_reason: None,
                delegated_to: None,
                total_event_bytes: 0,
                created_at: timestamp,
                updated_at: timestamp,
            },
        )
        .expect("session must create");
        let records = [
            record("run-1", 10),
            record("run-2", 20),
            record("run-3", 30),
        ];

        // When: 実行記録を逆順で作成して一件を更新する
        for item in records.iter().rev() {
            create(&database.conn, item).expect("run must create");
        }
        let updated = AgentRunRecord {
            status: AgentRunStatus::Completed,
            finished_at: Some(UNIX_EPOCH + Duration::from_secs(40)),
            ..records[1].clone()
        };
        update(&database.conn, &updated).expect("run must update");

        // Then: 全値、並び順、削除結果が契約どおりである
        assert_eq!(get(&database.conn, "run-2").unwrap(), Some(updated));
        let mut expected = records;
        expected[1].status = AgentRunStatus::Completed;
        expected[1].finished_at = Some(UNIX_EPOCH + Duration::from_secs(40));
        assert_eq!(
            list_by_session(&database.conn, "session").unwrap(),
            expected
        );
        assert!(delete(&database.conn, "run-2").unwrap());
        assert_eq!(get(&database.conn, "run-2").unwrap(), None);
    }
}
