//! End-of-work reconciliation: when a root's work period ends, its unfinished
//! items become interrupted, once, before the run's public state moves on, and
//! without any extra provider request.
use super::*;
use ThreadTodoStatus::{Completed, InProgress, Interrupted, Pending};
use event_bus::{EventKind, LifecycleEvent, OrchestratorEvent};

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Seen {
    Todo(String, u64, Vec<ThreadTodoStatus>),
    Phase(AgentRunPhase),
    Turn,
    Diagnostic(String),
}

pub(super) fn statuses(items: &[ThreadTodoItem]) -> Vec<ThreadTodoStatus> {
    items.iter().map(|item| item.status).collect()
}

pub(super) fn todo_seen(thread: &str, revision: u64, statuses: &[ThreadTodoStatus]) -> Seen {
    Seen::Todo(thread.into(), revision, statuses.to_vec())
}

fn observe(event: event_bus::Event, run: RunId) -> Option<Seen> {
    match event.kind {
        EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated { snapshot }) => {
            Some(Seen::Todo(
                snapshot.thread_id.clone(),
                snapshot.revision,
                statuses(&snapshot.items),
            ))
        }
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
            if run_id == run.to_string() =>
        {
            Some(Seen::Phase(to))
        }
        EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. })
            if run_id == run.to_string() =>
        {
            Some(Seen::Turn)
        }
        EventKind::Diagnostic(diagnostic) => Some(Seen::Diagnostic(diagnostic.code)),
        _ => None,
    }
}

/// Everything observable up to and including the first event `done` accepts.
pub(super) async fn until(
    events: &mut event_bus::EventReceiver,
    run: RunId,
    done: impl Fn(&Seen) -> bool,
) -> Vec<Seen> {
    let mut seen = Vec::new();
    loop {
        let event = events.recv().await.unwrap();
        if let Some(observed) = observe(event, run) {
            let finished = done(&observed);
            seen.push(observed);
            if finished {
                return seen;
            }
        }
    }
}

pub(super) fn position(seen: &[Seen], wanted: &Seen) -> usize {
    seen.iter()
        .position(|candidate| candidate == wanted)
        .unwrap_or_else(|| panic!("{wanted:?} not in {seen:?}"))
}

fn terminal(seen: &Seen) -> bool {
    matches!(
        seen,
        Seen::Phase(AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped)
    )
}

fn interactive() -> RunConfig {
    RunConfig {
        interactive: true,
        keep_alive: true,
        ..config()
    }
}

fn running_item() -> Vec<ThreadTodoItem> {
    vec![
        item("Doing", InProgress),
        item("Later", Pending),
        item("Done", Completed),
    ]
}

fn marked() -> Vec<ThreadTodoStatus> {
    vec![Interrupted, Pending, Completed]
}

fn ask(blocking: bool) -> Step {
    Step::Tools(vec![(
        "ask_user",
        json!({"title":"Which option?","blocking":blocking}),
    )])
}

#[tokio::test]
async fn a_completed_turn_marks_running_items_before_waiting_and_adds_no_request() {
    let (runtime, model, mut events) = harness(vec![write(running_item()), Step::Stop, Step::Stop]);
    let run = start(&runtime, Role::Worker, interactive(), true);
    let seen = until(&mut events, run, |seen| *seen == Seen::Turn).await;
    let marked_at = position(&seen, &todo_seen("thread", 2, &marked()));
    assert!(
        marked_at
            > position(
                &seen,
                &todo_seen("thread", 1, &[InProgress, Pending, Completed])
            )
    );
    assert!(marked_at < position(&seen, &Seen::Phase(AgentRunPhase::Waiting)));
    assert!(position(&seen, &Seen::Phase(AgentRunPhase::Waiting)) < position(&seen, &Seen::Turn));
    // The write and the final answer are the only requests; marking made none.
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    let marked_list = runtime.thread_todo("thread").unwrap();
    assert_eq!(
        (marked_list.revision, statuses(&marked_list.items)),
        (2, marked())
    );

    // The next human input is the next request; it carries the marked list
    // after that input, and everything already sent stays a prefix.
    runtime.send_message(run, "Next input".into()).unwrap();
    let seen = until(&mut events, run, |seen| *seen == Seen::Turn).await;
    assert!(!seen.iter().any(|seen| matches!(seen, Seen::Todo(..))));
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let (previous, next) = (&requests[1].1, &requests[2].1);
        assert_eq!(previous[..], next[..previous.len()]);
        let user = next
            .iter()
            .position(|message| {
                message.content.iter().any(
                    |block| matches!(block, ContentBlock::Text { text } if text == "Next input"),
                )
            })
            .unwrap();
        let snapshot = next
            .iter()
            .position(|message| contains_snapshot(std::slice::from_ref(message), &marked_list))
            .expect("marked list reaches the next request");
        assert!(snapshot > user);
    }
    // Nothing is left to mark at the next boundary.
    assert_eq!(runtime.thread_todo("thread").unwrap().revision, 2);
    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);
    assert_eq!(runtime.thread_todo("thread").unwrap().revision, 2);
}

#[tokio::test]
async fn only_a_blocking_unanswered_question_keeps_a_finished_turn_unmarked() {
    // Non-blocking: the turn still ends, so the items are marked.
    let (runtime, _, mut events) = harness(vec![write(running_item()), ask(false), Step::Stop]);
    let run = start(&runtime, Role::Worker, interactive(), true);
    let seen = until(&mut events, run, |seen| {
        *seen == Seen::Phase(AgentRunPhase::Waiting)
    })
    .await;
    position(&seen, &todo_seen("thread", 2, &marked()));
    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();

    // Blocking: the run waits for an answer inside the same work period.
    let (runtime, _, mut events) = harness(vec![write(running_item()), ask(true), Step::Stop]);
    let run = start(&runtime, Role::Worker, interactive(), true);
    let seen = until(&mut events, run, |seen| {
        *seen == Seen::Phase(AgentRunPhase::Waiting)
    })
    .await;
    assert!(!seen.iter().any(|seen| matches!(seen, Seen::Todo(_, 2, _))));
    let waiting = runtime.thread_todo("thread").unwrap();
    assert_eq!(waiting.revision, 1);
    assert_eq!(waiting.items, running_item());
    // Stopping the waiting run ends the period.
    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    let seen = until(&mut events, run, terminal).await;
    assert!(
        position(&seen, &todo_seen("thread", 2, &marked()))
            < position(&seen, &Seen::Phase(AgentRunPhase::Stopped))
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);
}

enum End {
    Natural,
    ProviderError,
    Panic,
    Stop,
    Cancel,
}

#[tokio::test]
async fn every_way_a_run_ends_marks_running_items_before_the_terminal_event() {
    for (end, expected) in [
        (End::Natural, AgentRunPhase::Done),
        (End::ProviderError, AgentRunPhase::Error),
        (End::Panic, AgentRunPhase::Error),
        (End::Stop, AgentRunPhase::Stopped),
        (End::Cancel, AgentRunPhase::Error),
    ] {
        let entered = Arc::new(Notify::new());
        let last = match end {
            End::Natural => Step::Stop,
            End::ProviderError => Step::Fail,
            End::Panic => Step::Panic,
            End::Stop | End::Cancel => Step::Hold(entered.clone(), Arc::new(Notify::new())),
        };
        let (runtime, model, mut events) = harness(vec![write(running_item()), last]);
        let run = start(&runtime, Role::Worker, config(), true);
        match end {
            End::Stop => {
                entered.notified().await;
                runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
            }
            End::Cancel => {
                entered.notified().await;
                runtime.cancel(run).unwrap();
            }
            _ => {}
        }
        let seen = until(&mut events, run, terminal).await;
        let name = format!("{expected:?}");
        assert_eq!(seen.last(), Some(&Seen::Phase(expected)), "{name}");
        assert!(
            position(&seen, &todo_seen("thread", 2, &marked()))
                < position(&seen, &Seen::Phase(expected)),
            "{name}: {seen:?}"
        );
        let requests = model.requests.lock().unwrap().len();
        assert_eq!(runtime.wait(run).await.unwrap(), expected);
        // A second look at the same ended period changes and publishes nothing.
        assert_eq!(runtime.reconcile_current_root_todo(run, None), Ok(false));
        assert!(
            events
                .drain_pending_snapshot()
                .into_iter()
                .filter_map(|event| observe(event, run))
                .all(|seen| !matches!(seen, Seen::Todo(..))),
            "{name}"
        );
        assert_eq!(runtime.thread_todo("thread").unwrap().revision, 2, "{name}");
        assert_eq!(model.requests.lock().unwrap().len(), requests, "{name}");
    }
}

#[test]
fn reconciliation_is_scoped_idempotent_and_never_creates_a_list() {
    let (runtime, _, mut events) = harness(vec![]);
    let (old, current) = (runtime.reserve_run_id(), runtime.reserve_run_id());
    // Unregistered roots and missing lists are left alone.
    assert_eq!(runtime.reconcile_current_root_todo(old, None), Ok(false));
    runtime.bind_thread_root("thread", old).unwrap();
    assert_eq!(runtime.reconcile_current_root_todo(old, None), Ok(false));
    assert!(runtime.thread_todo("thread").is_none());

    let mut snapshot = restored(vec![
        item("a", InProgress),
        item("b", InProgress),
        item("c", Pending),
        item("d", Completed),
        item("e", Interrupted),
    ]);
    snapshot.revision = 7;
    runtime.restore_thread_todo(snapshot.clone()).unwrap();
    // A replaced root cannot touch the list its successor owns.
    runtime.bind_thread_root("thread", current).unwrap();
    assert_eq!(runtime.reconcile_current_root_todo(old, None), Ok(false));
    assert_eq!(runtime.thread_todo("thread"), Some(snapshot.clone()));
    assert!(events.drain_pending_snapshot().is_empty());

    assert_eq!(runtime.reconcile_current_root_todo(current, None), Ok(true));
    let marked = runtime.thread_todo("thread").unwrap();
    assert_eq!(marked.revision, 8);
    assert_eq!(marked.list_id, snapshot.list_id);
    let contents: Vec<_> = marked
        .items
        .iter()
        .map(|item| item.content.as_str())
        .collect();
    assert_eq!(contents, ["a", "b", "c", "d", "e"]);
    assert_eq!(
        statuses(&marked.items),
        [Interrupted, Interrupted, Pending, Completed, Interrupted]
    );
    assert_eq!(events.drain_pending_snapshot().len(), 1);
    assert_eq!(
        runtime.reconcile_current_root_todo(current, None),
        Ok(false)
    );
    assert!(events.drain_pending_snapshot().is_empty());
}

#[test]
fn an_exhausted_revision_is_an_error_and_leaves_the_list_as_it_was() {
    let (runtime, _, mut events) = harness(vec![]);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    let mut snapshot = restored(vec![item("a", InProgress)]);
    snapshot.revision = u64::MAX;
    runtime.restore_thread_todo(snapshot.clone()).unwrap();
    assert!(runtime.reconcile_current_root_todo(root, None).is_err());
    assert!(!runtime.reconcile_todo_or_report(root, None));
    assert_eq!(runtime.thread_todo("thread"), Some(snapshot));
    let codes: Vec<_> = events
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|event| observe(event, root))
        .collect();
    assert_eq!(
        codes,
        [Seen::Diagnostic(
            event_bus::event::diagnostic_codes::THREAD_TODO_RECONCILE_FAILED.into()
        )]
    );
}

#[tokio::test]
async fn an_old_owner_generation_cannot_mark_a_list_it_no_longer_owns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owners.db");
    let mut registry = crate::ownership::Registry::open(&path).unwrap();
    let owner = crate::ownership::ThreadOwner::new(
        "thread".into(),
        crate::ownership::Lease {
            owner_id: "original".into(),
            generation: 1,
            expires_at: 1,
        },
    );
    registry.start(&owner).unwrap();
    let permit = crate::ownership::OwnerPermit {
        registry_path: path,
        thread_id: "thread".into(),
        lease: owner.lease.clone(),
        run_id: None,
    };
    let (runtime, _, mut events) = harness(vec![]);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    runtime
        .restore_thread_todo(restored(vec![item("a", InProgress)]))
        .unwrap();
    // While the permit is current, the root may mark its list.
    assert_eq!(
        runtime.reconcile_current_root_todo(root, Some(&permit)),
        Ok(true)
    );

    let mut again = restored(vec![item("a", InProgress)]);
    again.revision = 9;
    runtime.restore_thread_todo(again.clone()).unwrap();
    let current = registry.attach("thread").unwrap();
    registry
        .update("thread", |state| {
            state.claim(&current.lease, "replacement", current.lease.expires_at, 0)
        })
        .unwrap();
    events.drain_pending_snapshot();
    // The same RunId under the replaced generation is fenced off.
    assert!(
        runtime
            .reconcile_current_root_todo(root, Some(&permit))
            .is_err()
    );
    assert!(!runtime.reconcile_todo_or_report(root, Some(&permit)));
    assert_eq!(runtime.thread_todo("thread"), Some(again));
    let seen: Vec<_> = events
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|event| observe(event, root))
        .collect();
    assert_eq!(
        seen,
        [Seen::Diagnostic(
            event_bus::event::diagnostic_codes::THREAD_TODO_RECONCILE_FAILED.into()
        )]
    );
}

#[tokio::test]
async fn restoring_a_finished_turn_marks_but_a_mid_request_crash_keeps_running_items() {
    let directory = tempfile::tempdir().unwrap();
    let storage_config = storage::StorageConfig {
        db_path: directory.path().join("runs.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let store = || crate::RunStore::open(&storage_config, storage.handle()).unwrap();

    // A finished turn was saved; the crash came before its marking was stored.
    let (original, _, mut events) =
        harness_with_store(vec![write(running_item()), Step::Stop], Some(store()));
    let run = start(&original, Role::Worker, interactive(), true);
    until(&mut events, run, |seen| *seen == Seen::Turn).await;
    original.stop(run, crate::StopScope::SelfOnly).unwrap();
    original.wait(run).await.unwrap();
    let mut crashed = original.thread_todo("thread").unwrap();
    crashed.items = running_item();
    crashed.revision += 1;

    let (runtime, model, mut events) = harness_with_store(vec![Step::Stop], Some(store()));
    runtime.restore_thread_todo(crashed.clone()).unwrap();
    runtime.bind_thread_root("thread", run).unwrap();
    runtime.resume_chat(run, config()).unwrap();
    let seen = until(&mut events, run, |seen| *seen == Seen::Turn).await;
    position(&seen, &todo_seen("thread", crashed.revision + 1, &marked()));
    assert!(position(&seen, &Seen::Phase(AgentRunPhase::Waiting)) < position(&seen, &Seen::Turn));
    assert!(
        model.requests.lock().unwrap().is_empty(),
        "restoring asks nothing"
    );
    runtime.send_message(run, "Continue".into()).unwrap();
    until(&mut events, run, |seen| *seen == Seen::Turn).await;
    {
        let requests = model.requests.lock().unwrap();
        let marked_list = runtime.thread_todo("thread").unwrap();
        assert!(contains_snapshot(&requests[0].1, &marked_list));
    }
    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();

    // A request interrupted mid-turn: the saved list stays as written until the
    // retried turn is over.
    let (running, entered) = (running_item(), Arc::new(Notify::new()));
    let release = Arc::new(Notify::new());
    let (first, first_model, _) = harness_with_store(
        vec![write(running.clone()), Step::Hold(entered.clone(), release)],
        Some(store()),
    );
    let mid = start(&first, Role::Worker, interactive(), true);
    entered.notified().await;
    first.stop(mid, crate::StopScope::SelfOnly).unwrap();
    first.wait(mid).await.unwrap();
    let interrupted = first_model.requests.lock().unwrap().last().unwrap().clone();
    let mut crashed = first.thread_todo("thread").unwrap();
    crashed.items = running.clone();
    crashed.revision += 1;

    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (runtime, model, mut events) = harness_with_store(
        vec![Step::Hold(entered.clone(), release.clone()), Step::Stop],
        Some(store()),
    );
    runtime.restore_thread_todo(crashed.clone()).unwrap();
    runtime.bind_thread_root("thread", mid).unwrap();
    runtime.resume_chat(mid, config()).unwrap();
    entered.notified().await;
    assert_eq!(model.requests.lock().unwrap()[0], interrupted);
    assert_eq!(runtime.thread_todo("thread"), Some(crashed.clone()));
    release.notify_one();
    let seen = until(&mut events, mid, |seen| {
        *seen == Seen::Phase(AgentRunPhase::Waiting)
    })
    .await;
    position(&seen, &todo_seen("thread", crashed.revision + 1, &marked()));
    runtime.stop(mid, crate::StopScope::SelfOnly).unwrap();
    runtime.wait(mid).await.unwrap();
}

fn escalation(call: &'static str) -> Step {
    Step::Tools(vec![(
        call,
        json!({"original_request":"Research","escalation_reason":"Parallel investigation"}),
    )])
}

#[tokio::test]
async fn a_successful_escalation_moves_the_list_without_marking_or_a_second_event() {
    let entered = Arc::new(Notify::new());
    let (runtime, _, mut events) = harness(vec![
        write(running_item()),
        escalation("escalate"),
        Step::Hold(entered.clone(), Arc::new(Notify::new())),
    ]);
    let source = start(&runtime, Role::Worker, config(), true);
    entered.notified().await;
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
    let child = runtime
        .list_agents()
        .into_iter()
        .find(|row| row.run_id != source)
        .unwrap()
        .run_id;
    let moved_thread = event_bus::escalation_thread_id(&child.to_string());
    let todos: Vec<_> = events
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|event| observe(event, source))
        .filter(|seen| matches!(seen, Seen::Todo(..)))
        .collect();
    // The write, then the move to the new thread; neither is marked.
    assert_eq!(
        todos,
        [
            todo_seen("thread", 1, &[InProgress, Pending, Completed]),
            todo_seen(&moved_thread, 2, &[InProgress, Pending, Completed]),
        ]
    );
    assert!(runtime.thread_todo("thread").is_none());
    assert_eq!(
        runtime.thread_todo(&moved_thread).unwrap().items,
        running_item()
    );
}

#[tokio::test]
async fn a_failed_escalation_still_marks_the_source_list() {
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (runtime, model, mut events) = harness(vec![
        write(running_item()),
        ask(true),
        Step::GatedTools(
            entered.clone(),
            release.clone(),
            vec![(
                "escalate",
                json!({"original_request":"Research","escalation_reason":"Parallel investigation"}),
            )],
        ),
    ]);
    let source = start(&runtime, Role::Worker, config(), true);
    entered.notified().await;
    // As in the question-inheritance failure: the handoff cannot complete.
    rusqlite::Connection::open(model._dir.path().join("review.db"))
        .unwrap()
        .execute_batch("DROP TABLE user_question_links")
        .unwrap();
    release.notify_one();
    let seen = until(&mut events, source, terminal).await;
    let failure = Seen::Diagnostic("EscalationHandoffFailed".into());
    let marked_at = position(&seen, &todo_seen("thread", 2, &marked()));
    assert!(position(&seen, &failure) < position(&seen, &Seen::Phase(AgentRunPhase::Done)));
    assert!(marked_at < position(&seen, &Seen::Phase(AgentRunPhase::Done)));
    assert_eq!(runtime.list_agents().len(), 1);
    assert_eq!(
        statuses(&runtime.thread_todo("thread").unwrap().items),
        marked()
    );
}

#[tokio::test]
async fn a_run_under_a_live_owner_marks_its_list_after_its_terminal_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owners.db");
    let mut registry = crate::ownership::Registry::open(&path).unwrap();
    let owner = crate::ownership::ThreadOwner::new(
        "thread".into(),
        crate::ownership::Lease {
            owner_id: "original".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    registry.start(&owner).unwrap();
    let entered = Arc::new(Notify::new());
    let (runtime, _, mut events) = harness(vec![
        write(running_item()),
        Step::Hold(entered.clone(), Arc::new(Notify::new())),
    ]);
    let run = start(
        &runtime,
        Role::Worker,
        RunConfig {
            ownership: Some(crate::ownership::OwnerPermit {
                registry_path: path,
                thread_id: "thread".into(),
                lease: owner.lease.clone(),
                run_id: None,
            }),
            ..config()
        },
        true,
    );
    entered.notified().await;
    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    let seen = until(&mut events, run, terminal).await;
    assert_eq!(seen.last(), Some(&Seen::Phase(AgentRunPhase::Stopped)));
    assert!(
        position(&seen, &todo_seen("thread", 2, &marked()))
            < position(&seen, &Seen::Phase(AgentRunPhase::Stopped))
    );
    assert!(
        !seen.iter().any(
            |seen| matches!(seen, Seen::Diagnostic(code) if code == "ThreadTodoReconcileFailed")
        )
    );
}
