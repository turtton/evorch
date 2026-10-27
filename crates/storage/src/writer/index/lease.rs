//! DB-scoped index lease with a fencing epoch (ADR 0041 M6 / R5).
//!
//! Every election runs in an IMMEDIATE transaction so a GUI and a CLI process
//! on the same database serialize on SQLite's write lock. A new owner always
//! gets `epoch + 1`, so a previous owner's guards stop matching even if it
//! resumes before noticing; an expired lease never commits (checked again by
//! the repository guard inside the commit transaction).

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::StorageError;
use crate::thread_index::{IndexLease, IndexOwner, IndexOwnerKind, LeaseResult};

fn kind_name(kind: IndexOwnerKind) -> &'static str {
    match kind {
        IndexOwnerKind::Gui => "gui",
        IndexOwnerKind::Cli => "cli",
    }
}

fn parse_kind(name: &str) -> Result<IndexOwnerKind, StorageError> {
    match name {
        "gui" => Ok(IndexOwnerKind::Gui),
        "cli" => Ok(IndexOwnerKind::Cli),
        _ => Err(StorageError::Serialization(
            "unknown index owner kind".into(),
        )),
    }
}

type LeaseRow = (Option<String>, Option<String>, u64, Option<i64>);

/// The stored lease, live or expired; `None` when nobody holds it.
pub(crate) fn current(conn: &Connection) -> Result<Option<IndexLease>, StorageError> {
    let row: Option<LeaseRow> = conn
        .query_row(
            "SELECT owner_id, owner_kind, epoch, expires_at_ns FROM thread_index_lease WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((Some(id), Some(kind), epoch, Some(expires_at_ns))) = row else {
        return Ok(None);
    };
    Ok(Some(IndexLease {
        owner: IndexOwner {
            id,
            kind: parse_kind(&kind)?,
        },
        epoch,
        expires_at_ns,
    }))
}

/// The stored lease if it has not expired at `now_ns`.
pub(crate) fn live(conn: &Connection, now_ns: i64) -> Result<Option<IndexLease>, StorageError> {
    Ok(current(conn)?.filter(|lease| lease.expires_at_ns > now_ns))
}

pub(crate) fn is_valid(
    conn: &Connection,
    lease: &IndexLease,
    now_ns: i64,
) -> Result<bool, StorageError> {
    Ok(live(conn, now_ns)?
        .is_some_and(|held| held.owner == lease.owner && held.epoch == lease.epoch))
}

fn expiry(now_ns: i64, duration_ns: i64) -> i64 {
    now_ns.saturating_add(duration_ns.max(1))
}

/// Elects inside the caller's transaction (used by request submission).
pub(crate) fn acquire_in(
    conn: &Connection,
    owner: &IndexOwner,
    now_ns: i64,
    duration_ns: i64,
) -> Result<LeaseResult, StorageError> {
    let held = current(conn)?;
    let expires_at_ns = expiry(now_ns, duration_ns);
    if let Some(lease) = held.as_ref().filter(|lease| lease.expires_at_ns > now_ns) {
        if lease.owner != *owner {
            return Ok(LeaseResult::HeldByOther(lease.clone()));
        }
        conn.execute(
            "UPDATE thread_index_lease SET expires_at_ns = ?1 WHERE id = 1",
            [expires_at_ns],
        )?;
        return Ok(LeaseResult::Acquired(IndexLease {
            expires_at_ns,
            ..lease.clone()
        }));
    }
    let epoch: u64 = conn.query_row(
        "SELECT epoch FROM thread_index_lease WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    let epoch = epoch
        .checked_add(1)
        .ok_or(StorageError::OutOfRange("thread index lease epoch"))?;
    conn.execute(
        "UPDATE thread_index_lease SET owner_id = ?1, owner_kind = ?2, epoch = ?3, \
         expires_at_ns = ?4 WHERE id = 1",
        params![owner.id, kind_name(owner.kind), epoch, expires_at_ns],
    )?;
    Ok(LeaseResult::Acquired(IndexLease {
        owner: owner.clone(),
        epoch,
        expires_at_ns,
    }))
}

fn immediate<T>(
    conn: &Connection,
    body: impl FnOnce(&Connection) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let transaction = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let value = body(&transaction)?;
    transaction.commit()?;
    Ok(value)
}

pub(crate) fn acquire(
    conn: &Connection,
    owner: &IndexOwner,
    now_ns: i64,
    duration_ns: i64,
) -> Result<LeaseResult, StorageError> {
    immediate(conn, |conn| acquire_in(conn, owner, now_ns, duration_ns))
}

/// Extends only the exact live lease; an expired or replaced lease is `Lost`
/// even if nobody took it over yet.
pub(crate) fn heartbeat(
    conn: &Connection,
    lease: &IndexLease,
    now_ns: i64,
    duration_ns: i64,
) -> Result<LeaseResult, StorageError> {
    immediate(conn, |conn| {
        if !is_valid(conn, lease, now_ns)? {
            return Ok(LeaseResult::Lost);
        }
        let expires_at_ns = expiry(now_ns, duration_ns);
        conn.execute(
            "UPDATE thread_index_lease SET expires_at_ns = ?1 WHERE id = 1",
            [expires_at_ns],
        )?;
        Ok(LeaseResult::Acquired(IndexLease {
            expires_at_ns,
            ..lease.clone()
        }))
    })
}

/// Releases only the caller's own epoch; the epoch itself never decreases.
pub(crate) fn release(conn: &Connection, lease: &IndexLease) -> Result<(), StorageError> {
    immediate(conn, |conn| {
        conn.execute(
            "UPDATE thread_index_lease SET owner_id = NULL, owner_kind = NULL, \
             expires_at_ns = NULL WHERE id = 1 AND owner_id = ?1 AND owner_kind = ?2 \
             AND epoch = ?3",
            params![lease.owner.id, kind_name(lease.owner.kind), lease.epoch],
        )?;
        Ok(())
    })
}
