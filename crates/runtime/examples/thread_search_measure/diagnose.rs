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
    path: &Path,
    corpus: &Corpus,
    case: &super::corpus::Case,
    actual: &super::driver::Sample,
) -> Result<Value> {
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
    let filter = case.literal_filter();
    let term_routes = if let ConversationFilter::Literals(alternatives) = &filter {
        alternatives
            .iter()
            .flatten()
            .map(|term| match term_route(term) {
                storage::thread_search::TermRoute::Trigram(_) => "trigram",
                storage::thread_search::TermRoute::Like(_) => "like",
                storage::thread_search::TermRoute::Scan => "scan",
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let redactor = secret_guard::SecretRedactor::from_env();
    let patterns = case.diagnostic_patterns()?;
    let kinds = case.diagnostic_kinds();
    let mut scans = Vec::new();
    let mut calls = Vec::new();
    let mut plan = None;
    for (label, filter) in [("literal", filter), ("full_scan", ConversationFilter::All)] {
        let mut seen = std::collections::BTreeSet::new();
        for (page, token) in actual.page_cursors.iter().enumerate() {
            let position = token.as_deref().map(observed_position).transpose()?;
            let positioned = position
                .as_ref()
                .map(|position| {
                    let stored = db
                        .thread_cursor_from_token(&position.thread)?
                        .ok_or("diagnostic_cursor_thread_missing")?;
                    let at = corpus
                        .threads
                        .iter()
                        .position(|thread| thread.thread_id == stored.thread_id)
                        .ok_or("diagnostic_cursor_scope_missing")?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>((at, &position.resume))
                })
                .transpose()?;
            let cursor = positioned.as_ref().map_or(0, |(at, _)| at + 1);
            let resume = positioned.as_ref().and_then(|(_, resume)| resume.as_ref());
            let end = (cursor + 100).min(corpus.threads.len());
            let mut selected_threads = Vec::new();
            if let Some((at, Some(_))) = positioned {
                selected_threads.push(&corpus.threads[at]);
            }
            selected_threads.extend(&corpus.threads[cursor..end]);
            let mut slots = Vec::new();
            let started = Instant::now();
            for (thread_number, thread) in selected_threads.iter().enumerate() {
                let runs = db.runs_of_thread(&thread.thread_id)?;
                let ids = runs
                    .iter()
                    .map(|run| run.run_id.clone())
                    .collect::<Vec<_>>();
                let summaries = db.run_context_headers(&ids)?;
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
                if thread_number == 0
                    && let Some(resume) = resume
                {
                    selected.retain(|run| run >= &resume.run);
                }
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
                if label == "literal" && plan.is_none() {
                    plan = Some(explain(path, batch, &filter)?);
                }
                let mut after = resume.filter(|_| batch_number == 0).map(|resume| {
                    storage::thread_search::CandidatePosition {
                        seq: 0,
                        block_id: resume.from_block.saturating_sub(1),
                    }
                });
                loop {
                    let start = Instant::now();
                    let candidates = db.conversation_candidates(&ConversationQuery {
                        runs: batch,
                        kinds: &kinds,
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
                        hits += usize::from(
                            patterns.iter().all(|pattern| pattern.find(&text).is_some()),
                        );
                        matcher_us += start.elapsed().as_micros();
                    }
                    if candidates.len() < 200 {
                        break;
                    }
                    after = candidates.last().map(|candidate| candidate.position);
                }
            }
            scans.push(json!({"label":label,"actual_route":route,"page":page,
                "threads_preloaded":selected_threads.len(),"resumed_thread":resume.is_some(),"slots":slots.len(),"members_headers_us":members_us,
                "statements":statements,"rows":rows,"bytes":bytes,"hits":hits,
                "duplicate_candidates_prior_pages":duplicate,"sql_us":sql_us,
                "redact_us":redact_us,"matcher_us":matcher_us}));
        }
    }
    Ok(
        json!({"stage":"diagnostic_candidates","not_acceptance_measurement":true,
        "class":case.class,"query_sha256":case.hash(),"actual_route":route,
        "term_count":term_routes.len(),"term_routes":term_routes,"plan":plan,"samples":scans,"calls":calls,
        "limits":"public API replay from observed actual cursor payload (signature not checked here; production validated it), same 100-thread preload plus optional resumed thread, not internal call trace; no hit-limit/byte-fit early stop, candidate counts are overfetch upper bounds, not latency bounds; production matcher private, same RegexBuilder flags/limits reproduced, not reused"}),
    )
}

#[derive(serde::Deserialize)]
struct ObservedPosition {
    thread: String,
    resume: Option<ObservedResume>,
}

#[derive(serde::Deserialize)]
struct ObservedResume {
    run: String,
    from_block: i64,
}

fn observed_position(token: &str) -> Result<ObservedPosition> {
    use base64::Engine;
    let (payload, _) = token
        .strip_prefix("ts2.")
        .and_then(|token| token.split_once('.'))
        .ok_or("diagnostic_cursor_format")?;
    // This only inspects a token already emitted by our actual runtime call.
    // It grants no authority and deliberately cannot validate the process MAC.
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&payload)?;
    super::require(value["v"] == 2, "diagnostic_cursor_version")?;
    Ok(serde_json::from_value(value)?)
}

/// Plan details contain SQL identifiers, never bound query/source text.
fn explain(
    path: &Path,
    runs: &[String],
    filter: &storage::thread_search::ConversationFilter,
) -> Result<Value> {
    use rusqlite::{params_from_iter, types::Value as SqlValue};
    use storage::thread_search::{ConversationFilter, TermRoute, term_route};
    let ConversationFilter::Literals(alternatives) = filter else {
        return Ok(json!({"available":false,"reason":"nonliteral_route"}));
    };
    let [terms] = alternatives.as_slice() else {
        return Ok(json!({"available":false,"reason":"multiple_alternatives"}));
    };
    let mut params = Vec::<SqlValue>::new();
    let scope = runs
        .iter()
        .enumerate()
        .map(|(seq, run)| {
            params.extend([SqlValue::Text(run.clone()), SqlValue::Integer(seq as i64)]);
            "(?, ?)"
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut phrases = Vec::new();
    for term in terms {
        let TermRoute::Trigram(variants) = term_route(term) else {
            return Ok(json!({"available":false,"reason":"nontrigram_term"}));
        };
        phrases.push(format!(
            "({})",
            variants
                .iter()
                .map(|variant| format!("\"{}\"", variant.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" OR ")
        ));
    }
    params.extend([
        SqlValue::Text("text".into()),
        SqlValue::Integer(-1),
        SqlValue::Integer(0),
        SqlValue::Text(phrases.join(" AND ")),
        SqlValue::Integer(200),
    ]);
    let sql = format!(
        "EXPLAIN QUERY PLAN WITH scope(run_id, seq) AS (VALUES {scope}), \
         current_runs AS MATERIALIZED (SELECT scope.seq,r.run_id,r.source_revision,r.snapshot_revision \
         FROM scope CROSS JOIN thread_index_runs r ON r.run_id=scope.run_id \
         WHERE r.state='indexed' AND r.source_present=1 AND r.indexed_revision=r.source_revision) \
         SELECT b.id,r.seq,b.run_id,r.source_revision,r.snapshot_revision,b.source_kind,b.ordinal,\
         b.block_index,b.note_seq,b.role,b.kind,b.tool_name,b.truncated,b.text FROM current_runs r \
         CROSS JOIN conversation_blocks b INDEXED BY idx_conversation_blocks_kind ON b.run_id=r.run_id \
         WHERE b.kind IN (?) AND (r.seq,b.id)>(?,?) AND (EXISTS (SELECT 1 FROM conversation_fts \
         WHERE rowid=b.id AND conversation_fts MATCH ?)) ORDER BY r.seq,b.id LIMIT ?"
    );
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = conn.prepare(&sql)?;
    let details = statement
        .query_map(params_from_iter(&params), |row| row.get::<_, String>(3))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(
        json!({"available":true,"scope_runs":runs.len(),"parameters":params.len(),
        "details":details,"limits":"reproduced current storage SQL for Text, first replay batch, after=None, limit=200; not captured production internal trace"}),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn observed_cursor_position_retains_scanned_not_last_hit_and_resume() {
        use base64::Engine;
        let payload = serde_json::json!({"v":2,"thread":"v1:123:456",
            "resume":{"run":"run-hidden","from_block":19,"revision":3}});
        let token = format!(
            "ts2.{}.unverified",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap())
        );
        let position = super::observed_position(&token).unwrap();
        assert_eq!(position.thread, "v1:123:456");
        assert_eq!(position.resume.unwrap().from_block, 19);
        assert!(super::observed_position("ts1.invalid").is_err());
    }
}
