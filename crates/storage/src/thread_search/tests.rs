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
             next_attempt_at_ns, source_present, indexed_extractor_version) \
             VALUES (?1, ?2, ?3, 7, ?4, 'hash', 1, 0, 1, 1, ?5, ?6, ?7, ?8, 1)",
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
fn every_candidate_route_and_coverage_require_the_current_extractor() {
    let db = db();
    indexed(&db, "run-current");
    indexed(&db, "run-obsolete");
    indexed(&db, "run-mismatched");
    for run in ["run-current", "run-obsolete", "run-mismatched"] {
        block(&db, run, 0, "text", "oldneedle ab");
    }
    db.conn.execute("UPDATE thread_index_runs SET extractor_version = 0, indexed_extractor_version = 0 WHERE run_id = 'run-obsolete'", []).unwrap();
    db.conn.execute("UPDATE thread_index_runs SET indexed_extractor_version = 0 WHERE run_id = 'run-mismatched'", []).unwrap();
    let runs = ["run-current", "run-obsolete", "run-mismatched"];
    for filter in [
        ConversationFilter::All,
        literals(&[&["oldneedle"]]),
        literals(&[&["ab"]]),
    ] {
        assert_eq!(texts(&db, &runs, TEXT, &filter), ["oldneedle ab"]);
    }
    let coverage = db.thread_index_coverage(&runs.map(str::to_owned)).unwrap();
    assert_eq!(
        coverage.iter().map(|r| r.coverage).collect::<Vec<_>>(),
        [
            RunCoverage::Indexed,
            RunCoverage::Pending,
            RunCoverage::Pending
        ]
    );
}

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
    let (sql, params) = conversation_candidate_sql(query, &db.conn);
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
    let (sql, params) = conversation_candidate_sql(query, &db.conn);
    db.conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(params_from_iter(params), |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn original_filter_sql(filter: &ConversationFilter, params: &mut Vec<SqlValue>) -> String {
    let ConversationFilter::Literals(alternatives) = filter else {
        return "1".into();
    };
    let mut clauses = Vec::new();
    let mut clause_params = Vec::new();
    for terms in alternatives {
        let mut phrases = Vec::new();
        let mut likes = Vec::new();
        let mut like_params = Vec::new();
        for term in terms {
            match term_route(term) {
                TermRoute::Trigram(variants) => phrases.push(format!(
                    "({})",
                    variants
                        .iter()
                        .map(|variant| format!("\"{}\"", variant.replace('"', "\"\"")))
                        .collect::<Vec<_>>()
                        .join(" OR ")
                )),
                TermRoute::Like(variants) => {
                    let any = vec!["b.text LIKE ? ESCAPE '\\'"; variants.len()].join(" OR ");
                    likes.push(format!("({any})"));
                    like_params.extend(
                        variants
                            .iter()
                            .map(|variant| SqlValue::Text(format!("%{}%", like_escape(variant)))),
                    );
                }
                TermRoute::Scan => {}
            }
        }
        let mut predicates = Vec::new();
        if !phrases.is_empty() {
            predicates.push(
                "EXISTS (SELECT 1 FROM conversation_fts WHERE rowid = b.id AND conversation_fts MATCH ?)"
                    .to_owned(),
            );
            clause_params.push(SqlValue::Text(phrases.join(" AND ")));
        }
        predicates.extend(likes);
        clause_params.extend(like_params);
        if predicates.is_empty() {
            return "1".into();
        }
        clauses.push(predicates.join(" AND "));
    }
    params.extend(clause_params);
    clauses
        .into_iter()
        .map(|clause| format!("({clause})"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Pre-optimization SQL, intentionally independent of the new CTE builder.
/// Compare candidates before the runtime matcher (including FTS false hits).
fn original_candidate_sql(query: &ConversationQuery<'_>) -> (String, Vec<SqlValue>) {
    let mut params = Vec::new();
    let scope = scope_values(query.runs, &mut params);
    let kinds = query
        .kinds
        .iter()
        .map(|kind| {
            params.push(SqlValue::Text(kind_text(*kind).into()));
            "?"
        })
        .collect::<Vec<_>>()
        .join(", ");
    let (seq, id) = query
        .after
        .map_or((-1, 0), |after| (to_i64(after.seq), after.block_id));
    params.extend([SqlValue::Integer(seq), SqlValue::Integer(id)]);
    let filter = original_filter_sql(query.filter, &mut params);
    params.push(SqlValue::Integer(to_i64(query.limit)));
    let sql = format!(
        "WITH scope(run_id, seq) AS (VALUES {scope}), \
         current_runs AS MATERIALIZED ( \
             SELECT scope.seq, r.run_id, r.source_revision, r.snapshot_revision \
             FROM scope CROSS JOIN thread_index_runs r ON r.run_id = scope.run_id \
             WHERE r.state = 'indexed' AND r.source_present = 1 \
               AND r.indexed_revision = r.source_revision \
         ) \
         SELECT b.id, r.seq, b.run_id, r.source_revision, r.snapshot_revision, \
                b.source_kind, b.ordinal, b.block_index, b.note_seq, b.role, b.kind, \
                b.tool_name, b.truncated, b.text \
         FROM current_runs r \
         CROSS JOIN conversation_blocks b INDEXED BY idx_conversation_blocks_kind \
             ON b.run_id = r.run_id \
         WHERE b.kind IN ({kinds}) AND (r.seq, b.id) > (?, ?) AND ({filter}) \
         ORDER BY r.seq, b.id LIMIT ?"
    );
    (sql, params)
}

fn original_candidates(db: &Database, query: &ConversationQuery<'_>) -> Vec<ConversationCandidate> {
    let (sql, params) = original_candidate_sql(query);
    db.conn
        .prepare(&sql)
        .unwrap()
        .query_map(params_from_iter(params), candidate)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

// The UDF receives generated MATCH content, not MATCH grammar. Kept separate
// from the production encoder so these primitive tests exercise its input ABI.
fn fts_prefilter(filter: &ConversationFilter) -> Option<String> {
    let ConversationFilter::Literals(alternatives) = filter else {
        return None;
    };
    let clauses: Vec<Vec<Vec<String>>> = alternatives
        .iter()
        .map(|terms| {
            terms
                .iter()
                .filter_map(|term| match term_route(term) {
                    TermRoute::Trigram(variants) => Some(variants),
                    _ => None,
                })
                .collect()
        })
        .collect();
    serde_json::to_string(&clauses).ok()
}

#[test]
fn exact_evaluation_replaces_only_match_leaves_with_like_and_scan_or_preserved() {
    let db = db();
    for filter in [
        literals(&[&["needle", "%"]]),
        literals(&[&["needle"], &["検索"]]),
        literals(&[&["needle"], &["Ä"]]),
        literals(&[&["needle"], &[]]),
    ] {
        let mut params = Vec::new();
        let sql = filter_sql(
            &filter,
            &mut params,
            tokenizer_prefilter::install_untrusted_for_tests(&db.conn),
        );
        let old = original_filter_sql(&filter, &mut Vec::new());
        if old == "1" {
            assert_eq!(sql, "1");
        } else {
            assert!(sql.contains("COALESCE(evorch_fts5_exact"));
            assert_eq!(sql.matches("LIKE").count(), old.matches("LIKE").count());
        }
    }
}

#[test]
fn exact_preserves_original_fts_candidates_for_every_ascii_scalar() {
    let db = db();
    indexed(&db, "run");
    // Trigram treats punctuation/control/space as scalars, not word separators.
    // Exercise every non-NUL ASCII scalar, including quoting and LIKE escapes.
    let terms: Vec<_> = (1..=127_u8)
        .flat_map(|c| {
            let c = char::from(c);
            // Both a single trigram and a phrase of overlapping trigrams.
            [format!("A{c}b"), format!("Ab{c}CdE")]
        })
        .collect();
    for (ordinal, term) in terms.iter().enumerate() {
        block(&db, "run", ordinal, "text", &format!("!{term}?"));
        block(
            &db,
            "run",
            ordinal + terms.len(),
            "text",
            &format!("!{}?", term.to_ascii_lowercase()),
        );
    }
    let runs = ["run".into()];
    for term in &terms {
        let filter = literals(&[&[term]]);
        let query = ConversationQuery {
            runs: &runs,
            kinds: TEXT,
            filter: &filter,
            after: None,
            limit: MAX_SEARCH_PAGE_ROWS,
        };
        let expected = original_candidates(&db, &query);
        assert!(!expected.is_empty(), "{term:?}");
        assert_eq!(
            db.conversation_candidates(&query).unwrap(),
            expected,
            "{term:?}"
        );
    }
}

#[test]
fn unicode_and_nul_exact_preserves_fts_false_hits() {
    let db = db();
    assert!(tokenizer_prefilter::install_untrusted_for_tests(&db.conn));
    for (text, term, expected) in [
        ("日本語 unrelated", "abc", false),
        ("日本語 ABC", "abc", true),
        ("A\0B\0C", "abc", true),
        ("ABC\0tail", "abc", true),
        ("日本語\0unrelated", "abc", false),
        ("ſKY", "sky", true),
        ("a\u{301}bc", "abc", false),
        ("a\u{301}bc", "a\u{301}bc", true),
        ("データベース", "データベース", true),
        ("aaaaaababaaaa", "aaaaabab", true),
        ("aaaaaababaaaa", "aaaaabb", false),
        ("a\"b", "a\"b", true),
        ("a\nb", "a\nb", true),
    ] {
        let json = fts_prefilter(&literals(&[&[term]])).unwrap();
        let actual: bool = db
            .conn
            .query_row(
                "SELECT evorch_fts5_exact(?1, ?2)",
                params![text, json],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, expected, "{text:?}, {term:?}");
    }
}

#[test]
fn sqlite_unicode_folds_and_nul_positions_agree_with_original_fts() {
    let db = db();
    indexed(&db, "run");
    // These cased query literals may route to Scan in production. Exercise
    // the tokenizer primitive as well, against SQLite's independent MATCH.
    let pairs = [
        ("ÄBC", "äbc"),
        ("ſKY", "sky"),
        ("İbc", "ibc"),
        ("ＡＢＣ", "ａｂｃ"),
        ("Σbc", "ςbc"),
        ("a\u{0345}bc", "aιbc"),
        ("a\u{301}bc", "abc"),
        ("ébc", "e\u{301}bc"),
        ("A\0B\0C", "abc"),
        ("日本\0語", "日本語"),
        ("a\"bc", "a\"bc"),
        ("a\tbc", "a\tbc"),
        ("İbc", "İbc"),
        ("ébc", "ébc"),
        ("ÉBC", "ébc"),
        ("e\u{301}bc", "e\u{301}bc"),
        ("a\u{301}\u{301}bc", "a\u{301}\u{301}bc"),
        ("æbc", "ÆBC"),
        ("ﬃbc", "ffibc"),
        ("😀🦀日 abc", "😀🦀日"),
    ];
    assert!(tokenizer_prefilter::install_untrusted_for_tests(&db.conn));
    for (ordinal, (text, term)) in pairs.iter().enumerate() {
        let id = block(&db, "run", ordinal, "text", text);
        let phrase = format!("\"{}\"", term.replace('"', "\"\""));
        let expected: bool = db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM conversation_fts \
                 WHERE rowid = ?1 AND conversation_fts MATCH ?2)",
                params![id, phrase],
                |row| row.get(0),
            )
            .unwrap();
        let json = serde_json::to_string(&vec![vec![vec![term]]]).unwrap();
        let actual: bool = db
            .conn
            .query_row(
                "SELECT evorch_fts5_exact(?1, ?2)",
                params![text, json],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, expected, "{text:?}, {term:?}");
    }
}

#[test]
fn exact_sql_short_circuits_old_match_only_for_known_true_or_false() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let db = db();
    indexed(&db, "run");
    let id = block(&db, "run", 0, "text", "ABC");
    assert!(tokenizer_prefilter::install_untrusted_for_tests(&db.conn));
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    db.conn
        .create_scalar_function(
            c"fallback_probe",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |ctx| {
                count.fetch_add(1, Ordering::Relaxed);
                ctx.get::<bool>(0)
            },
        )
        .unwrap();
    let mut statement = db
        .conn
        .prepare(
            "SELECT COALESCE(evorch_fts5_exact(?1, ?2), fallback_probe( \
             EXISTS(SELECT 1 FROM conversation_fts WHERE rowid = ?3 \
                    AND conversation_fts MATCH ?4)))",
        )
        .unwrap();
    // Rebinding this SAME statement invalidates auxdata for argument 2.
    // Unknown (including zero-token/short phrases) delegates to the real FTS.
    for (text, json, phrase, expected, fallbacks) in [
        ("ABC", "[[[\"abc\"]]]", "\"absent\"", true, 0),
        ("unrelated", "[[[\"abc\"]]]", "\"abc\"", false, 0),
        ("ABC", "[[[\"xyz\"]]]", "\"abc\"", false, 0),
        ("ABC", "malformed", "\"abc\"", true, 1),
        ("ABC", "[[[\"ab\"]]]", "\"ab\"", false, 1),
        ("ABC", "[[[\"\"]]]", "\"\"", false, 1),
        ("ABC", "[]", "\"abc\"", true, 1),
        ("ABC", "[[[\"abc\"]]]", "\"absent\"", true, 0),
    ] {
        calls.store(0, Ordering::Relaxed);
        let result: bool = statement
            .query_row(params![text, json, id, phrase], |row| row.get(0))
            .unwrap();
        assert_eq!(result, expected, "{json}");
        assert_eq!(calls.load(Ordering::Relaxed), fallbacks, "{json}");
    }
}

#[test]
fn unknown_fts_schema_settings_disable_exact_even_after_install() {
    let db = db();
    assert!(tokenizer_prefilter::install_untrusted_for_tests(&db.conn));
    for settings in [
        "text, tokenize = 'trigram case_sensitive 1'",
        "text, tokenize = 'trigram remove_diacritics 1'",
        "text, tokenize = 'trigram', detail = none",
        "text, extra, tokenize = 'trigram'",
        "text, tokenize = 'unicode61'",
    ] {
        db.conn
            .execute_batch(&format!(
                "DROP TABLE conversation_fts; \
                 CREATE VIRTUAL TABLE conversation_fts USING fts5({settings});"
            ))
            .unwrap();
        assert!(
            !tokenizer_prefilter::install_untrusted_for_tests(&db.conn),
            "{settings}"
        );
        let filter = literals(&[&["abc"]]);
        let query = ConversationQuery {
            runs: &["run".into()],
            kinds: TEXT,
            filter: &filter,
            after: None,
            limit: 1,
        };
        let (sql, _) = conversation_candidate_sql(&query, &db.conn);
        assert!(sql.contains("COALESCE(evorch_fts5_exact"));
        assert!(
            db.conn
                .query_row(
                    "SELECT evorch_fts5_exact('abc', '[[[\"abc\"]]]')",
                    [],
                    |row| row.get::<_, Option<bool>>(0)
                )
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn tokenizer_api_unavailable_keeps_original_candidate_sql() {
    let db = db();
    indexed(&db, "run");
    block(&db, "run", 0, "text", "日本語 needle");
    // Simulate API discovery failure, without disabling the actual FTS index.
    db.conn
        .create_scalar_function(
            c"fts5",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_| Ok(false),
        )
        .unwrap();
    let runs = ["run".into()];
    let query = ConversationQuery {
        runs: &runs,
        kinds: TEXT,
        filter: &literals(&[&["needle"]]),
        after: None,
        limit: 1,
    };
    let (sql, _) = conversation_candidate_sql(&query, &db.conn);
    assert!(sql.contains("COALESCE(evorch_fts5_exact"));
    assert!(!sql.contains("scoped_blocks AS MATERIALIZED"));
    assert_eq!(
        db.conversation_candidates(&query).unwrap(),
        original_candidates(&db, &query)
    );
}

#[test]
fn tokenizer_function_auxdata_rebind_connection_move_and_finalize_are_safe() {
    let db = db();
    assert!(tokenizer_prefilter::install_untrusted_for_tests(&db.conn));
    let abc = fts_prefilter(&literals(&[&["abc"]])).unwrap();
    let xyz = fts_prefilter(&literals(&[&["xyz"]])).unwrap();
    {
        let mut statement = db.conn.prepare("SELECT evorch_fts5_exact(?1, ?2)").unwrap();
        for (text, json, expected) in [
            ("日本語 ABC", &abc, true),
            ("日本語 xyz", &abc, false),
            ("日本語 xyz", &xyz, true),
            ("A\0BC", &abc, true),
        ] {
            assert_eq!(
                statement
                    .query_row(params![text, json], |row| row.get::<_, bool>(0))
                    .unwrap(),
                expected
            );
        }
    } // Finalize clears auxdata before the connection/tokenizer can be dropped.
    std::thread::spawn(move || {
        assert!(tokenizer_prefilter::install_untrusted_for_tests(&db.conn));
        assert!(
            db.conn
                .query_row(
                    "SELECT evorch_fts5_exact(?1, ?2)",
                    params!["A\0BC", abc],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        db.conn.close().unwrap();
    })
    .join()
    .unwrap();
}

#[test]
fn differential_pages_preserve_false_hits_unicode_or_kinds_and_budget_prefixes() {
    let db = db();
    for run in ["a", "b", "invisible"] {
        indexed(&db, run);
    }
    run_row(&db, "stale", "indexed", 4, Some(3));
    let corpus = [
        "ALPHA beta % _ \\ \"quote\"",
        "alpha ... beta",
        "SKY ſky SKY",
        "ſKY",
        "SKY",
        "ſky",
        "Key",
        "Ä Ö ä ö",
        "検索語 データベース",
        "A\0BC", // FTS abc passes; final literal matcher need not accept it.
        "ABC\0tail",
        "alpha\nbeta\tend",
        "a\"b a%b a_b a\\b",
        "a\u{301}bc abc",
        "ÄBC äbc",
        "ＡＢＣ ａｂｃ",
        "a\0\0b\0c",
        "\0abc\0",
        "a\u{301}\u{301}bc",
        "a\u{0345}bc", // Unicode fold of a combining scalar, not Rust lower.
        "aaaaaababaaaa",
        "検索語\0データベース",
        "no match",
        "aιbc", // SQLite folds U+0345 to iota; final literal matcher can reject.
    ];
    for run in ["a", "b", "invisible", "stale"] {
        for (ordinal, text) in corpus.iter().enumerate() {
            block(&db, run, ordinal, "text", text);
            block(&db, run, ordinal + corpus.len(), "tool_result", text);
        }
    }
    let runs = ["b".into(), "stale".into(), "a".into(), "b".into()];
    let filters = [
        literals(&[&["alpha beta"]]),
        literals(&[&["alpha", "beta"]]),
        literals(&[&["abc"]]),
        literals(&[&["sky"]]),
        literals(&[&["key"]]),
        literals(&[&["alpha", "%"], &["sky", "Ä"]]),
        literals(&[&["abc"], &["検索語"]]),
        literals(&[&["abc"], &["%"]]),
        literals(&[&["ab"], &["_"], &["\\"]]),
        literals(&[&["a\"b"], &["a%b"], &["a_b"], &["a\\b"]]),
        literals(&[&["alpha\nbeta"]]),
        literals(&[&["abc", "Ä"], &["sky", "検索"]]),
        literals(&[&["検索語", "データベース"]]),
        literals(&[&["abc", "検索語"]]),
        literals(&[&["a\u{301}bc"], &["a\u{0345}bc"]]),
        literals(&[&["aaaaabab"]]),
        literals(&[&["abc"], &["ssssss"]]),
        literals(&[&["abc", "\0"]]),
        literals(&[&["abc"], &["Ä"]]),
        literals(&[&["abc"], &[]]),
        literals(&[&["abc", "ab"], &["alpha", "%"]]),
        literals(&[&["abc", "missing"], &["alpha", "%"]]),
        literals(&[&[""]]),
        ConversationFilter::All,
    ];
    for filter in &filters {
        for kinds in [TEXT, &[IndexBlockKind::Text, IndexBlockKind::ToolResult]] {
            for limit in [1, 2, 7, MAX_SEARCH_PAGE_ROWS] {
                let mut query = ConversationQuery {
                    runs: &runs,
                    kinds,
                    filter,
                    after: None,
                    limit,
                };
                let mut consumed = Vec::new();
                loop {
                    let expected = original_candidates(&db, &query);
                    let actual = db.conversation_candidates(&query).unwrap();
                    assert_eq!(actual, expected, "{filter:?}, {query:?}");
                    if actual.is_empty() {
                        break;
                    }
                    query.after = actual.last().map(|row| row.position);
                    consumed.extend(actual);
                }
                let full = original_candidates(
                    &db,
                    &ConversationQuery {
                        after: None,
                        limit: MAX_SEARCH_PAGE_ROWS,
                        ..query
                    },
                );
                assert_eq!(consumed, full);
                // Every possible candidate budget sees the same prefix/cursor.
                for budget in 1..=full.len() {
                    assert_eq!(&consumed[..budget], &full[..budget]);
                    assert_eq!(consumed[budget - 1].position, full[budget - 1].position);
                }
                if filter == &literals(&[&["abc"]]) {
                    assert!(consumed.iter().any(|row| row.text == "A\0BC"));
                }
            }
        }
    }
}

#[test]
fn unknown_oversized_leaf_falls_back_in_production_candidate_pages() {
    let db = db();
    indexed(&db, "run");
    let term = "a".repeat(4100);
    block(&db, "run", 0, "text", "unrelated");
    block(&db, "run", 1, "text", &term);
    let filter = literals(&[&[&term]]);
    let mut query = ConversationQuery {
        runs: &["run".into()],
        kinds: TEXT,
        filter: &filter,
        after: None,
        limit: 1,
    };
    let (sql, _) = conversation_candidate_sql(&query, &db.conn);
    assert!(sql.contains("COALESCE(evorch_fts5_exact"));
    for _ in 0..2 {
        let expected = original_candidates(&db, &query);
        let actual = db.conversation_candidates(&query).unwrap();
        assert_eq!(actual, expected);
        query.after = actual.last().map(|row| row.position);
    }
}

#[test]
fn fts_false_hits_keep_count_byte_budget_and_cursor_boundary_candidates() {
    let db = db();
    indexed(&db, "run");
    db.conn.execute_batch("BEGIN").unwrap();
    for ordinal in 0..2002 {
        // SQLite removes NULs during trigram tokenization; substring matching
        // must reject this. Such candidates still consume runtime budgets.
        block(&db, "run", ordinal, "text", "A\0BC");
    }
    db.conn.execute_batch("COMMIT").unwrap();
    let filter = literals(&[&["abc"]]);
    let mut query = ConversationQuery {
        runs: &["run".into()],
        kinds: TEXT,
        filter: &filter,
        after: None,
        limit: MAX_SEARCH_PAGE_ROWS,
    };
    let mut count = 0;
    let mut bytes = 0;
    loop {
        let expected = original_candidates(&db, &query);
        let actual = db.conversation_candidates(&query).unwrap();
        assert_eq!(actual, expected);
        if actual.is_empty() {
            break;
        }
        for (new, old) in actual.iter().zip(&expected) {
            count += 1;
            bytes += new.text.len();
            assert_eq!(bytes, count * old.text.len());
            assert_eq!(new.position, old.position);
            assert!(!new.text.to_ascii_lowercase().contains("abc"));
        }
        query.after = actual.last().map(|row| row.position);
        if count == 2000 {
            assert_eq!(
                query.after.unwrap().block_id,
                actual.last().unwrap().position.block_id
            );
        }
    }
    assert_eq!(count, 2002);

    // Cross the runtime's 4-MiB byte boundary on a false hit as well, without
    // replacing the candidate sequence with final substring matches.
    db.conn
        .execute("DELETE FROM conversation_blocks", [])
        .unwrap();
    let padding = " ".repeat(8192);
    db.conn.execute_batch("BEGIN").unwrap();
    for ordinal in 0..MAX_SEARCH_PAGE_ROWS + 15 {
        block(&db, "run", ordinal, "text", &format!("A\0BC{padding}"));
    }
    db.conn.execute_batch("COMMIT").unwrap();
    query.after = None;
    let mut bytes = 0;
    let mut reached_boundary = false;
    loop {
        let actual = db.conversation_candidates(&query).unwrap();
        let expected = original_candidates(&db, &query);
        assert_eq!(actual, expected);
        if actual.is_empty() {
            break;
        }
        for (new, old) in actual.iter().zip(&expected) {
            bytes += new.text.len();
            if bytes >= 4 * 1024 * 1024 && !reached_boundary {
                assert_eq!(new.position, old.position);
                reached_boundary = true;
            }
        }
        query.after = actual.last().map(|row| row.position);
    }
    assert!(reached_boundary);
}

#[test]
fn empty_alternatives_keep_the_original_sql_error_instead_of_changing_semantics() {
    let db = db();
    let filter = ConversationFilter::Literals(Vec::new());
    let mut old_params = Vec::new();
    let mut new_params = Vec::new();
    assert_eq!(
        filter_sql(&filter, &mut new_params, true),
        original_filter_sql(&filter, &mut old_params)
    );
    assert_eq!(new_params, old_params);
    assert!(
        db.conversation_candidates(&ConversationQuery {
            runs: &["run".into()],
            kinds: TEXT,
            filter: &filter,
            after: None,
            limit: 1,
        })
        .is_err()
    );
}

#[test]
fn external_content_triggers_integrity_and_revision_share_read_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = crate::StorageConfig {
        db_path: dir.path().join("snapshot.db"),
        ..Default::default()
    };
    let db = Database::open(&cfg).unwrap();
    let writer = Database::open(&cfg).unwrap();
    indexed(&db, "run");
    let id = block(&db, "run", 0, "text", "old needle");
    let integrity = |db: &Database| {
        db.conn
            .execute(
                "INSERT INTO conversation_fts(conversation_fts, rank) \
                 VALUES('integrity-check', 1)",
                [],
            )
            .unwrap();
    };
    let compare = |db: &Database, term: &str| {
        let query = ConversationQuery {
            runs: &["run".into(), "hidden".into()],
            kinds: TEXT,
            filter: &literals(&[&[term]]),
            after: None,
            limit: 1,
        };
        let rows = db.conversation_candidates(&query).unwrap();
        assert_eq!(rows, original_candidates(db, &query));
        rows
    };
    integrity(&db); // insert trigger, including external-content comparison
    db.conn.execute_batch("BEGIN").unwrap();
    let old = compare(&db, "needle");
    writer
        .conn
        .execute_batch(&format!(
            "BEGIN; UPDATE conversation_blocks SET text = 'new value' WHERE id = {id}; \
             UPDATE thread_index_runs SET source_revision = 4, indexed_revision = 4, \
                    snapshot_revision = 8 WHERE run_id = 'run'; COMMIT;"
        ))
        .unwrap();
    integrity(&writer);
    assert_eq!(compare(&db, "needle"), old);
    assert!(compare(&db, "value").is_empty());
    db.conn.execute_batch("COMMIT").unwrap();
    assert!(compare(&db, "needle").is_empty());
    let new = compare(&db, "value");
    assert_eq!(new[0].source_revision, 4);
    assert_eq!(new[0].snapshot_revision, 8);
    // Locator-only and no-op text updates must leave postings intact.
    db.conn
        .execute(
            "UPDATE conversation_blocks SET ordinal = 9, text = text WHERE id = ?1",
            [id],
        )
        .unwrap();
    integrity(&db);
    assert_eq!(
        compare(&db, "value")[0].locator,
        BlockLocator::Message {
            ordinal: 9,
            block_index: 0,
        }
    );
    db.conn
        .execute_batch("UPDATE thread_index_runs SET source_revision = 5 WHERE run_id = 'run';")
        .unwrap();
    assert!(compare(&db, "value").is_empty()); // stale source fence
    db.conn
        .execute_batch("UPDATE thread_index_runs SET indexed_revision = 5 WHERE run_id = 'run';")
        .unwrap();
    assert_eq!(compare(&db, "value").len(), 1);
    db.conn
        .execute("DELETE FROM conversation_blocks WHERE id = ?1", [id])
        .unwrap();
    integrity(&db);
    assert!(compare(&db, "value").is_empty());
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
            let current = position("SEARCH r USING INDEX sqlite_autoindex_thread_index_runs_1");
            let blocks =
                position("SEARCH b USING INDEX idx_conversation_blocks_kind (kind=? AND run_id=?)");
            assert!(
                current < blocks,
                "current-run fence must precede blocks: {plan:#?}"
            );
            assert!(
                plan.iter().all(|step| !step.contains("TEMP B-TREE")),
                "{plan:#?}"
            );
            if count > 1 {
                assert!(plan.iter().any(|step| step == "MERGE (UNION ALL)"));
            }
            if matches!(filter, ConversationFilter::Literals(terms) if terms[0][0] == "needle") {
                let fts = position("SCAN conversation_fts");
                assert!(blocks < fts);
                assert!(!plan.iter().any(|step| step.contains("scoped_blocks")));
                // '=' is the rowid constraint, 'M' the MATCH constraint.
                assert!(plan[fts].contains("VIRTUAL TABLE INDEX 0:=M"));
            } else {
                assert!(!plan.iter().any(|step| step.starts_with("SCAN b")));
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
        block(&db, run, 4, "text", "common needle");
        block(&db, run, 5, "text", "common ... needle");
        block(&db, run, 6, "text", "unrelated ASCII block");
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
        literals(&[&["common needle"]]),
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
fn ordered_merge_bounds_fts_calls_without_limiting_before_the_predicate() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let db = db();
    let runs = ["a".into(), "b".into(), "a".into()];
    for run in ["a", "b"] {
        indexed(&db, run);
        for i in 0..240 {
            block(&db, run, i, "text", if i < 210 { "miss" } else { "needle" });
            block(&db, run, i + 240, "tool_result", "needle");
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    db.conn
        .create_scalar_function(
            c"count_fts",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |ctx| {
                count.fetch_add(1, Ordering::Relaxed);
                ctx.get::<bool>(0)
            },
        )
        .unwrap();
    let filter = literals(&[&["needle"]]);
    let query = ConversationQuery {
        runs: &runs,
        kinds: &[IndexBlockKind::Text, IndexBlockKind::ToolResult],
        filter: &filter,
        after: None,
        limit: 7,
    };
    // Wrap the independent old-FTS leaf, not its result after EXISTS, to count
    // predicate evaluations even when the FTS result is false.
    let counted = |sql: String| {
        sql.replace(
            "EXISTS (SELECT 1 FROM conversation_fts WHERE rowid = b.id AND conversation_fts MATCH ?2)",
            "count_fts(EXISTS (SELECT 1 FROM conversation_fts WHERE rowid = b.id AND conversation_fts MATCH ?2))",
        )
    };
    let (sql, args) = conversation_candidate_sql(&query, &db.conn);
    // Production NULL always evaluates old MATCH; this wrapping changes no
    // filtering/order. The test callback is absent from production SQL.
    let sql = counted(sql);
    let mut statement = db.conn.prepare(&sql).unwrap();
    let actual = statement
        .query_map(params_from_iter(args), candidate)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let new_calls = calls.swap(0, Ordering::Relaxed);
    let (old_sql, old_args) = original_candidate_sql(&query);
    let old_sql = old_sql.replace(
        "EXISTS (SELECT 1 FROM conversation_fts WHERE rowid = b.id AND conversation_fts MATCH ?)",
        "count_fts(EXISTS (SELECT 1 FROM conversation_fts WHERE rowid = b.id AND conversation_fts MATCH ?))",
    );
    let mut old = db.conn.prepare(&old_sql).unwrap();
    let expected = old
        .query_map(params_from_iter(old_args), candidate)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let old_calls = calls.swap(0, Ordering::Relaxed);
    assert_eq!(actual, expected);
    assert_eq!(old_calls, 240 * 2 * runs.len());
    assert_eq!(old.get_status(StatementStatus::Sort), 1);
    assert_eq!(statement.get_status(StatementStatus::Sort), 0);
    // Every arm must find its first match (including 210 misses per text
    // stream), but does not scan all remaining matches to sort them.
    assert!(new_calls >= 210 * 3 + query.limit);
    assert!(new_calls < old_calls, "{new_calls} >= {old_calls}");
    eprintln!("ordered merge: FTS calls={new_calls}, old FTS calls={old_calls}");
}

#[test]
fn compound_boundary_deduplicates_kinds_but_preserves_run_sequences() {
    let db = db();
    indexed(&db, "run");
    for (ordinal, kind) in [
        "text",
        "working_note",
        "tool_use",
        "tool_result",
        "tool_error",
    ]
    .iter()
    .enumerate()
    {
        if *kind != "working_note" {
            block(&db, "run", ordinal, kind, "needle");
        }
    }
    let runs = vec!["run".to_owned(); MAX_SEARCH_BATCH_RUNS];
    let kinds = [
        IndexBlockKind::Text,
        IndexBlockKind::WorkingNote,
        IndexBlockKind::ToolUse,
        IndexBlockKind::ToolResult,
        IndexBlockKind::ToolError,
        IndexBlockKind::Text,
        IndexBlockKind::ToolResult,
    ];
    let filter = literals(&[&["needle"]]);
    for limit in [1, 200, MAX_SEARCH_PAGE_ROWS] {
        let mut query = ConversationQuery {
            runs: &runs,
            kinds: &kinds,
            filter: &filter,
            after: None,
            limit,
        };
        let (sql, _) = conversation_candidate_sql(&query, &db.conn);
        assert_eq!(sql.matches(" UNION ALL ").count(), 499);
        loop {
            let actual = db.conversation_candidates(&query).unwrap();
            assert_eq!(actual, original_candidates(&db, &query));
            if actual.is_empty() {
                break;
            }
            query.after = actual.last().map(|row| row.position);
        }
        for seq in [runs.len(), usize::MAX] {
            query.after = Some(CandidatePosition {
                seq,
                block_id: i64::MAX,
            });
            assert_eq!(
                db.conversation_candidates(&query).unwrap(),
                original_candidates(&db, &query)
            );
        }
        query.kinds = &[];
        assert!(db.conversation_candidates(&query).is_err());
    }
}

#[test]
fn untrusted_production_udf_is_null_even_for_same_name_successful_function() {
    let db = db();
    db.conn
        .create_scalar_function(
            c"evorch_fts5_exact",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_| Ok(true),
        )
        .unwrap();
    // fts5() itself may also be shadowed. Neither name nor success is trusted.
    db.conn
        .create_scalar_function(
            c"fts5",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_| -> rusqlite::Result<bool> { panic!("must not discover unknown tokenizer") },
        )
        .unwrap();
    assert!(tokenizer_prefilter::install(&db.conn));
    assert_eq!(
        db.conn
            .query_row(
                "SELECT evorch_fts5_exact('abc', '[[[\"abc\"]]]')",
                [],
                |row| row.get::<_, Option<bool>>(0)
            )
            .unwrap(),
        None
    );
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
