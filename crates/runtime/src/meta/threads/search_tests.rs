//! Phase B search tests use explicitly native fixtures and the shared extractor.
//! Index rows are installed synchronously, without a background owner or sleeps.
use super::super::conversation::{CHUNK_BLOCKS, CHUNK_BYTES, ScanStats};
use super::super::cursor;
use super::search::{MatchMode, Matcher, SearchKind, SearchLimits};
use super::*;
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, BTreeSet};
use storage::thread_index::{BlockLocator, IndexBlockKind, extract_snapshot_bundle};
use storage::thread_search::{ConversationFilter, ConversationQuery, MAX_SEARCH_BATCH_RUNS};

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

type Who<'a> = (&'a str, Option<&'a str>, Option<&'a Path>);

impl Fixture {
    fn scope(&self, who: Who<'_>) -> Scope<'_> {
        let caller = Caller::new(who.0.parse().unwrap(), who.1.map(Into::into), who.2);
        Scope::new(
            self.store(),
            &self.runtime,
            caller,
            SecretRedactor::with_known_values([SECRET.into()]),
        )
    }

    /// One `thread_search` call under explicit work limits.
    fn search_limited(
        &self,
        who: Who<'_>,
        input: Value,
        limits: SearchLimits,
    ) -> Result<(Value, ScanStats), String> {
        let scope = self.scope(who);
        let (page, stats) =
            search::run_with_limits(&scope, serde_json::from_value(input).unwrap(), limits)?;
        assert_eq!(page["note"], UNTRUSTED_NOTE);
        assert!(page.to_string().len() <= MAX_OUTPUT_BYTES);
        Ok((page, stats))
    }

    /// Follow `next_cursor` until the search ends.
    fn drain(
        &self,
        who: Who<'_>,
        mut args: Value,
        limits: SearchLimits,
    ) -> Vec<(Value, ScanStats)> {
        let mut pages = Vec::new();
        loop {
            let (page, stats) = self.search_limited(who, args.clone(), limits).unwrap();
            let next = page["next_cursor"].clone();
            pages.push((page, stats));
            if next.is_null() {
                return pages;
            }
            assert!(pages.len() < 5000, "the cursor does not make progress");
            args["cursor"] = next;
        }
    }

    fn max_block_bytes(&self) -> usize {
        Connection::open(&self.store().db_path)
            .unwrap()
            .query_row(
                "SELECT COALESCE(MAX(length(CAST(text AS BLOB))), 0) FROM conversation_blocks",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap() as usize
    }
}

const fn limits(max_blocks: usize, max_bytes: usize) -> SearchLimits {
    SearchLimits {
        max_blocks,
        max_bytes,
    }
}

/// The deterministic work bounds of one call (#374), derived from how the
/// stream asks storage for chunks; `max_block_bytes` is the largest stored block.
///
/// - Charged work: a block is charged only while the budget is not exhausted,
///   so blocks never exceed `max_blocks`, and bytes stay below `max_bytes`
///   before the last block, which adds at most one block.
/// - Fetched work: a chunk is requested only with nothing buffered, so before
///   it everything fetched was charged. What is left over is the unprocessed
///   tail of the last chunk (below `CHUNK_*` plus the block that crossed it),
///   or, once the budget is spent, the single block read ahead for the cut,
///   whose text is fetched too. Text blocks have no size limit, so that
///   read-ahead is up to `max_block_bytes` on top of the charged bytes.
/// - Statements: all but the last chunk of each kind were charged in full, so
///   block-limited chunks number at most `max_blocks / CHUNK_BLOCKS + 1`,
///   byte-limited ones at most `max_bytes / CHUNK_BYTES + 1`; each batch ends
///   with at most one chunk that finds no further row, and the cut reads one
///   block ahead.
fn assert_scan_bounds(stats: ScanStats, limits: SearchLimits, max_block_bytes: usize, runs: usize) {
    let batches = runs.div_ceil(MAX_SEARCH_BATCH_RUNS);
    let context = format!("{stats:?} {limits:?} max_block_bytes={max_block_bytes}");
    assert!(stats.charged_blocks <= limits.max_blocks, "{context}");
    assert!(
        stats.charged_bytes < limits.max_bytes + max_block_bytes,
        "{context}"
    );
    assert!(stats.fetched_blocks >= stats.charged_blocks, "{context}");
    assert!(stats.fetched_bytes >= stats.charged_bytes, "{context}");
    assert!(
        stats.fetched_blocks <= stats.charged_blocks + CHUNK_BLOCKS,
        "{context}"
    );
    assert!(
        stats.fetched_bytes <= stats.charged_bytes + CHUNK_BYTES + max_block_bytes,
        "{context}"
    );
    assert!(
        stats.statements
            <= limits.max_blocks / CHUNK_BLOCKS + limits.max_bytes / CHUNK_BYTES + batches + 3,
        "{context}"
    );
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
fn obsolete_extractor_preserves_report_fallback_but_hides_conversation_hits_and_counts() {
    let f = Fixture::new();
    let own = f.own();
    f.native_run(
        "run-100",
        Some("own"),
        None,
        vec![
            assistant(vec![text("oldextractorneedle intermediate")]),
            assistant(vec![text("oldextractorneedle final")]),
        ],
    );
    f.index("run-100");
    let conn = Connection::open(&f.store().db_path).unwrap();
    conn.execute("UPDATE thread_index_runs SET extractor_version = 0, indexed_extractor_version = 0 WHERE run_id = 'run-100'", []).unwrap();
    let result = f
        .call(
            own,
            "thread_search",
            json!({
                "query": "oldextractorneedle", "kinds": ["conversation"]
            }),
        )
        .unwrap();
    assert_eq!(count(&result), 0, "{result}");
    assert_eq!(result["index"]["indexed"], 0);
    assert_eq!(result["index"]["pending"], 1);
    let result = f
        .call(
            own,
            "thread_search",
            json!({
                "query": "oldextractorneedle", "kinds": ["conversation", "final_report"]
            }),
        )
        .unwrap();
    assert_eq!(
        count(&result),
        1,
        "only metadata fallback, not old extracted text: {result}"
    );
    assert_eq!(result["threads"][0]["snippets"][0]["kind"], "final_report");
}

#[test]
fn extractor_currentness_change_makes_a_conversation_cursor_stale() {
    let f = Fixture::new();
    let own = f.own();
    f.native_run(
        "run-100",
        Some("own"),
        None,
        (0..2001)
            .map(|_| assistant(vec![text("versioncursorneedle")]))
            .collect(),
    );
    f.index("run-100");
    let mut args = json!({"query": "versioncursorneedle", "kinds": ["conversation"]});
    let page = f.call(own, "thread_search", args.clone()).unwrap();
    assert!(!page["next_cursor"].is_null());
    args["cursor"] = page["next_cursor"].clone();
    let conn = Connection::open(&f.store().db_path).unwrap();
    conn.execute(
        "UPDATE thread_index_runs SET indexed_extractor_version = 0 WHERE run_id = 'run-100'",
        [],
    )
    .unwrap();
    let error = f.call(own, "thread_search", args).unwrap_err();
    assert!(error.contains("thread_cursor_stale"), "{error}");
}

#[tokio::test]
async fn same_thread_continuation_searches_original_native_positions_after_restart() {
    let f = Fixture::with_script(vec![
        reply(
            vec![
                text("oldmidneedle"),
                ContentBlock::ToolUse {
                    id: "inspect-history".into(),
                    name: "thread_search".into(),
                    input: json!({"query": "no-match", "kinds": ["title"]}),
                },
            ],
            FinishReason::ToolUse,
        ),
        reply(vec![text("old final")], FinishReason::Stop),
        reply(vec![text("next final")], FinishReason::Stop),
    ]);
    let config = RunConfig {
        project_root: Some(f.project.path().into()),
        ..Default::default()
    };
    let first = f
        .runtime
        .delegate_chat("history", Role::Worker, "first".into(), config.clone())
        .unwrap();
    f.runtime.wait(first).await.unwrap();
    f.index(&first.to_string());
    let second = f
        .runtime
        .delegate_chat("history", Role::Worker, "second".into(), config.clone())
        .unwrap();
    f.runtime.wait(second).await.unwrap();
    f.index(&second.to_string());
    let own = f.own();
    for include_children in [false, true] {
        let result = f.call(own, "thread_search", json!({
            "query": "oldmidneedle", "kinds": ["conversation"], "include_children": include_children
        })).unwrap();
        assert_eq!(count(&result), 1, "{result}");
        assert!(
            result["threads"][0]["snippets"][0]["ref"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{first}#"))
        );
    }

    // Recreate both runtime and storage, using only the persisted continuation.
    let storage_config = storage::StorageConfig {
        db_path: f.store().db_path.clone(),
        ..Default::default()
    };
    let Fixture {
        runtime,
        _storage,
        _db,
        project,
        other,
    } = f;
    drop(runtime);
    drop(_storage);
    let bus = Arc::new(event_bus::EventBus::new(4096));
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(ScriptModel(Mutex::new(VecDeque::new()))),
    )
    .with_run_store(RunStore::open(&storage_config, storage.handle()).unwrap());
    let f = Fixture {
        runtime,
        _storage: storage,
        _db,
        project,
        other,
    };
    let third = f
        .runtime
        .delegate_chat("history", Role::Worker, "third".into(), config)
        .unwrap();
    f.runtime.wait(third).await.unwrap();
    f.index(&third.to_string());
    let result = f
        .call(
            ("run-100", Some("own"), Some(f.project.path())),
            "thread_search",
            json!({"query": "oldmidneedle", "kinds": ["conversation"]}),
        )
        .unwrap();
    assert_eq!(count(&result), 1, "{result}");
    assert!(
        result["threads"][0]["snippets"][0]["ref"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{first}#"))
    );
    let denied = f
        .call(
            ("run-100", Some("own"), Some(f.other.path())),
            "thread_search",
            json!({"query": "oldmidneedle", "kinds": ["conversation"]}),
        )
        .unwrap();
    assert_eq!(
        count(&denied),
        0,
        "history is not in the foreign caller's project"
    );
    let conn = Connection::open(&f.store().db_path).unwrap();
    conn.execute(
        "UPDATE thread_index_runs SET state = 'evicted', evicted_reason = 'logical_budget' WHERE run_id = ?1",
        [first.to_string()],
    )
    .unwrap();
    let evicted = f
        .call(
            ("run-100", Some("own"), Some(f.project.path())),
            "thread_search",
            json!({"query": "oldmidneedle", "kinds": ["conversation"]}),
        )
        .unwrap();
    assert_eq!(
        count(&evicted),
        0,
        "copies must not resurrect an evicted original"
    );
}

#[test]
fn final_report_after_candidate_budget_is_counted_once_across_cursor_drain() {
    let f = Fixture::new();
    let own = f.own();
    let mut messages: Vec<_> = (0..search::MAX_SCANNED_BLOCKS)
        .map(|ordinal| user(vec![text(&format!("boundaryneedle {ordinal}"))]))
        .collect();
    messages.push(assistant(vec![text(&format!(
        "boundaryneedle {}",
        search::MAX_SCANNED_BLOCKS
    ))]));
    f.native_run("run-100", Some("own"), None, messages);
    f.index("run-100");
    let caller = Caller::new(
        "run-100".parse().unwrap(),
        Some("own".into()),
        Some(f.project.path()),
    );
    let scope = Scope::new(
        f.store(),
        &f.runtime,
        caller,
        SecretRedactor::with_known_values([SECRET.into()]),
    );
    let mut stream = super::super::conversation::Stream::new(
        &scope,
        vec!["run-100".into()],
        vec![storage::thread_index::IndexBlockKind::Text],
        None,
    );
    let mut positions = std::collections::BTreeSet::new();
    while stream.peek(usize::MAX, usize::MAX).unwrap().is_some() {
        let (_, candidate) = stream.pop().unwrap();
        assert!(positions.insert(match candidate.locator {
            BlockLocator::Message {
                ordinal,
                block_index,
            } => (ordinal, block_index),
            _ => panic!("unexpected note"),
        }));
    }
    assert_eq!(
        positions,
        (0..=search::MAX_SCANNED_BLOCKS)
            .map(|ordinal| (ordinal, 0))
            .collect()
    );
    for kinds in [None, Some(json!(["conversation", "final_report"]))] {
        let mut args = json!({"query": "boundaryneedle"});
        if let Some(kinds) = kinds {
            args["kinds"] = kinds;
        }
        let mut total = 0;
        let mut pages = 0;
        let mut last_ref = None;
        loop {
            let page = f.call(own, "thread_search", args.clone()).unwrap();
            total += count(&page);
            pages += 1;
            if let Some(thread) = page["threads"].as_array().unwrap().first() {
                last_ref = thread["snippets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|s| s["ref"].as_str().map(str::to_owned))
                    .next_back();
            }
            let Some(cursor) = page["next_cursor"].as_str() else {
                break;
            };
            assert!(pages < 4, "{page}");
            args["cursor"] = json!(cursor);
        }
        assert_eq!(total, search::MAX_SCANNED_BLOCKS as u64 + 1);
        assert_eq!(pages, 2);
        assert!(
            last_ref
                .unwrap()
                .starts_with(&format!("run-100#{}.0@", search::MAX_SCANNED_BLOCKS))
        );
    }
    // Regex visits every candidate but matches only three on the first page,
    // making the entire returned position union observable despite snippet bounds.
    let mut args = json!({
        "query": "^boundaryneedle (?:0|1998|1999|2000)$", "match": "regex",
        "kinds": ["conversation", "final_report"]
    });
    let mut actual = std::collections::BTreeSet::new();
    let mut total = 0;
    let mut pages = 0;
    loop {
        let page = f.call(own, "thread_search", args.clone()).unwrap();
        total += count(&page);
        pages += 1;
        for snippet in page["threads"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|thread| thread["snippets"].as_array().unwrap())
        {
            assert!(
                actual.insert(
                    snippet["ref"]
                        .as_str()
                        .unwrap()
                        .split('@')
                        .next()
                        .unwrap()
                        .to_owned()
                )
            );
        }
        let Some(cursor) = page["next_cursor"].as_str() else {
            break;
        };
        assert!(pages < 4);
        args["cursor"] = json!(cursor);
    }
    assert_eq!(total, 4);
    assert_eq!(pages, 2);
    assert_eq!(
        actual,
        [0, 1998, 1999, 2000]
            .map(|ordinal| format!("run-100#{ordinal}.0"))
            .into_iter()
            .collect()
    );
}

#[test]
fn report_ownership_keeps_fallback_and_distinct_native_positions() {
    let f = Fixture::new();
    let own = f.own();
    f.native_run(
        "run-100",
        Some("own"),
        None,
        vec![
            assistant(vec![text("sameutterance")]),
            user(vec![text("other")]),
            assistant(vec![text("sameutterance"), text("sameutterance")]),
        ],
    );
    let args = json!({"query": "sameutterance", "kinds": ["conversation", "final_report"]});
    assert_eq!(
        count(&f.call(own, "thread_search", args.clone()).unwrap()),
        1,
        "pending fallback"
    );
    f.index("run-100");
    assert_eq!(
        count(
            &f.call(
                own,
                "thread_search",
                json!({
                    "query": "sameutterance", "kinds": ["tool_result", "final_report"]
                })
            )
            .unwrap()
        ),
        1,
        "a tool-only stream cannot own the final text position"
    );
    let indexed = f.call(own, "thread_search", args.clone()).unwrap();
    assert_eq!(
        count(&indexed),
        3,
        "same text at three native positions is three hits"
    );
    let positions: std::collections::BTreeSet<_> = indexed["threads"][0]["snippets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snippet| snippet["ref"].as_str().unwrap().split('@').next().unwrap())
        .collect();
    assert_eq!(
        positions,
        ["run-100#0.0", "run-100#2.0", "run-100#2.1"]
            .into_iter()
            .collect()
    );
    let conn = Connection::open(&f.store().db_path).unwrap();
    conn.execute(
        "UPDATE thread_index_runs SET state = 'evicted', evicted_reason = 'logical_budget' WHERE run_id = 'run-100'",
        [],
    )
    .unwrap();
    assert_eq!(
        count(&f.call(own, "thread_search", args).unwrap()),
        1,
        "evicted metadata fallback"
    );
}

#[test]
fn report_on_a_later_run_invalidates_cursor_when_its_revision_changes() {
    let f = Fixture::new();
    let own = f.own();
    f.native_run(
        "run-100",
        Some("own"),
        None,
        vec![user(vec![text("laterreport")]); search::MAX_SCANNED_BLOCKS + 1],
    );
    f.native_run(
        "run-101",
        Some("own"),
        None,
        vec![assistant(vec![text("laterreport")])],
    );
    f.index("run-100");
    f.index("run-101");
    let first = f
        .call(
            own,
            "thread_search",
            json!({
                "query": "laterreport", "kinds": ["conversation", "final_report"]
            }),
        )
        .unwrap();
    assert_eq!(count(&first), search::MAX_SCANNED_BLOCKS as u64);
    let conn = Connection::open(&f.store().db_path).unwrap();
    conn.execute("UPDATE thread_index_runs SET source_revision = source_revision + 1 WHERE run_id = 'run-101'", []).unwrap();
    let error = f.call(own, "thread_search", json!({
        "query": "laterreport", "kinds": ["conversation", "final_report"], "cursor": first["next_cursor"]
    })).unwrap_err();
    assert!(error.starts_with("thread_cursor_stale:"), "{error}");
}

#[test]
fn header_only_search_does_not_decode_saved_payloads() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("headers", Some(f.project.path()), "run-201", |_| {});
    f.run(
        "run-201",
        Some("headers"),
        None,
        vec![assistant(vec![text("report")])],
    );
    let conn = Connection::open(&f.store().db_path).unwrap();
    conn.execute(
        "UPDATE run_contexts SET messages_json = x'ff', config_json = x'ff', \
         checkpoints_json = x'ff' WHERE run_id = 'run-201'",
        [],
    )
    .unwrap();
    for args in [json!({"query": "headers", "kinds": ["title"]}), json!({})] {
        let result = f.call(own, "thread_search", args).unwrap();
        assert!(result["threads"].as_array().unwrap().iter().any(|thread| {
            thread["thread_id"] == "headers"
                && thread["status"]["root_run_id"] == "run-201"
                && thread["status"]["phase"] == "Done"
        }));
    }
}

#[test]
fn cursor_drain_across_one_hundred_one_runs_matches_canonical_counts() {
    let f = Fixture::new();
    let own = f.own();
    let mut expected = 0;
    for i in 0..101 {
        let run = format!("run-{}", 200 + i);
        let mut messages = vec![user(vec![text("cursorneedle")]); 21];
        messages.insert(
            0,
            Message {
                role: MessageRole::System,
                content: vec![text("cursorneedle hidden")],
            },
        );
        f.native_run(&run, None, Some("run-100"), messages);
        let record = f
            .store()
            .with_database(|db| db.run_context(&run))
            .unwrap()
            .unwrap();
        // The oracle reads canonical snapshots, not index candidates.
        let normalized = super::super::normalize::normalize(
            &record,
            &SecretRedactor::with_known_values([SECRET.into()]),
        )
        .unwrap();
        expected += normalized
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter(|block| block.text.contains("cursorneedle"))
            .count() as u64;
        f.index(&run);
    }
    assert_eq!(expected, 101 * 21);
    let mut args = json!({"query": "cursorneedle", "kinds": ["conversation"]});
    let mut actual = 0;
    let mut pages = 0;
    let mut cursors = std::collections::HashSet::new();
    loop {
        let result = f.call(own, "thread_search", args.clone()).unwrap();
        actual += count(&result);
        pages += 1;
        if result["next_cursor"].is_null() {
            break;
        }
        assert_eq!(result["partial"], true);
        assert!(cursors.insert(result["next_cursor"].as_str().unwrap().to_owned()));
        assert!(pages < 10);
        args["cursor"] = result["next_cursor"].clone();
    }
    assert!(pages > 1);
    assert_eq!(actual, expected);
}

#[test]
fn narrow_search_members_preserve_primary_and_saved_state() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("headers", Some(f.project.path()), "run-201", |_| {});
    for (run, thread, parent) in [
        ("run-201", Some("headers"), None),
        ("run-202", Some("headers"), None),
        ("run-203", None, Some("run-201")),
    ] {
        f.run(run, thread, parent, vec![assistant(vec![text("result")])]);
    }
    let conn = Connection::open(&f.store().db_path).unwrap();
    // A tie uses the same run-ID ordering as full summaries. A newer child
    // never replaces the latest root; a missing recorded root remains fallback.
    conn.execute(
        "UPDATE run_contexts SET updated_at_ns = CASE WHEN run_id = 'run-203' \
         THEN 99 ELSE 10 END, terminal_phase = 'Checkpoint' \
         WHERE run_id IN ('run-201', 'run-202', 'run-203')",
        [],
    )
    .unwrap();
    let scope = Scope::new(
        f.store(),
        &f.runtime,
        Caller::new(own.0.parse().unwrap(), own.1.map(Into::into), own.2),
        SecretRedactor::with_known_values([SECRET.into()]),
    );
    let row = scope.resolve("headers").unwrap();
    let narrow = scope.search_members(&row).unwrap();
    let full = scope.members(&row).unwrap();
    assert_eq!(narrow.primary.as_deref(), Some("run-202"));
    assert_eq!(narrow.primary, full.primary);
    assert_eq!(narrow.headers, full.headers);
    assert!(narrow.summaries.is_empty());
    for run in &narrow.runs {
        assert_eq!(
            narrow.is_saved_member(&run.run_id),
            full.is_saved_member(&run.run_id)
        );
    }
    let output = f
        .call(
            own,
            "thread_search",
            json!({"query": "headers", "kinds": ["title"]}),
        )
        .unwrap();
    assert_eq!(output["threads"][0]["status"]["root_run_id"], "run-202");
    assert_eq!(output["threads"][0]["status"]["phase"], "Checkpoint");

    f.thread("unsaved", Some(f.project.path()), "run-299", |_| {});
    let row = scope.resolve("unsaved").unwrap();
    let narrow = scope.search_members(&row).unwrap();
    assert_eq!(narrow.primary.as_deref(), Some("run-299"));
    assert!(narrow.headers.is_empty());
}

#[test]
fn lazy_final_reports_keep_full_text_all_roots_and_child_filtering() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("reports", Some(f.project.path()), "run-201", |_| {});
    let long = format!("{} tailneedle", "padding ".repeat(2048));
    f.run(
        "run-201",
        Some("reports"),
        None,
        vec![assistant(vec![text(&long)])],
    );
    f.run(
        "run-202",
        Some("reports"),
        None,
        vec![assistant(vec![text("latest root")])],
    );
    f.run(
        "run-203",
        None,
        Some("run-201"),
        vec![assistant(vec![text("childneedle")])],
    );
    for (query, children, run) in [
        ("tailneedle", false, Some("run-201")),
        ("childneedle", false, None),
        ("childneedle", true, Some("run-203")),
    ] {
        let result = f
            .call(
                own,
                "thread_search",
                json!({
                    "query": query, "kinds": ["final_report"],
                    "include_children": children
                }),
            )
            .unwrap();
        if let Some(run) = run {
            assert_eq!(result["threads"][0]["matched_runs"], json!([run]));
            assert_eq!(result["threads"][0]["hit_count"], 1);
            assert_eq!(result["threads"][0]["snippets"][0]["kind"], "final_report");
        } else {
            assert_eq!(result["threads"], json!([]));
        }
    }
    // A later request must see the new phase and payload: no cross-page cache.
    let mut record = f
        .store()
        .with_database(|db| db.run_context("run-201"))
        .unwrap()
        .unwrap();
    record.terminal_phase = "Error".into();
    f.store().handle.upsert_run_context(&record).unwrap();
    let result = f
        .call(
            own,
            "thread_search",
            json!({"query": "tailneedle", "kinds": ["final_report"]}),
        )
        .unwrap();
    assert_eq!(result["threads"], json!([]));
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
    let messages = (0..search::MAX_SCANNED_BLOCKS + 3)
        .map(|i| user(vec![text(&format!("scan-{i}"))]))
        .collect();
    f.native_run("run-100", Some("own"), None, messages);
    f.index("run-100");
    let args = json!({"query": "^scan-", "match": "regex", "kinds": ["conversation"]});
    let first = f.call(own, "thread_search", args.clone()).unwrap();
    assert_eq!(count(&first), search::MAX_SCANNED_BLOCKS as u64);
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
    // Literal and phrase queries examine every block too, so they are cut and
    // continue at the same places.
    let literal_args = json!({"query": "scan", "kinds": ["conversation"]});
    let phrase_args = json!({"query": "scan-1", "match": "phrase", "kinds": ["conversation"]});
    let literal_first = f.call(own, "thread_search", literal_args.clone()).unwrap();
    let phrase_first = f.call(own, "thread_search", phrase_args.clone()).unwrap();
    assert_eq!(count(&literal_first), search::MAX_SCANNED_BLOCKS as u64);
    assert_eq!(literal_first["partial"], true);
    assert_eq!(phrase_first["partial"], true);
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
    assert_eq!(
        f.call(own, "thread_search", literal_args).unwrap(),
        literal_first
    );
    assert_eq!(
        f.call(own, "thread_search", phrase_args).unwrap(),
        phrase_first
    );
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

fn host_ids(search: &crate::HostThreadSearch) -> Vec<&str> {
    let mut ids: Vec<_> = search
        .hits
        .iter()
        .map(|hit| hit.thread_id.as_str())
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn host_search_finds_titles_and_bodies_of_the_project_only() {
    // Given: threads in the open project and one in another project, each body indexed.
    let f = Fixture::new();
    f.thread("alpha", Some(f.project.path()), "run-201", |_| {});
    f.thread("beta", Some(f.project.path()), "run-202", |row| {
        row.title = Some("unrelated".into());
    });
    f.thread("outside", Some(f.other.path()), "run-203", |row| {
        row.title = Some("title of outside".into());
    });
    for (run, thread, body) in [
        ("run-201", "alpha", "nothing special"),
        ("run-202", "beta", "the hostneedle is here"),
        ("run-203", "outside", "an outside hostneedle"),
    ] {
        f.native_run(run, Some(thread), None, vec![assistant(vec![text(body)])]);
        f.index(run);
    }
    let search = |query: &str| {
        f.runtime
            .search_threads_for_host(f.project.path(), query, 20, None)
            .unwrap()
    };
    // When / Then: a title query finds only the project's thread, with its title as the snippet.
    let by_title = search("title of");
    assert_eq!(host_ids(&by_title), ["alpha"]);
    assert_eq!(by_title.hits[0].snippet_kind.as_deref(), Some("title"));
    // A body query finds the thread whose conversation holds the text.
    let by_body = search("hostneedle");
    assert_eq!(host_ids(&by_body), ["beta"]);
    assert_eq!(
        by_body.hits[0].snippet_kind.as_deref(),
        Some("conversation")
    );
    assert!(
        by_body.hits[0]
            .snippet
            .as_deref()
            .is_some_and(|snippet| snippet.contains("hostneedle"))
    );
    assert_eq!(by_body.index.unindexed, 0);
    // A blank query lists the project's threads without snippets.
    let listing = search("   ");
    assert_eq!(host_ids(&listing), ["alpha", "beta"]);
    assert!(listing.hits.iter().all(|hit| hit.snippet.is_none()));
}

#[test]
fn host_search_reports_unindexed_bodies_instead_of_hiding_them() {
    // Given: a project thread whose body was never indexed.
    let f = Fixture::new();
    f.thread("alpha", Some(f.project.path()), "run-201", |_| {});
    f.native_run(
        "run-201",
        Some("alpha"),
        None,
        vec![
            assistant(vec![text("unindexedneedle intermediate")]),
            assistant(vec![text("a different final report")]),
        ],
    );
    // When: searching text outside the final report, which only the index holds.
    let search = f
        .runtime
        .search_threads_for_host(f.project.path(), "unindexedneedle", 20, None)
        .unwrap();
    // Then: no body hit, and the gap in coverage is reported.
    assert!(search.hits.is_empty());
    assert!(search.index.unindexed > 0);
}

#[test]
fn host_search_rejects_a_project_folder_that_does_not_exist() {
    let f = Fixture::new();
    let missing = f.project.path().join("gone");
    assert!(
        f.runtime
            .search_threads_for_host(&missing, "x", 5, None)
            .is_err()
    );
}

#[test]
fn host_search_ranks_fuzzy_title_matches_ahead_of_body_matches() {
    // Given: titles that contain "pg" literally, by scattered letters or not at all,
    // and a thread whose body (not its title) holds it.
    let f = Fixture::new();
    for (id, title, run, at) in [
        ("exact", "Fix pg retries", "run-201", 1),
        ("scattered", "Payment gateway rewrite", "run-202", 5),
        ("none", "Unrelated chat", "run-203", 9),
        ("body", "Notes", "run-204", 7),
    ] {
        f.thread(id, Some(f.project.path()), run, |row| {
            row.title = Some(title.into());
            row.updated_at_ns = at;
        });
        let body = if id == "body" {
            "we talked about pg"
        } else {
            "nothing"
        };
        f.native_run(run, Some(id), None, vec![assistant(vec![text(body)])]);
        f.index(run);
    }
    let search = f
        .runtime
        .search_threads_for_host(f.project.path(), "pg", 20, None)
        .unwrap();
    // Then: the literal title leads, the fuzzy (subsequence) title follows, the body-only
    // thread comes last and the unrelated one is absent.
    let ids: Vec<_> = search
        .hits
        .iter()
        .map(|hit| hit.thread_id.as_str())
        .collect();
    assert_eq!(ids, ["exact", "scattered", "body"]);
    assert_eq!(search.hits[1].snippet_kind.as_deref(), Some("title"));
    // Matching is case-insensitive and spans non-adjacent letters.
    let fuzzy = f
        .runtime
        .search_threads_for_host(f.project.path(), "PMGW", 20, None)
        .unwrap();
    assert_eq!(host_ids(&fuzzy), ["scattered"]);
}

#[test]
fn host_search_cursor_reaches_a_match_beyond_the_first_hundred_threads() {
    // Given: 101 project threads, the oldest holding the only body match.
    let f = Fixture::new();
    for index in 0..=100_i64 {
        let id = format!("t{index:03}");
        let run = format!("run-{}", 1000 + index);
        f.thread(&id, Some(f.project.path()), &run, |row| {
            row.updated_at_ns = 10 + index;
        });
    }
    f.native_run(
        "run-1000",
        Some("t000"),
        None,
        vec![
            assistant(vec![text("tailneedle early body")]),
            assistant(vec![text("final")]),
        ],
    );
    f.index("run-1000");
    // When: searching, the first page examines only the 100 newest threads.
    let first = f
        .runtime
        .search_threads_for_host(f.project.path(), "tailneedle", 20, None)
        .unwrap();
    // Then: it is empty but not exhausted, and the cursor leads to the match.
    assert!(first.hits.is_empty());
    let cursor = first.next_cursor.expect("unexamined threads remain");
    let second = f
        .runtime
        .search_threads_for_host(f.project.path(), "tailneedle", 20, Some(&cursor))
        .unwrap();
    assert_eq!(host_ids(&second), ["t000"]);
    assert!(second.next_cursor.is_none());
}

#[test]
fn host_search_surfaces_the_cursor_of_a_page_cut_by_the_body_budget() {
    // Given: one thread with more matching blocks than a page may verify.
    let f = Fixture::new();
    f.thread("alpha", Some(f.project.path()), "run-201", |_| {});
    f.native_run(
        "run-201",
        Some("alpha"),
        None,
        (0..2001)
            .map(|_| assistant(vec![text("budgetneedle")]))
            .collect(),
    );
    f.index("run-201");
    let first = f
        .runtime
        .search_threads_for_host(f.project.path(), "budgetneedle", 20, None)
        .unwrap();
    // Then: the cut is reported rather than treated as the end of the search.
    let cursor = first.next_cursor.expect("the page was cut");
    let rest = f
        .runtime
        .search_threads_for_host(f.project.path(), "budgetneedle", 20, Some(&cursor))
        .unwrap();
    assert!(rest.next_cursor.is_none());
}

fn many_threads_with_titles(f: &Fixture, titles: &[&str]) {
    for (index, title) in titles.iter().enumerate() {
        let id = format!("t{index:03}");
        f.thread(
            &id,
            Some(f.project.path()),
            &format!("run-{}", 3000 + index),
            |row| {
                row.title = Some((*title).into());
                row.updated_at_ns = 10 + index as i64;
            },
        );
    }
}

#[test]
fn host_search_never_returns_more_than_the_limit_even_with_fuzzy_titles() {
    // Given: three titles that match "pg" by scattered letters, and a body-only thread.
    let f = Fixture::new();
    many_threads_with_titles(
        &f,
        &["Payment gateway", "Pipeline graph", "Page group", "Notes"],
    );
    f.native_run(
        "run-3003",
        Some("t003"),
        None,
        vec![
            assistant(vec![text("pg in the body")]),
            assistant(vec![text("done")]),
        ],
    );
    f.index("run-3003");
    let search = |limit, cursor: Option<&str>| {
        f.runtime
            .search_threads_for_host(f.project.path(), "pg", limit, cursor)
            .unwrap()
    };
    // When: following the cursors with a page of one.
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = search(1, cursor.as_deref());
        // Then: no page exceeds the limit, and every match is reached exactly once.
        assert!(page.hits.len() <= 1, "{:?}", page.hits);
        seen.extend(page.hits.iter().map(|hit| hit.thread_id.clone()));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    seen.sort();
    assert_eq!(seen, ["t000", "t001", "t002", "t003"]);

    // When: the page holds the fuzzy matches and one more.
    let four = search(4, None);
    // Then: fuzzy titles and the body hit share the page without exceeding it.
    assert_eq!(four.hits.len(), 4);
    assert_eq!(four.hits[3].thread_id, "t003");
    assert!(search(2, None).hits.len() <= 2);
}

#[test]
fn host_search_cursor_reaches_every_fuzzy_title_match_beyond_the_first_page() {
    // Given: 21 titles that match "pg" only by scattered letters.
    let f = Fixture::new();
    let titles: Vec<String> = (0..21).map(|index| format!("Page group {index}")).collect();
    let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
    many_threads_with_titles(&f, &refs);
    let first = f
        .runtime
        .search_threads_for_host(f.project.path(), "pg", 20, None)
        .unwrap();
    // Then: the first page is full and the 21st is reachable from its cursor.
    assert_eq!(first.hits.len(), 20);
    let cursor = first.next_cursor.expect("one title is left");
    let second = f
        .runtime
        .search_threads_for_host(f.project.path(), "pg", 20, Some(&cursor))
        .unwrap();
    assert_eq!(second.hits.len(), 1);
    assert!(second.next_cursor.is_none());
    let mut all: Vec<_> = first
        .hits
        .iter()
        .chain(&second.hits)
        .map(|hit| hit.thread_id.clone())
        .collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 21);
}

#[test]
fn host_search_flags_title_matching_that_did_not_reach_older_threads() {
    // Given: more threads than the fuzzy title pass reads; only the oldest title matches.
    let f = Fixture::new();
    let mut titles = vec!["needle in the oldest"];
    titles.extend(std::iter::repeat_n("Chat", 2100));
    many_threads_with_titles(&f, &titles);
    // When: searching letters that match it only as a subsequence.
    let search = f
        .runtime
        .search_threads_for_host(f.project.path(), "ndlo", 20, None)
        .unwrap();
    // Then: the miss is reported instead of looking like an exhaustive result.
    assert!(search.hits.is_empty());
    assert!(search.title_scan_limited);
    let small = Fixture::new();
    many_threads_with_titles(&small, &["Chat"]);
    assert!(
        !small
            .runtime
            .search_threads_for_host(small.project.path(), "ndlo", 20, None)
            .unwrap()
            .title_scan_limited
    );
}

/// Follows `search_threads_for_host` cursors to the end, returning every page's thread IDs.
fn host_pages(f: &Fixture, query: &str, limit: usize) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = f
            .runtime
            .search_threads_for_host(f.project.path(), query, limit, cursor.as_deref())
            .unwrap();
        pages.push(page.hits.iter().map(|hit| hit.thread_id.clone()).collect());
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return pages,
        }
        assert!(pages.len() < 20, "the cursor chain must end");
    }
}

#[test]
fn host_search_lists_a_thread_once_when_its_title_and_body_both_match() {
    // Given: threads whose titles and bodies both contain the query, one body-only
    // thread, and a page of 20 so titles end both mid-page and exactly on a page.
    for (titles, limit) in [(25, 20), (20, 20)] {
        let f = Fixture::new();
        let names: Vec<String> = (0..titles).map(|index| format!("report {index}")).collect();
        let mut all: Vec<&str> = names.iter().map(String::as_str).collect();
        all.push("Notes");
        many_threads_with_titles(&f, &all);
        for index in 0..=titles {
            let run = format!("run-{}", 3000 + index);
            f.native_run(
                &run,
                Some(&format!("t{index:03}")),
                None,
                vec![
                    assistant(vec![text("the report body")]),
                    assistant(vec![text("done")]),
                ],
            );
            f.index(&run);
        }
        // When: following the cursors through title pages and into the body pages.
        let pages = host_pages(&f, "report", limit);
        // Then: every thread appears exactly once, none repeated by a body page.
        let mut flat: Vec<_> = pages.concat();
        let listed = flat.len();
        flat.sort();
        flat.dedup();
        assert_eq!(listed, flat.len(), "{pages:?}");
        assert_eq!(flat.len(), titles + 1, "{pages:?}");
        assert!(pages.iter().all(|page| page.len() <= limit));
    }
}

// ---- Bounded scan (#374): the matcher alone decides, the budget counts what
// ---- was examined, and every page advances.

const CORPUS_KINDS: [&str; 3] = ["conversation", "tool_use", "tool_result"];

/// Block text whose match class depends deterministically on run and slot:
/// both words, one word, or neither.
fn corpus_text(run: u64, slot: u64) -> String {
    match (run + slot) % 3 {
        0 => format!("needle alpha {run}.{slot}"),
        1 => format!("needle {run}.{slot}"),
        _ => format!("plain {run}.{slot}"),
    }
}

/// Five blocks of five kinds: user text, tool use, tool result, tool error,
/// assistant text.
fn corpus_messages(run: u64) -> Vec<Message> {
    let result = |id: &str, slot: u64, is_error: bool| ContentBlock::ToolResult {
        tool_call_id: id.into(),
        content: vec![ToolResultContent::Text {
            text: corpus_text(run, slot),
        }],
        is_error,
    };
    vec![
        user(vec![text(&corpus_text(run, 0))]),
        assistant(vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "shell".into(),
            input: json!({"command": corpus_text(run, 1)}),
        }]),
        user(vec![result("c1", 2, false), result("c2", 3, true)]),
        assistant(vec![text(&corpus_text(run, 4))]),
    ]
}

struct Corpus {
    /// Thread ID and the runs it owns.
    threads: Vec<(String, Vec<String>)>,
}

impl Corpus {
    fn runs(&self) -> usize {
        self.threads.iter().map(|(_, runs)| runs.len()).sum()
    }
}

/// Three threads, each a root with children; `t2` has 101 runs, one more
/// than a storage batch.
fn seed_corpus(f: &Fixture) -> Corpus {
    let mut threads = Vec::new();
    let layout: [(&str, u64, Vec<u64>); 3] = [
        ("t1", 301, vec![311]),
        ("t2", 302, (400..500).collect()),
        ("t3", 303, vec![313]),
    ];
    for (index, (thread, root, children)) in layout.into_iter().enumerate() {
        let root_id = format!("run-{root}");
        f.thread(thread, Some(f.project.path()), &root_id, |row| {
            row.updated_at_ns = 100 + 10 * index as i64;
        });
        f.native_run(&root_id, Some(thread), None, corpus_messages(root));
        f.index(&root_id);
        let mut runs = vec![root_id.clone()];
        for child in children {
            let child_id = format!("run-{child}");
            f.native_run(&child_id, None, Some(&root_id), corpus_messages(child));
            f.index(&child_id);
            runs.push(child_id);
        }
        threads.push((thread.to_owned(), runs));
    }
    Corpus { threads }
}

fn matcher_of(query: &Value, mode: &str) -> Matcher {
    Matcher::new(
        Some(serde_json::from_value(query.clone()).unwrap()),
        serde_json::from_value(json!(mode)).unwrap(),
    )
    .unwrap()
    .unwrap()
}

fn db_kinds(kinds: &[&str]) -> Vec<&'static str> {
    kinds
        .iter()
        .flat_map(|kind| match *kind {
            "conversation" => &["text"][..],
            "tool_use" => &["tool_use"][..],
            "tool_result" => &["tool_result", "tool_error"][..],
            other => panic!("unexpected kind {other}"),
        })
        .copied()
        .collect()
}

/// Brute force: every stored block of the corpus, redacted and judged by the
/// same matcher, as `(thread, ref)` sorted.
fn oracle(
    f: &Fixture,
    who: Who<'_>,
    corpus: &Corpus,
    matcher: &Matcher,
    kinds: &[&str],
) -> Vec<(String, String)> {
    let scope = f.scope(who);
    let kinds = db_kinds(kinds);
    let conn = Connection::open(&f.store().db_path).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT ordinal, block_index, kind, text FROM conversation_blocks WHERE run_id = ?1",
        )
        .unwrap();
    let mut hits = Vec::new();
    for (thread, runs) in &corpus.threads {
        for run in runs {
            let rows = statement
                .query_map([run], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .unwrap();
            for row in rows {
                let (ordinal, block, kind, body) = row.unwrap();
                if kinds.contains(&kind.as_str()) && matcher.find(&scope.redact(&body)).is_some() {
                    hits.push((thread.clone(), format!("{run}#{ordinal}.{block}")));
                }
            }
        }
    }
    hits.sort();
    hits
}

/// The previous storage-routed path: literal prefilter, then the matcher.
fn legacy_hits(
    f: &Fixture,
    who: Who<'_>,
    corpus: &Corpus,
    matcher: &Matcher,
    terms: Vec<Vec<String>>,
    kinds: &[&str],
) -> BTreeSet<(String, String)> {
    let scope = f.scope(who);
    let index_kinds: Vec<IndexBlockKind> = db_kinds(kinds)
        .into_iter()
        .map(|kind| serde_json::from_value(json!(kind)).unwrap())
        .collect();
    let filter = ConversationFilter::Literals(terms);
    let mut hits = BTreeSet::new();
    for (thread, runs) in &corpus.threads {
        for batch in runs.chunks(MAX_SEARCH_BATCH_RUNS) {
            let mut after = None;
            loop {
                let rows = f
                    .store()
                    .with_database(|db| {
                        db.conversation_candidates(&ConversationQuery {
                            runs: batch,
                            kinds: &index_kinds,
                            filter: &filter,
                            after,
                            limit: 500,
                        })
                    })
                    .unwrap();
                for row in &rows {
                    if matcher.find(&scope.redact(&row.text)).is_some() {
                        let BlockLocator::Message {
                            ordinal,
                            block_index,
                        } = row.locator
                        else {
                            panic!("unexpected note");
                        };
                        hits.insert((
                            thread.clone(),
                            format!("{}#{ordinal}.{block_index}", row.run_id),
                        ));
                    }
                }
                if rows.len() < 500 {
                    break;
                }
                after = rows.last().map(|row| row.position);
            }
        }
    }
    hits
}

/// Hit counts per thread and every snippet ref (without the guard) of a drain.
fn collected(pages: &[(Value, ScanStats)]) -> (BTreeMap<String, u64>, Vec<(String, String)>) {
    let mut counts = BTreeMap::new();
    let mut refs = Vec::new();
    for (page, _) in pages {
        for thread in page["threads"].as_array().unwrap() {
            let id = thread["thread_id"].as_str().unwrap().to_owned();
            *counts.entry(id.clone()).or_insert(0) += thread["hit_count"].as_u64().unwrap();
            for snippet in thread["snippets"].as_array().unwrap() {
                // Metadata snippets have no ref.
                if let Some(reference) = snippet["ref"].as_str() {
                    refs.push((id.clone(), reference.split('@').next().unwrap().to_owned()));
                }
            }
        }
    }
    refs.sort();
    (counts, refs)
}

struct Case {
    query: Value,
    mode: &'static str,
    kinds: Vec<&'static str>,
    /// Literal terms the previous path routed by, for words and phrases.
    terms: Option<Vec<Vec<String>>>,
}

#[test]
fn bounded_drain_neither_skips_nor_repeats_any_block_for_every_query_shape() {
    let f = Fixture::new();
    let own = f.own();
    let corpus = seed_corpus(&f);
    let runs = corpus.runs() + 1;
    let max_block = f.max_block_bytes();
    let words = |terms: &[&[&str]]| {
        Some(
            terms
                .iter()
                .map(|alternative| alternative.iter().map(|term| (*term).to_owned()).collect())
                .collect(),
        )
    };
    let cases = [
        Case {
            query: json!("needle alpha"),
            mode: "words",
            kinds: CORPUS_KINDS.to_vec(),
            terms: words(&[&["needle", "alpha"]]),
        },
        Case {
            query: json!(["alpha 30", "needle 44"]),
            mode: "words",
            kinds: CORPUS_KINDS.to_vec(),
            terms: words(&[&["alpha", "30"], &["needle", "44"]]),
        },
        Case {
            query: json!("needle alpha 4"),
            mode: "phrase",
            kinds: CORPUS_KINDS.to_vec(),
            terms: words(&[&["needle alpha 4"]]),
        },
        Case {
            query: json!(r"needle (alpha )?3\d\d\."),
            mode: "regex",
            kinds: CORPUS_KINDS.to_vec(),
            terms: None,
        },
        Case {
            query: json!("needle"),
            mode: "words",
            kinds: vec!["tool_use"],
            terms: words(&[&["needle"]]),
        },
        Case {
            query: json!("needle"),
            mode: "words",
            kinds: vec!["tool_result"],
            terms: words(&[&["needle"]]),
        },
        Case {
            query: json!("alpha"),
            mode: "words",
            kinds: vec!["conversation"],
            terms: words(&[&["alpha"]]),
        },
    ];
    for (index, case) in cases.iter().enumerate() {
        let matcher = matcher_of(&case.query, case.mode);
        let expected = oracle(&f, own, &corpus, &matcher, &case.kinds);
        assert!(expected.len() > 10, "case {index} must hit");
        let mut expected_counts: BTreeMap<String, u64> = BTreeMap::new();
        for (thread, _) in &expected {
            *expected_counts.entry(thread.clone()).or_insert(0) += 1;
        }
        let args =
            json!({"query": case.query, "match": case.mode, "kinds": case.kinds, "limit": 20});
        // Small budgets give many pages; keep them to one case so the suite
        // stays fast. The others still cross several pages and both batches.
        let sweeps: &[SearchLimits] = if index == 0 {
            &[limits(7, 1024)]
        } else {
            &[limits(64, 4096)]
        };
        for &sweep in sweeps {
            let pages = f.drain(own, args.clone(), sweep);
            assert!(pages.len() > 1, "case {index} {sweep:?}");
            for (_, stats) in &pages {
                assert_scan_bounds(*stats, sweep, max_block, runs);
            }
            let (counts, refs) = collected(&pages);
            assert_eq!(counts, expected_counts, "case {index} {sweep:?}");
            let unique: BTreeSet<_> = refs.iter().collect();
            assert_eq!(unique.len(), refs.len(), "a ref repeated: case {index}");
            let oracle_refs: BTreeSet<_> = expected.iter().collect();
            assert!(
                refs.iter().all(|reference| oracle_refs.contains(reference)),
                "case {index} {sweep:?}"
            );
            let last = pages.last().unwrap();
            assert_eq!(last.0["partial"], false);
            assert_eq!(last.0["next_cursor"], Value::Null);
        }
        if let Some(terms) = &case.terms {
            let legacy = legacy_hits(&f, own, &corpus, &matcher, terms.clone(), &case.kinds);
            let current: BTreeSet<_> = expected.iter().cloned().collect();
            assert!(!legacy.is_empty(), "case {index}");
            assert!(
                legacy.is_subset(&current),
                "the scan must find everything the FTS-routed path found: case {index}"
            );
        }
    }
    // A selective regex makes every returned position observable despite the
    // per-thread snippet bound: the union of all pages is exactly the oracle.
    let rare = json!(r"needle alpha (301|313|440)\.");
    let matcher = matcher_of(&rare, "regex");
    let expected = oracle(&f, own, &corpus, &matcher, &CORPUS_KINDS);
    assert!((3..=9).contains(&expected.len()), "{expected:?}");
    for sweep in [limits(13, 1 << 20), limits(2000, 4 << 20)] {
        let pages = f.drain(
            own,
            json!({"query": rare, "match": "regex", "kinds": CORPUS_KINDS, "limit": 20}),
            sweep,
        );
        assert_eq!(collected(&pages).1, expected, "{sweep:?}");
    }
}

#[test]
fn pages_without_hits_still_advance_strictly_to_the_end() {
    let f = Fixture::new();
    let own = f.own();
    let messages = (0..23)
        .map(|i| user(vec![text(&format!("filler {i}"))]))
        .collect();
    f.thread("t8", Some(f.project.path()), "run-801", |row| {
        row.updated_at_ns = 100;
    });
    f.native_run("run-801", Some("t8"), None, messages);
    f.index("run-801");
    let query = json!("zzz-absent");
    let args = json!({"query": query, "match": "phrase", "kinds": ["conversation"]});
    let pages = f.drain(own, args, limits(5, 1 << 20));
    assert_eq!(pages.len(), 5);
    let scope = f.scope(own);
    let matcher = Matcher::new(
        Some(serde_json::from_value(query).unwrap()),
        MatchMode::Phrase,
    )
    .unwrap();
    let conditions = search::conditions_key(
        matcher.as_ref(),
        MatchMode::Phrase,
        &BTreeSet::from([SearchKind::Conversation]),
        true,
        None,
    );
    let caller = search::caller_key(&scope);
    let mut from_blocks = Vec::new();
    let mut cursors = BTreeSet::new();
    for (page, _) in &pages[..4] {
        assert_eq!(page["threads"], json!([]), "{page}");
        assert_eq!(page["partial"], true);
        let token = page["next_cursor"].as_str().unwrap();
        assert!(cursors.insert(token.to_owned()), "cursor repeated");
        let position = cursor::decode(token, &caller, &conditions).unwrap();
        from_blocks.push(
            position
                .resume
                .expect("a cut resumes inside the thread")
                .from_block,
        );
    }
    assert!(
        from_blocks.windows(2).all(|pair| pair[0] < pair[1]),
        "{from_blocks:?}"
    );
    let (last, _) = &pages[4];
    assert_eq!(last["threads"], json!([]));
    assert_eq!(last["partial"], false);
    assert_eq!(last["next_cursor"], Value::Null);
}

#[test]
fn budget_spent_exactly_at_a_thread_end_cuts_at_the_next_thread_start() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("tA", Some(f.project.path()), "run-901", |row| {
        row.updated_at_ns = 200;
    });
    f.thread("tB", Some(f.project.path()), "run-902", |row| {
        row.updated_at_ns = 100;
        row.title = Some("bound title".into());
    });
    let messages = |prefix: &str, n: usize| {
        (0..n)
            .map(|i| user(vec![text(&format!("bound {prefix}{i}"))]))
            .collect::<Vec<_>>()
    };
    f.native_run("run-901", Some("tA"), None, messages("a", 5));
    f.native_run("run-902", Some("tB"), None, messages("b", 3));
    f.index("run-901");
    f.index("run-902");
    let args = json!({"query": "bound", "kinds": ["title", "conversation"]});
    let pages = f.drain(own, args, limits(5, 1 << 20));
    assert_eq!(pages.len(), 2);
    let first = pages[0].0["threads"].as_array().unwrap();
    assert_eq!(first.len(), 2, "{}", pages[0].0);
    assert_eq!(first[0]["thread_id"], "tA");
    assert_eq!(first[0]["hit_count"], 5);
    assert!(first[0].get("partial").is_none(), "tA is complete");
    assert_eq!(first[1]["thread_id"], "tB");
    assert_eq!(first[1]["hit_count"], 1, "only the title before the cut");
    assert_eq!(first[1]["snippets"][0]["kind"], "title");
    assert_eq!(first[1]["partial"], true);
    assert_eq!(pages[0].0["partial"], true);
    let second = pages[1].0["threads"].as_array().unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["thread_id"], "tB");
    assert_eq!(
        second[0]["hit_count"], 3,
        "the title is not found a second time"
    );
    let titles = pages
        .iter()
        .flat_map(|(page, _)| page["threads"].as_array().unwrap())
        .flat_map(|thread| thread["snippets"].as_array().unwrap())
        .filter(|snippet| snippet["kind"] == "title")
        .count();
    assert_eq!(titles, 1);
    // One block per page still reaches every hit exactly once.
    let args = json!({"query": "bound", "kinds": ["title", "conversation"]});
    let pages = f.drain(own, args, limits(1, 1));
    let (counts, _) = collected(&pages);
    assert_eq!(
        counts,
        BTreeMap::from([("tA".to_owned(), 5), ("tB".to_owned(), 4)])
    );
}

#[test]
fn secrets_never_match_even_when_the_index_holds_them() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("t11", Some(f.project.path()), "run-1101", |_| {});
    f.native_run(
        "run-1101",
        Some("t11"),
        None,
        vec![
            user(vec![text(&format!("prefix {SECRET} suffix"))]),
            user(vec![text("second line")]),
        ],
    );
    f.index("run-1101");
    let search = |query: &str, mode: &str| {
        f.call(
            own,
            "thread_search",
            json!({"query": query, "match": mode, "kinds": ["conversation"]}),
        )
        .unwrap()
    };
    for (query, mode) in [
        ("known-secret-value", "phrase"),
        (SECRET, "phrase"),
        (SECRET, "words"),
        (r"secret-value-\d+", "regex"),
    ] {
        assert_eq!(count(&search(query, mode)), 0, "{query} {mode}");
    }
    assert_eq!(count(&search("prefix", "words")), 1);
    // An unredacted row (an older extractor, a bug) is redacted again on read.
    Connection::open(&f.store().db_path)
        .unwrap()
        .execute(
            "UPDATE conversation_blocks SET text = ?1 WHERE run_id = 'run-1101' AND ordinal = 0",
            [format!("leak {SECRET} tail")],
        )
        .unwrap();
    for (query, mode) in [
        ("known-secret-value", "phrase"),
        (SECRET, "words"),
        (r"secret-value-\d+", "regex"),
    ] {
        assert_eq!(count(&search(query, mode)), 0, "{query} {mode}");
    }
    let leak = search("leak", "words");
    assert_eq!(count(&leak), 1);
    assert!(!leak.to_string().contains(SECRET), "{leak}");
}

#[test]
fn invisible_threads_and_runs_change_neither_pages_nor_cursors() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("vis", Some(f.project.path()), "run-1201", |row| {
        row.updated_at_ns = 50;
    });
    f.native_run(
        "run-1201",
        Some("vis"),
        None,
        (0..40)
            .map(|i| user(vec![text(&format!("scan-{i} common"))]))
            .collect(),
    );
    f.index("run-1201");
    let queries = [
        json!({"query": "scan common", "kinds": ["conversation"]}),
        json!({"query": "scan-1", "match": "phrase", "kinds": ["conversation"]}),
        json!({"query": "^scan-[1-3]", "match": "regex", "kinds": ["conversation"]}),
        json!({"query": "absent-phrase", "match": "phrase", "kinds": ["conversation"]}),
    ];
    let snapshot = |f: &Fixture| -> Vec<Vec<Value>> {
        queries
            .iter()
            .map(|args| {
                f.drain(own, args.clone(), limits(7, 1024))
                    .into_iter()
                    .map(|(page, _)| page)
                    .collect()
            })
            .collect()
    };
    let before = snapshot(&f);
    assert!(before.iter().all(|pages| pages.len() > 1));
    for i in 0..105 {
        let run = format!("run-{}", 2000 + i);
        let thread = format!("hidden-{i}");
        f.thread(&thread, Some(f.other.path()), &run, |row| {
            row.updated_at_ns = 999;
        });
        f.native_run(
            &run,
            Some(&thread),
            None,
            vec![user(vec![text("scan-1 common absent-phrase")]); 3],
        );
        f.index(&run);
    }
    // A run no visible thread owns, and a child of a hidden thread's run.
    f.native_run(
        "run-3000",
        None,
        None,
        vec![user(vec![text("scan-1 common")])],
    );
    f.index("run-3000");
    f.native_run(
        "run-3001",
        None,
        Some("run-2000"),
        vec![user(vec![text("scan-1 common")])],
    );
    f.index("run-3001");
    assert_eq!(snapshot(&f), before);
}

#[test]
fn one_call_fetches_a_bounded_amount_even_when_nothing_or_everything_matches() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("perf", Some(f.project.path()), "run-1301", |_| {});
    let mut messages = Vec::new();
    for i in 0..300 {
        messages.push(assistant(vec![ContentBlock::ToolUse {
            id: format!("c{i}"),
            name: "shell".into(),
            input: json!({"command": format!("frequentneedle step {i}")}),
        }]));
        messages.push(user(vec![ContentBlock::ToolResult {
            tool_call_id: format!("c{i}"),
            content: vec![ToolResultContent::Text {
                text: format!("frequentneedle output {i}"),
            }],
            is_error: false,
        }]));
    }
    f.native_run("run-1301", Some("perf"), None, messages);
    f.index("run-1301");
    let max_block = f.max_block_bytes();
    let kinds = json!(["conversation", "tool_use", "tool_result"]);
    for args in [
        json!({"query": "zzz-no-such-phrase", "match": "phrase", "kinds": kinds}),
        json!({"query": "frequentneedle", "kinds": kinds}),
    ] {
        for sweep in [
            limits(7, 1024),
            limits(450, 1 << 20),
            limits(1000, 2048),
            limits(2000, 4 << 20),
        ] {
            let (page, stats) = f.search_limited(own, args.clone(), sweep).unwrap();
            assert_scan_bounds(stats, sweep, max_block, 2);
            assert!(stats.fetched_blocks > 0, "{page}");
        }
    }
    // The budget is spent on blocks examined, not on hits: a query that matches
    // nothing is cut exactly like one that matches everything.
    let (page, stats) = f
        .search_limited(
            own,
            json!({"query": "zzz-no-such-phrase", "match": "phrase", "kinds": kinds}),
            limits(450, 1 << 20),
        )
        .unwrap();
    assert_eq!(page["partial"], true);
    assert!(page["next_cursor"].is_string());
    assert_eq!(stats.fetched_blocks, 450 + 1);
}

#[test]
fn byte_heavy_blocks_stay_within_derived_bounds_and_drain_completely() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("big", Some(f.project.path()), "run-1401", |row| {
        row.updated_at_ns = 100;
    });
    let padded =
        |marker: &str, bytes: usize| format!("{marker}{}", "y".repeat(bytes - marker.len()));
    let marker = |hit: bool| if hit { "needle " } else { "plain " };
    // Many one-byte blocks (count splits), blocks of a quarter MiB (byte
    // splits) and two Text blocks of 2 MiB (the read-ahead past a spent
    // budget is as large as the block).
    let mut messages = Vec::new();
    for i in 0..1800 {
        messages.push(user(vec![text(if [5, 900, 1799].contains(&i) {
            "needle"
        } else {
            "x"
        })]));
    }
    for i in 0..16 {
        messages.push(user(vec![text(&padded(
            marker(i == 3 || i == 15),
            256 * 1024,
        ))]));
    }
    for i in 0..2 {
        messages.push(user(vec![text(&padded(marker(i == 1), 2 * 1024 * 1024))]));
    }
    for i in 0..20 {
        messages.push(user(vec![text(if i == 10 { "needle" } else { "x" })]));
    }
    f.native_run("run-1401", Some("big"), None, messages);
    f.index("run-1401");
    let max_block = f.max_block_bytes();
    assert!(max_block >= 2 * 1024 * 1024, "{max_block}");
    let corpus = Corpus {
        threads: vec![("big".to_owned(), vec!["run-1401".to_owned()])],
    };
    let query = json!("needle");
    let expected = oracle(
        &f,
        own,
        &corpus,
        &matcher_of(&query, "words"),
        &["conversation"],
    );
    assert_eq!(expected.len(), 7);
    let args = json!({"query": query, "kinds": ["conversation"], "limit": 20});
    for sweep in [
        limits(search::MAX_SCANNED_BLOCKS, search::MAX_SCANNED_BYTES),
        limits(300, 1 << 20),
        limits(500, 512 * 1024),
        limits(50, 300_000),
    ] {
        let pages = f.drain(own, args.clone(), sweep);
        assert!(pages.len() > 1, "{sweep:?}");
        for (_, stats) in &pages {
            assert_scan_bounds(*stats, sweep, max_block, 2);
        }
        let (counts, _) = collected(&pages);
        assert_eq!(counts, BTreeMap::from([("big".to_owned(), 7)]), "{sweep:?}");
        assert_eq!(pages.last().unwrap().0["next_cursor"], Value::Null);
    }
    // Nothing matches: the same blocks are examined and the drain still ends.
    let pages = f.drain(
        own,
        json!({"query": "zzz-absent", "match": "phrase", "kinds": ["conversation"]}),
        limits(search::MAX_SCANNED_BLOCKS, search::MAX_SCANNED_BYTES),
    );
    assert!(pages.len() > 1);
    for (page, stats) in &pages {
        assert_scan_bounds(
            *stats,
            limits(search::MAX_SCANNED_BLOCKS, search::MAX_SCANNED_BYTES),
            max_block,
            2,
        );
        assert_eq!(page["threads"], json!([]));
    }
}
