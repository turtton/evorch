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
fn a_not_found_for_an_old_selection_keeps_the_new_one() {
    // Given: task t1 is asked for, then t2 is chosen before the answer.
    let mut state = shown(5);
    state.select("t1");
    let requests = state.requests(true);
    let generation = take(&requests, ReadKind::Task).generation;
    state.select("t2");

    // When: the read for t1 fails with "not found".
    state.apply(ReadKind::Task, generation, Err(ReadFailure::NotFound));

    // Then: t2 stays selected and is read next.
    assert_eq!(state.selected(), Some("t2"));
    let requests = state.requests(true);
    assert_eq!(
        take(&requests, ReadKind::Task).query,
        ExecutionQuery::GetConversationPlanTask(PlanTaskQuery {
            thread: thread("a"),
            task_id: "t2".into(),
        })
    );
}

#[test]
fn refresh_reads_every_view_again_at_the_known_revision() {
    // Given: every view read and up to date at revision 5.
    let mut state = shown(5);
    let requests = state.requests(true);
    let generation = take(&requests, ReadKind::Tree).generation;
    state.apply(ReadKind::Tree, generation, Ok(tree_page(5, &["t1"], None)));
    state.apply(ReadKind::History, generation, Ok(history(5, 1..=5)));
    assert!(state.requests(true).is_empty());

    // When: notifications were lost and the views are asked to read again.
    state.refresh();

    // Then: all of them are read though no newer revision is known.
    assert_eq!(
        kinds(&state.requests(true)),
        [ReadKind::Summary, ReadKind::Tree, ReadKind::History]
    );
}

#[test]
fn a_refresh_during_a_read_keeps_the_view_due_for_another() {
    // Given: a summary read is in flight when notifications are found lost.
    let mut state = PlanState::default();
    state.set_thread(Some(thread("a")));
    let requests = state.requests(false);
    let generation = take(&requests, ReadKind::Summary).generation;
    state.refresh();

    // When: the (possibly older) answer arrives.
    state.apply(ReadKind::Summary, generation, Ok(summary("a", 3, 0)));

    // Then: the summary is read once more.
    assert_eq!(kinds(&state.requests(false)), [ReadKind::Summary]);
}

#[test]
fn restart_reads_keeps_the_shown_summary_and_drops_the_old_answers() {
    // Given: the summary shown at revision 5 and a newer read in flight.
    let mut state = shown(5);
    state.invalidate(&thread("a"), Revision(6));
    let requests = state.requests(false);
    let old = take(&requests, ReadKind::Summary).generation;

    // When: the connection is replaced before the answer arrives.
    state.restart_reads();

    // Then: the shown summary stays, the read is sent again, and the late
    // answer of the old connection moves nothing.
    assert_eq!(state.summary().map(|s| s.revision), Some(Revision(5)));
    let again = state.requests(false);
    assert_eq!(kinds(&again), [ReadKind::Summary]);
    assert_ne!(again[0].generation, old);
    state.apply(ReadKind::Summary, old, Ok(summary("a", 9, 900_000)));
    assert_eq!(state.summary().map(|s| s.revision), Some(Revision(5)));
    assert!(
        state.requests(false).is_empty(),
        "the new read is in flight"
    );
}
