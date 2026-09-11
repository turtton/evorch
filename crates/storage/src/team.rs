use crate::{Database, StorageError};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSnapshot {
    pub team_id: String,
    pub revision: u64,
    pub state: String,
}

impl Database {
    pub fn team_snapshot(&self, id: &str) -> Result<Option<TeamSnapshot>, StorageError> {
        Ok(self.conn.query_row(
            "SELECT team_id, revision, state FROM team_ledger WHERE team_id=?1 ORDER BY revision DESC LIMIT 1",
            [id],
            |row| {
                let revision: i64 = row.get(1)?;
                let revision = u64::try_from(revision).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Integer, Box::new(error))
                })?;
                Ok(TeamSnapshot { team_id: row.get(0)?, revision, state: row.get(2)? })
            },
        ).optional()?)
    }
}

pub(crate) fn append(conn: &Connection, snapshot: &TeamSnapshot) -> Result<(), StorageError> {
    let guard = crate::entity::SecretGuard::from_env();
    guard.check_text("team", "identity", &snapshot.team_id)?;
    guard.check_text("team", "state", &snapshot.state)?;
    if snapshot.team_id.trim().is_empty() {
        return Err(StorageError::Serialization("team identity is empty".into()));
    }
    let transaction = conn.unchecked_transaction()?;
    let revision: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(revision), 0) FROM team_ledger WHERE team_id=?1",
        [&snapshot.team_id],
        |row| row.get(0),
    )?;
    let next_revision = i64::try_from(snapshot.revision).map_err(|_| {
        StorageError::Serialization("team revision exceeds SQLite integer range".into())
    })?;
    if revision.checked_add(1) != Some(next_revision) {
        return Err(StorageError::Serialization("team revision conflict".into()));
    }
    transaction.execute(
        "INSERT INTO team_ledger(team_id,revision,state) VALUES(?1,?2,?3)",
        params![snapshot.team_id, next_revision, snapshot.state],
    )?;
    transaction.commit()?;
    Ok(())
}
