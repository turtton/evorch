//! メッセージのリポジトリを定義します。
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::StorageError;
use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::entity::{MessageRecord, MessageRole, SecretGuard};

type MessageRow = (String, String, String, String, Option<String>, i64, i64);

/// メッセージを作成します。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "exercised by in-crate contract tests; writer commands are the production path"
    )
)]
pub fn create(conn: &Connection, record: &MessageRecord) -> Result<(), StorageError> {
    SecretGuard::from_env().check_message_record(record)?;
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "exercised by in-crate contract tests; writer commands are the production path"
    )
)]
pub fn update(conn: &Connection, record: &MessageRecord) -> Result<(), StorageError> {
    SecretGuard::from_env().check_message_record(record)?;
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "exercised by in-crate contract tests; writer commands are the production path"
    )
)]
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

    const SHAPE_SECRET: &str = "sk-test-evorch-9f8e7d6c5b4a3f2e1d";
    const KNOWN_ENV: &str = "GH_TOKEN";
    const KNOWN_SENTINEL: &str = "evorch-msg-known-sentinel-71039afd-0123456789";

    fn session_and_db() -> Database {
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
        database
    }

    #[test]
    fn create_and_update_reject_secret_shaped_text_without_touching_db() {
        // Given: セッションと通常の既存メッセージ
        let database = session_and_db();
        let secret = format!("leak {SHAPE_SECRET}");
        let bad = MessageRecord {
            id: "m-bad".into(),
            session_id: "session".into(),
            role: MessageRole::User,
            content: secret.clone(),
            reasoning: None,
            created_at: UNIX_EPOCH,
            updated_at: UNIX_EPOCH,
        };

        // When: secret 形状の本文を持つレコードを create する
        let Err(error) = create(&database.conn, &bad) else {
            panic!("secret-shaped content must be rejected");
        };

        // Then: field=content で拒否され、DB 行は作られない
        assert!(matches!(
            error,
            StorageError::SecretDetected {
                entity: "message",
                field: "content",
                ..
            }
        ));
        assert_eq!(get(&database.conn, "m-bad").unwrap(), None);

        // Given: 正常に作成された既存レコード
        let clean = MessageRecord {
            id: "m-ok".into(),
            session_id: "session".into(),
            role: MessageRole::User,
            content: "hello".into(),
            reasoning: None,
            created_at: UNIX_EPOCH,
            updated_at: UNIX_EPOCH,
        };
        create(&database.conn, &clean).expect("clean record must create");

        // When: reasoning へ secret 形状値を含む更新を試みる
        let bad_update = MessageRecord {
            reasoning: Some(secret),
            ..clean.clone()
        };
        let Err(error) = update(&database.conn, &bad_update) else {
            panic!("secret-shaped reasoning must be rejected");
        };

        // Then: field=reasoning で拒否され、既存行は不変
        assert!(matches!(
            error,
            StorageError::SecretDetected {
                entity: "message",
                field: "reasoning",
                ..
            }
        ));
        assert_eq!(get(&database.conn, "m-ok").unwrap(), Some(clean));
    }

    #[test]
    fn create_rejects_known_credential_env_value() {
        // Given: 限定 credential env 名に注入された既知値
        let previous = std::env::var(KNOWN_ENV).ok();
        // SAFETY: テストプロセス内で一意の sentinel のみを設定し、
        // 終了時に元の値へ復元する。他テストの fixture は sentinel を含まない。
        unsafe { std::env::set_var(KNOWN_ENV, KNOWN_SENTINEL) };
        let database = session_and_db();
        let bad = MessageRecord {
            id: "m-known".into(),
            session_id: "session".into(),
            role: MessageRole::User,
            content: format!("key is {KNOWN_SENTINEL}"),
            reasoning: None,
            created_at: UNIX_EPOCH,
            updated_at: UNIX_EPOCH,
        };

        // When: 既知値を含むレコードを create する
        let result = create(&database.conn, &bad);
        let stored = get(&database.conn, "m-known").unwrap();
        // SAFETY: 上記と同じ sentinel の後始末で、外部環境へ影響を残さない。
        unsafe {
            match &previous {
                Some(value) => std::env::set_var(KNOWN_ENV, value),
                None => std::env::remove_var(KNOWN_ENV),
            }
        }

        // Then: known-credential-value 規則で拒否され、DB 行は作られない
        let Err(error) = result else {
            panic!("known credential value must be rejected");
        };
        assert!(matches!(
            error,
            StorageError::SecretDetected {
                entity: "message",
                field: "content",
                rule: crate::error::SecretRule::KnownCredentialValue,
                ..
            }
        ));
        assert_eq!(stored, None);
    }

    #[test]
    fn create_accepts_normal_prose_and_short_token_like_values() {
        // Given: 通常文と短い token 風文字列を含むレコード群
        let database = session_and_db();
        let corpus = [
            "hello, this is a normal message",
            "これは通常の日本語の文章です。",
            "abc12345",
            "ghp_short",
            "sk-x",
            "123e4567-e89b-12d3-a456-426614174000",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4",
        ];

        // When / Then: すべて受け入れ、全件が永続化される
        for (index, content) in corpus.iter().enumerate() {
            let record = MessageRecord {
                id: format!("m-neg-{index}"),
                session_id: "session".into(),
                role: MessageRole::User,
                content: (*content).to_owned(),
                reasoning: Some("short note".into()),
                created_at: UNIX_EPOCH,
                updated_at: UNIX_EPOCH,
            };
            create(&database.conn, &record).expect("negative corpus must be accepted");
        }
        assert_eq!(
            list_by_session(&database.conn, "session").unwrap().len(),
            corpus.len()
        );
    }
}
