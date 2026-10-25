//! Supplementary read-only attribution. Never an S8 acceptance measurement.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use storage::{Database, thread_search::MAX_SEARCH_BATCH_RUNS};

use super::{Result, corpus::Corpus};

pub fn reads(db: &Database, path: &Path, corpus: &Corpus) -> Result<Value> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut totals = BTreeMap::<&str, u128>::new();
    let mut threads = Vec::new();
    for pass in 0..3 {
        for (position, thread) in corpus.threads.iter().enumerate() {
            let started = Instant::now();
            let runs = db.runs_of_thread(&thread.thread_id)?;
            let members = started.elapsed().as_micros();
            let ids = runs
                .iter()
                .map(|run| run.run_id.clone())
                .collect::<Vec<_>>();
            let started = Instant::now();
            let summaries = db.terminal_summaries(&ids)?;
            let summary = started.elapsed().as_micros();
            let started = Instant::now();
            let notes = db.latest_ledgers(&ids)?;
            let ledger = started.elapsed().as_micros();
            let started = Instant::now();
            let evidence = db.review_evidence_for_lineage(&ids)?;
            let evidence_us = started.elapsed().as_micros();
            let started = Instant::now();
            let mut coverage = 0;
            for batch in ids.chunks(MAX_SEARCH_BATCH_RUNS) {
                coverage += db.thread_index_coverage(batch)?.len();
            }
            let coverage_us = started.elapsed().as_micros();
            for (key, value) in [
                ("members", members),
                ("terminal_summaries", summary),
                ("notes", ledger),
                ("evidence", evidence_us),
                ("coverage", coverage_us),
            ] {
                *totals.entry(key).or_default() += value;
            }
            let bytes: i64 = conn.query_row(
                "SELECT coalesce(sum(length(messages_json)),0) FROM run_contexts \
                 WHERE run_id IN(SELECT value FROM json_each(?1))",
                [serde_json::to_string(&ids)?],
                |row| row.get(0),
            )?;
            threads.push(json!({"pass":pass,"position":position,"runs":ids.len(),
                "summaries":summaries.len(),"notes":notes.len(),"evidence":evidence.len(),
                "coverage":coverage,"source_bytes":bytes,"members_us":members,
                "terminal_summaries_us":summary,"notes_us":ledger,"evidence_us":evidence_us,
                "coverage_us":coverage_us}));
        }
    }
    Ok(
        json!({"stage":"diagnostic_reads","not_acceptance_measurement":true,
        "same_connection_passes":3,"thread_samples":threads,"total_us":totals}),
    )
}
