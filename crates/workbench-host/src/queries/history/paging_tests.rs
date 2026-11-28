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
    let presentation = storage::history::PresentationRow {
        title: Some(("Compare \"日本語\"".into(), false)),
        caption: Some(("\u{e000}ref".into(), true)),
        artifacts: vec![storage::history::PresentedRow {
            artifact_id: format!("artifact-{:032x}", 1),
            digest: format!("{:064x}", 1),
            title: ("A".into(), false),
            caption: None,
            media_type: ("image/png".into(), false),
        }],
    };
    for role in [
        Role::User,
        Role::Assistant,
        Role::Tool,
        Role::Reasoning,
        Role::Notice,
        Role::Error,
        Role::Artifacts,
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
                    presentation: (role == Role::Artifacts).then(|| presentation.clone()),
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

// Given: the largest presentation `present` can make, with text that doubles when JSON escapes it
// Then: what both routes charge a page for the row is at least the size of the view it becomes
#[test]
fn the_budget_of_a_card_covers_the_view_it_expands_to() {
    use storage::history::{PresentationRow, PresentedRow};
    let quotes = |chars: usize| "\"".repeat(chars);
    let artifacts = (0..8)
        .map(|i| PresentedRow {
            artifact_id: format!("artifact-{i:032x}"),
            digest: format!("{i:064x}"),
            title: (quotes(200), false),
            caption: Some((quotes(1000), false)),
            media_type: ("image/png".into(), false),
        })
        .collect();
    let row = PresentationRow {
        title: Some((quotes(200), false)),
        caption: Some((quotes(1000), false)),
        artifacts,
    };
    let raw = RawMessage {
        run: "run-1".into(),
        index: 7,
        role: MessageRole::Artifacts,
        parts: vec![],
        input: None,
        tool: None,
        presentation: Some(row),
    };
    let mut never = |_: &[storage::BodyRef]| -> Result<Vec<_>, storage::StorageError> {
        panic!("no body is stored")
    };
    let message = Bodies::default()
        .message(&ThreadId::new("thread-1").unwrap(), raw.clone(), &mut never)
        .unwrap()
        .unwrap();
    message.validate().unwrap();
    let shown = serde_json::to_vec(&message).unwrap().len();
    assert!(
        shown > 8 * 2 * 1000,
        "the fixture is as large as a view gets: {shown}"
    );
    assert!(raw.budget_len() >= shown, "{} < {shown}", raw.budget_len());
}

#[test]
fn storage_budgets_use_the_protocol_caps() {
    use storage::history::rows::*;
    use workbench_protocol::namespaces::artifacts::*;
    assert_eq!(PRESENTATION_TITLE_BYTES, MAX_PRESENTATION_TITLE_BYTES);
    assert_eq!(PRESENTATION_CAPTION_BYTES, MAX_PRESENTATION_CAPTION_BYTES);
    assert_eq!(
        PRESENTATION_MEDIA_TYPE_BYTES,
        MAX_PRESENTATION_MEDIA_TYPE_BYTES
    );
}
