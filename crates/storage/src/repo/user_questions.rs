use crate::{StorageError, entity::SecretGuard};
use event_bus::UserQuestion;
use rusqlite::{Connection, OptionalExtension, params};

pub fn create(conn: &Connection, question: &UserQuestion) -> Result<(), StorageError> {
    if question.id.is_empty()
        || question.id.len() > 128
        || question.run_id.is_empty()
        || question.run_id.len() > 64
        || question.root_run_id.is_empty()
        || question.root_run_id.len() > 64
        || question.root_name.len() > 1024
        || question.title.trim().is_empty()
        || question.title.len() > 2048
        || question.options.len() > 3
        || question
            .options
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 256)
        || question.answer.is_some()
    {
        return Err(invalid(
            "invalid question: title 1..2048 bytes, at most 3 options of 1..256 bytes",
        ));
    }
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM user_questions q WHERE run_id=?1 OR EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=q.id AND l.run_id=?1)",
        [&question.run_id],
        |row| row.get(0),
    )?;
    if count >= 32 {
        return Err(invalid("question limit reached (32 per run)"));
    }
    let pending: i64 = conn.query_row(
        "SELECT count(*) FROM user_questions WHERE answered=0",
        [],
        |row| row.get(0),
    )?;
    if pending >= 1024 {
        return Err(invalid(
            "pending question limit reached (1024); answer existing questions first",
        ));
    }
    let payload = serde_json::to_string(question).map_err(|e| invalid(&e.to_string()))?;
    SecretGuard::from_env().check_text("user_question", "payload", &payload)?;
    conn.execute("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES (?1,?2,?3,?4,?5)",
        params![question.id, question.run_id, question.root_run_id, question.root_name, payload])?;
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<UserQuestion>, StorageError> {
    let json: Option<String> = conn
        .query_row(
            "SELECT payload FROM user_questions WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|s| serde_json::from_str(&s).map_err(|e| invalid(&e.to_string())))
        .transpose()
}

pub fn answer(conn: &Connection, id: &str, answer: &str) -> Result<(), StorageError> {
    if answer.trim().is_empty() || answer.len() > 4096 {
        return Err(invalid("answer must be 1..4096 bytes"));
    }
    SecretGuard::from_env().check_text("user_question", "answer", answer)?;
    let mut question = get(conn, id)?.ok_or_else(|| invalid("unknown question"))?;
    if let Some(previous) = &question.answer {
        return if previous == answer {
            Ok(())
        } else {
            Err(invalid("question already answered"))
        };
    }
    question.answer = Some(answer.into());
    let payload = serde_json::to_string(&question).map_err(|e| invalid(&e.to_string()))?;
    conn.execute(
        "UPDATE user_questions SET payload=?2,answered=1 WHERE id=?1 AND answered=0",
        params![id, payload],
    )?;
    Ok(())
}

pub fn for_run(conn: &Connection, run_id: &str) -> Result<Vec<UserQuestion>, StorageError> {
    let questions = list(
        conn,
        "SELECT q.payload FROM user_questions q WHERE q.run_id=?1 OR EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=q.id AND l.run_id=?1) ORDER BY q.rowid LIMIT 33",
        run_id,
    )?;
    if questions.len() > 32 {
        return Err(invalid(
            "question recipient limit exceeded (32); refusing to omit questions",
        ));
    }
    Ok(questions)
}

/// Explicit continuation inheritance; caller-chosen IDs must already belong to
/// source. Links preserve the original requester's provenance in the payload.
pub fn bind(
    conn: &Connection,
    source: &str,
    target: &str,
    ids: &[String],
) -> Result<(), StorageError> {
    if source.is_empty()
        || target.is_empty()
        || source.len() > 64
        || target.len() > 64
        || ids.len() > 32
    {
        return Err(invalid("invalid question inheritance"));
    }
    let source_questions = for_run(conn, source)?;
    if ids
        .iter()
        .any(|id| !source_questions.iter().any(|question| &question.id == id))
    {
        return Err(invalid(
            "question inheritance source does not own every question",
        ));
    }
    let mut target_ids = for_run(conn, target)?
        .into_iter()
        .map(|question| question.id)
        .collect::<std::collections::HashSet<_>>();
    target_ids.extend(ids.iter().cloned());
    if target_ids.len() > 32 {
        return Err(invalid("question recipient limit reached (32)"));
    }
    let transaction = conn.unchecked_transaction()?;
    for id in ids {
        transaction.execute(
            "INSERT OR IGNORE INTO user_question_links(question_id,run_id) VALUES (?1,?2)",
            params![id, target],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

pub fn recipients(conn: &Connection, id: &str) -> Result<Vec<String>, StorageError> {
    let mut statement = conn.prepare("SELECT run_id FROM user_questions WHERE id=?1 UNION SELECT run_id FROM user_question_links WHERE question_id=?1")?;
    let rows = statement.query_map([id], |row| row.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn pending(conn: &Connection) -> Result<Vec<UserQuestion>, StorageError> {
    list(
        conn,
        "SELECT payload FROM user_questions WHERE answered=0 AND ?1='' ORDER BY rowid DESC LIMIT 1024",
        "",
    )
}

fn list(conn: &Connection, sql: &str, arg: &str) -> Result<Vec<UserQuestion>, StorageError> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map([arg], |row| row.get::<_, String>(0))?;
    rows.map(|s| serde_json::from_str(&s?).map_err(|e| invalid(&e.to_string())))
        .collect()
}
fn invalid(message: &str) -> StorageError {
    StorageError::Serialization(message.into())
}
