//! メッセージのリポジトリを定義します。
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "storage services consume this staged crate-private repository in the next task"
    )
)]

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::StorageError;
use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::entity::{MessageRecord, MessageRole};

type MessageRow = (String, String, String, String, Option<String>, i64, i64);

/// メッセージを作成します。
pub fn create(conn: &Connection, record: &MessageRecord) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO messages (id, session_id, role, content, reasoning, created_at_ns, updated_at_ns) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![record.id, record.session_id, record.role.as_str(), record.content, record.reasoning, system_time_to_ns(record.created_at)?, system_time_to_ns(record.updated_at)?],
    )?;
    Ok(())
}

/// 識別子に一致するメッセージを返します。
pub fn get(conn: &Connection, id: &str) -> Result<Option<MessageRecord>, StorageError> {
    conn.query_row(
        "SELECT id, session_id, role, content, reasoning, created_at_ns, updated_at_ns FROM messages WHERE id = ?1",
        [id],
        message_row,
    )
    .optional()?
    .map(message_from_row)
    .transpose()
}

/// メッセージの全フィールドを更新します。
pub fn update(conn: &Connection, record: &MessageRecord) -> Result<(), StorageError> {
    let changed = conn.execute(
        "UPDATE messages SET session_id = ?2, role = ?3, content = ?4, reasoning = ?5, created_at_ns = ?6, updated_at_ns = ?7 WHERE id = ?1",
        params![record.id, record.session_id, record.role.as_str(), record.content, record.reasoning, system_time_to_ns(record.created_at)?, system_time_to_ns(record.updated_at)?],
    )?;
    if changed == 0 {
        return Err(StorageError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
    }
    Ok(())
}

/// メッセージを削除し、削除件数が一件だったかを返します。
pub fn delete(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM messages WHERE id = ?1", [id])? == 1)
}

/// セッションに属するメッセージを作成日時順で返します。
pub fn list_by_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<MessageRecord>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT id, session_id, role, content, reasoning, created_at_ns, updated_at_ns FROM messages WHERE session_id = ?1 ORDER BY created_at_ns, id",
    )?;
    statement
        .query_map([session_id], message_row)?
        .map(|row| message_from_row(row?))
        .collect()
}

fn message_row(row: &Row<'_>) -> rusqlite::Result<MessageRow> {
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

fn message_from_row(row: MessageRow) -> Result<MessageRecord, StorageError> {
    Ok(MessageRecord {
        id: row.0,
        session_id: row.1,
        role: MessageRole::from_str(&row.2).ok_or_else(|| {
            StorageError::Serialization(format!("invalid message role: {}", row.2))
        })?,
        content: row.3,
        reasoning: row.4,
        created_at: ns_to_system_time(row.5),
        updated_at: ns_to_system_time(row.6),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use crate::entity::{SessionRecord, SessionStatus};
    use crate::repo::session;
    use std::time::{Duration, UNIX_EPOCH};

    fn record(id: &str, seconds: u64) -> MessageRecord {
        let timestamp = UNIX_EPOCH + Duration::from_secs(seconds);
        MessageRecord {
            id: id.into(),
            session_id: "session".into(),
            role: MessageRole::User,
            content: seconds.to_string(),
            reasoning: None,
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    #[test]
    fn crud_and_list_by_session_round_trip_all_fields() {
        // Given: セッションと作成日時が異なる三つのメッセージ
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
            record("message-1", 10),
            record("message-2", 20),
            record("message-3", 30),
        ];

        // When: メッセージを逆順で作成して一件を更新する
        for item in records.iter().rev() {
            create(&database.conn, item).expect("message must create");
        }
        let updated = MessageRecord {
            role: MessageRole::Assistant,
            reasoning: Some("reason".into()),
            updated_at: UNIX_EPOCH + Duration::from_secs(40),
            ..records[1].clone()
        };
        update(&database.conn, &updated).expect("message must update");

        // Then: 全値、並び順、削除結果が契約どおりである
        assert_eq!(get(&database.conn, "message-2").unwrap(), Some(updated));
        let mut expected = records;
        expected[1].role = MessageRole::Assistant;
        expected[1].reasoning = Some("reason".into());
        expected[1].updated_at = UNIX_EPOCH + Duration::from_secs(40);
        assert_eq!(
            list_by_session(&database.conn, "session").unwrap(),
            expected
        );
        assert!(delete(&database.conn, "message-2").unwrap());
        assert_eq!(get(&database.conn, "message-2").unwrap(), None);
    }
}
