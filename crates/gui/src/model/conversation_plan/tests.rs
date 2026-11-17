use workbench_protocol::namespaces::execution::plan::{
    PlanNodeDetail, PlanStatusValues, PlanTaskStatus,
};
use workbench_protocol::wire::Sequence;

use super::*;

fn thread(id: &str) -> ThreadId {
    ThreadId::new(id).expect("thread id")
}

fn progress(nodes: u32, completed_ppm: u32) -> PlanProgress {
    PlanProgress {
        node_count: nodes,
        leaf_count: nodes,
        leaf_counts: PlanStatusValues {
            pending: nodes,
            ..PlanStatusValues::default()
        },
        weighted_ppm: PlanStatusValues {
            completed: completed_ppm,
            pending: 1_000_000 - completed_ppm,
            ..PlanStatusValues::default()
        },
    }
}

fn summary(id: &str, revision: u64, completed_ppm: u32) -> ExecutionReply {
    ExecutionReply::ConversationPlan(Box::new(PlanSummary {
        thread: thread(id),
        plan_id: Some("plan-1".into()),
        revision: Revision(revision),
        progress: progress(2, completed_ppm),
        updated_at_unix_ms: None,
    }))
}

fn node(task_id: &str) -> PlanNodeView {
    PlanNodeView {
        task_id: task_id.into(),
        parent_id: None,
        position: 0,
        depth: 0,
        title: task_id.into(),
        weight: 1,
        status: Some(PlanTaskStatus::Pending),
        child_count: 0,
        progress_ppm: PlanStatusValues::default(),
    }
}

fn tree_page(revision: u64, ids: &[&str], next: Option<&str>) -> ExecutionReply {
    ExecutionReply::ConversationPlanTree(Box::new(PlanTreePage {
        plan_id: Some("plan-1".into()),
        revision: Revision(revision),
        progress: progress(2, 0),
        nodes: ids.iter().map(|id| node(id)).collect(),
        next_cursor: next.map(str::to_owned),
    }))
}

fn task(task_id: &str, revision: u64) -> ExecutionReply {
    ExecutionReply::ConversationPlanTask(Box::new(PlanTaskRead {
        plan_id: "plan-1".into(),
        revision: Revision(revision),
        node: PlanNodeDetail {
            task_id: task_id.into(),
            parent_id: None,
            position: 0,
            depth: 0,
            title: task_id.into(),
            description: String::new(),
            result: String::new(),
            blocker: String::new(),
            weight: 1,
            status: Some(PlanTaskStatus::Pending),
            child_ids: Vec::new(),
            progress_ppm: PlanStatusValues::default(),
        },
    }))
}

fn history(revision: u64, revisions: std::ops::RangeInclusive<u64>) -> ExecutionReply {
    ExecutionReply::ConversationPlanHistory(Box::new(PlanHistoryPage {
        plan_id: Some("plan-1".into()),
        revision: Revision(revision),
        entries: revisions
            .map(|revision| PlanHistoryEntry {
                revision: Revision(revision),
                event_id: Sequence(revision),
                change: None,
                updated_at_unix_ms: None,
            })
            .collect(),
        next_after_revision: None,
    }))
}

fn kinds(requests: &[PlanRequest]) -> Vec<ReadKind> {
    requests.iter().map(|request| request.kind).collect()
}

fn take(requests: &[PlanRequest], kind: ReadKind) -> &PlanRequest {
    requests
        .iter()
        .find(|request| request.kind == kind)
        .expect("request of kind")
}

/// A state showing thread `a` with its summary at `revision`.
fn shown(revision: u64) -> PlanState {
    let mut state = PlanState::default();
    state.set_thread(Some(thread("a")));
    let requests = state.requests(false);
    let request = take(&requests, ReadKind::Summary);
    state.apply(
        ReadKind::Summary,
        request.generation,
        Ok(summary("a", revision, 500_000)),
    );
    state
}

#[test]
fn only_the_summary_is_read_while_the_panel_is_hidden() {
    let mut state = PlanState::default();
    state.set_thread(Some(thread("a")));
    assert_eq!(kinds(&state.requests(false)), [ReadKind::Summary]);
    // The panel reads wait until it is shown, then go out once.
    assert_eq!(
        kinds(&state.requests(true)),
        [ReadKind::Tree, ReadKind::History]
    );
    assert!(state.requests(true).is_empty());
}

#[test]
fn a_late_notification_does_not_reread_or_move_back() {
    let mut state = shown(5);
    assert!(state.requests(false).is_empty());
    state.invalidate(&thread("a"), Revision(4));
    state.invalidate(&thread("a"), Revision(5));
    assert!(state.requests(false).is_empty());
    assert_eq!(state.summary().map(|s| s.revision), Some(Revision(5)));
}

#[test]
fn a_newer_notification_rereads_and_an_older_answer_is_dropped() {
    let mut state = shown(5);
    state.invalidate(&thread("a"), Revision(7));
    let requests = state.requests(false);
    let generation = take(&requests, ReadKind::Summary).generation;
    // An answer that raced the notification still names the older plan.
    state.apply(ReadKind::Summary, generation, Ok(summary("a", 6, 900_000)));
    state.apply(ReadKind::Summary, generation, Ok(summary("a", 4, 100_000)));
    let shown = state.summary().expect("summary");
    assert_eq!(shown.revision, Revision(6));
    assert_eq!(completed_percent(&shown.progress), 90);
}

#[test]
fn a_notification_for_another_thread_is_ignored() {
    let mut state = shown(5);
    state.invalidate(&thread("b"), Revision(9));
    assert!(state.requests(false).is_empty());
}

#[test]
fn switching_threads_drops_the_old_plan_and_its_answers() {
    let mut state = shown(5);
    let old = state.requests(true);
    let old_generation = take(&old, ReadKind::Tree).generation;
    state.set_thread(Some(thread("b")));
    assert!(state.summary().is_none());
    state.apply(
        ReadKind::Tree,
        old_generation,
        Ok(tree_page(5, &["x"], None)),
    );
    assert!(state.tree().is_none());
    let requests = state.requests(false);
    let ExecutionQuery::GetConversationPlan(target) = &take(&requests, ReadKind::Summary).query
    else {
        panic!("summary query");
    };
    assert_eq!(target.thread, thread("b"));
}

#[test]
fn tree_pages_are_joined_and_restart_when_the_plan_moves() {
    let mut state = shown(5);
    let requests = state.requests(true);
    let generation = take(&requests, ReadKind::Tree).generation;
    state.apply(
        ReadKind::Tree,
        generation,
        Ok(tree_page(5, &["a1"], Some("c1"))),
    );
    let requests = state.requests(true);
    let ExecutionQuery::ReadConversationPlanTree(query) = &take(&requests, ReadKind::Tree).query
    else {
        panic!("tree query");
    };
    assert_eq!(query.cursor.as_deref(), Some("c1"));
    // The cursor went stale: the next read starts again without one.
    state.apply(ReadKind::Tree, generation, Err(ReadFailure::Conflict));
    let requests = state.requests(true);
    let ExecutionQuery::ReadConversationPlanTree(query) = &take(&requests, ReadKind::Tree).query
    else {
        panic!("tree query");
    };
    assert_eq!(query.cursor, None);
    state.apply(
        ReadKind::Tree,
        generation,
        Ok(tree_page(6, &["a1", "a2"], None)),
    );
    let tree = state.tree().expect("tree");
    assert_eq!(tree.revision, Revision(6));
    let ids: Vec<_> = tree.nodes.iter().map(|n| n.task_id.as_str()).collect();
    assert_eq!(ids, ["a1", "a2"]);
}

#[test]
fn a_selected_task_that_is_gone_is_deselected() {
    let mut state = shown(5);
    state.select("t1");
    let requests = state.requests(true);
    let generation = take(&requests, ReadKind::Task).generation;
    state.apply(ReadKind::Task, generation, Ok(task("t1", 5)));
    assert_eq!(state.task().map(|t| t.node.task_id.as_str()), Some("t1"));
    state.invalidate(&thread("a"), Revision(6));
    let requests = state.requests(true);
    assert!(kinds(&requests).contains(&ReadKind::Task));
    state.apply(ReadKind::Task, generation, Err(ReadFailure::NotFound));
    assert_eq!(state.selected(), None);
    assert!(state.task().is_none());
}

#[test]
fn history_pages_count_back_from_the_newest_revision() {
    let mut state = shown(25);
    assert_eq!(state.history_pages(), 3);
    let requests = state.requests(true);
    let request = take(&requests, ReadKind::History);
    let ExecutionQuery::ReadConversationPlanHistory(query) = &request.query else {
        panic!("history query");
    };
    assert_eq!(query.after_revision, Some(Revision(15)));
    let generation = request.generation;
    state.apply(ReadKind::History, generation, Ok(history(25, 16..=25)));
    let page = state.history().expect("history");
    assert_eq!(page.entries.first().map(|e| e.revision), Some(Revision(25)));
    assert_eq!(page.pages, 3);

    state.set_history_page(9);
    assert_eq!(state.history_page(), 2);
    let requests = state.requests(true);
    let ExecutionQuery::ReadConversationPlanHistory(query) =
        &take(&requests, ReadKind::History).query
    else {
        panic!("history query");
    };
    assert_eq!(query.after_revision, Some(Revision(0)));
    // The oldest page only shows what is below the page above it.
    state.apply(ReadKind::History, generation, Ok(history(25, 1..=10)));
    let page = state.history().expect("history");
    assert_eq!(page.page, 2);
    assert_eq!(page.entries.len(), 5);
    assert_eq!(page.entries.first().map(|e| e.revision), Some(Revision(5)));
}

#[test]
fn an_empty_plan_is_not_planned_and_not_complete() {
    let empty = PlanProgress::default();
    assert!(!is_planned(&empty));
    assert_eq!(completed_percent(&empty), 0);
    assert_eq!(completed_percent(&progress(3, 400_000)), 40);
}

#[test]
fn the_model_reads_through_the_reader_and_wakes_once_answered() {
    struct Reader;
    impl PlanReader for Reader {
        fn read(&self, query: &ExecutionQuery) -> Result<ExecutionReply, ReadFailure> {
            match query {
                ExecutionQuery::GetConversationPlan(target) => {
                    Ok(summary(&target.thread.to_string(), 3, 250_000))
                }
                _ => Err(ReadFailure::NotFound),
            }
        }
    }
    let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut model = ConversationPlanModel::default()
        .with_reader(Arc::new(Reader))
        .with_spawner(inline_spawner());
    let counter = Arc::clone(&woken);
    model.set_waker(Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    model.state_mut().set_thread(Some(thread("a")));
    model.poll(false);
    model.poll(false);
    assert!(woken.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert_eq!(
        model.state().summary().map(|s| s.revision),
        Some(Revision(3))
    );
}
