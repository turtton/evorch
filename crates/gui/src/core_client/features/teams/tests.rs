//! The teams feature drawn headless: the board, its tasks and one task's
//! detail follow what the core answered.
use super::*;
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
    screen.get_by_label("保存済みのボード（履歴のみ） — 待機 1 / 担当中 1 / 完了 2");
    screen.get_by_label("task-1");
    screen.get_by_label("task-2");

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
    screen.get_by_label("タスク task-1");
    screen.get_by_label("担当 run: run-w");
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
