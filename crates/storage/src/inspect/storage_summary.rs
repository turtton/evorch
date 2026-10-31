//! `evorch inspect storage`: where the database's bytes go.
//!
//! Every figure is a count or a byte length. No column that can hold conversation text
//! is ever selected, only its `OCTET_LENGTH`, which SQLite answers from the record
//! header without reading overflow pages. All reads share one transaction so the
//! numbers describe a single snapshot even while another process writes.
//!
//! The default (logical) view takes seconds on a large store. `physical` adds a `dbstat`
//! walk of every page, which is slow, and the file and pragma figures that explain
//! the difference between logical bytes and file size.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use crate::{Database, StorageError};

/// Threads listed by default.
pub const DEFAULT_TOP_THREADS: usize = 10;
const MAX_TOP_THREADS: usize = 1_000;
/// Rows whose `subkind` the event backfill has not filled yet.
const UNBACKFILLED: &str = "(unbackfilled)";
const BUCKETS: [&str; 4] = ["active", "archived", "orphan", "unattributed"];

/// Bytes a run snapshot row occupies in its four large columns, as stored (a
/// compressed value counts its compressed length).
const RUN_CONTEXT_BYTES: &str = "(OCTET_LENGTH(s.messages_json) + OCTET_LENGTH(s.checkpoints_json) \
     + OCTET_LENGTH(s.config_json) + OCTET_LENGTH(s.host_inputs_json))";

/// What `storage_summary` measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageSummaryOptions {
    /// Threads in `top_threads`, clamped to `1..=1000`.
    pub top: usize,
    /// Also walk `dbstat` and read the file-level pragmas. Slow on a large store.
    pub physical: bool,
}

impl Default for StorageSummaryOptions {
    fn default() -> Self {
        Self {
            top: DEFAULT_TOP_THREADS,
            physical: false,
        }
    }
}

impl Database {
    /// Sizes of the tables, events, run snapshots and conversation index, split by the
    /// thread they belong to. `path` is the file this database was opened from.
    ///
    /// # Errors
    /// Returns an error if SQLite access or a file size lookup fails.
    pub fn storage_summary(
        &self,
        path: &Path,
        options: StorageSummaryOptions,
    ) -> Result<Value, StorageError> {
        let transaction = self.conn.unchecked_transaction()?;
        let conn: &Connection = &transaction;
        let sizes = crate::db::file_sizes(path)?;
        let mut summary = Map::new();
        summary.insert(
            "files".into(),
            json!({
                "db_bytes": sizes.db,
                "wal_bytes": sizes.wal,
                "shm_bytes": sizes.shm,
                "total_bytes": sizes.total(),
            }),
        );
        summary.insert("tables".into(), tables(conn)?);
        summary.insert("events".into(), events(conn)?);
        summary.insert("run_contexts".into(), run_contexts(conn)?);
        summary.insert("conversation_index".into(), conversation_index(conn)?);
        summary.insert("threads".into(), threads(conn)?);
        summary.insert(
            "attribution".into(),
            json!({
                "events": attribution(conn, "events", "OCTET_LENGTH(s.payload)")?,
                "run_contexts": attribution(conn, "run_contexts", RUN_CONTEXT_BYTES)?,
                "conversation_index": attribution(conn, "thread_index_runs", "s.charged_bytes")?,
            }),
        );
        summary.insert(
            "top_threads".into(),
            top_threads(conn, options.top.clamp(1, MAX_TOP_THREADS))?,
        );
        if options.physical {
            summary.insert("physical".into(), physical(conn)?);
        }
        Ok(Value::Object(summary))
    }
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn is_virtual(sql: Option<&str>) -> bool {
    sql.is_some_and(|sql| {
        sql.trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE VIRTUAL")
    })
}

/// Row count and the stored length of every text-like column, per ordinary table.
/// Virtual tables are skipped: their content lives in shadow tables listed on their own.
fn tables(conn: &Connection) -> Result<Value, StorageError> {
    let mut statement = conn.prepare(
        "SELECT name, sql FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
    )?;
    let names: Vec<String> = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .filter_map(|row| match row {
            Ok((name, sql)) => (!is_virtual(sql.as_deref())).then_some(Ok(name)),
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<_, _>>()?;
    let mut columns = conn.prepare("SELECT name, type FROM pragma_table_info(?1)")?;
    let mut listed = Vec::new();
    for name in names {
        let lengths: Vec<String> = columns
            .query_map([&name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, declared)| {
                let declared = declared.to_ascii_uppercase();
                declared.is_empty()
                    || ["TEXT", "CHAR", "CLOB", "BLOB"]
                        .iter()
                        .any(|kind| declared.contains(kind))
            })
            .map(|(column, _)| format!("COALESCE(OCTET_LENGTH({}), 0)", quote(&column)))
            .collect();
        let bytes = if lengths.is_empty() {
            "0".to_owned()
        } else {
            lengths.join(" + ")
        };
        let (rows, value_bytes): (i64, i64) = conn.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM({bytes}), 0) FROM {}",
                quote(&name)
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        listed.push((value_bytes, name, rows));
    }
    listed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(Value::Array(
        listed
            .into_iter()
            .map(|(value_bytes, table, rows)| {
                json!({ "table": table, "rows": rows, "value_bytes": value_bytes })
            })
            .collect(),
    ))
}

fn events(conn: &Connection) -> Result<Value, StorageError> {
    let mut statement = conn.prepare(
        "SELECT kind, subkind, COUNT(*), COALESCE(SUM(OCTET_LENGTH(payload)), 0)
         FROM events GROUP BY kind, subkind",
    )?;
    let mut by_kind = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    by_kind.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    let rows: i64 = by_kind.iter().map(|entry| entry.2).sum();
    let payload_bytes: i64 = by_kind.iter().map(|entry| entry.3).sum();
    let unbackfilled: i64 = by_kind
        .iter()
        .filter(|entry| entry.1.is_none())
        .map(|entry| entry.2)
        .sum();
    Ok(json!({
        "rows": rows,
        "payload_bytes": payload_bytes,
        "unbackfilled_rows": unbackfilled,
        "by_kind": by_kind
            .into_iter()
            .map(|(kind, subkind, rows, payload_bytes)| json!({
                "kind": kind,
                "subkind": subkind.unwrap_or_else(|| UNBACKFILLED.to_owned()),
                "rows": rows,
                "payload_bytes": payload_bytes,
            }))
            .collect::<Vec<_>>(),
    }))
}

fn run_contexts(conn: &Connection) -> Result<Value, StorageError> {
    // `*_len` is the plain byte length kept by every write; `OCTET_LENGTH` is what the
    // (possibly zstd-compressed) column occupies.
    Ok(conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(messages_len), 0), COALESCE(SUM(OCTET_LENGTH(messages_json)), 0),
                COALESCE(SUM(typeof(messages_json) = 'blob'), 0),
                COALESCE(SUM(checkpoints_len), 0), COALESCE(SUM(OCTET_LENGTH(checkpoints_json)), 0),
                COALESCE(SUM(typeof(checkpoints_json) = 'blob'), 0),
                COALESCE(SUM(OCTET_LENGTH(config_json)), 0),
                COALESCE(SUM(OCTET_LENGTH(host_inputs_json)), 0)
         FROM run_contexts",
        [],
        |row| {
            Ok(json!({
                "rows": row.get::<_, i64>(0)?,
                "messages": {
                    "plain_bytes": row.get::<_, i64>(1)?,
                    "stored_bytes": row.get::<_, i64>(2)?,
                    "compressed_rows": row.get::<_, i64>(3)?,
                },
                "checkpoints": {
                    "plain_bytes": row.get::<_, i64>(4)?,
                    "stored_bytes": row.get::<_, i64>(5)?,
                    "compressed_rows": row.get::<_, i64>(6)?,
                },
                "config_bytes": row.get::<_, i64>(7)?,
                "host_inputs_bytes": row.get::<_, i64>(8)?,
            }))
        },
    )?)
}

fn conversation_index(conn: &Connection) -> Result<Value, StorageError> {
    let mut statement = conn.prepare(
        "SELECT state, COUNT(*), SUM(charged_bytes), SUM(text_bytes), SUM(block_count)
         FROM thread_index_runs GROUP BY state ORDER BY state",
    )?;
    let mut by_state = Map::new();
    let (mut runs, mut charged) = (0_i64, 0_i64);
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let (count, charged_bytes): (i64, i64) = (row.get(1)?, row.get(2)?);
        runs += count;
        charged += charged_bytes;
        by_state.insert(
            row.get::<_, String>(0)?,
            json!({
                "runs": count,
                "charged_bytes": charged_bytes,
                "text_bytes": row.get::<_, i64>(3)?,
                "blocks": row.get::<_, i64>(4)?,
            }),
        );
    }
    Ok(json!({ "runs": runs, "charged_bytes": charged, "by_state": by_state }))
}

fn threads(conn: &Connection) -> Result<Value, StorageError> {
    Ok(conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(archived = 0), 0), COALESCE(SUM(archived = 1), 0),
                COALESCE(SUM(superseded), 0)
         FROM threads",
        [],
        |row| {
            Ok(json!({
                "total": row.get::<_, i64>(0)?,
                "active": row.get::<_, i64>(1)?,
                "archived": row.get::<_, i64>(2)?,
                "superseded": row.get::<_, i64>(3)?,
            }))
        },
    )?)
}

/// Rows and bytes of `table` (which has a `run_id` column) by the kind of thread its
/// run belongs to. `unattributed` rows carry no run id; `orphan` rows name a run that
/// no thread claims.
fn attribution(conn: &Connection, table: &str, bytes: &str) -> Result<Value, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT CASE WHEN s.run_id IS NULL THEN 'unattributed'
                     WHEN th.thread_id IS NULL THEN 'orphan'
                     WHEN th.archived = 1 THEN 'archived' ELSE 'active' END,
                COUNT(*), COALESCE(SUM({bytes}), 0)
         FROM {table} s
         LEFT JOIN thread_runs tr ON tr.run_id = s.run_id
         LEFT JOIN threads th ON th.thread_id = tr.thread_id
         GROUP BY 1"
    ))?;
    let mut found = BTreeMap::new();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        found.insert(
            row.get::<_, String>(0)?,
            (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
        );
    }
    let mut out = Map::new();
    for bucket in BUCKETS {
        let (rows, bytes) = found.get(bucket).copied().unwrap_or_default();
        out.insert(bucket.into(), json!({ "rows": rows, "bytes": bytes }));
    }
    Ok(Value::Object(out))
}

/// The `limit` threads holding the most bytes across events, run snapshots and the
/// conversation index. Only identifiers and sizes are listed, never titles.
fn top_threads(conn: &Connection, limit: usize) -> Result<Value, StorageError> {
    let mut statement = conn.prepare(&format!(
        "WITH ev AS (
             SELECT tr.thread_id AS thread_id, COUNT(*) AS n,
                    COALESCE(SUM(OCTET_LENGTH(s.payload)), 0) AS bytes
             FROM events s JOIN thread_runs tr ON tr.run_id = s.run_id GROUP BY tr.thread_id),
         rc AS (
             SELECT tr.thread_id AS thread_id, COUNT(*) AS n,
                    COALESCE(SUM({RUN_CONTEXT_BYTES}), 0) AS bytes
             FROM run_contexts s JOIN thread_runs tr ON tr.run_id = s.run_id GROUP BY tr.thread_id),
         ix AS (
             SELECT tr.thread_id AS thread_id, COALESCE(SUM(s.charged_bytes), 0) AS bytes
             FROM thread_index_runs s JOIN thread_runs tr ON tr.run_id = s.run_id
             GROUP BY tr.thread_id),
         ids AS (SELECT thread_id FROM threads UNION SELECT thread_id FROM thread_runs)
         SELECT ids.thread_id, th.archived,
                COALESCE(ev.n, 0), COALESCE(ev.bytes, 0),
                COALESCE(rc.n, 0), COALESCE(rc.bytes, 0), COALESCE(ix.bytes, 0),
                COALESCE(ev.bytes, 0) + COALESCE(rc.bytes, 0) + COALESCE(ix.bytes, 0) AS total
         FROM ids
         LEFT JOIN threads th ON th.thread_id = ids.thread_id
         LEFT JOIN ev ON ev.thread_id = ids.thread_id
         LEFT JOIN rc ON rc.thread_id = ids.thread_id
         LEFT JOIN ix ON ix.thread_id = ids.thread_id
         ORDER BY total DESC, ids.thread_id LIMIT ?1"
    ))?;
    let limit = i64::try_from(limit).map_err(|_| StorageError::OutOfRange("top thread count"))?;
    let rows = statement
        .query_map([limit], |row| {
            Ok(json!({
                "thread_id": row.get::<_, String>(0)?,
                "archived": row.get::<_, Option<i64>>(1)?.map(|flag| flag == 1),
                "events": { "rows": row.get::<_, i64>(2)?, "payload_bytes": row.get::<_, i64>(3)? },
                "run_contexts": { "rows": row.get::<_, i64>(4)?, "stored_bytes": row.get::<_, i64>(5)? },
                "conversation_index_charged_bytes": row.get::<_, i64>(6)?,
                "total_bytes": row.get::<_, i64>(7)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Array(rows))
}

#[derive(Default)]
struct Group {
    table_bytes: i64,
    index_bytes: i64,
    unused_bytes: i64,
    pages: i64,
    /// `(bytes, name, kind, pages, unused_bytes)` of each b-tree in the group.
    objects: Vec<(i64, String, &'static str, i64, i64)>,
}

/// Page-level sizes. `dbstat` covers b-tree pages only, so the freelist and the file
/// totals under `files` account for the rest of the file.
fn physical(conn: &Connection) -> Result<Value, StorageError> {
    let pragma = |name: &str| -> Result<i64, StorageError> {
        Ok(conn.pragma_query_value(None, name, |row| row.get(0))?)
    };
    let page_size = pragma("page_size")?;
    let freelist_count = pragma("freelist_count")?;
    let auto_vacuum = pragma("auto_vacuum")?;

    // Each b-tree is charged to the table it serves; FTS shadow tables to their
    // virtual table.
    let mut statement = conn.prepare("SELECT name, type, tbl_name, sql FROM sqlite_master")?;
    let schema = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let virtual_tables: Vec<&str> = schema
        .iter()
        .filter(|(_, kind, _, sql)| kind == "table" && is_virtual(sql.as_deref()))
        .map(|(name, ..)| name.as_str())
        .collect();
    let mut owner = BTreeMap::<&str, (&str, &'static str)>::new();
    for (name, kind, table, _) in &schema {
        let group = virtual_tables
            .iter()
            .find(|virtual_table| {
                name.strip_prefix(**virtual_table)
                    .is_some_and(|rest| rest.starts_with('_'))
            })
            .copied()
            .unwrap_or(table.as_str());
        owner.insert(
            name,
            (group, if kind == "index" { "index" } else { "table" }),
        );
    }

    let mut groups = BTreeMap::<String, Group>::new();
    let mut statement =
        conn.prepare("SELECT name, COUNT(*), SUM(pgsize), SUM(unused) FROM dbstat GROUP BY name")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let (pages, bytes, unused): (i64, i64, i64) = (row.get(1)?, row.get(2)?, row.get(3)?);
        // dbstat reports the schema table under its legacy name.
        let (group, kind) = if name == "sqlite_master" {
            ("sqlite_schema", "table")
        } else {
            owner
                .get(name.as_str())
                .copied()
                .unwrap_or((name.as_str(), "table"))
        };
        let group = groups.entry(group.to_owned()).or_default();
        if kind == "index" {
            group.index_bytes += bytes;
        } else {
            group.table_bytes += bytes;
        }
        group.unused_bytes += unused;
        group.pages += pages;
        group.objects.push((bytes, name, kind, pages, unused));
    }
    let btree_bytes: i64 = groups
        .values()
        .map(|group| group.table_bytes + group.index_bytes)
        .sum();
    let mut tables: Vec<_> = groups.into_iter().collect();
    tables.sort_by(|a, b| {
        (b.1.table_bytes + b.1.index_bytes)
            .cmp(&(a.1.table_bytes + a.1.index_bytes))
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(json!({
        "page_size": page_size,
        "page_count": pragma("page_count")?,
        "freelist_count": freelist_count,
        "freelist_bytes": freelist_count.saturating_mul(page_size),
        "auto_vacuum": {
            "value": auto_vacuum,
            "mode": match auto_vacuum { 0 => "none", 1 => "full", 2 => "incremental", _ => "unknown" },
        },
        "user_version": pragma("user_version")?,
        "btree_bytes": btree_bytes,
        "tables": tables
            .into_iter()
            .map(|(table, mut group)| {
                group.objects.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                json!({
                    "table": table,
                    "total_bytes": group.table_bytes + group.index_bytes,
                    "table_bytes": group.table_bytes,
                    "index_bytes": group.index_bytes,
                    "unused_bytes": group.unused_bytes,
                    "pages": group.pages,
                    "objects": group.objects
                        .into_iter()
                        .map(|(bytes, name, kind, pages, unused)| json!({
                            "name": name, "kind": kind, "pages": pages,
                            "bytes": bytes, "unused_bytes": unused,
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests;
