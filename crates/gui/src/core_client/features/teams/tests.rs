//! The teams feature drawn headless: the board, its tasks and one task's
//! detail follow what the core answered.
use super::*;
use crate::core_client::features::FeatureError;
use crate::core_client::features::execution::tests::{
    answer, asked, query_of, query_reply, screen,
};
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

fn team(claimed: u32) -> FeatureReply {
    query_reply(
        json!({"type": "teams", "payload": {"type": "team", "payload": {
        "thread": "t1", "source": "persisted", "ready": 1, "claimed": claimed, "complete": 2,
        "members": [{"owner": "worker-1", "run": "run-w", "claimed_tasks": claimed}],
        "revision": "1"}}}),
    )
}

fn task(id: &str, state: &str) -> Value {
    json!({"task_id": id, "state": state, "paths": ["src/lib.rs"], "owner": "worker-1",
        "owner_run": "run-w", "claim_generation": "2", "revision": "1"})
}

fn tasks(items: Vec<Value>) -> FeatureReply {
    query_reply(
        json!({"type": "teams", "payload": {"type": "tasks", "payload": {
        "items": items, "snapshot_revision": "1"}}}),
    )
}

#[test]
fn the_board_its_tasks_and_one_task_detail_are_read_and_shown() {
    let mut screen = screen(Box::<TeamsFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    assert_eq!(first.len(), 2);
    let team_tag = first
        .iter()
        .find(|a| matches!(query_of(a), Query::Teams(TeamsQuery::GetTeam(_))))
        .unwrap()
        .tag;
    let tasks_tag = first
        .iter()
        .find(|a| matches!(query_of(a), Query::Teams(TeamsQuery::ListTeamTasks(_))))
        .unwrap()
        .tag;
    answer(&mut screen, team_tag, team(1));
    answer(
        &mut screen,
        tasks_tag,
        tasks(vec![task("task-1", "claimed"), task("task-2", "complete")]),
    );
    screen.get_by_label("Team");
    screen.get_by_label("Saved board");
    screen.get_by_label("1 ready · 1 claimed · 2 complete");
    screen.get_by_label("worker-1 · 1 task claimed · run run-w");
    screen.get_by_label("task-1");
    screen.get_by_label("task-2");
    screen.get_by_label("Claimed");
    screen.get_by_label("Complete");
    // A path belongs to the opened task, not to the list.
    assert!(screen.query_by_label("src/lib.rs").is_none());

    screen.get_by_label("task-1").click();
    let detail = asked(&mut screen);
    assert_eq!(detail.len(), 1);
    let Query::Teams(TeamsQuery::GetTeamTask(reference)) = query_of(&detail[0]) else {
        panic!("a task detail was expected");
    };
    assert_eq!(
        (reference.thread.as_str(), reference.task_id.as_str()),
        ("t1", "task-1")
    );
    answer(
        &mut screen,
        detail[0].tag,
        query_reply(json!({"type": "teams", "payload": {"type": "task",
            "payload": task("task-1", "claimed")}})),
    );
    screen.get_by_label("Task task-1");
    screen.get_by_label("Owner: worker-1");
    screen.get_by_label_contains("Generation 2");
    screen.get_by_label("Owner run: run-w");
    screen.get_by_label("Paths");
    screen.get_by_label("src/lib.rs");
    assert!(screen.query_by_label("Task task-2").is_none());
}

#[test]
fn a_changed_board_is_asked_for_again_and_another_threads_change_is_ignored() {
    let mut screen = screen(Box::<TeamsFeature>::default(), Some("t1"));
    asked(&mut screen);
    let overview = |thread: &str| {
        serde_json::from_value(json!({
            "thread": thread, "source": "live", "ready": 0, "claimed": 0, "complete": 0,
            "members": [], "revision": "1"}))
        .unwrap()
    };
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Teams(TeamsEvent::TeamChanged {
            team: overview("t2"),
        }));
    assert!(asked(&mut screen).is_empty());
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Teams(TeamsEvent::TeamChanged {
            team: overview("t1"),
        }));
    assert_eq!(asked(&mut screen).len(), 2);
}

#[test]
fn without_a_thread_or_a_board_the_screen_says_so() {
    let mut screen = screen(Box::<TeamsFeature>::default(), None);
    assert!(asked(&mut screen).is_empty());
    screen.get_by_label("Select a conversation to show its team board.");

    screen.state_mut().selection.thread = Some(ThreadId::new("t1").unwrap());
    let first = asked(&mut screen);
    assert_eq!(first.len(), 2);
    screen.get_by_label(crate::panes::team_widgets::EMPTY);
}

#[test]
fn a_thread_without_a_team_shows_the_empty_board_and_not_the_cores_refusal() {
    let mut screen = screen(Box::<TeamsFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    let team_tag = first
        .iter()
        .find(|a| matches!(query_of(a), Query::Teams(TeamsQuery::GetTeam(_))))
        .unwrap()
        .tag;
    answer(
        &mut screen,
        team_tag,
        FeatureReply::Query(Err(FeatureError {
            message: "this thread has no team".to_owned(),
            outcome_unknown: false,
            code: Some(ErrorCode::NotFound),
            retry: None,
        })),
    );
    screen.get_by_label(crate::panes::team_widgets::EMPTY);
    assert!(screen.query_by_label("this thread has no team").is_none());

    // Once a team starts, the board replaces the empty text; a count above one is plural.
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Teams(TeamsEvent::TeamChanged {
            team: serde_json::from_value(json!({
                "thread": "t1", "source": "live", "ready": 0, "claimed": 2, "complete": 0,
                "members": [], "revision": "2"}))
            .unwrap(),
        }));
    let again = asked(&mut screen);
    let team_tag = again
        .iter()
        .find(|a| matches!(query_of(a), Query::Teams(TeamsQuery::GetTeam(_))))
        .unwrap()
        .tag;
    answer(&mut screen, team_tag, team(2));
    screen.get_by_label("worker-1 · 2 tasks claimed · run run-w");
    assert!(
        screen
            .query_by_label(crate::panes::team_widgets::EMPTY)
            .is_none()
    );
}
