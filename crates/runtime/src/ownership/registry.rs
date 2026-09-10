use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{OwnershipError, ThreadOwner};

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Ownership(#[from] OwnershipError),
    #[error("thread does not exist")]
    Absent,
    #[error("thread already exists; attach or claim explicitly")]
    Exists,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub struct Registry {
    connection: Connection,
}

impl Registry {
    pub fn list(&self) -> Result<Vec<ThreadOwner>, RegistryError> {
        let mut statement = self
            .connection
            .prepare("SELECT state FROM thread_owners ORDER BY thread_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
    pub fn open(path: &Path) -> Result<Self, RegistryError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS thread_owners (thread_id TEXT PRIMARY KEY, state TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS owner_checkpoints (thread_id TEXT PRIMARY KEY, generation TEXT NOT NULL, payload TEXT NOT NULL)")?;
        Ok(Self { connection })
    }

    pub fn attach(&self, thread_id: &str) -> Result<ThreadOwner, RegistryError> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT state FROM thread_owners WHERE thread_id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(serde_json::from_str(&json.ok_or(RegistryError::Absent)?)?)
    }

    pub fn start(&mut self, owner: &ThreadOwner) -> Result<(), RegistryError> {
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO thread_owners (thread_id, state) VALUES (?1, ?2)",
            params![owner.thread_id, serde_json::to_string(owner)?],
        )?;
        if changed == 0 {
            return Err(RegistryError::Exists);
        }
        Ok(())
    }

    pub fn update(
        &mut self,
        thread_id: &str,
        update: impl FnOnce(&mut ThreadOwner) -> Result<(), OwnershipError>,
    ) -> Result<ThreadOwner, RegistryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json: Option<String> = transaction
            .query_row(
                "SELECT state FROM thread_owners WHERE thread_id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .optional()?;
        let mut owner = serde_json::from_str(&json.ok_or(RegistryError::Absent)?)?;
        update(&mut owner)?;
        transaction.execute(
            "UPDATE thread_owners SET state = ?2 WHERE thread_id = ?1",
            params![thread_id, serde_json::to_string(&owner)?],
        )?;
        transaction.commit()?;
        Ok(owner)
    }

    pub fn checkpoint_permit(
        &mut self,
        permit: &super::OwnerPermit,
        messages: &[providers::Message],
    ) -> Result<ThreadOwner, RegistryError> {
        let thread_id = &permit.thread_id;
        let token = &permit.lease;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json: String = transaction.query_row(
            "SELECT state FROM thread_owners WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )?;
        let mut owner: ThreadOwner = serde_json::from_str(&json)?;
        match permit.run_id.as_deref() {
            Some(run) => owner.checkpoint_run(token, run)?,
            None => owner.checkpoint(token)?,
        }
        transaction.execute("INSERT INTO owner_checkpoints VALUES (?1, ?2, ?3) ON CONFLICT(thread_id) DO UPDATE SET generation=excluded.generation, payload=excluded.payload", params![thread_id, token.generation.to_string(), serde_json::to_string(messages)?])?;
        transaction.execute(
            "UPDATE thread_owners SET state=?2 WHERE thread_id=?1",
            params![thread_id, serde_json::to_string(&owner)?],
        )?;
        transaction.commit()?;
        Ok(owner)
    }
}
