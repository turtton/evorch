//! Append-only lesson history and its searchable current projection.

use crate::{Database, StorageError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Candidate,
    Validated,
    Promoted,
    Rejected,
}

impl MemoryStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Validated => "validated",
            Self::Promoted => "promoted",
            Self::Rejected => "rejected",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "validated" => Ok(Self::Validated),
            "promoted" => Ok(Self::Promoted),
            "rejected" => Ok(Self::Rejected),
            _ => Err(StorageError::Serialization(format!(
                "invalid memory status: {value}"
            ))),
        }
    }
}

/// Who a lesson is about, which decides where a promoted lesson is used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonScope {
    /// Knowledge about the project the run worked in; injected into that project only.
    #[default]
    Project,
    /// Feedback about evorch itself (tools, prompts, runtime); self-improvement intake only.
    Harness,
    /// The user's cross-project preferences and working style; injected into every project.
    User,
}

impl LessonScope {
    pub const ALL: [Self; 3] = [Self::Project, Self::Harness, Self::User];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Harness => "harness",
            Self::User => "user",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "project" => Ok(Self::Project),
            "harness" => Ok(Self::Harness),
            "user" => Ok(Self::User),
            _ => Err(StorageError::Serialization(format!(
                "invalid lesson scope: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub project: String,
    pub task_id: String,
    pub content: String,
    pub evidence: String,
    #[serde(default)]
    pub scope: LessonScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub lesson: Lesson,
    pub status: MemoryStatus,
    /// The user's approval mark. Independent of `status`; any status except
    /// `Rejected` may carry it, and a favorite cannot be rejected until it is cleared.
    /// In [`Database::memory_history`] rows this is the flag as of that row.
    pub favorite: bool,
}

/// Narrows [`Database::search_memory_filtered`]; `None` leaves a dimension open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryFilter {
    pub status: Option<MemoryStatus>,
    /// `Some(true)` keeps favorites only, `Some(false)` the unconfirmed ones.
    pub favorite: Option<bool>,
}

/// One change of a lesson's favorite flag, oldest first in [`Database::favorite_history`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FavoriteChange {
    /// The flag after the change.
    pub favorite: bool,
    /// The lesson's status when the change was made.
    pub status: MemoryStatus,
}

pub(crate) fn append_finding(
    conn: &rusqlite::Connection,
    finding: &Lesson,
) -> Result<(), StorageError> {
    let guard = crate::entity::SecretGuard::from_env();
    for text in [
        &finding.id,
        &finding.project,
        &finding.task_id,
        &finding.content,
        &finding.evidence,
    ] {
        guard.check_text("memory", "finding", text)?;
        if text.trim().is_empty() {
            return Err(StorageError::Serialization(
                "finding fields must be non-empty".into(),
            ));
        }
    }
    conn.execute(
            "INSERT INTO memory_ledger(entry_id,project,task_id,content,evidence,status,kind,scope) VALUES(?1,?2,?3,?4,?5,'candidate','finding',?6)",
            params![finding.id, finding.project, finding.task_id, finding.content, finding.evidence, finding.scope.as_str()],
        )?;
    Ok(())
}

impl Database {
    pub fn findings(&self, project: &str) -> Result<Vec<Lesson>, StorageError> {
        let mut statement = self.conn.prepare("SELECT entry_id,project,task_id,content,evidence,status,scope,0 FROM memory_ledger WHERE project=?1 AND kind='finding' ORDER BY seq")?;
        statement
            .query_map([project], entry_row)?
            .map(|row| decode(row?).map(|entry| entry.lesson))
            .collect()
    }

    pub fn search_memory(
        &self,
        project: &str,
        query: &str,
        status: Option<MemoryStatus>,
    ) -> Result<Vec<MemoryEntry>, StorageError> {
        self.search_memory_filtered(
            project,
            query,
            MemoryFilter {
                status,
                favorite: None,
            },
        )
    }

    pub fn search_memory_filtered(
        &self,
        project: &str,
        query: &str,
        filter: MemoryFilter,
    ) -> Result<Vec<MemoryEntry>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT id, project, task_id, content, evidence, status, scope, favorite FROM memory_entries
             WHERE project = ?1 AND (?2 IS NULL OR status = ?2) AND (?4 IS NULL OR favorite = ?4)
             AND (?3 = '' OR rowid IN (SELECT rowid FROM memory_fts WHERE memory_fts MATCH ?3))
             ORDER BY ledger_seq DESC LIMIT 100",
        )?;
        let rows = statement.query_map(
            params![
                project,
                filter.status.map(MemoryStatus::as_str),
                query,
                filter.favorite
            ],
            entry_row,
        )?;
        rows.map(|row| decode(row?)).collect()
    }

    /// The current projection of one lesson, or `None` when it does not exist.
    pub fn memory_entry(&self, id: &str) -> Result<Option<MemoryEntry>, StorageError> {
        self.conn
            .query_row(
                "SELECT id, project, task_id, content, evidence, status, scope, favorite FROM memory_entries WHERE id = ?1",
                [id],
                entry_row,
            )
            .optional()?
            .map(decode)
            .transpose()
    }

    /// Promoted lessons a task in `project` may see: that project's own project
    /// lessons plus user lessons from any project. Harness lessons never enter
    /// task prompts; they feed self-improvement intake instead.
    pub fn boundary_memory(&self, project: &str) -> Result<Vec<MemoryEntry>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT id, project, task_id, content, evidence, status, scope, favorite FROM memory_entries
             WHERE status = 'promoted'
             AND ((project = ?1 AND scope = 'project') OR scope = 'user')
             ORDER BY ledger_seq DESC LIMIT 100",
        )?;
        let rows = statement.query_map([project], entry_row)?;
        rows.map(|row| decode(row?)).collect()
    }

    pub fn memory_history(&self, id: &str) -> Result<Vec<MemoryEntry>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT l.entry_id, l.project, l.task_id, l.content, l.evidence, l.status, l.scope,
                    COALESCE((SELECT f.kind = 'favorite' FROM memory_ledger f
                              WHERE f.entry_id = l.entry_id AND f.seq <= l.seq
                                AND f.kind IN ('favorite', 'unfavorite')
                              ORDER BY f.seq DESC LIMIT 1), 0)
             FROM memory_ledger l WHERE l.entry_id = ?1 AND l.kind = 'lesson' ORDER BY l.seq",
        )?;
        statement
            .query_map([id], entry_row)?
            .map(|row| decode(row?))
            .collect()
    }

    /// Every favorite flag change of a lesson, oldest first.
    pub fn favorite_history(&self, id: &str) -> Result<Vec<FavoriteChange>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT kind = 'favorite', status FROM memory_ledger
             WHERE entry_id = ?1 AND kind IN ('favorite', 'unfavorite') ORDER BY seq",
        )?;
        statement
            .query_map([id], |row| {
                Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (favorite, status) = row?;
                Ok(FavoriteChange {
                    favorite,
                    status: MemoryStatus::parse(&status)?,
                })
            })
            .collect()
    }
}

/// Row of `id, project, task_id, content, evidence, status, scope, favorite` in that order.
pub(crate) type EntryRow = (Lesson, String, String, bool);

pub(crate) fn entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok((
        Lesson {
            id: row.get(0)?,
            project: row.get(1)?,
            task_id: row.get(2)?,
            content: row.get(3)?,
            evidence: row.get(4)?,
            scope: LessonScope::Project,
        },
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

pub(crate) fn decode(
    (mut lesson, status, scope, favorite): EntryRow,
) -> Result<MemoryEntry, StorageError> {
    lesson.scope = LessonScope::parse(&scope)?;
    Ok(MemoryEntry {
        lesson,
        status: MemoryStatus::parse(&status)?,
        favorite,
    })
}
