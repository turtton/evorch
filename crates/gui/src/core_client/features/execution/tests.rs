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

fn run_changed(screen: &mut Harness<'_, Screen>) {
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Execution(ExecutionEvent::RunChanged {
            run: serde_json::from_value(
                run("run-b", Some("parent"), "active", "live", 1)["summary"].clone(),
            )
            .unwrap(),
        }));
}

#[test]
fn a_run_refresh_keeps_loaded_pages_visible_until_the_whole_prefix_is_replaced() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    // Pages may be short because of the byte bound, not just the row limit.
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("さらに読み込む").click();
    let more = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&more),
        runs_reply(vec![run("run-b", None, "active", "live", 1)], false),
    );
    screen.get_by_label("子 run (1)");

    // A delayed run event must not collapse the already loaded second page.
    run_changed(&mut screen);
    let refresh = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&refresh),
        runs_reply(vec![run("run-c", None, "active", "live", 0)], true),
    );
    screen.get_by_label("name-run-a");
    screen.get_by_label("子 run (1)");
    let next = asked(&mut screen);
    let Query::Execution(ExecutionQuery::ListRunDetails(query)) = query_of(&next[0]) else {
        panic!("the refreshed prefix's next page");
    };
    assert_eq!(query.page.cursor.as_ref().unwrap().as_str(), "next");
    answer(
        &mut screen,
        runs_tag(&next),
        runs_reply(vec![run("run-b", None, "active", "live", 2)], false),
    );
    screen.get_by_label("name-run-c");
    screen.get_by_label("子 run (2)");
    assert!(screen.query_by_label("name-run-a").is_none());
    assert!(screen.query_by_label("さらに読み込む").is_none());
}

#[test]
fn a_run_change_during_load_more_waits_for_its_reply_then_refreshes_both_pages() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("さらに読み込む").click();
    let more = asked(&mut screen);
    run_changed(&mut screen);
    screen.get_by_label("さらに読み込む").click();
    assert!(
        asked(&mut screen).is_empty(),
        "no duplicate or competing read"
    );
    answer(
        &mut screen,
        runs_tag(&more),
        runs_reply(vec![run("run-b", None, "active", "live", 1)], false),
    );
    let refresh = asked(&mut screen);
    let Query::Execution(ExecutionQuery::ListRunDetails(query)) = query_of(&refresh[0]) else {
        panic!("a refresh was expected");
    };
    assert!(query.page.cursor.is_none());
    answer(
        &mut screen,
        runs_tag(&refresh),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("子 run (1)");
    let next = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&next),
        runs_reply(vec![run("run-b", None, "active", "live", 1)], false),
    );
    screen.get_by_label("子 run (1)");
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn load_more_during_a_refresh_is_kept_and_a_shorter_refresh_can_finish_early() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    run_changed(&mut screen);
    let refresh = asked(&mut screen);
    screen.get_by_label("さらに読み込む").click();
    assert!(asked(&mut screen).is_empty());
    answer(
        &mut screen,
        runs_tag(&refresh),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    let more = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&more),
        runs_reply(vec![run("run-b", None, "active", "live", 1)], false),
    );
    run_changed(&mut screen);
    let refresh = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&refresh),
        runs_reply(vec![run("run-c", None, "active", "live", 0)], false),
    );
    screen.get_by_label("name-run-c");
    assert!(screen.query_by_label("name-run-b").is_none());
    assert!(
        asked(&mut screen).is_empty(),
        "no cursor means the prefix is complete"
    );
}

#[test]
fn a_reset_or_a_revisited_level_drops_old_run_replies_including_errors() {
    for reset in [false, true] {
        let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
        let first = asked(&mut screen);
        answer(
            &mut screen,
            runs_tag(&first),
            runs_reply(vec![run("run-a", None, "active", "live", 1)], false),
        );
        run_changed(&mut screen);
        let old = asked(&mut screen);
        if reset {
            screen.state_mut().feature.reset();
        } else {
            screen.get_by_label("子 run (1)").click();
            let _children = asked(&mut screen);
            screen.get_by_label("ルート").click();
        }
        let current = asked(&mut screen);
        answer(
            &mut screen,
            runs_tag(&current),
            runs_reply(vec![run("run-new", None, "active", "live", 0)], false),
        );
        answer(
            &mut screen,
            runs_tag(&old),
            runs_reply(vec![run("run-old", None, "active", "live", 0)], false),
        );
        screen.get_by_label("name-run-new");
        assert!(screen.query_by_label("name-run-old").is_none());
        // The same invalidation also applies to failed reads, not only successful ones.
        run_changed(&mut screen);
        let old = asked(&mut screen);
        screen.state_mut().feature.reset();
        let current = asked(&mut screen);
        answer(
            &mut screen,
            runs_tag(&old),
            FeatureReply::Query(Err(FeatureError::new("obsolete failure"))),
        );
        assert!(screen.query_by_label("obsolete failure").is_none());
        answer(
            &mut screen,
            runs_tag(&current),
            runs_reply(vec![run("run-new", None, "active", "live", 0)], false),
        );
        screen.get_by_label("name-run-new");
    }
}

#[test]
fn a_failed_page_keeps_the_displayed_prefix_and_releases_the_read() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("さらに読み込む").click();
    let more = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&more),
        FeatureReply::Query(Err(FeatureError::new("page failed"))),
    );
    screen.get_by_label("page failed");
    screen.get_by_label("name-run-a");
    assert!(
        asked(&mut screen).is_empty(),
        "errors are not silently retried"
    );
    run_changed(&mut screen);
    let refresh = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&refresh),
        runs_reply(vec![run("run-new", None, "active", "live", 0)], false),
    );
    screen.get_by_label("name-run-new");
}

#[test]
fn a_cursor_conflict_restarts_the_prefix_without_waiting_for_a_run_changed_event() {
    for during_refresh in [false, true] {
        let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
        let first = asked(&mut screen);
        answer(
            &mut screen,
            runs_tag(&first),
            runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
        );
        screen.get_by_label("さらに読み込む").click();
        let more = asked(&mut screen);
        let conflicting = if during_refresh {
            answer(
                &mut screen,
                runs_tag(&more),
                runs_reply(vec![run("run-b", None, "active", "live", 1)], false),
            );
            run_changed(&mut screen);
            let refresh = asked(&mut screen);
            answer(
                &mut screen,
                runs_tag(&refresh),
                runs_reply(vec![run("run-new", None, "active", "live", 0)], true),
            );
            asked(&mut screen)
        } else {
            more
        };
        // A checkpoint can invalidate this cursor without changing RunSummary,
        // so no RunChanged arrives after the Conflict to trigger recovery.
        answer(
            &mut screen,
            runs_tag(&conflicting),
            FeatureReply::Query(Err(
                FeatureError::new("snapshot changed").with_code(ErrorCode::Conflict)
            )),
        );
        screen.get_by_label("name-run-a");
        if during_refresh {
            screen.get_by_label("子 run (1)");
        }
        let restarted = asked(&mut screen);
        let Query::Execution(ExecutionQuery::ListRunDetails(query)) = query_of(&restarted[0])
        else {
            panic!("the prefix must restart without another event");
        };
        assert!(
            query.page.cursor.is_none(),
            "never resend the invalid cursor"
        );
        answer(
            &mut screen,
            runs_tag(&restarted),
            runs_reply(vec![run("run-new", None, "active", "live", 0)], true),
        );
        screen.get_by_label("name-run-a");
        let next = asked(&mut screen);
        answer(
            &mut screen,
            runs_tag(&next),
            runs_reply(vec![run("run-b", None, "active", "live", 2)], false),
        );
        screen.get_by_label("name-run-new");
        screen.get_by_label("子 run (2)");
        assert!(screen.query_by_label("name-run-a").is_none());
        assert!(asked(&mut screen).is_empty());
    }
}

#[test]
fn a_conflict_on_the_first_page_is_reported_without_restarting() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        FeatureReply::Query(Err(
            FeatureError::new("first page failed").with_code(ErrorCode::Conflict)
        )),
    );
    screen.get_by_label("first page failed");
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn repeated_run_ids_across_pages_keep_one_row_per_id() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("さらに読み込む").click();
    let more = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&more),
        runs_reply(
            vec![
                run("run-a", None, "active", "live", 0),
                run("run-b", None, "active", "live", 1),
                run("run-b", None, "active", "live", 1),
            ],
            false,
        ),
    );
    assert_eq!(screen.query_all_by_label("name-run-a").count(), 1);
    assert_eq!(screen.query_all_by_label("name-run-b").count(), 1);
}

#[test]
fn a_refresh_follows_cursors_for_a_prefix_larger_than_the_maximum_page_size() {
    use workbench_protocol::limits::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};
    let pages = MAX_PAGE_SIZE / DEFAULT_PAGE_SIZE + 1;
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let items = |page: u32| {
        (0..DEFAULT_PAGE_SIZE)
            .map(|row| run(&format!("run-{page}-{row}"), None, "active", "live", 0))
            .collect()
    };
    for page in 0..pages {
        let actions = asked(&mut screen);
        answer(
            &mut screen,
            runs_tag(&actions),
            runs_reply(items(page), page + 1 < pages),
        );
        if page + 1 < pages {
            screen.get_by_label("さらに読み込む").click();
        }
    }
    let count = (pages * DEFAULT_PAGE_SIZE) as usize;
    assert_eq!(screen.query_all_by_label("Worker").count(), count);
    run_changed(&mut screen);
    let mut queries = 0;
    let mut events = 1;
    let refreshes = 3;
    for cycle in 0..refreshes {
        for page in 0..pages {
            let actions = asked(&mut screen);
            assert_eq!(actions.len(), 1, "one serial run query per page");
            queries += actions.len();
            let Query::Execution(ExecutionQuery::ListRunDetails(query)) = query_of(&actions[0])
            else {
                panic!("a page was expected");
            };
            assert_eq!(query.page.limit, DEFAULT_PAGE_SIZE);
            assert_eq!(query.page.cursor.is_some(), page > 0);
            // Bursts while a read is in flight coalesce into one later refresh,
            // rather than restarting the prefix for each event.
            if cycle + 1 < refreshes {
                for _ in 0..10 {
                    run_changed(&mut screen);
                    events += 1;
                }
            }
            answer(
                &mut screen,
                runs_tag(&actions),
                runs_reply(items(page), page + 1 < pages),
            );
            assert_eq!(screen.query_all_by_label("Worker").count(), count);
        }
    }
    assert_eq!(queries, (pages * refreshes) as usize);
    assert!(asked(&mut screen).is_empty());
    eprintln!("{count} rows / {pages} pages / {events} events: {queries} refresh queries");
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
    assert_eq!(
        first.len(),
        3,
        "the runs, the thread execution and the plan"
    );
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
    assert_eq!(again.len(), 3);
    assert!(screen.query_by_label("name-run-a").is_none());

    // A lost reply keeps its tag: the resolved reply later still lands.
    screen.state_mut().selection.thread = Some(ThreadId::new("t2").unwrap());
    let other = asked(&mut screen);
    assert_eq!(other.len(), 3);
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
