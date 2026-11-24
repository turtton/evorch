use super::*;
use workbench_host::queries::HistoryRoute;

fn append(f: &Fixture, text: &str) {
    f.store
        .handle()
        .append_event(
            None,
            &Event::new(MessageEvent::MessageCompleted {
                run_id: "run-1".into(),
                text: text.into(),
            }),
        )
        .unwrap();
}
fn page(f: &Fixture, cursor: Option<PageCursor>, limit: u32) -> Page<CommittedMessage> {
    let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) =
        f.query(history(PageRequest { cursor, limit }))
    else {
        panic!()
    };
    page
}
fn rebuild(f: &Fixture) {
    use storage::history::build::{BuildBudget, Clock};
    struct FrozenClock;
    impl Clock for FrozenClock {
        fn now_ns(&self) -> i64 {
            0
        }
    }
    let db = storage::Database::open(&storage::StorageConfig {
        db_path: f.dir.path().join("host.db"),
        ..Default::default()
    })
    .unwrap();
    for _ in 0..64 {
        if db
            .history_build_step("run-1", BuildBudget::default(), &FrozenClock)
            .unwrap()
            .ready
        {
            return;
        }
    }
    panic!("bounded fixture construction did not complete");
}
fn dirty(f: &Fixture) {
    let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
    db.execute("UPDATE events SET wall_clock_ns=wall_clock_ns+1 WHERE id=(SELECT MIN(id) FROM events WHERE run_id='run-1')", []).unwrap();
}

#[test]
fn indexed_handle_query_keeps_h_after_append_and_exposes_the_selected_route() {
    let f = current_thread_fixture();
    for word in ["first", "second", "third"] {
        append(&f, word);
    }
    let first = page(&f, None, 2);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Indexed)
    );
    assert_eq!(
        first
            .items
            .iter()
            .map(|m| m.index.get())
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    append(&f, "fourth");
    let next = page(&f, first.next_cursor, 2);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Indexed)
    );
    assert_eq!(next.snapshot_revision, first.snapshot_revision);
    assert_eq!(
        next.items.iter().map(|m| m.index.get()).collect::<Vec<_>>(),
        vec![2]
    );
    assert!(next.next_cursor.is_none());
    assert_eq!(page(&f, None, 50).items.len(), 4);
}

fn reuse_scanned_noncommitted_id(with_hook: bool, paused: bool) {
    use storage::history::build::{BuildBudget, Clock};
    struct FrozenClock;
    impl Clock for FrozenClock {
        fn now_ns(&self) -> i64 {
            0
        }
    }
    let f = current_thread_fixture();
    append(&f, "first");
    append(&f, "second");
    f.store
        .handle()
        .append_event(
            None,
            &Event::new(MessageEvent::MessageDelta {
                run_id: Some("run-1".into()),
                delta: "not committed".into(),
            }),
        )
        .unwrap();
    let sql = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
    let tail: i64 = sql
        .query_row("SELECT MAX(id) FROM events", [], |r| r.get(0))
        .unwrap();
    dirty(&f);
    if paused {
        let db = storage::Database::open(&storage::StorageConfig {
            db_path: f.dir.path().join("host.db"),
            ..Default::default()
        })
        .unwrap();
        for _ in 0..3 {
            assert!(
                !db.history_build_step(
                    "run-1",
                    BuildBudget {
                        rows: 1,
                        ..Default::default()
                    },
                    &FrozenClock,
                )
                .unwrap()
                .ready
            );
        }
    } else {
        rebuild(&f);
    }
    let state: (i64, i64, i64, i64) = sql
        .query_row(
            "SELECT phase,checkpoint,last_indexed_id,generation FROM history_run_state WHERE run_id='run-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (state.0, state.1, state.2),
        (if paused { 1 } else { 2 }, tail, tail - 1)
    );
    let old_cursor = if paused {
        None
    } else {
        let first = page(&f, None, 1);
        assert_eq!(
            f.host.projection().history_route(),
            Some(HistoryRoute::Indexed)
        );
        assert_eq!(first.snapshot_revision, Revision(tail as u64));
        Some(first.next_cursor.unwrap())
    };
    sql.execute("DELETE FROM events WHERE id=?1", [tail])
        .unwrap();
    if with_hook {
        append(&f, "reused");
    } else {
        let payload = serde_json::to_string(&event_bus::EventKind::from(
            MessageEvent::MessageCompleted {
                run_id: "run-1".into(),
                text: "reused".into(),
            },
        ))
        .unwrap();
        sql.execute(
            "INSERT INTO events(id,schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload) \
             VALUES (?1,1,0,0,'Message','MessageCompleted','run-1',?2)",
            rusqlite::params![tail, payload],
        )
        .unwrap();
    }
    assert_eq!(
        sql.query_row::<i64, _, _>("SELECT MAX(id) FROM events", [], |r| r.get(0))
            .unwrap(),
        tail
    );
    // Tail replay is incremental, not a destructive invalidation. In the Ready
    // cases below, only the changed H cut can revoke the old cursor.
    assert_eq!(
        sql.query_row::<i64, _, _>(
            "SELECT generation FROM history_run_state WHERE run_id='run-1'",
            [],
            |r| r.get(0),
        )
        .unwrap(),
        state.3
    );
    if paused || !with_hook {
        assert_eq!(
            sql.query_row::<i64, _, _>(
                "SELECT checkpoint FROM history_run_state WHERE run_id='run-1'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            tail - 1
        );
    }
    rebuild(&f);
    if let Some(cursor) = old_cursor {
        assert_eq!(
            query_error(
                &f,
                history(PageRequest {
                    cursor: Some(cursor),
                    limit: 1
                })
            ),
            ErrorCode::Conflict
        );
    }
    let indexed = page(&f, None, 50);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Indexed)
    );
    assert_eq!(indexed.items.len(), 3);
    assert_eq!(
        indexed
            .items
            .iter()
            .map(|m| m.index.get())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    dirty(&f);
    let legacy = page(&f, None, 50);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Legacy("dirty"))
    );
    assert_eq!(
        serde_json::to_value(indexed.items).unwrap(),
        serde_json::to_value(legacy.items).unwrap()
    );
}

#[test]
fn direct_sql_id_reuse_replays_the_scanned_tail_and_revokes_h_cut_cursors() {
    for paused in [false, true] {
        reuse_scanned_noncommitted_id(false, paused);
    }
}

#[test]
fn ordinary_append_id_reuse_replays_the_scanned_tail_and_revokes_h_cut_cursors() {
    for paused in [false, true] {
        reuse_scanned_noncommitted_id(true, paused);
    }
}

#[test]
fn unavailable_run_falls_back_as_a_whole_and_cursor_paths_never_switch() {
    let f = current_thread_fixture();
    for word in ["first", "second", "third"] {
        append(&f, word);
    }
    let indexed = page(&f, None, 1);
    let revoked = indexed.next_cursor.clone();
    dirty(&f);
    assert_eq!(
        query_error(
            &f,
            history(PageRequest {
                cursor: indexed.next_cursor,
                limit: 1,
            })
        ),
        ErrorCode::Conflict
    );
    let legacy = page(&f, None, 1);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Legacy("dirty"))
    );
    rebuild(&f);
    let next = page(&f, legacy.next_cursor, 1);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Legacy("legacy_cursor"))
    );
    assert_eq!(next.items[0].index.get(), 1);
    let indexed = page(&f, None, 1);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Indexed)
    );
    let tail = page(&f, next.next_cursor, 1);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Legacy("legacy_cursor"))
    );
    assert_eq!(tail.items[0].index.get(), 2);
    assert_eq!(
        query_error(
            &f,
            history(PageRequest {
                cursor: revoked,
                limit: 1
            })
        ),
        ErrorCode::Conflict
    );
    assert!(indexed.next_cursor.is_some());
}

#[test]
fn external_query_validation_still_precedes_history_layer_errors() {
    let f = current_thread_fixture();
    for word in ["first", "second"] {
        append(&f, word);
    }
    let first = page(&f, None, 1);
    let unknown = Some(PageCursor::new("unknown").unwrap());
    let Query::Conversation(ConversationQuery::ReadCommittedHistory(mut q)) =
        history(PageRequest {
            cursor: unknown.clone(),
            limit: 0,
        })
    else {
        panic!()
    };
    q.thread = ThreadId::new("missing").unwrap();
    assert_eq!(
        query_error(
            &f,
            Query::Conversation(ConversationQuery::ReadCommittedHistory(q.clone()))
        ),
        ErrorCode::InvalidRequest
    );
    q.page.limit = 1;
    assert_eq!(
        query_error(
            &f,
            Query::Conversation(ConversationQuery::ReadCommittedHistory(q.clone()))
        ),
        ErrorCode::NotFound
    );
    q.page.limit = 0;
    q.thread = ThreadId::new("thread-1").unwrap();
    q.run = Some(RunId::new("missing").unwrap());
    assert_eq!(
        query_error(
            &f,
            Query::Conversation(ConversationQuery::ReadCommittedHistory(q.clone()))
        ),
        ErrorCode::InvalidRequest
    );
    q.page.limit = 1;
    assert_eq!(
        query_error(
            &f,
            Query::Conversation(ConversationQuery::ReadCommittedHistory(q))
        ),
        ErrorCode::NotFound
    );
    assert_eq!(
        query_error(
            &f,
            history(PageRequest {
                cursor: unknown.clone(),
                limit: 0
            })
        ),
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        query_error(
            &f,
            history(PageRequest {
                cursor: unknown,
                limit: 1
            })
        ),
        ErrorCode::Conflict
    );
    assert_eq!(
        query_error(
            &f,
            history(PageRequest {
                cursor: None,
                limit: 0
            })
        ),
        ErrorCode::InvalidRequest
    );
    let Query::Conversation(ConversationQuery::ReadCommittedHistory(mut q)) =
        history(PageRequest {
            cursor: first.next_cursor,
            limit: 0,
        })
    else {
        panic!()
    };
    q.direction = HistoryDirection::Backward;
    assert_eq!(
        query_error(
            &f,
            Query::Conversation(ConversationQuery::ReadCommittedHistory(q.clone()))
        ),
        ErrorCode::InvalidRequest
    );
    q.page.limit = 1;
    assert_eq!(
        query_error(
            &f,
            Query::Conversation(ConversationQuery::ReadCommittedHistory(q))
        ),
        ErrorCode::Conflict
    );
}

#[test]
fn inline_receipt_replacement_revokes_the_cursor_without_any_projection_writes() {
    let f = current_thread_fixture();
    for word in ["first", "second", "third"] {
        append(&f, word);
    }
    let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
    db.execute("INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns,acceptance_seq) VALUES ('input','run-1','run-1','aaaa','delivered',0,0,1)", []).unwrap();
    let first = page(&f, None, 1);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Indexed)
    );
    let before = db.total_changes();
    db.execute(
        "UPDATE host_input_receipts SET text='bbbb' WHERE prompt_id='input'",
        [],
    )
    .unwrap();
    assert_eq!(db.total_changes() - before, 1);
    assert_eq!(
        query_error(
            &f,
            history(PageRequest {
                cursor: first.next_cursor,
                limit: 1
            })
        ),
        ErrorCode::Conflict
    );
    let fresh = page(&f, None, 1);
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Indexed)
    );
    assert_eq!(
        fresh.items[0].content,
        MessageContent::Inline {
            text: "bbbb".into()
        }
    );
}

fn measured(
    f: &Fixture,
    direction: HistoryDirection,
    cursor: Option<PageCursor>,
    limit: u32,
) -> (Page<CommittedMessage>, storage::test_support::HistoryCounts) {
    let db = storage::Database::open_read_only(&f.dir.path().join("host.db")).unwrap();
    let meter = storage::test_support::meter_history(&db);
    let before = meter.counts();
    let Query::Conversation(ConversationQuery::ReadCommittedHistory(mut q)) =
        history(PageRequest { cursor, limit })
    else {
        panic!()
    };
    q.direction = direction;
    let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) = f.query(
        Query::Conversation(ConversationQuery::ReadCommittedHistory(q)),
    ) else {
        panic!()
    };
    assert_eq!(
        f.host.projection().history_route(),
        Some(HistoryRoute::Indexed)
    );
    let mut counts = meter.counts();
    macro_rules! delta {
        ($($field:ident),*) => {$(counts.$field -= before.$field;)*};
    }
    delta!(
        payload_decodes,
        source_rows,
        source_references,
        cut_headers,
        message_headers,
        receipt_headers,
        fingerprint_bytes,
        merge_candidates,
        vm_steps,
        fullscan_steps,
        sorts
    );
    (page, counts)
}

#[test]
fn handle_query_work_depends_on_page_not_history_in_both_directions() {
    let f = current_thread_fixture();
    let mut observations = Vec::new();
    for n in [100, 1000] {
        for i in observations.last().map_or(0, |_| 100)..n {
            append(&f, &format!("answer-{i}"));
            // Neither ignored source rows nor incomplete responses may turn
            // the page read into a history-length scan.
            for event in [
                MessageEvent::ReasoningDelta {
                    run_id: Some("run-1".into()),
                    delta: "ignored".into(),
                },
                MessageEvent::FinalResultPublished {
                    run_id: "run-1".into(),
                    text: format!("answer-{i}"),
                },
                MessageEvent::MessagePart {
                    run_id: "run-1".into(),
                    response_id: format!("pending-{i}"),
                    index: 0,
                    total: 2,
                    text: "unfinished".into(),
                },
            ] {
                f.store
                    .handle()
                    .append_event(None, &Event::new(event))
                    .unwrap();
            }
        }
        if observations.is_empty() {
            let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
            let accepted: i64 = db.query_row("SELECT wall_clock_ns FROM events WHERE run_id='run-1' AND subkind='MessageCompleted' ORDER BY id LIMIT 1 OFFSET 49", [], |r| r.get(0)).unwrap();
            for i in 0..10 {
                db.execute("INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns,acceptance_seq) VALUES (?1,'run-1','run-1','input','delivered',?2,0,?3)", rusqlite::params![format!("fixed-{i}"), accepted, i+1]).unwrap();
            }
        }
        let mut ends = Vec::new();
        for direction in [HistoryDirection::Forward, HistoryDirection::Backward] {
            let (first, count) = measured(&f, direction, None, 10);
            assert_eq!(first.items.len(), 10);
            assert_eq!(count.payload_decodes, 10);
            assert_eq!(count.source_references, 10);
            assert_eq!(count.fullscan_steps, 0);
            // Receipt ordering / first-in-run retain two bounded sorters;
            // neither scans the events or pending-parts history.
            assert_eq!(count.sorts, 2);
            assert!(count.message_headers <= 12);
            let (middle, mid) = measured(&f, direction, first.next_cursor, 10);
            assert_eq!(middle.items.len(), 10);
            assert_eq!(mid.payload_decodes, 10);
            assert!(mid.message_headers <= 12);
            assert_eq!(mid.fullscan_steps, 0);
            ends.push((count, mid));
        }
        observations.push(ends);
    }
    for (small, large) in observations[0].iter().zip(&observations[1]) {
        assert_eq!(small.0, large.0);
        assert_eq!(small.1, large.1);
    }
    eprintln!("HISTORY_SCALE N100/N1000 fixed I10: {observations:?}");
    let (_, larger) = measured(&f, HistoryDirection::Backward, None, 50);
    assert_eq!(larger.payload_decodes, 50);
    assert_eq!(larger.source_references, 50);
    assert_eq!(larger.merge_candidates, 51);
    assert!(larger.message_headers <= 52);
    f.thread("unrelated", "other-run", "Unrelated");
    for _ in 0..1000 {
        f.store
            .handle()
            .append_event(
                None,
                &Event::new(MessageEvent::MessageCompleted {
                    run_id: "other-run".into(),
                    text: "outside".into(),
                }),
            )
            .unwrap();
    }
    let (_, unchanged) = measured(&f, HistoryDirection::Backward, None, 50);
    assert_eq!(larger, unchanged);
}

#[test]
fn handle_query_accounts_for_runs_receipts_fingerprint_bytes_and_all_status_cost() {
    let f = current_thread_fixture();
    append(&f, "one");
    let (_, one_run) = measured(&f, HistoryDirection::Forward, None, 10);
    let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
    for i in 1..624 {
        db.execute("INSERT INTO thread_runs(thread_id,run_id,root_run_id,depth) VALUES ('thread-1',?1,'run-1',1)", [format!("child-{i}")]).unwrap();
    }
    let (_, runs) = measured(&f, HistoryDirection::Forward, None, 10);
    assert_eq!(runs.payload_decodes, 1);
    assert_eq!(runs.message_headers, 1);
    assert_eq!(runs.cut_headers, 1);
    assert!(
        runs.vm_steps > one_run.vm_steps,
        "run discovery remains an explicit R cost"
    );
    for i in 0..10 {
        db.execute("INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns,acceptance_seq) VALUES (?1,'run-1','run-1',?2,'delivered',0,0,?3)",
            rusqlite::params![format!("input-{i}"), "a".repeat(100), i + 1]).unwrap();
    }
    let (_, inputs) = measured(&f, HistoryDirection::Forward, None, 10);
    assert_eq!(inputs.receipt_headers, 10);
    assert_eq!(inputs.fingerprint_bytes, 1000);
    assert_eq!(inputs.payload_decodes, 0);
    db.execute("UPDATE host_input_receipts SET text=?1", ["b".repeat(1000)])
        .unwrap();
    let (_, bytes) = measured(&f, HistoryDirection::Forward, None, 10);
    assert_eq!(bytes.receipt_headers, 10);
    assert_eq!(bytes.fingerprint_bytes, 10000);
    for i in 0..1000 {
        db.execute("INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns,acceptance_seq) VALUES (?1,'run-1','run-1','pending','accepted',0,0,?2)",
            rusqlite::params![format!("pending-{i}"), i + 11]).unwrap();
    }
    let (_, all_status) = measured(&f, HistoryDirection::Forward, None, 10);
    assert_eq!(all_status.receipt_headers, bytes.receipt_headers);
    assert_eq!(all_status.fingerprint_bytes, bytes.fingerprint_bytes);
    assert!(
        all_status.vm_steps > bytes.vm_steps,
        "first_in_run retains the all-status M cost"
    );
    eprintln!(
        "HISTORY_SCALE R1={one_run:?} R624={runs:?} I10={inputs:?} bytes10000={bytes:?} M1010={all_status:?}"
    );
}

#[test]
fn all_indexed_pages_match_legacy_with_fixed_seed_parts_and_receipts() {
    let f = current_thread_fixture();
    let mut seed = 17u64;
    for i in 0..240 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        match seed % 5 {
            0 => {
                for part in 0..3 {
                    f.store
                        .handle()
                        .append_event(
                            None,
                            &Event::new(MessageEvent::MessagePart {
                                run_id: "run-1".into(),
                                response_id: format!("response-{i}"),
                                index: part,
                                total: 3,
                                text: format!("part-{part}"),
                            }),
                        )
                        .unwrap();
                }
            }
            1 => {
                f.store
                    .handle()
                    .append_event(
                        None,
                        &Event::new(MessageEvent::FinalResultPublished {
                            run_id: "run-1".into(),
                            text: "same".into(),
                        }),
                    )
                    .unwrap();
            }
            _ => append(&f, "same"),
        }
    }
    let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
    for i in 0..12 {
        db.execute("INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns,acceptance_seq) VALUES (?1,'run-1','run-1','input','delivered',?2,0,?3)",
            rusqlite::params![format!("input-{i}"), i64::MAX - i, i + 1]).unwrap();
    }
    fn collect(f: &Fixture, direction: HistoryDirection, indexed: bool) -> serde_json::Value {
        let mut result = Vec::new();
        let mut cursor = None;
        loop {
            let Query::Conversation(ConversationQuery::ReadCommittedHistory(mut q)) =
                history(PageRequest { cursor, limit: 7 })
            else {
                panic!()
            };
            q.direction = direction;
            let QueryReply::Conversation(ConversationReply::CommittedHistory(p)) = f.query(
                Query::Conversation(ConversationQuery::ReadCommittedHistory(q)),
            ) else {
                panic!()
            };
            assert_eq!(
                f.host.projection().history_route() == Some(HistoryRoute::Indexed),
                indexed
            );
            if direction == HistoryDirection::Backward {
                result.splice(0..0, p.items);
            } else {
                result.extend(p.items);
            }
            cursor = p.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        serde_json::to_value(result).unwrap()
    }
    let forward = collect(&f, HistoryDirection::Forward, true);
    let backward = collect(&f, HistoryDirection::Backward, true);
    assert_eq!(forward, backward);
    dirty(&f);
    assert_eq!(forward, collect(&f, HistoryDirection::Forward, false));
    assert_eq!(backward, collect(&f, HistoryDirection::Backward, false));
}
