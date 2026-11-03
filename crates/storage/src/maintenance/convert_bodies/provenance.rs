//! One provenance map's conversion: its entries become `run_message_provenance` rows and
//! its header columns are filled, in place and in one transaction, and the stored form is
//! read back through the normal reader and compared with the old map before the
//! transaction commits.
//!
//! `stage_snapshot` is never called: the snapshot revision, the status, the evidence, the
//! times and the source revision of the index do not move. A map the old column holds that
//! is not a typed `ProvenanceMap` stops the command before it changes anything (see
//! `source::analyze_provenance`); it is never replaced by an empty one.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::StorageError;
use crate::repo::thread_index::provenance_rows::{MapHeader, read_entries, write_entries};
use crate::thread_index::ProvenanceMap;

use super::super::invalid;
use super::runs::mismatch;

/// A seam for tests: called inside the transaction of a map at the named point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProvenancePoint {
    BeforeReadback,
    BeforeCommit,
}

/// The next run with a provenance sidecar after `after`, in `run_id` order.
pub(super) fn next_row(
    conn: &Connection,
    after: Option<&str>,
) -> Result<Option<String>, StorageError> {
    Ok(conn
        .prepare_cached(
            "SELECT run_id FROM run_context_provenance WHERE (?1 IS NULL OR run_id > ?1) \
             ORDER BY run_id LIMIT 1",
        )?
        .query_row(params![after], |row| row.get(0))
        .optional()?)
}

/// Everything of a sidecar row except the map: the conversion must not change it.
type Identity = (u64, String, u32, String, Option<String>, Option<String>);

fn identity(conn: &Connection, run_id: &str) -> Result<Identity, StorageError> {
    Ok(conn.query_row(
        "SELECT snapshot_revision, snapshot_sha256, format_version, status, reason_code, \
         evidence_json FROM run_context_provenance WHERE run_id = ?1",
        [run_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?)
}

/// Converts the map of `run_id`; returns how many entries became rows. On any error
/// nothing of the run is stored.
pub(super) fn convert_row(
    conn: &Connection,
    run_id: &str,
    hook: &mut dyn FnMut(ProvenancePoint, &Connection) -> Result<(), StorageError>,
) -> Result<u64, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let before = identity(&tx, run_id)?;
    let old: String = tx.query_row(
        "SELECT map_json FROM run_context_provenance WHERE run_id = ?1",
        [run_id],
        |row| row.get(0),
    )?;
    let map: ProvenanceMap = serde_json::from_str(&old)
        .map_err(|_| mismatch(run_id, "the provenance map is not a typed provenance map"))?;
    drop(old);
    let header = MapHeader::of(&map).columns()?;
    let changed = tx
        .prepare_cached(
            "UPDATE run_context_provenance SET map_version = ?2, raw_message_count = ?3, \
             checkpoint_count = ?4, map_message_count = ?5 WHERE run_id = ?1",
        )?
        .execute(params![run_id, header[0], header[1], header[2], header[3]])?;
    if changed != 1 {
        return Err(invalid(format!(
            "run {run_id} has {changed} provenance rows to update"
        )));
    }
    let written = write_entries(&tx, run_id, &map.messages)?;
    if written.inserted != map.messages.len() as u64 {
        return Err(mismatch(run_id, "the provenance rows already existed"));
    }
    hook(ProvenancePoint::BeforeReadback, &tx)?;
    let stored = read_map(&tx, run_id)?
        .ok_or_else(|| mismatch(run_id, "the provenance rows do not read back"))?;
    if stored != map {
        return Err(mismatch(
            run_id,
            "the provenance map differs after the round trip",
        ));
    }
    if identity(&tx, run_id)? != before {
        return Err(mismatch(
            run_id,
            "the provenance status or revision changed",
        ));
    }
    hook(ProvenancePoint::BeforeCommit, &tx)?;
    tx.commit()?;
    Ok(map.messages.len() as u64)
}

/// The map of `run_id` as the rows hold it; `None` when they do not make one.
fn read_map(conn: &Connection, run_id: &str) -> Result<Option<ProvenanceMap>, StorageError> {
    let columns: [i64; 4] = conn.query_row(
        "SELECT map_version, raw_message_count, checkpoint_count, map_message_count \
         FROM run_context_provenance WHERE run_id = ?1",
        [run_id],
        |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?]),
    )?;
    let Some(header) = MapHeader::from_columns(columns) else {
        return Ok(None);
    };
    Ok(read_entries(conn, run_id, &header)?
        .ok()
        .map(|messages| ProvenanceMap {
            version: header.version,
            raw_message_count: header.raw_message_count,
            checkpoint_count: header.checkpoint_count,
            messages,
        }))
}

/// Every map must read back through the rows, and the rows must be exactly the ones the
/// headers count (no row of a run without a header: the foreign key sees to that).
pub(super) fn verify_all(conn: &Connection) -> Result<(), StorageError> {
    let uneven: u64 = conn.query_row(
        "SELECT COUNT(*) FROM run_context_provenance p WHERE p.map_message_count != \
         (SELECT COUNT(*) FROM run_message_provenance c WHERE c.run_id = p.run_id)",
        [],
        |row| row.get(0),
    )?;
    if uneven != 0 {
        return Err(invalid(format!(
            "{uneven} provenance maps have a different number of rows than their header counts"
        )));
    }
    let mut cursor: Option<String> = None;
    while let Some(run_id) = next_row(conn, cursor.as_deref())? {
        if read_map(conn, &run_id)?.is_none() {
            return Err(mismatch(&run_id, "the provenance rows do not make a map"));
        }
        cursor = Some(run_id);
    }
    Ok(())
}
