use super::*;

#[test]
fn storage_descriptor_encoding_is_the_legacy_raw_byte_cap_measure() {
    use storage::history::{Role, read::Descriptor};
    let tool = storage::history::ToolRow {
        tool_name: "shell".into(),
        call_id: "call-1".into(),
        parent_call_id: Some("call-0".into()),
        is_error: true,
        duration_ms: Some(1240),
        input: Some(storage::history::ToolInput {
            summary: Some("echo \"日本語\"".into()),
            parts: vec![
                ("command: echo".into(), false),
                ("\u{e000}ref".into(), true),
            ],
        }),
        detail: Some(vec![("k: v".into(), false)]),
    };
    for role in [
        Role::User,
        Role::Assistant,
        Role::Tool,
        Role::Reasoning,
        Role::Notice,
        Role::Error,
    ] {
        for index in [0, 9, 10, u64::MAX] {
            for input in [false, true] {
                let descriptor = Descriptor {
                    run: "run-1".into(),
                    index,
                    role,
                    parts: if input {
                        vec![]
                    } else {
                        vec![("unicode 日本語\n\t\"".into(), false)]
                    },
                    input: input.then(|| storage::history::read::InputBody {
                        id: "input".into(),
                        bytes: 16385,
                    }),
                    tool: (role == Role::Tool).then(|| tool.clone()),
                };
                let raw = RawMessage::from(descriptor.clone());
                assert_eq!(
                    serde_json::to_vec(&raw).unwrap(),
                    serde_json::to_vec(&descriptor).unwrap()
                );
            }
        }
    }
}

#[test]
fn history_layer_checks_existence_and_cursor_before_limit_without_changing_ingress_validation() {
    use workbench_protocol::queries::PageRequest;
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let db = storage::Database::open(&config).unwrap();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute_batch("INSERT INTO threads(thread_id,root_run_id,created_at_ns,updated_at_ns,source) VALUES ('thread','run',0,0,'runtime'); INSERT INTO thread_runs VALUES ('run','thread','run',0);").unwrap();
    let pager = Pager::default();
    let mut q = ReadCommittedHistory {
        direction: Default::default(),
        thread: ThreadId::new("missing").unwrap(),
        run: None,
        page: PageRequest {
            cursor: Some(PageCursor::new("unknown").unwrap()),
            limit: 0,
        },
    };
    assert_eq!(
        read_history(&db, &pager, &q, "scope").err().unwrap().code,
        ErrorCode::NotFound
    );
    q.thread = ThreadId::new("thread").unwrap();
    q.run = Some(RunId::new("missing").unwrap());
    assert_eq!(
        read_history(&db, &pager, &q, "scope").err().unwrap().code,
        ErrorCode::NotFound
    );
    q.run = None;
    assert_eq!(
        read_history(&db, &pager, &q, "scope").err().unwrap().code,
        ErrorCode::Conflict
    );
    q.page.cursor = None;
    assert_eq!(
        read_history(&db, &pager, &q, "scope").err().unwrap().code,
        ErrorCode::InvalidRequest
    );
    q.page.cursor = Some(pager.history_next(
        "other_scope",
        0,
        storage::history::read::Order(0, true, 0, String::new()),
        [0; 32],
    ));
    assert_eq!(
        read_history(&db, &pager, &q, "scope").err().unwrap().code,
        ErrorCode::Conflict
    );
}
