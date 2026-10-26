//! v24 migration, backfill and run binding against a genuine v23 database.

use std::path::Path;

use rusqlite::{Connection, params};
use tempfile::TempDir;

use crate::threads::{ThreadLineageKind, ThreadRunRow, ThreadSource};
use crate::{Database, StorageConfig};

const V23: usize = 23;

fn config(path: &Path) -> StorageConfig {
    StorageConfig {
        db_path: path.to_path_buf(),
        ..StorageConfig::default()
    }
}

/// A database exactly as v23 left it.
fn v23_database(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    for migration in &crate::migrations::MIGRATIONS[..V23] {
        conn.execute_batch(migration).unwrap();
    }
    conn.pragma_update(None, "user_version", i64::try_from(V23).unwrap())
        .unwrap();
    conn
}

fn context(
    conn: &Connection,
    run_id: &str,
    name: &str,
    parent: Option<&str>,
    config: &str,
    at: i64,
) {
    conn.execute(
        "INSERT INTO run_contexts (run_id, role, name, parent_run_id, config_json, messages_json, \
         checkpoints_json, terminal_phase, restorable, updated_at_ns) \
         VALUES (?1, 'worker', ?2, ?3, ?4, '[]', '[]', 'Done', 1, ?5)",
        params![run_id, name, parent, config, at],
    )
    .unwrap();
}

fn request(conn: &Connection, id: &str, run_id: &str, parent: Option<&str>) {
    conn.execute(
        "INSERT INTO usage_requests (request_id, at_ns, provider, model, run_id, parent_run_id, \
         status) VALUES (?1, 1, 'openai', 'gpt', ?2, ?3, 'ok')",
        params![id, run_id, parent],
    )
    .unwrap();
}

fn escalation_event(conn: &Connection, source: &str, new_run: &str) {
    let kind = event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::EscalationRequested {
        source_run_id: source.into(),
        new_run_id: new_run.into(),
        summary: event_bus::EscalationMemoSummary {
            original_request: "do it".into(),
            escalation_reason: "too big".into(),
            files_touched: vec![],
            blockers: vec![],
            suggested_next: "plan".into(),
        },
    });
    conn.execute(
        "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
         payload) VALUES ('gui', 1, 0, 0, 'Lifecycle', ?1)",
        [serde_json::to_string(&kind).unwrap()],
    )
    .unwrap();
}

fn bindings(conn: &Connection) -> Vec<(String, String, String, i64)> {
    conn.prepare("SELECT run_id, thread_id, root_run_id, depth FROM thread_runs ORDER BY run_id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn binding(run: &str, thread: &str, root: &str, depth: i64) -> (String, String, String, i64) {
    (run.into(), thread.into(), root.into(), depth)
}

/// Legacy runs covering every backfill source, with a malformed config among them.
fn legacy_data(conn: &Connection) {
    let repo = r#"{"project_root":"/repo"}"#;
    context(conn, "run-1", "chat:Worker:t-1", None, repo, 100);
    context(conn, "run-2", "explore", Some("run-1"), "{}", 150);
    request(conn, "req-3", "run-3", Some("run-2"));
    context(conn, "run-8", "broken", Some("run-1"), "not json", 120);
    context(
        conn,
        "run-4",
        "chat:Orchestrator:escalation-run-4",
        None,
        repo,
        300,
    );
    escalation_event(conn, "run-2", "run-4");
    context(
        conn,
        "run-5",
        "cli",
        None,
        r#"{"project_root":"/other"}"#,
        400,
    );
    context(
        conn,
        "run-6",
        "chat:Worker:t-start",
        None,
        r#"{"project_root":"/config"}"#,
        90,
    );
    conn.execute(
        "INSERT INTO thread_start_states (thread_id, root_run_id, project_root, captured_at_ns, \
         replayable, reason) VALUES ('t-start', 'run-6', '/started', 50, 0, 'no_commit')",
        [],
    )
    .unwrap();
    context(conn, "run-7", "gui-root", None, "{}", 200);
    conn.execute_batch(
        "INSERT INTO usage_run_threads VALUES ('run-7', 't-gui', 'proj-gui'), \
         ('run-1', 't-1', 'proj-1');",
    )
    .unwrap();
    context(conn, "run-10", "chat:Advisor:t-1:consult", None, repo, 110);
}

#[test]
fn v23_database_migrates_backfills_and_opens_read_only() {
    // Given: a v23 database holding runs from every legacy source.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("v23.db");
    legacy_data(&v23_database(&path));

    // When: the current schema opens it, twice.
    for _ in 0..2 {
        drop(Database::open(&config(&path)).unwrap());
    }
    let database = Database::open_read_only(&path).unwrap();

    // Then: runs are bound by name, thread start, GUI attribution and parent links.
    assert_eq!(database.pragma_i64("user_version").unwrap(), 26);
    assert_eq!(
        bindings(&database.conn),
        [
            binding("run-1", "t-1", "run-1", 0),
            binding("run-10", "t-1:consult", "run-10", 0),
            binding("run-2", "t-1", "run-1", 1),
            binding("run-3", "t-1", "run-1", 2),
            binding("run-4", "escalation-run-4", "run-4", 0),
            binding("run-6", "t-start", "run-6", 0),
            binding("run-7", "t-gui", "run-7", 0),
            binding("run-8", "t-1", "run-1", 1),
        ]
    );
    // And: threads carry what the metadata supports, and nothing more.
    let t1 = database.thread("t-1").unwrap().unwrap();
    assert_eq!(t1.source, ThreadSource::Backfill);
    assert_eq!(
        (
            t1.project_root.as_deref(),
            t1.project_id.as_deref(),
            t1.root_run_id.as_deref(),
            t1.chat_role.as_deref(),
            t1.title.as_deref(),
            t1.lineage_kind,
        ),
        (
            Some("/repo"),
            Some("proj-1"),
            Some("run-1"),
            Some("Worker"),
            None,
            None
        )
    );
    assert_eq!((t1.created_at_ns, t1.updated_at_ns), (100, 150));
    let escalated = database.thread("escalation-run-4").unwrap().unwrap();
    assert_eq!(
        (
            escalated.lineage_kind,
            escalated.escalation_source_run_id.as_deref(),
            escalated.parent_thread_id.as_deref(),
            escalated.chat_role.as_deref(),
            escalated.project_root.as_deref(),
        ),
        (
            Some(ThreadLineageKind::Escalation),
            Some("run-2"),
            Some("t-1"),
            Some("Orchestrator"),
            Some("/repo")
        )
    );
    let started = database.thread("t-start").unwrap().unwrap();
    assert_eq!(started.project_root.as_deref(), Some("/started"));
    assert_eq!((started.created_at_ns, started.updated_at_ns), (50, 90));
    let gui = database.thread("t-gui").unwrap().unwrap();
    assert_eq!(
        (gui.project_root, gui.project_id.as_deref(), gui.chat_role),
        (None, Some("proj-gui"), None)
    );
    assert!(database.thread("run-5").unwrap().is_none());
    assert_eq!(database.thread_of_run("run-5").unwrap(), None);
    // And: project listing and lineage reads see the backfill.
    let page = database.threads_in_project("/repo", None, 10).unwrap();
    assert_eq!(
        page.threads
            .iter()
            .map(|thread| thread.thread_id.as_str())
            .collect::<Vec<_>>(),
        ["escalation-run-4", "t-1", "t-1:consult"]
    );
    assert_eq!(
        database
            .thread_children("t-1")
            .unwrap()
            .into_iter()
            .map(|thread| thread.thread_id)
            .collect::<Vec<_>>(),
        ["escalation-run-4"]
    );
}

#[test]
fn v23_database_without_runs_migrates_to_empty_tables() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.db");
    drop(v23_database(&path));

    let database = Database::open(&config(&path)).unwrap();

    assert!(bindings(&database.conn).is_empty());
    assert!(
        database
            .threads_in_project("/repo", None, 10)
            .unwrap()
            .threads
            .is_empty()
    );
}

#[test]
fn saving_a_context_binds_explicit_named_and_inherited_threads() {
    // Given: a migrated database.
    let database = Database::open_in_memory().unwrap();
    let conn = &database.conn;
    let save = |run: &str, name: &str, parent: Option<&str>, config: &str| {
        crate::repo::run_context::upsert(
            conn,
            &crate::RunContextRecord {
                run_id: run.into(),
                role: "worker".into(),
                name: name.into(),
                parent_run_id: parent.map(str::to_owned),
                config_json: config.into(),
                messages_json: "[]".into(),
                checkpoints_json: "[]".into(),
                terminal_phase: "Running".into(),
                restorable: true,
                updated_at_ns: 1,
                host_inputs_json: "[]".into(),
            },
        )
        .unwrap();
    };

    // When: a named root, its child, a grandchild with an explicit thread and
    // an unrelated root are saved.
    save("root", "chat:Worker:named", None, "{}");
    save("child", "explore", Some("root"), "{}");
    save(
        "grandchild",
        "deep",
        Some("child"),
        r#"{"thread_id":"explicit"}"#,
    );
    save("lonely", "cli", None, "{}");
    save(
        "orphan",
        "sub",
        Some("missing"),
        r#"{"thread_id":"adopted"}"#,
    );

    // Then: each is bound by the strongest evidence it has.
    assert_eq!(
        bindings(conn),
        [
            binding("child", "named", "root", 1),
            binding("grandchild", "explicit", "root", 2),
            binding("orphan", "adopted", "missing", 1),
            binding("root", "named", "root", 0),
        ]
    );

    // When: the root is saved again with an explicit thread.
    save(
        "root",
        "chat:Worker:named",
        None,
        r#"{"thread_id":"renamed"}"#,
    );
    // Then: the explicit binding replaces the inferred one.
    assert_eq!(
        database.thread_of_run("root").unwrap(),
        Some(ThreadRunRow {
            run_id: "root".into(),
            thread_id: "renamed".into(),
            root_run_id: "root".into(),
            depth: 0,
        })
    );
}

#[test]
fn legacy_runs_without_bindings_resolve_by_name_and_parent_chain() {
    // Given: runs written after migration without bindings, as old data was.
    let database = Database::open_in_memory().unwrap();
    let conn = &database.conn;
    context(conn, "root", "chat:Orchestrator:legacy", None, "{}", 1);
    context(conn, "child", "explore", Some("root"), "{}", 2);
    request(conn, "req", "ledger-only", Some("child"));
    context(conn, "host-root", "gui", None, "{}", 3);
    conn.execute(
        "INSERT INTO threads (thread_id, root_run_id, created_at_ns, updated_at_ns, source) \
         VALUES ('host-thread', 'host-root', 1, 1, 'host')",
        [],
    )
    .unwrap();
    assert!(bindings(conn).is_empty());

    // When / Then: the name, the usage ledger's parent link and a recorded root resolve them.
    let resolved = |run: &str| database.thread_of_run(run).unwrap();
    assert_eq!(
        resolved("ledger-only"),
        Some(ThreadRunRow {
            run_id: "ledger-only".into(),
            thread_id: "legacy".into(),
            root_run_id: "root".into(),
            depth: 2,
        })
    );
    assert_eq!(resolved("child").unwrap().depth, 1);
    assert_eq!(resolved("host-root").unwrap().thread_id, "host-thread");
    assert_eq!(resolved("unknown"), None);
    // And: listing the thread finds the same runs.
    assert_eq!(
        database
            .runs_of_thread("legacy")
            .unwrap()
            .into_iter()
            .map(|row| (row.run_id, row.depth))
            .collect::<Vec<_>>(),
        [
            ("root".to_owned(), 0),
            ("child".to_owned(), 1),
            ("ledger-only".to_owned(), 2)
        ]
    );
}

#[test]
fn parent_cycles_fail_closed_upward_and_terminate_downward() {
    let database = Database::open_in_memory().unwrap();
    let conn = &database.conn;
    context(conn, "a", "x", Some("b"), "{}", 1);
    context(conn, "b", "y", Some("a"), "{}", 1);

    assert!(database.thread_of_run("a").is_err());

    conn.execute("INSERT INTO thread_runs VALUES ('a', 'cycle', 'a', 0)", [])
        .unwrap();
    assert_eq!(
        database
            .runs_of_thread("cycle")
            .unwrap()
            .into_iter()
            .map(|row| (row.run_id, row.depth))
            .collect::<Vec<_>>(),
        [("a".to_owned(), 0), ("b".to_owned(), 1)]
    );
}
