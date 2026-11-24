use std::sync::{Arc, Mutex};

use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use workbench_protocol::{
    events::EventCursor,
    handshake::Capability,
    ids::{ProjectId, RunId, ThreadId},
    namespaces::{
        WorkspaceQuery, WorkspaceReply,
        workspace::{FamilyView, LineageType, ProjectAccessView, ThreadDetail, TrustLevel},
    },
    queries::{Query, QueryReply},
    views::{
        ApprovalView, CommittedMessage, Page, ProjectSummary, QuestionView, RunPhase, RunSummary,
        ThreadSummary,
    },
    wire::{InstanceEpoch, Revision, Sequence},
};

use super::connection::Action;
use super::features::{FeatureAction, FeatureId, FeatureReply, FeatureRequest};
use super::shell::{self, CoreTab};
use super::{CoreClientScreen, Shared};

fn view<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn message(index: u64, role: &str, text: &str) -> CommittedMessage {
    view(json!({
        "thread": "thr-1",
        "run": "run-1",
        "index": index.to_string(),
        "role": role,
        "content": {"type": "inline", "payload": {"text": text}},
    }))
}

const PROJECTS: [(&str, &str, &str); 2] = [
    ("proj-1", "evorch", "/work/evorch"),
    ("proj-2", "website", "/work/website"),
];

struct Fixture {
    id: &'static str,
    project: &'static str,
    title: &'static str,
    state: &'static str,
    lead: Option<&'static str>,
    parent: Option<(&'static str, LineageType)>,
    family: &'static str,
    root_run: Option<&'static str>,
    pinned: bool,
    archived: bool,
    updated: u64,
}

const fn thread(id: &'static str, title: &'static str, updated: u64) -> Fixture {
    Fixture {
        id,
        project: "proj-1",
        title,
        state: "idle",
        lead: None,
        parent: None,
        family: id,
        root_run: None,
        pinned: false,
        archived: false,
        updated,
    }
}

fn threads() -> Vec<Fixture> {
    vec![
        Fixture {
            state: "waiting_for_approval",
            root_run: Some("run-1"),
            ..thread("thr-1", "Fix login redirect", 1_793_799_134_000)
        },
        Fixture {
            pinned: true,
            ..thread("thr-2", "Refactor storage writer", 1_793_799_000_000)
        },
        Fixture {
            parent: Some(("thr-1", LineageType::Fork)),
            family: "thr-1",
            ..thread("thr-3", "Try a session-scoped redirect", 1_793_799_120_000)
        },
        Fixture {
            lead: Some("thr-1"),
            parent: Some(("thr-1", LineageType::Fork)),
            family: "thr-1",
            ..thread("thr-4", "Auth regression tests", 1_793_799_110_000)
        },
        Fixture {
            archived: true,
            ..thread("thr-5", "Old storage spike", 1_793_700_000_000)
        },
        Fixture {
            state: "running",
            root_run: Some("run-6"),
            ..thread("thr-6", "Rebuild the search index", 1_793_799_050_000)
        },
        Fixture {
            root_run: Some("run-7"),
            ..thread("thr-7", "Migrate the lane table", 1_793_799_040_000)
        },
        Fixture {
            project: "proj-2",
            ..thread("thr-8", "Landing page copy", 1_793_799_030_000)
        },
    ]
}

fn detail(fixture: &Fixture) -> ThreadDetail {
    let id = |id: &str| ThreadId::new(id).unwrap();
    ThreadDetail {
        thread: id(fixture.id),
        project: ProjectId::new(fixture.project).unwrap(),
        title: fixture.title.into(),
        family: Some(id(fixture.family)),
        parent_thread: fixture.parent.as_ref().map(|(parent, _)| id(parent)),
        lineage: fixture.parent.as_ref().map(|(_, lineage)| *lineage),
        root_run: fixture.root_run.map(|run| RunId::new(run).unwrap()),
        active_root: None,
        model_profile: None,
        chat_role: None,
        extra_read_dirs: Vec::new(),
        archived: fixture.archived,
        pinned: fixture.pinned,
        revision: Revision(3),
    }
}

fn project(id: &str, name: &str) -> ProjectSummary {
    view(json!({"id": id, "display_name": name, "thread_count": 1, "revision": "1"}))
}

fn answer(query: &WorkspaceQuery) -> WorkspaceReply {
    fn page<T>(items: Vec<T>) -> Page<T> {
        Page {
            items,
            next_cursor: None,
            snapshot_revision: Revision(1),
        }
    }
    match query {
        WorkspaceQuery::ListFamilies(_) => {
            let fixtures = threads();
            let families = fixtures.iter().filter(|t| t.family == t.id).map(|root| {
                let members = fixtures.iter().filter(|t| t.family == root.id).count();
                FamilyView {
                    key: ThreadId::new(root.id).unwrap(),
                    membership_epoch: 0,
                    members: members as u32,
                    archived: root.archived,
                    archived_at: None,
                    archive_revision: Revision(1),
                    pinned: root.pinned,
                    pin_revision: Revision(1),
                }
            });
            WorkspaceReply::Families(page(families.collect()))
        }
        WorkspaceQuery::ListProjects(_) => WorkspaceReply::Projects(page(
            PROJECTS
                .iter()
                .map(|(id, name, _)| project(id, name))
                .collect(),
        )),
        WorkspaceQuery::GetThreadDetail(target) => {
            let fixture = threads()
                .into_iter()
                .find(|t| t.id == target.thread.as_str())
                .expect("a fixture thread");
            WorkspaceReply::ThreadDetail(Box::new(detail(&fixture)))
        }
        WorkspaceQuery::GetProjectAccess(target) => {
            let (id, name, root) = PROJECTS
                .iter()
                .find(|(id, ..)| *id == target.project.as_str())
                .expect("a fixture project");
            WorkspaceReply::ProjectAccess(Box::new(ProjectAccessView {
                project: ProjectId::new(*id).unwrap(),
                name: (*name).into(),
                root: (*root).into(),
                trust: TrustLevel::Approved,
                allowed_directories: Vec::new(),
                revision: Revision(1),
            }))
        }
        other => panic!("the shell test does not answer {other:?}"),
    }
}

fn populated() -> Shared {
    let mut shared = Shared {
        connected: true,
        status: "Connected".into(),
        capabilities: Capability::IMPLEMENTED.to_vec(),
        ..Shared::default()
    };
    let cache = &mut shared.cache;
    for (id, name, _) in PROJECTS {
        cache.projects.insert(id.into(), project(id, name));
    }
    for fixture in threads() {
        let thread: ThreadSummary = view(json!({
            "id": fixture.id, "project": fixture.project, "title": fixture.title,
            "state": fixture.state, "lead_thread": fixture.lead,
            "revision": "3", "updated_at": fixture.updated.to_string(),
        }));
        cache.threads.insert(fixture.id.into(), thread);
    }
    for (id, thread, phase) in [
        ("run-1", "thr-1", "waiting_for_approval"),
        ("run-6", "thr-6", "running"),
        ("run-7", "thr-7", "failed"),
    ] {
        let run: RunSummary = view(json!({
            "id": id, "thread": thread, "role": "orchestrator",
            "phase": phase, "revision": "4",
        }));
        cache.runs.insert(id.into(), run);
    }
    cache.history.insert(
        "thr-1".into(),
        vec![
            message(0, "user", "The login page redirects to /home even when `next` is set. Fix it."),
            message(
                1,
                "assistant",
                "I'll look at the redirect handler first.\n\n- `src/auth/login.rs`\n- `src/auth/session.rs`",
            ),
            message(2, "tool", "exit code: 0\nsrc/auth/login.rs:42: redirect_to(\"/home\")"),
            message(3, "tool", "--- a/src/auth/login.rs\n+++ b/src/auth/login.rs\n-redirect_to(\"/home\")\n+redirect_to(next.unwrap_or(\"/home\"))"),
            message(
                4,
                "assistant",
                "The handler ignored `next`. I changed it to honour the parameter and will run the tests.",
            ),
        ],
    );
    let question: QuestionView = view(json!({
        "id": "q-1",
        "requester": {"run": "run-1", "thread": "thr-1", "root_run": "run-1"},
        "target": {"type": "user"},
        "management_revision": "1", "routing_revision": "1", "blocking": true,
        "title": "Should an invalid `next` fall back to /home or to /?",
        "options": ["/home", "/"],
        "revision": "2",
        "raised_by": {"actor": "agent", "agent_run_id": "run-1", "surface": "none"},
    }));
    cache.questions.insert("q-1".into(), question);
    for (id, thread, run, summary) in [
        ("ap-1", "thr-1", "run-1", "cargo test -p auth"),
        (
            "ap-2",
            "thr-4",
            "run-4",
            "cargo test -p auth --test redirect",
        ),
    ] {
        let approval: ApprovalView = view(json!({
            "id": id, "core_epoch": "7", "revision": "2", "thread": thread, "run": run,
            "tool_name": "shell", "summary": summary, "summary_truncated": false,
            "input_bytes": "22", "state": "pending",
        }));
        cache.approvals.insert(id.into(), approval);
    }
    shared
}

struct Screen {
    harness: Harness<'static, CoreClientScreen>,
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Receiver<Action>,
}

fn screen(size: egui::Vec2) -> Screen {
    let (tx, actions) = tokio::sync::mpsc::channel(256);
    let shared = Arc::new(Mutex::new(populated()));
    let screen = CoreClientScreen::new(
        shared.clone(),
        tx,
        Some(ProjectId::new("proj-1").unwrap()),
        Some(ThreadId::new("thr-1").unwrap()),
    );
    let harness = Harness::builder()
        .with_size(size)
        .build_ui_state(|ui, screen: &mut CoreClientScreen| screen.ui(ui), screen);
    crate::theme::style::install_preset(&harness.ctx, crate::theme::style::ThemePreset::Graphite);
    Screen {
        harness,
        shared,
        actions,
    }
}

impl Screen {
    /// Answers the workspace queries from the fixture until settled; returns the other actions.
    fn settle(&mut self) -> Vec<Action> {
        let mut others = Vec::new();
        for _ in 0..16 {
            self.harness.run_steps(1);
            let mut answered = false;
            while let Ok(action) = self.actions.try_recv() {
                match action {
                    Action::Feature(FeatureAction {
                        feature: FeatureId::Workspace,
                        tag,
                        request: FeatureRequest::Query(Query::Workspace(query)),
                    }) => {
                        let reply = FeatureReply::Query(Ok(QueryReply::Workspace(answer(&query))));
                        let mut shared = self.shared.lock().unwrap();
                        shared.features.reply(FeatureId::Workspace, tag, reply);
                        answered = true;
                    }
                    other => others.push(other),
                }
            }
            let settled = self
                .shared
                .lock()
                .unwrap()
                .features
                .workspace()
                .is_some_and(|workspace| workspace.settled());
            if !answered && settled {
                return others;
            }
        }
        panic!("the workspace feature kept asking");
    }

    /// Saves the frame to `EVORCH_CORE_CLIENT_CAPTURE`, with `-{suffix}` on the stem.
    fn capture(&mut self, suffix: &str) {
        let Ok(path) = std::env::var("EVORCH_CORE_CLIENT_CAPTURE") else {
            return;
        };
        let mut path = std::path::PathBuf::from(path);
        if !suffix.is_empty() {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            path.set_file_name(format!("{stem}-{suffix}.png"));
        }
        self.harness.render().unwrap().save(path).unwrap();
    }
}

#[test]
fn shell_shows_the_selected_conversation_beside_the_area_tabs() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.harness.run_steps(4);
    let harness = &screen.harness;
    harness.get_by_label("Tool approval needed");
    harness.get_by_label("Question for you");
    harness.get_by_label("Message");
}

// Given: a core with two projects whose threads fork, run a lane, are pinned,
//        archived, running, failed or wait on the user
// When: the Projects tab has read what it needs, at a wide and a narrow window
// Then: the toolbar, both projects and each thread's tree position, status and
//       attention are shown as the embedded workbench shows them
#[test]
fn projects_tab_shows_the_toolbar_and_the_project_tree() {
    for (width, height, suffix) in [(1440.0, 900.0, ""), (1024.0, 768.0, "narrow")] {
        let mut screen = screen(egui::vec2(width, height));
        screen.settle();
        screen.capture(suffix);
        let harness = &screen.harness;
        for label in [
            "Add project",
            "Sort projects",
            "Collapse all projects",
            "Collapse threads of evorch",
            "Collapse threads of website",
            "New thread in evorch",
            "★",
            "Archived (1)",
            "Thread attention: Answer needed: this thread has an unanswered question",
            "Thread attention: Approval needed: a tool call is waiting",
            "Thread status: Failed",
            "Forked thread",
            "Lane thread",
            "↳ Try a session-scoped redirect",
            "↳ Auth regression tests",
            "Landing page copy",
        ] {
            harness.get_by_label(label);
        }
        assert_eq!(harness.get_all_by_label("Project settings").count(), 2);
        // A run waiting on an approval still runs, as in the embedded workbench.
        assert_eq!(
            harness.get_all_by_label("Thread status: Running").count(),
            2
        );
        // Every live root but the pinned one offers a pin; children do not.
        assert_eq!(harness.get_all_by_label("☆").count(), 4);
        assert!(harness.query_by_label("Old storage spike").is_none());
    }
}

// Given: the Projects tab showing thr-1
// When: another thread's title is clicked
// Then: the conversation shows that thread and its history is asked for
#[test]
fn a_thread_click_selects_it_and_loads_history() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen
        .harness
        .get_by_label("Refactor storage writer")
        .click();
    let actions = screen.settle();
    let thr_2 = ThreadId::new("thr-2").unwrap();
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::History(thread, _) if *thread == thr_2)),
        "no history request for thr-2"
    );
    assert_eq!(screen.harness.state().selected, Some(thr_2));
}

// Given: a core with a pending question on thr-1 and pending approvals on thr-1 and thr-4,
//        and the Browser tab in front of the conversation
// When: the window opens and the second approval's notification is clicked
// Then: the Notifications tab, in front at the bottom left, lists each once, and the
//       click brings the Conversation tab to front on thr-4 and asks for its history
#[test]
fn notifications_tab_lists_the_pending_question_and_approval() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    shell::focus(
        &mut screen.harness.state_mut().dock,
        CoreTab::Feature(FeatureId::Browser),
    );
    assert_ne!(front_of_conversation_leaf(&screen), CoreTab::Conversation);
    let harness = &screen.harness;
    harness.get_by_label("Notifications tab");
    harness.get_by_label("Lanes tab");
    harness.get_by_label("Question: Should an invalid `next` fall back to /home or to /?");
    assert_eq!(
        harness
            .get_all_by_label("Approval requested: shell")
            .count(),
        2
    );
    screen
        .harness
        .get_all_by_label("Approval requested: shell")
        .nth(1)
        .unwrap()
        .click();
    let actions = screen.settle();
    let thr_4 = ThreadId::new("thr-4").unwrap();
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::History(thread, _) if *thread == thr_4)),
        "no history request for thr-4"
    );
    assert_eq!(screen.harness.state().selected, Some(thr_4));
    assert_eq!(front_of_conversation_leaf(&screen), CoreTab::Conversation);
}

fn front_of_conversation_leaf(screen: &Screen) -> CoreTab {
    let dock = &screen.harness.state().dock;
    let path = dock
        .find_tab(&CoreTab::Conversation)
        .expect("the Conversation tab");
    let leaf = dock.leaf(path.node_path()).expect("a leaf");
    leaf.tabs[leaf.active.0]
}

// Given: the Notifications tab listing the pending question and approvals, unread
// When: thr-6's root run finishes and thr-1's fails, then the window gains focus
// Then: both outcomes are listed after the requests, and the rows drawn while the
//       window has focus become read
#[test]
fn notifications_tab_lists_root_run_outcomes_and_reads_them_when_focused() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    {
        let mut shared = screen.shared.lock().unwrap();
        for (run, phase) in [("run-6", RunPhase::Done), ("run-1", RunPhase::Failed)] {
            shared.cache.runs.get_mut(run).unwrap().phase = phase;
        }
    }
    screen.harness.run_steps(2);
    screen.capture("");
    let harness = &screen.harness;
    harness.get_by_label("Rebuild the search index completed");
    harness.get_by_label("Fix login redirect failed");
    assert_eq!(harness.get_all_by_label("Unread").count(), 5);

    screen
        .harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);
    screen.harness.run_steps(2);
    screen.capture("read");
    assert_eq!(screen.harness.state().notifications.unread_count(), 0);
    assert_eq!(screen.harness.get_all_by_label("Read").count(), 5);
}

// Given: the Notifications tab drawn once, then the window minimized
// When: the core restarts and thr-6's root run, running in the new snapshot, finishes,
//       with eframe running only logic passes; then the window is restored
// Then: the restored tab lists the outcome, as the log kept watching while minimized
#[test]
fn notifications_keep_watching_runs_while_the_window_is_minimized() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    let ctx = screen.harness.ctx.clone();
    let mut minimized = egui::RawInput::default();
    minimized
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .minimized = Some(true);
    let logic_pass = |screen: &mut Screen| {
        let before = ctx.cumulative_pass_nr();
        let _ = ctx.run_logic(&minimized, |ctx| screen.harness.state_mut().logic(ctx));
        assert_eq!(
            ctx.cumulative_pass_nr(),
            before,
            "a logic pass draws nothing"
        );
    };
    screen.shared.lock().unwrap().cache.at(EventCursor {
        core_epoch: InstanceEpoch(8),
        sequence: Sequence(0),
    });
    logic_pass(&mut screen);
    screen
        .shared
        .lock()
        .unwrap()
        .cache
        .runs
        .get_mut("run-6")
        .unwrap()
        .phase = RunPhase::Done;
    logic_pass(&mut screen);

    screen.harness.run_steps(2);
    screen
        .harness
        .get_by_label("Rebuild the search index completed");
}

mod progress_band {
    use super::*;
    use workbench_protocol::commands::Command;
    use workbench_protocol::namespaces::execution::plan::{
        PlanHistoryPage, PlanNodeView, PlanProgress, PlanStatusValues, PlanSummary, PlanTaskStatus,
        PlanTreePage,
    };
    use workbench_protocol::namespaces::{ExecutionCommand, ExecutionQuery, ExecutionReply};

    fn progress() -> PlanProgress {
        PlanProgress {
            node_count: 5,
            leaf_count: 4,
            leaf_counts: PlanStatusValues {
                completed: 2,
                blocked: 1,
                pending: 1,
                ..PlanStatusValues::default()
            },
            weighted_ppm: PlanStatusValues {
                completed: 500_000,
                pending: 500_000,
                ..PlanStatusValues::default()
            },
        }
    }

    fn tree() -> Vec<PlanNodeView> {
        let node = |id: &str, position, depth, title: &str, status| PlanNodeView {
            task_id: id.into(),
            parent_id: (depth > 0).then(|| "task-0".into()),
            position,
            depth,
            title: title.into(),
            weight: 1,
            status,
            child_count: if status.is_none() { 4 } else { 0 },
            progress_ppm: if status.is_none() {
                progress().weighted_ppm
            } else {
                PlanStatusValues::default()
            },
        };
        vec![
            node("task-0", 0, 0, "Fix the login redirect", None),
            node(
                "task-1",
                0,
                1,
                "Find the redirect handler",
                Some(PlanTaskStatus::Completed),
            ),
            node(
                "task-2",
                1,
                1,
                "Honour the next parameter",
                Some(PlanTaskStatus::Completed),
            ),
            node(
                "task-3",
                2,
                1,
                "Write the redirect test",
                Some(PlanTaskStatus::Blocked),
            ),
            node(
                "task-4",
                3,
                1,
                "Run the auth suite",
                Some(PlanTaskStatus::Pending),
            ),
        ]
    }

    fn reply(query: &ExecutionQuery) -> Option<ExecutionReply> {
        let thread = ThreadId::new("thr-1").unwrap();
        Some(match query {
            ExecutionQuery::GetThreadExecution(_) => {
                ExecutionReply::ThreadExecution(Box::new(view(json!({
                    "thread": "thr-1", "root_run": "run-1",
                    "goal": {"goal": "g1", "root_run": "run-1", "objective": "ship it",
                        "criteria": ["tests pass", "an invalid next falls back to /home"],
                        "checks": [{"criterion": 0, "met": true,
                            "evidence": "cargo test -p auth: 12 passed"}],
                        "phase": "working",
                        "review_enabled": false, "checks_paused": false,
                        "work_stopped": false, "review_round": 1, "max_review_rounds": 3,
                        "findings": ["the redirect test does not cover an empty next"],
                        "epoch": "1"},
                }))))
            }
            ExecutionQuery::GetConversationPlan(_) => {
                ExecutionReply::ConversationPlan(Box::new(PlanSummary {
                    thread,
                    plan_id: Some("plan-1".into()),
                    revision: Revision(3),
                    progress: progress(),
                    updated_at_unix_ms: None,
                }))
            }
            ExecutionQuery::ReadConversationPlanTree(_) => {
                ExecutionReply::ConversationPlanTree(Box::new(PlanTreePage {
                    plan_id: Some("plan-1".into()),
                    revision: Revision(3),
                    progress: progress(),
                    nodes: tree(),
                    next_cursor: None,
                }))
            }
            ExecutionQuery::ReadConversationPlanHistory(_) => {
                ExecutionReply::ConversationPlanHistory(Box::new(PlanHistoryPage {
                    plan_id: Some("plan-1".into()),
                    revision: Revision(3),
                    entries: Vec::new(),
                    next_after_revision: None,
                }))
            }
            _ => return None,
        })
    }

    impl Screen {
        /// Answers the execution feature's goal and plan reads from the
        /// fixture; returns what was asked for, answered or not.
        fn answer_execution(&mut self, actions: Vec<Action>) -> Vec<FeatureRequest> {
            let mut asked = Vec::new();
            for action in actions {
                let Action::Feature(FeatureAction {
                    feature: FeatureId::Execution,
                    tag,
                    request,
                }) = action
                else {
                    continue;
                };
                if let FeatureRequest::Query(Query::Execution(query)) = &request
                    && let Some(answer) = reply(query)
                {
                    let reply = FeatureReply::Query(Ok(QueryReply::Execution(answer)));
                    let mut shared = self.shared.lock().unwrap();
                    shared.features.reply(FeatureId::Execution, tag, reply);
                }
                asked.push(request);
            }
            self.harness.run_steps(2);
            asked
        }

        /// Settles the workspace, then answers the execution reads until none is left.
        fn settle_band(&mut self) -> Vec<FeatureRequest> {
            let mut asked = Vec::new();
            for _ in 0..8 {
                // What is pressed leaves in the execution feature's next pass.
                self.harness.run_steps(2);
                let actions = self.settle();
                if actions.is_empty() {
                    return asked;
                }
                asked.extend(self.answer_execution(actions));
            }
            panic!("the execution feature kept asking");
        }
    }

    fn teams_in_front() -> Screen {
        teams_in_front_at(egui::vec2(1440.0, 900.0))
    }

    fn teams_in_front_at(size: egui::Vec2) -> Screen {
        let mut screen = screen(size);
        shell::focus(
            &mut screen.harness.state_mut().dock,
            CoreTab::Feature(FeatureId::Teams),
        );
        screen.settle_band();
        screen
    }

    fn is_query(request: &FeatureRequest, wanted: fn(&ExecutionQuery) -> bool) -> bool {
        matches!(request, FeatureRequest::Query(Query::Execution(query)) if wanted(query))
    }

    // Given: thr-1 has a goal and a plan half done, and Teams is in front of Execution
    // When: the window is drawn
    // Then: the conversation's band above the composer shows the goal and the plan
    #[test]
    fn the_conversation_shows_the_goal_and_plan_band_while_teams_is_in_front() {
        let mut screen = teams_in_front();
        // A newer plan is read again though the Execution tab is not drawn.
        screen.shared.lock().unwrap().features.event(
            &workbench_protocol::events::UiEvent::Execution(
                workbench_protocol::namespaces::ExecutionEvent::ConversationPlanChanged {
                    thread: ThreadId::new("thr-1").unwrap(),
                    revision: Revision(4),
                },
            ),
        );
        screen.harness.run_steps(1);
        let asked = screen.settle_band();
        assert!(
            asked
                .iter()
                .any(|r| is_query(r, |q| matches!(q, ExecutionQuery::GetConversationPlan(_))))
        );
        assert!(
            !asked
                .iter()
                .any(|r| is_query(r, |q| matches!(q, ExecutionQuery::ListRunDetails(_))))
        );
        screen.capture("plan-band");
        let harness = &screen.harness;
        harness.get_by_label("Goal: ship it");
        harness.get_by_label("Plan 50% complete, 2 of 4 tasks completed, 1 blocked");
        harness.get_by_label("Pause goal checks");
        harness.get_by_label("Enable independent review before completion");
        harness.get_by_label("Message");
        assert!(harness.query_by_label("Plan tab").is_none());
    }

    // Given: the band shows thr-1's goal with a met check and a review finding,
    //        at a wide and a narrow window
    // When: Show goal details is pressed
    // Then: the details open inside the band, and the composer and the status
    //       strip below stay on screen
    #[test]
    fn goal_details_open_in_the_band_and_keep_the_composer_on_screen() {
        for (width, height, suffix) in [
            (1440.0, 900.0, "plan-band-details"),
            (1024.0, 768.0, "plan-band-details-narrow"),
        ] {
            let mut screen = teams_in_front_at(egui::vec2(width, height));
            screen.harness.get_by_label("Show goal details").click();
            screen.harness.run_steps(4);
            screen.capture(suffix);
            let harness = &screen.harness;
            harness.get_by_label("Hide goal details");
            harness.get_by_label("Independent review: disabled · round 1/3");
            harness.get_by_label("cargo test -p auth: 12 passed");
            harness.get_by_label("Review findings");
            assert!(
                harness
                    .query_by_label_contains("Original request")
                    .is_none()
            );
            let message = harness.get_by_label("Message").rect();
            let status = harness.get_by_label("Waiting for approval").rect();
            let goal = harness.get_by_label("Goal: ship it").rect();
            assert!(
                goal.max.y <= message.min.y,
                "{suffix}: {goal:?} over {message:?}"
            );
            let finding = harness
                .get_by_label("the redirect test does not cover an empty next")
                .rect();
            let composer_top =
                harness.get_by_label("Role: Worker").rect().min.y - crate::theme::tokens::SP_2;
            assert!(
                goal.max.y < finding.min.y && finding.max.y <= composer_top,
                "{suffix}: {finding:?} not between {goal:?} and the composer at {composer_top}"
            );
            assert!(
                message.max.y <= status.min.y && status.max.y <= height,
                "{suffix}: {message:?} and {status:?} in a {height} high window"
            );
        }
    }

    // Given: thr-1's goal details are open in the band
    // When: the session restarts, so the goal is read again and is missing
    //       until the answer arrives
    // Then: the details are open again once the goal is back
    #[test]
    fn goal_details_stay_open_across_a_reconnect() {
        let mut screen = teams_in_front();
        screen.harness.get_by_label("Show goal details").click();
        screen.harness.run_steps(2);
        screen.harness.get_by_label("Hide goal details");

        screen
            .shared
            .lock()
            .unwrap()
            .features
            .begin_session(&[], Vec::new());
        screen.harness.run_steps(2);
        assert!(screen.harness.query_by_label("Goal: ship it").is_none());

        screen.settle_band();
        screen.harness.get_by_label("Hide goal details");
        screen
            .harness
            .get_by_label("Independent review: disabled · round 1/3");
    }

    // Given: the band shows thr-1's plan
    // When: Open plan is pressed
    // Then: the Plan tab opens in front right after Execution in its leaf, and the
    //       plan's tree and history are read
    #[test]
    fn open_plan_opens_the_plan_tab_beside_execution_and_reads_the_tree() {
        let mut screen = teams_in_front();
        screen.harness.get_by_label("Open plan").click();
        let asked = screen.settle_band();
        assert!(asked.iter().any(|r| is_query(r, |q| matches!(
            q,
            ExecutionQuery::ReadConversationPlanTree(_)
        ))));
        assert!(asked.iter().any(|r| is_query(r, |q| matches!(
            q,
            ExecutionQuery::ReadConversationPlanHistory(_)
        ))));
        let dock = &screen.harness.state().dock;
        let path = dock.find_tab(&CoreTab::Plan).expect("the Plan tab");
        let leaf = dock.leaf(path.node_path()).unwrap();
        let execution = leaf
            .tabs
            .iter()
            .position(|tab| *tab == CoreTab::Feature(FeatureId::Execution))
            .expect("in Execution's leaf");
        assert_eq!(path.tab.0, execution + 1);
        assert_eq!(leaf.tabs[leaf.active.0], CoreTab::Plan);
        screen.capture("plan-tab");
        screen.harness.get_by_label("Plan tab");
        screen.harness.get_by_label("Fix the login redirect");
        screen.harness.get_by_label("Write the redirect test");
    }

    // Given: the band shows thr-1's goal with its checks running
    // When: Pause goal checks is pressed
    // Then: exactly one SetGoalChecksPaused for that goal is sent, nothing else
    #[test]
    fn pause_goal_checks_in_the_band_sends_set_goal_checks_paused() {
        let mut screen = teams_in_front();
        screen.harness.get_by_label("Pause goal checks").click();
        let asked = screen.settle_band();
        let commands: Vec<_> = asked
            .iter()
            .filter_map(|request| match request {
                FeatureRequest::Command(command) => Some(command),
                FeatureRequest::Query(_) => None,
            })
            .collect();
        let [Command::Execution(ExecutionCommand::SetGoalChecksPaused(paused))] = commands[..]
        else {
            panic!("one pause was expected, got {commands:?}");
        };
        assert_eq!(
            (paused.thread.as_str(), paused.goal.as_str(), paused.paused),
            ("thr-1", "g1", true)
        );
    }

    // Given: the Plan tab opened from the band
    // When: it is closed, then opened from ⚙ → Panels
    // Then: it returns beside Execution
    #[test]
    fn plan_tab_closes_and_reopens_from_panels() {
        let mut screen = teams_in_front();
        screen.harness.get_by_label("Open plan").click();
        screen.settle_band();
        let dock = &mut screen.harness.state_mut().dock;
        let path = dock.find_tab(&CoreTab::Plan).expect("the Plan tab");
        dock.remove_tab(path);
        screen.harness.run_steps(2);
        assert!(screen.harness.query_by_label("Plan tab").is_none());

        screen.harness.get_by_label("⚙").click();
        screen.harness.run_steps(2);
        screen.harness.get_by_label("Panels ⏵").click();
        screen.harness.run_steps(2);
        screen.harness.get_by_label("Plan").click();
        screen.settle_band();
        screen.harness.get_by_label("Plan tab");
        let dock = &screen.harness.state().dock;
        let plan = dock.find_tab(&CoreTab::Plan).unwrap();
        let execution = dock
            .find_tab(&CoreTab::Feature(FeatureId::Execution))
            .unwrap();
        assert_eq!(plan.node_path(), execution.node_path());
        assert_eq!(plan.tab.0, execution.tab.0 + 1);
    }
}
