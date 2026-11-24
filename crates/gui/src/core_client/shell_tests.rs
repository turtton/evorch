use std::sync::{Arc, Mutex};

use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use workbench_protocol::{
    handshake::Capability,
    ids::{ProjectId, RunId, ThreadId},
    namespaces::{
        WorkspaceQuery, WorkspaceReply,
        workspace::{FamilyView, LineageType, ProjectAccessView, ThreadDetail, TrustLevel},
    },
    queries::{Query, QueryReply},
    views::{
        ApprovalView, CommittedMessage, Page, ProjectSummary, QuestionView, RunSummary,
        ThreadSummary,
    },
    wire::Revision,
};

use super::connection::Action;
use super::features::{FeatureAction, FeatureId, FeatureReply, FeatureRequest};
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
