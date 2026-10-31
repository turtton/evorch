//! Archived-thread retention: candidates, family closure, confirmation, reclaim.
//!
//! Every function here takes one connection and assumes its caller serialises
//! writers: the exclusive-phase functions (`explore`, `finalize`, oversized
//! reclaim) must run while the caller holds the locks proving that no other
//! evorch process exists, the bounded `reclaim` steps on the writer thread.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::thread_retention::RetentionBinding;

mod candidates;
#[cfg(test)]
pub(crate) mod catalog;
mod closure;
mod confirm;
pub(crate) mod reclaim;

pub(crate) use candidates::schedule;
pub(crate) use closure::explore;
pub(crate) use confirm::finalize;

/// The rows the exploration of `family_key` read, with slack: what the fixed-point
/// check of the same, unchanged family needs.
pub(crate) fn finalize_budget(
    conn: &Connection,
    family_key: &str,
) -> Result<Option<crate::thread_retention::FinalizeBudget>, StorageError> {
    let explored: Option<i64> = conn
        .query_row(
            "SELECT explored_edges FROM retention_staging_state WHERE family_key = ?1",
            [family_key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(explored.map(|explored| {
        let explored = u64::try_from(explored).unwrap_or(0);
        crate::thread_retention::FinalizeBudget {
            max_edges: explored.saturating_add(explored / 4).saturating_add(1024),
        }
    }))
}

pub(crate) fn binding(conn: &Connection) -> Result<Option<RetentionBinding>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT ownership_root, sidebar_path, bound_at_ns FROM retention_binding WHERE id = 1",
            [],
            |row| {
                Ok(RetentionBinding {
                    ownership_root: row.get(0)?,
                    sidebar_path: row.get(1)?,
                    bound_at_ns: row.get(2)?,
                })
            },
        )
        .optional()?)
}

/// The database's random ID, created in a transaction of its own.
pub(crate) fn identity(conn: &Connection) -> Result<String, StorageError> {
    if let Some(id) = read_identity(conn)? {
        return Ok(id);
    }
    let fresh = random_id();
    conn.execute(
        "INSERT OR IGNORE INTO retention_identity(id, db_id) VALUES (1, ?1)",
        params![fresh],
    )?;
    read_identity(conn)?.ok_or(StorageError::OutOfRange("retention identity"))
}

fn read_identity(conn: &Connection) -> Result<Option<String>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT db_id FROM retention_identity WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

/// 26 base32 characters from the operating system's randomness; without it,
/// from the clock, process and a counter (uniqueness is all that is needed).
fn random_id() -> String {
    use std::io::Read;
    let mut bytes = [0_u8; 16];
    let from_os = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !from_os {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seed = format!(
            "{:?}:{}:{}",
            std::time::SystemTime::now(),
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        bytes.copy_from_slice(&Sha256::digest(seed.as_bytes())[..16]);
    }
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut id = String::with_capacity(26);
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 && id.len() < 26 {
            bits -= 5;
            id.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    id
}

#[cfg(test)]
mod tests {
    use crate::Database;
    use crate::repo::thread_index::purge_index_rows;
    use crate::thread_index::PurgeTarget;

    fn indexed_run(database: &Database, state_sql: &str) {
        let conn = &database.conn;
        conn.execute_batch(&format!(
            "INSERT INTO thread_index_runs(run_id, state, evicted_reason, source_revision, \
             snapshot_revision, source_context_sha256, extractor_version, accounting_version, \
             source_updated_at_ns, retention_at_ns, text_bytes, block_count, charged_bytes) \
             VALUES ('run', {state_sql}, 1, 1, 'x', 1, 0, 1, 1, 30, 5, 30);"
        ))
        .unwrap();
        for index in 0..5 {
            conn.execute(
                "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, \
                 block_index, role, kind, truncated, hash, text) \
                 VALUES ('run', 'message', ?1, ?2, 0, 'user', 'text', 0, 'h', 'abcdef')",
                rusqlite::params![format!("{index}:0"), index],
            )
            .unwrap();
        }
    }

    fn blocks(database: &Database) -> i64 {
        database
            .conn
            .query_row("SELECT COUNT(*) FROM conversation_blocks", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn an_archived_eviction_is_purged_in_bounded_steps_and_keeps_its_state() {
        // Given: a run evicted for archiving that still holds five blocks.
        let database = Database::open_in_memory().unwrap();
        indexed_run(&database, "'evicted', 'archived'");

        // When: purged two rows at a time, then with a text budget.
        let first =
            purge_index_rows(&database.conn, "run", PurgeTarget::Archived, 2, u64::MAX).unwrap();
        let second =
            purge_index_rows(&database.conn, "run", PurgeTarget::Archived, 10, 12).unwrap();
        let last =
            purge_index_rows(&database.conn, "run", PurgeTarget::Archived, 10, u64::MAX).unwrap();

        // Then: each step is bounded, the charge follows and the row stays evicted.
        assert_eq!((first.rows, first.remaining), (2, true));
        assert_eq!(
            (second.rows, second.text_bytes, second.remaining),
            (2, 12, true)
        );
        assert_eq!((last.rows, last.remaining), (1, false));
        assert_eq!(blocks(&database), 0);
        let (state, reason, charged, generation): (String, Option<String>, i64, i64) = database
            .conn
            .query_row(
                "SELECT state, evicted_reason, charged_bytes, work_generation FROM thread_index_runs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (state.as_str(), reason.as_deref(), charged),
            ("evicted", Some("archived"), 0)
        );
        assert_eq!(generation, 1);
        database
            .conn
            .execute_batch(
                "INSERT INTO conversation_fts(conversation_fts) VALUES('integrity-check')",
            )
            .unwrap();
    }

    #[test]
    fn a_purge_does_nothing_unless_the_run_state_allows_the_target() {
        // Given: an indexed run that is neither evicted for archiving nor tombstoned.
        let database = Database::open_in_memory().unwrap();
        indexed_run(&database, "'indexed', NULL");

        // When/Then: neither target may touch it.
        for target in [PurgeTarget::Archived, PurgeTarget::Tombstoned] {
            let progress = purge_index_rows(&database.conn, "run", target, 10, u64::MAX).unwrap();
            assert_eq!((progress.rows, progress.remaining), (0, false));
        }
        assert_eq!(blocks(&database), 5);
        // And: an unknown run is a no-op, not an error.
        assert_eq!(
            purge_index_rows(&database.conn, "missing", PurgeTarget::Tombstoned, 10, 10)
                .unwrap()
                .rows,
            0
        );
    }
}
