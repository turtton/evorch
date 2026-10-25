//! Phase B search tests use explicitly native fixtures and the shared extractor.
//! Index rows are installed synchronously, without a background owner or sleeps.
use super::*;
use rusqlite::{Connection, params};
use storage::thread_index::{BlockLocator, extract_snapshot_bundle};

fn label(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

impl Fixture {
    fn native_run(
        &self,
        run: &str,
        thread: Option<&str>,
        parent: Option<&str>,
        messages: Vec<Message>,
    ) {
        self.run(run, thread, parent, messages);
        let record = self
            .store()
            .with_database(|db| db.run_context(run))
            .unwrap()
            .unwrap();
        let write = crate::restore::provenance::ConversationProvenance::default()
            .write(record)
            .unwrap();
        self.store().handle.save_snapshot_bundle(&write).unwrap();
    }

    fn index(&self, run: &str) {
        let bundle = self
            .store()
            .with_database(|db| Ok(db.snapshot_bundle(run).unwrap().unwrap()))
            .unwrap();
        let input =
            extract_snapshot_bundle(&bundle, &SecretRedactor::with_known_values([SECRET.into()]))
                .unwrap();
        let mut conn = Connection::open(&self.store().db_path).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute("DELETE FROM conversation_blocks WHERE run_id = ?1", [run])
            .unwrap();
        for block in input.blocks {
            let (source, key, ordinal, index, note) = match block.locator {
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
                    "working_note".into(),
                    None,
                    None,
                    Some(note_seq as i64),
                ),
            };
            tx.execute(
                "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, \
                 block_index, note_seq, role, kind, tool_name, truncated, hash, text) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    run,
                    source,
                    key,
                    ordinal,
                    index,
                    note,
                    label(&block.role),
                    label(&block.kind),
                    block.tool_name,
                    block.truncated,
                    block.hash,
                    block.text
                ],
            )
            .unwrap();
        }
        tx.execute(
            "UPDATE thread_index_runs SET state = 'indexed', indexed_revision = source_revision, \
             indexed_extractor_version = extractor_version, \
             indexed_accounting_version = accounting_version WHERE run_id = ?1",
            [run],
        )
        .unwrap();
        tx.commit().unwrap();
    }
}

fn count(value: &Value) -> u64 {
    value["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["hit_count"].as_u64().unwrap())
        .sum()
}

#[test]
fn default_conversation_explicit_tools_and_position_dedup() {
    let f = Fixture::new();
    let own = f.own();
    f.native_run(
        "run-100",
        Some("own"),
        None,
        vec![
            user(vec![text("literal alpha beta")]),
            assistant(vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "shell".into(),
                input: json!({"command": "explicit toolneedle"}),
            }]),
            user(vec![
                ContentBlock::ToolResult {
                    tool_call_id: "c1".into(),
                    content: vec![
                        ToolResultContent::Image {
                            media_type: "image/png".into(),
                            data: "aW1hZ2U=".into(),
                            source: None,
                        },
                        ToolResultContent::Text {
                            text: format!("toolneedle success {}", "x".repeat(2300)),
                        },
                    ],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_call_id: "c2".into(),
                    content: vec![ToolResultContent::Text {
                        text: format!("toolneedle failure {}", "x".repeat(2300)),
                    }],
                    is_error: true,
                },
            ]),
            assistant(vec![text("the final body")]),
        ],
    );
    f.index("run-100");
    let result = f
        .call(own, "thread_search", json!({"query": ["alpha", "beta"]}))
        .unwrap();
    assert_eq!(
        count(&result),
        1,
        "OR alternatives count the same position once"
    );
    assert_eq!(result["index"]["indexed"], 1);
    assert_eq!(
        count(
            &f.call(own, "thread_search", json!({"query": "final body"}))
                .unwrap()
        ),
        1,
        "final report and conversation refer to the same canonical position"
    );
    assert_eq!(result["threads"][0]["snippets"][0]["kind"], "conversation");
    assert_eq!(
        count(
            &f.call(own, "thread_search", json!({"query": "toolneedle"}))
                .unwrap()
        ),
        0
    );
    let tools = f
        .call(
            own,
            "thread_search",
            json!({
                "query": "toolneedle", "kinds": ["tool_result"]
            }),
        )
        .unwrap();
    assert_eq!(count(&tools), 2);
    let snippets = tools["threads"][0]["snippets"].as_array().unwrap();
    assert!(
        snippets
            .iter()
            .any(|s| s["is_error"] == false && s["truncated"] == true)
    );
    let error = snippets.iter().find(|s| s["is_error"] == true).unwrap();
    assert_eq!(error["truncated"], false, "errors are indexed in full");
    assert!(error["source_revision"].is_u64());
    let success = snippets.iter().find(|s| s["is_error"] == false).unwrap();
    let canonical = f
        .call(
            own,
            "thread_read",
            json!({
                "thread": "own", "view": "messages", "around": success["ref"],
                "before": 0, "after": 0
            }),
        )
        .unwrap();
    assert!(
        canonical.to_string().contains(&"x".repeat(2200)),
        "guarded reads must return canonical text, not the truncated index"
    );
    assert_eq!(
        count(
            &f.call(
                own,
                "thread_search",
                json!({
                    "query": "toolneedle", "kinds": ["tool_use"]
                })
            )
            .unwrap()
        ),
        1
    );
}

#[test]
fn short_literals_escaped_like_unicode_phrase_and_words_preserve_hits() {
    let f = Fixture::new();
    let own = f.own();
    f.native_run(
        "run-100",
        Some("own"),
        None,
        vec![user(vec![text(
            "日 東京 ab 100% a_b c\\d ÉCOLE Kelvin ſample alpha beta",
        )])],
    );
    f.index("run-100");
    for query in [
        "日", "東京", "ab", "%", "_", "\\", "a_b", "c\\d", "école", "kelvin", "sample",
    ] {
        let result = f
            .call(
                own,
                "thread_search",
                json!({"query": query, "kinds": ["conversation"]}),
            )
            .unwrap();
        assert_eq!(count(&result), 1, "{query}: {result}");
    }
    for args in [
        json!({"query": "alpha beta", "match": "phrase"}),
        json!({"query": "beta alpha", "match": "words"}),
        json!({"query": ["not present", "東京"], "match": "phrase"}),
    ] {
        assert_eq!(count(&f.call(own, "thread_search", args).unwrap()), 1);
    }
    assert_eq!(
        count(
            &f.call(
                own,
                "thread_search",
                json!({"query": "alpha beta", "match": "regex"})
            )
            .unwrap()
        ),
        1
    );
    assert_eq!(
        count(
            &f.call(
                own,
                "thread_search",
                json!({"query": "beta alpha", "match": "phrase"})
            )
            .unwrap()
        ),
        0
    );
}

#[test]
fn more_than_one_hundred_runs_and_include_children_are_not_truncated() {
    let f = Fixture::new();
    let own = f.own();
    for i in 0..101 {
        let run = format!("run-{}", 200 + i);
        f.native_run(
            &run,
            None,
            Some("run-100"),
            vec![user(vec![text("batchneedle")])],
        );
        f.index(&run);
    }
    let result = f
        .call(own, "thread_search", json!({"query": "batchneedle"}))
        .unwrap();
    assert_eq!(count(&result), 101);
    assert_eq!(result["index"]["indexed"], 101);
    assert_eq!(result["partial"], false);
    assert_eq!(
        count(
            &f.call(
                own,
                "thread_search",
                json!({
                    "query": "batchneedle", "include_children": false
                })
            )
            .unwrap()
        ),
        0
    );
}

#[test]
fn regex_scan_budget_continues_and_invisible_runs_do_not_change_any_output() {
    let f = Fixture::new();
    let own = f.own();
    let messages = (0..search::MAX_CANDIDATES + 3)
        .map(|i| user(vec![text(&format!("scan-{i}"))]))
        .collect();
    f.native_run("run-100", Some("own"), None, messages);
    f.index("run-100");
    let args = json!({"query": "^scan-", "match": "regex", "kinds": ["conversation"]});
    let first = f.call(own, "thread_search", args.clone()).unwrap();
    assert_eq!(count(&first), search::MAX_CANDIDATES as u64);
    assert_eq!(first["partial"], true);
    let no_hit = f
        .call(
            own,
            "thread_search",
            json!({
                "query": "^absent$", "match": "regex", "kinds": ["conversation"]
            }),
        )
        .unwrap();
    assert_eq!(count(&no_hit), 0);
    assert_eq!(no_hit["partial"], true);
    assert!(no_hit["next_cursor"].is_string());
    assert!(first["next_cursor"].as_str().unwrap().starts_with("ts2."));
    for i in 0..105 {
        let run = format!("run-{}", 1000 + i);
        let thread = format!("hidden-{i}");
        f.thread(&thread, Some(f.other.path()), &run, |row| {
            row.updated_at_ns = 999
        });
        f.native_run(
            &run,
            Some(&thread),
            None,
            vec![user(vec![text("scan-invisible")])],
        );
        f.index(&run);
    }
    assert_eq!(f.call(own, "thread_search", args.clone()).unwrap(), first);
    let mut next_args = args.clone();
    next_args["cursor"] = first["next_cursor"].clone();
    let second = f.call(own, "thread_search", next_args.clone()).unwrap();
    assert_eq!(count(&second), 3);
    assert_eq!(second["partial"], false);
    assert_eq!(second["next_cursor"], Value::Null);
    for (field, value) in [
        ("query", json!("changed")),
        ("match", json!("phrase")),
        ("kinds", json!(["tool_use"])),
        ("include_children", json!(false)),
        ("updated_after", json!(0)),
    ] {
        let mut changed = next_args.clone();
        changed[field] = value;
        assert!(
            f.call(own, "thread_search", changed)
                .unwrap_err()
                .starts_with("thread_cursor_mismatch")
        );
    }
    let token = first["next_cursor"].as_str().unwrap();
    let mut other_run = args.clone();
    other_run["cursor"] = json!(token);
    assert!(
        f.call(
            ("run-101", Some("own"), Some(f.project.path())),
            "thread_search",
            other_run
        )
        .unwrap_err()
        .starts_with("thread_cursor_invalid"),
        "a different invoking run cannot reuse a cursor even in the same thread/project"
    );
    let mut bytes = token.as_bytes().to_vec();
    bytes[10] = if bytes[10] == b'A' { b'B' } else { b'A' };
    next_args["cursor"] = json!(String::from_utf8(bytes).unwrap());
    assert!(
        f.call(own, "thread_search", next_args)
            .unwrap_err()
            .starts_with("thread_cursor_invalid")
    );
    // A changed canonical revision must invalidate a continuation.
    f.store()
        .handle
        .append_run_ledger("run-100", "new source revision")
        .unwrap();
    let mut stale = args;
    stale["cursor"] = json!(token);
    assert!(
        f.call(own, "thread_search", stale)
            .unwrap_err()
            .starts_with("thread_cursor_stale")
    );
}

#[test]
fn guarded_message_note_staleness_and_legacy_refs_use_the_canonical_source() {
    let f = Fixture::new();
    let own = f.own();
    f.native_run(
        "run-100",
        Some("own"),
        None,
        vec![user(vec![text("canonicalneedle")])],
    );
    f.index("run-100");
    let found = f
        .call(own, "thread_search", json!({"query": "canonicalneedle"}))
        .unwrap();
    let reference = found["threads"][0]["snippets"][0]["ref"].as_str().unwrap();
    let read_args = json!({"thread": "own", "view": "messages", "around": reference});
    let read = f.call(own, "thread_read", read_args.clone()).unwrap();
    assert!(read.to_string().contains("canonicalneedle"));
    f.store()
        .handle
        .append_run_ledger("run-100", "noteneedle")
        .unwrap();
    assert!(
        f.call(own, "thread_read", read_args)
            .unwrap_err()
            .starts_with("stale_reference")
    );
    let stale = f
        .call(
            own,
            "thread_search",
            json!({"query": "canonicalneedle", "kinds": ["conversation"]}),
        )
        .unwrap();
    assert_eq!(count(&stale), 0);
    assert_eq!(stale["index"]["indexed"], 0);
    assert_eq!(stale["index"]["pending"], 1);
    assert!(
        f.call(
            own,
            "thread_read",
            json!({
                "thread": "own", "view": "messages", "around": "run-100#0.0"
            })
        )
        .unwrap()
        .to_string()
        .contains("canonicalneedle")
    );
    // grep remains a canonical scan even while the index is pending.
    assert!(
        !f.call(
            own,
            "thread_grep",
            json!({
                "thread": "own", "pattern": "canonicalneedle"
            })
        )
        .unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let note = f
        .call(
            own,
            "thread_search",
            json!({
                "query": "noteneedle", "kinds": ["working_note"]
            }),
        )
        .unwrap();
    let note_ref = note["threads"][0]["snippets"][0]["ref"].as_str().unwrap();
    let note_args = json!({"thread": "own", "view": "messages", "around": note_ref});
    assert!(
        f.call(own, "thread_read", note_args.clone())
            .unwrap()
            .to_string()
            .contains("noteneedle")
    );
    f.store().handle.append_run_ledger("run-100", "").unwrap();
    assert!(
        f.call(own, "thread_read", note_args)
            .unwrap_err()
            .starts_with("stale_reference")
    );
}

#[test]
fn note_only_refs_and_deleted_sources_stay_authorized_without_a_snapshot() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("notes", Some(f.project.path()), "run-250", |_| {});
    f.store()
        .handle
        .append_run_ledger("run-250", "noteonlyneedle")
        .unwrap();
    let found = f
        .call(
            own,
            "thread_search",
            json!({
                "query": "noteonlyneedle", "kinds": ["working_note"]
            }),
        )
        .unwrap();
    let reference = found["threads"][0]["snippets"][0]["ref"].as_str().unwrap();
    let args = json!({"thread": "notes", "view": "messages", "around": reference});
    let note = f.call(own, "thread_read", args.clone()).unwrap();
    assert_eq!(note["view"], "working_note");
    assert_eq!(note["body"], "noteonlyneedle");
    assert!(
        f.call(
            own,
            "thread_read",
            json!({
                "thread": "notes", "view": "messages", "around": "run-250#0.0"
            })
        )
        .is_err(),
        "legacy message refs must still require a saved snapshot"
    );
    let conn = Connection::open(&f.store().db_path).unwrap();
    conn.execute(
        "UPDATE thread_index_runs SET state='deleted', source_present=0, \
         source_revision=source_revision+1 WHERE run_id='run-250'",
        [],
    )
    .unwrap();
    assert!(
        f.call(own, "thread_read", args.clone())
            .unwrap_err()
            .starts_with("source_deleted")
    );
    f.thread("notes", Some(f.other.path()), "run-250", |_| {});
    let hidden = f.call(own, "thread_read", args.clone()).unwrap_err();
    let mut missing = args;
    missing["thread"] = json!("missing");
    assert_eq!(hidden, denial("notes"));
    assert_eq!(
        f.call(own, "thread_read", missing).unwrap_err(),
        denial("missing")
    );
}

#[test]
fn search_output_pages_at_sixteen_kib_without_losing_threads() {
    let f = Fixture::new();
    let own = f.own();
    let long = format!("need {}", "🙂".repeat(300));
    for i in 0..20 {
        let thread = format!("page-{i:02}");
        let run = format!("run-{}", 200 + i);
        f.thread(&thread, Some(f.project.path()), &run, |row| {
            row.title = Some(long.clone());
            row.updated_at_ns = 100 + i;
        });
        f.native_run(&run, Some(&thread), None, vec![user(vec![text(&long); 3])]);
        f.index(&run);
    }
    let mut args = json!({"query": "need", "limit": 20});
    let mut seen = std::collections::HashSet::new();
    let mut pages = 0;
    loop {
        let result = f.call(own, "thread_search", args.clone()).unwrap();
        assert!(result.to_string().len() <= MAX_OUTPUT_BYTES);
        for thread in result["threads"].as_array().unwrap() {
            assert!(seen.insert(thread["thread_id"].as_str().unwrap().to_owned()));
        }
        pages += 1;
        assert!(pages < 10, "pagination must progress");
        if result["next_cursor"].is_null() {
            break;
        }
        args["cursor"] = result["next_cursor"].clone();
    }
    assert!(pages > 1, "fixture must actually exercise the output bound");
    assert_eq!(seen.len(), 20);
}
