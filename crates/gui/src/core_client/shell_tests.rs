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

/// The window at `size`, whose Diff never runs git and whose Terminal starts no shell.
fn screen(size: egui::Vec2) -> Screen {
    screen_with(
        size,
        super::local::LocalPanes::new(Arc::new(crate::diff::FixtureDiffSource::empty()), None),
    )
}

fn screen_with(size: egui::Vec2, local: super::local::LocalPanes) -> Screen {
    screen_from(size, local, |_| ())
}

/// [`screen_with`] with `patch` applied to the shared state before the first frame.
fn screen_from(
    size: egui::Vec2,
    local: super::local::LocalPanes,
    patch: impl FnOnce(&mut Shared),
) -> Screen {
    screen_from_outbox(size, local, 256, patch)
}

/// [`screen_from`] whose client queue holds `capacity` actions.
fn screen_from_outbox(
    size: egui::Vec2,
    local: super::local::LocalPanes,
    capacity: usize,
    patch: impl FnOnce(&mut Shared),
) -> Screen {
    let (tx, actions) = tokio::sync::mpsc::channel(capacity);
    let mut populated = populated();
    patch(&mut populated);
    let shared = Arc::new(Mutex::new(populated));
    let screen = CoreClientScreen::new(
        shared.clone(),
        tx,
        Some(ProjectId::new("proj-1").unwrap()),
        Some(ThreadId::new("thr-1").unwrap()),
    )
    .with_local_panes(local);
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
        self.settle_with(&|_, _| ())
    }

    /// [`Self::settle`], with each answer changed by `patch` first.
    fn settle_with(&mut self, patch: &dyn Fn(&WorkspaceQuery, &mut WorkspaceReply)) -> Vec<Action> {
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
                        let mut answer = answer(&query);
                        patch(&query, &mut answer);
                        let reply = FeatureReply::Query(Ok(QueryReply::Workspace(answer)));
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
            .any(|action| matches!(action, Action::History(thread, None, _) if *thread == thr_2)),
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
    // The read features keep the legacy panels' titles.
    harness.get_by_label("Memory tab");
    harness.get_by_label("Self-improvement drafts tab");
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
            .any(|action| matches!(action, Action::History(thread, None, _) if *thread == thr_4)),
        "no history request for thr-4"
    );
    assert_eq!(screen.harness.state().selected, Some(thr_4));
    assert_eq!(front_of_conversation_leaf(&screen), CoreTab::Conversation);

    // The toolbar opens the on-demand Evaluations tab under the legacy panel's title.
    screen.harness.get_by_label("Evaluations").click();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Evaluations tab");
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

mod run_tabs {
    use super::*;
    use workbench_protocol::commands::Command;
    use workbench_protocol::ids::PageCursor;
    use workbench_protocol::namespaces::{ExecutionCommand, ExecutionQuery, execution::RunScope};

    const CHILD_TEXT: &str = "You: Write the redirect test first";

    fn subagent(phase: &str) -> RunSummary {
        view(json!({
            "id": "run-1a", "thread": "thr-1", "parent_run": "run-1", "role": "worker",
            "phase": phase, "revision": "4",
        }))
    }

    /// thr-1's root run-1 has a running subagent run-1a, which committed a message. The
    /// window opens on it already running, which gives it a split of its own as the
    /// embedded workbench restores it; the person closed that tab.
    fn with_subagent() -> Screen {
        let local = super::super::local::LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            None,
        );
        let mut screen = screen_from(egui::vec2(1440.0, 900.0), local, |shared| {
            shared
                .cache
                .runs
                .insert("run-1a".into(), subagent("running"));
            let history = shared.cache.history.get_mut("thr-1").unwrap();
            history.push(child_message());
        });
        screen.settle_runs();
        let [split] = screen.run_tabs()[..] else {
            panic!("run-1a's split was expected, got {:?}", screen.run_tabs());
        };
        let dock = &mut screen.harness.state_mut().dock;
        let path = dock.find_tab(&split).unwrap();
        assert_eq!(dock.leaf(path.node_path()).unwrap().tabs, [split]);
        dock.remove_tab(path);
        screen.settle_runs();
        assert!(screen.run_tabs().is_empty());
        screen
    }

    fn child_message() -> CommittedMessage {
        view(json!({
            "thread": "thr-1", "run": "run-1a", "index": "0", "role": "user",
            "content": {"type": "inline", "payload": {"text": "Write the redirect test first"}},
        }))
    }

    fn run_detail(id: &str, parent: Option<&str>, name: &str, children: u32) -> serde_json::Value {
        json!({"summary": {"id": id, "thread": "thr-1", "parent_run": parent,
                "role": "worker", "phase": "running", "revision": "4"},
            "name": name, "lifecycle": "active", "source": "live",
            "child_count": children, "revision": "4"})
    }

    impl Screen {
        /// Settles the workspace and answers the execution feature's run listings of
        /// thr-1 until none is left; returns the other actions.
        fn settle_runs(&mut self) -> Vec<Action> {
            self.settle_runs_with(&[run_detail("run-1a", Some("run-1"), "redirect-tester", 0)])
        }

        /// [`Screen::settle_runs`] with `children` as run-1's subagents.
        pub(super) fn settle_runs_with(&mut self, children: &[serde_json::Value]) -> Vec<Action> {
            self.settle_runs_by(&|parent| {
                if parent == "run-1" {
                    children.to_vec()
                } else {
                    Vec::new()
                }
            })
        }

        /// [`Screen::settle_runs`] with `children_of(parent)` as the subagents of each
        /// run of thr-1.
        fn settle_runs_by(
            &mut self,
            children_of: &dyn Fn(&str) -> Vec<serde_json::Value>,
        ) -> Vec<Action> {
            let mut others = Vec::new();
            for _ in 0..8 {
                self.harness.run_steps(2);
                let mut answered = false;
                for action in self.settle() {
                    let Action::Feature(FeatureAction {
                        feature: FeatureId::Execution,
                        tag,
                        request:
                            FeatureRequest::Query(Query::Execution(ExecutionQuery::ListRunDetails(q))),
                    }) = action
                    else {
                        others.push(action);
                        continue;
                    };
                    let items = match (&q.scope, q.thread.as_str()) {
                        (RunScope::Children { parent }, "thr-1") => children_of(parent.as_str()),
                        _ => Vec::new(),
                    };
                    let reply = view(json!({"type": "execution", "payload": {
                        "type": "run_details",
                        "payload": {"items": items, "snapshot_revision": "1"}}}));
                    let mut shared = self.shared.lock().unwrap();
                    shared.features.reply(
                        FeatureId::Execution,
                        tag,
                        FeatureReply::Query(Ok(reply)),
                    );
                    answered = true;
                }
                if !answered {
                    return others;
                }
            }
            panic!("the execution feature kept asking");
        }

        /// Opens run-1a's tab from its Execution row; returns what was sent.
        fn open_subagent(&mut self) -> Vec<Action> {
            self.harness.get_by_label("Open pane").click();
            self.settle_runs()
        }

        fn run_tabs(&self) -> Vec<CoreTab> {
            self.harness
                .state()
                .dock
                .iter_all_tabs()
                .map(|(_, tab)| *tab)
                .filter(|tab| matches!(tab, CoreTab::Run(_)))
                .collect()
        }
    }

    // Given: thr-1's root and its subagent run-1a each committed a message
    // When: the window is drawn, then Open pane is pressed on run-1a's Execution row
    // Then: the Conversation shows only the root's message; the run tab opens beside
    //       Execution, asks for run-1a's history alone and shows its message
    #[test]
    fn child_messages_stay_out_of_the_conversation_and_show_in_their_run_tab() {
        let mut screen = with_subagent();
        screen.harness.get_by_label(
            "You: The login page redirects to /home even when `next` is set. Fix it.",
        );
        assert!(screen.harness.query_by_label(CHILD_TEXT).is_none());
        screen.capture("root-conversation");

        let sent = screen.open_subagent();
        let (thr_1, run_1a) = (
            ThreadId::new("thr-1").unwrap(),
            RunId::new("run-1a").unwrap(),
        );
        assert!(
            sent.iter().any(|action| matches!(
                action,
                Action::History(thread, Some(run), false) if *thread == thr_1 && *run == run_1a
            )),
            "no history request for run-1a"
        );
        let [tab] = screen.run_tabs()[..] else {
            panic!("one run tab was expected");
        };
        let dock = &screen.harness.state().dock;
        let path = dock.find_tab(&tab).unwrap();
        let leaf = dock.leaf(path.node_path()).unwrap();
        let execution = leaf
            .tabs
            .iter()
            .position(|tab| *tab == CoreTab::Feature(FeatureId::Execution))
            .expect("in Execution's leaf");
        assert_eq!(path.tab.0, execution + 1);
        assert_eq!(leaf.tabs[leaf.active.0], tab);
        screen
            .harness
            .get_by_label("Transcript: redirect-tester tab");
        screen
            .harness
            .get_by_label("No events for redirect-tester yet");

        let page = Page {
            items: vec![child_message()],
            next_cursor: None,
            snapshot_revision: Revision(1),
        };
        screen
            .shared
            .lock()
            .unwrap()
            .cache
            .run_history_page("run-1a", page, false);
        screen.harness.run_steps(2);
        screen.capture("run-tab");
        // Conversation and the run tab are both on screen; only the run tab has it.
        assert_eq!(screen.harness.get_all_by_label(CHILD_TEXT).count(), 1);
        screen.harness.get_by_label("Copy run ID");
    }

    // Given: thr-1's root run-1 has subagents that run, wait for an approval and are done
    // When: the Subagents tab is drawn, at a wide and a narrow window
    // Then: they are its first level, without a breadcrumb, and each subagent is a card of the
    //       legacy Subagents column: name, role and status, model and provider,
    //       Copy run ID and Open pane, and Stop only while it can still be stopped
    #[test]
    fn execution_draws_the_subagents_of_a_run_as_cards() {
        let detail = |id: &str, name: &str, phase: &str, lifecycle: &str, model: &str| {
            let mut run = run_detail(id, Some("run-1"), name, 0);
            run["summary"]["phase"] = json!(phase);
            run["lifecycle"] = json!(lifecycle);
            run["model"] = json!(model);
            run
        };
        let mut reviewer = detail(
            "run-1b",
            "auth-reviewer",
            "waiting_for_approval",
            "active",
            "anthropic/claude-sonnet",
        );
        reviewer["summary"]["role"] = json!("reviewer");
        let mut tester = detail(
            "run-1a",
            "redirect-tester",
            "running",
            "active",
            "openai/gpt-5",
        );
        tester["category"] = json!("quick");
        let children = [
            tester,
            reviewer,
            detail(
                "run-1c",
                "docs-writer",
                "done",
                "terminal",
                "local/worker-model",
            ),
        ];
        for (width, height, suffix) in [(1440.0, 900.0, ""), (1024.0, 768.0, "narrow")] {
            let mut screen = screen(egui::vec2(width, height));
            screen.settle_runs_with(&children);
            screen.harness.event(egui::Event::PointerGone);
            screen.harness.run_steps(2);
            screen.capture(suffix);
            let harness = &screen.harness;
            assert!(harness.query_by_label("Root").is_none());
            for label in [
                "redirect-tester",
                "Worker(quick) · Running",
                "openai/gpt-5 · openai",
                "auth-reviewer",
                "reviewer · Waiting for input",
                "docs-writer",
                "worker · Done",
                "local/worker-model · local",
            ] {
                harness.get_by_label(label);
            }
            assert_eq!(harness.get_all_by_label("Copy run ID").count(), 3);
            assert_eq!(harness.get_all_by_label("Open pane").count(), 3);
            // The finished subagent offers no Stop; the other two are the
            // conversation's, in its header and its composer.
            assert_eq!(harness.get_all_by_label("Stop").count(), 2 + 2);
        }
    }

    // Given: run-1a's tab open
    // When: it is closed
    // Then: its history is forgotten, and a later message of run-1a reaches only
    //       the thread's history
    #[test]
    fn closing_a_run_tab_forgets_its_history() {
        let mut screen = with_subagent();
        screen.open_subagent();
        assert!(
            screen
                .shared
                .lock()
                .unwrap()
                .cache
                .run_history
                .contains_key("run-1a")
        );
        let [tab] = screen.run_tabs()[..] else {
            panic!("one run tab was expected");
        };
        let dock = &mut screen.harness.state_mut().dock;
        let path = dock.find_tab(&tab).unwrap();
        dock.remove_tab(path);
        screen.harness.run_steps(2);

        let mut shared = screen.shared.lock().unwrap();
        assert!(!shared.cache.run_history.contains_key("run-1a"));
        shared.cache.at(EventCursor {
            core_epoch: InstanceEpoch(7),
            sequence: Sequence(0),
        });
        let mut message = child_message();
        message.index = view(json!("1"));
        let delivered = shared
            .cache
            .event(workbench_protocol::events::EventEnvelope {
                subscription_id: workbench_protocol::ids::SubscriptionId::new("sub").unwrap(),
                cursor: EventCursor {
                    core_epoch: InstanceEpoch(7),
                    sequence: Sequence(1),
                },
                caused_by_request_id: None,
                event: workbench_protocol::events::UiEvent::Conversation(
                    workbench_protocol::namespaces::ConversationEvent::MessageCommitted { message },
                ),
            });
        assert!(delivered);
        assert!(!shared.cache.run_history.contains_key("run-1a"));
        assert_eq!(shared.cache.history["thr-1"].len(), 7);
    }

    fn history_of_run_1a(sent: &[Action]) -> usize {
        sent.iter()
            .filter(|action| {
                matches!(action, Action::History(thread, Some(run), false)
                    if thread.as_str() == "thr-1" && run.as_str() == "run-1a")
            })
            .count()
    }

    // Given: run-1a's tab open
    // When: another thread is selected, then thr-1 again
    // Then: the tab leaves the dock and its history is forgotten; back on thr-1 the
    //       tab is beside Execution again, behind it, and reads run-1a's history again
    //       once it is brought to the front
    #[test]
    fn another_thread_hides_its_run_tabs_until_it_is_selected_again() {
        let mut screen = with_subagent();
        screen.open_subagent();
        screen
            .harness
            .get_by_label("Refactor storage writer")
            .click();
        screen.settle_runs();
        assert!(screen.run_tabs().is_empty());
        assert!(
            screen
                .harness
                .query_by_label("Transcript: redirect-tester tab")
                .is_none()
        );
        assert!(screen.shared.lock().unwrap().cache.run_history.is_empty());

        screen.harness.get_by_label("Fix login redirect").click();
        let sent = screen.settle_runs();
        assert_eq!(history_of_run_1a(&sent), 0, "a tab behind is not drawn");
        let [tab] = screen.run_tabs()[..] else {
            panic!("the run tab was expected back");
        };
        let dock = &screen.harness.state().dock;
        let path = dock.find_tab(&tab).unwrap();
        let leaf = dock.leaf(path.node_path()).unwrap();
        let execution = CoreTab::Feature(FeatureId::Execution);
        assert_eq!(leaf.tabs[path.tab.0 - 1], execution);
        assert_ne!(leaf.tabs[leaf.active.0], tab, "it comes back behind");
        screen
            .harness
            .get_by_label("Transcript: redirect-tester tab")
            .click();
        let sent = screen.settle_runs();
        assert_eq!(history_of_run_1a(&sent), 1);
    }

    // Given: run-1a's tab open with its history
    // When: a new session replaces the projection (a reconnect or resync), and run-1a
    //       commits a message afterwards
    // Then: the tab reads run-1a's history again and the message reaches it
    #[test]
    fn a_new_session_reads_the_history_of_open_run_tabs_again() {
        let mut screen = with_subagent();
        screen.open_subagent();
        {
            let mut shared = screen.shared.lock().unwrap();
            let mut cache = populated().cache;
            cache
                .runs
                .insert("run-1a".into(), shared.cache.runs["run-1a"].clone());
            cache.at(EventCursor {
                core_epoch: InstanceEpoch(8),
                sequence: Sequence(0),
            });
            shared.begin_session(cache, Capability::IMPLEMENTED.to_vec(), &[], Vec::new());
            assert!(!shared.cache.run_history.contains_key("run-1a"));
        }
        let sent = screen.settle_runs();
        assert_eq!(history_of_run_1a(&sent), 1);

        let mut shared = screen.shared.lock().unwrap();
        let delivered = shared
            .cache
            .event(workbench_protocol::events::EventEnvelope {
                subscription_id: workbench_protocol::ids::SubscriptionId::new("sub").unwrap(),
                cursor: EventCursor {
                    core_epoch: InstanceEpoch(8),
                    sequence: Sequence(1),
                },
                caused_by_request_id: None,
                event: workbench_protocol::events::UiEvent::Conversation(
                    workbench_protocol::namespaces::ConversationEvent::MessageCommitted {
                        message: child_message(),
                    },
                ),
            });
        assert!(delivered);
        assert_eq!(shared.cache.run_history["run-1a"].len(), 1);
    }

    fn new_session(screen: &mut Screen) {
        let mut shared = screen.shared.lock().unwrap();
        let mut cache = populated().cache;
        cache
            .runs
            .insert("run-1a".into(), shared.cache.runs["run-1a"].clone());
        shared.begin_session(cache, Capability::IMPLEMENTED.to_vec(), &[], Vec::new());
    }

    // Given: run-1a's tab shows a message whose text is a body, read once
    // When: a new session replaces the projection, and run-1a's history is read again
    // Then: the body is asked for again rather than left loading
    #[test]
    fn a_new_session_asks_for_the_bodies_of_a_run_tab_again() {
        use workbench_protocol::views::{BodyRef, MessageContent};
        use workbench_protocol::{ids::BodyId, wire::ByteOffset};
        let mut screen = with_subagent();
        screen.open_subagent();
        let body = BodyId::new("body-1").unwrap();
        let mut message = child_message();
        message.content = MessageContent::Body(BodyRef {
            body_id: body.clone(),
            total_bytes: ByteOffset(2048),
        });
        let page = || Page {
            items: vec![message.clone()],
            next_cursor: None,
            snapshot_revision: Revision(1),
        };
        let bodies = |sent: &[Action]| {
            sent.iter()
                .filter(|action| matches!(action, Action::Body(id) if *id == body))
                .count()
        };
        let set_page = |screen: &mut Screen| {
            let mut shared = screen.shared.lock().unwrap();
            shared.cache.run_history_page("run-1a", page(), false);
        };
        set_page(&mut screen);
        assert_eq!(bodies(&screen.settle_runs()), 1);
        {
            let mut shared = screen.shared.lock().unwrap();
            shared.cache.bodies.insert("body-1".into(), "Long".into());
        }
        assert_eq!(bodies(&screen.settle_runs()), 0);

        new_session(&mut screen);
        assert_eq!(history_of_run_1a(&screen.settle_runs()), 1);
        set_page(&mut screen);
        assert_eq!(bodies(&screen.settle_runs()), 1);
    }

    // Given: run-1a's tab open, and its first page could not be read
    // When: the window is drawn, and Retry is pressed
    // Then: the tab says so and does not ask again by itself; Retry asks once
    #[test]
    fn a_run_tab_whose_history_failed_offers_retry() {
        let mut screen = with_subagent();
        screen.open_subagent();
        screen.shared.lock().unwrap().cache.run_failed("run-1a");
        assert_eq!(history_of_run_1a(&screen.settle_runs()), 0);
        screen
            .harness
            .get_by_label("Could not read this run's history");
        screen.harness.get_by_label("Retry").click();
        assert_eq!(history_of_run_1a(&screen.settle_runs()), 1);
    }

    // Given: run-1a's tab out of the dock while another thread is selected
    // When: the layout is reset, and thr-1 is selected again
    // Then: the tab does not come back into the reset layout
    #[test]
    fn reset_layout_drops_the_run_tabs_of_other_threads() {
        let mut screen = with_subagent();
        screen.open_subagent();
        screen
            .harness
            .get_by_label("Refactor storage writer")
            .click();
        screen.settle_runs();
        screen.harness.get_by_label("⚙").click();
        screen.harness.run_steps(2);
        screen.harness.get_by_label("Reset layout").click();
        screen.settle_runs();

        screen.harness.get_by_label("Fix login redirect").click();
        let sent = screen.settle_runs();
        assert!(screen.run_tabs().is_empty());
        assert_eq!(history_of_run_1a(&sent), 0);
        assert!(screen.shared.lock().unwrap().cache.run_history.is_empty());
    }

    // Given: thr-1's newest page holds only run-1a's messages, older pages exist, and
    //        nothing waits on the person
    // When: the window is drawn, and older pages arrive
    // Then: the Conversation does not claim the thread is new, and the older pages are
    //       read one at a time until a root's message shows
    #[test]
    fn a_page_of_subagent_messages_reads_back_to_the_roots_message() {
        let mut screen = with_subagent();
        let page = |message: CommittedMessage, older: Option<&str>| Page {
            items: vec![message],
            next_cursor: older.map(|cursor| PageCursor::new(cursor).unwrap()),
            snapshot_revision: Revision(1),
        };
        let older_reads = |sent: &[Action]| {
            sent.iter()
                .filter(|action| {
                    matches!(action, Action::History(thread, None, true)
                        if thread.as_str() == "thr-1")
                })
                .count()
        };
        {
            let mut shared = screen.shared.lock().unwrap();
            shared.cache.questions.clear();
            shared.cache.approvals.clear();
            let newest = page(child_message(), Some("c1"));
            shared.cache.history_page("thr-1", newest, false);
        }
        let sent = screen.settle_runs();
        assert_eq!(older_reads(&sent), 1);
        assert!(
            screen
                .harness
                .query_by_label("Start the conversation")
                .is_none()
        );
        screen
            .harness
            .get_by_label_contains("Load earlier messages");

        let mut earlier = child_message();
        earlier.index = view(json!("1"));
        let older = page(earlier, Some("c2"));
        let history = |screen: &mut Screen, page: Page<CommittedMessage>, older: bool| {
            let mut shared = screen.shared.lock().unwrap();
            shared.cache.history_page("thr-1", page, older);
        };
        history(&mut screen, older, true);
        assert_eq!(older_reads(&screen.settle_runs()), 1);

        let root = message(0, "user", "Where does the redirect go?");
        history(&mut screen, page(root, Some("c3")), true);
        assert_eq!(older_reads(&screen.settle_runs()), 0);
        screen
            .harness
            .get_by_label("You: Where does the redirect go?");
        assert!(screen.harness.query_by_label(CHILD_TEXT).is_none());
    }

    // Given: thr-1's root was stopped while run-1a still runs
    // When: Stop subagents is pressed, then the root is done instead
    // Then: the thread shows Stopped and one StopChat for thr-1 is sent; a finished
    //       root offers neither Stop nor Stop subagents
    #[test]
    fn a_stopped_root_with_running_subagents_offers_stop_subagents() {
        let mut screen = with_subagent();
        // Execution's per-run Stop is not the conversation's: keep its cards out of sight.
        shell::focus(
            &mut screen.harness.state_mut().dock,
            CoreTab::Feature(FeatureId::Teams),
        );
        screen.settle_runs();
        let set_root = |screen: &mut Screen, phase| {
            let mut shared = screen.shared.lock().unwrap();
            shared.cache.runs.get_mut("run-1").unwrap().phase = phase;
        };
        set_root(&mut screen, RunPhase::Cancelled);
        screen.settle_runs();
        screen.capture("stopped");
        screen.harness.get_by_label("Stopped");
        assert!(screen.harness.query_by_label("Stop").is_none());
        screen.harness.get_by_label("Stop subagents").click();
        let sent = screen.settle_runs();
        let commands: Vec<_> = sent
            .iter()
            .filter_map(|action| match action {
                Action::Command { command, .. } => Some(command),
                _ => None,
            })
            .collect();
        let [Command::Execution(ExecutionCommand::StopChat(target))] = commands[..] else {
            panic!("one StopChat was expected, got {} commands", commands.len());
        };
        assert_eq!(target.thread.as_str(), "thr-1");

        set_root(&mut screen, RunPhase::Done);
        screen.settle_runs();
        assert!(screen.harness.query_by_label("Stop").is_none());
        assert!(screen.harness.query_by_label("Stop subagents").is_none());
        assert!(screen.harness.query_by_label("Stopped").is_none());
    }

    // Given: run-1a's tab open in front of the Subagents leaf, the window unfocused
    // When: run-1a fails (its tab is parked behind Subagents) and the root run-1 is done,
    //       then its tab is brought forward and the window gains focus
    // Then: the run tab is flagged as an error, the Conversation and Notifications tabs
    //       as info (Execution never), under their unchanged accessible names, until
    //       drawn with focus; the pane shows the Failed badge, filled until read
    #[test]
    fn a_run_tab_flags_its_run_until_drawn_with_focus() {
        use super::attention::flagged;
        use crate::panes::tab_attention::PaneAttention;
        use crate::theme::icons::{self, with_icon};
        use workspace_ui::ThreadRunPhase;
        let mut screen = with_subagent();
        screen.open_subagent();
        let [tab] = screen.run_tabs()[..] else {
            panic!("one run tab was expected");
        };
        let attention = |screen: &Screen, tab| screen.harness.state().attention.of(tab);
        assert_eq!(attention(&screen, tab), PaneAttention::None);
        let flagged_run = format!(
            "• {}",
            with_icon(icons::SCROLL, "Transcript: redirect-tester")
        );
        assert!(!flagged(&screen).contains(&flagged_run));
        let CoreTab::Run(id) = tab else {
            panic!("a run tab was expected");
        };
        let badge = |screen: &Screen| screen.harness.state().attention.run_phase(id);
        assert!(screen.harness.query_by_label("Failed").is_none());

        {
            let mut shared = screen.shared.lock().unwrap();
            shared.cache.runs.get_mut("run-1a").unwrap().phase = RunPhase::Failed;
            shared.cache.runs.get_mut("run-1").unwrap().phase = RunPhase::Done;
        }
        screen.harness.run_steps(2);
        assert_eq!(attention(&screen, tab), PaneAttention::Error);
        assert_eq!(
            attention(&screen, CoreTab::Conversation),
            PaneAttention::Info
        );
        let execution = CoreTab::Feature(FeatureId::Execution);
        assert_eq!(attention(&screen, execution), PaneAttention::None);
        let mut expected = vec![
            flagged_run,
            format!("• {}", with_icon(icons::CHAT_CIRCLE_TEXT, "Conversation")),
            format!("• {}", with_icon(icons::BELL, "Notifications")),
        ];
        expected.sort();
        assert_eq!(flagged(&screen), expected);
        assert_eq!(badge(&screen), Some((ThreadRunPhase::Error, true)));
        for name in [
            "Transcript: redirect-tester tab",
            "Conversation tab",
            "Notifications tab",
        ] {
            screen.harness.get_by_label(name);
        }
        // The failed subagent was parked at the end of the Subagents leaf, behind it; the
        // person brings its tab forward.
        assert_eq!(screen.front_of_leaf(tab), execution);
        shell::focus(&mut screen.harness.state_mut().dock, tab);
        screen.harness.run_steps(2);
        assert_eq!(screen.front_of_leaf(tab), tab);
        assert_eq!(
            attention(&screen, tab),
            PaneAttention::Error,
            "not read unfocused"
        );
        screen.harness.get_by_label("Failed");
        screen.capture("attention");

        screen
            .harness
            .input_mut()
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .focused = Some(true);
        screen.harness.run_steps(2);
        assert_eq!(attention(&screen, tab), PaneAttention::None);
        assert_eq!(
            attention(&screen, CoreTab::Conversation),
            PaneAttention::None
        );
        assert!(flagged(&screen).is_empty(), "{:?}", flagged(&screen));
        // The pane keeps its run's phase, outlined once read.
        assert_eq!(badge(&screen), Some((ThreadRunPhase::Error, false)));
        screen.harness.get_by_label("Failed");
    }

    fn leaf_of(screen: &Screen, tab: CoreTab) -> Vec<CoreTab> {
        let dock = &screen.harness.state().dock;
        let path = dock.find_tab(&tab).expect("the tab is in the dock");
        dock.leaf(path.node_path()).unwrap().tabs.clone()
    }

    fn focused(screen: &Screen) -> Option<Vec<CoreTab>> {
        let dock = &screen.harness.state().dock;
        dock.focused_leaf()
            .and_then(|path| dock.leaf(path).ok())
            .map(|leaf| leaf.tabs.clone())
    }

    fn set_subagent(screen: &Screen, phase: &str) {
        let mut shared = screen.shared.lock().unwrap();
        shared.cache.runs.insert("run-1a".into(), subagent(phase));
    }

    /// The window drawn on thr-1 with the Conversation focused, before run-1a exists.
    fn before_subagent() -> Screen {
        let mut screen = screen(egui::vec2(1440.0, 900.0));
        screen.settle_runs();
        shell::focus(&mut screen.harness.state_mut().dock, CoreTab::Conversation);
        screen.settle_runs();
        assert!(screen.run_tabs().is_empty());
        screen
    }

    // Given: thr-1 drawn with the Conversation focused
    // When: its subagent run-1a starts, then ends
    // Then: run-1a's tab opens by itself in a split of its own below the Subagents tab,
    //       reads its history and takes its agent's name, while the Conversation keeps the
    //       focus; once it ends, the tab is parked behind the Subagents tab in its leaf
    #[test]
    fn a_subagent_opens_below_subagents_when_it_starts_and_is_parked_when_it_ends() {
        let mut screen = before_subagent();
        let conversation = focused(&screen);
        assert!(conversation.is_some_and(|tabs| tabs.contains(&CoreTab::Conversation)));
        let execution = CoreTab::Feature(FeatureId::Execution);

        set_subagent(&screen, "running");
        let sent = screen.settle_runs();
        assert_eq!(history_of_run_1a(&sent), 1);
        let [tab] = screen.run_tabs()[..] else {
            panic!("run-1a's tab was expected");
        };
        assert_eq!(leaf_of(&screen, tab), [tab], "a split of its own");
        assert!(!leaf_of(&screen, execution).contains(&tab));
        assert_eq!(
            focused(&screen),
            Some(leaf_of(&screen, CoreTab::Conversation))
        );
        screen
            .harness
            .get_by_label("Transcript: redirect-tester tab");
        screen.harness.get_by_label("Subagents(1) tab");
        let page = Page {
            items: vec![child_message()],
            next_cursor: None,
            snapshot_revision: Revision(1),
        };
        screen
            .shared
            .lock()
            .unwrap()
            .cache
            .run_history_page("run-1a", page, false);
        screen.harness.run_steps(2);
        screen.harness.get_by_label(CHILD_TEXT);
        screen.capture("subagent-started");

        set_subagent(&screen, "done");
        screen.settle_runs();
        let leaf = leaf_of(&screen, tab);
        assert!(
            leaf.contains(&execution),
            "parked in the Subagents leaf: {leaf:?}"
        );
        assert_eq!(leaf.last(), Some(&tab));
        assert_eq!(screen.front_of_leaf(tab), execution);
        assert_eq!(
            focused(&screen),
            Some(leaf_of(&screen, CoreTab::Conversation))
        );
        screen.harness.get_by_label("Subagents(1) tab");
        screen.capture("subagent-parked");
    }

    // Given: run-1a's tab opened by itself while it runs
    // When: another thread is selected, then thr-1 again
    // Then: the tab leaves the dock and comes back in a split of its own
    #[test]
    fn a_running_subagents_split_comes_back_with_its_thread() {
        let mut screen = before_subagent();
        set_subagent(&screen, "running");
        screen.settle_runs();
        screen
            .harness
            .get_by_label("Refactor storage writer")
            .click();
        screen.settle_runs();
        assert!(screen.run_tabs().is_empty());
        screen.harness.get_by_label("Subagents(0) tab");

        screen.harness.get_by_label("Fix login redirect").click();
        screen.settle_runs();
        let [tab] = screen.run_tabs()[..] else {
            panic!("run-1a's tab was expected back");
        };
        assert_eq!(leaf_of(&screen, tab), [tab]);
    }

    // Given: thr-1 drawn before run-1a exists, then run-1a's tab opened and closed
    // When: a new session's projection has run-1a ended (it ran while disconnected), and
    //       likewise for a run never seen at all
    // Then: each gets a parked tab behind the Subagents tab, as a closed one does
    #[test]
    fn a_subagent_that_ended_while_disconnected_is_parked() {
        let mut screen = before_subagent();
        set_subagent(&screen, "running");
        screen.settle_runs();
        let [tab] = screen.run_tabs()[..] else {
            panic!("run-1a's tab was expected");
        };
        let dock = &mut screen.harness.state_mut().dock;
        let path = dock.find_tab(&tab).unwrap();
        dock.remove_tab(path);
        screen.settle_runs();
        assert!(screen.run_tabs().is_empty(), "closed");

        {
            let mut shared = screen.shared.lock().unwrap();
            let mut cache = populated().cache;
            cache.runs.insert("run-1a".into(), subagent("failed"));
            let mut unseen = subagent("done");
            unseen.id = RunId::new("run-1b").unwrap();
            cache.runs.insert("run-1b".into(), unseen);
            shared.begin_session(cache, Capability::IMPLEMENTED.to_vec(), &[], Vec::new());
        }
        screen.settle_runs();
        let execution = CoreTab::Feature(FeatureId::Execution);
        let tabs = screen.run_tabs();
        assert_eq!(tabs.len(), 2, "{tabs:?}");
        let leaf = leaf_of(&screen, execution);
        assert!(tabs.iter().all(|tab| leaf.contains(tab)), "{leaf:?}");
        assert_eq!(screen.front_of_leaf(execution), execution);
        screen.harness.get_by_label("Subagents(2) tab");
    }

    // Given: run-1a's tab opened by itself, then closed by the person while it runs
    // When: run-1a ends, then starts again
    // Then: it comes back parked behind the Subagents tab, then in a split of its own
    #[test]
    fn a_closed_subagent_tab_comes_back_parked_then_split_again() {
        let mut screen = before_subagent();
        set_subagent(&screen, "running");
        screen.settle_runs();
        let [tab] = screen.run_tabs()[..] else {
            panic!("run-1a's tab was expected");
        };
        let dock = &mut screen.harness.state_mut().dock;
        let path = dock.find_tab(&tab).unwrap();
        dock.remove_tab(path);
        screen.settle_runs();
        assert!(screen.run_tabs().is_empty(), "closed");

        set_subagent(&screen, "done");
        screen.settle_runs();
        let execution = CoreTab::Feature(FeatureId::Execution);
        let [tab] = screen.run_tabs()[..] else {
            panic!("run-1a's tab was expected back");
        };
        assert_eq!(leaf_of(&screen, execution).last(), Some(&tab));
        assert_eq!(screen.front_of_leaf(tab), execution);

        set_subagent(&screen, "running");
        screen.settle_runs();
        assert_eq!(leaf_of(&screen, tab), [tab], "unparked into a split");
        assert!(!leaf_of(&screen, execution).contains(&tab));
    }

    // Given: thr-1 drawn with the Conversation focused
    // When: a subagent of thr-6 starts, then thr-6 is selected
    // Then: no tab opens on thr-1; on thr-6 the subagent's tab is in a split of its own
    #[test]
    fn another_threads_subagent_opens_only_once_its_thread_is_selected() {
        let mut screen = before_subagent();
        {
            let mut shared = screen.shared.lock().unwrap();
            let mut run = subagent("running");
            run.id = RunId::new("run-6a").unwrap();
            run.thread = ThreadId::new("thr-6").unwrap();
            run.parent_run = Some(RunId::new("run-6").unwrap());
            shared.cache.runs.insert("run-6a".into(), run);
        }
        screen.settle_runs();
        assert!(screen.run_tabs().is_empty());
        screen.harness.get_by_label("Subagents(0) tab");

        screen
            .harness
            .get_by_label("Rebuild the search index")
            .click();
        screen.settle_runs();
        let [tab] = screen.run_tabs()[..] else {
            panic!("run-6a's tab was expected on thr-6");
        };
        assert_eq!(leaf_of(&screen, tab), [tab]);
        screen.harness.get_by_label("Subagents(1) tab");
    }

    // Given: run-1's subagent run-1a has a subagent of its own, run-1x
    // When: the Subagents tab is drawn, run-1a's Subagents button is pressed, then Root
    // Then: run-1a is listed first without a breadcrumb; drilled in, run-1x is listed
    //       under Root › run-1a; Root lists run-1a again
    #[test]
    fn the_subagents_tab_lists_the_roots_subagents_and_drills_in_and_out() {
        let children_of = |parent: &str| match parent {
            "run-1" => vec![run_detail("run-1a", Some("run-1"), "redirect-tester", 1)],
            "run-1a" => vec![run_detail("run-1x", Some("run-1a"), "fixture-writer", 0)],
            _ => Vec::new(),
        };
        let mut screen = screen(egui::vec2(1440.0, 900.0));
        screen.settle_runs_by(&children_of);
        screen.harness.get_by_label("redirect-tester");
        assert!(screen.harness.query_by_label("Root").is_none());
        assert!(screen.harness.query_by_label("fixture-writer").is_none());

        screen.harness.get_by_label("Subagents (1)").click();
        screen.settle_runs_by(&children_of);
        screen.harness.get_by_label("Root");
        screen.harness.get_by_label("run-1a");
        screen.harness.get_by_label("fixture-writer");
        assert!(screen.harness.query_by_label("redirect-tester").is_none());

        screen.harness.get_by_label("Root").click();
        screen.settle_runs_by(&children_of);
        screen.harness.get_by_label("redirect-tester");
        assert!(screen.harness.query_by_label("Root").is_none());
        assert!(screen.harness.query_by_label("fixture-writer").is_none());
    }
}

mod tool_outputs {
    use super::*;

    const SHELL: &str = "exit code: 0";
    const CHANGE: &str = "--- a/src/auth/login.rs";
    const FOLDED: &str = "2 tool outputs";

    // Given: thr-1's root committed two tool outputs and has not replied yet
    // When: the window is drawn, then the root's reply is committed, then the folded
    //       header is pressed and more of the conversation arrives
    // Then: both outputs stay in sight without a header while it works; once it
    //       replied only "2 tool outputs" shows; pressed, they show again and stay
    //       shown over later frames and messages
    #[test]
    fn tool_outputs_stay_in_sight_while_the_root_works_and_fold_once_it_replies() {
        let local = super::super::local::LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            None,
        );
        let mut screen = screen_from(egui::vec2(1440.0, 900.0), local, |shared| {
            let history = shared.cache.history.get_mut("thr-1").unwrap();
            history.pop();
        });
        screen.settle_runs_with(&[]);
        screen.harness.run_steps(2);
        screen.harness.get_by_label(SHELL);
        screen.harness.get_by_label(CHANGE);
        assert!(
            screen
                .harness
                .query_by_label_contains("tool outputs")
                .is_none()
        );
        screen.harness.get_by_label("No subagents of this run");
        screen.capture("working");

        let reply = |screen: &Screen, index, role, text| {
            let mut shared = screen.shared.lock().unwrap();
            let history = shared.cache.history.get_mut("thr-1").unwrap();
            history.push(message(index, role, text));
        };
        reply(
            &screen,
            4,
            "assistant",
            "The handler ignored `next`; it is fixed.",
        );
        screen.harness.run_steps(2);
        screen.harness.get_by_label(FOLDED);
        assert!(screen.harness.query_by_label(SHELL).is_none());
        assert!(screen.harness.query_by_label(CHANGE).is_none());
        screen.capture("done");

        screen.harness.get_by_label(FOLDED).click();
        screen.harness.run_steps(4);
        reply(&screen, 5, "user", "Thanks, run the tests too.");
        screen.harness.run_steps(4);
        screen.harness.get_by_label(SHELL);
        screen.harness.get_by_label(CHANGE);
        screen.harness.get_by_label(FOLDED);
    }
}

mod local_panes {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    use super::*;
    use crate::core_client::local::LocalPanes;
    use crate::diff::{DiffError, DiffRequest, DiffSource};
    use crate::pty::{PtySession, TerminalError, TerminalSpawner, Wake};

    /// Reports the root of each fetch and finds no changes there.
    struct RecordingDiffSource(Mutex<mpsc::Sender<PathBuf>>);

    impl DiffSource for RecordingDiffSource {
        fn fetch(&self, req: &DiffRequest) -> Result<String, DiffError> {
            let _ = self.0.lock().unwrap().send(req.repo_root.clone());
            Ok(String::new())
        }
    }

    /// Records where each shell would start, and starts none.
    #[derive(Default)]
    struct RecordingSpawner(Mutex<Vec<PathBuf>>);

    impl TerminalSpawner for RecordingSpawner {
        fn spawn(&self, cwd: &Path, _: u16, _: u16, _: Wake) -> Result<PtySession, TerminalError> {
            self.0.lock().unwrap().push(cwd.to_path_buf());
            Err(TerminalError::Spawn("test spawner".into()))
        }
    }

    impl RecordingSpawner {
        fn cwds(&self) -> Vec<PathBuf> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Answers with these project roots and thread active roots.
    fn roots(
        projects: Vec<(&'static str, PathBuf)>,
        threads: Vec<(&'static str, PathBuf)>,
    ) -> impl Fn(&WorkspaceQuery, &mut WorkspaceReply) {
        move |_, reply| match reply {
            WorkspaceReply::ProjectAccess(access) => {
                if let Some((_, root)) = projects
                    .iter()
                    .find(|(id, _)| *id == access.project.as_str())
                {
                    access.root = root.to_string_lossy().into_owned();
                }
            }
            WorkspaceReply::ThreadDetail(detail) => {
                if let Some((_, root)) =
                    threads.iter().find(|(id, _)| *id == detail.thread.as_str())
                {
                    detail.active_root = Some(root.to_string_lossy().into_owned());
                }
            }
            _ => {}
        }
    }

    impl Screen {
        fn open_panel(&mut self, label: &str) {
            self.harness.get_by_label("⚙").click();
            self.harness.run_steps(2);
            self.harness.get_by_label("Panels ⏵").click();
            self.harness.run_steps(2);
            self.harness.get_by_label(label).click();
            self.harness.run_steps(2);
        }

        /// Clicks `title` in the Projects tab and settles; the frames after draw the
        /// panes for the new selection, which a click applies only at its frame's end.
        fn select(&mut self, title: &str, patch: &dyn Fn(&WorkspaceQuery, &mut WorkspaceReply)) {
            self.harness.get_by_label(title).click();
            self.settle_with(patch);
            self.harness.run_steps(2);
        }

        pub(super) fn front_of_leaf(&self, tab: CoreTab) -> CoreTab {
            let dock = &self.harness.state().dock;
            let path = dock.find_tab(&tab).expect("the tab is in the dock");
            let leaf = dock.leaf(path.node_path()).expect("a leaf");
            leaf.tabs[leaf.active.0]
        }

        pub(super) fn terminal_collapsed(&self) -> bool {
            let dock = &self.harness.state().dock;
            let path = dock.find_tab(&CoreTab::Terminal).expect("the Terminal tab");
            dock.leaf(path.node_path()).expect("a leaf").collapsed
        }
    }

    // Given: proj-1's root and thr-2's own working tree exist on this machine
    // When: the Diff tab is opened on thr-1, then thr-2 is selected
    // Then: the diff is read in proj-1's root (thr-1 has no active root), then in thr-2's
    #[test]
    fn the_diff_follows_the_selected_threads_root() {
        let project = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let (tx, fetched) = mpsc::channel();
        let local = LocalPanes::new(Arc::new(RecordingDiffSource(Mutex::new(tx))), None);
        let mut screen = screen_with(egui::vec2(1440.0, 900.0), local);
        let patch = roots(
            vec![("proj-1", project.path().into())],
            vec![("thr-2", worktree.path().into())],
        );
        screen.settle_with(&patch);

        screen.harness.get_by_label("Diff tab").click();
        screen.harness.run_steps(2);
        assert_eq!(fetched.recv().unwrap(), project.path());
        while screen.harness.query_by_label("no changes").is_none() {
            screen.harness.step();
        }

        screen.select("Refactor storage writer", &patch);
        // A refresh of proj-1's root may be due before the new root is read.
        let root = fetched
            .iter()
            .find(|root| root != project.path())
            .expect("a fetch in thr-2's root");
        assert_eq!(root, worktree.path());
    }

    // Given: the core names a project root this machine does not have
    // When: the Diff and the Terminal are opened from ⚙ → Panels
    // Then: both say the workspace is not available here; Diff is in front of its leaf
    //       and no shell is started
    #[test]
    fn a_root_missing_on_this_machine_shows_workspace_not_available() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let spawner = Arc::new(RecordingSpawner::default());
        let local = LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            Some(spawner.clone()),
        );
        let mut screen = screen_with(egui::vec2(1440.0, 900.0), local);
        screen.settle_with(&roots(vec![("proj-1", gone)], Vec::new()));

        screen.open_panel("Diff");
        assert_eq!(screen.front_of_leaf(CoreTab::Diff), CoreTab::Diff);
        screen
            .harness
            .get_by_label("Workspace not available on this machine");

        screen.open_panel("Terminal");
        assert!(!screen.terminal_collapsed());
        assert_eq!(
            screen
                .harness
                .get_all_by_label("Workspace not available on this machine")
                .count(),
            2
        );
        assert!(spawner.cwds().is_empty());
    }

    // Given: the Terminal opened before the core has said where proj-1 is
    // When: the roots arrive, proj-2's thread is selected, then thr-1 again, and the
    //       layout is reset
    // Then: no shell waits on an unknown root, each project starts one shell in its root,
    //       a project selected again keeps its shell, and a reset folds the Terminal
    //       without ending the shells
    #[test]
    fn the_terminal_runs_one_shell_per_project_and_survives_reset_layout() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let spawner = Arc::new(RecordingSpawner::default());
        let local = LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            Some(spawner.clone()),
        );
        let mut screen = screen_with(egui::vec2(1440.0, 900.0), local);
        assert!(screen.terminal_collapsed());
        screen.harness.run_steps(1);
        screen.open_panel("Terminal");
        screen.harness.get_by_label("Loading workspace…");
        assert!(spawner.cwds().is_empty());

        let patch = roots(
            vec![
                ("proj-1", first.path().into()),
                ("proj-2", second.path().into()),
            ],
            Vec::new(),
        );
        screen.settle_with(&patch);
        // The shell is started while its pane is drawn, after the header.
        screen.harness.run_steps(2);
        assert_eq!(spawner.cwds(), [first.path()]);
        screen
            .harness
            .get_by_label("Failed to start shell: PTY spawn failed: test spawner");

        screen.select("Landing page copy", &patch);
        assert_eq!(spawner.cwds(), [first.path(), second.path()]);

        screen.select("Fix login redirect", &patch);
        assert_eq!(spawner.cwds().len(), 2);

        screen.harness.get_by_label("⚙").click();
        screen.harness.run_steps(2);
        screen.harness.get_by_label("Reset layout").click();
        screen.settle_with(&patch);
        assert!(screen.terminal_collapsed());
        assert_eq!(screen.harness.state().local.terminals.len(), 2);

        screen.open_panel("Terminal");
        screen
            .harness
            .get_by_label("Failed to start shell: PTY spawn failed: test spawner");
        assert_eq!(spawner.cwds().len(), 2);
    }

    // Given: proj-2 listed by the core but added after the snapshot, so the cache lacks it
    // When: its thread is selected with the Terminal open and frames keep coming
    // Then: proj-2's shell is started once and kept
    #[test]
    fn a_project_listed_after_the_snapshot_keeps_its_shell() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let spawner = Arc::new(RecordingSpawner::default());
        let local = LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            Some(spawner.clone()),
        );
        let mut screen = screen_with(egui::vec2(1440.0, 900.0), local);
        screen
            .shared
            .lock()
            .unwrap()
            .cache
            .projects
            .remove("proj-2");
        let patch = roots(
            vec![
                ("proj-1", first.path().into()),
                ("proj-2", second.path().into()),
            ],
            Vec::new(),
        );
        screen.settle_with(&patch);
        screen.shared.lock().unwrap().features.event(
            &workbench_protocol::events::UiEvent::Workspace(
                workbench_protocol::namespaces::WorkspaceEvent::WorkspaceChanged {
                    revision: Revision(2),
                },
            ),
        );
        screen.settle_with(&patch);
        screen.open_panel("Terminal");
        assert_eq!(spawner.cwds(), [first.path()]);

        screen.select("Landing page copy", &patch);
        screen.harness.run_steps(4);

        assert_eq!(spawner.cwds(), [first.path(), second.path()]);
        assert_eq!(screen.harness.state().local.terminals.len(), 2);
    }

    // Given: proj-1's shell started in its root
    // When: the Diff and the Terminal are closed with a middle click, then opened from
    //       ⚙ → Panels
    // Then: the Diff returns beside Notifications and the Terminal below the
    //       conversation, with the shell it had
    #[test]
    fn the_local_tabs_close_and_reopen_from_panels() {
        let project = tempfile::tempdir().unwrap();
        let spawner = Arc::new(RecordingSpawner::default());
        let local = LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            Some(spawner.clone()),
        );
        let mut screen = screen_with(egui::vec2(1440.0, 900.0), local);
        screen.settle_with(&roots(vec![("proj-1", project.path().into())], Vec::new()));
        screen.open_panel("Terminal");
        assert_eq!(spawner.cwds(), [project.path()]);

        for (tab, label) in [
            (CoreTab::Diff, "Diff tab"),
            (CoreTab::Terminal, "Terminal tab"),
        ] {
            screen
                .harness
                .get_by_label(label)
                .click_button(egui::PointerButton::Middle);
            screen.harness.run_steps(2);
            assert!(
                screen.harness.state().dock.find_tab(&tab).is_none(),
                "{label}"
            );
        }

        screen.open_panel("Diff");
        let dock = &screen.harness.state().dock;
        let diff = dock.find_tab(&CoreTab::Diff).expect("the Diff tab");
        let notifications = dock.find_tab(&CoreTab::Notifications).unwrap();
        assert_eq!(diff.node_path(), notifications.node_path());
        assert_eq!(diff.tab.0, notifications.tab.0 + 1);
        assert_eq!(screen.front_of_leaf(CoreTab::Diff), CoreTab::Diff);

        screen.open_panel("Terminal");
        assert!(!screen.terminal_collapsed());
        let terminal = screen.harness.get_by_label("Terminal tab").rect();
        let conversation = screen.harness.get_by_label("Conversation tab").rect();
        assert!(terminal.top() > conversation.bottom());
        assert!((terminal.left() - conversation.left()).abs() < 1.0);
        screen
            .harness
            .get_by_label("Failed to start shell: PTY spawn failed: test spawner");
        assert_eq!(spawner.cwds().len(), 1);
    }

    const DIFF: &str = "\
diff --git a/src/auth/redirect.rs b/src/auth/redirect.rs
index 1111111..2222222 100644
--- a/src/auth/redirect.rs
+++ b/src/auth/redirect.rs
@@ -1,5 +1,6 @@
 use crate::session::Session;

-pub fn redirect(session: &Session) -> &str {
-    \"/\"
+pub fn redirect(session: &Session) -> String {
+    let next = session.next.as_deref().unwrap_or(\"/\");
+    next.to_owned()
 }
";

    // Given: proj-1's working tree has changes
    // When: the Diff tab is opened
    // Then: the changed files are listed with their lines
    #[test]
    fn the_diff_tab_shows_the_changes_in_the_selected_root() {
        let project = tempfile::tempdir().unwrap();
        let local = LocalPanes::new(Arc::new(crate::diff::FixtureDiffSource::ready(DIFF)), None);
        let mut screen = screen_with(egui::vec2(1440.0, 900.0), local);
        screen.settle_with(&roots(vec![("proj-1", project.path().into())], Vec::new()));

        screen.harness.get_by_label("Diff tab").click();
        while screen.harness.query_by_label("1 changed files").is_none() {
            screen.harness.step();
        }
        screen.harness.run_steps(1);
        screen.capture("diff");
    }

    const OUTPUT: &str = concat!(
        "\x1b[1;32m~/evorch\x1b[0m $ cargo test -p auth --test redirect\r\n",
        "   Compiling auth v0.1.0\r\n",
        "    Finished `test` profile [unoptimized + debuginfo] target(s) in 2.31s\r\n",
        "     Running tests/redirect.rs\r\n",
        "\r\n",
        "running 2 tests\r\n",
        "test keeps_the_next_path ... \x1b[32mok\x1b[0m\r\n",
        "test falls_back_to_the_root ... \x1b[32mok\x1b[0m\r\n",
        "\r\n",
        "test result: \x1b[32mok\x1b[0m. 2 passed; 0 failed\r\n",
        "\x1b[1;32m~/evorch\x1b[0m $ ",
    );

    // Given: the Terminal open on proj-1, whose session has written output
    // When: the frame is drawn
    // Then: the grid shows that output under the root it runs in
    #[test]
    fn the_terminal_draws_its_sessions_output() {
        let project = tempfile::tempdir().unwrap();
        let mut screen = screen(egui::vec2(1440.0, 900.0));
        screen.settle_with(&roots(vec![("proj-1", project.path().into())], Vec::new()));
        screen.open_panel("Terminal");

        let key = Some(ProjectId::new("proj-1").unwrap());
        let local = &mut screen.harness.state_mut().local;
        let session = local.terminals.get(&key).expect("proj-1's session");
        assert_eq!(session.cwd(), project.path());
        local
            .terminals
            .session_mut(&key, project.path())
            .emulator_mut()
            .feed(OUTPUT.as_bytes());
        screen.harness.run_steps(2);
        let grid = screen.harness.get_by_label("Terminal");
        assert!(grid.value().unwrap().contains("test result: ok. 2 passed"));
        screen.capture("terminal");
    }

    // Given: a 1024×768 window on thr-1, whose root run is running
    // When: the Terminal is folded, as by default, and then opened
    // Then: the conversation's Stop, Message and Queue stay inside the window
    #[test]
    fn the_conversation_controls_stay_in_view_with_the_terminal_open() {
        let size = egui::vec2(1024.0, 768.0);
        let project = tempfile::tempdir().unwrap();
        let mut screen = screen(size);
        screen.settle_with(&roots(vec![("proj-1", project.path().into())], Vec::new()));
        let window = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        for open in [false, true] {
            if open {
                screen.open_panel("Terminal");
            }
            for control in ["Stop", "Message", "Queue"] {
                let mut nodes = screen.harness.query_all_by_label(control).peekable();
                assert!(nodes.peek().is_some(), "{control}");
                for node in nodes {
                    let centre = node.rect().center();
                    assert!(
                        window.contains(centre),
                        "{control} at {centre:?}, open: {open}"
                    );
                }
            }
        }
        screen.capture("terminal-narrow");
    }

    mod files;
}

mod session;

mod attention;

mod footer;

mod settings;

mod lanes;

mod composer;
mod many_subagents;
mod pull_requests;
mod read_panes;

// Given: the lane thr-4 asked its lead thr-1 a question the lead has not passed on
// When: the lead thread thr-1 is shown
// Then: the lane's question is not offered as a question for the person; the lead
//       thread shows the embedded workbench's count of lane questions instead
#[test]
fn a_lane_question_routed_to_its_lead_is_not_offered_to_the_person() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let lane_question: QuestionView = view(json!({
        "id": "q-lane",
        "requester": {"run": "run-4", "thread": "thr-4", "root_run": "run-4"},
        "target": {"type": "lead", "payload": {"lead_thread": "thr-1", "lead_run": "run-1"}},
        "management_revision": "1", "routing_revision": "1", "blocking": true,
        "title": "Which redirect test matrix should the lane cover?",
        "options": [],
        "revision": "2",
        "raised_by": {"actor": "agent", "agent_run_id": "run-4", "surface": "none"},
    }));
    screen
        .shared
        .lock()
        .unwrap()
        .cache
        .questions
        .insert("q-lane".into(), lane_question);
    screen.harness.run_steps(4);
    let harness = &screen.harness;
    assert_eq!(harness.query_all_by_label("Question for you").count(), 1);
    assert!(
        harness
            .query_by_label_contains("Which redirect test matrix")
            .is_none()
    );
    harness.get_by_label_contains("1 lane question(s) wait for your answer here");
}
