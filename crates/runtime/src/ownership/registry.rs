use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use super::observe::{DbOperation, DbPhase, GuardHold, Site};
use super::{OwnershipError, ThreadOwner};

/// Per-lock wait of a single registry access, as before bounded retries.
pub(super) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    /// A SQLite failure with the operation and phase that hit it, so a run's
    /// terminal reason identifies where `database is locked` occurred.
    #[error("ownership {operation} failed at {phase} (attempt {attempt}): {source}")]
    Database {
        operation: DbOperation,
        phase: DbPhase,
        attempt: u32,
        #[source]
        source: rusqlite::Error,
    },
    /// The caller was interrupted while waiting to retry contention. Nothing
    /// was committed by this operation.
    #[error("ownership {operation} interrupted while retrying after {attempts} attempt(s)")]
    Interrupted {
        operation: DbOperation,
        attempts: u32,
    },
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

impl RegistryError {
    fn sqlite(&self) -> Option<&rusqlite::Error> {
        match self {
            Self::Sql(error) | Self::Database { source: error, .. } => Some(error),
            _ => None,
        }
    }

    pub fn sqlite_code(&self) -> Option<rusqlite::ErrorCode> {
        self.sqlite().and_then(rusqlite::Error::sqlite_error_code)
    }

    /// SQLite lock contention (`SQLITE_BUSY` / `SQLITE_LOCKED`).
    pub fn is_contention(&self) -> bool {
        matches!(
            self.sqlite_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        )
    }

    /// The registry showed that the generation lost authority: another owner
    /// or generation holds the thread, or it was released. Contention never is.
    pub fn is_fenced(&self) -> bool {
        matches!(self, Self::Ownership(OwnershipError::Fenced))
    }

    /// Transient contention after which a rolled-back transaction may run
    /// again. `SQLITE_BUSY` (including a busy `COMMIT`, which commits nothing)
    /// qualifies. Of `SQLITE_LOCKED`, only shared-cache table locks held by
    /// another connection do; other variants are conflicts within the same
    /// connection that a retry would repeat. Ownership, validation, JSON and
    /// I/O errors are never retried.
    pub(super) fn is_retryable(&self) -> bool {
        let Some(error) = self.sqlite().and_then(rusqlite::Error::sqlite_error) else {
            return false;
        };
        match error.code {
            rusqlite::ErrorCode::DatabaseBusy => true,
            rusqlite::ErrorCode::DatabaseLocked => {
                error.extended_code == rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE
            }
            _ => false,
        }
    }

    pub(super) fn with_attempts(self, attempts: u32) -> Self {
        match self {
            Self::Database {
                operation,
                phase,
                source,
                ..
            } => Self::Database {
                operation,
                phase,
                attempt: attempts,
                source,
            },
            other => other,
        }
    }
}

pub struct Registry {
    connection: Connection,
    attempt: u32,
    quiet_contention: bool,
    guard: Option<GuardHold>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            guard.finish(self.connection.path().unwrap_or(""));
        }
    }
}

#[cfg(test)]
thread_local! {
    static WRITER_CONTENTION: std::cell::RefCell<Option<(
        std::sync::mpsc::SyncSender<()>, std::sync::mpsc::Receiver<()>
    )>> = const { std::cell::RefCell::new(None) };
}

#[derive(Clone, Copy)]
pub(super) enum Access {
    ReadOnly,
    ReadWrite,
}

/// How a connection waits for SQLite locks.
#[derive(Clone, Copy)]
pub(super) enum Wait {
    /// SQLite's busy timeout, applied to each lock wait separately.
    Timeout(Duration),
    /// The current retry attempt's absolute budget, shared by all statements.
    Budget,
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

    fn site<'a>(
        &'a self,
        operation: DbOperation,
        thread_id: &'a str,
        started: Instant,
        payload_bytes: Option<usize>,
    ) -> Site<'a> {
        Site {
            db: self.connection.path().unwrap_or(""),
            operation,
            thread_id,
            attempt: self.attempt,
            started,
            payload_bytes,
            quiet_contention: self.quiet_contention,
        }
    }

    /// Begin the read transaction kept until this registry is dropped.
    pub fn guard_generation(&mut self, permit: &super::OwnerPermit) -> Result<(), RegistryError> {
        let started = Instant::now();
        let site = self.site(DbOperation::Guard, &permit.thread_id, started, None);
        self.connection
            .execute_batch("BEGIN DEFERRED")
            .map_err(|error| site.sql(DbPhase::Begin, error))?;
        let owner = self.read_owner(&site)?;
        owner.validate(&permit.lease)?;
        if owner.state == super::OwnerState::Released {
            return Err(OwnershipError::Fenced.into());
        }
        self.guard = Some(GuardHold {
            thread_id: permit.thread_id.clone(),
            generation: permit.lease.generation,
            since: Instant::now(),
        });
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<ThreadOwner>, RegistryError> {
        let site = self.site(DbOperation::List, "", Instant::now(), None);
        let states = (|| {
            let mut statement = self
                .connection
                .prepare_cached("SELECT state FROM thread_owners ORDER BY thread_id")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })()
        .map_err(|error| site.sql(DbPhase::Read, error))?;
        states
            .iter()
            .map(|state| Ok(serde_json::from_str(state)?))
            .collect()
    }

    pub fn open(path: &Path) -> Result<Self, RegistryError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(ATTEMPT_TIMEOUT)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS thread_owners (thread_id TEXT PRIMARY KEY, state TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS owner_checkpoints (thread_id TEXT PRIMARY KEY, generation TEXT NOT NULL, payload TEXT NOT NULL)")?;
        Ok(Self::from_connection(connection, 1, false))
    }

    fn from_connection(connection: Connection, attempt: u32, quiet_contention: bool) -> Self {
        Self {
            connection,
            attempt,
            quiet_contention,
            guard: None,
        }
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
        let mut registry =
            Self::open_with_timeout(path, OpenFlags::SQLITE_OPEN_READ_ONLY, Duration::ZERO)?;
        registry.quiet_contention = true;
        Ok(registry)
    }

    /// Open one attempt of an operation, waiting for locks as `wait` allows.
    pub(super) fn open_attempt(
        path: &Path,
        access: Access,
        wait: Wait,
        operation: DbOperation,
        thread_id: &str,
        attempt: u32,
    ) -> Result<Self, RegistryError> {
        let flags = match access {
            Access::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
            Access::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
        };
        let opened = match wait {
            Wait::Timeout(timeout) => Self::open_with_timeout(path, flags, timeout),
            Wait::Budget => Self::open_with_budget(path, flags),
        };
        let mut registry = opened.map_err(|error| match error {
            RegistryError::Sql(error) => Site {
                db: &path.to_string_lossy(),
                operation,
                thread_id,
                attempt,
                started: Instant::now(),
                payload_bytes: None,
                quiet_contention: false,
            }
            .sql(DbPhase::Open, error),
            other => other,
        })?;
        registry.attempt = attempt;
        Ok(registry)
    }

    /// Every statement on this connection waits within the budget of the
    /// retry attempt running on the current thread, never per statement.
    fn open_with_budget(path: &Path, flags: OpenFlags) -> Result<Self, RegistryError> {
        let connection =
            Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        connection.busy_handler(Some(super::retry::wait_within_budget))?;
        Ok(Self::from_connection(connection, 1, false))
    }

    fn open_with_flags(path: &Path, flags: OpenFlags) -> Result<Self, RegistryError> {
        Self::open_with_timeout(path, flags, ATTEMPT_TIMEOUT)
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
        Ok(Self::from_connection(connection, 1, false))
    }

    pub fn attach(&self, thread_id: &str) -> Result<ThreadOwner, RegistryError> {
        self.attach_as(DbOperation::Attach, thread_id)
    }

    pub(super) fn attach_as(
        &self,
        operation: DbOperation,
        thread_id: &str,
    ) -> Result<ThreadOwner, RegistryError> {
        self.read_owner(&self.site(operation, thread_id, Instant::now(), None))
    }

    fn read_owner(&self, site: &Site<'_>) -> Result<ThreadOwner, RegistryError> {
        let json: Option<String> = self
            .connection
            .prepare_cached("SELECT state FROM thread_owners WHERE thread_id = ?1")
            .and_then(|mut statement| {
                statement
                    .query_row([site.thread_id], |row| row.get(0))
                    .optional()
            })
            .map_err(|error| site.sql(DbPhase::Read, error))?;
        Ok(serde_json::from_str(&json.ok_or(RegistryError::Absent)?)?)
    }

    pub fn start(&mut self, owner: &ThreadOwner) -> Result<(), RegistryError> {
        let site = self.site(DbOperation::Start, &owner.thread_id, Instant::now(), None);
        let changed = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO thread_owners (thread_id, state) VALUES (?1, ?2)",
                params![owner.thread_id, serde_json::to_string(owner)?],
            )
            .map_err(|error| site.sql(DbPhase::Upsert, error))?;
        if changed == 0 {
            return Err(RegistryError::Exists);
        }
        Ok(())
    }

    /// Create an independent child owner only while the source generation still
    /// accepts work. Checking the source and reserving the target share one write
    /// transaction; a target collision never adopts an unrelated thread's lease.
    /// `prepare` may have external effects, so this is never retried.
    pub(super) fn prepare_child<T>(
        &mut self,
        source: &super::OwnerPermit,
        thread_id: &str,
        now_ms: u64,
        prepare: impl FnOnce() -> Result<T, RegistryError>,
    ) -> Result<(ThreadOwner, T), RegistryError> {
        let started = Instant::now();
        let db = self.connection.path().unwrap_or("").to_owned();
        let site = Site {
            db: &db,
            operation: DbOperation::Child,
            thread_id: &source.thread_id,
            attempt: self.attempt,
            started,
            payload_bytes: None,
            quiet_contention: false,
        };
        // Drain ownership readers before taking the runtime goal lock. An
        // IMMEDIATE transaction would defer that wait until commit and invert
        // the GUI's ownership-read -> goal-control order.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Exclusive)
            .map_err(|error| site.sql(DbPhase::Begin, error))?;
        let json: Option<String> = transaction
            .query_row(
                "SELECT state FROM thread_owners WHERE thread_id = ?1",
                [&source.thread_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| site.sql(DbPhase::Read, error))?;
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
        let changed = transaction
            .execute(
                "INSERT OR IGNORE INTO thread_owners (thread_id, state) VALUES (?1, ?2)",
                params![thread_id, serde_json::to_string(&child)?],
            )
            .map_err(|error| site.sql(DbPhase::Upsert, error))?;
        if changed == 0 {
            return Err(RegistryError::Exists);
        }
        let prepared = prepare()?;
        transaction
            .commit()
            .map_err(|error| site.sql(DbPhase::Commit, error))?;
        Ok((child, prepared))
    }

    /// Apply `update` in one IMMEDIATE transaction. Never retried: `update`
    /// is an arbitrary callback. Retrying callers use [`Self::update_as`] with
    /// a pure state transition they rebuild for every attempt.
    pub fn update(
        &mut self,
        thread_id: &str,
        update: impl FnOnce(&mut ThreadOwner) -> Result<(), OwnershipError>,
    ) -> Result<ThreadOwner, RegistryError> {
        self.update_as(DbOperation::Update, thread_id, update)
    }

    pub(super) fn update_as(
        &mut self,
        operation: DbOperation,
        thread_id: &str,
        update: impl FnOnce(&mut ThreadOwner) -> Result<(), OwnershipError>,
    ) -> Result<ThreadOwner, RegistryError> {
        let started = Instant::now();
        let db = self.connection.path().unwrap_or("").to_owned();
        let site = Site {
            db: &db,
            operation,
            thread_id,
            attempt: self.attempt,
            started,
            payload_bytes: None,
            quiet_contention: false,
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| site.sql(DbPhase::Begin, error))?;
        let json: Option<String> = transaction
            .query_row(
                "SELECT state FROM thread_owners WHERE thread_id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| site.sql(DbPhase::Read, error))?;
        let mut owner: ThreadOwner = serde_json::from_str(&json.ok_or(RegistryError::Absent)?)?;
        let previous_generation = owner.lease.generation;
        update(&mut owner)?;
        if owner.state == super::OwnerState::Released
            || owner.lease.generation != previous_generation
        {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO owner_checkpoints VALUES (?1, ?2, '[]')",
                    params![thread_id, previous_generation.to_string()],
                )
                .map_err(|error| site.sql(DbPhase::Upsert, error))?;
        }
        transaction
            .execute(
                "UPDATE thread_owners SET state = ?2 WHERE thread_id = ?1",
                params![thread_id, serde_json::to_string(&owner)?],
            )
            .map_err(|error| site.sql(DbPhase::Upsert, error))?;
        transaction
            .commit()
            .map_err(|error| site.sql(DbPhase::Commit, error))?;
        Ok(owner)
    }

    pub fn checkpoint_permit(
        &mut self,
        permit: &super::OwnerPermit,
        _messages: &[providers::Message],
    ) -> Result<ThreadOwner, RegistryError> {
        let input = super::checkpoint::CheckpointInput {
            boundary: if permit.run_id.is_some() {
                super::CheckpointBoundary::TurnEnd
            } else {
                super::CheckpointBoundary::Idle
            },
            ..Default::default()
        };
        let payload = self.serialize_checkpoint(permit, &input)?;
        self.checkpoint_serialized(permit, &payload)
    }

    /// Serialize a checkpoint before any write lock is taken. A failure leaves
    /// the registry untouched; no ownership decision is made here.
    pub(super) fn serialize_checkpoint<P: serde::Serialize + ?Sized>(
        &self,
        permit: &super::OwnerPermit,
        messages: &P,
    ) -> Result<String, RegistryError> {
        let started = Instant::now();
        serde_json::to_string(messages).map_err(|error| {
            self.site(DbOperation::Checkpoint, &permit.thread_id, started, None)
                .json(DbPhase::Serialize, error)
        })
    }

    /// Store an already serialized checkpoint. Reading the owner, validating
    /// the permit and writing both rows stay in one IMMEDIATE transaction, so a
    /// claim committed after serialization fences this write.
    pub(super) fn checkpoint_serialized(
        &mut self,
        permit: &super::OwnerPermit,
        payload: &str,
    ) -> Result<ThreadOwner, RegistryError> {
        let thread_id = &permit.thread_id;
        let token = &permit.lease;
        let started = Instant::now();
        let db = self.connection.path().unwrap_or("").to_owned();
        let mut site = Site {
            db: &db,
            operation: DbOperation::Checkpoint,
            thread_id,
            attempt: self.attempt,
            started,
            payload_bytes: Some(payload.len()),
            quiet_contention: false,
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| site.sql(DbPhase::Begin, error))?;
        let json: String = transaction
            .query_row(
                "SELECT state FROM thread_owners WHERE thread_id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .map_err(|error| site.sql(DbPhase::Read, error))?;
        let mut owner: ThreadOwner = serde_json::from_str(&json)?;
        owner.validate(token)?;
        if owner.state == super::OwnerState::Released {
            return Err(OwnershipError::Fenced.into());
        }
        let input: super::checkpoint::CheckpointInput = serde_json::from_str(payload)?;
        match permit.run_id.as_deref() {
            Some(run) if owner.active_runs.contains(run) => owner.checkpoint_run(token, run)?,
            Some(_) if input.boundary != super::CheckpointBoundary::Terminal => {
                // An idempotent run checkpoint must not report an idle barrier
                // while another run still owns an active turn.
                if input.boundary == super::CheckpointBoundary::Idle && owner.active_turn {
                    return Err(OwnershipError::Active.into());
                }
                return Ok(owner);
            }
            Some(_) => {}
            None => owner.checkpoint(token)?,
        }
        if input.boundary == super::CheckpointBoundary::Idle && !owner.active_runs.is_empty() {
            return Err(OwnershipError::Active.into());
        }
        // Legacy arrays are never brought into the metadata merge.
        let previous: Option<String> = transaction.query_row(
            "SELECT CASE WHEN substr(ltrim(payload), 1, 1) = '{' THEN payload END FROM owner_checkpoints WHERE thread_id=?1",
            [thread_id], |row| row.get(0),
        ).optional().map_err(|error| site.sql(DbPhase::Read, error))?.flatten();
        let row = super::OwnerCheckpoint::advance(
            previous.as_deref(),
            &owner,
            permit,
            input.boundary,
            input.snapshot,
        );
        let payload =
            serde_json::to_string(&row).map_err(|error| site.json(DbPhase::Serialize, error))?;
        site.payload_bytes = Some(payload.len());
        transaction
            .execute(
                "INSERT INTO owner_checkpoints VALUES (?1, ?2, ?3) ON CONFLICT(thread_id) DO UPDATE SET generation=excluded.generation, payload=excluded.payload",
                params![thread_id, token.generation.to_string(), &payload],
            )
            .map_err(|error| site.sql(DbPhase::Upsert, error))?;
        transaction
            .execute(
                "UPDATE thread_owners SET state=?2 WHERE thread_id=?1",
                params![thread_id, serde_json::to_string(&owner)?],
            )
            .map_err(|error| site.sql(DbPhase::Upsert, error))?;
        transaction
            .commit()
            .map_err(|error| site.sql(DbPhase::Commit, error))?;
        tracing::debug!(
            target: super::observe::TARGET,
            db = %db,
            operation = DbOperation::Checkpoint.as_str(),
            thread_id = %thread_id,
            attempt = self.attempt,
            elapsed_ms = super::observe::millis(started.elapsed()),
            payload_bytes = payload.len(),
            "ownership checkpoint committed"
        );
        Ok(owner)
    }
}
