use crate::StorageError;
use crate::memory::{Lesson, MemoryStatus, decode, entry_row};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone)]
pub(crate) enum Mutation {
    Finding(Lesson),
    Team(crate::team::TeamSnapshot),
    Candidates(Vec<Lesson>),
    Candidate(Lesson),
    Validate { id: String, evidence: String },
    Promote(String),
    Reject(String),
    SetFavorite { id: String, favorite: bool },
}

pub(crate) fn append(conn: &Connection, mutation: &Mutation) -> Result<(), StorageError> {
    if let Mutation::Team(snapshot) = mutation {
        return crate::team::append(conn, snapshot);
    }
    let transaction = conn.unchecked_transaction()?;
    match mutation {
        Mutation::Candidates(lessons) => {
            for lesson in lessons {
                apply(&transaction, &Mutation::Candidate(lesson.clone()))?;
            }
        }
        _ => apply(&transaction, mutation)?,
    }
    transaction.commit()?;
    Ok(())
}

fn apply(transaction: &Connection, mutation: &Mutation) -> Result<(), StorageError> {
    let (lesson, status) = match mutation {
        Mutation::Finding(finding) => return crate::memory::append_finding(transaction, finding),
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
            let existing = transaction.query_row(
                "SELECT id, project, task_id, content, evidence, status, scope, favorite FROM memory_entries WHERE id = ?1",
                [&lesson.id],
                entry_row,
            ).optional()?.map(decode).transpose()?;
            if let Some(existing) = existing {
                return if existing.lesson == *lesson {
                    Ok(())
                } else {
                    Err(invalid("conflicting duplicate lesson"))
                };
            }
            (lesson.clone(), MemoryStatus::Candidate)
        }
        Mutation::SetFavorite { id, favorite } => return set_favorite(transaction, id, *favorite),
        Mutation::Validate { id, .. } | Mutation::Promote(id) | Mutation::Reject(id) => {
            let entry = transaction.query_row("SELECT id, project, task_id, content, evidence, status, scope, favorite FROM memory_entries WHERE id = ?1", [id], entry_row).optional()?.map(decode).transpose()?.ok_or_else(|| invalid("lesson not found"))?;
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
                    // A favorite is the user's approval; only clearing it makes the lesson
                    // rejectable, whoever asks (GUI, learning review, any other caller).
                    if entry.favorite {
                        return Err(StorageError::LessonFavorite {
                            id: entry.lesson.id,
                        });
                    }
                    MemoryStatus::Rejected
                }
                Mutation::Candidate(_)
                | Mutation::Candidates(_)
                | Mutation::Finding(_)
                | Mutation::Team(_)
                | Mutation::SetFavorite { .. } => {
                    return Err(invalid("invalid transition"));
                }
            };
            (entry.lesson, status)
        }
        Mutation::Candidates(_) | Mutation::Team(_) => {
            return Err(invalid("invalid transition"));
        }
    };
    transaction.execute("INSERT INTO memory_ledger(entry_id,project,task_id,content,evidence,status,scope) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![lesson.id,lesson.project,lesson.task_id,lesson.content,lesson.evidence,status.as_str(),lesson.scope.as_str()])?;
    let seq = transaction.last_insert_rowid();
    transaction.execute("INSERT INTO memory_entries(id,project,task_id,content,evidence,status,ledger_seq,scope) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET status=excluded.status, ledger_seq=excluded.ledger_seq", params![lesson.id,lesson.project,lesson.task_id,lesson.content,lesson.evidence,status.as_str(),seq,lesson.scope.as_str()])?;
    Ok(())
}

/// Flips the favorite flag and records the change in the ledger as history. The
/// projection's `ledger_seq` stays put: it orders what task prompts receive, and the
/// user's approval mark must not reshuffle that.
fn set_favorite(transaction: &Connection, id: &str, favorite: bool) -> Result<(), StorageError> {
    let entry = transaction
        .query_row(
            "SELECT id, project, task_id, content, evidence, status, scope, favorite FROM memory_entries WHERE id = ?1",
            [id],
            entry_row,
        )
        .optional()?
        .map(decode)
        .transpose()?
        .ok_or_else(|| invalid("lesson not found"))?;
    if entry.status == MemoryStatus::Rejected {
        return Err(invalid("rejected lessons cannot be favorited"));
    }
    if entry.favorite == favorite {
        return Ok(());
    }
    let lesson = &entry.lesson;
    let kind = if favorite { "favorite" } else { "unfavorite" };
    transaction.execute(
        "INSERT INTO memory_ledger(entry_id,project,task_id,content,evidence,status,kind,scope) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![lesson.id, lesson.project, lesson.task_id, lesson.content, lesson.evidence, entry.status.as_str(), kind, lesson.scope.as_str()],
    )?;
    transaction.execute(
        "UPDATE memory_entries SET favorite = ?2 WHERE id = ?1",
        params![lesson.id, favorite],
    )?;
    Ok(())
}

fn invalid(message: &str) -> StorageError {
    StorageError::Serialization(message.into())
}
