//! タスクのリポジトリを定義します。
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::StorageError;
use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::entity::{TaskRecord, TaskStatus};

type TaskRow = (String, Option<String>, String, i64, i64);

/// タスクを作成します。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "exercised by in-crate contract tests; writer commands are the production path"
    )
)]
pub fn create(conn: &Connection, record: &TaskRecord) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO tasks (id, session_id, status, created_at_ns, updated_at_ns) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![record.id, record.session_id, record.status.as_str(), system_time_to_ns(record.created_at)?, system_time_to_ns(record.updated_at)?],
    )?;
    Ok(())
}

/// 識別子に一致するタスクを返します。
pub fn get(conn: &Connection, id: &str) -> Result<Option<TaskRecord>, StorageError> {
    conn.query_row(
        "SELECT id, session_id, status, created_at_ns, updated_at_ns FROM tasks WHERE id = ?1",
        [id],
        task_row,
    )
    .optional()?
    .map(task_from_row)
    .transpose()
}

/// タスクの全フィールドを更新します。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "exercised by in-crate contract tests; writer commands are the production path"
    )
)]
pub fn update(conn: &Connection, record: &TaskRecord) -> Result<(), StorageError> {
    let changed = conn.execute(
        "UPDATE tasks SET session_id = ?2, status = ?3, created_at_ns = ?4, updated_at_ns = ?5 WHERE id = ?1",
        params![record.id, record.session_id, record.status.as_str(), system_time_to_ns(record.created_at)?, system_time_to_ns(record.updated_at)?],
    )?;
    if changed == 0 {
        return Err(StorageError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
    }
    Ok(())
}

/// タスクを削除し、削除件数が一件だったかを返します。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "exercised by in-crate contract tests; writer commands are the production path"
    )
)]
pub fn delete(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM tasks WHERE id = ?1", [id])? == 1)
}

/// セッションに属するタスクを作成日時順で返します。
pub fn list_by_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<TaskRecord>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT id, session_id, status, created_at_ns, updated_at_ns FROM tasks WHERE session_id = ?1 ORDER BY created_at_ns, id",
    )?;
    statement
        .query_map([session_id], task_row)?
        .map(|row| task_from_row(row?))
        .collect()
}

fn task_row(row: &Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn task_from_row(row: TaskRow) -> Result<TaskRecord, StorageError> {
    Ok(TaskRecord {
        id: row.0,
        session_id: row.1,
        status: TaskStatus::from_str(&row.2).ok_or_else(|| {
            StorageError::Serialization(format!("invalid task status: {}", row.2))
        })?,
        created_at: ns_to_system_time(row.3),
        updated_at: ns_to_system_time(row.4),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use crate::entity::{SessionRecord, SessionStatus};
    use crate::repo::session;
    use std::time::{Duration, UNIX_EPOCH};

    fn record(id: &str, seconds: u64) -> TaskRecord {
        let timestamp = UNIX_EPOCH + Duration::from_secs(seconds);
        TaskRecord {
            id: id.into(),
            session_id: Some("session".into()),
            status: TaskStatus::Running,
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    #[test]
    fn crud_and_list_by_session_round_trip_all_fields() {
        // Given: セッションと作成日時が異なる三つのタスク
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
            record("task-1", 10),
            record("task-2", 20),
            record("task-3", 30),
        ];

        // When: タスクを逆順で作成して一件を更新する
        for item in records.iter().rev() {
            create(&database.conn, item).expect("task must create");
        }
        let updated = TaskRecord {
            status: TaskStatus::Completed,
            updated_at: UNIX_EPOCH + Duration::from_secs(40),
            ..records[1].clone()
        };
        update(&database.conn, &updated).expect("task must update");

        // Then: 全値、並び順、削除結果が契約どおりである
        assert_eq!(get(&database.conn, "task-2").unwrap(), Some(updated));
        let mut expected = records;
        expected[1].status = TaskStatus::Completed;
        expected[1].updated_at = UNIX_EPOCH + Duration::from_secs(40);
        assert_eq!(
            list_by_session(&database.conn, "session").unwrap(),
            expected
        );
        assert!(delete(&database.conn, "task-2").unwrap());
        assert_eq!(get(&database.conn, "task-2").unwrap(), None);
    }
}
