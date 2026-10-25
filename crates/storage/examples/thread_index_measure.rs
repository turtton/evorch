//! ADR 0041 Phase B capacity measurement (W6, order 2). Run with --release.
//!
//! The source store is opened `mode=ro` and copied with the SQLite online backup
//! API; every later query reads only the copy. Extraction goes through the
//! storage extractor (`extract_snapshot_bundle`), never a second extractor, and
//! no conversation text is printed. Output is one JSON document on stdout.
//!
//! The strict pass proves legacy positions from structured generation metadata;
//! unprovable runs remain `provenance_unknown`. Capacity also has two labelled
//! hypotheses that are NOT an indexable population: `upper_bound` treats every
//! block as native, and `fork_prefix_excluded` additionally drops the prefix a
//! fork/rewind root run inherited (`threads.fork_point_context_len`).

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, ffi, params};
use secret_guard::SecretRedactor;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use storage::thread_index::{
    ACCOUNTING_VERSION, AccountingPolicy, DEFAULT_ACCOUNTING_POLICY, charge_index_bytes,
    index_attribute_bytes,
};
use storage::thread_index::{
    BlockLocator, BlockOrigin, BlockProvenance, CopySource, ExtractedIndexInput, ExtractionError,
    ForkBoundary, MessageProvenance, PROVENANCE_FORMAT_VERSION, ProvenanceEvidence, ProvenanceMap,
    ProvenanceStatus, SnapshotBundle, SnapshotProvenance, extract_snapshot_bundle,
    parse_index_snapshot, snapshot_sha256,
};
use storage::{RunContextRecord, RunLedgerEntry};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const DEFAULT_BUDGETS: [u64; 3] = [256 << 20, 512 << 20, 1 << 30];
const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const FTS_DENOMINATOR: u64 = 1_000;

struct Options {
    source: Option<PathBuf>,
    copy: PathBuf,
    work_dir: PathBuf,
    fts_percent: u8,
    budgets: Vec<u64>,
    skip_store_dbstat: bool,
}

fn options() -> Result<Options> {
    let mut source = None;
    let mut copy = None;
    let mut work_dir = None;
    let mut reuse = false;
    let mut fts_percent = 100;
    let mut budgets = DEFAULT_BUDGETS.to_vec();
    let mut skip_store_dbstat = false;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--help" => {
                println!(
                    "thread_index_measure (--source <store.db> | --reuse-copy) --copy <copy.db> --work-dir <dir> [--fts-percent 1..=100] [--budgets 268435456,536870912,1073741824] [--skip-store-dbstat]\nOpens --source with mode=ro and copies it with the SQLite backup API to --copy (must not exist); with --reuse-copy an existing copy is measured instead. The FTS measurement database is created in --work-dir (must not contain one). Never prints conversation text."
                );
                std::process::exit(0);
            }
            "--reuse-copy" => reuse = true,
            "--skip-store-dbstat" => skip_store_dbstat = true,
            _ => {
                let value = args.next().ok_or("missing option value")?;
                match argument.as_str() {
                    "--source" => source = Some(PathBuf::from(value)),
                    "--copy" => copy = Some(PathBuf::from(value)),
                    "--work-dir" => work_dir = Some(PathBuf::from(value)),
                    "--fts-percent" => fts_percent = value.parse()?,
                    "--budgets" => {
                        budgets = value
                            .split(',')
                            .map(str::parse)
                            .collect::<std::result::Result<_, _>>()?;
                    }
                    other => return Err(format!("unknown option {other}").into()),
                }
            }
        }
    }
    if !(1..=100).contains(&fts_percent) {
        return Err("--fts-percent must be in 1..=100".into());
    }
    if source.is_some() == reuse {
        return Err("give exactly one of --source or --reuse-copy".into());
    }
    Ok(Options {
        source,
        copy: copy.ok_or("--copy is required")?,
        work_dir: work_dir.ok_or("--work-dir is required")?,
        fts_percent,
        budgets,
        skip_store_dbstat,
    })
}

fn uri_path(path: &Path) -> String {
    let mut out = String::new();
    for character in path.display().to_string().chars() {
        match character {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            other => out.push(other),
        }
    }
    out
}

/// Copies `source` into a new `copy` file without ever opening `source` writable.
fn backup_copy(source: &Path, copy: &Path) -> Result<Value> {
    if copy.exists() {
        return Err(format!("refusing to overwrite {}", copy.display()).into());
    }
    let source_conn = Connection::open_with_flags(
        format!("file:{}?mode=ro", uri_path(source)),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    if !source_conn.is_readonly(rusqlite::MAIN_DB)? {
        return Err("source connection is not read-only".into());
    }
    let copy_conn = Connection::open(copy)?;
    let started = Instant::now();
    // SAFETY: both connection handles stay valid for the whole block because the
    // connections outlive it and are not used elsewhere meanwhile; the backup
    // object is finished on every path before the block ends. rusqlite's `backup`
    // feature is not enabled for this crate, so the C API is called directly.
    let code = unsafe {
        let backup = ffi::sqlite3_backup_init(
            copy_conn.handle(),
            c"main".as_ptr(),
            source_conn.handle(),
            c"main".as_ptr(),
        );
        if backup.is_null() {
            return Err(format!(
                "sqlite3_backup_init failed with code {}",
                ffi::sqlite3_errcode(copy_conn.handle())
            )
            .into());
        }
        // -1 copies every page under one read snapshot, so concurrent writers to
        // the live store cannot make the copy mix two states.
        let mut step = ffi::sqlite3_backup_step(backup, -1);
        while step == ffi::SQLITE_BUSY || step == ffi::SQLITE_LOCKED {
            std::thread::sleep(Duration::from_millis(50));
            step = ffi::sqlite3_backup_step(backup, -1);
        }
        let finish = ffi::sqlite3_backup_finish(backup);
        if step == ffi::SQLITE_DONE {
            finish
        } else {
            step
        }
    };
    if code != ffi::SQLITE_OK {
        return Err(format!("SQLite backup failed with code {code}").into());
    }
    let seconds = started.elapsed().as_secs_f64();
    drop(source_conn);
    // The copy is self-contained in one file; measurement then opens it read-only.
    copy_conn.pragma_update(None, "journal_mode", "DELETE")?;
    drop(copy_conn);
    Ok(json!({
        "method": "sqlite3_backup_step(-1) from a mode=ro, is_readonly-verified connection",
        "seconds": seconds,
        "copy_bytes": std::fs::metadata(copy)?.len(),
    }))
}

fn open_read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        format!("file:{}?mode=ro", uri_path(path)),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn pragma(conn: &Connection, name: &str) -> Result<i64> {
    Ok(conn.pragma_query_value(None, name, |row| row.get(0))?)
}

fn dbstat(conn: &Connection, limit: Option<usize>) -> Result<Vec<(String, u64)>> {
    let mut statement = conn
        .prepare("SELECT name, pgsize FROM dbstat WHERE aggregate = TRUE ORDER BY pgsize DESC")?;
    let rows = statement.query_map([], |row| {
        let bytes: i64 = row.get(1)?;
        Ok((row.get(0)?, u64::try_from(bytes).unwrap_or(0)))
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
        if limit.is_some_and(|limit| result.len() >= limit) {
            break;
        }
    }
    Ok(result)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )?)
}

/// Time-ordered run IDs carry their creation time in the top 48 of 128 bits.
fn ulid_ms(run_id: &str) -> Option<i64> {
    let body = run_id.strip_prefix("run-")?;
    if body.len() != 26 {
        return None;
    }
    let mut value: u128 = 0;
    for byte in body.bytes() {
        let digit = CROCKFORD.iter().position(|&candidate| candidate == byte)?;
        value = value.checked_mul(32)?.checked_add(digit as u128)?;
    }
    i64::try_from(value >> 80).ok()
}

/// Days-from-civil inverse (proleptic Gregorian), so no date crate is needed.
fn utc(ns: i64) -> String {
    let seconds = ns.div_euclid(1_000_000_000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest / 60 % 60,
        rest % 60
    )
}

fn serde_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => name,
        _ => String::new(),
    }
}

fn error_code(error: ExtractionError) -> String {
    match error {
        ExtractionError::ProvenanceUnknown(reason) => {
            format!("provenance_unknown:{}", serde_name(&reason))
        }
        ExtractionError::InvalidJson => "invalid_json".to_owned(),
        ExtractionError::NoteRunMismatch => "note_run_mismatch".to_owned(),
    }
}

/// Measurement-only map: every block is native unless it lies in an inherited
/// fork prefix. Used for capacity hypotheses, never to promote real provenance.
fn assumed_provenance(
    record: &RunContextRecord,
    messages: &Value,
    checkpoint_count: usize,
    fork_prefix: Option<&(String, usize)>,
) -> SnapshotProvenance {
    let raw = messages.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut mapped = Vec::new();
    for (raw_index, message) in raw.iter().enumerate() {
        if message["role"] == "system" {
            continue;
        }
        let ordinal = mapped.len();
        let block_count = message["content"].as_array().map_or(0, Vec::len);
        let blocks = (0..block_count)
            .map(|block_index| BlockProvenance {
                block_index,
                origin: match fork_prefix {
                    Some((source_run, context_len)) if ordinal < *context_len => {
                        BlockOrigin::ForkCopy {
                            source: CopySource::Message {
                                run_id: source_run.clone(),
                                snapshot_revision: 0,
                                ordinal,
                                block_index,
                            },
                            original_origin: Box::new(BlockOrigin::Native),
                            boundary: ForkBoundary {
                                run_id: source_run.clone(),
                                snapshot_revision: 0,
                                context_len: *context_len,
                            },
                        }
                    }
                    _ => BlockOrigin::Native,
                },
                excluded_reason: None,
            })
            .collect();
        mapped.push(MessageProvenance {
            raw_index,
            ordinal,
            blocks,
        });
    }
    SnapshotProvenance {
        run_id: record.run_id.clone(),
        snapshot_revision: 0,
        snapshot_sha256: snapshot_sha256(&record.messages_json, &record.checkpoints_json),
        format_version: PROVENANCE_FORMAT_VERSION,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: raw.len(),
            checkpoint_count,
            messages: mapped,
        },
    }
}

#[derive(Clone, Copy, Default)]
struct Footprint {
    text: u64,
    attributes: u64,
    blocks: u64,
}

/// Attribute bytes from the shared accountant (`IndexAccounting::attribute_bytes`).
fn footprint(input: &ExtractedIndexInput) -> Footprint {
    Footprint {
        text: input.text_bytes,
        attributes: index_attribute_bytes(input).unwrap_or(u64::MAX),
        blocks: input.blocks.len() as u64,
    }
}

#[derive(Clone, Copy)]
struct Coefficients {
    per_block: u64,
    per_run: u64,
    fts_numerator: u64,
}

fn policy(coefficients: Coefficients) -> AccountingPolicy {
    AccountingPolicy {
        version: ACCOUNTING_VERSION,
        per_block_bytes: coefficients.per_block,
        per_run_bytes: coefficients.per_run,
        fts_numerator: coefficients.fts_numerator,
        fts_denominator: FTS_DENOMINATOR,
    }
}

/// Charges through the shared accountant with the coefficients derived here, so
/// the proposal is evaluated with the same formula production uses.
fn charge(footprint: Footprint, coefficients: Coefficients) -> u64 {
    charge_index_bytes(
        footprint.text,
        footprint.attributes,
        footprint.blocks,
        policy(coefficients),
    )
    .unwrap_or(u64::MAX)
}

#[derive(Default)]
struct KindStat {
    blocks: u64,
    bytes: u64,
    truncated: u64,
}

fn kind_key(block: &storage::thread_index::IndexBlock) -> String {
    let kind = serde_name(&block.kind);
    if kind == "text" {
        format!("{}_text", serde_name(&block.role))
    } else {
        kind
    }
}

struct RunSummary {
    run_id: String,
    retention_at_ns: i64,
    strict: std::result::Result<Footprint, String>,
    upper: std::result::Result<Footprint, String>,
    estimate: std::result::Result<Footprint, String>,
    sampled: bool,
}

struct FtsBuild {
    conn: Connection,
    path: PathBuf,
    max_wal_bytes: u64,
    max_commit_growth: u64,
    max_commit_text: u64,
    runs: u64,
    footprint: Footprint,
    seconds: f64,
}

struct IndexSizes {
    fts: u64,
    blocks: u64,
    runs: u64,
    stats: Vec<(String, u64)>,
}

const FTS_SCHEMA: &str = "
CREATE TABLE thread_index_runs (
    run_id TEXT PRIMARY KEY, state TEXT NOT NULL, source_revision INTEGER NOT NULL,
    indexed_revision INTEGER, source_context_sha256 TEXT, source_note_seq INTEGER,
    source_provenance_version INTEGER, extractor_version INTEGER NOT NULL,
    accounting_version INTEGER NOT NULL, source_updated_at_ns INTEGER NOT NULL,
    retention_at_ns INTEGER NOT NULL, indexed_at_ns INTEGER, text_bytes INTEGER NOT NULL,
    block_count INTEGER NOT NULL, charged_bytes INTEGER NOT NULL, priority INTEGER NOT NULL,
    attempt_count INTEGER NOT NULL, next_attempt_at_ns INTEGER, blocked_reason TEXT,
    last_error_code TEXT, source_present INTEGER NOT NULL, work_generation INTEGER NOT NULL
);
CREATE INDEX idx_thread_index_runs_work
    ON thread_index_runs(state, priority DESC, retention_at_ns DESC, run_id DESC);
CREATE INDEX idx_thread_index_runs_retention
    ON thread_index_runs(retention_at_ns, run_id) WHERE charged_bytes > 0;
CREATE TABLE conversation_blocks (
    id INTEGER PRIMARY KEY, run_id TEXT NOT NULL, source_kind TEXT NOT NULL,
    source_key TEXT NOT NULL, ordinal INTEGER, block_index INTEGER, note_seq INTEGER,
    role TEXT NOT NULL, kind TEXT NOT NULL, tool_name TEXT, truncated INTEGER NOT NULL,
    hash TEXT NOT NULL, text TEXT NOT NULL, UNIQUE(run_id, source_kind, source_key)
);
CREATE VIRTUAL TABLE conversation_fts USING fts5(
    text, content='conversation_blocks', content_rowid='id', tokenize='trigram', columnsize=0
);
";

impl FtsBuild {
    fn create(work_dir: &Path) -> Result<Self> {
        let path = work_dir.join("thread_index_measure_fts.db");
        if path.exists() {
            return Err(format!("refusing to overwrite {}", path.display()).into());
        }
        std::fs::create_dir_all(work_dir)?;
        let conn = Connection::open(&path)?;
        // Same file policy as `storage::db::pragma_init` for new databases.
        conn.pragma_update(None, "auto_vacuum", 2_i64)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "wal_autocheckpoint", 1_000_i64)?;
        conn.execute_batch(FTS_SCHEMA)?;
        Ok(Self {
            conn,
            path,
            max_wal_bytes: 0,
            max_commit_growth: 0,
            max_commit_text: 0,
            runs: 0,
            footprint: Footprint::default(),
            seconds: 0.0,
        })
    }

    fn file_bytes(&self) -> u64 {
        let size = |suffix: &str| {
            let mut name = self.path.clone().into_os_string();
            name.push(suffix);
            std::fs::metadata(name).map_or(0, |metadata| metadata.len())
        };
        size("") + size("-wal")
    }

    fn insert(&mut self, input: &ExtractedIndexInput, retention_at_ns: i64) -> Result<()> {
        let started = Instant::now();
        let before = self.file_bytes();
        let footprint = footprint(input);
        let transaction = self.conn.transaction()?;
        {
            let mut block_insert = transaction.prepare_cached(
                "INSERT INTO conversation_blocks (run_id, source_kind, source_key, ordinal, block_index, note_seq, role, kind, tool_name, truncated, hash, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            let mut fts_insert = transaction
                .prepare_cached("INSERT INTO conversation_fts (rowid, text) VALUES (?1, ?2)")?;
            for block in &input.blocks {
                let (source_kind, source_key, ordinal, block_index, note_seq) = match block.locator
                {
                    BlockLocator::Message {
                        ordinal,
                        block_index,
                    } => (
                        "message",
                        format!("{ordinal}:{block_index}"),
                        Some(ordinal as i64),
                        Some(block_index as i64),
                        None,
                    ),
                    BlockLocator::WorkingNote { note_seq } => (
                        "working_note",
                        note_seq.to_string(),
                        None,
                        None,
                        Some(note_seq as i64),
                    ),
                };
                block_insert.execute(params![
                    input.run_id,
                    source_kind,
                    source_key,
                    ordinal,
                    block_index,
                    note_seq,
                    serde_name(&block.role),
                    serde_name(&block.kind),
                    block.tool_name,
                    block.truncated,
                    block.hash,
                    block.text,
                ])?;
                fts_insert.execute(params![transaction.last_insert_rowid(), block.text])?;
            }
            transaction.execute(
                "INSERT INTO thread_index_runs VALUES (?1, 'indexed', 1, 1, ?2, NULL, 1, ?3, 1, ?4, ?4, ?4, ?5, ?6, ?7, 0, 0, NULL, NULL, NULL, 1, 1)",
                params![
                    input.run_id,
                    format!("{:x}", Sha256::digest(input.run_id.as_bytes())),
                    input.extractor_version,
                    retention_at_ns,
                    footprint.text as i64,
                    footprint.blocks as i64,
                    footprint.text as i64,
                ],
            )?;
        }
        transaction.commit()?;
        let after = self.file_bytes();
        let mut wal = self.path.clone().into_os_string();
        wal.push("-wal");
        self.max_wal_bytes = self
            .max_wal_bytes
            .max(std::fs::metadata(wal).map_or(0, |metadata| metadata.len()));
        if after.saturating_sub(before) > self.max_commit_growth {
            self.max_commit_growth = after.saturating_sub(before);
            self.max_commit_text = footprint.text;
        }
        self.runs += 1;
        self.footprint.text += footprint.text;
        self.footprint.attributes += footprint.attributes;
        self.footprint.blocks += footprint.blocks;
        self.seconds += started.elapsed().as_secs_f64();
        Ok(())
    }

    fn sizes(&self) -> Result<IndexSizes> {
        let stats = dbstat(&self.conn, None)?;
        let sum = |predicate: &dyn Fn(&str) -> bool| -> u64 {
            stats
                .iter()
                .filter(|(name, _)| predicate(name))
                .map(|(_, bytes)| bytes)
                .sum()
        };
        let fts = sum(&|name| name.starts_with("conversation_fts"));
        let blocks = sum(&|name| {
            name == "conversation_blocks" || name == "sqlite_autoindex_conversation_blocks_1"
        });
        let runs = sum(&|name| {
            name == "thread_index_runs"
                || name == "sqlite_autoindex_thread_index_runs_1"
                || name.starts_with("idx_thread_index_runs")
        });
        Ok(IndexSizes {
            fts,
            blocks,
            runs,
            stats,
        })
    }

    fn finish(self, coefficients: Coefficients) -> Result<Value> {
        let IndexSizes {
            fts,
            blocks,
            runs,
            stats,
        } = self.sizes()?;
        let integrity = self.conn.execute(
            "INSERT INTO conversation_fts (conversation_fts, rank) VALUES ('integrity-check', 1)",
            [],
        );
        let peak_files = self.file_bytes();
        self.conn.execute(
            "INSERT INTO conversation_fts (conversation_fts) VALUES ('optimize')",
            [],
        )?;
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        let fts_optimized = self.sizes()?.fts;
        let footprint = self.footprint;
        let text = footprint.text.max(1);
        Ok(json!({
            "schema": "ADR 0041 v25 shape: conversation_blocks + fts5(trigram, external content, columnsize=0) + thread_index_runs with work/retention indexes",
            "runs": self.runs,
            "blocks": footprint.blocks,
            "text_bytes": footprint.text,
            "attribute_bytes": footprint.attributes,
            "fts_bytes": fts,
            "fts_bytes_after_optimize": fts_optimized,
            "fts_ratio": fts as f64 / text as f64,
            "fts_ratio_after_optimize": fts_optimized as f64 / text as f64,
            "blocks_table_bytes": blocks,
            "runs_table_bytes": runs,
            "index_tables_total_bytes": fts + blocks + runs,
            "per_table": stats.iter().map(|(name, bytes)| json!({"name": name, "bytes": bytes})).collect::<Vec<_>>(),
            "db_plus_wal_bytes_before_optimize": peak_files,
            "max_wal_file_bytes": self.max_wal_bytes,
            "max_single_commit_db_plus_wal_growth": self.max_commit_growth,
            "text_bytes_of_that_commit": self.max_commit_text,
            "build_seconds": self.seconds,
            "fts_integrity_check": integrity.map_or_else(|error| error.to_string(), |_| "ok".to_owned()),
            "derived_coefficients": {
                "per_block_bytes": coefficients.per_block,
                "per_run_bytes": coefficients.per_run,
                "fts_numerator": coefficients.fts_numerator,
                "fts_denominator": FTS_DENOMINATOR,
            },
        }))
    }

    fn coefficients(&self) -> Result<Coefficients> {
        let IndexSizes {
            fts, blocks, runs, ..
        } = self.sizes()?;
        let footprint = self.footprint;
        Ok(Coefficients {
            per_block: blocks
                .saturating_sub(footprint.text + footprint.attributes)
                .div_ceil(footprint.blocks.max(1)),
            per_run: runs.div_ceil(self.runs.max(1)),
            fts_numerator: u64::try_from(
                (u128::from(fts) * u128::from(FTS_DENOMINATOR))
                    .div_ceil(u128::from(footprint.text.max(1))),
            )
            .unwrap_or(u64::MAX),
        })
    }
}

fn sampled(run_id: &str, percent: u8) -> bool {
    percent >= 100 || Sha256::digest(run_id.as_bytes())[0] as u32 * 100 / 256 < u32::from(percent)
}

/// `(run_id, retention_at_ns, Some((text_bytes, charged_bytes)) | None for a hold)`.
type RunCost = (String, i64, Option<(u64, u64)>);

struct Retention {
    retained: u64,
    retained_text: u64,
    retained_charged: u64,
    oldest_ns: Option<i64>,
    evicted: u64,
    pending: u64,
    oversized: u64,
    greedy_retained: u64,
}

/// Newest first (`retention_at_ns DESC, run_id DESC`); keeping that prefix is
/// what evicting `retention_at_ns ASC, run_id ASC` converges to. Oversized runs
/// are evicted on their own; holds are pending, not evicted.
fn retain(runs: &[RunCost], budget: u64) -> Retention {
    let mut result = Retention {
        retained: 0,
        retained_text: 0,
        retained_charged: 0,
        oldest_ns: None,
        evicted: 0,
        pending: 0,
        oversized: 0,
        greedy_retained: 0,
    };
    let mut used = 0_u64;
    let mut greedy_used = 0_u64;
    let mut stopped = false;
    for (_, retention_at_ns, cost) in runs {
        let Some((text, charged)) = *cost else {
            result.pending += 1;
            continue;
        };
        if charged > budget {
            result.oversized += 1;
            continue;
        }
        if greedy_used + charged <= budget {
            greedy_used += charged;
            result.greedy_retained += 1;
        }
        if !stopped && used + charged <= budget {
            used += charged;
            result.retained += 1;
            result.retained_text += text;
            result.retained_charged += charged;
            result.oldest_ns = Some(*retention_at_ns);
        } else {
            stopped = true;
            result.evicted += 1;
        }
    }
    result
}

fn retention_json(
    summaries: &[RunSummary],
    pick: fn(&RunSummary) -> &std::result::Result<Footprint, String>,
    coefficients: Coefficients,
    budgets: &[u64],
) -> Value {
    let mut runs: Vec<RunCost> = summaries
        .iter()
        .map(|summary| {
            let cost = pick(summary)
                .as_ref()
                .ok()
                .map(|footprint| (footprint.text, charge(*footprint, coefficients)));
            (summary.run_id.clone(), summary.retention_at_ns, cost)
        })
        .collect();
    runs.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));
    let indexable = runs.iter().filter(|run| run.2.is_some()).count();
    let total_text: u64 = runs.iter().filter_map(|run| run.2).map(|cost| cost.0).sum();
    let total_charged: u64 = runs.iter().filter_map(|run| run.2).map(|cost| cost.1).sum();
    let largest = runs
        .iter()
        .filter_map(|run| run.2.map(|cost| (cost.1, run.0.clone())))
        .max();
    json!({
        "runs_total": runs.len(),
        "runs_extractable": indexable,
        "target_text_bytes": total_text,
        "target_charged_bytes": total_charged,
        "largest_run_charged_bytes": largest.as_ref().map(|largest| largest.0),
        "largest_run_id": largest.map(|largest| largest.1),
        "budgets": budgets.iter().map(|&budget| {
            let result = retain(&runs, budget);
            json!({
                "max_bytes": budget,
                "retained": result.retained,
                "retained_text_bytes": result.retained_text,
                "retained_charged_bytes": result.retained_charged,
                "oldest_retained_at_ns": result.oldest_ns,
                "oldest_retained_utc": result.oldest_ns.map(utc),
                "evicted_by_budget": result.evicted,
                "pending": result.pending,
                "oversized": result.oversized,
                "greedy_retained_if_older_small_runs_fill_gaps": result.greedy_retained,
            })
        }).collect::<Vec<_>>(),
    })
}

fn main() -> Result<()> {
    let options = options()?;
    let started = Instant::now();
    let copy = match &options.source {
        Some(source) => backup_copy(source, &options.copy)?,
        None => json!({"method": "reused existing copy"}),
    };
    let store = open_read_only(&options.copy)?;
    let page_size = pragma(&store, "page_size")?;
    let store_json = json!({
        "copy": copy,
        "page_size": page_size,
        "page_count": pragma(&store, "page_count")?,
        "freelist_count": pragma(&store, "freelist_count")?,
        "auto_vacuum": pragma(&store, "auto_vacuum")?,
        "user_version": pragma(&store, "user_version")?,
        "file_bytes": std::fs::metadata(&options.copy)?.len(),
        "top_tables": if options.skip_store_dbstat {
            Value::Null
        } else {
            dbstat(&store, Some(12))?
                .into_iter()
                .map(|(name, bytes)| json!({"name": name, "bytes": bytes}))
                .collect()
        },
    });

    let mut latest_notes: HashMap<String, RunLedgerEntry> = HashMap::new();
    let mut first_note_ns: HashMap<String, i64> = HashMap::new();
    {
        let mut statement = store.prepare(
            "SELECT l.seq, l.run_id, l.body, l.created_at_ns, f.first_ns FROM run_ledger l JOIN (SELECT run_id, MAX(seq) AS seq, MIN(created_at_ns) AS first_ns FROM run_ledger GROUP BY run_id) f ON l.seq = f.seq",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                RunLedgerEntry {
                    seq: row.get::<_, i64>(0)? as u64,
                    run_id: row.get(1)?,
                    body: row.get(2)?,
                    created_at_ns: row.get(3)?,
                },
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (entry, first) = row?;
            first_note_ns.insert(entry.run_id.clone(), first);
            latest_notes.insert(entry.run_id.clone(), entry);
        }
    }
    let mut first_usage_ns: HashMap<String, i64> = HashMap::new();
    if table_exists(&store, "usage_requests")? {
        let mut statement = store.prepare(
            "SELECT run_id, MIN(at_ns) FROM usage_requests WHERE run_id IS NOT NULL GROUP BY run_id",
        )?;
        for row in statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))? {
            let (run_id, at): (String, i64) = row?;
            first_usage_ns.insert(run_id, at);
        }
    }
    let mut fork_prefixes: HashMap<String, (String, usize)> = HashMap::new();
    if table_exists(&store, "threads")? {
        let mut statement = store.prepare(
            "SELECT root_run_id, fork_point_run_id, fork_point_context_len FROM threads WHERE lineage_kind IN ('fork', 'rewind') AND root_run_id IS NOT NULL AND fork_point_run_id IS NOT NULL AND fork_point_context_len IS NOT NULL",
        )?;
        for row in statement.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)?))
        })? {
            let (root, source, length): (String, String, i64) = row?;
            fork_prefixes.insert(root, (source, usize::try_from(length).unwrap_or(0)));
        }
    }

    let redactor = SecretRedactor::from_env();
    let mut fts = FtsBuild::create(&options.work_dir)?;
    let mut summaries = Vec::new();
    let mut strict_codes: BTreeMap<String, u64> = BTreeMap::new();
    let mut strict_indexable = 0_u64;
    let mut legacy_reasons: BTreeMap<String, u64> = BTreeMap::new();
    let mut upper_codes: BTreeMap<String, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<String, KindStat> = BTreeMap::new();
    let mut retention_sources: BTreeMap<&str, u64> = BTreeMap::new();
    let mut extract_ns: Vec<u64> = Vec::new();
    let mut fork_runs = 0_u64;
    let mut note_only = 0_u64;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    {
        let mut process = |record: RunContextRecord,
                           note: Option<RunLedgerEntry>,
                           summaries: &mut Vec<RunSummary>,
                           fts: &mut FtsBuild|
         -> Result<()> {
            let (retention_at_ns, source) = if let Some(ms) = ulid_ms(&record.run_id) {
                (ms.saturating_mul(1_000_000), "run_id_time")
            } else if let Some(ns) = [
                first_note_ns.get(&record.run_id),
                first_usage_ns.get(&record.run_id),
            ]
            .into_iter()
            .flatten()
            .min()
            {
                (*ns, "first_note_or_usage")
            } else {
                (record.updated_at_ns, "snapshot_updated_at")
            };
            *retention_sources.entry(source).or_default() += 1;
            let mut bundle = SnapshotBundle {
                record,
                snapshot_revision: 0,
                source_revision: 0,
                provenance: None,
                latest_note: note,
            };
            match storage::thread_index::derive_legacy_provenance(&store, &bundle.record)? {
                Ok(provenance) => bundle.provenance = Some(provenance),
                Err(reason) => {
                    *legacy_reasons.entry(serde_name(&reason)).or_default() += 1;
                    let mut sidecar = assumed_provenance(&bundle.record, &json!([]), 0, None);
                    sidecar.status = ProvenanceStatus::Unknown {
                        reason: storage::thread_index::ProvenanceUnknownReason::LegacyUnproven,
                    };
                    bundle.provenance = Some(sidecar);
                }
            }
            let strict = extract_snapshot_bundle(&bundle, &redactor);
            match &strict {
                Ok(_) => strict_indexable += 1,
                Err(error) => *strict_codes.entry(error_code(*error)).or_default() += 1,
            }
            let strict_footprint = strict
                .as_ref()
                .map(footprint)
                .map_err(|error| error_code(*error));
            let parsed = parse_index_snapshot(&bundle.record.messages_json).and_then(|messages| {
                let checkpoints = parse_index_snapshot(&bundle.record.checkpoints_json)?;
                Ok((messages, checkpoints.as_array().map_or(0, Vec::len)))
            });
            let (upper, estimate) = match parsed {
                Err(error) => {
                    let code = error_code(error);
                    (Err(code.clone()), Err(code))
                }
                Ok((messages, checkpoint_count)) => {
                    bundle.provenance = Some(assumed_provenance(
                        &bundle.record,
                        &messages,
                        checkpoint_count,
                        None,
                    ));
                    let timer = Instant::now();
                    let upper = extract_snapshot_bundle(&bundle, &redactor);
                    extract_ns.push(timer.elapsed().as_nanos() as u64);
                    let upper_footprint = upper.as_ref().map(footprint).map_err(|e| error_code(*e));
                    if let Ok(input) = &upper {
                        for block in &input.blocks {
                            let stat = kinds.entry(kind_key(block)).or_default();
                            stat.blocks += 1;
                            stat.bytes += block.text.len() as u64;
                            stat.truncated += u64::from(block.truncated);
                        }
                        if sampled(&input.run_id, options.fts_percent) {
                            fts.insert(input, retention_at_ns)?;
                        }
                    }
                    let estimate = match fork_prefixes.get(&bundle.record.run_id) {
                        None => upper_footprint.clone(),
                        Some(prefix) => {
                            fork_runs += 1;
                            bundle.provenance = Some(assumed_provenance(
                                &bundle.record,
                                &messages,
                                checkpoint_count,
                                Some(prefix),
                            ));
                            extract_snapshot_bundle(&bundle, &redactor)
                                .as_ref()
                                .map(footprint)
                                .map_err(|e| error_code(*e))
                        }
                    };
                    (upper_footprint, estimate)
                }
            };
            if let Err(code) = &upper {
                *upper_codes.entry(code.clone()).or_default() += 1;
            }
            summaries.push(RunSummary {
                sampled: sampled(&bundle.record.run_id, options.fts_percent),
                run_id: bundle.record.run_id,
                retention_at_ns,
                strict: strict_footprint,
                upper,
                estimate,
            });
            Ok(())
        };

        {
            let mut statement = store.prepare(
            "SELECT run_id, role, name, parent_run_id, config_json, messages_json, checkpoints_json, terminal_phase, restorable, updated_at_ns FROM run_contexts ORDER BY run_id",
        )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let record = RunContextRecord {
                    run_id: row.get(0)?,
                    role: row.get(1)?,
                    name: row.get(2)?,
                    parent_run_id: row.get(3)?,
                    config_json: row.get(4)?,
                    messages_json: row.get(5)?,
                    checkpoints_json: row.get(6)?,
                    terminal_phase: row.get(7)?,
                    restorable: row.get(8)?,
                    updated_at_ns: row.get(9)?,
                };
                seen.insert(record.run_id.clone());
                let note = latest_notes.get(&record.run_id).cloned();
                process(record, note, &mut summaries, &mut fts)?;
            }
        }
        let mut note_only_runs: Vec<&RunLedgerEntry> = latest_notes
            .values()
            .filter(|entry| !seen.contains(&entry.run_id))
            .collect();
        note_only_runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        for entry in note_only_runs {
            note_only += 1;
            let record = RunContextRecord {
                run_id: entry.run_id.clone(),
                role: String::new(),
                name: String::new(),
                parent_run_id: None,
                config_json: "{}".to_owned(),
                messages_json: "[]".to_owned(),
                checkpoints_json: "[]".to_owned(),
                terminal_phase: String::new(),
                restorable: false,
                updated_at_ns: entry.created_at_ns,
            };
            process(record, Some(entry.clone()), &mut summaries, &mut fts)?;
        }
    }

    let coefficients = fts.coefficients()?;
    let fts_json = fts.finish(coefficients)?;
    extract_ns.sort_unstable();
    let percentile = |p: usize| {
        extract_ns
            .get((extract_ns.len().saturating_sub(1)) * p / 100)
            .copied()
    };
    let sampled_runs = summaries.iter().filter(|summary| summary.sampled).count();
    let output = json!({
        "harness": "crates/storage/examples/thread_index_measure.rs",
        "extractor_version": storage::thread_index::EXTRACTOR_VERSION,
        "accounting": "PROVISIONAL local copy of the documented IndexAccounting formula (shared IndexAccountant not implemented yet)",
        "store": store_json,
        "population": {
            "run_contexts": seen.len(),
            "note_only_runs": note_only,
            "total_runs": summaries.len(),
            "retention_time_source": retention_sources,
            "fork_or_rewind_root_runs_with_prefix": fork_runs,
        },
        "strict": {
            "rule": "verified structured generation metadata only; unknown is never assumed native",
            "legacy_held_by_detail": legacy_reasons,
            "indexable_runs": strict_indexable,
            "held_by_reason": strict_codes,
        },
        "upper_bound_kinds": kinds.iter().map(|(kind, stat)| (kind.clone(), json!({
            "blocks": stat.blocks, "bytes": stat.bytes, "truncated": stat.truncated,
        }))).collect::<BTreeMap<_, _>>(),
        "upper_bound_holds": upper_codes,
        "extract_seconds_per_run": {
            "median": percentile(50).map(|ns| ns as f64 / 1e9),
            "p95": percentile(95).map(|ns| ns as f64 / 1e9),
            "max": extract_ns.last().map(|ns| *ns as f64 / 1e9),
        },
        "fts_sample": {
            "percent": options.fts_percent,
            "sampled_runs": sampled_runs,
            "measurement": fts_json,
        },
        "proposed_policy": {
            "per_block_bytes": coefficients.per_block,
            "per_run_bytes": coefficients.per_run,
            "fts_numerator": coefficients.fts_numerator,
            "fts_denominator": FTS_DENOMINATOR,
            "matches_default_policy": policy(coefficients) == DEFAULT_ACCOUNTING_POLICY,
        },
        "retention": {
            "order": "retention_at_ns DESC, run_id DESC (newest kept)",
            "strict": retention_json(&summaries, |summary| &summary.strict, coefficients, &options.budgets),
            "upper_bound": retention_json(&summaries, |summary| &summary.upper, coefficients, &options.budgets),
            "fork_prefix_excluded": retention_json(&summaries, |summary| &summary.estimate, coefficients, &options.budgets),
        },
        "elapsed_seconds": started.elapsed().as_secs_f64(),
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
