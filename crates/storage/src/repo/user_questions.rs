use crate::{StorageError, entity::SecretGuard};
use event_bus::{AnswerOrigin, UserQuestion};
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
        || question.answer_origin.is_some()
    {
        return Err(invalid(
            "invalid question: title 1..2048 bytes, at most 3 options of 1..256 bytes",
        ));
    }
    let count = requester_question_count(conn, &question.run_id)?;
    if count >= u64::from(crate::lane_delivery::QuestionCapacity::Requester.limit()) {
        return Err(StorageError::QuestionCapacity(
            crate::lane_delivery::QuestionCapacity::Requester,
        ));
    }
    let payload = serde_json::to_string(question).map_err(|e| invalid(&e.to_string()))?;
    SecretGuard::from_env().check_text("user_question", "payload", &payload)?;
    conn.execute("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES (?1,?2,?3,?4,?5)",
        params![question.id, question.run_id, question.root_run_id, question.root_name, payload])?;
    Ok(())
}

/// Unanswered questions that count against one recipient's quota: those the run
/// owns plus those it inherited through `user_question_links`. Answering frees a
/// slot. Questions that are no longer visible (owner run or root tombstoned, with
/// no live inherited recipient), the same set `get` and the pending listings use,
/// are not counted. Creation and attention must use this same count.
pub(super) fn requester_question_count(
    conn: &Connection,
    run_id: &str,
) -> Result<u64, StorageError> {
    Ok(conn.query_row(
        "SELECT count(*) FROM user_questions q WHERE q.answered=0 \
         AND (q.run_id=?1 OR EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=q.id AND l.run_id=?1)) \
         AND ((q.run_id NOT IN (SELECT run_id FROM run_tombstones) AND q.root_run_id NOT IN (SELECT run_id FROM run_tombstones)) \
              OR EXISTS (SELECT 1 FROM user_question_links l2 WHERE l2.question_id=q.id AND l2.run_id != q.run_id AND l2.run_id NOT IN (SELECT run_id FROM run_tombstones)))",
        [run_id], |row| row.get(0),
    )?)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<UserQuestion>, StorageError> {
    let json: Option<String> = conn
        .query_row(
            "SELECT payload FROM user_questions WHERE id=?1 AND ((run_id NOT IN (SELECT run_id FROM run_tombstones) AND root_run_id NOT IN (SELECT run_id FROM run_tombstones)) OR EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=user_questions.id AND l.run_id != user_questions.run_id AND l.run_id NOT IN (SELECT run_id FROM run_tombstones)))",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|s| hydrate(conn, &s)).transpose()
}

/// What a caller needs when the question already has an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerMode {
    /// Repeating the stored answer succeeds; a different one is rejected.
    Idempotent,
    /// Succeeds only for the answer that is stored by this call. Any earlier answer,
    /// even an identical one, fails with [`StorageError::QuestionAlreadyAnswered`],
    /// so the caller knows it was not the one who answered.
    FirstOnly,
}

pub fn answer(
    conn: &Connection,
    id: &str,
    answer: &str,
    origin: &AnswerOrigin,
    mode: AnswerMode,
) -> Result<(), StorageError> {
    if conn.is_autocommit() {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        answer_checked(&tx, id, answer, origin, mode)?;
        tx.commit()?;
        return Ok(());
    }
    answer_checked(conn, id, answer, origin, mode)
}
fn answer_checked(
    conn: &Connection,
    id: &str,
    answer: &str,
    origin: &AnswerOrigin,
    mode: AnswerMode,
) -> Result<(), StorageError> {
    // Canonical routing, not a cached projection or inherited recipient link.
    let canonical: Option<String> = conn
        .query_row(
            "SELECT payload FROM lane_question_routes WHERE question_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(canonical) = &canonical {
        let route: event_bus::lane::QuestionRouting =
            serde_json::from_str(canonical).map_err(|e| invalid(&e.to_string()))?;
        use event_bus::lane::{LaneOperationError, QuestionResponder};
        let permitted = match (&route.responder, origin) {
            (QuestionResponder::User, AnswerOrigin::Host { .. }) => true,
            (
                QuestionResponder::Lead { thread_id },
                AnswerOrigin::LaneLead { thread_id: by, .. },
            ) if thread_id == by => {
                crate::repo::lanes::get(conn, &route.lane_id)?.is_some_and(|l| {
                    l.policy == event_bus::LanePolicy::Attached
                        && l.lead_thread_id.as_deref() == Some(by)
                        && l.management_revision == route.management_revision
                })
            }
            _ => false,
        };
        if !permitted {
            return Err(StorageError::LaneOperation(
                LaneOperationError::NotResponder,
            ));
        }
    }
    if answer.trim().is_empty() || answer.len() > 4096 {
        return Err(invalid("answer must be 1..4096 bytes"));
    }
    if matches!(origin, AnswerOrigin::ParentAgent { answered_by_run_id }
        if answered_by_run_id.is_empty() || answered_by_run_id.len() > 64)
    {
        return Err(invalid("answering run ID must be 1..64 bytes"));
    }
    SecretGuard::from_env().check_text("user_question", "answer", answer)?;
    let mut question = get(conn, id)?.ok_or_else(|| invalid("unknown question"))?;
    if let Some(previous) = &question.answer {
        if mode == AnswerMode::FirstOnly {
            return Err(StorageError::QuestionAlreadyAnswered);
        }
        return if previous == answer {
            Ok(())
        } else {
            Err(invalid("question already answered"))
        };
    }
    // User-rerouted lane questions retain the same return path. This shares the
    // answer/evidence transaction, so neither a rejected answer nor a crash can
    // leave a committed answer without its durable lane notification.
    if matches!(origin, AnswerOrigin::Host { .. })
        && let Some(canonical) = canonical
    {
        let mut route: event_bus::lane::QuestionRouting =
            serde_json::from_str(&canonical).map_err(|e| invalid(&e.to_string()))?;
        route.routing_revision = route
            .routing_revision
            .checked_add(1)
            .ok_or(StorageError::OutOfRange("routing revision"))?;
        let payload = serde_json::to_string(&route).map_err(|e| invalid(&e.to_string()))?;
        conn.execute(
            "UPDATE lane_question_routes SET routing_revision=?2,payload=?3 WHERE question_id=?1",
            params![id, route.routing_revision, payload],
        )?;
        question.routing_revision = route.routing_revision;
        crate::repo::lane_delivery::enqueue(
            conn,
            event_bus::lane::LaneNotification {
                notification_id: String::new(),
                lane_id: route.lane_id,
                management_revision: route.management_revision,
                source_thread_id: route.requester.thread_id.clone(),
                destination_thread_id: route.requester.thread_id,
                kind: event_bus::lane::LaneNotificationKind::Answer {
                    question_id: id.into(),
                    routing_revision: route.routing_revision,
                },
                report_seq: None,
                created_at_ns: crate::system_time_to_ns(std::time::SystemTime::now())?,
            },
        )?;
    }
    question.answer = Some(answer.into());
    question.answer_origin = Some(origin.clone());
    let payload = serde_json::to_string(&question).map_err(|e| invalid(&e.to_string()))?;
    // Plan A's fence is on the answer itself, not each recipient. Never persist
    // evidence for a losing answer, even if the caller read a stale pending row.
    let updated = conn.execute(
        "UPDATE user_questions SET payload=?2,answered=1 WHERE id=?1 AND answered=0",
        params![id, payload],
    )?;
    if updated != 1 {
        if mode == AnswerMode::FirstOnly {
            return Err(StorageError::QuestionAlreadyAnswered);
        }
        return Err(invalid("question answer changed during checked update"));
    }
    Ok(())
}

/// Every question the run owns or inherited, answered or not, oldest first.
///
/// This is the run's answer history, not its quota: only unanswered questions
/// count against the 32-question capacity (`requester_question_count`). A run
/// restores and injects answers from this complete list, so it must never be
/// truncated or fail once the lifetime total passes the capacity; omitting a
/// delivered answer would make a restored run inject it again.
pub fn for_run(conn: &Connection, run_id: &str) -> Result<Vec<UserQuestion>, StorageError> {
    list(
        conn,
        "SELECT q.payload FROM user_questions q WHERE ?1 NOT IN (SELECT run_id FROM run_tombstones) AND ((q.run_id=?1 AND q.root_run_id NOT IN (SELECT run_id FROM run_tombstones)) OR EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=q.id AND l.run_id != q.run_id AND l.run_id=?1)) ORDER BY q.rowid",
        run_id,
    )
}

/// Explicit continuation inheritance; caller-chosen IDs must already belong to
/// source. Links preserve the original requester's provenance in the payload.
pub fn bind(
    conn: &Connection,
    source: &str,
    target: &str,
    ids: &[String],
) -> Result<(), StorageError> {
    if source.is_empty() || target.is_empty() || source.len() > 64 || target.len() > 64 {
        return Err(invalid("invalid question inheritance"));
    }
    // Nothing may be inherited from, or handed to, a run a retention deleted.
    for run_id in [source, target] {
        let deleted: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)",
            [run_id],
            |row| row.get(0),
        )?;
        if deleted {
            return Err(StorageError::Tombstoned {
                run_id: Some(run_id.to_owned()),
                thread_id: None,
            });
        }
    }
    // Inheritance carries the whole answer history, so only the unanswered
    // questions are bounded: the same quota creation and attention count.
    let source_questions = for_run(conn, source)?
        .into_iter()
        .map(|question| (question.id, question.answer.is_some()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut unanswered = for_run(conn, target)?
        .into_iter()
        .filter(|question| question.answer.is_none())
        .map(|question| question.id)
        .collect::<std::collections::HashSet<_>>();
    for id in ids {
        match source_questions.get(id) {
            None => {
                return Err(invalid(
                    "question inheritance source does not own every question",
                ));
            }
            Some(false) => {
                unanswered.insert(id.clone());
            }
            Some(true) => {}
        }
    }
    if unanswered.len() > crate::lane_delivery::QuestionCapacity::Requester.limit() as usize {
        return Err(invalid("question recipient limit reached (32 unanswered)"));
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
    let mut statement = conn.prepare(
        "SELECT run_id FROM user_questions WHERE id=?1 AND run_id NOT IN (SELECT run_id FROM run_tombstones) AND root_run_id NOT IN (SELECT run_id FROM run_tombstones) \
         UNION SELECT l.run_id FROM user_question_links l WHERE l.question_id=?1 AND l.run_id NOT IN (SELECT run_id FROM run_tombstones) \
         AND EXISTS (SELECT 1 FROM user_questions q WHERE q.id=l.question_id AND (q.run_id != l.run_id OR q.root_run_id NOT IN (SELECT run_id FROM run_tombstones)))",
    )?;
    let rows = statement.query_map([id], |row| row.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn pending(conn: &Connection) -> Result<Vec<UserQuestion>, StorageError> {
    list(
        conn,
        "SELECT payload FROM user_questions WHERE answered=0 AND ?1='' AND ((run_id NOT IN (SELECT run_id FROM run_tombstones) AND root_run_id NOT IN (SELECT run_id FROM run_tombstones)) OR EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=user_questions.id AND l.run_id != user_questions.run_id AND l.run_id NOT IN (SELECT run_id FROM run_tombstones))) ORDER BY rowid DESC",
        "",
    )
}

fn list(conn: &Connection, sql: &str, arg: &str) -> Result<Vec<UserQuestion>, StorageError> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map([arg], |row| row.get::<_, String>(0))?;
    rows.map(|s| hydrate(conn, &s?)).collect()
}

fn hydrate(conn: &Connection, payload: &str) -> Result<UserQuestion, StorageError> {
    let mut question: UserQuestion =
        serde_json::from_str(payload).map_err(|e| invalid(&e.to_string()))?;
    if let Some(route) = crate::repo::lane_delivery::routing(conn, &question.id)? {
        question.responder = Some(route.responder);
        question.routing_revision = route.routing_revision;
        question.management_revision = Some(route.management_revision);
    } else {
        // No canonical route means legacy routing, never a stale serialized Lead.
        question.responder = None;
        question.routing_revision = 0;
        question.management_revision = None;
    }
    // Links, not a stale event/payload projection, are canonical consumer routing.
    // Insertion order retains the continuation path, including across restarts.
    let mut statement = conn.prepare(
        "SELECT run_id FROM user_question_links WHERE question_id=?1 AND run_id NOT IN (SELECT run_id FROM run_tombstones) ORDER BY rowid",
    )?;
    question.recipient_run_ids = statement
        .query_map([&question.id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(question)
}
fn invalid(message: &str) -> StorageError {
    StorageError::Serialization(message.into())
}
