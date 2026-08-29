//! セッションのリポジトリを定義します。
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::StorageError;
use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::entity::{SessionRecord, SessionStatus};

type SessionRow = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    i64,
);

/// セッションを作成します。
pub fn create(conn: &Connection, record: &SessionRecord) -> Result<(), StorageError> {
    let total_event_bytes = i64::try_from(record.total_event_bytes)
        .map_err(|_| StorageError::OutOfRange("total_event_bytes"))?;
    conn.execute(
        "INSERT INTO sessions (id, parent_id, status, failure_reason, delegated_to, total_event_bytes, created_at_ns, updated_at_ns) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![record.id, record.parent_id, record.status.as_str(), record.failure_reason, record.delegated_to, total_event_bytes, system_time_to_ns(record.created_at)?, system_time_to_ns(record.updated_at)?],
    )?;
    Ok(())
}

/// 識別子に一致するセッションを返します。
pub fn get(conn: &Connection, id: &str) -> Result<Option<SessionRecord>, StorageError> {
    conn.query_row(
        "SELECT id, parent_id, status, failure_reason, delegated_to, total_event_bytes, created_at_ns, updated_at_ns FROM sessions WHERE id = ?1",
        [id],
        session_row,
    )
    .optional()?
    .map(session_from_row)
    .transpose()
}

/// セッションの全フィールドを更新します。
pub fn update(conn: &Connection, record: &SessionRecord) -> Result<(), StorageError> {
    let total_event_bytes = i64::try_from(record.total_event_bytes)
        .map_err(|_| StorageError::OutOfRange("total_event_bytes"))?;
    let changed = conn.execute(
        "UPDATE sessions SET parent_id = ?2, status = ?3, failure_reason = ?4, delegated_to = ?5, total_event_bytes = ?6, created_at_ns = ?7, updated_at_ns = ?8 WHERE id = ?1",
        params![record.id, record.parent_id, record.status.as_str(), record.failure_reason, record.delegated_to, total_event_bytes, system_time_to_ns(record.created_at)?, system_time_to_ns(record.updated_at)?],
    )?;
    if changed == 0 {
        return Err(StorageError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
    }
    Ok(())
}

/// セッションを削除し、削除件数が一件だったかを返します。
pub fn delete(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM sessions WHERE id = ?1", [id])? == 1)
}

/// 親セッションに属する子セッションを作成日時順で返します。
pub fn list_by_parent(
    conn: &Connection,
    parent_id: &str,
) -> Result<Vec<SessionRecord>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT id, parent_id, status, failure_reason, delegated_to, total_event_bytes, created_at_ns, updated_at_ns FROM sessions WHERE parent_id = ?1 ORDER BY created_at_ns, id",
    )?;
    statement
        .query_map([parent_id], session_row)?
        .map(|row| session_from_row(row?))
        .collect()
}

fn session_row(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn session_from_row(row: SessionRow) -> Result<SessionRecord, StorageError> {
    Ok(SessionRecord {
        id: row.0,
        parent_id: row.1,
        status: SessionStatus::from_str(&row.2).ok_or_else(|| {
            StorageError::Serialization(format!("invalid session status: {}", row.2))
        })?,
        failure_reason: row.3,
        delegated_to: row.4,
        total_event_bytes: u64::try_from(row.5)
            .map_err(|_| StorageError::OutOfRange("total_event_bytes"))?,
        created_at: ns_to_system_time(row.6),
        updated_at: ns_to_system_time(row.7),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use std::time::{Duration, UNIX_EPOCH};

    fn record(id: &str, parent_id: Option<&str>, seconds: u64) -> SessionRecord {
        let timestamp = UNIX_EPOCH + Duration::from_secs(seconds);
        SessionRecord {
            id: id.into(),
            parent_id: parent_id.map(String::from),
            status: SessionStatus::Running,
            failure_reason: None,
            delegated_to: None,
            total_event_bytes: seconds,
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    #[test]
    fn crud_and_list_by_parent_round_trip_all_fields() {
        // Given: 親と作成日時が異なる三つの子セッション
        let database = Database::open_in_memory().expect("database must open");
        let parent = record("parent", None, 1);
        create(&database.conn, &parent).expect("parent must create");
        let children = [
            record("child-1", Some("parent"), 10),
            record("child-2", Some("parent"), 20),
            record("child-3", Some("parent"), 30),
        ];

        // When: 子を逆順で作成して一件を更新する
        for child in children.iter().rev() {
            create(&database.conn, child).expect("child must create");
        }
        let updated = SessionRecord {
            status: SessionStatus::Completed,
            updated_at: UNIX_EPOCH + Duration::from_secs(40),
            ..children[1].clone()
        };
        update(&database.conn, &updated).expect("child must update");

        // Then: 全値、並び順、削除結果が契約どおりである
        assert_eq!(get(&database.conn, "child-2").unwrap(), Some(updated));
        let mut expected = children;
        expected[1].status = SessionStatus::Completed;
        expected[1].updated_at = UNIX_EPOCH + Duration::from_secs(40);
        assert_eq!(list_by_parent(&database.conn, "parent").unwrap(), expected);
        assert!(delete(&database.conn, "child-2").unwrap());
        assert_eq!(get(&database.conn, "child-2").unwrap(), None);
    }
}
