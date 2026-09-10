use crate::StorageError;
use crate::memory::{Lesson, MemoryStatus, decode, entry_row};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone)]
pub(crate) enum Mutation {
    EvalTrace(crate::eval::EvalTrace),
    Candidate(Lesson),
    Validate { id: String, evidence: String },
    Promote(String),
    Reject(String),
}

pub(crate) fn append(conn: &Connection, mutation: &Mutation) -> Result<(), StorageError> {
    if let Mutation::EvalTrace(trace) = mutation {
        return crate::eval::append(conn, trace);
    }
    let transaction = conn.unchecked_transaction()?;
    let (lesson, status) = match mutation {
        Mutation::Candidate(lesson) => {
            let guard = crate::entity::SecretGuard::from_env();
            for text in [
                &lesson.id,
                &lesson.project,
                &lesson.task_id,
                &lesson.content,
                &lesson.evidence,
            ] {
                guard.check_text("memory", "lesson", text)?;
            }
            if [
                &lesson.id,
                &lesson.project,
                &lesson.task_id,
                &lesson.content,
                &lesson.evidence,
            ]
            .iter()
            .any(|s| s.trim().is_empty())
            {
                return Err(invalid("candidate requires identity, content and evidence"));
            }
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_entries WHERE id = ?1)",
                [&lesson.id],
                |row| row.get(0),
            )?;
            if exists {
                return Err(invalid("duplicate lesson"));
            }
            (lesson.clone(), MemoryStatus::Candidate)
        }
        Mutation::Validate { id, .. } | Mutation::Promote(id) | Mutation::Reject(id) => {
            let entry = transaction.query_row("SELECT id, project, task_id, content, evidence, status FROM memory_entries WHERE id = ?1", [id], entry_row).optional()?.map(decode).transpose()?.ok_or_else(|| invalid("lesson not found"))?;
            let status = match mutation {
                Mutation::Validate { evidence, .. } => {
                    if entry.status != MemoryStatus::Candidate
                        || evidence.trim().is_empty()
                        || evidence != &entry.lesson.evidence
                    {
                        return Err(invalid(
                            "validation requires matching evidence on a candidate",
                        ));
                    }
                    MemoryStatus::Validated
                }
                Mutation::Promote(_) => {
                    if entry.status != MemoryStatus::Validated {
                        return Err(invalid("promotion requires validated evidence"));
                    }
                    MemoryStatus::Promoted
                }
                Mutation::Reject(_) => {
                    if entry.status == MemoryStatus::Rejected {
                        return Err(invalid("lesson already rejected"));
                    }
                    MemoryStatus::Rejected
                }
                Mutation::Candidate(_) | Mutation::EvalTrace(_) => {
                    return Err(invalid("invalid transition"));
                }
            };
            (entry.lesson, status)
        }
        Mutation::EvalTrace(_) => return Err(invalid("invalid transition")),
    };
    transaction.execute("INSERT INTO memory_ledger(entry_id,project,task_id,content,evidence,status) VALUES(?1,?2,?3,?4,?5,?6)", params![lesson.id,lesson.project,lesson.task_id,lesson.content,lesson.evidence,status.as_str()])?;
    let seq = transaction.last_insert_rowid();
    transaction.execute("INSERT INTO memory_entries(id,project,task_id,content,evidence,status,ledger_seq) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET status=excluded.status, ledger_seq=excluded.ledger_seq", params![lesson.id,lesson.project,lesson.task_id,lesson.content,lesson.evidence,status.as_str(),seq])?;
    transaction.commit()?;
    Ok(())
}

fn invalid(message: &str) -> StorageError {
    StorageError::Serialization(message.into())
}
