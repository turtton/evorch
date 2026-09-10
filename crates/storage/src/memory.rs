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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub project: String,
    pub task_id: String,
    pub content: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub lesson: Lesson,
    pub status: MemoryStatus,
}

impl Database {
    pub fn append_finding(&self, finding: &Lesson) -> Result<(), StorageError> {
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
        self.conn.execute(
            "INSERT INTO memory_ledger(entry_id,project,task_id,content,evidence,status,kind) VALUES(?1,?2,?3,?4,?5,'candidate','finding')",
            params![finding.id, finding.project, finding.task_id, finding.content, finding.evidence],
        )?;
        Ok(())
    }

    pub fn findings(&self, project: &str) -> Result<Vec<Lesson>, StorageError> {
        let mut statement = self.conn.prepare("SELECT entry_id,project,task_id,content,evidence,status FROM memory_ledger WHERE project=?1 AND kind='finding' ORDER BY seq")?;
        Ok(statement
            .query_map([project], entry_row)?
            .map(|row| row.map(|(lesson, _)| lesson))
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn search_memory(
        &self,
        project: &str,
        query: &str,
        status: Option<MemoryStatus>,
    ) -> Result<Vec<MemoryEntry>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT id, project, task_id, content, evidence, status FROM memory_entries
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

    pub fn memory_history(&self, id: &str) -> Result<Vec<MemoryEntry>, StorageError> {
        let mut statement = self.conn.prepare("SELECT entry_id, project, task_id, content, evidence, status FROM memory_ledger WHERE entry_id = ?1 AND kind='lesson' ORDER BY seq")?;
        statement
            .query_map([id], entry_row)?
            .map(|row| decode(row?))
            .collect()
    }
}

pub(crate) fn entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(Lesson, String)> {
    Ok((
        Lesson {
            id: row.get(0)?,
            project: row.get(1)?,
            task_id: row.get(2)?,
            content: row.get(3)?,
            evidence: row.get(4)?,
        },
        row.get(5)?,
    ))
}

pub(crate) fn decode((lesson, status): (Lesson, String)) -> Result<MemoryEntry, StorageError> {
    Ok(MemoryEntry {
        lesson,
        status: MemoryStatus::parse(&status)?,
    })
}
