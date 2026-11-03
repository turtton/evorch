//! The provenance map of a run as rows (PR A2c): a small header on
//! `run_context_provenance` and one `run_message_provenance` row per entry of
//! `ProvenanceMap.messages`, keyed by the entry's position in that array.
//!
//! A save compares each entry's SHA-256 and length with the stored row (the stored
//! `entry_json` is never read) and writes only the entries that differ, so a save that
//! appends one message writes one row however long the map is. A reader rebuilds the
//! typed map from the header and the rows and refuses anything that does not fit
//! together; the caller turns that into an `Unknown` provenance, never a verified one.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::thread_index::{MessageProvenance, ProvenanceMap};

/// The scalar fields of a [`ProvenanceMap`], as the header columns hold them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MapHeader {
    pub version: u32,
    pub raw_message_count: usize,
    pub checkpoint_count: usize,
    pub message_count: usize,
}

impl MapHeader {
    pub(crate) fn of(map: &ProvenanceMap) -> Self {
        Self {
            version: map.version,
            raw_message_count: map.raw_message_count,
            checkpoint_count: map.checkpoint_count,
            message_count: map.messages.len(),
        }
    }

    /// The header columns as the integers SQLite stores.
    pub(crate) fn columns(&self) -> Result<[i64; 4], StorageError> {
        let convert = |value: usize, what: &'static str| {
            i64::try_from(value).map_err(|_| StorageError::OutOfRange(what))
        };
        Ok([
            i64::from(self.version),
            convert(self.raw_message_count, "provenance raw message count")?,
            convert(self.checkpoint_count, "provenance checkpoint count")?,
            convert(self.message_count, "provenance map message count")?,
        ])
    }

    /// Reads the header columns back; `None` when one is not a count.
    pub(crate) fn from_columns(columns: [i64; 4]) -> Option<Self> {
        Some(Self {
            version: u32::try_from(columns[0]).ok()?,
            raw_message_count: usize::try_from(columns[1]).ok()?,
            checkpoint_count: usize::try_from(columns[2]).ok()?,
            message_count: usize::try_from(columns[3]).ok()?,
        })
    }
}

/// What one map write changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MapWrite {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub unchanged: u64,
}

pub(crate) fn entry_json(entry: &MessageProvenance) -> Result<String, StorageError> {
    serde_json::to_string(entry).map_err(|error| StorageError::Serialization(error.to_string()))
}

fn index_i64(index: usize) -> Result<i64, StorageError> {
    i64::try_from(index).map_err(|_| StorageError::OutOfRange("provenance map index"))
}

/// Makes the rows of `run_id` equal to `messages`, writing only what differs. The header
/// row must exist already (the foreign key), and the caller holds the write transaction.
///
/// Reads `(map_index, id, entry_sha256, entry_len)` of the stored rows and nothing else.
pub(crate) fn write_entries(
    conn: &Connection,
    run_id: &str,
    messages: &[MessageProvenance],
) -> Result<MapWrite, StorageError> {
    let mut stored: BTreeMap<i64, (i64, Vec<u8>, i64)> = BTreeMap::new();
    {
        let mut statement = conn.prepare_cached(
            "SELECT map_index, id, entry_sha256, entry_len FROM run_message_provenance \
             WHERE run_id = ?1 ORDER BY map_index",
        )?;
        let mut rows = statement.query([run_id])?;
        while let Some(row) = rows.next()? {
            stored.insert(row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?));
        }
    }
    let mut stats = MapWrite::default();
    for (index, entry) in messages.iter().enumerate() {
        let json = entry_json(entry)?;
        let sha = Sha256::digest(json.as_bytes());
        let len = index_i64(json.len())?;
        let map_index = index_i64(index)?;
        match stored.remove(&map_index) {
            Some((_, old_sha, old_len))
                if old_sha.as_slice() == sha.as_slice() && old_len == len =>
            {
                stats.unchanged += 1;
            }
            Some((id, ..)) => {
                conn.prepare_cached(
                    "UPDATE run_message_provenance SET entry_sha256 = ?2, entry_len = ?3, \
                     entry_json = ?4 WHERE id = ?1",
                )?
                .execute(params![id, sha.as_slice(), len, json])?;
                stats.updated += 1;
            }
            None => {
                conn.prepare_cached(
                    "INSERT INTO run_message_provenance(run_id, map_index, entry_sha256, \
                     entry_len, entry_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(params![run_id, map_index, sha.as_slice(), len, json])?;
                stats.inserted += 1;
            }
        }
    }
    if !stored.is_empty() {
        let deleted = conn
            .prepare_cached(
                "DELETE FROM run_message_provenance WHERE run_id = ?1 AND map_index >= ?2",
            )?
            .execute(params![run_id, index_i64(messages.len())?])?;
        stats.deleted = deleted as u64;
    }
    Ok(stats)
}

/// Why the rows of a run do not make a map. Never carries any of their content.
pub(crate) type Fault = &'static str;

/// Rebuilds the entries of `run_id` for `header`: the rows must be exactly
/// `0..header.message_count`, each with the length and hash it records, each a
/// `MessageProvenance`.
pub(crate) fn read_entries(
    conn: &Connection,
    run_id: &str,
    header: &MapHeader,
) -> Result<Result<Vec<MessageProvenance>, Fault>, StorageError> {
    let mut statement = conn.prepare_cached(
        "SELECT map_index, entry_sha256, entry_len, entry_json FROM run_message_provenance \
         WHERE run_id = ?1 ORDER BY map_index",
    )?;
    let mut rows = statement.query([run_id])?;
    let mut entries = Vec::with_capacity(header.message_count.min(4096));
    while let Some(row) = rows.next()? {
        if entries.len() >= header.message_count {
            return Ok(Err("more provenance rows than the header counts"));
        }
        let map_index: i64 = row.get(0)?;
        if usize::try_from(map_index).ok() != Some(entries.len()) {
            return Ok(Err("provenance rows are not consecutive from zero"));
        }
        let sha: Vec<u8> = row.get(1)?;
        let len: i64 = row.get(2)?;
        let json: String = row.get(3)?;
        if usize::try_from(len).ok() != Some(json.len()) {
            return Ok(Err("a provenance row's length differs from its text"));
        }
        if sha.as_slice() != Sha256::digest(json.as_bytes()).as_slice() {
            return Ok(Err("a provenance row's hash differs from its text"));
        }
        match serde_json::from_str::<MessageProvenance>(&json) {
            Ok(entry) => entries.push(entry),
            Err(_) => return Ok(Err("a provenance row is not a message provenance")),
        }
    }
    if entries.len() != header.message_count {
        return Ok(Err("fewer provenance rows than the header counts"));
    }
    Ok(Ok(entries))
}
