//! The execution feature drawn headless: what the screen shows follows what
//! the core answered, a click sends exactly one typed request, and an answer
//! that belongs to a level or thread no longer shown is dropped.
use super::*;
use crate::core_client::{
    cache::Cache,
    features::{FeatureAction, FeatureFrame, FeatureRequest, FeatureView, Selection},
};
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::{Value, json};
use workbench_protocol::ids::ProjectId;

/// What a headless screen holds besides the feature.
pub(crate) struct Screen {
    pub feature: Box<dyn Feature>,
    pub selection: Selection,
    pub cache: Cache,
    pub frame: FeatureFrame,
}

pub(crate) fn screen(feature: Box<dyn Feature>, thread: Option<&str>) -> Harness<'static, Screen> {
    let state = Screen {
        feature,
        selection: Selection {
            project: Some(ProjectId::new("p").unwrap()),
            thread: thread.map(|id| ThreadId::new(id).unwrap()),
        },
        cache: Cache::default(),
        frame: FeatureFrame::default(),
    };
    Harness::new_ui_state(
        |ui, screen: &mut Screen| {
            let capabilities = [Capability::Queries, Capability::Commands];
            let view = FeatureView {
                connected: true,
                capabilities: &capabilities,
                cache: &screen.cache,
                selection: &screen.selection,
            };
            let mut ctx = FeatureContext {
                feature: screen.feature.id(),
                view,
                frame: &mut screen.frame,
            };
            screen.feature.render(ui, &mut ctx);
        },
        state,
    )
}

/// What the screen asked for since the last call.
pub(crate) fn asked(harness: &mut Harness<'_, Screen>) -> Vec<FeatureAction> {
    harness.run_steps(2);
    std::mem::take(&mut harness.state_mut().frame.actions)
}

pub(crate) fn answer(harness: &mut Harness<'_, Screen>, tag: u64, reply: FeatureReply) {
    harness.state_mut().feature.reply(tag, reply);
    harness.run_steps(2);
}

pub(crate) fn query_of(action: &FeatureAction) -> &Query {
    match &action.request {
        FeatureRequest::Query(query) => query,
        FeatureRequest::Command(command) => panic!("a query was expected, got {command:?}"),
    }
}

pub(crate) fn command_of(action: &FeatureAction) -> &Command {
    match &action.request {
        FeatureRequest::Command(command) => command,
        FeatureRequest::Query(query) => panic!("a command was expected, got {query:?}"),
    }
}

pub(crate) fn query_reply(value: Value) -> FeatureReply {
    FeatureReply::Query(Ok(serde_json::from_value(value).expect("a query reply")))
}

pub(crate) fn ok_command() -> FeatureReply {
    FeatureReply::Command(Ok(workbench_protocol::commands::CommandReply::accepted()))
}

fn run(id: &str, parent: Option<&str>, lifecycle: &str, source: &str, children: u32) -> Value {
    let mut summary = json!({"id": id, "thread": "t1", "role": "worker",
        "phase": if lifecycle == "active" { "running" } else { "cancelled" }, "revision": "1"});
    if let Some(parent) = parent {
        summary["parent_run"] = json!(parent);
    }
    json!({"summary": summary, "name": format!("name-{id}"), "lifecycle": lifecycle,
        "source": source, "child_count": children, "revision": "1"})
}

fn runs_reply(items: Vec<Value>, more: bool) -> FeatureReply {
    let mut page = json!({"items": items, "snapshot_revision": "1"});
    if more {
        page["next_cursor"] = json!("next");
    }
    query_reply(json!({"type": "execution", "payload": {"type": "run_details", "payload": page}}))
}

fn execution_reply(goal: bool) -> FeatureReply {
    let mut body = json!({"thread": "t1", "root_run": "run-a",
        "todo": {"list_id": "l", "revision": "1",
                 "items": [{"content": "write tests", "status": "in_progress"}]},
        "ledger": {"run": "run-a", "seq": "1", "body": "ledger-body", "truncated": false}});
    if goal {
        body["goal"] = json!({"goal": "g1", "root_run": "run-a", "objective": "ship it",
            "criteria": ["tests pass"],
            "checks": [{"criterion": 0, "met": true, "evidence": "ci"}],
            "phase": "reviewing", "review_enabled": true, "checks_paused": false,
            "work_stopped": false, "review_round": 1, "max_review_rounds": 3,
            "findings": ["fix the docs"], "epoch": "2"});
    }
    query_reply(json!({"type": "execution",
        "payload": {"type": "thread_execution", "payload": body}}))
}

fn scope_of(action: &FeatureAction) -> RunScope {
    match query_of(action) {
        Query::Execution(ExecutionQuery::ListRunDetails(q)) => q.scope.clone(),
        other => panic!("{other:?}"),
    }
}

fn runs_tag(actions: &[FeatureAction]) -> u64 {
    actions
        .iter()
        .find(|a| {
            matches!(
                query_of(a),
                Query::Execution(ExecutionQuery::ListRunDetails(_))
            )
        })
        .expect("a run listing was asked for")
        .tag
}

#[test]
fn the_four_ends_of_a_run_and_history_only_are_told_apart() {
    let all: Vec<_> = [
        RunLifecycle::Active,
        RunLifecycle::Terminal,
        RunLifecycle::Stopped,
        RunLifecycle::Recovering,
    ]
    .into_iter()
    .map(|l| lifecycle_label(l, RunSource::Live))
    .collect();
    let distinct: std::collections::BTreeSet<_> = all.iter().collect();
    assert_eq!(distinct.len(), 4);
    for lifecycle in [RunLifecycle::Terminal, RunLifecycle::Stopped] {
        let live = lifecycle_label(lifecycle, RunSource::Live);
        let history = lifecycle_label(lifecycle, RunSource::HistoryOnly);
        assert_ne!(live, history);
        assert!(history.contains("履歴のみ"));
    }
}

#[test]
fn a_drill_down_asks_for_the_children_and_drops_an_answer_for_a_level_left() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    assert_eq!(first.len(), 2, "the runs and the thread execution");
    assert_eq!(scope_of(&first[0]), RunScope::Roots);
    let roots_tag = runs_tag(&first);
    answer(
        &mut screen,
        roots_tag,
        runs_reply(
            vec![
                run("run-a", None, "active", "live", 2),
                run("run-b", None, "stopped", "history_only", 0),
            ],
            false,
        ),
    );
    // Both roots stay listed, a stopped one marked as history; only the live
    // active one can be stopped.
    screen.get_by_label("name-run-a");
    screen.get_by_label("name-run-b");
    screen.get_by_label("停止・中止（履歴のみ）");
    assert_eq!(screen.query_all_by_label("停止").count(), 1);

    // A refresh asks for the roots again (the answer comes later), then the
    // drill-down asks for the children of run-a.
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Execution(ExecutionEvent::RunChanged {
            run: serde_json::from_value(
                json!({"id": "run-a", "thread": "t1", "role": "worker", "phase": "running",
                       "revision": "2"}),
            )
            .unwrap(),
        }));
    let refresh = asked(&mut screen);
    assert_eq!(scope_of(&refresh[0]), RunScope::Roots);
    screen.get_by_label("子 run (2)").click();
    let drill = asked(&mut screen);
    assert_eq!(drill.len(), 1);
    assert_eq!(
        scope_of(&drill[0]),
        RunScope::Children {
            parent: RunId::new("run-a").unwrap()
        }
    );
    answer(
        &mut screen,
        drill[0].tag,
        runs_reply(
            vec![run("run-c", Some("run-a"), "terminal", "live", 0)],
            false,
        ),
    );
    screen.get_by_label("name-run-c");
    // The slow answer for the roots now belongs to a level that is gone.
    answer(
        &mut screen,
        refresh[0].tag,
        runs_reply(
            vec![run("run-b", None, "stopped", "history_only", 0)],
            false,
        ),
    );
    screen.get_by_label("name-run-c");
    assert!(screen.query_by_label("name-run-b").is_none());
}

#[test]
fn stop_names_the_shown_thread_and_the_one_run_clicked() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(
            vec![
                run("run-a", None, "active", "live", 0),
                run("run-b", None, "terminal", "live", 0),
            ],
            false,
        ),
    );
    screen.get_by_label("停止").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let Command::Execution(ExecutionCommand::StopRun(stop)) = command_of(&sent[0]) else {
        panic!("a stop was expected");
    };
    assert_eq!(stop.thread.as_str(), "t1");
    assert_eq!(stop.run.as_str(), "run-a");
}

#[test]
fn the_goal_review_and_the_checks_pause_send_their_own_commands() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    let execution_tag = first
        .iter()
        .find(|a| {
            matches!(
                query_of(a),
                Query::Execution(ExecutionQuery::GetThreadExecution(_))
            )
        })
        .unwrap()
        .tag;
    answer(&mut screen, execution_tag, execution_reply(true));
    screen.get_by_label("ゴール: ship it");
    screen.get_by_label("✔ tests pass");
    screen.get_by_label("指摘: fix the docs");
    screen.get_by_label("▶ write tests");

    screen.get_by_label("ゴールレビュー").click();
    let sent = asked(&mut screen);
    let Command::Execution(ExecutionCommand::SetGoalReview(review)) = command_of(&sent[0]) else {
        panic!("a review toggle was expected");
    };
    assert_eq!((review.goal.as_str(), review.enabled), ("g1", false));

    screen.get_by_label("チェックを一時停止").click();
    let sent = asked(&mut screen);
    let Command::Execution(ExecutionCommand::SetGoalChecksPaused(paused)) = command_of(&sent[0])
    else {
        panic!("a pause toggle was expected");
    };
    assert_eq!((paused.goal.as_str(), paused.paused), ("g1", true));
}

#[test]
fn a_changed_thread_starts_over_and_a_reset_asks_again_without_losing_tags() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], false),
    );
    screen.get_by_label("name-run-a");

    // An event of another thread changes nothing.
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Execution(ExecutionEvent::RunChanged {
            run: serde_json::from_value(
                json!({"id": "run-x", "thread": "t2", "role": "worker", "phase": "running",
                       "revision": "1"}),
            )
            .unwrap(),
        }));
    assert!(asked(&mut screen).is_empty());

    // A new snapshot drops what came from the core and asks for it again.
    screen.state_mut().feature.reset();
    let again = asked(&mut screen);
    assert_eq!(again.len(), 2);
    assert!(screen.query_by_label("name-run-a").is_none());

    // A lost reply keeps its tag: the resolved reply later still lands.
    screen.state_mut().selection.thread = Some(ThreadId::new("t2").unwrap());
    let other = asked(&mut screen);
    assert_eq!(other.len(), 2);
    assert!(screen.query_by_label("name-run-a").is_none());
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], false),
    );
    assert!(
        screen.query_by_label("name-run-a").is_none(),
        "an answer for the thread left is dropped"
    );
}

#[test]
fn an_unknown_command_outcome_is_shown_and_never_resent() {
    let mut feature = ExecutionFeature::default();
    let tag = feature.tag(Pending::Command("run の停止"));
    feature.reply(
        tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    assert!(feature.status.contains("結果不明"));
    assert!(
        feature.pending.contains_key(&tag),
        "the resolved reply is awaited"
    );
    feature.reply(tag, ok_command());
    assert!(feature.status.contains("受け付けました"));
    assert!(!feature.pending.contains_key(&tag));
}
