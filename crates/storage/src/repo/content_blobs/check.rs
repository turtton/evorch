//! Read-only views over the content blobs: the integrity check behind
//! `evorch inspect check-bodies`, the figures of `inspect storage`, and blob lookup.
//!
//! Nothing here writes, collects or repairs. A blob that nothing refers to is reported
//! as *waiting for collection* when it is queued (or registered for the exclusive phase),
//! and as an `orphan_blob` problem when nothing will ever collect it.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Value, json};

use super::{OVERSIZED_FAMILY, OVERSIZED_TABLE, ref_tables, unreferenced_clause};
use crate::StorageError;
use crate::body_ref::{collect_tokens, hex};
use crate::body_store::{
    OwnerSpec, blob_bytes, decode_blob, decode_skeleton, unresolved, verify_decoded,
};
use crate::error::BodyUnresolvedReason;

/// Problems kept in a report; the count goes on.
pub(crate) const MAX_REPORTED_PROBLEMS: usize = 200;

/// Which owner rows `check_bodies` looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyScope<'a> {
    All,
    Run(&'a str),
    Thread(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Problem {
    pub kind: &'static str,
    pub owner: Option<String>,
    pub hash: Option<String>,
    pub detail: String,
}

#[derive(Debug, Default)]
pub(crate) struct CheckReport {
    pub owners_checked: usize,
    pub owner_rows: u64,
    pub tokens: u64,
    pub blobs_verified: u64,
    pub ref_tables: Vec<String>,
    /// Unreferenced blobs that the collector or the exclusive phase will take.
    pub waiting_for_collection: u64,
    pub problems: Vec<Problem>,
    pub problem_count: u64,
}

impl CheckReport {
    pub(super) fn add(
        &mut self,
        kind: &'static str,
        owner: Option<&str>,
        hash: Option<String>,
        detail: impl Into<String>,
    ) {
        self.problem_count += 1;
        if self.problems.len() < MAX_REPORTED_PROBLEMS {
            self.problems.push(Problem {
                kind,
                owner: owner.map(str::to_owned),
                hash,
                detail: detail.into(),
            });
        }
    }

    pub(crate) fn is_ok(&self) -> bool {
        self.problem_count == 0
    }
}

/// Checks the reference tables' shape, then every row of every owner in `owners`
/// against its reference rows, and (for [`BodyScope::All`]) every blob against the
/// owners. With `verify` each blob read is decompressed and hashed too.
pub(crate) fn check_bodies(
    conn: &Connection,
    owners: &[OwnerSpec],
    scope: BodyScope<'_>,
    verify: bool,
) -> Result<CheckReport, StorageError> {
    // One read snapshot for the whole check (a caller that checks more in the same
    // snapshot has already opened it), so a writer cannot make it disagree with itself.
    crate::repo::run_messages::read_snapshot(conn, || {
        check_bodies_in_snapshot(conn, owners, scope, verify)
    })
}

fn check_bodies_in_snapshot(
    conn: &Connection,
    owners: &[OwnerSpec],
    scope: BodyScope<'_>,
    verify: bool,
) -> Result<CheckReport, StorageError> {
    let mut report = CheckReport::default();
    let tables = ref_tables(conn)?;
    check_schema(conn, owners, &tables, &mut report)?;
    report.ref_tables = tables.clone();
    let mut verified: BTreeSet<[u8; 32]> = BTreeSet::new();
    for owner in owners {
        if !tables.iter().any(|table| table == owner.ref_table) {
            continue;
        }
        check_owner(conn, owner, scope, verify, &mut verified, &mut report)?;
    }
    if scope == BodyScope::All {
        check_blobs(conn, &tables, verify, &mut verified, &mut report)?;
    }
    Ok(report)
}

fn check_schema(
    conn: &Connection,
    owners: &[OwnerSpec],
    tables: &[String],
    report: &mut CheckReport,
) -> Result<(), StorageError> {
    for owner in owners {
        if !tables.iter().any(|table| table == owner.ref_table) {
            report.add(
                "owner_ref_table_missing",
                Some(owner.name),
                None,
                format!("reference table {} does not exist", owner.ref_table),
            );
        }
    }
    for table in tables {
        if !owners.iter().any(|owner| owner.ref_table == table) {
            report.add(
                "unregistered_ref_table",
                None,
                None,
                format!("{table} is not in the owner registry"),
            );
        }
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info(?1)")?
            .query_map([table], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        if !columns.iter().any(|column| column == "blob_id") {
            report.add(
                "ref_table_shape",
                None,
                None,
                format!("{table} has no blob_id"),
            );
            continue;
        }
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_list(?1)")?
            .query_map([table], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut blob_indexed = false;
        for index in indexes {
            let first: Option<String> = conn
                .prepare("SELECT name FROM pragma_index_info(?1) WHERE seqno = 0")?
                .query_row([index], |row| row.get(0))
                .optional()?;
            blob_indexed |= first.as_deref() == Some("blob_id");
        }
        if !blob_indexed {
            report.add(
                "ref_table_index_missing",
                None,
                None,
                format!("{table} has no index starting with blob_id"),
            );
        }
        let queues: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 \
             AND sql LIKE '%content_blob_gc_queue%'",
            [table],
            |row| row.get(0),
        )?;
        if queues == 0 {
            report.add(
                "ref_table_trigger_missing",
                None,
                None,
                format!("{table} has no AFTER DELETE trigger that queues blobs"),
            );
        }
        let violations: Vec<(Option<i64>, String)> = conn
            .prepare("SELECT rowid, parent FROM pragma_foreign_key_check(?1) LIMIT ?2")?
            .query_map(params![table, MAX_REPORTED_PROBLEMS as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        for (rowid, parent) in violations {
            report.add(
                "foreign_key_violation",
                None,
                None,
                format!("{table} row {rowid:?} has no row in {parent}"),
            );
        }
    }
    Ok(())
}

fn check_owner(
    conn: &Connection,
    owner: &OwnerSpec,
    scope: BodyScope<'_>,
    verify: bool,
    verified: &mut BTreeSet<[u8; 32]>,
    report: &mut CheckReport,
) -> Result<(), StorageError> {
    let mut conditions: Vec<String> = owner
        .filter
        .map(|filter| format!("({filter})"))
        .into_iter()
        .collect();
    let scope_value = match (scope, owner.run_column) {
        (BodyScope::All, _) => None,
        (_, None) => return Ok(()),
        (BodyScope::Run(run), Some(column)) => {
            conditions.push(format!("{column} = ?1"));
            Some(run)
        }
        (BodyScope::Thread(thread), Some(column)) => {
            conditions.push(format!(
                "{column} IN (SELECT run_id FROM thread_runs WHERE thread_id = ?1)"
            ));
            Some(thread)
        }
    };
    let key_columns: Vec<&str> = owner.keys.iter().map(|key| key.owner_column).collect();
    let select: Vec<&str> = key_columns
        .iter()
        .chain(owner.container_columns)
        .chain(owner.codec_column.iter())
        .copied()
        .collect();
    let mut sql = format!("SELECT {} FROM {}", select.join(", "), owner.owner_table);
    if !conditions.is_empty() {
        sql.push_str(&format!(" WHERE {}", conditions.join(" AND ")));
    }
    sql.push_str(&format!(" ORDER BY {}", key_columns.join(", ")));
    let key_where: Vec<String> = owner
        .keys
        .iter()
        .enumerate()
        .map(|(index, key)| format!("r.{} = ?{}", key.ref_column, index + 1))
        .collect();
    let mut refs = conn.prepare(&format!(
        "SELECT r.blob_id, b.hash, b.byte_len FROM {} r \
         LEFT JOIN content_blobs b ON b.id = r.blob_id WHERE {}",
        owner.ref_table,
        key_where.join(" AND ")
    ))?;
    let mut statement = conn.prepare(&sql)?;
    let mut rows = match scope_value {
        Some(value) => statement.query([value])?,
        None => statement.query([])?,
    };
    report.owners_checked += 1;
    while let Some(row) = rows.next()? {
        report.owner_rows += 1;
        let mut key = Vec::with_capacity(owner.keys.len());
        for index in 0..owner.keys.len() {
            key.push(row.get::<_, SqlValue>(index)?);
        }
        let label = owner.label(&key);
        let mut expected: BTreeMap<[u8; 32], BTreeSet<u64>> = BTreeMap::new();
        let mut readable = true;
        for index in 0..owner.container_columns.len() {
            let slot = owner.keys.len() + index;
            let text: String = match owner.codec_column {
                // A codec-tagged BLOB container: decode it with the row's own codec.
                Some(_) => {
                    let codec: i64 = row.get(owner.keys.len() + owner.container_columns.len())?;
                    match row.get_ref(slot)? {
                        rusqlite::types::ValueRef::Blob(data) => {
                            match decode_skeleton(codec, data, &label) {
                                Ok(text) => text,
                                Err(error) => {
                                    report.add(
                                        "container_decode",
                                        Some(&label),
                                        None,
                                        format!("{}: {error}", owner.container_columns[index]),
                                    );
                                    readable = false;
                                    continue;
                                }
                            }
                        }
                        _ => {
                            report.add(
                                "container_decode",
                                Some(&label),
                                None,
                                format!("{} is not a BLOB", owner.container_columns[index]),
                            );
                            readable = false;
                            continue;
                        }
                    }
                }
                None => {
                    let Some(text) = row
                        .get_ref(slot)?
                        .as_str_or_null()
                        .map_err(rusqlite::Error::from)?
                    else {
                        continue;
                    };
                    text.to_owned()
                }
            };
            let text = text.as_str();
            let parsed: Value = match serde_json::from_str(text) {
                Ok(parsed) => parsed,
                Err(error) => {
                    report.add(
                        "container_json",
                        Some(&label),
                        None,
                        format!("{}: {error}", owner.container_columns[index]),
                    );
                    readable = false;
                    continue;
                }
            };
            match collect_tokens(&parsed, &label) {
                Ok(tokens) => {
                    for token in tokens {
                        report.tokens += 1;
                        expected
                            .entry(token.hash)
                            .or_default()
                            .insert(token.byte_len);
                    }
                }
                Err(error) => {
                    report.add(
                        "invalid_token",
                        Some(&label),
                        None,
                        format!("{}: {error}", owner.container_columns[index]),
                    );
                    readable = false;
                }
            }
        }
        if !readable {
            continue;
        }
        let mut referenced: BTreeSet<[u8; 32]> = BTreeSet::new();
        let mut reference_rows = refs.query(params_from_iter(key.iter()))?;
        while let Some(reference) = reference_rows.next()? {
            let blob_id: i64 = reference.get(0)?;
            match reference.get::<_, Option<Vec<u8>>>(1)? {
                Some(hash) => match <[u8; 32]>::try_from(hash) {
                    Ok(hash) => {
                        referenced.insert(hash);
                    }
                    Err(_) => report.add(
                        "malformed_hash",
                        Some(&label),
                        None,
                        format!("blob {blob_id} has a hash that is not 32 bytes"),
                    ),
                },
                None => report.add(
                    "dangling_reference",
                    Some(&label),
                    None,
                    format!("reference to blob {blob_id}, which does not exist"),
                ),
            }
        }
        for (hash, token_lens) in &expected {
            let hash_text = hex(hash);
            if token_lens.len() > 1 {
                report.add(
                    "token_length_conflict",
                    Some(&label),
                    Some(hash_text.clone()),
                    format!("tokens for this blob declare different lengths: {token_lens:?}"),
                );
            }
            let blob: Option<(i64, i64)> = conn
                .prepare_cached("SELECT id, byte_len FROM content_blobs WHERE hash = ?1")?
                .query_row([&hash[..]], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            let Some((_, blob_len)) = blob else {
                report.add(
                    "blob_missing",
                    Some(&label),
                    Some(hash_text),
                    "no such blob",
                );
                continue;
            };
            for token_len in token_lens {
                if u64::try_from(blob_len).ok() != Some(*token_len) {
                    report.add(
                        "length_mismatch",
                        Some(&label),
                        Some(hash_text.clone()),
                        format!("token says {token_len} bytes, blob says {blob_len}"),
                    );
                }
            }
            if !referenced.contains(hash) {
                report.add(
                    "missing_reference",
                    Some(&label),
                    Some(hash_text),
                    "the row names this blob but has no reference row",
                );
            }
            if verify {
                verify_blob(conn, hash, &label, verified, report)?;
            }
        }
        for hash in referenced.difference(&expected.keys().copied().collect()) {
            report.add(
                "surplus_reference",
                Some(&label),
                Some(hex(hash)),
                "a reference row names a blob the row's containers do not contain",
            );
        }
    }
    Ok(())
}

/// Decompresses and hashes the blob once per check; a failure is a problem.
fn verify_blob(
    conn: &Connection,
    hash: &[u8; 32],
    label: &str,
    verified: &mut BTreeSet<[u8; 32]>,
    report: &mut CheckReport,
) -> Result<(), StorageError> {
    if !verified.insert(*hash) {
        return Ok(());
    }
    let hash_text = hex(hash);
    let outcome = conn
        .prepare_cached("SELECT byte_len, codec, data FROM content_blobs WHERE hash = ?1")?
        .query_row([&hash[..]], |row| {
            let (byte_len, codec) = (row.get(0)?, row.get(1)?);
            Ok(match blob_bytes(row.get_ref(2)?) {
                Some(data) => decode_blob(byte_len, codec, data, label, &hash_text),
                None => Err(unresolved(
                    label,
                    Some(hash_text.clone()),
                    BodyUnresolvedReason::MalformedRow,
                )),
            })
        })
        .optional()?;
    let Some(outcome) = outcome else {
        return Ok(());
    };
    report.blobs_verified += 1;
    let outcome = outcome.and_then(|text| verify_decoded(&text, hash, label));
    if let Err(error) = outcome {
        report.add(
            "blob_unreadable",
            Some(label),
            Some(hash_text),
            error.to_string(),
        );
    }
    Ok(())
}

/// Blobs no reference table names: waiting for collection, or orphans. With `verify`,
/// every blob that was not verified through an owner is verified here too.
fn check_blobs(
    conn: &Connection,
    tables: &[String],
    verify: bool,
    verified: &mut BTreeSet<[u8; 32]>,
    report: &mut CheckReport,
) -> Result<(), StorageError> {
    let sql = format!(
        "SELECT b.id, b.hash, \
                EXISTS (SELECT 1 FROM content_blob_gc_queue WHERE blob_id = b.id), \
                EXISTS (SELECT 1 FROM reclaim_oversized \
                        WHERE family_key = ?1 AND tbl = ?2 AND key = CAST(b.id AS TEXT)) \
         FROM content_blobs b WHERE 1{}",
        unreferenced_clause(tables, "b.id")
    );
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(params![OVERSIZED_FAMILY, OVERSIZED_TABLE])?;
    while let Some(row) = rows.next()? {
        let (id, hash): (i64, Vec<u8>) = (row.get(0)?, row.get(1)?);
        let (queued, registered): (bool, bool) = (row.get(2)?, row.get(3)?);
        if queued || registered {
            report.waiting_for_collection += 1;
        } else {
            report.add(
                "orphan_blob",
                None,
                Some(hex(&hash)),
                format!("blob {id} is referenced by no owner and nothing will collect it"),
            );
        }
    }
    if verify {
        let mut all = conn.prepare("SELECT hash FROM content_blobs ORDER BY id")?;
        let hashes: Vec<Vec<u8>> = all
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for hash in hashes {
            match <[u8; 32]>::try_from(hash) {
                Ok(hash) => verify_blob(conn, &hash, "content_blobs", verified, report)?,
                Err(hash) => report.add(
                    "malformed_hash",
                    None,
                    Some(hex(&hash)),
                    "a blob hash is not 32 bytes",
                ),
            }
        }
    }
    Ok(())
}

/// Metadata of one blob row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlobMeta {
    pub id: i64,
    pub hash: Vec<u8>,
    pub byte_len: i64,
    pub codec: i64,
    pub stored_bytes: u64,
}

/// Blobs whose hash starts with `prefix` (lowercase hex, 1 to 64 digits); at most two
/// are returned, which is enough to tell an ambiguous prefix. Uses a range on the unique
/// index on `hash`.
pub(crate) fn find_blobs(conn: &Connection, prefix: &str) -> Result<Vec<BlobMeta>, StorageError> {
    if prefix.is_empty()
        || prefix.len() > 64
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StorageError::Serialization(
            "a blob hash prefix is 1 to 64 lowercase hex digits".into(),
        ));
    }
    let bound = |fill: char| -> Result<[u8; 32], StorageError> {
        let mut text = prefix.to_owned();
        text.extend(std::iter::repeat_n(fill, 64 - prefix.len()));
        let mut bytes = [0_u8; 32];
        for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let digits = std::str::from_utf8(pair).unwrap_or("00");
            bytes[index] = u8::from_str_radix(digits, 16)
                .map_err(|_| StorageError::Serialization("bad hex".into()))?;
        }
        Ok(bytes)
    };
    let (low, high) = (bound('0')?, bound('f')?);
    let blobs = conn
        .prepare(
            "SELECT id, hash, byte_len, codec, OCTET_LENGTH(data) FROM content_blobs \
             WHERE hash >= ?1 AND hash <= ?2 ORDER BY hash LIMIT 2",
        )?
        .query_map([&low[..], &high[..]], |row| {
            Ok(BlobMeta {
                id: row.get(0)?,
                hash: row.get(1)?,
                byte_len: row.get(2)?,
                codec: row.get(3)?,
                stored_bytes: row.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(blobs)
}

/// The body of a blob, decoded and (always, here) hash-verified.
pub(crate) fn blob_text(conn: &Connection, meta: &BlobMeta) -> Result<String, StorageError> {
    let hash = hex(&meta.hash);
    let text = conn.query_row(
        "SELECT data FROM content_blobs WHERE id = ?1",
        [meta.id],
        |row| {
            Ok(match blob_bytes(row.get_ref(0)?) {
                Some(data) => decode_blob(meta.byte_len, meta.codec, data, "content_blobs", &hash),
                None => Err(unresolved(
                    "content_blobs",
                    Some(hash.clone()),
                    BodyUnresolvedReason::MalformedRow,
                )),
            })
        },
    )??;
    verify_decoded(&text, &meta.hash, "content_blobs")?;
    Ok(text)
}

/// Whether a blob is queued for collection and whether it is registered for the
/// exclusive phase.
pub(crate) fn blob_state(conn: &Connection, id: i64) -> Result<(bool, bool), StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM content_blob_gc_queue WHERE blob_id = ?1), \
                EXISTS (SELECT 1 FROM reclaim_oversized \
                        WHERE family_key = ?2 AND tbl = ?3 AND key = ?4)",
        params![id, OVERSIZED_FAMILY, OVERSIZED_TABLE, id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

/// One reference table's rows for a blob: `(table, total rows, listed owners)`.
pub(crate) type BlobReferences = (String, i64, Vec<Value>);

/// For each reference table that names blob `id`: how many rows do, and `limit` of the
/// owners (as `{key column: value}`) from the `offset`-th on, in key order. The owners of
/// one blob are a range of the `blob_id` index, so paging reads only that range.
pub(crate) fn blob_references(
    conn: &Connection,
    id: i64,
    offset: usize,
    limit: usize,
) -> Result<Vec<BlobReferences>, StorageError> {
    let mut found = Vec::new();
    for table in ref_tables(conn)? {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM \"{table}\" WHERE blob_id = ?1"),
            [id],
            |row| row.get(0),
        )?;
        if count == 0 {
            continue;
        }
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info(?1) WHERE name <> 'blob_id' ORDER BY cid")?
            .query_map([&table], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut statement = conn.prepare(&format!(
            "SELECT {columns} FROM \"{table}\" WHERE blob_id = ?1 ORDER BY {columns} \
             LIMIT ?2 OFFSET ?3",
            columns = columns.join(", ")
        ))?;
        let owners = statement
            .query_map(params![id, limit as i64, offset as i64], |row| {
                let mut owner = serde_json::Map::new();
                for (index, column) in columns.iter().enumerate() {
                    let value = match row.get::<_, SqlValue>(index)? {
                        SqlValue::Integer(number) => json!(number),
                        SqlValue::Text(text) => json!(text),
                        other => json!(format!("{other:?}")),
                    };
                    owner.insert(column.clone(), value);
                }
                Ok(Value::Object(owner))
            })?
            .collect::<Result<_, _>>()?;
        found.push((table, count, owners));
    }
    Ok(found)
}

/// The figures `inspect storage` reports about bodies. Streams the blob rows once; row
/// payloads are only measured (`OCTET_LENGTH`), never read.
pub(crate) fn body_stats(conn: &Connection) -> Result<Value, StorageError> {
    let mut by_codec: BTreeMap<i64, (i64, i64, i64)> = BTreeMap::new();
    {
        let mut statement =
            conn.prepare("SELECT codec, byte_len, OCTET_LENGTH(data) FROM content_blobs")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let entry = by_codec.entry(row.get(0)?).or_default();
            entry.0 += 1;
            entry.1 += row.get::<_, i64>(1)?;
            entry.2 += row.get::<_, i64>(2)?;
        }
    }
    let tables = ref_tables(conn)?;
    let mut references = Vec::new();
    for table in &tables {
        let rows: i64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
                row.get(0)
            })?;
        references.push(json!({ "table": table, "rows": rows }));
    }
    let shared: i64 = if tables.is_empty() {
        0
    } else {
        let all = tables
            .iter()
            .map(|table| format!("SELECT blob_id FROM \"{table}\""))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM (SELECT blob_id FROM ({all}) GROUP BY blob_id HAVING COUNT(*) > 1)"
            ),
            [],
            |row| row.get(0),
        )?
    };
    let unreferenced: Vec<(bool, bool)> = {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM content_blob_gc_queue WHERE blob_id = b.id), \
                    EXISTS (SELECT 1 FROM reclaim_oversized \
                            WHERE family_key = ?1 AND tbl = ?2 AND key = CAST(b.id AS TEXT)) \
             FROM content_blobs b WHERE 1{}",
            unreferenced_clause(&tables, "b.id")
        );
        conn.prepare(&sql)?
            .query_map(params![OVERSIZED_FAMILY, OVERSIZED_TABLE], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?
    };
    let queued: i64 = conn.query_row("SELECT COUNT(*) FROM content_blob_gc_queue", [], |row| {
        row.get(0)
    })?;
    let oversized: i64 = conn.query_row(
        "SELECT COUNT(*) FROM reclaim_oversized WHERE family_key = ?1 AND tbl = ?2",
        params![OVERSIZED_FAMILY, OVERSIZED_TABLE],
        |row| row.get(0),
    )?;
    let blobs: i64 = by_codec.values().map(|entry| entry.0).sum();
    Ok(json!({
        "blobs": blobs,
        "plain_bytes": by_codec.values().map(|entry| entry.1).sum::<i64>(),
        "stored_bytes": by_codec.values().map(|entry| entry.2).sum::<i64>(),
        "by_codec": by_codec
            .iter()
            .map(|(codec, (count, plain, stored))| json!({
                "codec": match codec { 0 => "plain", 1 => "zstd", _ => "unknown" },
                "blobs": count,
                "plain_bytes": plain,
                "stored_bytes": stored,
            }))
            .collect::<Vec<_>>(),
        "references": references,
        "shared_blobs": shared,
        "unreferenced": {
            "waiting_in_queue": unreferenced.iter().filter(|(queued, _)| *queued).count(),
            "waiting_for_exclusive_phase": unreferenced
                .iter()
                .filter(|(queued, registered)| !queued && *registered)
                .count(),
            "unregistered_orphans": unreferenced
                .iter()
                .filter(|(queued, registered)| !queued && !registered)
                .count(),
        },
        "gc_queue_entries": queued,
        "oversized_registrations": oversized,
        "token_threshold_bytes": crate::body_ref::TOKEN_THRESHOLD_BYTES,
    }))
}
