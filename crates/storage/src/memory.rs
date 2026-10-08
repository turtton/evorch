//! Append-only lesson history and its searchable current projection.

use crate::{Database, StorageError};
use rusqlite::params;
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
        let mut statement = self.conn.prepare("SELECT entry_id,project,task_id,content,evidence,status,scope FROM memory_ledger WHERE project=?1 AND kind='finding' ORDER BY seq")?;
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
        let mut statement = self.conn.prepare(
            "SELECT id, project, task_id, content, evidence, status, scope FROM memory_entries
             WHERE project = ?1 AND (?2 IS NULL OR status = ?2)
             AND (?3 = '' OR rowid IN (SELECT rowid FROM memory_fts WHERE memory_fts MATCH ?3))
             ORDER BY ledger_seq DESC LIMIT 100",
        )?;
        let rows = statement.query_map(
            params![project, status.map(MemoryStatus::as_str), query],
            entry_row,
        )?;
        rows.map(|row| decode(row?)).collect()
    }

    /// Promoted lessons a task in `project` may see: that project's own project
    /// lessons plus user lessons from any project. Harness lessons never enter
    /// task prompts; they feed self-improvement intake instead.
    pub fn boundary_memory(&self, project: &str) -> Result<Vec<MemoryEntry>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT id, project, task_id, content, evidence, status, scope FROM memory_entries
             WHERE status = 'promoted'
             AND ((project = ?1 AND scope = 'project') OR scope = 'user')
             ORDER BY ledger_seq DESC LIMIT 100",
        )?;
        let rows = statement.query_map([project], entry_row)?;
        rows.map(|row| decode(row?)).collect()
    }

    pub fn memory_history(&self, id: &str) -> Result<Vec<MemoryEntry>, StorageError> {
        let mut statement = self.conn.prepare("SELECT entry_id, project, task_id, content, evidence, status, scope FROM memory_ledger WHERE entry_id = ?1 AND kind='lesson' ORDER BY seq")?;
        statement
            .query_map([id], entry_row)?
            .map(|row| decode(row?))
            .collect()
    }
}

/// Row of `id, project, task_id, content, evidence, status, scope` in that order.
pub(crate) type EntryRow = (Lesson, String, String);

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
    ))
}

pub(crate) fn decode((mut lesson, status, scope): EntryRow) -> Result<MemoryEntry, StorageError> {
    lesson.scope = LessonScope::parse(&scope)?;
    Ok(MemoryEntry {
        lesson,
        status: MemoryStatus::parse(&status)?,
    })
}
