//! Archived-thread retention: closure, confirmation, fences, readers and reclaim.

use std::collections::BTreeMap;
use std::path::PathBuf;

use event_bus::{Event, OrchestratorEvent, ThreadTodoItem, ThreadTodoSnapshot, ThreadTodoStatus};
use rusqlite::{Connection, params};
use storage::thread_retention::{
    BindingInput, BlockReason, ExploreBudget, FamilyState, FilesOutcome, FinalizeBudget,
    FinalizeOutcome, LegacyArchivedPolicy, ReclaimBudget, RetentionInput, RetentionStatus,
    SidebarThread, ThreadRetentionPolicy,
};
use storage::{Database, Storage, StorageConfig, StorageError};
use tempfile::TempDir;

const NOW_NS: i64 = 100 * 86_400 * 1_000_000_000;
const BIG: ExploreBudget = ExploreBudget {
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
             VALUES (?1, ?2, ?2, 'root', '{}')",
            params![format!("q-{id}"), id],
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
            FinalizeBudget::proportional(1_000),
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
            .retention_finalize(
                &key,
                &input,
                &policy,
                &owners,
                &binding(),
                FinalizeBudget::proportional(10),
            )
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
        .retention_finalize(
            "A",
            &input,
            &policy(),
            &owners,
            &binding(),
            FinalizeBudget::proportional(10),
        )
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
                FinalizeBudget::proportional(10)
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
                FinalizeBudget::proportional(302)
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
    assert_eq!(progress.jobs_db_pending, 1);
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
