//! Archived-thread retention: closure, confirmation, fences, readers and reclaim.

use std::collections::BTreeMap;
use std::path::PathBuf;

use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event, EventKind,
    OrchestratorEvent, ThreadTodoItem, ThreadTodoSnapshot, ThreadTodoStatus,
};
use rusqlite::{Connection, params};
use storage::inspect::EventScope;
use storage::thread_index::IndexBlockKind;
use storage::thread_retention::{
    BindingInput, BlockReason, ExploreBudget, FamilyState, FilesOutcome, FinalizeBudget,
    FinalizeOutcome, LegacyArchivedPolicy, ReclaimBudget, RetentionInput, RetentionStatus,
    SidebarThread, ThreadRetentionPolicy,
};
use storage::thread_search::{ConversationFilter, ConversationQuery, ConversationScan, ScanLimits};
use storage::{Database, Storage, StorageConfig, StorageError};
use tempfile::TempDir;

const NOW_NS: i64 = 100 * 86_400 * 1_000_000_000;
const BIG: ExploreBudget = ExploreBudget {
    max_edges: 1_000_000,
};
const BIG_FINALIZE: FinalizeBudget = FinalizeBudget {
    max_edges: 1_000_000,
};

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
    conn: Connection,
}

fn config(path: &std::path::Path) -> StorageConfig {
    StorageConfig {
        db_path: path.to_path_buf(),
        ..StorageConfig::default()
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retention.db");
        drop(Database::open(&config(&path)).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        Self {
            _dir: dir,
            path,
            conn,
        }
    }

    fn database(&self) -> Database {
        Database::open(&config(&self.path)).unwrap()
    }

    fn sql(&self, sql: &str) {
        self.conn.execute_batch(sql).unwrap();
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn thread(&self, id: &str, parent: Option<&str>, root: Option<&str>, escalation: Option<&str>) {
        self.conn
            .execute(
                "INSERT INTO threads(thread_id, project_root, root_run_id, parent_thread_id, \
                 lineage_kind, escalation_source_run_id, archived, created_at_ns, updated_at_ns, \
                 source) VALUES (?1, '/p', ?2, ?3, ?4, ?5, 1, 1, 1, 'host')",
                params![
                    id,
                    root,
                    parent,
                    parent.map(|_| if escalation.is_some() {
                        "escalation"
                    } else {
                        "fork"
                    }),
                    escalation
                ],
            )
            .unwrap();
    }

    /// A saved run with a binding to `thread`, plus a row in every run-keyed table.
    fn run(&self, id: &str, parent: Option<&str>, thread: Option<&str>, name: &str) {
        let c = &self.conn;
        c.execute(
            "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, \
             messages_json, checkpoints_json, terminal_phase, restorable, updated_at_ns, \
             messages_len, checkpoints_len) VALUES (?1, 'Worker', ?2, ?3, '{}', '[]', '[]', \
             'Done', 1, 1, 2, 2)",
            params![id, name, parent],
        )
        .unwrap();
        if let Some(thread) = thread {
            let depth = i64::from(parent.is_some());
            c.execute(
                "INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth) VALUES (?1, ?2, ?1, ?3)",
                params![id, thread, depth],
            )
            .unwrap();
        }
    }

    /// Rows in every table that reclaim must remove for run `id`.
    fn run_payload(&self, id: &str, session: &str) {
        let c = &self.conn;
        c.execute(
            "INSERT OR IGNORE INTO sessions(id, status, created_at_ns, updated_at_ns) \
             VALUES (?1, 'running', 0, 0)",
            [session],
        )
        .unwrap();
        c.execute(
            "INSERT INTO run_context_provenance(run_id, snapshot_revision, snapshot_sha256, \
             format_version, status, reason_code, evidence_json, map_json) \
             VALUES (?1, 0, 'x', 1, 'unknown', 'missing_sidecar', NULL, '{}')",
            [id],
        )
        .unwrap();
        for index in 0..3 {
            c.execute(
                "INSERT INTO run_ledger(run_id, body, created_at_ns) VALUES (?1, ?2, 1)",
                params![id, format!("note {index}")],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO host_input_receipts(prompt_id, run_id, root_run_id, text, status, \
             accepted_at_ns, status_at_ns, acceptance_seq) VALUES (?1, ?2, ?2, 't', 'accepted', 1, 1, \
             (SELECT COALESCE(MAX(acceptance_seq), 0) + 1 FROM host_input_receipts))",
            params![format!("prompt-{id}"), id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO review_evidence(id, root_run_id, target_run_id, kind, text, created_at_ns) \
             VALUES (?1, ?2, ?2, 'k', 't', 1)",
            params![format!("evidence-{id}"), id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO review_lineage_edges(child_run_id, parent_run_id, created_at_ns) \
             VALUES (?1, ?2, 1)",
            params![id, format!("{id}-ancestor")],
        )
        .unwrap();
        c.execute(
            "INSERT INTO assistant_references(reference_id, source_run_id, turn_end, text, \
             consumed, created_at_ns) VALUES (?1, ?2, 1, 't', 0, 1)",
            params![format!("ref-{id}"), id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO user_questions(id, run_id, root_run_id, root_name, payload) \
             VALUES (?1, ?2, ?2, 'root', ?3)",
            params![format!("q-{id}"), id, serde_json::json!({"id": format!("q-{id}"), "run_id": id, "root_run_id": id, "root_name": "root", "recipient_run_ids": [], "title": "t", "options": [], "blocking": false, "answer": null}).to_string()],
        )
        .unwrap();
        c.execute(
            "INSERT INTO user_question_links(question_id, run_id) VALUES (?1, ?2)",
            params![format!("q-{id}"), id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision, \
             source_context_sha256, extractor_version, accounting_version, source_updated_at_ns, \
             retention_at_ns, text_bytes, block_count, charged_bytes) \
             VALUES (?1, 'indexed', 1, 1, 'x', 1, 0, 1, 1, 12, 2, 12)",
            [id],
        )
        .unwrap();
        for index in 0..2 {
            c.execute(
                "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, \
                 block_index, role, kind, truncated, hash, text) \
                 VALUES (?1, 'message', ?2, ?3, 0, 'user', 'text', 0, 'h', 'hello!')",
                params![id, format!("{index}:0"), index],
            )
            .unwrap();
        }
        for index in 0..5 {
            self.event(session, Some(id), "Message", "MessageCompleted", 40 + index);
        }
    }

    fn event(&self, session: &str, run: Option<&str>, kind: &str, subkind: &str, size: usize) {
        self.conn
            .execute(
                "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
                 payload, subkind, run_id) VALUES (?1, 1, 1, ?2, ?3, ?4, ?5, ?6)",
                params![session, NOW_NS, kind, "x".repeat(size), subkind, run],
            )
            .unwrap();
        self.conn
            .execute(
                "UPDATE sessions SET total_event_bytes = total_event_bytes + ?2 WHERE id = ?1",
                params![session, size as i64],
            )
            .unwrap();
    }

    fn todo_event(&self, session: &str, thread: &str) {
        let snapshot = ThreadTodoSnapshot {
            list_id: "list".into(),
            thread_id: thread.into(),
            revision: 1,
            items: vec![ThreadTodoItem {
                content: "item".into(),
                status: ThreadTodoStatus::Pending,
            }],
        };
        let payload = serde_json::to_string(
            &Event::new(OrchestratorEvent::ThreadTodoUpdated { snapshot }).kind,
        )
        .unwrap();
        let size = payload.len();
        self.conn
            .execute(
                "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
                 payload, subkind, run_id) VALUES (?1, 1, 1, ?2, 'Orchestrator', ?3, 'ThreadTodoUpdated', NULL)",
                params![session, NOW_NS, payload],
            )
            .unwrap();
        self.conn
            .execute(
                "UPDATE sessions SET total_event_bytes = total_event_bytes + ?2 WHERE id = ?1",
                params![session, size as i64],
            )
            .unwrap();
    }

    fn task(&self, id: &str, run: &str) {
        self.conn
            .execute(
                "INSERT INTO tasks(id, status, created_at_ns, updated_at_ns, parent_run_id) \
                 VALUES (?1, 'pending', 1, 1, ?2)",
                params![id, run],
            )
            .unwrap();
        self.conn
            .execute(
                "INSERT INTO task_queue_ledger(operation, payload) VALUES ('enqueue', ?1)",
                [serde_json::json!({ "id": id }).to_string()],
            )
            .unwrap();
    }

    fn start_state(&self, thread: &str, root: &str) {
        self.conn
            .execute(
                "INSERT INTO thread_start_states(thread_id, root_run_id, project_root, repo_prefix, \
                 head_commit, diff, captured_at_ns, replayable, reason) \
                 VALUES (?1, ?2, '/p', '', 'abc', zeroblob(64), 1, 1, NULL)",
                params![thread, root],
            )
            .unwrap();
        for index in 0..3 {
            self.conn
                .execute(
                    "INSERT INTO thread_start_untracked(thread_id, path, kind, executable, content) \
                     VALUES (?1, ?2, 'file', 0, zeroblob(16))",
                    params![thread, format!("file-{index}")],
                )
                .unwrap();
        }
    }

    fn usage(&self, request: &str, run: &str, parent: Option<&str>, thread: &str) {
        self.conn
            .execute(
                "INSERT INTO usage_requests(request_id, at_ns, provider, model, run_id, parent_run_id, \
                 status) VALUES (?1, 1, 'p', 'm', ?2, ?3, 'ok')",
                params![request, run, parent],
            )
            .unwrap();
        self.conn
            .execute(
                "INSERT OR IGNORE INTO usage_run_threads(run_id, thread_id) VALUES (?1, ?2)",
                params![run, thread],
            )
            .unwrap();
    }

    fn tombstones(&self) -> i64 {
        self.count(
            "SELECT (SELECT COUNT(*) FROM thread_tombstones) + (SELECT COUNT(*) FROM run_tombstones) \
             + (SELECT COUNT(*) FROM task_tombstones)",
        )
    }
}

fn sidebar(archived: &[&str], others: &[&str]) -> RetentionInput {
    let thread = |id: &str, archived: bool| SidebarThread {
        thread_id: id.to_owned(),
        archived,
        archived_at_s: None,
        pinned: false,
        updated_at_s: 1,
        created_at_s: 1,
        archive_revision: None,
        pin_revision: None,
    };
    RetentionInput {
        threads: archived
            .iter()
            .map(|id| thread(id, true))
            .chain(others.iter().map(|id| thread(id, false)))
            .collect(),
        sidebar_path: "/sidebar.json".into(),
        ownership_root: "/root".into(),
        now_ns: NOW_NS,
    }
}

fn binding() -> BindingInput {
    BindingInput {
        ownership_root: "/root".into(),
        sidebar_path: "/sidebar.json".into(),
    }
}

fn policy() -> ThreadRetentionPolicy {
    ThreadRetentionPolicy::default()
}

/// Explores until no family is still being explored.
fn settle(
    database: &Database,
    input: &RetentionInput,
    policy: &ThreadRetentionPolicy,
    budget: ExploreBudget,
) -> Vec<(String, FamilyState)> {
    for _ in 0..1000 {
        let states = database.retention_explore(input, policy, budget).unwrap();
        if states
            .iter()
            .all(|(_, state)| !matches!(state, FamilyState::Exploring { .. }))
        {
            return states;
        }
    }
    panic!("exploration did not settle");
}

fn finalize(database: &Database, key: &str, input: &RetentionInput) -> FinalizeOutcome {
    database
        .retention_finalize(
            key,
            input,
            &policy(),
            &BTreeMap::new(),
            &binding(),
            BIG_FINALIZE,
        )
        .unwrap()
}

/// A fork family: P (root run rp, child run rp2 delegated, usage child ru), C a fork of P
/// named root run rc, E an escalation requested by rp.
fn build_family(f: &Fixture) {
    f.thread("P", None, Some("rp"), None);
    f.thread("C", Some("P"), None, None);
    f.thread("E", Some("P"), Some("re"), Some("rp"));
    f.run("rp", None, Some("P"), "chat:Worker:P");
    f.run("rp2", Some("rp"), Some("P"), "child");
    f.run("rc", None, None, "chat:Worker:C");
    f.run("re", None, Some("E"), "chat:Orchestrator:E");
    f.usage("req-1", "ru", Some("rp"), "P");
    f.usage("req-2", "rp", None, "P");
    for run in ["rp", "rp2", "rc", "re"] {
        f.run_payload(run, "session-1");
    }
    f.todo_event("session-1", "P");
    f.todo_event("session-1", "C");
    f.task("task-1", "rp");
    f.task("task-2", "rp2");
    f.sql("INSERT INTO task_links(blocker_id, blocked_id) VALUES ('task-1', 'task-2');");
    f.start_state("P", "rp");
    f.start_state("C", "rc");
    // An unrelated live thread with the same kinds of rows.
    f.thread("X", None, Some("rx"), None);
    f.sql("UPDATE threads SET archived = 0 WHERE thread_id = 'X'");
    f.run("rx", None, Some("X"), "chat:Worker:X");
    f.run_payload("rx", "session-1");
    f.todo_event("session-1", "X");
    f.task("task-x", "rx");
    f.start_state("X", "rx");
}

fn confirm_family(f: &Fixture) -> Database {
    let database = f.database();
    let input = sidebar(&["P", "C", "E"], &["X"]);
    let states = settle(&database, &input, &policy(), BIG);
    let explored: Vec<_> = states
        .iter()
        .filter(|(_, state)| matches!(state, FamilyState::Explored { .. }))
        .collect();
    assert_eq!(explored.len(), 1, "{states:?}");
    assert_eq!(explored[0].0, "C");
    match &explored[0].1 {
        FamilyState::Explored { threads, runs } => {
            assert_eq!(threads, &["C", "E", "P"]);
            assert_eq!(
                runs,
                &["rc", "re", "ru", "rp", "rp2"]
                    .map(String::from)
                    .iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            );
        }
        other => panic!("{other:?}"),
    }
    match finalize(&database, "C", &input) {
        FinalizeOutcome::Confirmed(family) => {
            assert_eq!(family.threads, ["C", "E", "P"]);
            assert_eq!(family.tasks, ["task-1", "task-2"]);
            assert!(family.estimate_bytes > 0);
        }
        other => panic!("{other:?}"),
    }
    database
}

fn run_ids(f: &Fixture) -> &'static [&'static str] {
    let _ = f;
    &["rp", "rp2", "rc", "re", "ru"]
}

const FAMILY_RUN_TABLES: &[(&str, &str)] = &[
    ("events", "run_id"),
    ("run_contexts", "run_id"),
    ("run_context_provenance", "run_id"),
    ("run_ledger", "run_id"),
    ("host_input_receipts", "run_id"),
    ("review_evidence", "target_run_id"),
    ("review_lineage_edges", "child_run_id"),
    ("assistant_references", "source_run_id"),
    ("user_questions", "run_id"),
    ("user_question_links", "run_id"),
    ("conversation_blocks", "run_id"),
];

fn family_residue(f: &Fixture) -> Vec<(String, i64)> {
    let runs = run_ids(f)
        .iter()
        .map(|run| format!("'{run}'"))
        .collect::<Vec<_>>()
        .join(",");
    let mut checks: Vec<(String, String)> = FAMILY_RUN_TABLES
        .iter()
        .map(|(table, column)| {
            (
                (*table).to_owned(),
                format!("SELECT COUNT(*) FROM {table} WHERE {column} IN ({runs})"),
            )
        })
        .collect();
    for (label, sql) in [
        ("thread_index_runs", format!("SELECT COUNT(*) FROM thread_index_runs WHERE run_id IN ({runs}) AND state <> 'deleted'")),
        ("threads", "SELECT COUNT(*) FROM threads WHERE thread_id IN ('P','C','E')".to_owned()),
        ("thread_runs", "SELECT COUNT(*) FROM thread_runs WHERE thread_id IN ('P','C','E')".to_owned()),
        ("tasks", "SELECT COUNT(*) FROM tasks WHERE id IN ('task-1','task-2')".to_owned()),
        ("task_links", "SELECT COUNT(*) FROM task_links WHERE blocker_id IN ('task-1','task-2')".to_owned()),
        ("task_queue_ledger", "SELECT COUNT(*) FROM task_queue_ledger WHERE json_extract(payload, '$.id') IN ('task-1','task-2')".to_owned()),
        ("thread_start_states", "SELECT COUNT(*) FROM thread_start_states WHERE thread_id IN ('P','C','E')".to_owned()),
        ("thread_start_untracked", "SELECT COUNT(*) FROM thread_start_untracked WHERE thread_id IN ('P','C','E')".to_owned()),
        ("todo events", "SELECT COUNT(*) FROM events WHERE subkind = 'ThreadTodoUpdated' AND json_extract(payload, '$.payload.payload.snapshot.thread_id') IN ('P','C','E')".to_owned()),
    ] {
        checks.push((label.to_owned(), sql));
    }
    checks
        .into_iter()
        .map(|(label, sql)| (label, f.count(&sql)))
        .filter(|(_, count)| *count > 0)
        .collect()
}

fn family_rows(f: &Fixture) -> i64 {
    family_residue(f).iter().map(|(_, count)| count).sum()
}

fn survivor_rows(f: &Fixture) -> Vec<i64> {
    vec![
        f.count("SELECT COUNT(*) FROM threads WHERE thread_id = 'X'"),
        f.count("SELECT COUNT(*) FROM run_contexts WHERE run_id = 'rx'"),
        f.count("SELECT COUNT(*) FROM events WHERE run_id = 'rx'"),
        f.count("SELECT COUNT(*) FROM events WHERE subkind = 'ThreadTodoUpdated' AND json_extract(payload, '$.payload.payload.snapshot.thread_id') = 'X'"),
        f.count("SELECT COUNT(*) FROM conversation_blocks WHERE run_id = 'rx'"),
        f.count("SELECT COUNT(*) FROM tasks WHERE id = 'task-x'"),
        f.count("SELECT COUNT(*) FROM task_queue_ledger WHERE json_extract(payload, '$.id') = 'task-x'"),
        f.count("SELECT COUNT(*) FROM thread_start_states WHERE thread_id = 'X'"),
        f.count("SELECT COUNT(*) FROM run_ledger WHERE run_id = 'rx'"),
    ]
}

fn kept_snapshot(f: &Fixture) -> Vec<i64> {
    vec![
        f.count("SELECT COUNT(*) FROM usage_requests"),
        f.count("SELECT COUNT(*) FROM usage_run_threads"),
        f.count("SELECT COUNT(*) FROM sessions"),
    ]
}

#[test]
fn a_confirmed_family_is_reclaimed_completely_and_the_rest_is_untouched() {
    // Given: a fork/escalation family with rows in every reclaimed table, an
    // unrelated live thread, and kept usage rows.
    let f = Fixture::new();
    build_family(&f);
    let survivors_before = survivor_rows(&f);
    let kept_before = kept_snapshot(&f);
    assert!(family_rows(&f) > 40);

    // When: the family is confirmed and the writer reclaims it step by step.
    let database = confirm_family(&f);
    assert_eq!(
        database.reclaim_summary().unwrap().jobs_db_pending,
        1,
        "confirmation queues exactly one job"
    );
    drop(database);
    let storage = Storage::open(config(&f.path)).unwrap();
    let handle = storage.handle();
    let mut previous_rows = 0;
    for _ in 0..10_000 {
        let progress = handle
            .reclaim_step(ReclaimBudget {
                max_transactions: 1,
            })
            .unwrap();
        assert_eq!(progress.last_error, None);
        let rows: i64 = f.count("SELECT reclaimed_rows FROM reclaim_jobs");
        assert!(rows >= previous_rows, "counters never go back");
        previous_rows = rows;
        if progress.jobs_db_pending == 0 {
            break;
        }
    }
    storage.close();

    // Then: every family row is gone, the survivors and kept tables are intact,
    // and the derived structures agree with their sources.
    assert_eq!(family_residue(&f), vec![]);
    assert_eq!(survivor_rows(&f), survivors_before);
    assert_eq!(kept_snapshot(&f), kept_before);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM reclaim_jobs WHERE state = 'db_done'"),
        1
    );
    let violations: i64 = f
        .conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    f.sql("INSERT INTO conversation_fts(conversation_fts) VALUES('integrity-check');");
    assert_eq!(
        f.count("SELECT COALESCE(SUM(payload_bytes), 0) FROM event_session_bytes"),
        f.count("SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events WHERE session_id IS NOT NULL")
    );
    assert_eq!(
        f.count("SELECT COALESCE(SUM(payload_bytes), 0) FROM event_day_bytes"),
        f.count("SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events")
    );
    assert_eq!(
        f.count("SELECT total_event_bytes FROM sessions WHERE id = 'session-1'"),
        f.count("SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events WHERE session_id = 'session-1'")
    );
    assert_eq!(
        f.count("SELECT COALESCE(SUM(charged_bytes), 0) FROM thread_index_runs"),
        f.count("SELECT COALESCE(SUM(OCTET_LENGTH(text)), 0) FROM conversation_blocks")
    );
    let deleted: i64 = f.count("SELECT COUNT(*) FROM thread_index_runs WHERE state = 'deleted'");
    assert_eq!(
        deleted, 4,
        "the four indexed family runs end as deleted rows"
    );
    // And the fence stays: tombstones remain.
    assert_eq!(f.count("SELECT COUNT(*) FROM thread_tombstones"), 3);

    // The files hand-off is the caller's: the job moves on request.
    let storage = Storage::open(config(&f.path)).unwrap();
    assert!(
        storage
            .handle()
            .reclaim_mark_files("C", FilesOutcome::Pending)
            .unwrap()
    );
    assert!(
        storage
            .handle()
            .reclaim_mark_files("C", FilesOutcome::Done)
            .unwrap()
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM reclaim_jobs WHERE state = 'done'"),
        1
    );
    storage.close();
}

type Mutation = Box<
    dyn Fn(
        &Fixture,
        &mut RetentionInput,
        &mut ThreadRetentionPolicy,
        &mut BTreeMap<String, String>,
    ),
>;
type Case = (&'static str, Mutation, BlockReason);

#[test]
fn protections_keep_a_family_from_being_confirmed() {
    let cases: Vec<Case> = vec![
        (
            "pinned",
            Box::new(|_, input, _, _| input.threads[0].pinned = true),
            BlockReason::Pinned,
        ),
        (
            "eval sample",
            Box::new(|f, _, _, _| {
                f.sql(
                    "INSERT INTO thread_start_states(thread_id, root_run_id, project_root, repo_prefix, \
                     head_commit, diff, captured_at_ns, replayable, reason) \
                     VALUES ('A', 'ra', '/p', '', 'abc', zeroblob(4), 1, 1, NULL); \
                     INSERT INTO eval_samples(thread_id, problem, focus_roles, created_at_ns, updated_at_ns) \
                     VALUES ('A', 'p', '[]', 1, 1);",
                );
            }),
            BlockReason::EvalSample,
        ),
        (
            "family member not archived",
            Box::new(|_, input, _, _| input.threads[1].archived = false),
            BlockReason::FamilyMemberNotDue("B".into()),
        ),
        (
            "not yet due",
            Box::new(|_, input, _, _| {
                for thread in &mut input.threads {
                    thread.archived_at_s = Some(NOW_NS / 1_000_000_000 - 86_400);
                }
            }),
            BlockReason::FamilyMemberNotDue("A".into()),
        ),
        (
            "legacy by last activity",
            Box::new(|_, input, policy, _| {
                policy.legacy = LegacyArchivedPolicy::ByLastActivity;
                for thread in &mut input.threads {
                    thread.updated_at_s = NOW_NS / 1_000_000_000 - 3_600;
                }
            }),
            BlockReason::FamilyMemberNotDue("A".into()),
        ),
        (
            "unknown to the sidebar",
            Box::new(|_, input, _, _| {
                input.threads.remove(1);
            }),
            BlockReason::FamilyUnresolved("B".into()),
        ),
    ];
    for (label, mutate, expected) in cases {
        // Given: two forked, archived threads in one family.
        let f = Fixture::new();
        f.thread("A", None, Some("ra"), None);
        f.thread("B", Some("A"), Some("rb"), None);
        f.run("ra", None, Some("A"), "chat:Worker:A");
        f.run("rb", None, Some("B"), "chat:Worker:B");
        let database = f.database();
        let mut input = sidebar(&["A", "B"], &[]);
        let mut policy = policy();
        let mut owners = BTreeMap::new();
        mutate(&f, &mut input, &mut policy, &mut owners);

        // When: the family is explored and finalised.
        let key = settle(&database, &input, &policy, BIG)
            .first()
            .map_or_else(|| "A".to_owned(), |(key, _)| key.clone());
        let outcome = database
            .retention_finalize(&key, &input, &policy, &owners, &binding(), BIG_FINALIZE)
            .unwrap();

        // Then: it is blocked and nothing was written.
        assert_eq!(outcome, FinalizeOutcome::Blocked(expected), "{label}");
        assert_eq!(f.tombstones(), 0, "{label}");
        assert_eq!(f.count("SELECT COUNT(*) FROM threads"), 2, "{label}");
    }
}

#[test]
fn owners_disabled_policy_and_unfinished_backfill_block_confirmation() {
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    let database = f.database();
    let input = sidebar(&["A"], &[]);
    settle(&database, &input, &policy(), BIG);

    // An owner holds the thread.
    let owners = BTreeMap::from([("A".to_owned(), "running".to_owned())]);
    let outcome = database
        .retention_finalize("A", &input, &policy(), &owners, &binding(), BIG_FINALIZE)
        .unwrap();
    assert_eq!(
        outcome,
        FinalizeOutcome::Blocked(BlockReason::Owner("running".into()))
    );

    // Days = 0 confirms nothing and drops the staging.
    let disabled = ThreadRetentionPolicy {
        archived_delete_after_days: 0,
        ..policy()
    };
    assert_eq!(
        database
            .retention_finalize(
                "A",
                &input,
                &disabled,
                &BTreeMap::new(),
                &binding(),
                BIG_FINALIZE
            )
            .unwrap(),
        FinalizeOutcome::Disabled
    );
    assert!(
        database
            .retention_explore(&input, &disabled, BIG)
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM retention_staging_state"), 0);

    // An unconverted event holds everything back until the backfill is done.
    f.sql("INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload) VALUES (1, 1, 1, 'Message', '{}');");
    let blocked = database.retention_explore(&input, &policy(), BIG).unwrap();
    assert_eq!(
        blocked,
        [("A".to_owned(), FamilyState::Blocked(BlockReason::Exploring))]
    );
    assert_eq!(
        finalize(&database, "A", &input),
        FinalizeOutcome::Blocked(BlockReason::Exploring)
    );
    let progress = database.retention_backfill_startup(100, 1_000).unwrap();
    assert_eq!(progress.converted, 1);
    assert_eq!(progress.remaining_unconverted, 0);
    settle(&database, &input, &policy(), BIG);
    assert!(matches!(
        finalize(&database, "A", &input),
        FinalizeOutcome::Confirmed(_)
    ));
    assert_eq!(
        database
            .retention_binding()
            .unwrap()
            .map(|bound| bound.ownership_root),
        Some("/root".to_owned())
    );
}

#[test]
fn a_family_that_grows_after_exploration_is_stale_and_confirmed_after_re_exploring() {
    // Given: an explored single-thread family.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    let database = f.database();
    let input = sidebar(&["A", "B"], &[]);
    settle(&database, &input, &policy(), BIG);

    // When: a child thread, a child run and a new binding appear.
    f.thread("B", Some("A"), Some("rb"), None);
    f.run("rb", None, Some("B"), "chat:Worker:B");
    f.run("ra-child", Some("ra"), None, "child");
    f.run("ra-bound", None, Some("A"), "other");
    let outcome = finalize(&database, "A", &input);

    // Then: it reports the growth and confirms nothing.
    assert!(
        matches!(outcome, FinalizeOutcome::Stale { new_members } if new_members >= 3),
        "{outcome:?}"
    );
    assert_eq!(f.tombstones(), 0);

    // And: after exploring again the additions are part of the confirmed family.
    settle(&database, &input, &policy(), BIG);
    let key = f.count("SELECT COUNT(DISTINCT family_key) FROM retention_staging");
    assert_eq!(key, 1);
    match finalize(&database, "A", &input) {
        FinalizeOutcome::Confirmed(family) => {
            assert_eq!(family.threads, ["A", "B"]);
            assert_eq!(family.runs, ["ra", "ra-bound", "ra-child", "rb"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_large_family_needs_the_size_proportional_budget_and_a_failed_try_changes_nothing() {
    // Given: one thread with 300 delegated runs.
    let f = Fixture::new();
    f.thread("A", None, Some("root"), None);
    f.run("root", None, Some("A"), "chat:Worker:A");
    for index in 0..300 {
        f.run(
            &format!("child-{index:03}"),
            Some("root"),
            Some("A"),
            "child",
        );
    }
    let database = f.database();
    let input = sidebar(&["A"], &[]);
    settle(&database, &input, &policy(), BIG);
    let staged_before = f.count("SELECT COUNT(*) FROM retention_staging");

    // When: the ordinary budget is too small.
    let outcome = database
        .retention_finalize(
            "A",
            &input,
            &policy(),
            &BTreeMap::new(),
            &binding(),
            FinalizeBudget { max_edges: 100 },
        )
        .unwrap();

    // Then: nothing changed, and the proportional budget confirms.
    assert!(
        matches!(outcome, FinalizeOutcome::OverBudget { members, .. } if members == 302),
        "{outcome:?}"
    );
    assert_eq!(f.tombstones(), 0);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM retention_staging"),
        staged_before
    );
    assert!(matches!(
        database
            .retention_finalize(
                "A",
                &input,
                &policy(),
                &BTreeMap::new(),
                &binding(),
                BIG_FINALIZE
            )
            .unwrap(),
        FinalizeOutcome::Confirmed(_)
    ));
}

#[test]
fn exploration_resumes_across_calls_with_a_small_budget() {
    let f = Fixture::new();
    f.thread("A", None, Some("root"), None);
    f.run("root", None, Some("A"), "chat:Worker:A");
    for index in 0..30 {
        f.run(
            &format!("child-{index:02}"),
            Some("root"),
            Some("A"),
            "child",
        );
    }
    let database = f.database();
    let input = sidebar(&["A"], &[]);
    let first = database
        .retention_explore(&input, &policy(), ExploreBudget { max_edges: 5 })
        .unwrap();
    assert!(matches!(first[0].1, FamilyState::Exploring { explored_edges } if explored_edges > 0));
    let settled = settle(&database, &input, &policy(), ExploreBudget { max_edges: 5 });
    assert!(matches!(&settled[0].1, FamilyState::Explored { runs, .. } if runs.len() == 31));
}

#[test]
fn colliding_families_merge_under_the_smaller_key() {
    // Given: two archived threads that only meet through a shared run.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.thread("Z", None, Some("rz"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.run("rz", None, Some("Z"), "chat:Worker:Z");
    f.run("shared", Some("ra"), Some("A"), "child");
    f.sql("INSERT INTO usage_requests(request_id, at_ns, provider, model, run_id, parent_run_id, status) VALUES ('u', 1, 'p', 'm', 'shared', 'rz', 'ok');");
    let database = f.database();
    let input = sidebar(&["A", "Z"], &[]);

    // When: both are explored.
    let states = settle(&database, &input, &policy(), ExploreBudget { max_edges: 3 });

    // Then: one family under the smaller key holds both threads.
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0].0, "A");
    assert!(
        matches!(&states[0].1, FamilyState::Explored { threads, .. } if threads == &["A", "Z"])
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM retention_staging_state"), 1);
}

#[test]
fn schedule_reports_due_dates_and_protections() {
    let f = Fixture::new();
    f.thread("old", None, None, None);
    f.thread("fresh", None, None, None);
    f.thread("legacy", None, None, None);
    f.thread("held", None, None, None);
    let database = f.database();
    let now_s = NOW_NS / 1_000_000_000;
    let mut input = sidebar(&["old", "fresh", "legacy", "held"], &["live"]);
    input.threads[0].archived_at_s = Some(now_s - 40 * 86_400);
    input.threads[1].archived_at_s = Some(now_s - 86_400);
    input.threads[0].updated_at_s = now_s - 40 * 86_400;
    input.threads[1].updated_at_s = now_s - 86_400;
    let overrides = BTreeMap::from([("held".to_owned(), BlockReason::Owner("running".into()))]);

    let statuses = database
        .retention_schedule(&input, &policy(), &overrides)
        .unwrap();

    assert_eq!(statuses[0].1, RetentionStatus::DueNextStartup);
    assert_eq!(statuses[1].1, RetentionStatus::DueIn { days: 29 });
    assert_eq!(
        statuses[2].1,
        RetentionStatus::DueNextStartup,
        "legacy is immediate"
    );
    assert_eq!(
        statuses[3].1,
        RetentionStatus::Blocked(BlockReason::Owner("running".into()))
    );
    assert_eq!(statuses[4].1, RetentionStatus::NotArchived);
    let disabled = ThreadRetentionPolicy {
        archived_delete_after_days: 0,
        ..policy()
    };
    assert_eq!(
        database
            .retention_schedule(&input, &disabled, &BTreeMap::new())
            .unwrap()[0]
            .1,
        RetentionStatus::Disabled
    );
}

#[test]
fn the_fence_rejects_writers_but_not_deletes_after_confirmation() {
    // Given: a confirmed family whose rows are not reclaimed yet.
    let f = Fixture::new();
    build_family(&f);
    let database = confirm_family(&f);
    drop(database);
    let reject = |sql: &str| {
        let error = f.conn.execute_batch(sql).unwrap_err().to_string();
        assert!(error.contains("tombstoned"), "{sql}: {error}");
    };

    // When/Then: inserting or reviving rows of a tombstoned identity fails.
    reject(
        "INSERT INTO run_contexts(run_id, role, name, config_json, messages_json, checkpoints_json, terminal_phase, restorable, updated_at_ns) VALUES ('rp', 'W', 'n', '{}', '[]', '[]', 'Done', 1, 1);",
    );
    reject(
        "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json, checkpoints_json, terminal_phase, restorable, updated_at_ns) VALUES ('fresh', 'W', 'n', 'rp', '{}', '[]', '[]', 'Done', 1, 1);",
    );
    reject("INSERT INTO run_ledger(run_id, body, created_at_ns) VALUES ('rp', 'b', 1);");
    reject(
        "INSERT INTO run_context_provenance(run_id, snapshot_revision, snapshot_sha256, format_version, status, reason_code, evidence_json, map_json) VALUES ('rp2', 0, 'x', 1, 'unknown', 'missing_sidecar', NULL, '{}');",
    );
    reject(
        "INSERT INTO host_input_receipts(prompt_id, run_id, root_run_id, text, status, accepted_at_ns, status_at_ns) VALUES ('n', 'rp', 'rp', 't', 'accepted', 1, 1);",
    );
    reject(
        "INSERT INTO review_evidence(id, root_run_id, target_run_id, kind, text, created_at_ns) VALUES ('n', 'rp', 'rx', 'k', 't', 1);",
    );
    reject(
        "INSERT INTO review_lineage_edges(child_run_id, parent_run_id, created_at_ns) VALUES ('rx', 'rp', 1);",
    );
    reject(
        "INSERT INTO assistant_references(reference_id, source_run_id, turn_end, text, consumed, created_at_ns) VALUES ('n', 'rp', 1, 't', 0, 1);",
    );
    reject(
        "INSERT INTO user_questions(id, run_id, root_run_id, root_name, payload) VALUES ('n', 'rp', 'rp', 'r', '{}');",
    );
    reject("INSERT INTO user_question_links(question_id, run_id) VALUES ('q-rx', 'rp');");
    reject(
        "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, role, kind, truncated, hash, text) VALUES ('rp', 'message', '9:0', 9, 0, 'user', 'text', 0, 'h', 't');",
    );
    reject(
        "INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth) VALUES ('rnew', 'P', 'rnew', 0);",
    );
    reject(
        "INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth) VALUES ('rnew', 'X', 'rp', 0);",
    );
    reject(
        "INSERT INTO thread_start_states(thread_id, root_run_id, project_root, captured_at_ns, replayable, reason) VALUES ('P', 'rp', '/p', 1, 0, 'no_commit');",
    );
    reject("INSERT INTO usage_run_threads(run_id, thread_id) VALUES ('rnew2', 'P');");
    reject(
        "INSERT INTO threads(thread_id, created_at_ns, updated_at_ns, source) VALUES ('P', 1, 1, 'host');",
    );
    reject(
        "INSERT INTO threads(thread_id, parent_thread_id, created_at_ns, updated_at_ns, source) VALUES ('new', 'P', 1, 1, 'host');",
    );
    reject(
        "INSERT INTO threads(thread_id, escalation_source_run_id, created_at_ns, updated_at_ns, source) VALUES ('new', 'rp', 1, 1, 'host');",
    );
    reject(
        "INSERT INTO tasks(id, status, created_at_ns, updated_at_ns) VALUES ('task-1', 'pending', 1, 1);",
    );
    reject(
        "INSERT INTO tasks(id, status, created_at_ns, updated_at_ns, parent_run_id) VALUES ('t-new', 'pending', 1, 1, 'rp');",
    );
    reject(
        "INSERT INTO task_queue_ledger(operation, payload) VALUES ('start', '{\"id\":\"task-1\"}');",
    );
    reject(
        "INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, run_id) VALUES (1, 1, 1, 'Message', '{}', 'MessageCompleted', 'rp');",
    );
    reject(
        "INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision, source_context_sha256, extractor_version, accounting_version, source_updated_at_ns, retention_at_ns) VALUES ('rnew3', 'pending', 0, 0, 'x', 1, 0, 1, 1); UPDATE thread_index_runs SET state = 'pending' WHERE run_id = 'rp';",
    );
    // A write that touches only unrelated identities is unaffected.
    f.sql("INSERT INTO run_ledger(run_id, body, created_at_ns) VALUES ('rx', 'still fine', 1);");
    // Deleting is always allowed: the ledger only with a grant.
    f.sql("DELETE FROM review_evidence WHERE target_run_id = 'rp';");
    assert!(
        f.conn
            .execute_batch("DELETE FROM run_ledger WHERE run_id = 'rp';")
            .is_err()
    );
    // The reclaim terminalisation of an index row passes the fence.
    f.sql(
        "UPDATE thread_index_runs SET state = 'deleted', source_present = 0 WHERE run_id = 'rp';",
    );
}

#[test]
fn appends_for_deleted_runs_are_refused_without_side_effects() {
    // Given: a confirmed family and a writer opened afterwards.
    let f = Fixture::new();
    build_family(&f);
    drop(confirm_family(&f));
    let storage = Storage::open(config(&f.path)).unwrap();
    let handle = storage.handle();
    let before = f.count("SELECT COUNT(*) FROM events");
    let session_before = f.count("SELECT total_event_bytes FROM sessions WHERE id = 'session-1'");
    let day_before = f.count("SELECT COALESCE(SUM(payload_bytes), 0) FROM event_day_bytes");
    let completed = |run: &str| {
        Event::new(event_bus::MessageEvent::MessageCompleted {
            run_id: run.to_owned(),
            text: "hello".into(),
        })
    };

    // When: an event of a deleted run and one of a live run are appended.
    let refused = handle.append_event(Some("session-1"), &completed("rp"));
    let accepted = handle.append_event(Some("session-1"), &completed("rx"));

    // Then: the first is refused and leaves no trace; the second is stored.
    assert_eq!(
        refused,
        Err(StorageError::Tombstoned {
            run_id: Some("rp".into()),
            thread_id: None
        })
    );
    assert!(accepted.is_ok(), "{accepted:?}");
    assert_eq!(f.count("SELECT COUNT(*) FROM events"), before + 1);
    assert!(
        f.count("SELECT total_event_bytes FROM sessions WHERE id = 'session-1'") > session_before
    );
    assert!(f.count("SELECT COALESCE(SUM(payload_bytes), 0) FROM event_day_bytes") > day_before);
    let todo = Event::new(OrchestratorEvent::ThreadTodoUpdated {
        snapshot: ThreadTodoSnapshot {
            list_id: "l".into(),
            thread_id: "P".into(),
            revision: 2,
            items: vec![],
        },
    });
    assert!(matches!(
        handle.append_event(None, &todo),
        Err(StorageError::Tombstoned { thread_id: Some(thread), .. }) if thread == "P"
    ));
    assert_eq!(handle.statistics().unwrap().events_rejected_tombstoned, 2);
    assert_eq!(handle.statistics().unwrap().event_write_failures, 0);

    // And: a host mirror of a deleted thread is skipped while the rest is stored.
    let upsert = |id: &str| storage::threads::ThreadUpsert {
        thread_id: id.to_owned(),
        created_at_ns: 1,
        updated_at_ns: 2,
        ..Default::default()
    };
    let skipped = handle
        .upsert_threads_host(&[upsert("P"), upsert("brand-new")])
        .unwrap();
    assert_eq!(skipped, 1);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM threads WHERE thread_id = 'brand-new'"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM threads WHERE thread_id = 'P'"),
        0
    );
    storage.close();
}

#[test]
fn readers_do_not_see_tombstoned_threads_and_runs() {
    let f = Fixture::new();
    build_family(&f);
    let database = confirm_family(&f);

    assert_eq!(database.thread("P").unwrap(), None);
    assert!(database.thread("X").unwrap().is_some());
    assert!(
        database
            .threads_in_project("/p", None, 50)
            .unwrap()
            .threads
            .iter()
            .all(|row| row.thread_id == "X")
    );
    assert!(database.thread_children("P").unwrap().is_empty());
    assert_eq!(database.thread_of_run("rp").unwrap(), None);
    assert!(database.runs_of_thread("P").unwrap().is_empty());
    assert_eq!(database.run_context("rp").unwrap(), None);
    assert_eq!(database.run_context_meta("rp").unwrap(), None);
    assert!(database.run_context("rx").unwrap().is_some());
    assert!(
        database
            .run_contexts_for(&["rp".into(), "rx".into()])
            .unwrap()
            .len()
            == 1
    );
    assert!(database.latest_ledgers(&["rp".into()]).unwrap().is_empty());
    assert!(database.run_ledger("rp").unwrap().is_empty());
    assert!(
        database
            .host_input_receipts_for_run("rp")
            .unwrap()
            .is_empty()
    );
    assert!(
        database
            .events_for_runs(&["rp".into()], 0, 100)
            .unwrap()
            .0
            .is_empty()
    );
    let mut seen = Vec::new();
    database
        .for_each_event_ordered(|stored| seen.push(stored))
        .unwrap();
    assert!(
        seen.iter()
            .all(|stored| stored.event.kind.primary_run_id() != Some("rp"))
    );
    assert!(database.orchestrator_events_ordered().unwrap().iter().all(|stored| !matches!(
        &stored.event.kind,
        event_bus::EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated { snapshot }) if snapshot.thread_id != "X"
    )));
    assert_eq!(database.tombstones().unwrap().threads.len(), 3);
}

#[test]
fn oversized_rows_wait_for_the_exclusive_step_which_deletes_at_least_one() {
    // Given: a confirmed single-thread family with a 5 MiB run context.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.run("rb", Some("ra"), Some("A"), "child");
    f.sql("UPDATE run_contexts SET messages_json = zeroblob(5242880) WHERE run_id = 'ra'");
    let database = f.database();
    let input = sidebar(&["A"], &[]);
    settle(&database, &input, &policy(), BIG);
    assert!(matches!(
        finalize(&database, "A", &input),
        FinalizeOutcome::Confirmed(_)
    ));
    drop(database);
    let storage = Storage::open(config(&f.path)).unwrap();

    // When: the bounded steps run.
    let progress = storage
        .handle()
        .reclaim_step(ReclaimBudget::default())
        .unwrap();
    storage.close();

    // Then: the small row is gone, the large one is registered, the job waits.
    assert_eq!(progress.oversized_pending, 1);
    assert_eq!(
        (progress.jobs_db_pending, progress.jobs_waiting_exclusive),
        (0, 1)
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM run_contexts"), 1);
    assert_eq!(f.count("SELECT COUNT(*) FROM reclaim_oversized"), 1);

    // And: a failing delete leaves the registration; a retry then succeeds even
    // with a zero budget.
    let database = f.database();
    f.sql("CREATE TRIGGER inject BEFORE DELETE ON run_contexts BEGIN SELECT RAISE(ABORT, 'injected'); END;");
    let failed = database.reclaim_oversized_step(0).unwrap();
    assert!(
        failed
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("injected"))
    );
    assert_eq!((failed.rows, failed.remaining), (0, 1));
    f.sql("DROP TRIGGER inject;");
    let done = database.reclaim_oversized_step(0).unwrap();
    assert_eq!((done.rows, done.remaining), (1, 0));
    assert_eq!(f.count("SELECT COUNT(*) FROM run_contexts"), 0);
    assert_eq!(database.reclaim_summary().unwrap().jobs_db_done, 1);
}

#[test]
fn a_failed_step_resumes_from_the_cursor_without_repeating_or_skipping() {
    // Given: a confirmed family and a trigger that fails deletes of one run's events.
    let f = Fixture::new();
    build_family(&f);
    drop(confirm_family(&f));
    f.sql("CREATE TRIGGER inject BEFORE DELETE ON events WHEN OLD.run_id = 're' BEGIN SELECT RAISE(ABORT, 'injected'); END;");
    let storage = Storage::open(config(&f.path)).unwrap();
    let handle = storage.handle();

    // When: reclaim runs into the failure.
    let mut failure = None;
    for _ in 0..200 {
        let progress = handle
            .reclaim_step(ReclaimBudget {
                max_transactions: 2,
            })
            .unwrap();
        if progress.last_error.is_some() {
            failure = progress.last_error;
            break;
        }
    }
    let cursor_at_failure: String = f
        .conn
        .query_row("SELECT cursor_json FROM reclaim_jobs", [], |row| row.get(0))
        .unwrap();
    let rows_at_failure = f.count("SELECT reclaimed_rows FROM reclaim_jobs");

    // Then: the failure rolled back whole; the cursor and counters did not move.
    assert!(failure.is_some_and(|error| error.contains("injected")));
    let again = handle
        .reclaim_step(ReclaimBudget {
            max_transactions: 1,
        })
        .unwrap();
    assert!(again.last_error.is_some());
    assert_eq!(
        f.count("SELECT reclaimed_rows FROM reclaim_jobs"),
        rows_at_failure
    );
    assert_eq!(
        f.conn
            .query_row::<String, _, _>("SELECT cursor_json FROM reclaim_jobs", [], |row| row.get(0))
            .unwrap(),
        cursor_at_failure
    );

    // And: once the cause is gone the remaining work completes exactly once.
    f.sql("DROP TRIGGER inject;");
    for _ in 0..10_000 {
        if handle
            .reclaim_step(ReclaimBudget::default())
            .unwrap()
            .jobs_db_pending
            == 0
        {
            break;
        }
    }
    storage.close();
    assert_eq!(family_residue(&f), vec![]);
}

fn orchestrator_row(f: &Fixture, event: OrchestratorEvent, subkind: &str) -> i64 {
    let payload = serde_json::to_string(&Event::new(event).kind).unwrap();
    f.conn
        .execute(
            "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, \
             subkind, run_id) VALUES (NULL, 1, 1, 1, 'Orchestrator', ?1, ?2, NULL)",
            params![payload, subkind],
        )
        .unwrap();
    f.conn.last_insert_rowid()
}

fn goal(root: &str) -> event_bus::ThreadGoalSnapshot {
    serde_json::from_value(serde_json::json!({
        "goal_id": "g", "thread_id": "t", "root_run_id": root,
        "related_root_run_ids": [], "objective": "o", "criteria": [], "checks": [],
        "phase": "working", "review_enabled": false, "checks_paused": false,
        "work_stopped": false, "epoch": 1, "review_round": 0, "findings": [],
        "reason": null, "usage": {"model_requests": 0, "input_tokens": 0, "output_tokens": 0},
        "max_review_rounds": 1, "max_tokens": null, "original_request": "r"
    }))
    .unwrap()
}

#[test]
fn the_second_backfill_target_attributes_orchestrator_events_without_rewriting_payloads() {
    // Given: Orchestrator events stored before they were attributed to a run, a
    // todo snapshot that never is, an unconverted row and an undecodable one.
    let f = Fixture::new();
    let progressed = orchestrator_row(
        &f,
        OrchestratorEvent::TaskProgressed {
            task_id: "t".into(),
            run_id: "run-a".into(),
            progress: serde_json::json!({}),
            reason: "r".into(),
        },
        "TaskProgressed",
    );
    let checkpoint = orchestrator_row(
        &f,
        OrchestratorEvent::TaskCheckpoint {
            task_id: "t".into(),
            run_id: "run-b".into(),
            tool_call_count: 1,
            cumulative_input_tokens: 1,
            cumulative_output_tokens: 1,
            elapsed_ms: 1,
        },
        "TaskCheckpoint",
    );
    let goal_row = orchestrator_row(
        &f,
        OrchestratorEvent::ThreadGoalUpdated {
            snapshot: goal("run-root"),
        },
        "ThreadGoalUpdated",
    );
    let todo = orchestrator_row(
        &f,
        OrchestratorEvent::ThreadTodoUpdated {
            snapshot: ThreadTodoSnapshot {
                list_id: "l".into(),
                thread_id: "t".into(),
                revision: 1,
                items: vec![],
            },
        },
        "ThreadTodoUpdated",
    );
    f.sql(
        "INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind) \
         VALUES (1, 1, 1, 'Orchestrator', '{', 'TaskProgressed');
         INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
         VALUES (1, 1, 1, 'Message', '{\"kind\":\"Message\",\"payload\":{\"kind\":\"MessageCompleted\",\"payload\":{\"run_id\":\"run-c\",\"text\":\"t\"}}}');",
    );
    let payloads = |f: &Fixture| -> Vec<String> {
        let mut statement = f
            .conn
            .prepare("SELECT payload FROM events WHERE id <= ?1 ORDER BY id")
            .unwrap();
        statement
            .query_map([todo], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = payloads(&f);
    let database = f.database();
    assert_eq!(database.unattributed_orchestrator_count().unwrap(), 4);
    assert_eq!(database.unbackfilled_event_count().unwrap(), 1);

    // When: the startup backfill runs in tiny steps.
    let progress = database.retention_backfill_startup(2, 1_000).unwrap();

    // Then: runs are attributed, todo snapshots stay unattributed, payloads are
    // byte-identical, and the undecodable row stops counting as pending.
    assert_eq!(progress.converted, 1);
    assert_eq!(progress.reattributed, 4);
    assert_eq!(
        (
            progress.remaining_unconverted,
            progress.remaining_unattributed
        ),
        (0, 0)
    );
    let run_of = |id: i64| -> Option<String> {
        f.conn
            .query_row("SELECT run_id FROM events WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(run_of(progressed).as_deref(), Some("run-a"));
    assert_eq!(run_of(checkpoint).as_deref(), Some("run-b"));
    assert_eq!(run_of(goal_row).as_deref(), Some("run-root"));
    assert_eq!(run_of(todo), None);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM events WHERE kind = 'Orchestrator' AND subkind = ''"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM events WHERE run_id = 'run-c'"),
        1
    );
    assert_eq!(payloads(&f), before);
    assert_eq!(database.unattributed_orchestrator_count().unwrap(), 0);
}

#[test]
fn the_backfill_step_is_available_on_the_writer_and_bounded_by_rows() {
    let f = Fixture::new();
    for index in 0..5 {
        orchestrator_row(
            &f,
            OrchestratorEvent::TaskProgressed {
                task_id: "t".into(),
                run_id: format!("run-{index}"),
                progress: serde_json::json!({}),
                reason: "r".into(),
            },
            "TaskProgressed",
        );
    }
    let storage = Storage::open(config(&f.path)).unwrap();

    let first = storage.handle().retention_backfill_step(2).unwrap();
    assert_eq!((first.reattributed, first.remaining_unattributed), (2, 3));
    let second = storage.handle().retention_backfill_step(100).unwrap();
    assert_eq!((second.reattributed, second.remaining_unattributed), (3, 0));
    let idle = storage.handle().retention_backfill_step(100).unwrap();
    assert_eq!((idle.converted, idle.reattributed), (0, 0));
    storage.close();
}

#[test]
fn a_late_usage_attribution_of_a_deleted_run_does_not_lose_the_rest_of_its_batch() {
    // Given: a confirmed family and a writer.
    let f = Fixture::new();
    build_family(&f);
    drop(confirm_family(&f));
    let storage = Storage::open(config(&f.path)).unwrap();
    let handle = storage.handle();
    let attribution = |run: &str, thread: &str| storage::usage::RunAttribution {
        run_id: run.to_owned(),
        thread_id: thread.to_owned(),
        project_id: None,
    };

    // When: one batch names a deleted run and a live one.
    assert!(handle.try_attribute_usage_runs(vec![
        attribution("rp", "elsewhere"),
        attribution("run-new", "X"),
    ]));
    handle.statistics().unwrap();

    // Then: the live attribution is stored; the deleted run's is left as it was.
    assert_eq!(
        f.count(
            "SELECT COUNT(*) FROM usage_run_threads WHERE run_id = 'run-new' AND thread_id = 'X'"
        ),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM usage_run_threads WHERE run_id = 'rp' AND thread_id = 'P'"),
        1
    );
    storage.close();
}

// ---- review round: readers, closure rules, budgets, atomic staging, large rows ----

/// Rows of the tables a deletion must leave alone, as comparable text.
const KEPT_TABLES: &[&str] = &[
    "usage_requests",
    "usage_daily",
    "usage_run_threads",
    "learning_claims",
    "learning_claim_roots",
    "improvement_candidates",
    "memory_ledger",
    "memory_entries",
    "thread_index_requests",
    "thread_index_request_targets",
];

fn seed_kept(f: &Fixture) {
    f.sql(
        "INSERT INTO usage_daily(day, provider, profile, model, project_id, role, purpose, request_count)
         VALUES ('2026-01-01', 'p', '', 'm', 'proj', 'worker', 'agent', 3);
         INSERT INTO learning_claims(task_id, is_goal, utc_day) VALUES ('task-1', 0, 20000);
         INSERT INTO learning_claim_roots(task_id, run_id) VALUES ('task-1', 'rp');
         INSERT INTO improvement_candidates(candidate_id, project, created_at_ns, source, code, severity,
             title, evidence, dedup_key, run_id)
         VALUES ('cand-1', 'proj', 5, 'lesson', 'Code', 'info', 't', 'e', 'k', 'rp');
         INSERT INTO memory_ledger(entry_id, project, task_id, content, evidence, status)
         VALUES ('mem-1', 'proj', 'task-1', 'remember', 'ev', 'validated');
         INSERT INTO memory_entries(id, project, task_id, content, evidence, status, ledger_seq)
         VALUES ('mem-1', 'proj', 'task-1', 'remember', 'ev', 'validated', 1);
         INSERT INTO thread_index_requests(request_id, include_evicted, state, created_at_ns, finished_at_ns)
         VALUES ('req-a', 0, 'completed', 1, 2);
         INSERT INTO thread_index_request_targets(request_id, run_id, source_revision, extractor_version,
             accounting_version, work_generation, state) VALUES ('req-a', 'rp', 1, 1, 0, 0, 'succeeded');",
    );
}

fn dump(f: &Fixture, table: &str) -> Vec<String> {
    let mut statement = f.conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
    let columns = statement.column_count();
    let mut rows: Vec<String> = statement
        .query_map([], |row| {
            Ok((0..columns)
                .map(|index| format!("{:?}", row.get_ref(index).unwrap()))
                .collect::<Vec<_>>()
                .join("|"))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.sort();
    rows
}

fn kept_values(f: &Fixture) -> Vec<(String, Vec<String>)> {
    KEPT_TABLES
        .iter()
        .map(|table| ((*table).to_owned(), dump(f, table)))
        .collect()
}

#[test]
fn usage_and_learning_rows_are_byte_for_byte_unchanged_by_confirmation_and_reclaim() {
    // Given: non-empty usage and learning tables that mention the family's runs.
    let f = Fixture::new();
    build_family(&f);
    seed_kept(&f);
    let before = kept_values(&f);
    assert!(
        before.iter().all(|(_, rows)| !rows.is_empty()),
        "{table:?}",
        table = before.iter().find(|(_, rows)| rows.is_empty())
    );

    // When: the family is confirmed and fully reclaimed.
    drop(confirm_family(&f));
    let storage = Storage::open(config(&f.path)).unwrap();
    for _ in 0..10_000 {
        if storage
            .handle()
            .reclaim_step(ReclaimBudget::default())
            .unwrap()
            .jobs_db_pending
            == 0
        {
            break;
        }
    }
    storage.close();

    // Then: every kept value is identical, and the family is gone.
    assert_eq!(kept_values(&f), before);
    assert_eq!(family_residue(&f), vec![]);
}

#[test]
fn a_confirmed_family_is_invisible_to_every_reader_before_any_reclaim_ran() {
    // Given: a confirmed family whose rows are all still stored.
    let f = Fixture::new();
    build_family(&f);
    let database = confirm_family(&f);
    assert!(family_rows(&f) > 40, "nothing was reclaimed");

    // Then: no reader returns the family's content, live neighbours stay visible.
    assert!(
        database
            .review_evidence_for_target("rp")
            .unwrap()
            .is_empty()
    );
    assert!(database.review_evidence_for_root("rp").unwrap().is_empty());
    assert!(
        database
            .review_evidence_for_lineage(&["rp".into(), "rx".into()])
            .unwrap()
            .iter()
            .all(|row| row.target_run_id == "rx")
    );
    assert!(
        database
            .review_attachment_acceptances_for_lineage(&["rp".into()])
            .unwrap()
            .is_empty()
    );
    assert!(database.review_lineage_edges("rp").unwrap().is_empty());
    assert!(database.review_lineage_ancestors("rp").unwrap().is_empty());
    assert!(database.assistant_reference("ref-rp").unwrap().is_none());
    assert!(database.user_question("q-rp").unwrap().is_none());
    assert!(database.user_questions_for_run("rp").unwrap().is_empty());
    assert!(
        database
            .user_question_recipients("q-rp")
            .unwrap()
            .is_empty()
    );
    assert!(
        database
            .pending_user_questions()
            .unwrap()
            .iter()
            .all(|question| question.run_id == "rx")
    );
    assert!(database.thread_start_state("P").unwrap().is_none());
    assert!(
        database
            .thread_start_states_since(0)
            .unwrap()
            .iter()
            .all(|summary| summary.thread_id == "X")
    );
    assert!(
        !database
            .review_evidence_for_target("rx")
            .unwrap()
            .is_empty()
    );
    assert!(database.assistant_reference("ref-rx").unwrap().is_some());
    assert!(database.user_question("q-rx").unwrap().is_some());
    assert!(database.thread_start_state("X").unwrap().is_some());
}

/// A delivered agent message addressed to `recipient`, with its run column filled
/// in or, for a row the attribution backfill has not reached, left empty.
fn agent_message(f: &Fixture, session: &str, recipient: &str, attributed: bool) {
    let payload = serde_json::to_string(&EventKind::AgentMessage(AgentMessageEvent::Delivered {
        message: AgentMessage {
            message_id: format!("m-{recipient}-{attributed}"),
            sender_run_id: "someone".into(),
            recipient_run_id: recipient.into(),
            kind: AgentMessageKind::Send,
            content: "secret".into(),
            reply_to: None,
        },
        disposition: DeliveryDisposition::Wake,
    }))
    .unwrap();
    f.conn
        .execute(
            "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
             payload, subkind, run_id) VALUES (?1, 1, 1, ?2, 'AgentMessage', ?3, 'Delivered', ?4)",
            params![session, NOW_NS, payload, attributed.then_some(recipient)],
        )
        .unwrap();
}

#[test]
fn inspection_messages_tasks_and_search_do_not_return_a_confirmed_family_before_reclaim() {
    // Given: a family and a live neighbour, each with agent messages (attributed or
    // not), tasks in a session, and indexed conversation blocks.
    let f = Fixture::new();
    build_family(&f);
    for (run, attributed) in [("rp", true), ("rp", false), ("rx", true), ("rx", false)] {
        agent_message(&f, "session-1", run, attributed);
    }
    f.sql("UPDATE tasks SET session_id = 'session-1'");
    f.sql("UPDATE thread_index_runs SET indexed_revision = 1, indexed_extractor_version = 1");
    let database = confirm_family(&f);
    assert!(family_rows(&f) > 40, "nothing was reclaimed");

    // Then: inspection of the deleted run shows none of its stored content, and the
    // usage ledger (kept on purpose) still shows.
    let (events, _) = database
        .inspect_events(EventScope::Run("rp"), 1_000)
        .unwrap();
    assert!(events.is_empty(), "{events:?}");
    let run = database.inspect_run("rp", true).unwrap();
    assert!(run["context"].is_null(), "{run}");
    for key in ["children", "ledger", "user_questions", "diagnostics"] {
        assert_eq!(run[key].as_array().map(Vec::len), Some(0), "{key}: {run}");
    }
    assert_eq!(
        run["event_kinds"].as_object().map(|kinds| kinds.len()),
        Some(0)
    );
    assert_eq!(run["usage_requests"].as_array().map(Vec::len), Some(1));
    let live = database.inspect_run("rx", true).unwrap();
    assert!(!live["context"].is_null());
    assert!(!live["ledger"].as_array().unwrap().is_empty());
    assert!(!live["user_questions"].as_array().unwrap().is_empty());

    // And: a time window lists live events only, including todo snapshots by thread.
    let (window, _) = database
        .inspect_events(
            EventScope::Window {
                from_ns: 0,
                to_ns: i64::MAX,
            },
            10_000,
        )
        .unwrap();
    assert!(!window.is_empty());
    for event in &window {
        let text = event.payload.to_string();
        assert!(!text.contains("\"thread_id\":\"P\"") && !text.contains("\"thread_id\":\"C\""));
        assert!(!text.contains("\"recipient_run_id\":\"rp\""), "{text}");
    }
    assert!(window.iter().any(|event| event.kind == "AgentMessage"));
    let messages = database.agent_messages_by_session("session-1").unwrap();
    assert_eq!(messages.len(), 2);
    assert!(
        messages
            .iter()
            .all(|stored| stored.message.recipient_run_id == "rx")
    );

    // And: tasks of the family are gone from every task reader, the neighbour stays.
    assert!(database.task("task-1").unwrap().is_none());
    assert!(database.task("task-x").unwrap().is_some());
    let session_tasks: Vec<_> = database
        .tasks_by_session("session-1")
        .unwrap()
        .into_iter()
        .map(|task| task.id)
        .collect();
    assert_eq!(session_tasks, ["task-x"]);
    for listed in [
        database.durable_tasks().unwrap(),
        database.queued_tasks().unwrap(),
    ] {
        let ids: Vec<_> = listed.into_iter().map(|task| task.id).collect();
        assert_eq!(ids, ["task-x"]);
    }
    let links = database.task_dependencies("task-1").unwrap();
    assert!(links.blocks.is_empty() && links.blocked_by.is_empty());
    let links = database.task_dependencies("task-2").unwrap();
    assert!(links.blocks.is_empty() && links.blocked_by.is_empty());

    // And: run lookups, learning source reads and descendants skip the family.
    assert!(
        database
            .latest_terminal_run_context("chat:Worker:P")
            .unwrap()
            .is_none()
    );
    assert!(
        database
            .latest_terminal_run_context("chat:Worker:X")
            .unwrap()
            .is_some()
    );
    assert_eq!(database.run_context_parent("rp2").unwrap(), None);
    assert_eq!(database.run_context_parent("rx").unwrap(), Some(None));
    assert!(database.learning_source_contexts("rp").unwrap().is_empty());
    assert_eq!(database.learning_source_contexts("rx").unwrap().len(), 1);
    assert_eq!(
        database
            .runs_with_descendants(&["rp".into(), "rx".into()])
            .unwrap(),
        ["rx"]
    );

    // And: search candidates and scan chunks return blocks of live runs only.
    let runs = vec!["rp".to_owned(), "rx".to_owned()];
    let kinds = [IndexBlockKind::Text];
    let chunk = database
        .conversation_scan_chunk(
            &ConversationScan {
                runs: &runs,
                kinds: &kinds,
                after: None,
            },
            ScanLimits {
                max_blocks: 100,
                max_bytes: 1 << 20,
            },
        )
        .unwrap();
    assert_eq!(chunk.blocks.len(), 2);
    assert!(chunk.blocks.iter().all(|block| block.run_id == "rx"));
    let candidates = database
        .conversation_candidates(&ConversationQuery {
            runs: &runs,
            kinds: &kinds,
            filter: &ConversationFilter::All,
            after: None,
            limit: 100,
        })
        .unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().all(|block| block.run_id == "rx"));
}

#[test]
fn a_question_of_a_deleted_run_cannot_be_handed_to_a_live_run() {
    // Given: a confirmed family and a writer, with the deleted run's question unreclaimed.
    let f = Fixture::new();
    build_family(&f);
    drop(confirm_family(&f));
    let storage = Storage::open(config(&f.path)).unwrap();
    let links_before = f.count("SELECT COUNT(*) FROM user_question_links");

    // When: the question is inherited by a live run, through the API and in SQL.
    let through_api = storage
        .handle()
        .bind_user_questions("rp", "rx", &["q-rp".to_owned()]);
    let in_sql = f.conn.execute(
        "INSERT INTO user_question_links(question_id, run_id) VALUES ('q-rp', 'rx')",
        [],
    );
    storage.close();

    // Then: both are refused with a clear reason and no link appears.
    assert!(
        matches!(through_api, Err(StorageError::Tombstoned { run_id: Some(ref run), .. }) if run == "rp"),
        "{through_api:?}"
    );
    assert!(in_sql.unwrap_err().to_string().contains("tombstoned"));
    assert_eq!(
        f.count("SELECT COUNT(*) FROM user_question_links"),
        links_before
    );
}

#[test]
fn live_task_operations_continue_while_a_deleted_family_still_has_pending_tasks() {
    // Given: a confirmed family whose pending tasks are still stored, and a live task.
    let f = Fixture::new();
    build_family(&f);
    drop(confirm_family(&f));
    assert_eq!(
        f.count("SELECT COUNT(*) FROM tasks WHERE id IN ('task-1', 'task-2')"),
        2
    );
    let storage = Storage::open(config(&f.path)).unwrap();
    let handle = storage.handle();

    // When/Then: the live task starts and finishes, and a new one queues, without ABORT.
    handle.start_task("task-x").unwrap();
    handle.finish_task("task-x", true).unwrap();
    f.task("task-y", "rx");
    handle.start_task("task-y").unwrap();
    assert_eq!(
        f.count("SELECT COUNT(*) FROM tasks WHERE id = 'task-y' AND status = 'running'"),
        1
    );
    // The deleted family's tasks are left exactly as they were.
    assert_eq!(
        f.count(
            "SELECT COUNT(*) FROM tasks WHERE id IN ('task-1', 'task-2') AND status = 'pending'"
        ),
        2
    );
    storage.close();
}

#[test]
fn the_runtime_cannot_create_anything_under_a_deleted_thread_or_run() {
    let f = Fixture::new();
    build_family(&f);
    drop(confirm_family(&f));
    let storage = Storage::open(config(&f.path)).unwrap();
    let handle = storage.handle();
    let child = |id: &str| storage::threads::ThreadUpsert {
        thread_id: id.to_owned(),
        created_at_ns: 1,
        updated_at_ns: 2,
        ..Default::default()
    };

    // A new child thread of a deleted parent, a lineage from a deleted run, and
    // an update of a deleted thread are refused with an explicit error.
    let mut fork = child("fork-of-deleted");
    fork.parent_thread_id = Some("P".into());
    let mut escalation = child("escalation-of-deleted");
    escalation.escalation_source_run_id = Some("rp".into());
    for (upsert, run, thread) in [
        (fork, None, Some("P")),
        (escalation, Some("rp"), None),
        (child("C"), None, Some("C")),
    ] {
        let refused = handle.upsert_thread_runtime(&upsert);
        assert_eq!(
            refused,
            Err(StorageError::Tombstoned {
                run_id: run.map(str::to_owned),
                thread_id: thread.map(str::to_owned)
            })
        );
    }
    assert_eq!(
        f.count("SELECT COUNT(*) FROM threads WHERE thread_id LIKE '%of-deleted'"),
        0
    );
    // A thread unrelated to the deleted family is still recorded.
    handle.upsert_thread_runtime(&child("independent")).unwrap();
    assert_eq!(
        f.count("SELECT COUNT(*) FROM threads WHERE thread_id = 'independent'"),
        1
    );
    storage.close();
}

fn explore_all(
    database: &Database,
    input: &RetentionInput,
    max_edges: u64,
) -> (u64, Vec<(String, FamilyState)>) {
    let mut calls = 0;
    let mut previous: u64 = 0;
    loop {
        calls += 1;
        assert!(calls < 100_000, "exploration did not settle");
        let states = database
            .retention_explore(input, &policy(), ExploreBudget { max_edges })
            .unwrap();
        match states.as_slice() {
            [(_, FamilyState::Exploring { explored_edges })] => {
                assert!(
                    *explored_edges <= previous + max_edges + 65,
                    "a call read {} rows with a budget of {max_edges}",
                    explored_edges - previous
                );
                previous = *explored_edges;
            }
            _ => return (calls, states),
        }
    }
}

#[test]
fn a_huge_single_family_is_explored_and_confirmed_within_budgets_that_never_overrun() {
    // Given: one thread with 1,500 delegated runs.
    let f = Fixture::new();
    f.thread("A", None, Some("root"), None);
    f.run("root", None, Some("A"), "chat:Worker:A");
    f.sql(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 1500)
         INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json,
             checkpoints_json, terminal_phase, restorable, updated_at_ns)
         SELECT printf('child-%05d', x), 'Worker', 'child', 'root', '{}', '[]', '[]', 'Done', 1, 1 FROM n;",
    );
    let database = f.database();
    let input = sidebar(&["A"], &[]);

    // When: the search runs with a tiny budget per call.
    let (calls, states) = explore_all(&database, &input, 100);

    // Then: it needed many calls, each within its budget, and found the whole family.
    assert!(calls > 15, "{calls}");
    assert!(matches!(&states[0].1, FamilyState::Explored { runs, .. } if runs.len() == 1501));
    // And: the budget derived from the exploration is enough for the check.
    let budget = database.retention_finalize_budget("A").unwrap().unwrap();
    let outcome = database
        .retention_finalize("A", &input, &policy(), &BTreeMap::new(), &binding(), budget)
        .unwrap();
    assert!(
        matches!(outcome, FinalizeOutcome::Confirmed(_)),
        "{outcome:?} with {budget:?}"
    );
}

#[test]
fn a_high_fan_out_node_is_read_in_pages_and_a_doubling_retry_always_converges() {
    // Given: a root run that 3,000 usage rows name as parent of distinct runs.
    let f = Fixture::new();
    f.thread("A", None, Some("root"), None);
    f.run("root", None, Some("A"), "chat:Worker:A");
    f.sql(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 3000)
         INSERT INTO usage_requests(request_id, at_ns, provider, model, run_id, parent_run_id, status)
         SELECT printf('req-%05d', x), 1, 'p', 'm', printf('leaf-%05d', x), 'root', 'ok' FROM n;",
    );
    let database = f.database();
    let input = sidebar(&["A"], &[]);
    let (_, states) = explore_all(&database, &input, 50);
    assert!(matches!(&states[0].1, FamilyState::Explored { runs, .. } if runs.len() == 3001));

    // When: the check starts from a budget far too small.
    let mut budget = FinalizeBudget { max_edges: 50 };
    let mut attempts = 0;
    let outcome = loop {
        attempts += 1;
        let outcome = database
            .retention_finalize("A", &input, &policy(), &BTreeMap::new(), &binding(), budget)
            .unwrap();
        match outcome {
            FinalizeOutcome::OverBudget { edges_read, .. } => {
                // It stops at the budget (a canonical-thread walk is the one indivisible step).
                assert!(
                    edges_read <= budget.max_edges + 65,
                    "{edges_read} rows for {budget:?}"
                );
                assert_eq!(f.tombstones(), 0, "an overrun changes nothing");
                budget = budget.doubled();
            }
            other => break other,
        }
    };

    // Then: doubling reaches a budget that confirms.
    assert!(
        matches!(outcome, FinalizeOutcome::Confirmed(_)),
        "{outcome:?}"
    );
    assert!(attempts > 2 && attempts < 20, "{attempts}");
}

#[test]
fn a_run_named_for_a_live_thread_pulls_that_thread_into_the_family() {
    // Given: archived A escalated from a run that only its name ties to live thread B.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), Some("rb"));
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.thread("B", None, None, None);
    f.sql("UPDATE threads SET archived = 0 WHERE thread_id = 'B'");
    f.run("rb", None, None, "chat:Worker:B");
    let database = f.database();
    let input = sidebar(&["A"], &["B"]);

    // When: the family is explored.
    let states = settle(&database, &input, &policy(), BIG);

    // Then: B joins it, so its protection (not archived) holds the deletion back.
    assert_eq!(
        states,
        [(
            "A".to_owned(),
            FamilyState::Blocked(BlockReason::FamilyMemberNotDue("B".into()))
        )]
    );
    assert_eq!(f.tombstones(), 0);
}

#[test]
fn a_run_with_a_cyclic_or_too_deep_ancestry_is_unresolved_not_ignored() {
    // Given: archived thread A escalated from a run in a parent cycle.
    let cyclic = Fixture::new();
    cyclic.thread("A", None, Some("ra"), Some("x"));
    cyclic.run("ra", None, Some("A"), "chat:Worker:A");
    cyclic.run("x", Some("y"), None, "x");
    cyclic.run("y", Some("x"), None, "y");
    let database = cyclic.database();
    let states = settle(&database, &sidebar(&["A"], &[]), &policy(), BIG);
    assert!(
        matches!(states.as_slice(), [(key, FamilyState::Blocked(BlockReason::FamilyUnresolved(_)))] if key == "A"),
        "{states:?}"
    );
    assert_eq!(
        cyclic.count("SELECT COUNT(*) FROM retention_staging_state"),
        0
    );

    // And: an ancestry longer than the link limit is treated the same way.
    let deep = Fixture::new();
    deep.thread("A", None, Some("ra"), Some("n00"));
    deep.run("ra", None, Some("A"), "chat:Worker:A");
    for index in 0..70 {
        let parent = (index < 69).then(|| format!("n{:02}", index + 1));
        deep.run(&format!("n{index:02}"), parent.as_deref(), None, "link");
    }
    let database = deep.database();
    let states = settle(&database, &sidebar(&["A"], &[]), &policy(), BIG);
    assert!(
        matches!(
            states.as_slice(),
            [(_, FamilyState::Blocked(BlockReason::FamilyUnresolved(_)))]
        ),
        "{states:?}"
    );
}

#[test]
fn a_thread_lineage_cycle_or_a_self_parent_is_unresolved_and_confirms_nothing() {
    // Given: lineages that loop back, point at themselves, run into a loop from a
    // tail, or are longer than the link limit.
    type Lineage<'a> = Vec<(&'a str, Option<&'a str>)>;
    let cases: [(&str, Lineage<'_>); 4] = [
        ("two threads", vec![("A", Some("B")), ("B", Some("A"))]),
        ("self parent", vec![("A", Some("A"))]),
        (
            "a tail into a loop",
            vec![("A", Some("B")), ("B", Some("C")), ("C", Some("B"))],
        ),
        ("a long loop", {
            let ring = ["A", "B", "C", "D", "E", "F"];
            (0..ring.len())
                .map(|i| (ring[i], Some(ring[(i + 1) % ring.len()])))
                .collect()
        }),
    ];
    for (label, threads) in cases {
        let f = Fixture::new();
        for (id, parent) in &threads {
            f.thread(id, *parent, None, None);
        }
        let ids: Vec<&str> = threads.iter().map(|(id, _)| *id).collect();
        let database = f.database();
        let input = sidebar(&ids, &[]);

        // When: the family is explored and a confirmation is attempted.
        let states = settle(&database, &input, &policy(), BIG);

        // Then: every family is blocked as unresolved, nothing is staged or tombstoned.
        assert!(!states.is_empty(), "{label}");
        for (key, state) in &states {
            assert!(
                matches!(
                    state,
                    FamilyState::Blocked(BlockReason::FamilyUnresolved(_))
                ),
                "{label}: {key} {state:?}"
            );
            assert!(
                !matches!(
                    finalize(&database, key, &input),
                    FinalizeOutcome::Confirmed(_)
                ),
                "{label}"
            );
        }
        assert_eq!(f.tombstones(), 0, "{label}");
        assert_eq!(
            f.count("SELECT COUNT(*) FROM retention_staging_state"),
            0,
            "{label}"
        );
    }

    // And: a chain below the link limit still resolves and confirms.
    let f = Fixture::new();
    f.thread("T00", None, None, None);
    for index in 1..40 {
        f.thread(
            &format!("T{index:02}"),
            Some(&format!("T{:02}", index - 1)),
            None,
            None,
        );
    }
    let ids: Vec<String> = (0..40).map(|index| format!("T{index:02}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let database = f.database();
    let input = sidebar(&ids, &[]);
    let states = settle(&database, &input, &policy(), BIG);
    assert!(
        matches!(states.as_slice(), [(_, FamilyState::Explored { threads, .. })] if threads.len() == 40),
        "{states:?}"
    );
}

#[test]
fn an_interrupted_exploration_leaves_the_previous_resumable_state_and_resumes_cleanly() {
    // Given: two archived threads of one family, and a fault when a run is first staged.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.thread("B", Some("A"), Some("rb"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.run("rb", None, Some("B"), "chat:Worker:B");
    f.sql(
        "CREATE TRIGGER inject BEFORE INSERT ON retention_staging
         WHEN NEW.kind = 'run' AND NEW.id = 'rb' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    let database = f.database();
    let input = sidebar(&["A", "B"], &[]);
    let reference = {
        let clean = Fixture::new();
        clean.thread("A", None, Some("ra"), None);
        clean.thread("B", Some("A"), Some("rb"), None);
        clean.run("ra", None, Some("A"), "chat:Worker:A");
        clean.run("rb", None, Some("B"), "chat:Worker:B");
        settle(&clean.database(), &input, &policy(), BIG)
    };

    // When: exploration is interrupted at different points by the fault.
    let mut failures = 0;
    for budget in [1_u64, 3, 8, 1_000] {
        let result =
            database.retention_explore(&input, &policy(), ExploreBudget { max_edges: budget });
        if let Err(error) = result {
            failures += 1;
            assert!(error.to_string().contains("injected"), "{error}");
            // Then: no half-written seed or member remains.
            assert_eq!(
                f.count("SELECT COUNT(*) FROM retention_staging_state s WHERE NOT EXISTS (SELECT 1 FROM retention_staging m WHERE m.family_key = s.family_key AND m.kind = 'thread')"),
                0,
                "a state without members"
            );
        }
    }
    assert!(failures > 0, "the fault was never reached");

    // And: after the fault is gone the search completes with the uninterrupted result.
    f.sql("DROP TRIGGER inject;");
    assert_eq!(settle(&database, &input, &policy(), BIG), reference);
}

fn untracked_capture(f: &Fixture, thread: &str, root: &str, big: bool) {
    f.start_state(thread, root);
    if big {
        f.sql(&format!(
            "INSERT INTO thread_start_untracked(thread_id, path, kind, executable, content)
             VALUES ('{thread}', 'big.bin', 'file', 0, zeroblob(5242880));"
        ));
    }
}

#[test]
fn single_rows_above_the_step_budget_wait_for_the_exclusive_phase_whatever_their_table() {
    // Given: a family with a 5 MiB event, a 5 MiB captured file and a 3 MiB index block,
    // each next to small rows.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.run_payload("ra", "session-1");
    f.event(
        "session-1",
        Some("ra"),
        "Message",
        "MessageCompleted",
        5 * 1024 * 1024,
    );
    untracked_capture(&f, "A", "ra", true);
    f.sql(
        "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, role, kind,
             truncated, hash, text)
         VALUES ('ra', 'message', '9:0', 9, 0, 'user', 'text', 0, 'h', printf('%03145728d', 1));",
    );
    f.sql("UPDATE thread_index_runs SET text_bytes = text_bytes + 3145728, charged_bytes = charged_bytes + 3145728 WHERE run_id = 'ra'");
    let database = f.database();
    let input = sidebar(&["A"], &[]);
    settle(&database, &input, &policy(), BIG);
    assert!(matches!(
        finalize(&database, "A", &input),
        FinalizeOutcome::Confirmed(_)
    ));
    drop(database);

    // When: the bounded steps run to completion.
    let storage = Storage::open(config(&f.path)).unwrap();
    let mut last = None;
    for _ in 0..1_000 {
        let progress = storage
            .handle()
            .reclaim_step(ReclaimBudget::default())
            .unwrap();
        assert_eq!(progress.last_error, None);
        let done = progress.jobs_db_pending == 0;
        last = Some(progress);
        if done {
            break;
        }
    }
    storage.close();

    // Then: none of the three large rows was deleted by a bounded step.
    let last = last.unwrap();
    assert_eq!((last.jobs_db_pending, last.jobs_waiting_exclusive), (0, 1));
    assert_eq!(last.oversized_pending, 3);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM events WHERE OCTET_LENGTH(payload) > 4194304"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM thread_start_untracked WHERE LENGTH(content) > 4194304"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM conversation_blocks WHERE OCTET_LENGTH(text) > 2097152"),
        1
    );
    // The rows around them were reclaimed.
    assert_eq!(
        f.count("SELECT COUNT(*) FROM events WHERE OCTET_LENGTH(payload) <= 4194304"),
        0
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM run_contexts"), 0);

    // And: the exclusive phase removes them, with consistent accounting.
    let database = f.database();
    let mut rounds = 0;
    while database.reclaim_oversized_step(0).unwrap().remaining > 0 {
        rounds += 1;
        assert!(rounds < 10);
    }
    assert_eq!(family_residue(&f), vec![]);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM reclaim_jobs WHERE state = 'db_done'"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM thread_index_runs WHERE state = 'deleted'"),
        1
    );
    assert_eq!(
        f.count("SELECT COALESCE(SUM(charged_bytes), 0) FROM thread_index_runs"),
        0
    );
    assert_eq!(
        f.count("SELECT total_event_bytes FROM sessions WHERE id = 'session-1'"),
        f.count("SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events WHERE session_id = 'session-1'")
    );
    assert_eq!(
        f.count("SELECT COALESCE(SUM(payload_bytes), 0) FROM event_day_bytes"),
        f.count("SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events")
    );
}

#[test]
fn the_exclusive_phase_works_off_many_rows_in_bounded_batches_and_counts_what_it_deleted() {
    // Given: a family with one huge index block and a huge captured file, each next
    // to hundreds of small or empty rows.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.run_payload("ra", "session-1");
    untracked_capture(&f, "A", "ra", true);
    f.sql(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 1200)
         INSERT INTO thread_start_untracked(thread_id, path, kind, executable, content)
         SELECT 'A', printf('empty-%05d', x), 'file', 0, zeroblob(0) FROM n;",
    );
    f.sql(
        "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, role,
             kind, truncated, hash, text)
         VALUES ('ra', 'message', '9:0', 9, 0, 'user', 'text', 0, 'h', printf('%03145728d', 1));
         WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 450)
         INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, role,
             kind, truncated, hash, text)
         SELECT 'ra', 'message', (100 + x) || ':0', 100 + x, 0, 'user', 'text', 0, 'h', 'small' FROM n;",
    );
    f.sql("UPDATE thread_index_runs SET text_bytes = text_bytes + 3145728, charged_bytes = charged_bytes + 3145728 WHERE run_id = 'ra'");
    let database = f.database();
    let input = sidebar(&["A"], &[]);
    settle(&database, &input, &policy(), BIG);
    assert!(matches!(
        finalize(&database, "A", &input),
        FinalizeOutcome::Confirmed(_)
    ));
    drop(database);

    // When: the bounded steps run, then the exclusive phase with a zero budget.
    let storage = Storage::open(config(&f.path)).unwrap();
    let (mut rows, mut bytes) = (0, 0);
    for _ in 0..1_000 {
        let progress = storage
            .handle()
            .reclaim_step(ReclaimBudget::default())
            .unwrap();
        assert_eq!(progress.last_error, None);
        rows += progress.rows;
        bytes += progress.stored_bytes;
        if progress.jobs_db_pending == 0 {
            assert_eq!(progress.oversized_pending, 2, "{progress:?}");
            break;
        }
    }
    storage.close();
    let database = f.database();
    let mut calls = 0;
    loop {
        let progress = database.reclaim_oversized_step(0).unwrap();
        assert_eq!(progress.last_error, None);
        // Then: a call deletes one bounded batch or one oversized row, never a run's
        // or capture's whole set.
        assert!(
            progress.rows <= 500,
            "one call deleted {} rows",
            progress.rows
        );
        rows += progress.rows;
        bytes += progress.stored_bytes;
        calls += 1;
        assert!(calls < 200);
        if progress.remaining == 0 {
            break;
        }
    }
    assert!(calls >= 4, "{calls} calls: the sets were not batched");

    // And: nothing is left, and the job counted exactly the rows and bytes deleted.
    assert_eq!(f.count("SELECT COUNT(*) FROM thread_start_untracked"), 0);
    assert_eq!(f.count("SELECT COUNT(*) FROM thread_start_states"), 0);
    assert_eq!(f.count("SELECT COUNT(*) FROM conversation_blocks"), 0);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM thread_index_runs WHERE state = 'deleted'"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM reclaim_jobs WHERE state = 'db_done'"),
        1
    );
    assert_eq!(
        f.count("SELECT reclaimed_rows FROM reclaim_jobs"),
        i64::try_from(rows).unwrap()
    );
    assert_eq!(
        f.count("SELECT reclaimed_stored_bytes FROM reclaim_jobs"),
        i64::try_from(bytes).unwrap()
    );
}

#[test]
fn an_oversized_registration_whose_row_is_already_gone_counts_nothing() {
    // Given: a job with a registered 5 MiB run context that was deleted meanwhile.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.sql("UPDATE run_contexts SET messages_json = zeroblob(5242880) WHERE run_id = 'ra'");
    let database = f.database();
    let input = sidebar(&["A"], &[]);
    settle(&database, &input, &policy(), BIG);
    assert!(matches!(
        finalize(&database, "A", &input),
        FinalizeOutcome::Confirmed(_)
    ));
    drop(database);
    let storage = Storage::open(config(&f.path)).unwrap();
    storage
        .handle()
        .reclaim_step(ReclaimBudget::default())
        .unwrap();
    storage.close();
    assert_eq!(f.count("SELECT COUNT(*) FROM reclaim_oversized"), 1);
    f.sql("DELETE FROM run_contexts WHERE run_id = 'ra'");
    let before = f.count("SELECT reclaimed_rows FROM reclaim_jobs");

    // When: the exclusive phase reaches the registration.
    let progress = f.database().reclaim_oversized_step(0).unwrap();

    // Then: the registration is dropped and no deletion is claimed.
    assert_eq!(
        (progress.rows, progress.stored_bytes, progress.remaining),
        (0, 0, 0)
    );
    assert_eq!(f.count("SELECT reclaimed_rows FROM reclaim_jobs"), before);
    assert_eq!(
        f.count("SELECT reclaimed_stored_bytes FROM reclaim_jobs"),
        0
    );
}

#[test]
fn thousands_of_tasks_and_links_are_reclaimed_task_by_task_through_a_cursor() {
    // Given: a confirmed family with 1300 chained tasks and queue ledger rows, next to
    // a live thread's task.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.thread("X", None, Some("rx"), None);
    f.sql("UPDATE threads SET archived = 0 WHERE thread_id = 'X'");
    f.run("rx", None, Some("X"), "chat:Worker:X");
    f.task("task-x", "rx");
    f.sql(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 1300)
         INSERT INTO tasks(id, status, created_at_ns, updated_at_ns, parent_run_id)
         SELECT printf('task-%05d', x), 'pending', 1, 1, 'ra' FROM n;
         WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 1299)
         INSERT INTO task_links(blocker_id, blocked_id)
         SELECT printf('task-%05d', x), printf('task-%05d', x + 1) FROM n;
         INSERT INTO task_links(blocker_id, blocked_id) VALUES ('task-00001', 'task-x');
         WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 1300)
         INSERT INTO task_queue_ledger(operation, payload)
         SELECT 'enqueue', json_object('id', printf('task-%05d', x)) FROM n;",
    );
    let database = f.database();
    let input = sidebar(&["A"], &["X"]);
    settle(&database, &input, &policy(), BIG);
    assert!(matches!(
        finalize(&database, "A", &input),
        FinalizeOutcome::Confirmed(_)
    ));
    drop(database);

    // When: reclaim runs to completion, a few transactions at a time.
    let storage = Storage::open(config(&f.path)).unwrap();
    let mut rounds = 0;
    loop {
        let progress = storage
            .handle()
            .reclaim_step(ReclaimBudget {
                max_transactions: 4,
            })
            .unwrap();
        assert_eq!(progress.last_error, None);
        rounds += 1;
        assert!(rounds < 500);
        if progress.jobs_db_pending == 0 {
            break;
        }
    }
    storage.close();

    // Then: the family's tasks, links and ledger rows are gone; the neighbour's stay.
    assert_eq!(
        f.count("SELECT COUNT(*) FROM tasks WHERE id LIKE 'task-0%'"),
        0
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM task_links"), 0);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM task_queue_ledger WHERE json_extract(payload, '$.id') LIKE 'task-0%'"),
        0
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM tasks WHERE id = 'task-x'"), 1);
    assert_eq!(
        f.count(
            "SELECT COUNT(*) FROM task_queue_ledger WHERE json_extract(payload, '$.id') = 'task-x'"
        ),
        1
    );
}

#[test]
fn a_job_waiting_for_the_exclusive_phase_never_starves_the_jobs_behind_it() {
    // Given: family A (sorts first) with an oversized run context, and a normal family Z.
    let f = Fixture::new();
    f.thread("A", None, Some("ra"), None);
    f.run("ra", None, Some("A"), "chat:Worker:A");
    f.sql("UPDATE run_contexts SET messages_json = zeroblob(5242880) WHERE run_id = 'ra'");
    f.thread("Z", None, Some("rz"), None);
    f.run("rz", None, Some("Z"), "chat:Worker:Z");
    f.run_payload("rz", "session-z");
    let database = f.database();
    let input = sidebar(&["A", "Z"], &[]);
    settle(&database, &input, &policy(), BIG);
    assert!(matches!(
        finalize(&database, "A", &input),
        FinalizeOutcome::Confirmed(_)
    ));
    assert!(matches!(
        finalize(&database, "Z", &input),
        FinalizeOutcome::Confirmed(_)
    ));
    drop(database);
    let storage = Storage::open(config(&f.path)).unwrap();
    let handle = storage.handle();

    // When: the writer takes one transaction per step.
    let mut finished = false;
    for _ in 0..2_000 {
        let progress = handle
            .reclaim_step(ReclaimBudget {
                max_transactions: 1,
            })
            .unwrap();
        if progress.jobs_db_pending == 0 {
            finished = true;
            // Then: A only waits; no transaction is spent on it any more.
            assert_eq!(progress.jobs_waiting_exclusive, 1);
            let idle = handle
                .reclaim_step(ReclaimBudget {
                    max_transactions: 8,
                })
                .unwrap();
            assert_eq!(idle.transactions, 0);
            break;
        }
    }
    storage.close();
    assert!(finished, "Z must be reclaimed behind a waiting A");
    assert_eq!(
        f.count("SELECT COUNT(*) FROM reclaim_jobs WHERE family_key = 'Z' AND state = 'db_done'"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM events WHERE run_id = 'rz'"),
        0
    );
    assert_eq!(
        f.count(
            "SELECT COUNT(*) FROM reclaim_jobs WHERE family_key = 'A' AND state = 'db_pending'"
        ),
        1
    );
}
