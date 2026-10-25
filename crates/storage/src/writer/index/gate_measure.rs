//! ADR 0041 順序 5 start-gate measurement on a COPY of a real store.
//!
//! Ignored by default. Run in release mode:
//! `EVORCH_INDEX_GATE_SOURCE=<copy.db> EVORCH_INDEX_GATE_WORK=<new dir> cargo test
//! --release -p storage --lib gate_measure -- --ignored --nocapture`.
//! The source is copied first; only the copy is opened (and migrated).
//! Body text is never printed: only sizes, counts and durations.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::db::{FileSizes, file_sizes};
use crate::repo::thread_index::{SqliteSnapshotStore, SqliteThreadIndex};
use crate::thread_index::{
    BlockOrigin, BlockProvenance, DEFAULT_ACCOUNTING_POLICY, IndexCommitGuard, IndexCommitResult,
    IndexEvictionReason, IndexInput, IndexLease, IndexOwner, IndexOwnerKind, IndexState,
    IndexTarget, MessageProvenance, PROVENANCE_FORMAT_VERSION, ProvenanceEvidence, ProvenanceMap,
    ProvenanceStatus, SnapshotBundle, SnapshotProvenance, SnapshotRepository as _,
    ThreadIndexRepository as _, account_index_input, extract_index_input, parse_index_snapshot,
    snapshot_sha256,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn debug(error: impl std::fmt::Debug) -> Box<dyn std::error::Error> {
    format!("{error:?}").into()
}

fn checkpoint_truncate(conn: &Connection) -> Result<(i64, i64, i64)> {
    Ok(
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?,
    )
}

fn freelist_bytes(conn: &Connection, page: u64) -> Result<u64> {
    let pages: i64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    Ok(u64::try_from(pages)? * page)
}

/// Clean WAL before, WAL frames after the write, DB growth after checkpointing
/// those frames. `worst` is what admission must bound: the WAL file growing by
/// every frame AND the main file growing by the checkpointed pages.
fn measured_write(
    label: &str,
    conn: &Connection,
    path: &Path,
    write: impl FnOnce(&Connection) -> Result<()>,
) -> Result<Value> {
    checkpoint_truncate(conn)?;
    let before = file_sizes(path)?;
    let started = Instant::now();
    write(conn).map_err(|error| format!("{label}: {error}"))?;
    let micros = started.elapsed().as_micros();
    let after_write = file_sizes(path)?;
    let busy = checkpoint_truncate(conn)?;
    let after_checkpoint = file_sizes(path)?;
    Ok(sample(
        before,
        after_write,
        after_checkpoint,
        micros,
        busy.0,
    ))
}

fn sample(
    before: FileSizes,
    after: FileSizes,
    settled: FileSizes,
    micros: u128,
    busy: i64,
) -> Value {
    let db_growth = settled.db.saturating_sub(before.db);
    json!({
        "wal_bytes": after.wal,
        "shm_bytes": after.shm,
        "db_growth": db_growth,
        "total_growth_while_wal_live": after.total().saturating_sub(before.total()),
        "worst": after.wal + db_growth + after.shm,
        "micros": micros,
        "checkpoint_busy": busy,
    })
}

fn native_provenance(
    bundle: &SnapshotBundle,
    messages: &Value,
    checkpoints: usize,
) -> SnapshotProvenance {
    let raw = messages.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut mapped = Vec::new();
    for (raw_index, message) in raw.iter().enumerate() {
        if message["role"] == "system" {
            continue;
        }
        let blocks = (0..message["content"].as_array().map_or(0, Vec::len))
            .map(|block_index| BlockProvenance {
                block_index,
                origin: BlockOrigin::Native,
                excluded_reason: None,
            })
            .collect();
        mapped.push(MessageProvenance {
            raw_index,
            ordinal: mapped.len(),
            blocks,
        });
    }
    SnapshotProvenance {
        run_id: bundle.record.run_id.clone(),
        snapshot_revision: bundle.snapshot_revision,
        snapshot_sha256: snapshot_sha256(
            &bundle.record.messages_json,
            &bundle.record.checkpoints_json,
        ),
        format_version: PROVENANCE_FORMAT_VERSION,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: raw.len(),
            checkpoint_count: checkpoints,
            messages: mapped,
        },
    }
}

fn percentile(sorted: &[u128], pct: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) * pct).div_ceil(100)]
}

fn foreground(conn: &Connection, path: &Path) -> Result<Value> {
    let store = SqliteSnapshotStore::new(conn);
    let (largest, json_bytes): (String, i64) = conn.query_row(
        "SELECT run_id, length(messages_json) + length(checkpoints_json) FROM run_contexts \
         ORDER BY 2 DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let bundle = store
        .load_snapshot_bundle(&largest)
        .map_err(debug)?
        .ok_or("largest snapshot vanished")?;
    let mut inserted = bundle.record.clone();
    inserted.run_id = "gate-foreground-insert".into();
    let insert = measured_write("snapshot insert", conn, path, |conn| {
        let tx = conn.unchecked_transaction()?;
        crate::repo::thread_index::stage_legacy_snapshot(&tx, &inserted).map_err(debug)?;
        tx.commit()?;
        Ok(())
    })?;
    let mut updated = bundle.record.clone();
    updated.updated_at_ns += 1;
    let update = measured_write("snapshot update", conn, path, |conn| {
        let tx = conn.unchecked_transaction()?;
        crate::repo::thread_index::stage_legacy_snapshot(&tx, &updated).map_err(debug)?;
        tx.commit()?;
        Ok(())
    })?;
    let (note_run, body): (String, String) = conn.query_row(
        "SELECT run_id, body FROM run_ledger ORDER BY length(body) DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let note = measured_write("note append", conn, path, |conn| {
        crate::repo::thread_index::append_note(conn, &note_run, &body, i64::MAX / 2)?;
        Ok(())
    })?;
    let max_event: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(length(payload)), 0) FROM events",
            [],
            |row| row.get(0),
        )
        .unwrap_or(-1);
    Ok(json!({
        "largest_snapshot_json_bytes": json_bytes,
        "largest_note_bytes": body.len(),
        "largest_stored_event_payload_bytes": max_event,
        "snapshot_insert": insert,
        "snapshot_update": update,
        "note_append": note,
    }))
}

struct Row {
    text: u64,
    blocks: u64,
    charged: u64,
    frames: u64,
    db_growth: u64,
    worst: u64,
    micros: u128,
}

fn pending_targets(conn: &Connection) -> Result<Vec<(IndexTarget, u64)>> {
    let mut statement = conn.prepare(
        "SELECT run_id, source_revision, extractor_version, accounting_version, work_generation \
         FROM thread_index_runs WHERE state = 'pending' \
         ORDER BY retention_at_ns DESC, run_id DESC",
    )?;
    let targets = statement
        .query_map([], |row| {
            Ok((
                IndexTarget {
                    run_id: row.get(0)?,
                    source_revision: row.get(1)?,
                    extractor_version: row.get(2)?,
                    accounting_version: row.get(3)?,
                },
                row.get::<_, u64>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(targets)
}

fn index_commits(conn: &Connection, path: &Path, page: u64) -> Result<(Value, Vec<Row>)> {
    conn.execute_batch(
        "UPDATE thread_index_state SET accounting_version = 1;\
         UPDATE thread_index_lease SET owner_id = 'gate', owner_kind = 'gui', epoch = 1, \
         expires_at_ns = 9223372036854775807;",
    )?;
    let merge_pages: Option<i64> = std::env::var("EVORCH_INDEX_GATE_MERGE_PAGES")
        .ok()
        .map(|pages| pages.parse())
        .transpose()?;
    if merge_pages.is_some() {
        conn.execute_batch(
            "INSERT INTO conversation_fts(conversation_fts, rank) VALUES('automerge', 0);\
             INSERT INTO conversation_fts(conversation_fts, rank) VALUES('crisismerge', 64);",
        )?;
    }
    let mut merges: Vec<(u64, u128)> = Vec::new();
    let mut index = SqliteThreadIndex::new(conn);
    while !index.discover_batch(500, 1)?.complete {}
    let lease = IndexLease {
        owner: IndexOwner {
            id: "gate".into(),
            kind: IndexOwnerKind::Gui,
        },
        epoch: 1,
        expires_at_ns: i64::MAX,
    };
    let redactor = secret_guard::SecretRedactor::from_env();
    let store = SqliteSnapshotStore::new(conn);
    let mut rows = Vec::new();
    let mut skipped = 0_u64;
    let mut mismatched = 0_u64;
    let mut busy_checkpoints = 0_u64;
    let started = Instant::now();
    for (target, work_generation) in pending_targets(conn)? {
        let Some(bundle) = store.load_snapshot_bundle(&target.run_id).map_err(debug)? else {
            skipped += 1;
            continue;
        };
        let parsed = parse_index_snapshot(&bundle.record.messages_json).and_then(|messages| {
            let checkpoints = parse_index_snapshot(&bundle.record.checkpoints_json)?;
            Ok((messages, checkpoints.as_array().map_or(0, Vec::len)))
        });
        let Ok((messages, checkpoints)) = parsed else {
            skipped += 1;
            continue;
        };
        let provenance = native_provenance(&bundle, &messages, checkpoints);
        let sha = snapshot_sha256(
            &bundle.record.messages_json,
            &bundle.record.checkpoints_json,
        );
        let Ok(input) = extract_index_input(
            IndexInput {
                run_id: &target.run_id,
                snapshot_revision: bundle.snapshot_revision,
                source_revision: target.source_revision,
                snapshot_sha256: &sha,
                messages: &messages,
                checkpoint_count: checkpoints,
                provenance: Some(&provenance),
                latest_note: bundle.latest_note.as_ref(),
            },
            &redactor,
        ) else {
            skipped += 1;
            continue;
        };
        let accounting = account_index_input(&input, DEFAULT_ACCOUNTING_POLICY).map_err(debug)?;
        let summed: u64 = input
            .blocks
            .iter()
            .map(|block| block.text.len() as u64)
            .sum();
        let mut keys = std::collections::HashSet::new();
        let unique = input
            .blocks
            .iter()
            .all(|block| keys.insert(format!("{:?}", block.locator)));
        if summed != input.text_bytes
            || !unique
            || input.source_revision != target.source_revision
            || input.extractor_version != target.extractor_version
            || accounting.accounting_version != target.accounting_version
        {
            if mismatched == 0 {
                eprintln!(
                    "first mismatch: summed={summed} text={} unique={unique} rev={}/{} ext={}/{} acct={}/{}",
                    input.text_bytes,
                    input.source_revision,
                    target.source_revision,
                    input.extractor_version,
                    target.extractor_version,
                    accounting.accounting_version,
                    target.accounting_version
                );
            }
            mismatched += 1;
            continue;
        }
        let guard = IndexCommitGuard {
            lease: lease.clone(),
            target: target.clone(),
            work_generation,
            expected_state: IndexState::Pending,
            source_present: true,
            request: None,
        };
        checkpoint_truncate(conn)?;
        let before = file_sizes(path)?;
        let clock = Instant::now();
        let result = index.commit_index_input(&guard, &input, accounting, 2)?;
        let micros = clock.elapsed().as_micros();
        assert_eq!(result, IndexCommitResult::Committed, "{}", target.run_id);
        let after = file_sizes(path)?;
        let busy = checkpoint_truncate(conn)?;
        busy_checkpoints += u64::from(busy.0 != 0);
        let settled = file_sizes(path)?;
        let measured = sample(before, after, settled, micros, busy.0);
        rows.push(Row {
            text: input.text_bytes,
            blocks: accounting.block_count,
            charged: accounting.charged_bytes,
            frames: after.wal.saturating_sub(32) / (page + 24),
            db_growth: measured["db_growth"].as_u64().unwrap_or(0),
            worst: measured["worst"].as_u64().unwrap_or(0),
            micros,
        });
        if let Some(pages) = merge_pages {
            loop {
                checkpoint_truncate(conn)?;
                let total = |conn: &Connection| -> Result<i64> {
                    Ok(conn.query_row("SELECT total_changes()", [], |row| row.get(0))?)
                };
                let before_changes = total(conn)?;
                let clock = Instant::now();
                conn.execute(
                    "INSERT INTO conversation_fts(conversation_fts, rank) VALUES('merge', ?1)",
                    [pages],
                )?;
                let micros = clock.elapsed().as_micros();
                let frames = file_sizes(path)?.wal.saturating_sub(32) / (page + 24);
                merges.push((frames, micros));
                if total(conn)? - before_changes < 2 {
                    break;
                }
            }
        }
    }
    let mut merge_micros: Vec<u128> = merges.iter().map(|merge| merge.1).collect();
    merge_micros.sort_unstable();
    let fts_bytes: i64 = conn.query_row(
        "SELECT COALESCE(SUM(pgsize), 0) FROM dbstat WHERE name LIKE 'conversation_fts%'",
        [],
        |row| row.get(0),
    )?;
    let merge_json = json!({
        "pages": merge_pages,
        "steps": merges.len(),
        "max_frames": merges.iter().map(|merge| merge.0).max().unwrap_or(0),
        "micros_p50": percentile(&merge_micros, 50),
        "micros_p95": percentile(&merge_micros, 95),
        "micros_max": merge_micros.last().copied().unwrap_or(0),
        "fts_bytes_after": fts_bytes,
    });
    Ok((
        json!({
            "committed": rows.len(),
            "skipped": skipped,
            "mismatched": mismatched,
            "busy_checkpoints": busy_checkpoints,
            "seconds": started.elapsed().as_secs_f64(),
            "merge": merge_json,
        }),
        rows,
    ))
}

fn growth_summary(rows: &[Row], page: u64) -> Value {
    let frame = page + 24;
    let max_by = |key: fn(&Row) -> u64| rows.iter().map(key).max().unwrap_or(0);
    let mut fits = Vec::new();
    for constant_pages in [0_u64, 16, 64, 256, 1024] {
        // frames <= ceil(per_mille * charged / (1000 * page)) + constant_pages
        let per_mille = rows
            .iter()
            .map(|row| {
                (row.frames.saturating_sub(constant_pages) * page * 1000)
                    .div_ceil(row.charged.max(1))
            })
            .max()
            .unwrap_or(0);
        fits.push(
            json!({ "constant_pages": constant_pages, "per_mille_pages_of_charged": per_mille }),
        );
    }
    let worst_ratio = rows
        .iter()
        .map(|row| row.worst as f64 / row.charged.max(1) as f64)
        .fold(0.0, f64::max);
    let brief = |row: &&Row| {
        json!({ "text": row.text, "blocks": row.blocks, "charged": row.charged,
                "frames": row.frames, "wal_bytes": row.frames * frame, "db_growth": row.db_growth,
                "worst": row.worst, "micros": row.micros })
    };
    let mut by_text: Vec<&Row> = rows.iter().collect();
    by_text.sort_by_key(|row| std::cmp::Reverse(row.text));
    let mut by_frames: Vec<&Row> = rows.iter().collect();
    by_frames.sort_by_key(|row| std::cmp::Reverse(row.frames));
    let mut by_micros: Vec<&Row> = rows.iter().collect();
    by_micros.sort_by_key(|row| std::cmp::Reverse(row.micros));
    let mut micros: Vec<u128> = rows.iter().map(|row| row.micros).collect();
    micros.sort_unstable();
    let thresholds = [256_u64 << 10, 512 << 10, 1 << 20, 2 << 20, 4 << 20];
    let over_text: Vec<Value> = thresholds
        .iter()
        .map(|limit| json!({ "text_over": limit, "runs": rows.iter().filter(|row| row.text > *limit).count() }))
        .collect();
    let over_blocks: Vec<Value> = [1_000_u64, 2_000, 5_000, 10_000]
        .iter()
        .map(|limit| json!({ "blocks_over": limit, "runs": rows.iter().filter(|row| row.blocks > *limit).count() }))
        .collect();
    json!({
        "max_frames": max_by(|row| row.frames),
        "max_wal_bytes": max_by(|row| row.frames) * frame,
        "max_db_growth": max_by(|row| row.db_growth),
        "max_worst": max_by(|row| row.worst),
        "max_worst_over_charged": worst_ratio,
        "fits": fits,
        "largest_text": by_text.iter().take(8).map(brief).collect::<Vec<_>>(),
        "largest_frames": by_frames.iter().take(8).map(brief).collect::<Vec<_>>(),
        "slowest": by_micros.iter().take(8).map(brief).collect::<Vec<_>>(),
        "commit_micros": { "p50": percentile(&micros, 50), "p95": percentile(&micros, 95),
                           "p99": percentile(&micros, 99), "max": micros.last().copied().unwrap_or(0) },
        "runs_over_text": over_text,
        "runs_over_blocks": over_blocks,
    })
}

fn evictions(conn: &Connection, path: &Path, page: u64) -> Result<Value> {
    let mut statement = conn.prepare(
        "SELECT run_id, charged_bytes FROM thread_index_runs WHERE state = 'indexed' \
         ORDER BY charged_bytes DESC LIMIT 40",
    )?;
    let runs = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    let mut index = SqliteThreadIndex::new(conn);
    let mut worst_ratio = 0.0_f64;
    let mut max_frames = 0_u64;
    let mut micros = Vec::new();
    for (run_id, charged) in runs {
        checkpoint_truncate(conn)?;
        let clock = Instant::now();
        index.evict_index(&run_id, IndexEvictionReason::PhysicalReclaim)?;
        micros.push(clock.elapsed().as_micros());
        let frames = file_sizes(path)?.wal.saturating_sub(32) / (page + 24);
        max_frames = max_frames.max(frames);
        worst_ratio = worst_ratio.max((frames * (page + 24)) as f64 / charged.max(1) as f64);
    }
    micros.sort_unstable();
    Ok(json!({
        "evicted": micros.len(),
        "max_frames": max_frames,
        "max_wal_over_charged": worst_ratio,
        "micros_max": micros.last().copied().unwrap_or(0),
        "freelist_after": freelist_bytes(conn, page)?,
    }))
}

#[test]
#[ignore = "measures a copy of a real store; see module docs"]
fn gate_measure() -> Result<()> {
    let source = PathBuf::from(std::env::var("EVORCH_INDEX_GATE_SOURCE")?);
    let work = PathBuf::from(std::env::var("EVORCH_INDEX_GATE_WORK")?);
    std::fs::create_dir_all(&work)?;
    let db_path = work.join("gate.db");
    assert!(!db_path.exists(), "work dir must be fresh");
    std::fs::copy(&source, &db_path)?;
    let config = crate::StorageConfig {
        db_path: db_path.clone(),
        ..crate::StorageConfig::default()
    };
    let migrate = Instant::now();
    let database = crate::db::Database::open(&config)?;
    let conn = &database.conn;
    let migrate_seconds = migrate.elapsed().as_secs_f64();
    let page: u64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let auto_vacuum: i64 = conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
    conn.execute_batch("UPDATE thread_index_state SET accounting_version = 1")?;
    let foreground = foreground(conn, &db_path)?;
    let (commits, rows) = index_commits(conn, &db_path, page)?;
    let growth = growth_summary(&rows, page);
    let evictions = evictions(conn, &db_path, page)?;
    let total_charged: u64 = rows.iter().map(|row| row.charged).sum();
    let summary = json!({
        "page_size": page,
        "auto_vacuum": auto_vacuum,
        "migrate_seconds": migrate_seconds,
        "db_bytes_after": file_sizes(&db_path)?.db,
        "total_charged": total_charged,
        "foreground": foreground,
        "commits": commits,
        "growth": growth,
        "evictions": evictions,
    });
    let rendered = serde_json::to_string_pretty(&summary)?;
    std::fs::write(work.join("gate.json"), &rendered)?;
    let rows_json: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!([
                row.text,
                row.blocks,
                row.charged,
                row.frames,
                row.db_growth,
                row.worst,
                row.micros
            ])
        })
        .collect();
    std::fs::write(
        work.join("gate-rows.json"),
        serde_json::to_string(&rows_json)?,
    )?;
    println!("{rendered}");
    Ok(())
}
