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
