use rusqlite::{StatementStatus, params};

use super::*;

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

/// An index row in `state`; blocks are visible only when `indexed` at the
/// current revision.
fn run_row(db: &Database, run: &str, state: &str, source: u64, indexed: Option<u64>) {
    let blocked = (state == "blocked").then_some("provenance_unknown");
    let evicted = (state == "evicted").then_some("logical_budget");
    let retry = (state == "retry_wait").then_some(10_i64);
    let present = state != "deleted";
    db.conn
        .execute(
            "INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision, \
             indexed_revision, source_context_sha256, extractor_version, accounting_version, \
             source_updated_at_ns, retention_at_ns, blocked_reason, evicted_reason, \
             next_attempt_at_ns, source_present) \
             VALUES (?1, ?2, ?3, 7, ?4, 'hash', 1, 0, 1, 1, ?5, ?6, ?7, ?8)",
            params![
                run, state, source, indexed, blocked, evicted, retry, present
            ],
        )
        .unwrap();
}

fn indexed(db: &Database, run: &str) {
    run_row(db, run, "indexed", 3, Some(3));
}

fn block(db: &Database, run: &str, ordinal: usize, kind: &str, text: &str) -> i64 {
    let role = if ordinal.is_multiple_of(2) {
        "user"
    } else {
        "assistant"
    };
    db.conn
        .query_row(
            "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, \
             block_index, role, kind, tool_name, truncated, hash, text) \
             VALUES (?1, 'message', ?2, ?3, 0, ?4, ?5, ?6, 0, 'h', ?7) RETURNING id",
            params![
                run,
                format!("{ordinal}:0"),
                ordinal,
                role,
                kind,
                (kind != "text").then_some("shell"),
                text
            ],
            |row| row.get(0),
        )
        .unwrap()
}

fn literals(alternatives: &[&[&str]]) -> ConversationFilter {
    ConversationFilter::Literals(
        alternatives
            .iter()
            .map(|terms| terms.iter().map(|term| (*term).to_owned()).collect())
            .collect(),
    )
}

fn texts(
    db: &Database,
    runs: &[&str],
    kinds: &[IndexBlockKind],
    filter: &ConversationFilter,
) -> Vec<String> {
    let runs: Vec<String> = runs.iter().map(|run| (*run).to_owned()).collect();
    db.conversation_candidates(&ConversationQuery {
        runs: &runs,
        kinds,
        filter,
        after: None,
        limit: MAX_SEARCH_PAGE_ROWS,
    })
    .unwrap()
    .into_iter()
    .map(|candidate| candidate.text)
    .collect()
}

const TEXT: &[IndexBlockKind] = &[IndexBlockKind::Text];

#[test]
fn terms_route_to_trigram_like_or_scan_by_length_and_case_folding() {
    assert_eq!(term_route("ab"), TermRoute::Like(vec!["ab".into()]));
    assert_eq!(term_route("検索"), TermRoute::Like(vec!["検索".into()]));
    assert_eq!(term_route("abc"), TermRoute::Trigram(vec!["abc".into()]));
    assert_eq!(
        term_route("データベース"),
        TermRoute::Trigram(vec!["データベース".into()])
    );
    // ASCII k/s also fold to non-ASCII scalars that SQL does not pair.
    assert_eq!(
        term_route("Ks"),
        TermRoute::Like(vec![
            "ks".into(),
            "k\u{17F}".into(),
            "\u{212A}s".into(),
            "\u{212A}\u{17F}".into()
        ])
    );
    let TermRoute::Trigram(variants) = term_route("kiss") else {
        panic!("kiss is a trigram term");
    };
    assert_eq!(variants.len(), 8);
    // Other cased non-ASCII scalars, too many variants and NUL have no guarantee.
    for term in ["Äpfel", "σ", "ǅx", "sssss", "a\0b"] {
        assert_eq!(term_route(term), TermRoute::Scan, "{term:?}");
    }
}

#[test]
fn literal_filters_match_case_insensitively_by_trigram_and_like() {
    let db = db();
    indexed(&db, "run-1");
    block(&db, "run-1", 0, "text", "The CACHE prefix stays");
    block(&db, "run-1", 1, "text", "go fmt and Go vet");
    block(&db, "run-1", 2, "text", "日本語の検索結果");
    block(&db, "run-1", 3, "text", "\u{212A}ISS and ſoup");
    let only = |filter| texts(&db, &["run-1"], TEXT, &filter);
    assert_eq!(only(literals(&[&["cache"]])), ["The CACHE prefix stays"]);
    assert_eq!(only(literals(&[&["go"]])), ["go fmt and Go vet"]);
    assert_eq!(only(literals(&[&["検索"]])), ["日本語の検索結果"]);
    assert_eq!(only(literals(&[&["kiss"]])), ["\u{212A}ISS and ſoup"]);
    assert_eq!(only(literals(&[&["SOUP"]])), ["\u{212A}ISS and ſoup"]);
    // Words of one alternative must share a block; alternatives are OR'ed.
    assert_eq!(only(literals(&[&["cache", "vet"]])), Vec::<String>::new());
    assert_eq!(
        only(literals(&[&["cache"], &["vet"]])),
        ["The CACHE prefix stays", "go fmt and Go vet"]
    );
    // A term without a SQL guarantee leaves its alternative unrestricted.
    assert_eq!(only(literals(&[&["Äpfel"]])).len(), 4);
    assert_eq!(only(ConversationFilter::All).len(), 4);
}

#[test]
fn like_metacharacters_are_escaped_and_bound() {
    let db = db();
    indexed(&db, "run-1");
    block(&db, "run-1", 0, "text", "50% off");
    block(&db, "run-1", 1, "text", "500 off");
    block(&db, "run-1", 2, "text", "a_b");
    block(&db, "run-1", 3, "text", "axb");
    block(&db, "run-1", 4, "text", r"c:\d");
    block(&db, "run-1", 5, "text", "it's \"quoted\"");
    let only = |term: &str| texts(&db, &["run-1"], TEXT, &literals(&[&[term]]));
    assert_eq!(only("0%"), ["50% off"]);
    assert_eq!(only("_"), ["a_b"]);
    assert_eq!(only(r"\"), [r"c:\d"]);
    assert_eq!(only("'s \"quo"), ["it's \"quoted\""]);
    assert_eq!(only("%'"), Vec::<String>::new());
}

#[test]
fn only_current_indexed_runs_in_the_batch_are_returned_in_keyset_order() {
    let db = db();
    indexed(&db, "run-a");
    indexed(&db, "run-b");
    run_row(&db, "run-stale", "indexed", 4, Some(3));
    run_row(&db, "run-pending", "pending", 4, Some(3));
    let mut ids = Vec::new();
    for run in ["run-a", "run-b", "run-stale", "run-pending", "run-other"] {
        for ordinal in 0..3 {
            ids.push((
                run,
                block(&db, run, ordinal, "text", &format!("needle {run}")),
            ));
        }
    }
    let filter = literals(&[&["needle"]]);
    // The caller's order (b before a) is the scan order.
    let runs: Vec<String> = ["run-b", "run-a", "run-stale", "run-pending"]
        .map(String::from)
        .to_vec();
    let page = |after, limit| {
        db.conversation_candidates(&ConversationQuery {
            runs: &runs,
            kinds: TEXT,
            filter: &filter,
            after,
            limit,
        })
        .unwrap()
    };
    let all = page(None, MAX_SEARCH_PAGE_ROWS);
    let order: Vec<(String, i64)> = all
        .iter()
        .map(|candidate| (candidate.run_id.clone(), candidate.position.block_id))
        .collect();
    let expected: Vec<(String, i64)> = ids
        .iter()
        .filter(|(run, _)| *run == "run-b")
        .chain(ids.iter().filter(|(run, _)| *run == "run-a"))
        .map(|(run, id)| ((*run).to_owned(), *id))
        .collect();
    assert_eq!(order, expected);
    assert!(all.iter().all(|candidate| candidate.source_revision == 3
        && candidate.snapshot_revision == 7
        && !candidate.truncated));
    // Keyset pages concatenate to the full result.
    let mut paged = Vec::new();
    let mut after = None;
    loop {
        let rows = page(after, 2);
        after = rows.last().map(|row| row.position);
        paged.extend(
            rows.iter()
                .map(|row| (row.run_id.clone(), row.position.block_id)),
        );
        if rows.len() < 2 {
            break;
        }
    }
    assert_eq!(paged, expected);
}

#[test]
fn runs_outside_the_batch_never_change_rows_or_positions() {
    let db = db();
    indexed(&db, "run-visible");
    block(&db, "run-visible", 0, "text", "shared needle");
    let filter = literals(&[&["needle"]]);
    let runs = vec!["run-visible".to_owned()];
    let query = |db: &Database| {
        db.conversation_candidates(&ConversationQuery {
            runs: &runs,
            kinds: TEXT,
            filter: &filter,
            after: None,
            limit: 10,
        })
        .unwrap()
    };
    let before = query(&db);
    for index in 0..300 {
        let run = format!("run-hidden-{index}");
        indexed(&db, &run);
        block(&db, &run, 0, "text", "shared needle");
    }
    assert_eq!(query(&db), before);
    assert_eq!(
        db.thread_index_coverage(&runs).unwrap()[0].coverage,
        RunCoverage::Indexed
    );
}

fn measured_candidates(
    db: &Database,
    query: &ConversationQuery<'_>,
) -> (Vec<ConversationCandidate>, i32) {
    let (sql, params) = conversation_candidate_sql(query);
    let mut statement = db.conn.prepare(&sql).unwrap();
    let rows = statement
        .query_map(params_from_iter(params), candidate)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows, db.conversation_candidates(query).unwrap());
    (rows, statement.get_status(StatementStatus::VmStep))
}

fn candidate_plan(db: &Database, query: &ConversationQuery<'_>) -> Vec<String> {
    let (sql, params) = conversation_candidate_sql(query);
    db.conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(params_from_iter(params), |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn plan_checks_current_scope_before_run_blocks_and_correlated_fts() {
    let db = db();
    let filters = [
        ConversationFilter::All,
        literals(&[&["needle"]]),
        literals(&[&["%"]]),
        literals(&[&["Ä"]]),
    ];
    for count in [1, MAX_SEARCH_BATCH_RUNS] {
        let runs: Vec<_> = (0..count).map(|i| format!("run-{i}")).collect();
        for filter in &filters {
            let plan = candidate_plan(
                &db,
                &ConversationQuery {
                    runs: &runs,
                    kinds: TEXT,
                    filter,
                    after: None,
                    limit: MAX_SEARCH_PAGE_ROWS,
                },
            );
            let position = |text: &str| {
                plan.iter()
                    .position(|step| step.contains(text))
                    .unwrap_or_else(|| panic!("missing {text}: {plan:#?}"))
            };
            let materialize = position("MATERIALIZE current_runs");
            let scope = position("SCAN scope");
            let current = position("SEARCH r USING INDEX sqlite_autoindex_thread_index_runs_1");
            let blocks =
                position("SEARCH b USING INDEX idx_conversation_blocks_kind (kind=? AND run_id=?)");
            assert!(materialize < scope && scope < current && current < blocks);
            assert!(!plan.iter().any(|step| step.starts_with("SCAN b")));
            if matches!(filter, ConversationFilter::Literals(terms) if terms[0][0] == "needle") {
                let fts = position("SCAN conversation_fts");
                assert!(blocks < fts);
                // '=' is the rowid constraint, 'M' the MATCH constraint.
                assert!(plan[fts].contains("VIRTUAL TABLE INDEX 0:=M"));
            } else {
                assert!(plan.iter().all(|step| !step.contains("conversation_fts")));
            }
            eprintln!("scope={count} filter={filter:?}: {plan:?}");
        }
    }
}

#[test]
fn invisible_and_noncurrent_blocks_do_not_increase_candidate_vm_work() {
    let db = db();
    let runs: Vec<_> = (0..96).map(|i| format!("run-visible-{i}")).collect();
    for run in &runs {
        indexed(&db, run);
        for ordinal in 0..3 {
            block(&db, run, ordinal, "text", "common needle % _ \\ Ä");
        }
        block(&db, run, 3, "tool_result", "common needle % _ \\ Ä");
    }
    run_row(&db, "run-stale", "indexed", 4, Some(3));
    run_row(&db, "run-pending", "pending", 3, Some(3));
    run_row(&db, "run-deleted", "deleted", 3, Some(3));
    let mut scope = vec![
        "run-stale".to_owned(),
        "run-pending".to_owned(),
        "run-deleted".to_owned(),
        "run-missing".to_owned(),
    ];
    scope.extend(runs.iter().rev().cloned());
    assert_eq!(scope.len(), MAX_SEARCH_BATCH_RUNS);
    let filters = [
        ConversationFilter::All,
        literals(&[&["needle"]]),
        literals(&[&["%"]]),
        literals(&[&["Ä"]]),
        literals(&[&["needle", "_"], &["absent"]]),
        literals(&[&["absent"]]),
    ];
    let queries: Vec<_> = [&runs[..1], &scope[..4], &scope[..]]
        .into_iter()
        .flat_map(|runs| {
            filters.iter().flat_map(move |filter| {
                [1, 7, MAX_SEARCH_PAGE_ROWS]
                    .into_iter()
                    .flat_map(move |limit| {
                        [
                            None,
                            Some(CandidatePosition {
                                seq: 0,
                                block_id: 1,
                            }),
                        ]
                        .into_iter()
                        .map(move |after| ConversationQuery {
                            runs,
                            kinds: &[IndexBlockKind::Text, IndexBlockKind::ToolResult],
                            filter,
                            after,
                            limit,
                        })
                    })
            })
        })
        .collect();
    let before: Vec<_> = queries
        .iter()
        .map(|query| measured_candidates(&db, query))
        .collect();
    let tx = db.conn.unchecked_transaction().unwrap();
    for i in 0..256 {
        let run = format!("run-hidden-{i}");
        indexed(&db, &run);
        for extra in [&run, "run-stale", "run-pending", "run-deleted"] {
            for ordinal in 0..8 {
                block(
                    &db,
                    extra,
                    i * 8 + ordinal,
                    "text",
                    "common needle % _ \\ Ä",
                );
            }
        }
    }
    tx.commit().unwrap();
    for (query, expected) in queries.iter().zip(before) {
        let actual = measured_candidates(&db, query);
        assert_eq!(actual.0, expected.0, "{query:?}");
        assert_eq!(actual.1, expected.1, "VM steps changed: {query:?}");
        eprintln!(
            "scope={} filter={:?} limit={} after={:?}: rows={} VM={}->{}",
            query.runs.len(),
            query.filter,
            query.limit,
            query.after,
            actual.0.len(),
            expected.1,
            actual.1
        );
    }
}

#[test]
fn duplicate_scoped_runs_keep_distinct_keyset_positions() {
    let db = db();
    indexed(&db, "run-visible");
    let id = block(&db, "run-visible", 0, "text", "needle");
    let runs = vec!["run-visible".to_owned(); MAX_SEARCH_BATCH_RUNS];
    let filter = literals(&[&["needle"]]);
    let mut after = None;
    for seq in 0..MAX_SEARCH_BATCH_RUNS {
        let rows = db
            .conversation_candidates(&ConversationQuery {
                runs: &runs,
                kinds: TEXT,
                filter: &filter,
                after,
                limit: 1,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].position, CandidatePosition { seq, block_id: id });
        after = Some(rows[0].position);
    }
    assert!(
        db.conversation_candidates(&ConversationQuery {
            runs: &runs,
            kinds: TEXT,
            filter: &filter,
            after,
            limit: 1,
        })
        .unwrap()
        .is_empty()
    );
}

#[test]
fn tool_results_include_errors_only_when_requested() {
    let db = db();
    indexed(&db, "run-1");
    block(&db, "run-1", 0, "text", "needle text");
    block(&db, "run-1", 1, "tool_use", "needle call");
    block(&db, "run-1", 2, "tool_result", "needle ok");
    block(&db, "run-1", 3, "tool_error", "needle failed");
    let filter = literals(&[&["needle"]]);
    assert_eq!(texts(&db, &["run-1"], TEXT, &filter), ["needle text"]);
    let results = texts(
        &db,
        &["run-1"],
        &[IndexBlockKind::ToolResult, IndexBlockKind::ToolError],
        &filter,
    );
    assert_eq!(results, ["needle ok", "needle failed"]);
}

#[test]
fn coverage_classifies_every_state_and_missing_rows() {
    let db = db();
    indexed(&db, "run-i");
    run_row(&db, "run-old", "indexed", 5, Some(4));
    run_row(&db, "run-p", "pending", 1, None);
    run_row(&db, "run-r", "retry_wait", 1, None);
    run_row(&db, "run-b", "blocked", 1, None);
    run_row(&db, "run-e", "evicted", 1, None);
    run_row(&db, "run-d", "deleted", 1, None);
    let runs: Vec<String> = [
        "run-i", "run-old", "run-p", "run-r", "run-b", "run-e", "run-d", "run-none",
    ]
    .map(String::from)
    .to_vec();
    let coverage = db.thread_index_coverage(&runs).unwrap();
    let kinds: Vec<RunCoverage> = coverage.iter().map(|run| run.coverage).collect();
    assert_eq!(
        kinds,
        [
            RunCoverage::Indexed,
            RunCoverage::Pending,
            RunCoverage::Pending,
            RunCoverage::RetryWait,
            RunCoverage::Blocked,
            RunCoverage::Evicted,
            RunCoverage::Deleted,
            RunCoverage::Undiscovered
        ]
    );
    assert_eq!(coverage[0].source_revision, Some(3));
    assert_eq!(coverage[7].source_revision, None);
    let mut summary = IndexCoverageSummary::default();
    kinds.iter().for_each(|kind| summary.add(*kind));
    assert_eq!(
        (
            summary.indexed,
            summary.pending,
            summary.evicted,
            summary.missing
        ),
        (1, 4, 1, 2)
    );
    assert_eq!(summary.unindexed(), 7);
    assert!(summary.backfill_in_progress());
    let mut settled = IndexCoverageSummary::default();
    [
        RunCoverage::Indexed,
        RunCoverage::Blocked,
        RunCoverage::Evicted,
        RunCoverage::Deleted,
    ]
    .into_iter()
    .for_each(|kind| settled.add(kind));
    assert!(!settled.backfill_in_progress());
}

#[test]
fn batches_are_bounded_and_nonempty() {
    let db = db();
    let filter = ConversationFilter::All;
    let too_many: Vec<String> = (0..=MAX_SEARCH_BATCH_RUNS)
        .map(|index| format!("run-{index}"))
        .collect();
    for runs in [&too_many[..], &[]] {
        assert!(db.thread_index_coverage(runs).is_err());
        assert!(
            db.conversation_candidates(&ConversationQuery {
                runs,
                kinds: TEXT,
                filter: &filter,
                after: None,
                limit: 1,
            })
            .is_err()
        );
    }
    assert_eq!(
        db.thread_index_coverage(&too_many[..MAX_SEARCH_BATCH_RUNS])
            .unwrap()
            .len(),
        MAX_SEARCH_BATCH_RUNS
    );
    for (limit, kinds) in [(0, TEXT), (MAX_SEARCH_PAGE_ROWS + 1, TEXT), (1, &[][..])] {
        assert!(
            db.conversation_candidates(&ConversationQuery {
                runs: &too_many[..1],
                kinds,
                filter: &filter,
                after: None,
                limit,
            })
            .is_err()
        );
    }
}
