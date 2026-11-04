//! Checks that run between the last run's conversion and the removal of the old column.

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::body_store::PRODUCTION_OWNERS;
use crate::repo::content_blobs::{BodyScope, check_bodies, check_runs};
use crate::repo::run_messages::read_snapshot;

use super::super::invalid;
use super::super::offline::require_no_foreign_key_violations;

/// A digest of everything about the runs that the conversion must not change: identity,
/// role, parentage, phase, restorable flag, times, host input receipts and snapshot
/// revision (the revision the provenance, the index and the ownership receipts follow).
pub(super) fn metadata_fingerprint(conn: &Connection) -> Result<(u64, [u8; 32]), StorageError> {
    let mut statement = conn.prepare(
        "SELECT run_id, role, name, parent_run_id, terminal_phase, restorable, updated_at_ns, \
         host_inputs_json, snapshot_revision FROM run_contexts ORDER BY run_id",
    )?;
    let mut rows = statement.query([])?;
    let mut digest = Sha256::new();
    let mut count = 0_u64;
    let field = |digest: &mut Sha256, bytes: &[u8]| {
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    };
    while let Some(row) = rows.next()? {
        for index in [0, 1, 2] {
            field(&mut digest, row.get::<_, String>(index)?.as_bytes());
        }
        match row.get::<_, Option<String>>(3)? {
            Some(parent) => {
                field(&mut digest, b"1");
                field(&mut digest, parent.as_bytes());
            }
            None => field(&mut digest, b"0"),
        }
        field(&mut digest, row.get::<_, String>(4)?.as_bytes());
        field(&mut digest, &row.get::<_, i64>(5)?.to_le_bytes());
        field(&mut digest, &row.get::<_, i64>(6)?.to_le_bytes());
        field(&mut digest, row.get::<_, String>(7)?.as_bytes());
        field(&mut digest, &row.get::<_, i64>(8)?.to_le_bytes());
        count += 1;
    }
    Ok((count, digest.finalize().into()))
}

/// Everything must read back, and nothing of the old format may be left in a row.
pub(super) fn verify_all(conn: &Connection, runs_converted_here: bool) -> Result<(), StorageError> {
    if runs_converted_here {
        let unconverted: u64 = conn.query_row(
            "SELECT COUNT(*) FROM run_contexts WHERE messages_json != ''",
            [],
            |row| row.get(0),
        )?;
        if unconverted != 0 {
            return Err(invalid(format!(
                "{unconverted} runs still hold their conversation in the old column"
            )));
        }
    }
    super::provenance::verify_all(conn)?;
    let report = read_snapshot(conn, || {
        let mut report = check_bodies(conn, PRODUCTION_OWNERS, BodyScope::All, true)?;
        check_runs(conn, BodyScope::All, true, &mut report)?;
        Ok(report)
    })?;
    if !report.is_ok() {
        let first: Vec<String> = report
            .problems
            .iter()
            .take(5)
            .map(|problem| format!("{}: {}", problem.kind, problem.detail))
            .collect();
        return Err(invalid(format!(
            "verification found {} problems; first: {}",
            report.problem_count,
            first.join("; ")
        )));
    }
    require_no_foreign_key_violations(conn)
}
