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
        execution: None,
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

#[test]
fn legacy_trace_deserializes_without_execution() {
    // Given: a persisted pre-variant trace.
    let value = serde_json::json!({
        "id": "a", "arena_id": "arena", "project": "p", "task_id": "t",
        "task_spec": "legacy", "config_id": "c", "profile": "local",
        "model": "m", "attribution": "worker", "output": "ok",
        "input_tokens": 1, "output_tokens": 1, "elapsed_ms": 1, "failure": null
    });
    // When: the new storage schema reads it.
    let trace: EvalTrace = serde_json::from_value(value).expect("legacy trace");
    // Then: absent execution evidence remains explicitly unknown.
    assert!(trace.execution.is_none());
}
