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
                data: &mut super::super::data::DataInbox::default(),
                feature: screen.feature.id(),
                view,
                frame: &mut screen.frame,
            };
            // As the dock draws a tab: maintained, then rendered.
            let visibility = super::super::Visibility {
                available: true,
                expanded: true,
                minimized: false,
            };
            screen.feature.maintain(&mut ctx, visibility);
            screen.feature.render(ui, &mut ctx);
            screen.feature.render_overlay(&ui.ctx().clone(), &mut ctx);
        },
        state,
    )
}

/// A run without a parent, a root, of `thread` in the screen's cache.
fn add_root(screen: &mut Harness<'_, Screen>, thread: &str, id: &str) {
    let run = serde_json::from_value(json!({"id": id, "thread": thread, "role": "orchestrator",
        "phase": "running", "revision": "1"}))
    .unwrap();
    screen.state_mut().cache.runs.insert(id.into(), run);
}

/// The tab for t1, whose current root is `root`.
fn rooted() -> Harness<'static, Screen> {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    add_root(&mut screen, "t1", "root");
    screen
}

fn root_scope() -> RunScope {
    RunScope::Children {
        parent: RunId::new("root").unwrap(),
    }
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
    let mut body = json!({"thread": "t1", "root_run": "root",
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
    let mut screen = rooted();
    let first = asked(&mut screen);
    // Pages may be short because of the byte bound, not just the row limit.
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("Load more runs").click();
    let more = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&more),
        runs_reply(vec![run("run-b", None, "active", "live", 1)], false),
    );
    screen.get_by_label("Subagents (1)");

    // A delayed run event must not collapse the already loaded second page.
    run_changed(&mut screen);
    let refresh = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&refresh),
        runs_reply(vec![run("run-c", None, "active", "live", 0)], true),
    );
    screen.get_by_label("name-run-a");
    screen.get_by_label("Subagents (1)");
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
    screen.get_by_label("Subagents (2)");
    assert!(screen.query_by_label("name-run-a").is_none());
    assert!(screen.query_by_label("Load more runs").is_none());
}

#[test]
fn a_run_change_during_load_more_waits_for_its_reply_then_refreshes_both_pages() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("Load more runs").click();
    let more = asked(&mut screen);
    run_changed(&mut screen);
    screen.get_by_label("Load more runs").click();
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
    screen.get_by_label("Subagents (1)");
    let next = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&next),
        runs_reply(vec![run("run-b", None, "active", "live", 1)], false),
    );
    screen.get_by_label("Subagents (1)");
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn load_more_during_a_refresh_is_kept_and_a_shorter_refresh_can_finish_early() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    run_changed(&mut screen);
    let refresh = asked(&mut screen);
    screen.get_by_label("Load more runs").click();
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
        let mut screen = rooted();
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
            screen.get_by_label("Subagents (1)").click();
            let _children = asked(&mut screen);
            screen.get_by_label("Root").click();
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
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("Load more runs").click();
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
        let mut screen = rooted();
        let first = asked(&mut screen);
        answer(
            &mut screen,
            runs_tag(&first),
            runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
        );
        screen.get_by_label("Load more runs").click();
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
            screen.get_by_label("Subagents (1)");
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
        screen.get_by_label("Subagents (2)");
        assert!(screen.query_by_label("name-run-a").is_none());
        assert!(asked(&mut screen).is_empty());
    }
}

#[test]
fn a_conflict_on_the_first_page_is_reported_without_restarting() {
    let mut screen = rooted();
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
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], true),
    );
    screen.get_by_label("Load more runs").click();
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
    let mut screen = rooted();
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
            screen.get_by_label("Load more runs").click();
        }
    }
    let count = (pages * DEFAULT_PAGE_SIZE) as usize;
    assert_eq!(screen.query_all_by_label("Open pane").count(), count);
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
            assert_eq!(screen.query_all_by_label("Open pane").count(), count);
        }
    }
    assert_eq!(queries, (pages * refreshes) as usize);
    assert!(asked(&mut screen).is_empty());
    eprintln!("{count} rows / {pages} pages / {events} events: {queries} refresh queries");
}

fn detail(lifecycle: &str, phase: &str, source: &str) -> RunDetail {
    let mut value = run("run-a", None, lifecycle, source, 0);
    value["summary"]["phase"] = json!(phase);
    serde_json::from_value(value).unwrap()
}

#[test]
fn the_four_ends_of_a_run_and_history_only_are_told_apart() {
    let ends = [
        ("active", "running"),
        ("terminal", "done"),
        ("stopped", "cancelled"),
        ("recovering", "running"),
    ];
    let live: std::collections::BTreeSet<_> = ends
        .iter()
        .map(|(lifecycle, phase)| card::status_text(&detail(lifecycle, phase, "live")))
        .collect();
    assert_eq!(
        live,
        ["Running", "Done", "Stopped", "Waiting for recovery"]
            .map(String::from)
            .into()
    );
    for (lifecycle, phase) in ends {
        let history = card::status_text(&detail(lifecycle, phase, "history_only"));
        assert!(history.ends_with(" (history only)"), "{history}");
    }
}

#[test]
fn a_drill_down_asks_for_the_children_and_drops_an_answer_for_a_level_left() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    assert_eq!(
        first.len(),
        3,
        "the runs, the thread execution and the plan"
    );
    let roots_tag = runs_tag(&first);
    let roots = first.iter().find(|a| a.tag == roots_tag).unwrap();
    assert_eq!(scope_of(roots), root_scope());
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
    screen.get_by_label("worker · Stopped (history only)");
    assert_eq!(screen.query_all_by_label("Stop").count(), 1);

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
    let refresh_tag = runs_tag(&refresh);
    let roots = refresh.iter().find(|a| a.tag == refresh_tag).unwrap();
    assert_eq!(scope_of(roots), root_scope());
    screen.get_by_label("Subagents (2)").click();
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
        refresh_tag,
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
    let mut screen = rooted();
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
    // The first click only asks for confirmation.
    screen.get_by_label("Stop").click();
    assert!(asked(&mut screen).is_empty());
    screen.get_by_label("Confirm?").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let Command::Execution(ExecutionCommand::StopRun(stop)) = command_of(&sent[0]) else {
        panic!("a stop was expected");
    };
    assert_eq!(stop.thread.as_str(), "t1");
    assert_eq!(stop.run.as_str(), "run-a");
}

fn execution_tag(actions: &[FeatureAction]) -> u64 {
    actions
        .iter()
        .find(|a| {
            matches!(
                query_of(a),
                Query::Execution(ExecutionQuery::GetThreadExecution(_))
            )
        })
        .expect("the thread execution was asked for")
        .tag
}

#[test]
fn the_goal_is_left_to_the_band_and_the_working_note_folds() {
    // Given: the thread's goal and its root's working note answered.
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(&mut screen, execution_tag(&first), execution_reply(true));

    // Then: the goal is not drawn here, and the note stays folded.
    assert!(screen.query_by_label_contains("ship it").is_none());
    assert!(
        screen
            .query_by_label("Current revision 1: ledger-body")
            .is_none()
    );

    // When: the note is opened.
    screen.get_by_label("> Working note").click();
    screen.run_steps(2);

    // Then: its current revision shows, without a truncation line.
    screen.get_by_label("Current revision 1: ledger-body");
    assert!(
        screen
            .query_by_label("Only the start of this note is shown.")
            .is_none()
    );
}

#[test]
fn a_changed_thread_starts_over_and_a_reset_asks_again_without_losing_tags() {
    let mut screen = rooted();
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

    add_root(&mut screen, "t2", "root-2");
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
    let tag = feature.tag(Pending::Command("Stop run"));
    feature.reply(
        tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    assert_eq!(
        feature.status,
        "Stop run: outcome unknown (not resent) — connection lost"
    );
    assert!(
        feature.pending.contains_key(&tag),
        "the resolved reply is awaited"
    );
    feature.reply(tag, ok_command());
    assert_eq!(feature.status, "Stop run: accepted");
    assert!(!feature.pending.contains_key(&tag));
}

/// The feature with its tab hidden: only `maintain` runs, as for a tab behind
/// another one in the dock.
struct Hidden {
    feature: ExecutionFeature,
    selection: Selection,
    cache: Cache,
}

impl Hidden {
    fn new(thread: &str) -> Self {
        Self {
            feature: ExecutionFeature::default(),
            selection: Selection {
                project: Some(ProjectId::new("p").unwrap()),
                thread: Some(ThreadId::new(thread).unwrap()),
            },
            cache: Cache::default(),
        }
    }

    fn select(&mut self, thread: &str) {
        self.selection.thread = Some(ThreadId::new(thread).unwrap());
    }

    /// One pass; what it asked for.
    fn maintain(&mut self) -> Vec<FeatureAction> {
        self.maintain_available(true)
    }

    /// One pass with the area `available` or not; what it asked for.
    fn maintain_available(&mut self, available: bool) -> Vec<FeatureAction> {
        let capabilities = [Capability::Queries, Capability::Commands];
        let mut frame = FeatureFrame::default();
        let mut ctx = FeatureContext {
            data: &mut super::super::data::DataInbox::default(),
            feature: FeatureId::Execution,
            view: FeatureView {
                connected: true,
                capabilities: &capabilities,
                cache: &self.cache,
                selection: &self.selection,
            },
            frame: &mut frame,
        };
        let visibility = super::super::Visibility {
            available,
            expanded: false,
            minimized: false,
        };
        self.feature.maintain(&mut ctx, visibility);
        frame.actions
    }

    fn band_label(&self) -> String {
        crate::panes::conversation_plan::band_label(self.feature.band().unwrap().plan.summary)
    }
}

fn plan_query<'a>(actions: &'a [FeatureAction], kind: &str) -> Option<&'a FeatureAction> {
    actions.iter().find(|a| {
        let Query::Execution(query) = query_of(a) else {
            return false;
        };
        match query {
            ExecutionQuery::GetConversationPlan(_) => kind == "summary",
            ExecutionQuery::ReadConversationPlanTree(_) => kind == "tree",
            ExecutionQuery::ReadConversationPlanHistory(_) => kind == "history",
            ExecutionQuery::GetConversationPlanTask(_) => kind == "task",
            _ => false,
        }
    })
}

fn summary_tag(actions: &[FeatureAction]) -> u64 {
    plan_query(actions, "summary")
        .expect("the plan summary is read")
        .tag
}

fn plan_summary(thread: &str, revision: u64, completed: u32, blocked: u32) -> FeatureReply {
    let leaves = json!({"pending": 4 - completed - blocked, "in_progress": 0,
        "completed": completed, "blocked": blocked, "interrupted": 0});
    let weights = json!({"pending": 1_000_000 - completed * 250_000, "in_progress": 0,
        "completed": completed * 250_000, "blocked": 0, "interrupted": 0});
    query_reply(
        json!({"type": "execution", "payload": {"type": "conversation_plan",
        "payload": {"thread": thread, "plan_id": "plan-1", "revision": revision.to_string(),
            "progress": {"node_count": 5, "leaf_count": 4, "leaf_counts": leaves,
                "weighted_ppm": weights}}}}),
    )
}

fn empty_plan(revision: u64) -> FeatureReply {
    let empty = json!({"pending": 0, "in_progress": 0, "completed": 0, "blocked": 0,
        "interrupted": 0});
    query_reply(json!({"type": "execution", "payload": {
    "type": "conversation_plan", "payload": {"thread": "t1", "plan_id": null,
        "revision": revision.to_string(), "progress": {"node_count": 0, "leaf_count": 0,
            "leaf_counts": empty, "weighted_ppm": empty}}}}))
}

fn plan_changed(revision: u64) -> UiEvent {
    UiEvent::Execution(ExecutionEvent::ConversationPlanChanged {
        thread: ThreadId::new("t1").unwrap(),
        revision: workbench_protocol::wire::Revision(revision),
    })
}

#[test]
fn plan_summary_is_read_from_maintain_while_the_tab_is_hidden() {
    // Given: the Execution tab is never drawn (Teams is in front of it).
    let mut hidden = Hidden::new("t1");

    // When: the dock maintains the hidden feature.
    let first = hidden.maintain();

    // Then: the goal and the plan summary are read, the runs are not, and the
    // answer reaches the band.
    assert!(first.iter().any(|a| matches!(
        query_of(a),
        Query::Execution(ExecutionQuery::GetThreadExecution(_))
    )));
    assert!(plan_query(&first, "tree").is_none());
    assert!(!first.iter().any(|a| matches!(
        query_of(a),
        Query::Execution(ExecutionQuery::ListRunDetails(_))
    )));
    hidden
        .feature
        .reply(summary_tag(&first), plan_summary("t1", 3, 2, 1));
    assert_eq!(
        hidden.band_label(),
        "Plan 50% complete, 2 of 4 tasks completed, 1 blocked"
    );
    assert!(hidden.maintain().is_empty(), "nothing is read twice");
}

#[test]
fn a_newer_plan_notice_rereads_and_a_late_notice_or_answer_never_moves_back() {
    let mut hidden = Hidden::new("t1");
    let tag = summary_tag(&hidden.maintain());
    hidden.feature.reply(tag, plan_summary("t1", 5, 3, 0));
    assert_eq!(
        hidden.band_label(),
        "Plan 75% complete, 3 of 4 tasks completed"
    );

    // A notice for the revision already shown, or an older one, asks nothing.
    hidden.feature.event(&plan_changed(5));
    hidden.feature.event(&plan_changed(2));
    assert!(plan_query(&hidden.maintain(), "summary").is_none());

    hidden.feature.event(&plan_changed(6));
    let reread = summary_tag(&hidden.maintain());
    // An answer older than what is shown is dropped.
    hidden.feature.reply(reread, plan_summary("t1", 4, 1, 0));
    assert_eq!(
        hidden.band_label(),
        "Plan 75% complete, 3 of 4 tasks completed"
    );
}

#[test]
fn a_thread_switch_drops_the_old_plan_and_its_late_answer() {
    let mut hidden = Hidden::new("t1");
    let old = summary_tag(&hidden.maintain());
    hidden.select("t2");
    let switched = hidden.maintain();
    hidden.feature.reply(old, plan_summary("t1", 9, 4, 0));
    let band = hidden.feature.band().unwrap();
    assert_eq!(band.thread.as_str(), "t2");
    assert!(
        band.plan.summary.is_none(),
        "the old thread's answer is dropped"
    );
    hidden
        .feature
        .reply(summary_tag(&switched), plan_summary("t2", 1, 1, 0));
    assert_eq!(
        hidden.band_label(),
        "Plan 25% complete, 1 of 4 tasks completed"
    );
}

#[test]
fn an_unplanned_and_a_cleared_plan_are_told_apart() {
    for (revision, label) in [(0, "Not planned"), (4, "Plan cleared")] {
        let mut hidden = Hidden::new("t1");
        let tag = summary_tag(&hidden.maintain());
        hidden.feature.reply(tag, empty_plan(revision));
        assert_eq!(hidden.band_label(), label);
    }
}

#[test]
fn detail_reads_wait_for_the_plan_tab() {
    let mut hidden = Hidden::new("t1");
    let tag = summary_tag(&hidden.maintain());
    hidden.feature.reply(tag, plan_summary("t1", 3, 2, 0));
    assert!(
        hidden.maintain().is_empty(),
        "no tree or history while hidden"
    );

    // The Plan tab is drawn: its reads leave in the next pass.
    assert!(hidden.feature.plan_drawn(), "the tab was hidden before");
    let shown = hidden.maintain();
    assert!(plan_query(&shown, "tree").is_some());
    assert!(plan_query(&shown, "history").is_some());
    assert!(!hidden.feature.plan_drawn(), "the tab stays shown");

    hidden
        .feature
        .enqueue_plan(PlanPanelAction::Select("task-1".into()));
    let selected = hidden.maintain();
    let Query::Execution(ExecutionQuery::GetConversationPlanTask(task)) =
        query_of(plan_query(&selected, "task").expect("the selected task is read"))
    else {
        unreachable!()
    };
    assert_eq!(
        (task.thread.as_str(), task.task_id.as_str()),
        ("t1", "task-1")
    );
}

#[test]
fn a_reconnect_rereads_without_a_stuck_read() {
    // Given: the summary shown, and a newer read in flight when the
    // connection is replaced.
    let mut hidden = Hidden::new("t1");
    let tag = summary_tag(&hidden.maintain());
    hidden.feature.reply(tag, plan_summary("t1", 3, 2, 0));
    hidden.feature.event(&plan_changed(4));
    let old = summary_tag(&hidden.maintain());

    // When: a new snapshot resets the feature.
    hidden.feature.reset();

    // Then: the summary is read again at once and stays shown meanwhile, and
    // the old connection's late answer is ignored.
    let again = summary_tag(&hidden.maintain());
    assert_ne!(again, old);
    assert_eq!(
        hidden.band_label(),
        "Plan 50% complete, 2 of 4 tasks completed"
    );
    hidden.feature.reply(old, plan_summary("t1", 9, 4, 0));
    assert_eq!(
        hidden.band_label(),
        "Plan 50% complete, 2 of 4 tasks completed"
    );
    hidden.feature.reply(again, plan_summary("t1", 4, 3, 0));
    assert_eq!(
        hidden.band_label(),
        "Plan 75% complete, 3 of 4 tasks completed"
    );
}

#[test]
fn queued_goal_actions_send_review_and_pause_for_the_shown_goal() {
    let mut hidden = Hidden::new("t1");
    let first = hidden.maintain();
    let execution = first
        .iter()
        .find(|a| {
            matches!(
                query_of(a),
                Query::Execution(ExecutionQuery::GetThreadExecution(_))
            )
        })
        .unwrap()
        .tag;
    hidden.feature.reply(execution, execution_reply(true));
    assert_eq!(
        hidden
            .feature
            .band()
            .unwrap()
            .goal
            .map(|g| g.objective.as_str()),
        Some("ship it")
    );

    // An action for another goal or thread (pressed before a switch) is dropped.
    for (thread, goal) in [("t1", "g-old"), ("t2", "g1")] {
        hidden.feature.enqueue_goal(GoalAction::Review {
            thread_id: thread.into(),
            goal_id: goal.into(),
            enabled: false,
        });
    }
    assert!(hidden.maintain().is_empty());

    hidden.feature.enqueue_goal(GoalAction::Review {
        thread_id: "t1".into(),
        goal_id: "g1".into(),
        enabled: false,
    });
    hidden.feature.enqueue_goal(GoalAction::ChecksPaused {
        thread_id: "t1".into(),
        goal_id: "g1".into(),
        paused: true,
    });
    let sent = hidden.maintain();
    assert_eq!(sent.len(), 2);
    let Command::Execution(ExecutionCommand::SetGoalReview(review)) = command_of(&sent[0]) else {
        panic!("a review toggle was expected");
    };
    assert_eq!(
        (review.thread.as_str(), review.goal.as_str(), review.enabled),
        ("t1", "g1", false)
    );
    let Command::Execution(ExecutionCommand::SetGoalChecksPaused(paused)) = command_of(&sent[1])
    else {
        panic!("a pause toggle was expected");
    };
    assert_eq!((paused.goal.as_str(), paused.paused), ("g1", true));
    hidden.feature.reply(sent[0].tag, ok_command());
    assert_eq!(hidden.feature.status, "Goal review: accepted");
}

// Given: the roots of t1 listed
// When: Open pane is pressed on run-a's row
// Then: the shell is handed run-a with its name once
#[test]
fn open_pane_hands_the_row_to_the_shell_once() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], false),
    );
    screen.get_by_label("Open pane").click();
    screen.run_steps(2);
    let take = |screen: &mut Harness<'_, Screen>| {
        screen
            .state_mut()
            .feature
            .as_execution_mut()
            .unwrap()
            .take_open_pane()
    };
    assert_eq!(
        take(&mut screen),
        Some(RunPane {
            thread: ThreadId::new("t1").unwrap(),
            run: RunId::new("run-a").unwrap(),
            name: "name-run-a".into(),
            child: false,
        })
    );
    assert_eq!(take(&mut screen), None);
}

// Given: a worker with a category and a model, one without a model, and one
//   waiting for recovery
// When: the roots are drawn
// Then: each card reads like a row of the legacy Subagents column
#[test]
fn a_run_card_reads_like_the_subagents_column() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    let mut worker = run("run-a", None, "active", "live", 0);
    worker["category"] = json!("quick");
    worker["model"] = json!("openai/gpt-5");
    let mut recovering = run("run-c", None, "recovering", "live", 0);
    recovering["summary"]["role"] = json!("reviewer");
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(
            vec![
                worker,
                run("run-b", None, "terminal", "live", 0),
                recovering,
            ],
            false,
        ),
    );
    screen.get_by_label("name-run-a");
    screen.get_by_label("Worker(quick) · Running");
    screen.get_by_label("openai/gpt-5 · openai");
    screen.get_by_label("reviewer · Waiting for recovery");
    assert_eq!(
        screen.query_all_by_label("unknown · unknown").count(),
        2,
        "a run without a model names neither it nor its provider"
    );
    assert_eq!(screen.query_all_by_label("Copy run ID").count(), 3);
    assert_eq!(screen.query_all_by_label("Open pane").count(), 3);
    assert_eq!(screen.query_all_by_label("Stop").count(), 1);
    assert!(
        screen.query_by_label("run-a").is_none(),
        "the id is copied, not shown"
    );
    assert!(screen.query_by_label_contains("Subagents (").is_none());
}

// Given: a root with subagents and one without
// When: a name or the breadcrumb is clicked
// Then: only a run with subagents drills in, and Root returns to the roots
#[test]
fn the_name_and_the_count_drill_in_and_root_returns() {
    let mut screen = rooted();
    let roots = |screen: &mut Harness<'_, Screen>, actions: &[FeatureAction]| {
        answer(
            screen,
            runs_tag(actions),
            runs_reply(
                vec![
                    run("run-a", None, "active", "live", 2),
                    run("run-b", None, "terminal", "live", 0),
                ],
                false,
            ),
        );
    };
    let first = asked(&mut screen);
    roots(&mut screen, &first);

    screen.get_by_label("name-run-b").click();
    assert!(asked(&mut screen).is_empty(), "a run without subagents");

    screen.get_by_label("name-run-a").click();
    let drill = asked(&mut screen);
    assert_eq!(drill.len(), 1);
    assert_eq!(
        scope_of(&drill[0]),
        RunScope::Children {
            parent: RunId::new("run-a").unwrap()
        }
    );
    screen.get_by_label("run-a");
    answer(&mut screen, drill[0].tag, runs_reply(vec![], false));
    screen.get_by_label("No subagents of this run");

    screen.get_by_label("Root").click();
    let back = asked(&mut screen);
    assert_eq!(scope_of(&back[0]), root_scope());
    roots(&mut screen, &back);
    assert!(screen.query_by_label("run-a").is_none());
    screen.get_by_label("Subagents (2)");
}

fn frame_at(screen: &mut Harness<'_, Screen>, time: f64) {
    screen.input_mut().time = Some(time);
    screen.step();
}

// Given: Stop pressed once on an active run
// When: two seconds pass without the confirmation
// Then: nothing was sent and Stop is offered again
#[test]
fn a_stop_confirmation_expires_without_a_command() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", None, "active", "live", 0)], false),
    );
    frame_at(&mut screen, 100.0);
    screen.get_by_label("Stop").click();
    frame_at(&mut screen, 100.0);
    frame_at(&mut screen, 100.5);
    screen.get_by_label("Confirm?");

    frame_at(&mut screen, 102.5);
    frame_at(&mut screen, 102.5);
    assert!(screen.query_by_label("Confirm?").is_none());
    screen.get_by_label("Stop");
    assert!(std::mem::take(&mut screen.state_mut().frame.actions).is_empty());
}

// Given: a thread without runs, no thread, and a root without subagents
// When: the tab is drawn
// Then: without a root nothing is listed and the legacy empty state is shown; a root
//       whose listing is empty says it has no subagents
#[test]
fn an_empty_thread_and_no_thread_read_like_the_subagents_column() {
    let mut empty = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut empty);
    assert!(
        !first.iter().any(|a| matches!(
            query_of(a),
            Query::Execution(ExecutionQuery::ListRunDetails(_))
        )),
        "no root, no listing"
    );
    empty.get_by_label("No runs in this thread");
    empty.get_by_label("Start a conversation to see its agent runs.");

    let mut none = super::tests::screen(Box::<ExecutionFeature>::default(), None);
    none.run_steps(2);
    none.get_by_label("No runs in this thread");

    let mut rooted = rooted();
    let first = asked(&mut rooted);
    answer(&mut rooted, runs_tag(&first), runs_reply(vec![], false));
    rooted.get_by_label("No subagents of this run");
    assert!(rooted.query_by_label("Root").is_none());

    // A run change re-reads the listing; the line stays while it is read, so what is
    // drawn below it does not move.
    run_changed(&mut rooted);
    let again = asked(&mut rooted);
    rooted.get_by_label("No subagents of this run");
    answer(&mut rooted, runs_tag(&again), runs_reply(vec![], false));
    rooted.get_by_label("No subagents of this run");
}

// Given: t1's current root without subagents, and an earlier root whose subagent runs
// When: the tab is drawn, the earlier root is pressed, then Root
// Then: the earlier root is offered with its count; pressed, its subagents are listed;
//       Root lists the current root's again
#[test]
fn an_earlier_roots_subagents_stay_reachable() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    add_root(&mut screen, "t1", "root");
    add_root(&mut screen, "t1", "root-old");
    let child = serde_json::from_value(
        run("run-x", Some("root-old"), "active", "live", 0)["summary"].clone(),
    )
    .unwrap();
    screen.state_mut().cache.runs.insert("run-x".into(), child);
    let first = asked(&mut screen);
    answer(&mut screen, execution_tag(&first), execution_reply(false));
    let listing = asked(&mut screen);
    let current = listing
        .iter()
        .find(|a| a.tag == runs_tag(&listing))
        .unwrap();
    assert_eq!(scope_of(current), root_scope());
    answer(&mut screen, current.tag, runs_reply(vec![], false));
    screen.get_by_label("No subagents of this run");

    screen.get_by_label("root-old · Subagents (1)").click();
    let earlier = asked(&mut screen);
    let earlier = earlier
        .iter()
        .find(|a| a.tag == runs_tag(&earlier))
        .unwrap();
    assert_eq!(
        scope_of(earlier),
        RunScope::Children {
            parent: RunId::new("root-old").unwrap()
        }
    );
    answer(
        &mut screen,
        earlier.tag,
        runs_reply(
            vec![run("run-x", Some("root-old"), "active", "live", 0)],
            false,
        ),
    );
    screen.get_by_label("name-run-x");

    screen.get_by_label("Root").click();
    let back = asked(&mut screen);
    let back = back.iter().find(|a| a.tag == runs_tag(&back)).unwrap();
    assert_eq!(scope_of(back), root_scope());
}

// Given: t1 listed under the root the cache knows
// When: the core's thread execution names another root, then that root is answered
// Then: the listing moves to the named root's subagents and the old answer is dropped,
//       and a drill-down taken under the old root is left
#[test]
fn the_thread_executions_root_wins_and_a_new_root_starts_over() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    let old = runs_tag(&first);
    assert_eq!(
        scope_of(first.iter().find(|a| a.tag == old).unwrap()),
        root_scope()
    );
    answer(
        &mut screen,
        old,
        runs_reply(vec![run("run-a", Some("root"), "active", "live", 1)], false),
    );
    screen.get_by_label("Subagents (1)").click();
    let drill = asked(&mut screen);
    screen.get_by_label("Root");

    let execution = json!({"thread": "t1", "root_run": "root-new"});
    answer(
        &mut screen,
        execution_tag(&first),
        query_reply(json!({"type": "execution",
            "payload": {"type": "thread_execution", "payload": execution}})),
    );
    let moved = asked(&mut screen);
    let moved = moved
        .iter()
        .find(|a| a.tag == runs_tag(&moved))
        .unwrap()
        .clone();
    assert_eq!(
        scope_of(&moved),
        RunScope::Children {
            parent: RunId::new("root-new").unwrap()
        }
    );
    assert!(screen.query_by_label("Root").is_none(), "the path is left");
    answer(
        &mut screen,
        drill[0].tag,
        runs_reply(
            vec![run("run-old", Some("run-a"), "active", "live", 0)],
            false,
        ),
    );
    answer(
        &mut screen,
        moved.tag,
        runs_reply(
            vec![run("run-new", Some("root-new"), "active", "live", 0)],
            false,
        ),
    );
    screen.get_by_label("name-run-new");
    assert!(screen.query_by_label("name-run-old").is_none());
}

// Given: the root's subagents listed, with their names
// When: the shell wants the names of a listed run and of one not listed, twice
// Then: the listed name is known at once; the other is read with GetRun once, and its
//       answer names it
#[test]
fn names_come_from_the_listing_and_from_one_run_read_each() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(vec![run("run-a", Some("root"), "active", "live", 0)], false),
    );
    let run_a = RunId::new("run-a").unwrap();
    let run_x = RunId::new("run-x").unwrap();
    let t1 = ThreadId::new("t1").unwrap();
    {
        let execution = screen.state_mut().feature.as_execution_mut().unwrap();
        assert_eq!(execution.run_name(&run_a), Some("name-run-a"));
        assert_eq!(execution.run_name(&run_x), None);
        execution.want_name(t1.clone(), run_x.clone());
        execution.want_name(t1.clone(), run_x.clone());
    }
    let reads = asked(&mut screen);
    let [read] = &reads[..] else {
        panic!("one run read was expected, got {reads:?}");
    };
    let Query::Execution(ExecutionQuery::GetRun(target)) = query_of(read) else {
        panic!("a run read was expected");
    };
    assert_eq!(
        (target.thread.as_str(), target.run.as_str()),
        ("t1", "run-x")
    );
    screen
        .state_mut()
        .feature
        .as_execution_mut()
        .unwrap()
        .want_name(t1, run_x.clone());
    assert!(asked(&mut screen).is_empty(), "asked once");
    let mut detail = run("run-x", Some("run-a"), "active", "live", 0);
    detail["name"] = json!("redirect-tester");
    answer(
        &mut screen,
        read.tag,
        query_reply(json!({"type": "execution", "payload": {"type": "run", "payload": detail}})),
    );
    assert_eq!(
        screen
            .state_mut()
            .feature
            .as_execution_mut()
            .unwrap()
            .run_name(&run_x),
        Some("redirect-tester")
    );
}

fn name_reads(screen: &mut Harness<'_, Screen>) -> Vec<(u64, String)> {
    screen.run_steps(1);
    std::mem::take(&mut screen.state_mut().frame.actions)
        .iter()
        .filter_map(|action| match query_of(action) {
            Query::Execution(ExecutionQuery::GetRun(target)) => {
                Some((action.tag, target.run.as_str().to_owned()))
            }
            _ => None,
        })
        .collect()
}

// Given: the root's subagents listed, and the shell wanting the names of eleven more runs
// When: frames pass while the first reads are unanswered, a listing names one of the
//       rest, then one read is refused by the client, one by the core for good, one by
//       the busy core and one answered
// Then: no more than NAME_READS_IN_FLIGHT are unanswered at once, however many frames
//       pass; the listed name is not read; the client's and the busy core's refusals
//       are read again with the rest, the one refused for good is not
#[test]
fn name_reads_stay_few_in_flight_and_a_refused_one_is_asked_again() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    answer(&mut screen, runs_tag(&first), runs_reply(Vec::new(), false));
    let t1 = ThreadId::new("t1").unwrap();
    {
        let execution = screen.state_mut().feature.as_execution_mut().unwrap();
        for n in 0..11 {
            execution.want_name(t1.clone(), RunId::new(format!("run-{n:02}")).unwrap());
        }
        assert!(execution.names_waiting());
    }
    let reads = name_reads(&mut screen);
    assert_eq!(reads.len(), NAME_READS_IN_FLIGHT);
    for _ in 0..3 {
        assert!(
            name_reads(&mut screen).is_empty(),
            "the rest wait for answers"
        );
    }
    assert!(
        !screen
            .state_mut()
            .feature
            .as_execution_mut()
            .unwrap()
            .names_waiting(),
        "nothing to ask while the reads in flight are at the bound"
    );
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Execution(ExecutionEvent::RunChanged {
            run: serde_json::from_value(json!({"id": "run-10", "thread": "t1",
                "parent_run": "root", "role": "worker", "phase": "running", "revision": "2"}))
            .unwrap(),
        }));
    let relisted = asked(&mut screen);
    answer(
        &mut screen,
        runs_tag(&relisted),
        runs_reply(
            vec![run("run-10", Some("root"), "active", "live", 0)],
            false,
        ),
    );

    let (refused, refused_run) = reads[0].clone();
    let (missing, _) = reads[1].clone();
    let (named, _) = reads[2].clone();
    let (busy, busy_run) = reads[3].clone();
    screen.state_mut().feature.reply(
        busy,
        FeatureReply::Query(Err(FeatureError::new("busy").with_code(ErrorCode::Busy))),
    );
    screen.state_mut().feature.reply(
        refused,
        FeatureReply::Query(Err(FeatureError::new("client queue is full or closed"))),
    );
    screen.state_mut().feature.reply(
        missing,
        FeatureReply::Query(Err(
            FeatureError::new("no such run").with_code(ErrorCode::NotFound)
        )),
    );
    let detail = run(&reads[2].1, Some("root"), "active", "live", 0);
    screen.state_mut().feature.reply(
        named,
        query_reply(json!({"type": "execution", "payload": {"type": "run", "payload": detail}})),
    );
    let again = name_reads(&mut screen);
    assert_eq!(
        again
            .iter()
            .map(|(_, run)| run.as_str())
            .collect::<Vec<_>>(),
        [refused_run.as_str(), busy_run.as_str(), "run-08", "run-09"]
    );
    for (tag, run_id) in again {
        let detail = run(&run_id, Some("root"), "active", "live", 0);
        screen.state_mut().feature.reply(
            tag,
            query_reply(
                json!({"type": "execution", "payload": {"type": "run", "payload": detail}}),
            ),
        );
    }
    assert!(
        name_reads(&mut screen).is_empty(),
        "run-10 was named by the listing"
    );
    assert!(
        !screen
            .state_mut()
            .feature
            .as_execution_mut()
            .unwrap()
            .names_waiting()
    );
}

/// The run reads among `actions`, by tag and run.
fn run_reads(actions: &[FeatureAction]) -> Vec<(u64, String)> {
    actions
        .iter()
        .filter_map(|action| match query_of(action) {
            Query::Execution(ExecutionQuery::GetRun(target)) => {
                Some((action.tag, target.run.as_str().to_owned()))
            }
            _ => None,
        })
        .collect()
}

fn named_reply(run_id: &str) -> FeatureReply {
    let detail = run(run_id, Some("root"), "active", "live", 0);
    query_reply(json!({"type": "execution", "payload": {"type": "run", "payload": detail}}))
}

// Given: the names of more runs wanted than NAME_READS_IN_FLIGHT, the first reads out
// When: the thread is deselected (or the area unavailable) while those are answered, one
//       refused by the client
// Then: names_waiting is false while no thread is selected and while the area is
//       unavailable, so the view does not draw again for them; once t1 is selected and
//       available again the rest and the refused one are read
#[test]
fn names_wait_quietly_while_none_can_be_asked_and_resume_on_reselect() {
    let mut hidden = Hidden::new("t1");
    let t1 = ThreadId::new("t1").unwrap();
    for n in 0..10 {
        hidden
            .feature
            .want_name(t1.clone(), RunId::new(format!("run-{n:02}")).unwrap());
    }
    let reads = run_reads(&hidden.maintain());
    assert_eq!(reads.len(), NAME_READS_IN_FLIGHT);

    hidden.selection.thread = None;
    assert!(run_reads(&hidden.maintain()).is_empty());
    let (refused, refused_run) = reads[0].clone();
    hidden.feature.reply(
        refused,
        FeatureReply::Query(Err(FeatureError::new("client queue is full or closed"))),
    );
    for (tag, run_id) in &reads[1..] {
        hidden.feature.reply(*tag, named_reply(run_id));
    }
    assert!(!hidden.feature.names_waiting(), "no thread is selected");
    assert!(run_reads(&hidden.maintain()).is_empty());
    assert!(!hidden.feature.names_waiting());

    hidden.select("t1");
    assert!(run_reads(&hidden.maintain_available(false)).is_empty());
    assert!(!hidden.feature.names_waiting(), "the area is unavailable");

    let again = run_reads(&hidden.maintain());
    assert_eq!(
        again
            .iter()
            .map(|(_, run)| run.as_str())
            .collect::<Vec<_>>(),
        [refused_run.as_str(), "run-08", "run-09"]
    );
    assert!(!hidden.feature.names_waiting());
}

// Given: one name read per kind of refusal
// When: the core refuses them
// Then: a refusal for a while (busy or a fault without advice, any code whose advice
//       allows asking again) is read again, NAME_READ_ATTEMPTS times in all per
//       session; a missing or hidden run, or advice that forbids it, is not; a new
//       session reads the run that was refused for a while again
#[test]
fn a_name_refused_for_a_while_is_read_again_a_few_times() {
    let mut hidden = Hidden::new("t1");
    let t1 = ThreadId::new("t1").unwrap();
    let refusals = [
        (
            "run-a",
            FeatureError::new("busy").with_code(ErrorCode::Busy),
            true,
        ),
        (
            "run-b",
            FeatureError::new("draining").with_code(ErrorCode::Draining),
            true,
        ),
        (
            "run-c",
            FeatureError::new("store").with_code(ErrorCode::StorageUnavailable),
            true,
        ),
        (
            "run-d",
            FeatureError::new("odd")
                .with_code(ErrorCode::Conflict)
                .with_retry(RetryAdvice::SameRequest),
            true,
        ),
        (
            "run-e",
            FeatureError::new("gone").with_code(ErrorCode::NotFound),
            false,
        ),
        (
            "run-f",
            FeatureError::new("hidden")
                .with_code(ErrorCode::PermissionDenied)
                .with_retry(RetryAdvice::SameRequest),
            false,
        ),
        (
            "run-g",
            FeatureError::new("never")
                .with_code(ErrorCode::Busy)
                .with_retry(RetryAdvice::Never),
            false,
        ),
    ];
    for (run_id, _, _) in &refusals {
        hidden
            .feature
            .want_name(t1.clone(), RunId::new(*run_id).unwrap());
    }
    let reads = run_reads(&hidden.maintain());
    assert_eq!(reads.len(), refusals.len());
    for ((tag, _), (_, error, _)) in reads.iter().zip(&refusals) {
        hidden
            .feature
            .reply(*tag, FeatureReply::Query(Err(error.clone())));
    }
    let again = run_reads(&hidden.maintain());
    let expected: Vec<_> = refusals
        .iter()
        .filter(|(_, _, again)| *again)
        .map(|(run_id, _, _)| *run_id)
        .collect();
    assert_eq!(
        again
            .iter()
            .map(|(_, run)| run.as_str())
            .collect::<Vec<_>>(),
        expected
    );

    let busy = || FeatureReply::Query(Err(FeatureError::new("busy").with_code(ErrorCode::Busy)));
    let mut tag = again[0].0;
    for attempt in 2..=NAME_READ_ATTEMPTS {
        hidden.feature.reply(tag, busy());
        let reads = run_reads(&hidden.maintain());
        if attempt == NAME_READ_ATTEMPTS {
            assert!(reads.is_empty(), "run-a was refused {attempt} times");
        } else {
            let [(next, run_id)] = &reads[..] else {
                panic!("run-a alone was expected, got {reads:?}");
            };
            assert_eq!(run_id, "run-a");
            tag = *next;
        }
    }
    assert!(!hidden.feature.names_waiting());

    hidden.feature.reset();
    hidden.feature.want_name(t1, RunId::new("run-a").unwrap());
    let reads = run_reads(&hidden.maintain());
    assert!(
        reads.iter().any(|(_, run_id)| run_id == "run-a"),
        "a new session reads it again: {reads:?}"
    );
}

// Given: a listing whose live run reports a model, and one whose run has none
// When: the listing is read
// Then: the model is known for the first run only
#[test]
fn the_model_of_a_listed_live_run_is_kept() {
    let mut screen = rooted();
    let first = asked(&mut screen);
    let mut live = run("run-a", Some("root"), "active", "live", 0);
    live["model"] = json!("work/gpt-5");
    answer(
        &mut screen,
        runs_tag(&first),
        runs_reply(
            vec![live, run("run-b", Some("root"), "active", "live", 0)],
            false,
        ),
    );
    let (run_a, run_b) = (RunId::new("run-a").unwrap(), RunId::new("run-b").unwrap());
    let execution = screen.state_mut().feature.as_execution_mut().unwrap();
    assert_eq!(execution.run_model(&run_a), Some("work/gpt-5"));
    assert_eq!(execution.run_model(&run_b), None);
}
