use super::*;
use workbench_protocol::{
    ids::OperationId,
    namespaces::{
        diagnostics::ReadOutcome,
        evaluation::{EvaluationQuery, EvaluationReply},
        improvement::{ImprovementQuery, ImprovementReply},
        memory::{MemoryQuery, MemoryReply},
        teams::TeamsQuery,
    },
    operation::{OperationState, OperationView},
};

fn ready<T>(id: &OperationId, data: serde_json::Value) -> Box<ReadOutcome<T>>
where
    T: serde::de::DeserializeOwned,
{
    Box::new(ReadOutcome {
        operation: OperationView {
            operation_id: id.clone(),
            state: OperationState::Ready,
            revision: Revision(1),
            result: None,
        },
        data: Some(view(data)),
    })
}

fn page(items: Vec<serde_json::Value>) -> serde_json::Value {
    json!({"items": items, "snapshot_revision": "1"})
}

fn allowed(action: &str) -> serde_json::Value {
    json!({"action": action, "state": {"type": "allowed"}})
}

fn denied(action: &str, reason: &str) -> serde_json::Value {
    json!({"action": action, "state": {"type": "denied", "reason": reason}})
}

fn lesson(id: &str, status: &str, favorite: bool, scope: &str, preview: &str) -> serde_json::Value {
    let actions = if favorite {
        vec![
            allowed("clear_favorite"),
            denied("reject", "favorite_must_be_cleared"),
        ]
    } else {
        vec![
            allowed("set_favorite"),
            allowed("validate"),
            allowed("promote"),
            allowed("reject"),
        ]
    };
    json!({"state": {"id": id, "status": status, "favorite": favorite, "revision": "1",
        "actions": actions}, "scope": scope, "preview": preview})
}

fn text(text: &str) -> serde_json::Value {
    json!({"text": text, "total_bytes": text.len(), "truncated": false})
}

fn answer_memory(query: &MemoryQuery) -> QueryReply {
    let lessons = [
        lesson(
            "lesson-redirect",
            "promoted",
            true,
            "project",
            "Check `next` before redirecting after login.",
        ),
        lesson(
            "lesson-fmt",
            "validated",
            false,
            "harness",
            "Run cargo fmt before committing.",
        ),
        lesson(
            "lesson-tests",
            "candidate",
            false,
            "project",
            "Run the auth suite after touching the session store.",
        ),
    ];
    let reply = match query {
        MemoryQuery::ListMemory(list) => {
            MemoryReply::List(ready(&list.operation_id, page(lessons.to_vec())))
        }
        MemoryQuery::GetMemory(get) => {
            let mut detail = lessons
                .iter()
                .find(|lesson| lesson["state"]["id"] == get.id.as_str())
                .expect("a listed lesson")
                .clone();
            let object = detail.as_object_mut().unwrap();
            object.remove("preview");
            object.insert("task_id".into(), json!("task-42"));
            object.insert(
                "content".into(),
                text(
                    "Run the auth suite after touching the session store: the redirect \
                      tests read the session cookie the store writes.",
                ),
            );
            object.insert(
                "evidence".into(),
                text("thr-4 broke the redirect tests twice after editing session.rs."),
            );
            MemoryReply::Detail(ready(&get.operation_id, detail))
        }
        other => panic!("the memory test does not answer {other:?}"),
    };
    QueryReply::Memory(reply)
}

fn candidate(id: &str, status: &str, severity: &str, title: &str, seen: u32) -> serde_json::Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    json!({"state": {"id": id, "status": status, "revision": "1",
            "actions": [allowed("mark_reviewed"), allowed("dismiss"), allowed("triage")]},
        "source": "diagnostic", "severity": severity, "code": "tool_failure",
        "title": title, "occurrences": seen, "last_seen_ms": now - 3_600_000})
}

fn answer_improvement(query: &ImprovementQuery) -> QueryReply {
    let candidates = [
        candidate(
            "cand-shell",
            "new",
            "warning",
            "The shell tool exited 2 on cargo test",
            3,
        ),
        candidate(
            "cand-loop",
            "new",
            "error",
            "Identical tool calls repeated five times",
            1,
        ),
        candidate(
            "cand-lag",
            "reviewed",
            "info",
            "The observer lagged behind the event bus",
            2,
        ),
    ];
    let reply = match query {
        ImprovementQuery::ListImprovement(list) => ImprovementReply::List(ready(
            &list.operation_id,
            json!({"enabled": true, "candidates": page(candidates.to_vec())}),
        )),
        ImprovementQuery::GetImprovement(get) => {
            let summary = candidates
                .iter()
                .find(|c| c["state"]["id"] == get.candidate.as_str())
                .expect("a listed candidate")
                .clone();
            let created = summary["last_seen_ms"].as_u64().unwrap() - 86_400_000;
            ImprovementReply::Detail(ready(
                &get.operation_id,
                json!({"summary": summary,
                    "evidence": text("$ cargo test -p auth\nerror: could not compile `auth`"),
                    "draft_file": "cand-shell.md", "run": "run-1",
                    "recent_runs": ["run-1", "run-6"], "created_ms": created,
                    "reopened_count": 1}),
            ))
        }
        other => panic!("the improvement test does not answer {other:?}"),
    };
    QueryReply::Improvement(reply)
}

fn trials(planned: u32, finished: u32, running: u32, failed: u32) -> serde_json::Value {
    json!({"planned": planned, "queued": planned - finished - running - failed,
        "running": running, "finished": finished, "failed": failed, "interrupted": 0,
        "unavailable": 0, "unverified": 0})
}

fn member(
    name: &str,
    index: u32,
    thread: &str,
    phase: &str,
    trials: serde_json::Value,
    report: &str,
) -> serde_json::Value {
    let artifact = |kind: &str, presence: &str| json!({"group": "eval-20261020", "member": name, "kind": kind, "presence": presence});
    json!({"member": name, "index": index, "thread_id": thread, "phase": phase,
        "trials": trials, "report": artifact("report", report),
        "replay": artifact("replay", report), "problems": 0})
}

fn answer_evaluation(query: &EvaluationQuery) -> QueryReply {
    let members = vec![
        member(
            "baseline",
            0,
            "thr-11",
            "finished",
            trials(4, 4, 0, 0),
            "present",
        ),
        member(
            "candidate",
            1,
            "thr-12",
            "running_trials",
            trials(4, 2, 1, 1),
            "missing",
        ),
    ];
    let reply = match query {
        EvaluationQuery::ListEvaluations(list) => {
            let group = |id: &str, phase: &str, started: &str| {
                json!({"group": id, "presence": "present", "phase": phase,
                    "started_at": started, "members_total": 2, "revision": "1"})
            };
            EvaluationReply::List(ready(
                &list.operation_id,
                json!({"groups": page(vec![
                    group("eval-20261020", "running", "2026-10-20 09:12"),
                    group("eval-20261018", "finished", "2026-10-18 14:03"),
                    group("eval-20261015", "unverified", "2026-10-15 11:40"),
                    json!({"group": "eval-20261012", "presence": "missing", "revision": "1"}),
                ]), "omitted": 0, "scan_truncated": false}),
            ))
        }
        EvaluationQuery::GetEvaluation(get) => EvaluationReply::Group(ready(
            &get.operation_id,
            json!({"group": get.group, "phase": "running", "started_at": "2026-10-20 09:12",
                "candidate": text("model = \"candidate\"\nmax_turns = 40"),
                "members": members, "members_omitted": 0, "members_total": 2,
                "revision": "1"}),
        )),
        other => panic!("the evaluation test does not answer {other:?}"),
    };
    QueryReply::Evaluation(reply)
}

fn answer_teams(query: &TeamsQuery) -> QueryReply {
    let task = |id: &str, state: &str, owner: Option<&str>| {
        json!({"task_id": id, "state": state,
            "paths": ["src/auth/login.rs", "src/auth/session.rs"], "owner": owner,
            "owner_run": owner.map(|_| "run-6"), "claim_generation": "2", "revision": "1"})
    };
    let tasks = [
        task("task-redirect", "claimed", Some("worker-1")),
        task("task-tests", "claimed", Some("worker-2")),
        task("task-docs", "ready", None),
        task("task-audit", "complete", Some("worker-1")),
    ];
    let reply = match query {
        TeamsQuery::GetTeam(_) => view(json!({"type": "team", "payload": {
            "thread": "thr-1", "source": "live", "coordinator_run": "run-1",
            "ready": 1, "claimed": 2, "complete": 1,
            "members": [
                {"owner": "worker-1", "run": "run-6", "claimed_tasks": 1},
                {"owner": "worker-2", "claimed_tasks": 1}],
            "revision": "1"}})),
        TeamsQuery::ListTeamTasks(_) => view(json!({"type": "tasks", "payload":
            page(tasks.to_vec())})),
        TeamsQuery::GetTeamTask(get) => {
            let task = tasks
                .iter()
                .find(|task| task["task_id"] == get.task_id.as_str())
                .expect("a listed task")
                .clone();
            view(json!({"type": "task", "payload": task}))
        }
    };
    QueryReply::Teams(reply)
}

fn answer_feature(query: &Query) -> QueryReply {
    match query {
        Query::Memory(query) => answer_memory(query),
        Query::Improvement(query) => answer_improvement(query),
        Query::Evaluation(query) => answer_evaluation(query),
        Query::Teams(query) => answer_teams(query),
        other => panic!("the read panes test does not answer {other:?}"),
    }
}

impl Screen {
    fn read_in_front(&mut self, feature: FeatureId) {
        shell::focus(
            &mut self.harness.state_mut().dock,
            CoreTab::Feature(feature),
        );
        self.give_room(CoreTab::Feature(feature));
        self.settle_read(feature);
    }

    fn give_room(&mut self, tab: CoreTab) {
        let dock = &mut self.harness.state_mut().dock;
        let path = dock
            .find_tab(&tab)
            .expect("the tab is in the dock")
            .node_path();
        let Some(parent) = path.node.parent() else {
            return;
        };
        let top = path.node.is_left();
        if let egui_dock::Node::Vertical(split) =
            &mut dock[egui_dock::NodePath::new(path.surface, parent)]
        {
            split.fraction = if top { 0.85 } else { 0.15 };
        }
    }

    fn settle_read(&mut self, feature: FeatureId) {
        for _ in 0..12 {
            self.harness.run_steps(1);
            while let Ok(action) = self.actions.try_recv() {
                let Action::Feature(FeatureAction {
                    feature: asked,
                    tag,
                    request: FeatureRequest::Query(query),
                }) = action
                else {
                    continue;
                };
                let reply = match &query {
                    Query::Workspace(query) => QueryReply::Workspace(answer(query)),
                    _ if asked == feature => answer_feature(&query),
                    _ => continue,
                };
                let mut shared = self.shared.lock().unwrap();
                shared
                    .features
                    .reply(asked, tag, FeatureReply::Query(Ok(reply)));
            }
        }
        self.harness
            .event(egui::Event::PointerMoved(egui::pos2(1.0, 899.0)));
        self.harness.run_steps(2);
    }
}

// Given: proj-1 with a promoted approved lesson and two others
// When: the Memory tab reads them and a lesson is opened with its evidence
// Then: the toolbar, the rows and the opened lesson are worded as the legacy pane words them
#[test]
fn the_memory_tab_lists_and_opens_a_lesson_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.read_in_front(FeatureId::Memory);
    assert!(screen.harness.query_by_label("Refresh").is_none());
    for label in ["3 lessons", "lesson-redirect", "promoted", "Approved"] {
        screen.harness.get_by_label(label);
    }
    screen.harness.get_by_label("lesson-tests").click();
    screen.settle_read(FeatureId::Memory);
    screen.harness.get_by_label("Evidence").click();
    screen.settle_read(FeatureId::Memory);
    screen.harness.get_by_label("task-42");
    // The conversation's approval card beside the tab has its own Approve and Reject.
    for label in ["Approve", "Reject"] {
        assert_eq!(
            screen.harness.query_all_by_label(label).count(),
            2,
            "{label}"
        );
    }
    for label in ["Validate", "Promote"] {
        screen.harness.get_by_label(label);
    }
    screen.capture("memory");
}

// Given: proj-1 with three improvement candidates
// When: the Self-improvement drafts tab reads them and one is opened with its evidence
// Then: the status marks, badges, seen lines and the opened candidate are drawn as the
//       legacy pane draws them
#[test]
fn the_improvement_tab_lists_and_opens_a_candidate_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.read_in_front(FeatureId::Improvement);
    screen.harness.get_by_label("3 candidates");
    screen.harness.get_by_label("cand-shell").click();
    screen.settle_read(FeatureId::Improvement);
    screen.harness.get_by_label("Evidence").click();
    screen.settle_read(FeatureId::Improvement);
    for label in [
        "Reopened ×1",
        "Copy run ID",
        "Draft: cand-shell.md",
        "Mark reviewed",
        "Dismiss",
        "Create draft",
    ] {
        screen.harness.get_by_label(label);
    }
    screen.capture("improvement");
}

// Given: four evaluation groups, the newest running with two threads
// When: the Evaluations tab opens the running group
// Then: its badge, candidate configuration and threads are drawn as the legacy page
#[test]
fn the_evaluations_tab_opens_a_group_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.read_in_front(FeatureId::Evaluation);
    screen.harness.get_by_label("record missing");
    screen.harness.get_by_label("Unverified");
    screen.capture("evaluation-groups");
    screen.harness.get_by_label("eval-20261020").click();
    screen.settle_read(FeatureId::Evaluation);
    screen
        .harness
        .get_by_label("Candidate configuration")
        .click();
    screen.settle_read(FeatureId::Evaluation);
    for label in ["Groups", "Threads", "baseline · thr-11", "Running trials"] {
        screen.harness.get_by_label(label);
    }
    screen.capture("evaluation");
}

// Given: thr-1 runs a live team of two workers
// When: the Teams tab reads the board and a claimed task is opened
// Then: the board, the task rows and the task card are drawn as the legacy Team pane
#[test]
fn the_teams_tab_shows_the_board_and_opens_a_task_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.read_in_front(FeatureId::Teams);
    for label in ["Team", "Live board", "1 ready · 2 claimed · 1 complete"] {
        screen.harness.get_by_label(label);
    }
    screen.harness.get_by_label("task-redirect").click();
    screen.settle_read(FeatureId::Teams);
    screen.harness.get_by_label("Task task-redirect");
    screen.harness.get_by_label("Owner: worker-1");
    screen.capture("teams");
}
