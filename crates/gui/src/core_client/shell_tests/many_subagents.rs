//! A window opened on a thread whose root already has more subagents running than a
//! page lists, drawn as the core-client e2e draws it: a 1024×768 window over a client
//! queue of 64, every request answered between two frames.
use super::*;
use crate::core_client::QUEUE_FULL;
use workbench_protocol::namespaces::{ExecutionQuery, execution::RunScope, teams::TeamsQuery};

const COUNT: usize = workbench_protocol::limits::DEFAULT_PAGE_SIZE as usize + 1;
const OUTBOX: usize = 64;

fn subagent(index: usize) -> RunSummary {
    view(json!({
        "id": format!("run-1-{index:03}"), "thread": "thr-1", "parent_run": "run-1",
        "role": "worker", "phase": "running", "revision": "4",
    }))
}

fn detail(index: usize) -> serde_json::Value {
    json!({"summary": serde_json::to_value(subagent(index)).unwrap(),
        "name": format!("kid {index}"), "lifecycle": "active", "source": "live",
        "child_count": 0, "revision": "4"})
}

/// [`detail`] as the listing gives it: without the name, so every tab's name comes from
/// a run read of its own and all of them go through the bound on name reads.
fn listed(index: usize) -> serde_json::Value {
    let mut detail = detail(index);
    detail["name"] = json!("");
    detail
}

/// What the core answers: a page of run-1's subagents (unnamed), each one by itself, the goal with
/// eight criteria, and a team board with a page of tasks.
fn answer_query(query: &Query) -> Option<QueryReply> {
    let execution = |payload| view(json!({"type": "execution", "payload": payload}));
    let teams = |payload| view(json!({"type": "teams", "payload": payload}));
    Some(match query {
        Query::Workspace(query) => QueryReply::Workspace(answer(query)),
        Query::Execution(ExecutionQuery::ListRunDetails(list)) => {
            let items: Vec<_> = match &list.scope {
                RunScope::Children { parent } if parent.as_str() == "run-1" => {
                    (0..COUNT - 1).map(listed).collect()
                }
                _ => Vec::new(),
            };
            execution(json!({"type": "run_details",
                "payload": {"items": items, "snapshot_revision": "1"}}))
        }
        Query::Execution(ExecutionQuery::GetRun(target)) => {
            let index = target.run.as_str()["run-1-".len()..].parse().unwrap();
            execution(json!({"type": "run", "payload": detail(index)}))
        }
        Query::Execution(ExecutionQuery::GetThreadExecution(_)) => {
            execution(json!({"type": "thread_execution", "payload": {
            "thread": "thr-1", "root_run": "run-1",
            "goal": {"goal": "g1", "root_run": "run-1", "objective": "ship it",
                "criteria": (0..8).map(|n| format!("criterion {n}")).collect::<Vec<_>>(),
                "checks": [], "phase": "working", "review_enabled": false,
                "checks_paused": false, "work_stopped": false, "review_round": 1,
                "max_review_rounds": 3, "findings": [], "epoch": "1"}}}))
        }
        Query::Teams(TeamsQuery::GetTeam(_)) => teams(json!({"type": "team", "payload": {
            "thread": "thr-1", "source": "persisted", "ready": COUNT, "claimed": 0,
            "complete": 0, "members": [], "revision": "1"}})),
        Query::Teams(TeamsQuery::ListTeamTasks(_)) => {
            let items: Vec<_> = (0..COUNT - 1)
                .map(|n| {
                    json!({"task_id": format!("task-{n:03}"), "state": "ready",
                    "paths": [format!("src/file-{n}.rs")], "claim_generation": "0",
                    "revision": "1"})
                })
                .collect();
            teams(json!({"type": "tasks", "payload": {"items": items, "snapshot_revision": "1"}}))
        }
        _ => return None,
    })
}

impl Screen {
    /// Two frames, then every query asked is answered, as the e2e's connection does.
    /// Returns how many requests the frames left in the client's queue.
    fn flush_answering(&mut self) -> usize {
        self.harness.run_steps(2);
        let mut queued = 0;
        while let Ok(action) = self.actions.try_recv() {
            queued += 1;
            if let Action::Feature(FeatureAction {
                feature,
                tag,
                request: FeatureRequest::Query(query),
            }) = action
                && let Some(reply) = answer_query(&query)
            {
                let reply = FeatureReply::Query(Ok(reply));
                self.shared
                    .lock()
                    .unwrap()
                    .features
                    .reply(feature, tag, reply);
            }
        }
        queued
    }

    /// Flushes until `done`, each time checking that the client's queue kept room. The
    /// bound is on flushes, not on time: enough for every subagent's name to be read a
    /// few at a time.
    fn flush_until(&mut self, what: &str, done: impl Fn(&Screen) -> bool) {
        for _ in 0..COUNT {
            let queued = self.flush_answering();
            assert!(
                queued < OUTBOX,
                "{queued} requests filled the client's queue"
            );
            let status = self.shared.lock().unwrap().status.clone();
            assert_ne!(status, QUEUE_FULL, "a request was refused");
            if done(self) {
                return;
            }
        }
        panic!("{what} was not shown");
    }

    /// The subagent tabs not titled with their run's name yet.
    fn untitled_run_tabs(&self) -> Vec<String> {
        let state = self.harness.state();
        state
            .dock
            .iter_all_tabs()
            .filter_map(|(_, tab)| match tab {
                CoreTab::Run(id) => state.run_tabs.get(*id),
                _ => None,
            })
            .filter(|tab| {
                let index: usize = tab.run.as_str()["run-1-".len()..].parse().unwrap();
                tab.title != format!("Transcript: kid {index}")
            })
            .map(|tab| tab.title.clone())
            .collect()
    }

    fn in_window(&self, label: &str) -> bool {
        let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1024.0, 768.0));
        let mut nodes = self.harness.query_all_by_label(label).peekable();
        nodes.peek().is_some() && nodes.all(|node| window.contains(node.rect().center()))
    }
}

// Given: thr-1's root run-1 has a page and one more of subagents running when the window
//        opens on it, a goal with eight criteria and a team board with a page of tasks
// When: the window is drawn and answered frame by frame, the goal's details are opened
//       and folded, and the Teams tab is pressed near its start
// Then: every subagent opens below the Subagents tab (past the split bound as tabs of the
//       last split), its title counts them and every subagent tab comes to be titled
//       with its run's name, the client's queue never fills nor refuses
//       a request, the goal's criteria show, Teams comes forward although the wider title
//       pushes its centre past the tab bar's edge and lists its tasks, and the
//       conversation's Stop, Message and Queue stay in the window
#[test]
fn a_window_opened_on_many_running_subagents_opens_them_all_and_keeps_its_areas_usable() {
    let local = crate::core_client::local::LocalPanes::new(
        Arc::new(crate::diff::FixtureDiffSource::empty()),
        None,
    );
    let mut screen = screen_from_outbox(egui::vec2(1024.0, 768.0), local, OUTBOX, |shared| {
        for index in 0..COUNT {
            let run = subagent(index);
            shared.cache.runs.insert(run.id.as_str().into(), run);
        }
    });
    screen.flush_until("the goal", |s| {
        s.harness.query_by_label("Show goal details").is_some()
    });
    let run_tabs = screen
        .harness
        .state()
        .dock
        .iter_all_tabs()
        .filter(|(_, tab)| matches!(tab, CoreTab::Run(_)))
        .count();
    assert_eq!(run_tabs, COUNT);
    screen
        .harness
        .get_by_label(&format!("Subagents({COUNT}) tab"));
    screen.flush_until("every subagent's name", |s| {
        s.untitled_run_tabs().is_empty()
    });

    screen.harness.get_by_label("Show goal details").click();
    screen.flush_until("the last criterion", |s| {
        s.harness.query_by_label_contains("criterion 7").is_some()
    });
    screen.harness.get_by_label("Hide goal details").click();

    let teams = CoreTab::Feature(FeatureId::Teams);
    let rect = screen.harness.get_by_label("Teams tab").rect();
    let pos = egui::pos2(rect.left() + 10.0, rect.center().y);
    screen.harness.hover_at(pos);
    for pressed in [true, false] {
        screen.harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
    }
    screen.flush_until("the first task", |s| {
        s.harness.query_by_label("task-000").is_some()
    });
    assert_eq!(screen.front_of_leaf(teams), teams);
    for control in ["Stop", "Message", "Queue"] {
        assert!(screen.in_window(control), "{control} is out of the window");
    }
    screen.capture("many-subagents");
}
