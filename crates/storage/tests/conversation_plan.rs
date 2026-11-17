//! The conversation plan through the single writer: one transaction per revision.
//! SQL connections below are fixture and fault injection only, never parallel writers; the
//! race test starts its two callers at a barrier and lets the writer decide the order.

use std::sync::{Arc, Barrier};
use std::thread;

use event_bus::{
    ConversationPlanEvent, EventKind, PlanChange, PlanChangeCause, PlanTaskStatus, PlanWeight,
};
use rusqlite::Connection;
use storage::conversation_plan::{
    PLAN_MAX_NODES, PLAN_MAX_OPERATIONS, PlanApplied, PlanOperation, PlanRejection, PlanUpdate,
    TaskRef,
};
use storage::handoffs::{HandoffReceipt, HandoffState};
use storage::plan_write_gate::{PlanWriteGate, PlanWritePoint, PlanWriteResolution};
use storage::{HardLimits, Storage, StorageConfig, StorageError, StorageHandle};

const THREAD: &str = "thread-a";

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}

impl Fixture {
    fn new() -> Self {
        Self::with_limits(HardLimits::default())
    }

    fn with_limits(hard_limits: HardLimits) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("plan.db"),
            hard_limits,
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
        }
    }

    fn handle(&self) -> StorageHandle {
        self.storage.handle()
    }

    fn sql(&self) -> Connection {
        let conn = Connection::open(&self.config.db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }

    fn count(&self, sql: &str) -> i64 {
        self.sql().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn plan_events(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM events WHERE kind = 'ConversationPlan'")
    }

    /// Everything a plan owns: projection rows, history rows and change events.
    fn plan_rows(&self) -> (i64, i64, i64, i64) {
        (
            self.count("SELECT COUNT(*) FROM conversation_plans"),
            self.count("SELECT COUNT(*) FROM conversation_plan_nodes"),
            self.count("SELECT COUNT(*) FROM conversation_plan_history"),
            self.plan_events(),
        )
    }
}

fn add(client_ref: &str, parent: Option<TaskRef>, title: &str) -> PlanOperation {
    PlanOperation::Add {
        client_ref: client_ref.to_owned(),
        parent,
        position: None,
        title: title.to_owned(),
        description: None,
        weight: None,
        status: None,
    }
}

fn update(thread: &str, expected_revision: u64, operations: Vec<PlanOperation>) -> PlanUpdate {
    PlanUpdate {
        thread_id: thread.to_owned(),
        expected_revision,
        source_run_id: Some("run-1".to_owned()),
        operations,
    }
}

fn set_status(task: &str, status: PlanTaskStatus) -> PlanOperation {
    PlanOperation::Update {
        task: TaskRef::Id(task.to_owned()),
        title: None,
        description: None,
        result: None,
        blocker: None,
        weight: None,
        status: Some(status),
    }
}

fn rejection(error: &StorageError) -> &PlanRejection {
    match error {
        StorageError::ConversationPlan(rejection) => rejection,
        other => panic!("expected a plan rejection, got {other:?}"),
    }
}

fn changed(applied: &PlanApplied) -> &ConversationPlanEvent {
    match &applied.event.as_ref().expect("a stored event").event.kind {
        EventKind::ConversationPlan(event) => event,
        other => panic!("{other:?}"),
    }
}

/// A plan with parent `a` (children `b`, `c`) and a leaf `d`; returns the applied result.
fn seed(handle: &StorageHandle) -> PlanApplied {
    handle
        .update_conversation_plan(update(
            THREAD,
            0,
            vec![
                add("a", None, "parent"),
                add("b", Some(TaskRef::Ref("a".into())), "first"),
                add("c", Some(TaskRef::Ref("a".into())), "second"),
                add("d", None, "leaf"),
            ],
        ))
        .unwrap()
}

fn id_of(applied: &PlanApplied, name: &str) -> String {
    applied
        .created
        .iter()
        .find(|(client_ref, _)| client_ref == name)
        .unwrap_or_else(|| panic!("no task for {name}"))
        .1
        .clone()
}

#[test]
fn a_thread_without_changes_has_revision_zero_and_no_plan() {
    let f = Fixture::new();
    let summary = f.handle().conversation_plan_summary(THREAD).unwrap();
    assert_eq!((summary.revision, summary.plan_id), (0, None));
    assert_eq!(summary.progress.completion_ppm(), None);
    let page = f
        .handle()
        .conversation_plan_tree(THREAD, None, None)
        .unwrap();
    assert!(page.nodes.is_empty() && page.next_cursor.is_none());
    assert_eq!(f.plan_rows(), (0, 0, 0, 0));
}

#[test]
fn the_first_change_is_revision_one_with_one_event_and_one_history_row() {
    let f = Fixture::new();
    let h = f.handle();
    let applied = seed(&h);
    assert_eq!((applied.revision, applied.changed), (1, true));
    assert_eq!(applied.created.len(), 4);
    assert_eq!(applied.progress.node_count, 4);
    assert_eq!(f.plan_rows(), (1, 4, 1, 1));
    match changed(&applied) {
        ConversationPlanEvent::Changed {
            thread_id,
            plan_id,
            revision,
            cause,
            source_run_id,
            changes,
            ..
        } => {
            assert_eq!(thread_id, THREAD);
            assert_eq!(Some(plan_id), applied.plan_id.as_ref());
            assert_eq!(*revision, 1);
            assert_eq!(*cause, PlanChangeCause::ModelUpdate);
            assert_eq!(source_run_id.as_deref(), Some("run-1"));
            assert_eq!(changes.len(), 4);
            assert!(matches!(changes[0], PlanChange::Added { .. }));
        }
    }
    let summary = h.conversation_plan_summary(THREAD).unwrap();
    assert_eq!(summary.revision, 1);
    assert_eq!(summary.plan_id, applied.plan_id);
}

#[test]
fn every_change_is_one_revision_and_one_event_and_history_pages_follow_the_index() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    let leaf = id_of(&first, "d");
    for (n, status) in [PlanTaskStatus::InProgress, PlanTaskStatus::Completed]
        .into_iter()
        .enumerate()
    {
        let applied = h
            .update_conversation_plan(update(
                THREAD,
                1 + n as u64,
                vec![set_status(&leaf, status)],
            ))
            .unwrap();
        assert_eq!(applied.revision, 2 + n as u64);
    }
    assert_eq!(f.plan_rows(), (1, 4, 3, 3));
    // A page of two, then the rest, oldest first.
    let page = h.conversation_plan_history(THREAD, None, Some(2)).unwrap();
    assert_eq!(
        page.entries.iter().map(|e| e.revision).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(page.next_after_revision, Some(2));
    let rest = h
        .conversation_plan_history(THREAD, page.next_after_revision, Some(2))
        .unwrap();
    assert_eq!(rest.entries.len(), 1);
    assert_eq!(rest.entries[0].revision, 3);
    assert_eq!(rest.next_after_revision, None);
    let event = rest.entries[0].event.as_ref().unwrap();
    assert_eq!(event.revision(), 3);
    assert_eq!(event.thread_id(), THREAD);
}

#[test]
fn a_change_that_changes_nothing_adds_no_revision_and_no_event_but_still_checks_the_revision() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    let leaf = id_of(&first, "d");
    // Setting the value the task already has.
    let same = h
        .update_conversation_plan(update(
            THREAD,
            1,
            vec![set_status(&leaf, PlanTaskStatus::Pending)],
        ))
        .unwrap();
    assert!(!same.changed && same.event.is_none());
    assert_eq!(same.revision, 1);
    assert_eq!(f.plan_rows(), (1, 4, 1, 1));
    // The same request against a stale revision is still a conflict.
    let stale = h
        .update_conversation_plan(update(
            THREAD,
            0,
            vec![set_status(&leaf, PlanTaskStatus::Pending)],
        ))
        .unwrap_err();
    assert!(matches!(
        stale,
        StorageError::StaleRevision {
            expected: 0,
            current: 1,
            ..
        }
    ));
    // Clearing an unplanned thread does not create a plan.
    let other = h
        .update_conversation_plan(update("thread-b", 0, vec![PlanOperation::Clear]))
        .unwrap();
    assert!(!other.changed && other.plan_id.is_none() && other.revision == 0);
    assert_eq!(f.plan_rows(), (1, 4, 1, 1));
}

#[test]
fn two_requests_on_one_revision_let_exactly_one_win() {
    let f = Fixture::new();
    let h = f.handle();
    seed(&h);
    for round in 0..10_u64 {
        let revision = 1 + round;
        let gate = Arc::new(Barrier::new(2));
        let spawn = |title: &'static str| {
            let (h, gate) = (h.clone(), gate.clone());
            thread::spawn(move || {
                gate.wait();
                h.update_conversation_plan(update(
                    THREAD,
                    revision,
                    vec![add(&format!("{title}{round}"), None, title)],
                ))
            })
        };
        let (a, b) = (spawn("left"), spawn("right"));
        let results = [a.join().unwrap(), b.join().unwrap()];
        let winners = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "round {round}: {results:?}");
        let loser = results.iter().find_map(|r| r.as_ref().err()).unwrap();
        assert!(matches!(loser, StorageError::StaleRevision { .. }));
        assert_eq!(
            h.conversation_plan_summary(THREAD).unwrap().revision,
            revision + 1
        );
    }
    // One event and one history row per revision: nothing from a loser.
    assert_eq!(
        f.count("SELECT COUNT(*) FROM conversation_plan_history"),
        11
    );
    assert_eq!(f.plan_events(), 11);
}

#[test]
fn a_refused_batch_leaves_the_projection_the_history_and_the_events_untouched() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    let before = f.plan_rows();
    let ghost = TaskRef::Id("plan-elsewhere:1".into());
    for (operations, expected) in [
        (
            vec![add("x", None, "fine"), add("x", None, "again")],
            PlanRejection::DuplicateClientRef("x".into()),
        ),
        (
            vec![
                add("x", None, "fine"),
                PlanOperation::Remove { task: ghost },
            ],
            PlanRejection::ForeignTask("plan-elsewhere:1".into()),
        ),
        (
            vec![
                add("x", None, "fine"),
                PlanOperation::Move {
                    task: TaskRef::Id(id_of(&first, "a")),
                    parent: Some(TaskRef::Id(id_of(&first, "b"))),
                    position: None,
                },
            ],
            PlanRejection::Cycle(id_of(&first, "a")),
        ),
        (
            vec![
                add("x", None, "fine"),
                set_status(&id_of(&first, "d"), PlanTaskStatus::Interrupted),
            ],
            PlanRejection::InterruptedIsRuntimeOnly,
        ),
    ] {
        let error = h
            .update_conversation_plan(update(THREAD, 1, operations))
            .unwrap_err();
        assert_eq!(rejection(&error), &expected);
        assert_eq!(f.plan_rows(), before);
    }
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 1);
}

#[test]
fn the_node_limit_is_enforced_across_batches_and_a_refused_batch_adds_nothing() {
    let f = Fixture::new();
    let h = f.handle();
    let mut revision = 0;
    for batch in 0..PLAN_MAX_NODES / 32 {
        let operations = (0..32)
            .map(|n| add(&format!("n{n}"), None, &format!("task {batch}-{n}")))
            .collect();
        revision = h
            .update_conversation_plan(update(THREAD, revision, operations))
            .unwrap()
            .revision;
    }
    let error = h
        .update_conversation_plan(update(THREAD, revision, vec![add("x", None, "over")]))
        .unwrap_err();
    assert!(matches!(
        rejection(&error),
        PlanRejection::TooManyNodes { actual, max }
            if *actual == PLAN_MAX_NODES + 1 && *max == PLAN_MAX_NODES
    ));
    assert_eq!(f.count("SELECT COUNT(*) FROM conversation_plan_nodes"), 128);
    assert_eq!(
        h.conversation_plan_summary(THREAD).unwrap().revision,
        revision
    );
}

fn failing_trigger(f: &Fixture, table: &str, condition: &str) {
    f.sql()
        .execute_batch(&format!(
            "CREATE TRIGGER fail_plan BEFORE INSERT ON {table} WHEN {condition} \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END"
        ))
        .unwrap();
}

#[test]
fn a_failing_event_a_failing_history_row_or_a_failing_projection_rolls_everything_back() {
    for (table, condition) in [
        ("events", "NEW.kind = 'ConversationPlan'"),
        ("conversation_plan_history", "1"),
        ("conversation_plan_nodes", "1"),
        ("conversation_plans", "1"),
    ] {
        let f = Fixture::new();
        let h = f.handle();
        failing_trigger(&f, table, condition);
        let error = h
            .update_conversation_plan(update(THREAD, 0, vec![add("a", None, "task")]))
            .unwrap_err();
        assert!(
            error.to_string().contains("injected failure"),
            "{table}: {error}"
        );
        assert_eq!(f.plan_rows(), (0, 0, 0, 0), "{table}");
        assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 0);
        // The writer is healthy afterwards: dropping the fault lets the same request pass.
        f.sql().execute_batch("DROP TRIGGER fail_plan").unwrap();
        let applied = h
            .update_conversation_plan(update(THREAD, 0, vec![add("a", None, "task")]))
            .unwrap();
        assert_eq!(applied.revision, 1);
        assert_eq!(f.plan_rows(), (1, 1, 1, 1), "{table}");
    }
}

#[test]
fn a_failure_after_the_first_revision_keeps_the_previous_revision_whole() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    failing_trigger(&f, "conversation_plan_history", "1");
    let leaf = id_of(&first, "d");
    h.update_conversation_plan(update(
        THREAD,
        1,
        vec![set_status(&leaf, PlanTaskStatus::Completed)],
    ))
    .unwrap_err();
    assert_eq!(f.plan_rows(), (1, 4, 1, 1));
    let task = h.conversation_plan_task(THREAD, &leaf).unwrap();
    assert_eq!(task.revision, 1);
    assert_eq!(task.node.status, Some(PlanTaskStatus::Pending));
}

#[test]
fn an_event_over_the_size_limit_is_refused_whole_and_never_truncated() {
    // Given: a limit that holds a small change but not a batch of long descriptions.
    let f = Fixture::with_limits(HardLimits {
        max_event_bytes: 4_000,
        ..HardLimits::default()
    });
    let h = f.handle();
    h.update_conversation_plan(update(THREAD, 0, vec![add("a", None, "small")]))
        .unwrap();
    let long = |n: usize| PlanOperation::Add {
        client_ref: format!("long{n}"),
        parent: None,
        position: None,
        title: "long".into(),
        description: Some("d".repeat(1_500)),
        weight: None,
        status: None,
    };
    // When: three descriptions make the serialized event exceed the limit.
    let error = h
        .update_conversation_plan(update(THREAD, 1, vec![long(0), long(1), long(2)]))
        .unwrap_err();
    // Then: the whole request is refused and nothing of it is stored.
    assert!(
        matches!(error, StorageError::LimitExceeded { .. }),
        "{error:?}"
    );
    assert_eq!(f.plan_rows(), (1, 1, 1, 1));
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 1);
    // A request whose event fits is accepted at the same revision.
    let applied = h
        .update_conversation_plan(update(THREAD, 1, vec![long(0)]))
        .unwrap();
    assert_eq!(applied.revision, 2);
}

#[test]
fn json_escapes_count_toward_the_task_and_event_size() {
    let f = Fixture::new();
    let h = f.handle();
    // A description of control characters is within its text limit but its JSON form is
    // six times larger than the task may take.
    let escaped = PlanOperation::Add {
        client_ref: "a".into(),
        parent: None,
        position: None,
        title: "escapes".into(),
        description: Some("\u{1}".repeat(4096)),
        weight: None,
        status: None,
    };
    let error = h
        .update_conversation_plan(update(THREAD, 0, vec![escaped]))
        .unwrap_err();
    assert!(matches!(
        rejection(&error),
        PlanRejection::NodeDetailTooLarge { .. }
    ));
    assert_eq!(f.plan_rows(), (0, 0, 0, 0));
}

#[test]
fn a_secret_in_any_text_is_refused_before_anything_is_stored() {
    let f = Fixture::new();
    let h = f.handle();
    let secret = format!("key {}{}", "AKIA", "IOSFODNN7EXAMPLE");
    for field in ["title", "description"] {
        let operation = PlanOperation::Add {
            client_ref: "a".into(),
            parent: None,
            position: None,
            title: if field == "title" {
                secret.clone()
            } else {
                "ok".into()
            },
            description: (field == "description").then(|| secret.clone()),
            weight: None,
            status: None,
        };
        let error = h
            .update_conversation_plan(update(THREAD, 0, vec![operation]))
            .unwrap_err();
        assert!(
            matches!(error, StorageError::SecretDetected { .. }),
            "{field}: {error:?}"
        );
        assert_eq!(f.plan_rows(), (0, 0, 0, 0));
    }
    // A later update of an existing task is guarded too, and the stored plan stays clean.
    let first = seed(&h);
    let error = h
        .update_conversation_plan(update(
            THREAD,
            1,
            vec![PlanOperation::Update {
                task: TaskRef::Id(id_of(&first, "d")),
                title: None,
                description: None,
                result: Some(secret),
                blocker: None,
                weight: None,
                status: None,
            }],
        ))
        .unwrap_err();
    assert!(matches!(error, StorageError::SecretDetected { .. }));
    assert_eq!(f.plan_rows(), (1, 4, 1, 1));
    assert_eq!(
        f.count("SELECT COUNT(*) FROM events WHERE payload LIKE '%AKIA%'"),
        0
    );
}

#[test]
fn the_tree_is_paged_in_display_order_and_a_cursor_belongs_to_one_revision() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    let page = h.conversation_plan_tree(THREAD, None, Some(2)).unwrap();
    assert_eq!(
        page.nodes
            .iter()
            .map(|n| n.title.as_str())
            .collect::<Vec<_>>(),
        ["parent", "first"]
    );
    assert_eq!(page.nodes[0].status, None);
    assert_eq!(page.nodes[0].child_count, 2);
    assert_eq!(page.nodes[1].depth, 2);
    assert_eq!(page.revision, 1);
    let cursor = page.next_cursor.clone().unwrap();
    let rest = h
        .conversation_plan_tree(THREAD, Some(&cursor), Some(10))
        .unwrap();
    assert_eq!(
        rest.nodes
            .iter()
            .map(|n| n.title.as_str())
            .collect::<Vec<_>>(),
        ["second", "leaf"]
    );
    assert!(rest.next_cursor.is_none());
    // After the plan changed, the old cursor is refused instead of mixing revisions.
    h.update_conversation_plan(update(
        THREAD,
        1,
        vec![set_status(&id_of(&first, "d"), PlanTaskStatus::InProgress)],
    ))
    .unwrap();
    let stale = h
        .conversation_plan_tree(THREAD, Some(&cursor), Some(10))
        .unwrap_err();
    assert!(matches!(
        rejection(&stale),
        PlanRejection::StaleCursor {
            cursor_revision: 1,
            current: 2
        }
    ));
    let garbage = h
        .conversation_plan_tree(THREAD, Some("not-a-cursor"), None)
        .unwrap_err();
    assert_eq!(rejection(&garbage), &PlanRejection::InvalidCursor);
}

#[test]
fn a_task_detail_carries_its_text_weight_and_aggregate() {
    let f = Fixture::new();
    let h = f.handle();
    let first = h
        .update_conversation_plan(update(
            THREAD,
            0,
            vec![
                PlanOperation::Add {
                    client_ref: "a".into(),
                    parent: None,
                    position: None,
                    title: "parent".into(),
                    description: Some("why \"quoted\" \n and more".into()),
                    weight: Some(PlanWeight::new(5).unwrap()),
                    status: None,
                },
                PlanOperation::Add {
                    client_ref: "b".into(),
                    parent: Some(TaskRef::Ref("a".into())),
                    position: None,
                    title: "done".into(),
                    description: None,
                    weight: None,
                    status: Some(PlanTaskStatus::Completed),
                },
            ],
        ))
        .unwrap();
    let parent = h
        .conversation_plan_task(THREAD, &id_of(&first, "a"))
        .unwrap();
    assert_eq!(parent.node.description, "why \"quoted\" \n and more");
    assert_eq!(parent.node.weight.get(), 5);
    assert_eq!(parent.node.child_ids, [id_of(&first, "b")]);
    assert_eq!(parent.node.progress_ppm.completed, 1_000_000);
    let unknown = h
        .conversation_plan_task(THREAD, &format!("{}:99", parent.plan_id))
        .unwrap_err();
    assert!(matches!(rejection(&unknown), PlanRejection::UnknownTask(_)));
    let foreign = h
        .conversation_plan_task(THREAD, "plan-other:1")
        .unwrap_err();
    assert!(matches!(rejection(&foreign), PlanRejection::ForeignTask(_)));
}

#[test]
fn clear_keeps_the_plan_identity_and_revision_and_new_tasks_get_new_ids() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    let cleared = h
        .update_conversation_plan(update(THREAD, 1, vec![PlanOperation::Clear]))
        .unwrap();
    assert_eq!(
        (cleared.revision, cleared.plan_id.clone()),
        (2, first.plan_id.clone())
    );
    assert_eq!(cleared.progress.completion_ppm(), None);
    assert_eq!(f.count("SELECT COUNT(*) FROM conversation_plan_nodes"), 0);
    let summary = h.conversation_plan_summary(THREAD).unwrap();
    assert_eq!((summary.revision, summary.plan_id), (2, first.plan_id));
    let again = h
        .update_conversation_plan(update(THREAD, 2, vec![add("n", None, "new")]))
        .unwrap();
    assert_eq!(again.revision, 3);
    assert!(
        !first
            .created
            .iter()
            .any(|(_, id)| *id == again.created[0].1)
    );
}

#[test]
fn the_runtime_reconciliation_interrupts_in_progress_leaves_in_one_revision() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    // Nothing is in progress yet.
    let idle = h
        .reconcile_conversation_plan(THREAD, Some("run-2"))
        .unwrap();
    assert!(!idle.changed);
    let unplanned = h.reconcile_conversation_plan("thread-b", None).unwrap();
    assert!(!unplanned.changed && unplanned.plan_id.is_none());
    h.update_conversation_plan(update(
        THREAD,
        1,
        vec![
            set_status(&id_of(&first, "b"), PlanTaskStatus::InProgress),
            set_status(&id_of(&first, "d"), PlanTaskStatus::InProgress),
        ],
    ))
    .unwrap();
    let applied = h
        .reconcile_conversation_plan(THREAD, Some("run-2"))
        .unwrap();
    assert_eq!((applied.changed, applied.revision), (true, 3));
    match changed(&applied) {
        ConversationPlanEvent::Changed {
            cause,
            source_run_id,
            changes,
            ..
        } => {
            assert_eq!(*cause, PlanChangeCause::RuntimeInterrupt);
            assert_eq!(source_run_id.as_deref(), Some("run-2"));
            assert_eq!(changes.len(), 2);
        }
    }
    assert_eq!(applied.progress.leaf_counts.interrupted, 2);
    assert_eq!(applied.progress.leaf_counts.in_progress, 0);
    // A second reconciliation finds nothing and adds nothing.
    assert!(!h.reconcile_conversation_plan(THREAD, None).unwrap().changed);
    assert_eq!(f.count("SELECT COUNT(*) FROM conversation_plan_history"), 3);
    // The model can take an interrupted leaf up again.
    let resumed = h
        .update_conversation_plan(update(
            THREAD,
            3,
            vec![set_status(&id_of(&first, "d"), PlanTaskStatus::InProgress)],
        ))
        .unwrap();
    assert_eq!(resumed.revision, 4);
}

#[test]
fn the_plan_survives_a_restart() {
    let f = Fixture::new();
    let first = seed(&f.handle());
    let Fixture {
        _dir,
        config,
        storage,
    } = f;
    storage.close();
    let reopened = Storage::open(config).unwrap();
    let summary = reopened.handle().conversation_plan_summary(THREAD).unwrap();
    assert_eq!((summary.revision, summary.plan_id), (1, first.plan_id));
    let page = reopened
        .handle()
        .conversation_plan_tree(THREAD, None, None)
        .unwrap();
    assert_eq!(page.nodes.len(), 4);
    drop(_dir);
}

#[test]
fn a_rebind_moves_the_plan_with_its_identity_revision_and_history() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    assert!(h.rebind_conversation_plan(THREAD, "thread-moved").unwrap());
    let old = h.conversation_plan_summary(THREAD).unwrap();
    assert_eq!((old.revision, old.plan_id), (0, None));
    let moved = h.conversation_plan_summary("thread-moved").unwrap();
    assert_eq!((moved.revision, moved.plan_id), (1, first.plan_id.clone()));
    // The task IDs and the change history are unchanged; no event was added.
    let task = h
        .conversation_plan_task("thread-moved", &id_of(&first, "b"))
        .unwrap();
    assert_eq!(task.node.title, "first");
    let history = h
        .conversation_plan_history("thread-moved", None, None)
        .unwrap();
    assert_eq!(history.entries.len(), 1);
    assert_eq!(f.plan_events(), 1);
    // The next change continues the revision count.
    let next = h
        .update_conversation_plan(update(
            "thread-moved",
            1,
            vec![set_status(&id_of(&first, "d"), PlanTaskStatus::Completed)],
        ))
        .unwrap();
    assert_eq!(next.revision, 2);
    // Nothing to move, or a target that has a plan, moves nothing.
    assert!(!h.rebind_conversation_plan("nobody", "elsewhere").unwrap());
    h.update_conversation_plan(update("thread-c", 0, vec![add("a", None, "other")]))
        .unwrap();
    let error = h
        .rebind_conversation_plan("thread-moved", "thread-c")
        .unwrap_err();
    assert_eq!(rejection(&error), &PlanRejection::BindingConflict);
    assert_eq!(
        h.conversation_plan_summary("thread-moved")
            .unwrap()
            .revision,
        2
    );
}

#[test]
fn a_forked_thread_starts_unplanned_while_the_source_keeps_its_plan() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    let fork = h.conversation_plan_summary("thread-fork").unwrap();
    assert_eq!((fork.revision, fork.plan_id), (0, None));
    let forked = h
        .update_conversation_plan(update("thread-fork", 0, vec![add("a", None, "own")]))
        .unwrap();
    assert_eq!(forked.revision, 1);
    assert_ne!(forked.plan_id, first.plan_id);
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 1);
}

#[test]
fn a_committed_handoff_moves_the_plan_to_the_escalation_thread_without_an_event() {
    let f = Fixture::new();
    let h = f.handle();
    h.update_conversation_plan(update(THREAD, 0, vec![add("a", None, "carried")]))
        .unwrap();
    f.sql()
        .execute(
            "INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth) \
             VALUES ('source', ?1, 'source', 0)",
            [THREAD],
        )
        .unwrap();
    h.reserve_handoff(HandoffReceipt {
        source_run_id: "source".into(),
        source_incarnation: "incarnation".into(),
        target_run_id: "target".into(),
        lane_id: "lane-target".into(),
        request_json: "request".into(),
        state: HandoffState::Preparing,
        goal_json: None,
        todo_json: None,
    })
    .unwrap();
    h.advance_handoff(
        "source".into(),
        "incarnation".into(),
        HandoffState::Preparing,
        HandoffState::Prepared,
        None,
        None,
    )
    .unwrap();
    // Before the commit the plan stays where it was.
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 1);
    let events_before = f.plan_events();
    h.advance_handoff(
        "source".into(),
        "incarnation".into(),
        HandoffState::Prepared,
        HandoffState::Committed,
        None,
        None,
    )
    .unwrap();
    let target_thread = event_bus::escalation_thread_id("target");
    let moved = h.conversation_plan_summary(&target_thread).unwrap();
    assert_eq!(moved.revision, 1);
    assert!(moved.plan_id.is_some());
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 0);
    assert_eq!(f.plan_events(), events_before);
}

#[test]
fn a_deleted_thread_refuses_plan_writes_and_reads() {
    let f = Fixture::new();
    let h = f.handle();
    seed(&h);
    f.sql()
        .execute(
            "INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) \
             VALUES (?1, 'family', 1)",
            [THREAD],
        )
        .unwrap();
    let gone = |error: StorageError| matches!(error, StorageError::Tombstoned { .. });
    assert!(gone(
        h.update_conversation_plan(update(THREAD, 1, vec![add("x", None, "late")]))
            .unwrap_err()
    ));
    assert!(gone(
        h.reconcile_conversation_plan(THREAD, None).unwrap_err()
    ));
    assert!(gone(h.conversation_plan_summary(THREAD).unwrap_err()));
    assert!(gone(
        h.conversation_plan_tree(THREAD, None, None).unwrap_err()
    ));
    assert!(gone(
        h.conversation_plan_history(THREAD, None, None).unwrap_err()
    ));
    assert_eq!(f.plan_rows(), (1, 4, 1, 1));
}

#[test]
fn a_task_cannot_be_read_through_another_threads_plan() {
    let f = Fixture::new();
    let h = f.handle();
    let first = seed(&h);
    h.update_conversation_plan(update("thread-b", 0, vec![add("a", None, "other")]))
        .unwrap();
    let error = h
        .conversation_plan_task("thread-b", &id_of(&first, "a"))
        .unwrap_err();
    assert!(matches!(rejection(&error), PlanRejection::ForeignTask(_)));
}

/// `count` leaves in progress, added in model-sized batches; returns the revision reached.
fn fill_in_progress(handle: &StorageHandle, count: usize) -> u64 {
    let mut revision = 0;
    for (batch, chunk) in (0..count)
        .collect::<Vec<_>>()
        .chunks(PLAN_MAX_OPERATIONS)
        .enumerate()
    {
        let operations = chunk
            .iter()
            .map(|n| PlanOperation::Add {
                client_ref: format!("t{batch}-{n}"),
                parent: None,
                position: None,
                title: format!("task {n}"),
                description: None,
                weight: None,
                status: Some(PlanTaskStatus::InProgress),
            })
            .collect();
        revision = handle
            .update_conversation_plan(update(THREAD, revision, operations))
            .unwrap()
            .revision;
    }
    revision
}

#[test]
fn a_reconciliation_interrupts_32_33_and_128_leaves_in_one_revision_and_one_event() {
    for leaves in [32, 33, PLAN_MAX_NODES] {
        // Given: that many leaves in progress.
        let f = Fixture::new();
        let h = f.handle();
        let before = fill_in_progress(&h, leaves);
        let events = f.plan_events();
        // When: the runtime reconciles.
        let applied = h
            .reconcile_conversation_plan(THREAD, Some("run-2"))
            .unwrap();
        // Then: all of them are interrupted at once, in one revision with one event.
        assert!(applied.changed, "{leaves}");
        assert_eq!(applied.revision, before + 1, "{leaves}");
        assert_eq!(applied.progress.leaf_counts.interrupted, leaves as u32);
        assert_eq!(applied.progress.leaf_counts.in_progress, 0);
        assert_eq!(f.plan_events(), events + 1, "{leaves}");
        match changed(&applied) {
            ConversationPlanEvent::Changed { changes, cause, .. } => {
                assert_eq!(changes.len(), leaves);
                assert_eq!(*cause, PlanChangeCause::RuntimeInterrupt);
            }
        }
        assert!(!h.reconcile_conversation_plan(THREAD, None).unwrap().changed);
    }
}

#[test]
fn a_model_batch_is_still_limited_to_32_operations_while_reconciliation_is_not() {
    let f = Fixture::new();
    let h = f.handle();
    fill_in_progress(&h, 33);
    let tree = h.conversation_plan_tree(THREAD, None, Some(64)).unwrap();
    let to_pending: Vec<_> = tree
        .nodes
        .iter()
        .map(|node| set_status(&node.task_id, PlanTaskStatus::Pending))
        .collect();
    assert_eq!(to_pending.len(), 33);
    let error = h
        .update_conversation_plan(update(THREAD, 2, to_pending))
        .unwrap_err();
    assert!(matches!(
        rejection(&error),
        PlanRejection::TooManyOperations {
            actual: 33,
            max: 32
        }
    ));
}

fn tombstone(f: &Fixture, thread: &str) {
    f.sql()
        .execute(
            "INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) \
             VALUES (?1, 'family', 1)",
            [thread],
        )
        .unwrap();
}

#[test]
fn a_rebind_from_or_to_a_deleted_thread_is_refused_and_moves_nothing() {
    let gone = |error: &StorageError| matches!(error, StorageError::Tombstoned { .. });
    // To a deleted thread.
    let f = Fixture::new();
    let h = f.handle();
    seed(&h);
    tombstone(&f, "thread-deleted");
    let error = h
        .rebind_conversation_plan(THREAD, "thread-deleted")
        .unwrap_err();
    assert!(gone(&error), "{error:?}");
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 1);
    // From a deleted thread.
    let f = Fixture::new();
    let h = f.handle();
    seed(&h);
    tombstone(&f, THREAD);
    let error = h
        .rebind_conversation_plan(THREAD, "thread-live")
        .unwrap_err();
    assert!(gone(&error), "{error:?}");
    assert_eq!(f.plan_rows(), (1, 4, 1, 1));
    assert_eq!(
        f.count("SELECT COUNT(*) FROM conversation_plans WHERE thread_id = 'thread-live'"),
        0
    );
}

#[test]
fn a_handoff_to_a_deleted_escalation_thread_fails_whole_and_the_plan_stays() {
    let f = Fixture::new();
    let h = f.handle();
    seed(&h);
    f.sql()
        .execute(
            "INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth) \
             VALUES ('source', ?1, 'source', 0)",
            [THREAD],
        )
        .unwrap();
    h.reserve_handoff(HandoffReceipt {
        source_run_id: "source".into(),
        source_incarnation: "incarnation".into(),
        target_run_id: "target".into(),
        lane_id: "lane-target".into(),
        request_json: "request".into(),
        state: HandoffState::Preparing,
        goal_json: None,
        todo_json: None,
    })
    .unwrap();
    h.advance_handoff(
        "source".into(),
        "incarnation".into(),
        HandoffState::Preparing,
        HandoffState::Prepared,
        None,
        None,
    )
    .unwrap();
    tombstone(&f, &event_bus::escalation_thread_id("target"));
    let error = h
        .advance_handoff(
            "source".into(),
            "incarnation".into(),
            HandoffState::Prepared,
            HandoffState::Committed,
            None,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(error, StorageError::Tombstoned { .. }),
        "{error:?}"
    );
    // The state did not advance and the plan did not move: one transaction.
    assert_eq!(
        f.count("SELECT COUNT(*) FROM lane_handoffs WHERE state = 'prepared'"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM lane_handoffs WHERE state = 'committed'"),
        0
    );
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 1);
}

/// What a plan owns agrees with itself: every history row has its event, and the
/// projection's revision is the newest history row.
fn assert_consistent(f: &Fixture, plans: i64, revision: i64) {
    assert_eq!(f.plan_rows().0, plans);
    assert_eq!(
        f.plan_rows().2,
        f.plan_events(),
        "one event per history row"
    );
    assert_eq!(
        f.count(
            "SELECT COUNT(*) FROM conversation_plan_history h \
             JOIN events e ON e.id = h.event_id WHERE e.kind = 'ConversationPlan'"
        ),
        f.plan_events()
    );
    assert_eq!(
        f.count("SELECT COALESCE(MAX(revision), 0) FROM conversation_plan_history"),
        revision
    );
    assert_eq!(
        f.count("SELECT COALESCE(MAX(revision), 0) FROM conversation_plans"),
        revision
    );
}

/// A writer held at `point` by a test gate while a plan write is in flight on another
/// thread. The SQL connection of the fixture observes the database meanwhile.
fn hold_write_at(
    f: &Fixture,
    point: PlanWritePoint,
) -> (
    Arc<PlanWriteGate>,
    thread::JoinHandle<Result<PlanApplied, StorageError>>,
) {
    let gate = PlanWriteGate::new();
    let handle = f.handle();
    handle.install_plan_write_gate(gate.clone()).unwrap();
    gate.arm(point);
    let writer = thread::spawn(move || {
        handle.update_conversation_plan(update(THREAD, 0, vec![add("a", None, "task")]))
    });
    assert_eq!(gate.wait_reached(), point);
    (gate, writer)
}

#[test]
fn a_stop_before_the_commit_leaves_nothing_and_a_restart_reads_the_old_revision() {
    let f = Fixture::new();
    let (gate, writer) = hold_write_at(&f, PlanWritePoint::BeforeCommit);
    // While held, nothing of the write is visible to another connection.
    assert_consistent(&f, 0, 0);
    assert_eq!(f.plan_rows(), (0, 0, 0, 0));
    // When: the process "stops" before the commit.
    gate.resolve(PlanWriteResolution::Stop);
    assert!(writer.join().unwrap().is_err());
    // Then: after a restart the plan is still unwritten, and the same request is
    // accepted afresh as revision 1, with one event.
    let Fixture {
        _dir,
        config,
        storage,
    } = f;
    storage.close();
    let f = Fixture {
        storage: Storage::open(config.clone()).unwrap(),
        config,
        _dir,
    };
    assert_consistent(&f, 0, 0);
    let h = f.handle();
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 0);
    let applied = h
        .update_conversation_plan(update(THREAD, 0, vec![add("a", None, "task")]))
        .unwrap();
    assert_eq!(applied.revision, 1);
    assert_consistent(&f, 1, 1);
    assert_eq!(f.plan_rows(), (1, 1, 1, 1));
}

#[test]
fn a_stop_after_the_commit_keeps_the_change_whole_and_a_reread_shows_it() {
    let f = Fixture::new();
    let (gate, writer) = hold_write_at(&f, PlanWritePoint::AfterCommit);
    // While held, the commit is already complete and consistent on disk.
    assert_consistent(&f, 1, 1);
    assert_eq!(f.plan_rows(), (1, 1, 1, 1));
    // When: the caller never gets the success (the reply is lost).
    gate.resolve(PlanWriteResolution::Stop);
    assert!(writer.join().unwrap().is_err());
    // Then: re-reading, also after a restart, shows revision 1 exactly once; the caller
    // that retries its old request is told the revision moved instead of applying it twice.
    let Fixture {
        _dir,
        config,
        storage,
    } = f;
    storage.close();
    let f = Fixture {
        storage: Storage::open(config.clone()).unwrap(),
        config,
        _dir,
    };
    let h = f.handle();
    assert_eq!(h.conversation_plan_summary(THREAD).unwrap().revision, 1);
    assert_eq!(
        h.conversation_plan_tree(THREAD, None, None)
            .unwrap()
            .nodes
            .len(),
        1
    );
    let error = h
        .update_conversation_plan(update(THREAD, 0, vec![add("a", None, "task")]))
        .unwrap_err();
    assert!(
        matches!(error, StorageError::StaleRevision { .. }),
        "{error:?}"
    );
    assert_consistent(&f, 1, 1);
    assert_eq!(f.plan_rows(), (1, 1, 1, 1));
}

#[test]
fn a_held_write_that_is_let_go_completes_as_if_it_was_never_held() {
    for point in [PlanWritePoint::BeforeCommit, PlanWritePoint::AfterCommit] {
        let f = Fixture::new();
        let (gate, writer) = hold_write_at(&f, point);
        gate.resolve(PlanWriteResolution::Continue);
        assert_eq!(writer.join().unwrap().unwrap().revision, 1);
        assert_consistent(&f, 1, 1);
    }
}
