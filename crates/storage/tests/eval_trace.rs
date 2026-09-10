use storage::eval::{Attribution, EvalTrace, FailureAttribution};
use storage::{Database, Storage, StorageConfig};

#[test]
fn eval_trace_survives_reopen_without_becoming_a_lesson() {
    // Given: a real single-writer ledger and a failed evaluation.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = StorageConfig {
        db_path: dir.path().join("eval.db"),
        ..Default::default()
    };
    let trace = EvalTrace {
        id: "arena/a".into(),
        arena_id: "arena".into(),
        project: "project".into(),
        task_id: "task".into(),
        task_spec: "same task".into(),
        config_id: "a".into(),
        profile: "local".into(),
        model: "mock".into(),
        attribution: Attribution::Worker,
        output: "wrong".into(),
        input_tokens: 2,
        output_tokens: 1,
        elapsed_ms: 10,
        failure: Some(FailureAttribution::OutputMismatch),
    };
    // When: the trace is acknowledged and the database reopened.
    let storage = Storage::open(config.clone()).expect("storage");
    storage.handle().append_eval_trace(&trace).expect("append");
    storage.close();
    let db = Database::open(&config).expect("reopen");
    // Then: typed evidence roundtrips, is tagged, and does not pollute lessons.
    assert_eq!(db.eval_traces("project").expect("traces"), vec![trace]);
    assert!(
        db.search_memory("project", "", None)
            .expect("lessons")
            .is_empty()
    );
    let conn = rusqlite::Connection::open(&config.db_path).expect("connection");
    let kind: String = conn
        .query_row("SELECT kind FROM memory_ledger", [], |r| r.get(0))
        .expect("kind");
    assert_eq!(kind, "eval_trace");
    assert!(conn.execute("DELETE FROM memory_ledger", []).is_err());
}
