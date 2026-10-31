//! `evorch inspect storage`: where the database's bytes go.
//!
//! Every figure is a count or a byte length. Large text columns are only ever measured
//! with `OCTET_LENGTH`, which SQLite answers from the record header without reading
//! overflow pages. To keep it that way no query here groups, sorts or joins rows that
//! carry such a column: each big table is streamed once and tallied in Rust, and the
//! tests check the query plans for sorters and temporary b-trees. All reads share one
//! transaction so the numbers describe a single snapshot even while another process
//! writes.
//!
//! The default (logical) view takes seconds on a large store. `physical` adds a `dbstat`
//! walk of every page, which is slow, and the pragma figures that explain the difference
//! between logical bytes and file size.

use std::collections::{BTreeMap, HashMap};
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
const ACTIVE: usize = 0;
const ARCHIVED: usize = 1;
const ORPHAN: usize = 2;
const UNATTRIBUTED: usize = 3;

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

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    rows: i64,
    bytes: i64,
}

impl Tally {
    fn add(&mut self, rows: i64, bytes: i64) {
        self.rows += rows;
        self.bytes += bytes;
    }

    fn merge(&mut self, other: Tally) {
        self.add(other.rows, other.bytes);
    }

    fn json(self) -> Value {
        json!({ "rows": self.rows, "bytes": self.bytes })
    }
}

/// Rows of one table tallied by the run they name.
#[derive(Debug, Default)]
struct ByRun {
    runs: HashMap<String, Tally>,
    no_run: Tally,
}

impl ByRun {
    fn add(&mut self, run: Option<&str>, bytes: i64) {
        let Some(run) = run else {
            return self.no_run.add(1, bytes);
        };
        match self.runs.get_mut(run) {
            Some(tally) => tally.add(1, bytes),
            None => {
                self.runs.insert(run.to_owned(), Tally { rows: 1, bytes });
            }
        }
    }
}

/// The SQL of every scan over the data tables. Kept apart from running it so the tests
/// can check each statement's plan.
#[derive(Debug)]
struct Queries {
    events: String,
    run_contexts: String,
    index: String,
    /// `(table, sql)`: row count and value bytes of each remaining ordinary table.
    others: Vec<(String, String)>,
}

impl Queries {
    fn new(conn: &Connection) -> Result<Self, StorageError> {
        let mut statement = conn.prepare(
            "SELECT name, sql FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
        )?;
        let tables = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut columns = conn.prepare("SELECT name, type FROM pragma_table_info(?1)")?;
        let mut value_bytes = BTreeMap::new();
        for (name, sql) in tables {
            if is_virtual(sql.as_deref()) {
                continue;
            }
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
            let expr = if lengths.is_empty() {
                "0".to_owned()
            } else {
                lengths.join(" + ")
            };
            value_bytes.insert(name, expr);
        }
        let mut take = |table: &str| value_bytes.remove(table).unwrap_or_else(|| "0".into());
        let events = format!(
            "SELECT kind, subkind, run_id, OCTET_LENGTH(payload), {} FROM events",
            take("events")
        );
        let run_contexts = format!(
            "SELECT run_id, messages_len, OCTET_LENGTH(messages_json),
                    typeof(messages_json) = 'blob', checkpoints_len,
                    OCTET_LENGTH(checkpoints_json), typeof(checkpoints_json) = 'blob',
                    OCTET_LENGTH(config_json), OCTET_LENGTH(host_inputs_json), {}
             FROM run_contexts",
            take("run_contexts")
        );
        let index = format!(
            "SELECT run_id, state, charged_bytes, text_bytes, block_count, {}
             FROM thread_index_runs",
            take("thread_index_runs")
        );
        let others = value_bytes
            .into_iter()
            .map(|(table, expr)| {
                let sql = format!(
                    "SELECT COUNT(*), COALESCE(SUM({expr}), 0) FROM {}",
                    quote(&table)
                );
                (table, sql)
            })
            .collect();
        Ok(Self {
            events,
            run_contexts,
            index,
            others,
        })
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
        let queries = Queries::new(conn)?;
        let mut tables = Vec::new();
        for (table, sql) in &queries.others {
            let (rows, value_bytes): (i64, i64) =
                conn.query_row(sql, [], |row| Ok((row.get(0)?, row.get(1)?)))?;
            tables.push((value_bytes, table.clone(), rows));
        }

        let events = scan_events(conn, &queries.events)?;
        let contexts = scan_run_contexts(conn, &queries.run_contexts)?;
        let index = scan_index(conn, &queries.index)?;
        tables.push((events.value_bytes, "events".into(), events.rows));
        tables.push((
            contexts.value_bytes,
            "run_contexts".into(),
            contexts.sums.rows,
        ));
        tables.push((index.value_bytes, "thread_index_runs".into(), index.runs));
        tables.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let mut attribution = Attribution::load(conn)?;
        let events_buckets = attribution.attribute(&events.by_run, |acc, tally| {
            acc.events.merge(tally);
        })?;
        let contexts_buckets = attribution.attribute(&contexts.by_run, |acc, tally| {
            acc.contexts.merge(tally);
        })?;
        let index_buckets = attribution.attribute(&index.by_run, |acc, tally| {
            acc.index_bytes += tally.bytes;
        })?;

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
        summary.insert(
            "tables".into(),
            Value::Array(
                tables
                    .into_iter()
                    .map(|(value_bytes, table, rows)| {
                        json!({ "table": table, "rows": rows, "value_bytes": value_bytes })
                    })
                    .collect(),
            ),
        );
        summary.insert("events".into(), events.json());
        summary.insert("run_contexts".into(), contexts.sums.json());
        summary.insert("conversation_index".into(), index.json());
        summary.insert("threads".into(), attribution.counts());
        summary.insert(
            "attribution".into(),
            json!({
                "events": buckets_json(events_buckets),
                "run_contexts": buckets_json(contexts_buckets),
                "conversation_index": buckets_json(index_buckets),
            }),
        );
        summary.insert(
            "top_threads".into(),
            attribution.top(options.top.clamp(1, MAX_TOP_THREADS)),
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

fn text<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> Result<Option<&'a str>, StorageError> {
    Ok(row
        .get_ref(index)?
        .as_str_or_null()
        .map_err(rusqlite::Error::from)?)
}

struct EventScan {
    rows: i64,
    payload_bytes: i64,
    value_bytes: i64,
    unbackfilled: i64,
    by_kind: Vec<(String, Option<String>, Tally)>,
    by_run: ByRun,
}

impl EventScan {
    fn json(mut self) -> Value {
        self.by_kind.sort_by(|a, b| {
            b.2.bytes
                .cmp(&a.2.bytes)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        json!({
            "rows": self.rows,
            "payload_bytes": self.payload_bytes,
            "unbackfilled_rows": self.unbackfilled,
            "by_kind": self.by_kind
                .into_iter()
                .map(|(kind, subkind, tally)| json!({
                    "kind": kind,
                    "subkind": subkind.unwrap_or_else(|| UNBACKFILLED.to_owned()),
                    "rows": tally.rows,
                    "payload_bytes": tally.bytes,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

fn scan_events(conn: &Connection, sql: &str) -> Result<EventScan, StorageError> {
    let mut scan = EventScan {
        rows: 0,
        payload_bytes: 0,
        value_bytes: 0,
        unbackfilled: 0,
        by_kind: Vec::new(),
        by_run: ByRun::default(),
    };
    let mut position = HashMap::<String, usize>::new();
    let mut key = String::new();
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let kind = text(row, 0)?.unwrap_or_default();
        let subkind = text(row, 1)?;
        let (payload, value): (i64, i64) = (row.get(3)?, row.get(4)?);
        scan.rows += 1;
        scan.payload_bytes += payload;
        scan.value_bytes += value;
        scan.unbackfilled += i64::from(subkind.is_none());
        scan.by_run.add(text(row, 2)?, payload);

        key.clear();
        key.push_str(kind);
        key.push('\0');
        key.push(if subkind.is_some() { 's' } else { 'u' });
        key.push_str(subkind.unwrap_or_default());
        match position.get(key.as_str()) {
            Some(&at) => scan.by_kind[at].2.add(1, payload),
            None => {
                position.insert(key.clone(), scan.by_kind.len());
                scan.by_kind.push((
                    kind.to_owned(),
                    subkind.map(str::to_owned),
                    Tally {
                        rows: 1,
                        bytes: payload,
                    },
                ));
            }
        }
    }
    Ok(scan)
}

#[derive(Default)]
struct ContextSums {
    rows: i64,
    messages_plain: i64,
    messages_stored: i64,
    messages_compressed: i64,
    checkpoints_plain: i64,
    checkpoints_stored: i64,
    checkpoints_compressed: i64,
    config: i64,
    host_inputs: i64,
}

impl ContextSums {
    fn json(&self) -> Value {
        json!({
            "rows": self.rows,
            "messages": {
                "plain_bytes": self.messages_plain,
                "stored_bytes": self.messages_stored,
                "compressed_rows": self.messages_compressed,
            },
            "checkpoints": {
                "plain_bytes": self.checkpoints_plain,
                "stored_bytes": self.checkpoints_stored,
                "compressed_rows": self.checkpoints_compressed,
            },
            "config_bytes": self.config,
            "host_inputs_bytes": self.host_inputs,
        })
    }
}

struct ContextScan {
    sums: ContextSums,
    value_bytes: i64,
    by_run: ByRun,
}

/// `*_len` is the plain byte length kept by every write; `OCTET_LENGTH` is what the
/// possibly zstd-compressed column occupies.
fn scan_run_contexts(conn: &Connection, sql: &str) -> Result<ContextScan, StorageError> {
    let mut scan = ContextScan {
        sums: ContextSums::default(),
        value_bytes: 0,
        by_run: ByRun::default(),
    };
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let sums = &mut scan.sums;
        sums.rows += 1;
        sums.messages_plain += row.get::<_, i64>(1)?;
        let messages_stored: i64 = row.get(2)?;
        sums.messages_stored += messages_stored;
        sums.messages_compressed += row.get::<_, i64>(3)?;
        sums.checkpoints_plain += row.get::<_, i64>(4)?;
        let checkpoints_stored: i64 = row.get(5)?;
        sums.checkpoints_stored += checkpoints_stored;
        sums.checkpoints_compressed += row.get::<_, i64>(6)?;
        let (config, host_inputs): (i64, i64) = (row.get(7)?, row.get(8)?);
        sums.config += config;
        sums.host_inputs += host_inputs;
        scan.value_bytes += row.get::<_, i64>(9)?;
        scan.by_run.add(
            text(row, 0)?,
            messages_stored + checkpoints_stored + config + host_inputs,
        );
    }
    Ok(scan)
}

struct IndexScan {
    runs: i64,
    charged: i64,
    value_bytes: i64,
    /// `(runs, charged_bytes, text_bytes, blocks)` per state.
    by_state: BTreeMap<String, (i64, i64, i64, i64)>,
    by_run: ByRun,
}

impl IndexScan {
    fn json(&self) -> Value {
        let by_state: Map<String, Value> = self
            .by_state
            .iter()
            .map(|(state, (runs, charged, text, blocks))| {
                (
                    state.clone(),
                    json!({
                        "runs": runs,
                        "charged_bytes": charged,
                        "text_bytes": text,
                        "blocks": blocks,
                    }),
                )
            })
            .collect();
        json!({ "runs": self.runs, "charged_bytes": self.charged, "by_state": by_state })
    }
}

fn scan_index(conn: &Connection, sql: &str) -> Result<IndexScan, StorageError> {
    let mut scan = IndexScan {
        runs: 0,
        charged: 0,
        value_bytes: 0,
        by_state: BTreeMap::new(),
        by_run: ByRun::default(),
    };
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let state = text(row, 1)?.unwrap_or_default().to_owned();
        let (charged, text_bytes, blocks): (i64, i64, i64) =
            (row.get(2)?, row.get(3)?, row.get(4)?);
        scan.runs += 1;
        scan.charged += charged;
        scan.value_bytes += row.get::<_, i64>(5)?;
        let entry = scan.by_state.entry(state).or_default();
        entry.0 += 1;
        entry.1 += charged;
        entry.2 += text_bytes;
        entry.3 += blocks;
        scan.by_run.add(text(row, 0)?, charged);
    }
    Ok(scan)
}

#[derive(Default)]
struct ThreadTotals {
    events: Tally,
    contexts: Tally,
    index_bytes: i64,
}

impl ThreadTotals {
    fn total(&self) -> i64 {
        self.events.bytes + self.contexts.bytes + self.index_bytes
    }
}

/// Attributes runs to threads with the rules the thread views use (bindings, the
/// `chat:` run name, a thread's recorded root, and parent links through saved
/// contexts and the usage ledger). A run resolving to a thread with no `threads` row
/// is an orphan, as is one that resolves to none.
struct Attribution<'a> {
    conn: &'a Connection,
    archived: HashMap<String, bool>,
    superseded: i64,
    totals: HashMap<String, ThreadTotals>,
    resolved: HashMap<String, Option<String>>,
}

impl<'a> Attribution<'a> {
    fn load(conn: &'a Connection) -> Result<Self, StorageError> {
        let mut statement = conn.prepare("SELECT thread_id, archived, superseded FROM threads")?;
        let mut archived = HashMap::new();
        let mut superseded = 0;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            archived.insert(row.get::<_, String>(0)?, row.get::<_, i64>(1)? == 1);
            superseded += row.get::<_, i64>(2)?;
        }
        let totals = archived
            .keys()
            .map(|thread| (thread.clone(), ThreadTotals::default()))
            .collect();
        Ok(Self {
            conn,
            archived,
            superseded,
            totals,
            resolved: HashMap::new(),
        })
    }

    fn counts(&self) -> Value {
        let archived = self.archived.values().filter(|archived| **archived).count();
        json!({
            "total": self.archived.len(),
            "active": self.archived.len() - archived,
            "archived": archived,
            "superseded": self.superseded,
        })
    }

    fn thread_of(&mut self, run: &str) -> Result<Option<&str>, StorageError> {
        if !self.resolved.contains_key(run) {
            let thread = crate::repo::threads::resolve_thread_id(self.conn, run)?
                .filter(|thread| self.archived.contains_key(thread));
            self.resolved.insert(run.to_owned(), thread);
        }
        Ok(self.resolved.get(run).and_then(Option::as_deref))
    }

    /// Splits `by_run` into the four buckets and hands each thread its share.
    fn attribute(
        &mut self,
        by_run: &ByRun,
        add: impl Fn(&mut ThreadTotals, Tally),
    ) -> Result<[Tally; 4], StorageError> {
        let mut buckets = [Tally::default(); 4];
        buckets[UNATTRIBUTED].merge(by_run.no_run);
        for (run, tally) in &by_run.runs {
            let Some(thread) = self.thread_of(run)?.map(str::to_owned) else {
                buckets[ORPHAN].merge(*tally);
                continue;
            };
            let bucket = if self.archived[&thread] {
                ARCHIVED
            } else {
                ACTIVE
            };
            buckets[bucket].merge(*tally);
            if let Some(totals) = self.totals.get_mut(&thread) {
                add(totals, *tally);
            }
        }
        Ok(buckets)
    }

    /// The `limit` threads holding the most bytes across events, run snapshots and the
    /// conversation index. Only identifiers and sizes are listed, never titles.
    fn top(&self, limit: usize) -> Value {
        let mut threads: Vec<_> = self.totals.iter().collect();
        threads.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(b.0)));
        Value::Array(
            threads
                .into_iter()
                .take(limit)
                .map(|(thread, totals)| {
                    json!({
                        "thread_id": thread,
                        "archived": self.archived[thread],
                        "events": {
                            "rows": totals.events.rows,
                            "payload_bytes": totals.events.bytes,
                        },
                        "run_contexts": {
                            "rows": totals.contexts.rows,
                            "stored_bytes": totals.contexts.bytes,
                        },
                        "conversation_index_charged_bytes": totals.index_bytes,
                        "total_bytes": totals.total(),
                    })
                })
                .collect(),
        )
    }
}

fn buckets_json(buckets: [Tally; 4]) -> Value {
    let mut out = Map::new();
    for (name, tally) in BUCKETS.into_iter().zip(buckets) {
        out.insert(name.into(), tally.json());
    }
    Value::Object(out)
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
