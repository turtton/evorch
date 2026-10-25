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

pub fn candidates(
    db: &Database,
    corpus: &Corpus,
    case: &super::corpus::Case,
    actual: &super::driver::Sample,
) -> Result<Value> {
    use storage::thread_index::IndexBlockKind;
    use storage::thread_search::{ConversationFilter, ConversationQuery, term_route};
    let query = case.args()["query"]
        .as_str()
        .ok_or("diagnostic_query_missing")?
        .to_owned();
    let route = match term_route(&query) {
        storage::thread_search::TermRoute::Trigram(_) => "trigram",
        storage::thread_search::TermRoute::Like(_) => "like",
        storage::thread_search::TermRoute::Scan => "scan",
    };
    let redactor = secret_guard::SecretRedactor::from_env();
    let regex = regex::RegexBuilder::new(&regex::escape(&query))
        .case_insensitive(true)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()?;
    let mut scans = Vec::new();
    let mut calls = Vec::new();
    for (label, filter) in [
        ("literal", case.literal_filter()),
        ("full_scan", ConversationFilter::All),
    ] {
        let mut seen = std::collections::BTreeSet::new();
        let mut cursor = 0;
        for (page, returned) in actual.page_threads.iter().enumerate() {
            let end = (cursor + 100).min(corpus.threads.len());
            let mut slots = Vec::new();
            let started = Instant::now();
            for thread in &corpus.threads[cursor..end] {
                let runs = db.runs_of_thread(&thread.thread_id)?;
                let ids = runs
                    .iter()
                    .map(|run| run.run_id.clone())
                    .collect::<Vec<_>>();
                let summaries = db.terminal_summaries(&ids)?;
                let primary = runs
                    .iter()
                    .filter(|run| run.depth == 0)
                    .filter_map(|run| summaries.iter().find(|s| s.run_id == run.run_id))
                    .max_by_key(|s| (s.updated_at_ns, &s.run_id))
                    .map(|s| s.run_id.as_str())
                    .or(thread.root_run_id.as_deref());
                let mut selected = runs
                    .iter()
                    .filter(|run| run.depth > 0 || Some(run.run_id.as_str()) == primary)
                    .filter(|run| summaries.iter().any(|s| s.run_id == run.run_id))
                    .map(|run| run.run_id.clone())
                    .collect::<Vec<_>>();
                selected.sort();
                selected.dedup();
                slots.extend(selected);
            }
            let members_us = started.elapsed().as_micros();
            let (
                mut sql_us,
                mut redact_us,
                mut matcher_us,
                mut rows,
                mut bytes,
                mut hits,
                mut duplicate,
            ) = (0, 0, 0, 0, 0, 0, 0);
            let mut statements = 0;
            for (batch_number, batch) in slots.chunks(MAX_SEARCH_BATCH_RUNS).enumerate() {
                let mut after = None;
                loop {
                    let start = Instant::now();
                    let candidates = db.conversation_candidates(&ConversationQuery {
                        runs: batch,
                        kinds: &[IndexBlockKind::Text],
                        filter: &filter,
                        after,
                        limit: 200,
                    })?;
                    let elapsed = start.elapsed().as_micros();
                    sql_us += elapsed;
                    statements += 1;
                    let count = candidates.len();
                    let call_bytes = candidates.iter().map(|c| c.text.len()).sum::<usize>();
                    calls.push(json!({"label":label,"actual_route":route,"page":page,
                        "thread_start":cursor,"thread_end":end,"batch":batch_number,
                        "scope_sha256":super::corpus::hash(&serde_json::to_vec(batch)?),
                        "after":after.map(|p:storage::thread_search::CandidatePosition|(p.seq,p.block_id)),
                        "rows":count,"bytes":call_bytes,"sql_us":elapsed}));
                    for candidate in &candidates {
                        rows += 1;
                        bytes += candidate.text.len();
                        let identity = super::corpus::hash(
                            format!("{}:{}", candidate.run_id, candidate.position.block_id)
                                .as_bytes(),
                        );
                        duplicate += usize::from(!seen.insert(identity));
                        let start = Instant::now();
                        let text = redactor.redact(&candidate.text).text;
                        redact_us += start.elapsed().as_micros();
                        let start = Instant::now();
                        hits += usize::from(regex.find(&text).is_some());
                        matcher_us += start.elapsed().as_micros();
                    }
                    if candidates.len() < 200 {
                        break;
                    }
                    after = candidates.last().map(|candidate| candidate.position);
                }
            }
            scans.push(json!({"label":label,"actual_route":route,"page":page,
                "threads_preloaded":end-cursor,"slots":slots.len(),"members_summary_us":members_us,
                "statements":statements,"rows":rows,"bytes":bytes,"hits":hits,
                "duplicate_candidates_prior_pages":duplicate,"sql_us":sql_us,
                "redact_us":redact_us,"matcher_us":matcher_us}));
            if let Some(last) = returned.last() {
                cursor = corpus
                    .threads
                    .iter()
                    .position(|thread| &thread.thread_id == last)
                    .ok_or("diagnostic_page_thread_missing")?
                    + 1;
            }
        }
    }
    Ok(
        json!({"stage":"diagnostic_candidates","not_acceptance_measurement":true,
        "query_sha256":case.hash(),"actual_route":route,"samples":scans,"calls":calls,
        "limits":"public API replay of each actual cursor 100-thread preload window, not internal call trace; no hit-limit/byte-fit early stop, counts are overfetch upper bounds; production matcher private, same RegexBuilder flags/limits reproduced, not reused"}),
    )
}
