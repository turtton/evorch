use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

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
    #[error("ownership reader lock is poisoned")]
    ReaderPoisoned,
    #[error("handoff preparation failed: {0}")]
    Preparation(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub struct Registry {
    connection: Connection,
}

#[cfg(test)]
thread_local! {
    static WRITER_CONTENTION: std::cell::RefCell<Option<(
        std::sync::mpsc::SyncSender<()>, std::sync::mpsc::Receiver<()>
    )>> = const { std::cell::RefCell::new(None) };
}

impl Registry {
    /// An event-driven test seam: observe an actual SQLite writer/read conflict
    /// and release it after the reader has performed its guarded GUI action.
    #[cfg(test)]
    pub(crate) fn observe_writer_contention(
        entered: std::sync::mpsc::SyncSender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        WRITER_CONTENTION.with(|observer| *observer.borrow_mut() = Some((entered, resume)));
    }

    pub fn guard_generation(&self, permit: &super::OwnerPermit) -> Result<(), RegistryError> {
        self.connection.execute_batch("BEGIN DEFERRED")?;
        let owner = self.attach(&permit.thread_id)?;
        owner.validate(&permit.lease)?;
        if owner.state == super::OwnerState::Released {
            return Err(OwnershipError::Fenced.into());
        }
        Ok(())
    }
    pub fn list(&self) -> Result<Vec<ThreadOwner>, RegistryError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT state FROM thread_owners ORDER BY thread_id")?;
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

    /// Open an initialized registry without creating a database or running schema DDL.
    pub fn open_existing(path: &Path) -> Result<Self, RegistryError> {
        Self::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
    }

    /// Open an existing registry for probes and generation guards only.
    pub fn open_readonly(path: &Path) -> Result<Self, RegistryError> {
        Self::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    }

    pub(super) fn open_readonly_nonblocking(path: &Path) -> Result<Self, RegistryError> {
        Self::open_with_timeout(path, OpenFlags::SQLITE_OPEN_READ_ONLY, Duration::ZERO)
    }

    fn open_with_flags(path: &Path, flags: OpenFlags) -> Result<Self, RegistryError> {
        Self::open_with_timeout(path, flags, Duration::from_secs(2))
    }

    fn open_with_timeout(
        path: &Path,
        flags: OpenFlags,
        timeout: Duration,
    ) -> Result<Self, RegistryError> {
        let connection =
            Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        connection.busy_timeout(timeout)?;
        #[cfg(test)]
        if flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE)
            && WRITER_CONTENTION.with(|observer| observer.borrow().is_some())
        {
            connection.busy_handler(Some(|_| {
                WRITER_CONTENTION.with(|observer| {
                    let Some((entered, resume)) = observer.borrow_mut().take() else {
                        return false;
                    };
                    entered.send(()).expect("reader observes writer contention");
                    resume.recv().expect("reader releases its generation guard");
                    true
                })
            }))?;
        }
        Ok(Self { connection })
    }

    pub fn attach(&self, thread_id: &str) -> Result<ThreadOwner, RegistryError> {
        let json: Option<String> = self
            .connection
            .prepare_cached("SELECT state FROM thread_owners WHERE thread_id = ?1")?
            .query_row([thread_id], |row| row.get(0))
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

    /// Create an independent child owner only while the source generation still
    /// accepts work. Checking the source and reserving the target share one write
    /// transaction; a target collision never adopts an unrelated thread's lease.
    pub(super) fn prepare_child<T>(
        &mut self,
        source: &super::OwnerPermit,
        thread_id: &str,
        now_ms: u64,
        prepare: impl FnOnce() -> Result<T, RegistryError>,
    ) -> Result<(ThreadOwner, T), RegistryError> {
        // Drain ownership readers before taking the runtime goal lock. An
        // IMMEDIATE transaction would defer that wait until commit and invert
        // the GUI's ownership-read -> goal-control order.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Exclusive)?;
        let json: Option<String> = transaction
            .query_row(
                "SELECT state FROM thread_owners WHERE thread_id = ?1",
                [&source.thread_id],
                |row| row.get(0),
            )
            .optional()?;
        let parent: ThreadOwner = serde_json::from_str(&json.ok_or(RegistryError::Absent)?)?;
        parent.validate(&source.lease)?;
        if parent.state != super::OwnerState::Running {
            return Err(OwnershipError::Quiescing.into());
        }
        let mut child = ThreadOwner::new(
            thread_id.into(),
            super::Lease {
                owner_id: parent.lease.owner_id,
                generation: 1,
                expires_at: now_ms.saturating_add(parent.settings.lease_ms.get()),
            },
        );
        child.settings = parent.settings;
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO thread_owners (thread_id, state) VALUES (?1, ?2)",
            params![thread_id, serde_json::to_string(&child)?],
        )?;
        if changed == 0 {
            return Err(RegistryError::Exists);
        }
        let prepared = prepare()?;
        transaction.commit()?;
        Ok((child, prepared))
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
        let mut owner: ThreadOwner = serde_json::from_str(&json.ok_or(RegistryError::Absent)?)?;
        let previous_generation = owner.lease.generation;
        update(&mut owner)?;
        if owner.state == super::OwnerState::Released
            || owner.lease.generation != previous_generation
        {
            transaction.execute(
                "INSERT OR IGNORE INTO owner_checkpoints VALUES (?1, ?2, '[]')",
                params![thread_id, previous_generation.to_string()],
            )?;
        }
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
