//! Structured diagnostics for ownership registry I/O.
//!
//! Records identify the database, operation, phase, SQLite code, attempt,
//! elapsed time, payload size and guard hold time. They never contain
//! checkpoint payloads, owner state or other message bodies. Records only go
//! to `tracing`, never back through the event bus, so reporting a failed
//! mutation guard cannot reacquire that same guard.
use std::fmt;
use std::time::{Duration, Instant};

use super::RegistryError;

/// `tracing` target of every ownership registry I/O record.
pub const TARGET: &str = "evorch::ownership::db";

/// Readers holding a generation guard this long delay checkpoint commits.
pub(super) const SLOW_GUARD_HOLD: Duration = Duration::from_millis(500);

/// The ownership registry operation a SQLite access belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbOperation {
    Attach,
    BeginTurn,
    Checkpoint,
    Child,
    Claim,
    Guard,
    Heartbeat,
    List,
    Start,
    Update,
    Validate,
}

impl DbOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Attach => "attach",
            Self::BeginTurn => "begin_turn",
            Self::Checkpoint => "checkpoint",
            Self::Child => "child",
            Self::Claim => "claim",
            Self::Guard => "guard",
            Self::Heartbeat => "heartbeat",
            Self::List => "list",
            Self::Start => "start",
            Self::Update => "update",
            Self::Validate => "validate",
        }
    }
}

impl fmt::Display for DbOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The step of an operation at which SQLite reported an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbPhase {
    Open,
    Begin,
    Read,
    Serialize,
    Upsert,
    Commit,
}

impl DbPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Begin => "begin",
            Self::Read => "read",
            Self::Serialize => "serialize",
            Self::Upsert => "upsert",
            Self::Commit => "commit",
        }
    }
}

impl fmt::Display for DbPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub(super) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Where a registry access happens. Converts errors into contextual
/// [`RegistryError`]s and records each failure exactly once.
pub(super) struct Site<'a> {
    pub db: &'a str,
    pub operation: DbOperation,
    pub thread_id: &'a str,
    pub attempt: u32,
    pub started: Instant,
    pub payload_bytes: Option<usize>,
    /// Nonblocking guards report contention by design; keep those quiet.
    pub quiet_contention: bool,
}

impl Site<'_> {
    pub(super) fn sql(&self, phase: DbPhase, error: rusqlite::Error) -> RegistryError {
        let (code, extended_code) = error.sqlite_error().map_or((None, None), |error| {
            (Some(error.code), Some(error.extended_code))
        });
        let contention = matches!(
            code,
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        );
        let elapsed_ms = millis(self.started.elapsed());
        macro_rules! record {
            ($level:expr) => {
                tracing::event!(
                    target: TARGET,
                    $level,
                    db = self.db,
                    operation = self.operation.as_str(),
                    phase = phase.as_str(),
                    thread_id = self.thread_id,
                    sqlite_code = ?code,
                    extended_code,
                    attempt = self.attempt,
                    elapsed_ms,
                    payload_bytes = self.payload_bytes,
                    %error,
                    "ownership registry operation failed"
                )
            };
        }
        if contention && self.quiet_contention {
            record!(tracing::Level::DEBUG);
        } else {
            record!(tracing::Level::WARN);
        }
        RegistryError::Database {
            operation: self.operation,
            phase,
            attempt: self.attempt,
            source: error,
        }
    }

    /// Record a JSON failure by category only: its message may quote data.
    pub(super) fn json(&self, phase: DbPhase, error: serde_json::Error) -> RegistryError {
        tracing::warn!(
            target: TARGET,
            db = self.db,
            operation = self.operation.as_str(),
            phase = phase.as_str(),
            thread_id = self.thread_id,
            category = ?error.classify(),
            attempt = self.attempt,
            elapsed_ms = millis(self.started.elapsed()),
            payload_bytes = self.payload_bytes,
            "ownership registry operation failed"
        );
        RegistryError::Json(error)
    }
}

/// A successful generation guard, measured until its read transaction ends.
pub(super) struct GuardHold {
    pub thread_id: String,
    pub generation: u64,
    pub since: Instant,
}

impl GuardHold {
    pub(super) fn finish(self, db: &str) {
        let held = self.since.elapsed();
        let held_ms = millis(held);
        if held >= SLOW_GUARD_HOLD {
            tracing::warn!(
                target: TARGET,
                db,
                operation = DbOperation::Guard.as_str(),
                thread_id = %self.thread_id,
                generation = self.generation,
                held_ms,
                "ownership guard held long enough to delay registry writers"
            );
        } else {
            tracing::trace!(
                target: TARGET,
                db,
                operation = DbOperation::Guard.as_str(),
                thread_id = %self.thread_id,
                generation = self.generation,
                held_ms,
                "ownership guard released"
            );
        }
    }
}
