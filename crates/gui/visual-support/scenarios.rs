//! The screens of the evidence: the real `CoreClientScreen`, drawn through its public API from
//! a synthetic fixture, at a fixed size and DPI with the Graphite theme.
//!
//! Nothing here reaches outside the process: the shared state is filled by hand, the
//! client queue is a channel this module reads itself, the Diff pane reads a fixture and
//! no terminal spawner is set. Every request the screen makes is either answered from the
//! fixture or refused; a request this module does not know fails the capture.

use std::sync::{Arc, Mutex};

use egui_kittest::{Harness, kittest::Queryable};
use gui::core_client::{
    CoreClientScreen, Shared,
    connection::Action,
    features::{FeatureAction, FeatureId, FeatureReply, FeatureRequest},
    local::LocalPanes,
};
use image::RgbaImage;
use serde_json::json;
use workbench_protocol::{
    handshake::Capability,
    ids::{ProjectId, RunId, ThreadId},
    namespaces::{
        ExecutionQuery, ExecutionReply, WorkspaceQuery, WorkspaceReply,
        artifacts::ArtifactsReply,
        execution::{
            ThreadExecution,
            plan::{PlanProgress, PlanSummary},
        },
        settings::{
            CredentialRef, ModelKind, ModelView, ProviderKind, ProviderView, SettingsQuery,
            SettingsReply, SettingsRevision, SettingsSnapshot,
        },
        workspace::{FamilyView, LineageType, ProjectAccessView, ThreadDetail, TrustLevel},
    },
    queries::{Query, QueryReply},
    views::{
        ApprovalView, CommittedMessage, Page, ProjectSummary, QuestionView, RunSummary,
        ThreadSummary,
    },
    wire::Revision,
};

pub const THEME: &str = "graphite";
pub const DPI: f32 = 1.0;
const WIDTH: f32 = 1440.0;
const HEIGHT: f32 = 900.0;
/// A frame cap for "the screen stopped asking to be redrawn".
const MAX_FRAMES: u64 = 64;

pub struct ScenarioSpec {
    pub id: &'static str,
    /// Drawn texts the screen must show before it is captured.
    pub required: &'static [&'static str],
}

pub const SCENARIOS: &[ScenarioSpec] = &[
    ScenarioSpec {
        id: "workspace-conversation",
        required: &[
            "Add project",
            "Collapse threads of evorch",
            "Message",
            "ログイン後のリダイレクトを修正",
        ],
    },
    ScenarioSpec {
        id: "attention",
        required: &["Tool approval needed", "Question for you", "Message"],
    },
    ScenarioSpec {
        id: "settings-models",
        required: &["Add", "Refresh credentials", "ACCOUNT", "qwen3-coder"],
    },
];

pub fn size() -> (u32, u32) {
    (WIDTH as u32, HEIGHT as u32)
}

// ---------------------------------------------------------------- fixture data

const PROJECTS: [(&str, &str, &str); 2] = [
    ("proj-1", "evorch", "/fixture/evorch"),
    ("proj-2", "website", "/fixture/website"),
];

struct Fixture {
    id: &'static str,
    project: &'static str,
    title: &'static str,
    state: &'static str,
    parent: Option<&'static str>,
    root_run: Option<&'static str>,
    pinned: bool,
    updated: u64,
}

const fn thread(id: &'static str, title: &'static str, updated: u64) -> Fixture {
    Fixture {
        id,
        project: "proj-1",
        title,
        state: "idle",
        parent: None,
        root_run: None,
        pinned: false,
        updated,
    }
}

/// `waiting`: the first thread waits on the user, as in the attention scenario.
fn threads(waiting: bool) -> Vec<Fixture> {
    vec![
        Fixture {
            state: if waiting { "waiting_for_user" } else { "idle" },
            root_run: Some("run-1"),
            ..thread("thr-1", "ログイン後のリダイレクトを修正", 1_793_799_134_000)
        },
        Fixture {
            pinned: true,
            ..thread("thr-2", "Refactor the storage writer", 1_793_799_000_000)
        },
        Fixture {
            parent: Some("thr-1"),
            ..thread("thr-3", "Try a session-scoped redirect", 1_793_799_120_000)
        },
        Fixture {
            project: "proj-2",
            ..thread("thr-4", "Landing page copy", 1_793_799_030_000)
        },
    ]
}

fn view<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).expect("the fixture is valid")
}

fn message(index: u64, role: &str, text: &str) -> CommittedMessage {
    let tool = (role == "tool").then(|| {
        json!({
            "tool_name": "shell", "call_id": format!("call-{index}"),
            "input_summary": text.lines().next().unwrap_or(""), "status": "succeeded",
        })
    });
    view(json!({
        "thread": "thr-1", "run": "run-1", "index": index.to_string(), "role": role,
        "content": {"type": "inline", "payload": {"text": text}},
        "tool": tool,
    }))
}

fn project(id: &str, name: &str) -> ProjectSummary {
    view(json!({"id": id, "display_name": name, "thread_count": 2, "revision": "1"}))
}

fn detail(fixture: &Fixture) -> ThreadDetail {
    let id = |id: &str| ThreadId::new(id).expect("a fixture id");
    ThreadDetail {
        thread: id(fixture.id),
        project: ProjectId::new(fixture.project).expect("a fixture id"),
        title: fixture.title.into(),
        family: Some(id(fixture.parent.unwrap_or(fixture.id))),
        parent_thread: fixture.parent.map(id),
        lineage: fixture.parent.map(|_| LineageType::Fork),
        root_run: fixture
            .root_run
            .map(|run| RunId::new(run).expect("a fixture id")),
        active_root: None,
        model_choice: workbench_protocol::commands::ModelChoice::RoleDefault,
        chat_role: None,
        extra_read_dirs: Vec::new(),
        archived: false,
        pinned: fixture.pinned,
        revision: Revision(3),
    }
}

fn populated(attention: bool) -> Shared {
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
    for fixture in threads(attention) {
        let summary: ThreadSummary = view(json!({
            "id": fixture.id, "project": fixture.project, "title": fixture.title,
            "state": fixture.state, "lead_thread": null,
            "revision": "3", "updated_at": fixture.updated.to_string(),
        }));
        cache.threads.insert(fixture.id.into(), summary);
    }
    let phase = if attention {
        "waiting_for_user"
    } else {
        "done"
    };
    let run: RunSummary = view(json!({
        "id": "run-1", "thread": "thr-1", "role": "orchestrator", "phase": phase, "revision": "4",
    }));
    cache.runs.insert("run-1".into(), run);
    let closing = if attention {
        "`next` を使うように変更しました。テストを実行してよいか確認してください。"
    } else {
        "`next` を使うように変更し、テストもすべて成功しました。"
    };
    cache.history.insert(
        "thr-1".into(),
        vec![
            message(0, "user", "ログイン後のページが、`next` を指定しても `/home` になります。直してください。"),
            message(
                1,
                "assistant",
                "原因を調べます。次の2つのファイルを確認します。\n\n- `src/auth/login.rs`\n- `src/auth/session.rs`\n\n```rust\nfn after_login(next: Option<&str>) -> Redirect {\n    redirect_to(next.unwrap_or(\"/home\"))\n}\n```",
            ),
            message(2, "tool", "exit code: 0\nsrc/auth/login.rs:42: redirect_to(\"/home\")"),
            message(3, "tool", "--- a/src/auth/login.rs\n+++ b/src/auth/login.rs\n-redirect_to(\"/home\")\n+redirect_to(next.unwrap_or(\"/home\"))"),
            message(4, "assistant", closing),
        ],
    );
    if attention {
        let question: QuestionView = view(json!({
            "id": "q-1",
            "requester": {"run": "run-1", "thread": "thr-1", "root_run": "run-1"},
            "target": {"type": "user"},
            "management_revision": "1", "routing_revision": "1", "blocking": true,
            "title": "不正な `next` は /home と / のどちらへ戻しますか。",
            "options": ["/home", "/"],
            "revision": "2",
            "raised_by": {"actor": "agent", "agent_run_id": "run-1", "surface": "none"},
        }));
        cache.questions.insert("q-1".into(), question);
        let approval: ApprovalView = view(json!({
            "id": "ap-1", "core_epoch": "7", "revision": "2", "thread": "thr-1", "run": "run-1",
            "tool_name": "shell", "summary": "cargo test -p auth", "summary_truncated": false,
            "input_bytes": "22", "state": "pending",
        }));
        cache.approvals.insert("ap-1".into(), approval);
    }
    shared
}

fn answer(query: &WorkspaceQuery, attention: bool) -> Result<WorkspaceReply, String> {
    fn page<T>(items: Vec<T>) -> Page<T> {
        Page {
            items,
            next_cursor: None,
            snapshot_revision: Revision(1),
        }
    }
    Ok(match query {
        WorkspaceQuery::ListFamilies(_) => {
            let fixtures = threads(attention);
            let families = fixtures
                .iter()
                .filter(|t| t.parent.is_none())
                .map(|root| FamilyView {
                    key: ThreadId::new(root.id).expect("a fixture id"),
                    membership_epoch: 0,
                    members: fixtures
                        .iter()
                        .filter(|t| t.parent.unwrap_or(t.id) == root.id)
                        .count() as u32,
                    archived: false,
                    archived_at: None,
                    archive_revision: Revision(1),
                    pinned: root.pinned,
                    pin_revision: Revision(1),
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
            let fixture = threads(attention)
                .into_iter()
                .find(|t| t.id == target.thread.as_str())
                .ok_or_else(|| format!("no fixture thread {}", target.thread.as_str()))?;
            WorkspaceReply::ThreadDetail(Box::new(detail(&fixture)))
        }
        WorkspaceQuery::GetProjectAccess(target) => {
            let (id, name, root) = PROJECTS
                .iter()
                .find(|(id, ..)| *id == target.project.as_str())
                .ok_or_else(|| format!("no fixture project {}", target.project.as_str()))?;
            WorkspaceReply::ProjectAccess(Box::new(ProjectAccessView {
                project: ProjectId::new(*id).expect("a fixture id"),
                name: (*name).into(),
                root: (*root).into(),
                trust: TrustLevel::Approved,
                allowed_directories: Vec::new(),
                revision: Revision(1),
            }))
        }
        other => {
            return Err(format!(
                "the fixture does not answer the workspace query {other:?}"
            ));
        }
    })
}

const DOCUMENT: &str = r#"version = 3
[providers.local]
type = "openai-compatible"
base_url = "http://localhost:11434/v1"
api_key_env = "LOCAL_KEY"
models = ["qwen3-coder", "gpt-oss-20b"]
default_model = "qwen3-coder"
"#;

fn provider(
    name: &str,
    kind: ProviderKind,
    models: &[&str],
    credential: CredentialRef,
) -> ProviderView {
    let quota = matches!(
        kind,
        ProviderKind::OpenaiCodex
            | ProviderKind::AnthropicSubscription
            | ProviderKind::Cursor
            | ProviderKind::KimiSubscription
    );
    ProviderView {
        name: name.into(),
        kind,
        base_url: String::new(),
        credential,
        models: models
            .iter()
            .map(|id| ModelView {
                id: (*id).into(),
                enabled: true,
                kind: ModelKind::Chat,
                context_override: None,
            })
            .collect(),
        excluded_models: vec![],
        default_model: models[0].into(),
        show_quota: quota,
        quota_supported: quota,
    }
}

/// What the core says of the credentials of the three providers of the settings fixture.
fn credential_status() -> workbench_protocol::namespaces::auth::AuthStatus {
    use workbench_protocol::namespaces::auth::{
        AuthStatus, CredentialSlot, CredentialState, LoginMethod,
    };
    let keyring = |account: &str| CredentialRef::Keyring {
        service: "evorch".into(),
        account: account.into(),
    };
    let signed_in = CredentialState::Present {
        expires_at_unix: None,
    };
    AuthStatus {
        revision: Revision(1),
        slots: vec![
            CredentialSlot {
                profile: "local".into(),
                kind: ProviderKind::OpenaiCompatible,
                storage: CredentialRef::Env {
                    var: "LOCAL_KEY".into(),
                },
                state: CredentialState::Missing,
                login: None,
            },
            CredentialSlot {
                profile: "codex".into(),
                kind: ProviderKind::OpenaiCodex,
                storage: keyring("codex"),
                state: signed_in,
                login: Some(LoginMethod::Codex),
            },
            CredentialSlot {
                profile: "claude".into(),
                kind: ProviderKind::AnthropicSubscription,
                storage: keyring("claude"),
                state: signed_in,
                login: Some(LoginMethod::Claude),
            },
        ],
    }
}

fn settings_snapshot() -> SettingsSnapshot {
    let revision = SettingsRevision::new("a".repeat(64)).expect("a fixture revision");
    let keyring = |account: &str| CredentialRef::Keyring {
        service: "evorch".into(),
        account: account.into(),
    };
    SettingsSnapshot {
        revision: revision.clone(),
        applied_revision: Some(revision),
        apply_failure: None,
        document: DOCUMENT.into(),
        providers: vec![
            ProviderView {
                base_url: "http://localhost:11434/v1".into(),
                ..provider(
                    "local",
                    ProviderKind::OpenaiCompatible,
                    &["qwen3-coder", "gpt-oss-20b"],
                    CredentialRef::Env {
                        var: "LOCAL_KEY".into(),
                    },
                )
            },
            provider(
                "codex",
                ProviderKind::OpenaiCodex,
                &["gpt-5.3-codex", "gpt-5.2"],
                keyring("codex"),
            ),
            provider(
                "claude",
                ProviderKind::AnthropicSubscription,
                &["claude-sonnet-5-5"],
                keyring("claude"),
            ),
        ],
        problem: None,
    }
}

// ---------------------------------------------------------------- the screen

pub struct Built {
    harness: Harness<'static, CoreClientScreen>,
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Receiver<Action>,
    attention: bool,
    held_queries: usize,
}

fn action_name(action: &Action) -> String {
    match action {
        Action::Command { .. } => "command".into(),
        Action::History(thread, run, older) => {
            format!("history of {} {run:?} older={older}", thread.as_str())
        }
        Action::Body(_) => "body".into(),
        Action::ApprovalInput(_) => "approval input".into(),
        Action::Upload { .. } => "upload".into(),
        Action::Feature(action) => format!("{:?} request", action.feature),
        Action::Data(_) => "data".into(),
        Action::Reconnect => "reconnect".into(),
        Action::Shutdown => "shutdown".into(),
    }
}

impl Built {
    fn new(attention: bool) -> Self {
        let (tx, actions) = tokio::sync::mpsc::channel(256);
        let shared = Arc::new(Mutex::new(populated(attention)));
        let local = LocalPanes::new(Arc::new(gui::diff::FixtureDiffSource::empty()), None)
            .with_home("/fixture/home".into());
        let screen = CoreClientScreen::new(
            shared.clone(),
            tx,
            Some(ProjectId::new("proj-1").expect("a fixture id")),
            Some(ThreadId::new("thr-1").expect("a fixture id")),
        )
        .with_local_panes(local);
        let harness = Harness::builder()
            .with_size(egui::vec2(WIDTH, HEIGHT))
            .with_pixels_per_point(DPI)
            .with_max_steps(MAX_FRAMES)
            .build_ui_state(|ui, screen: &mut CoreClientScreen| screen.ui(ui), screen);
        gui::theme::style::install_preset(&harness.ctx, gui::theme::style::ThemePreset::Graphite);
        Self {
            harness,
            shared,
            actions,
            attention,
            held_queries: 0,
        }
    }

    /// Runs frames and answers what the screen asks from the fixture until it asks nothing
    /// more and the workspace feature has what it needs.
    fn settle(&mut self) -> Result<(), String> {
        let mut unknown: Vec<String> = Vec::new();
        for _ in 0..16 {
            self.harness.run_steps(1);
            let mut answered = false;
            while let Ok(action) = self.actions.try_recv() {
                let Action::Feature(FeatureAction {
                    feature,
                    tag,
                    request,
                }) = action
                else {
                    unknown.push(action_name(&action));
                    continue;
                };
                if let FeatureRequest::Command(workbench_protocol::commands::Command::Auth(
                    workbench_protocol::namespaces::auth::AuthCommand::RefreshCredentials(refresh),
                )) = &request
                {
                    // The Providers section has the core read the providers again when it opens.
                    let ready = workbench_protocol::namespaces::auth::AuthOperationView {
                        operation: workbench_protocol::operation::OperationView {
                            operation_id: refresh.operation_id.clone(),
                            state: workbench_protocol::operation::OperationState::Ready,
                            revision: Revision(1),
                            result: None,
                        },
                        kind:
                            workbench_protocol::namespaces::auth::AuthOperationKind::RefreshStatus,
                        profile: None,
                        message: None,
                    };
                    self.shared
                        .lock()
                        .map_err(|_| "the shared state is poisoned".to_owned())?
                        .features
                        .reply(
                            feature,
                            tag,
                            FeatureReply::Command(Ok(
                                workbench_protocol::commands::CommandReply::with_result(
                                    workbench_protocol::commands::CommandResult::Auth(
                                        workbench_protocol::namespaces::auth::AuthResult::Operation(
                                            ready,
                                        ),
                                    ),
                                ),
                            )),
                        );
                    answered = true;
                    continue;
                }
                let reply = match (feature, request) {
                    (FeatureId::Workspace, FeatureRequest::Query(Query::Workspace(query))) => {
                        QueryReply::Workspace(answer(&query, self.attention)?)
                    }
                    (
                        _,
                        FeatureRequest::Query(Query::Settings(
                            SettingsQuery::GetSettingsSnapshot {},
                        )),
                    ) => {
                        QueryReply::Settings(SettingsReply::Settings(Box::new(settings_snapshot())))
                    }
                    (
                        _,
                        FeatureRequest::Query(Query::Auth(
                            workbench_protocol::namespaces::auth::AuthQuery::GetCredentialStatus {},
                        )),
                    ) => QueryReply::Auth(workbench_protocol::namespaces::auth::AuthReply::Status(
                        credential_status(),
                    )),
                    (
                        _,
                        FeatureRequest::Query(Query::Settings(SettingsQuery::ModelChoices {
                            project,
                        })),
                    ) => {
                        use workbench_protocol::namespaces::settings::{
                            ProjectModelChoices, RoleModelChoice,
                        };
                        let role = RoleModelChoice {
                            model: "fixture/default".into(),
                            reasoning_effort: None,
                        };
                        QueryReply::Settings(SettingsReply::ModelChoices(ProjectModelChoices {
                            project,
                            profiles: Vec::new(),
                            worker: role.clone(),
                            orchestrator: role,
                        }))
                    }
                    (
                        FeatureId::Execution,
                        FeatureRequest::Query(Query::Execution(
                            ExecutionQuery::GetThreadExecution(target),
                        )),
                    ) => QueryReply::Execution(ExecutionReply::ThreadExecution(Box::new(
                        ThreadExecution {
                            thread: target.thread,
                            root_run: Some(RunId::new("run-1").expect("a fixture id")),
                            goal: None,
                            ledger: None,
                        },
                    ))),
                    (
                        FeatureId::Execution,
                        FeatureRequest::Query(Query::Execution(
                            ExecutionQuery::GetConversationPlan(target),
                        )),
                    ) => QueryReply::Execution(ExecutionReply::ConversationPlan(Box::new(
                        PlanSummary {
                            thread: target.thread,
                            plan_id: None,
                            revision: Revision(1),
                            progress: PlanProgress::default(),
                            updated_at_unix_ms: None,
                        },
                    ))),
                    (
                        FeatureId::Execution,
                        FeatureRequest::Query(Query::Execution(ExecutionQuery::ListRunDetails(_))),
                    ) => QueryReply::Execution(ExecutionReply::RunDetails(Page {
                        items: Vec::new(),
                        next_cursor: None,
                        snapshot_revision: Revision(1),
                    })),
                    // The sidebar looks the lanes of each row up by itself; the fixture has none
                    // and leaves the question open, as the screen's own tests do.
                    (FeatureId::Lanes, FeatureRequest::Query(_)) => {
                        self.held_queries += 1;
                        continue;
                    }
                    (_, FeatureRequest::Query(Query::Artifacts(_))) => {
                        QueryReply::Artifacts(ArtifactsReply::List {
                            artifacts: Vec::new(),
                            next: None,
                        })
                    }
                    // Features of panes these screens do not show; their first reads stay open too.
                    (_, FeatureRequest::Query(Query::Diagnostics(_) | Query::PullRequests(_))) => {
                        self.held_queries += 1;
                        continue;
                    }
                    (feature, request) => {
                        unknown.push(format!("{feature:?} {request:?}"));
                        continue;
                    }
                };
                self.shared
                    .lock()
                    .map_err(|_| "the shared state is poisoned".to_owned())?
                    .features
                    .reply(feature, tag, FeatureReply::Query(Ok(reply)));
                answered = true;
            }
            let settled = self
                .shared
                .lock()
                .map_err(|_| "the shared state is poisoned".to_owned())?
                .features
                .workspace()
                .is_some_and(|workspace| workspace.settled());
            if !answered && settled {
                return if unknown.is_empty() {
                    Ok(())
                } else {
                    unknown.sort();
                    unknown.dedup();
                    Err(format!(
                        "the screen made requests the fixture does not answer:\n  {}",
                        unknown.join("\n  ")
                    ))
                };
            }
        }
        Err("the workspace feature kept asking".into())
    }

    fn open_model_settings(&mut self) -> Result<(), String> {
        self.harness.get_by_label("⚙").click();
        self.harness.run_steps(2);
        match self.harness.query_by_label("Model Settings") {
            Some(item) => item.click(),
            None => self.harness.get_by_label_contains("Model Settings").click(),
        }
        self.harness.run_steps(2);
        self.settle()?;
        // The modal sizes itself over its first frames before it is complete.
        self.harness.run_steps(8);
        Ok(())
    }

    /// Every text and value the screen exposes to accessibility tools.
    pub fn texts(&self) -> Vec<String> {
        let found = std::cell::RefCell::new(Vec::new());
        let _ = self
            .harness
            .query_all_by(|node| {
                let mut found = found.borrow_mut();
                found.extend(node.label().map(|s| s.to_string()));
                found.extend(node.value().map(|s| s.to_string()));
                false
            })
            .count();
        let mut texts = found.into_inner();
        texts.sort();
        texts.dedup();
        texts
    }

    /// The screen is ready when it shows what the scenario is about, nothing it drew carries
    /// a path or setting of this process, and no frame asks to be drawn again at once.
    pub fn check_ready(&mut self, spec: &ScenarioSpec, forbidden: &[String]) -> Result<(), String> {
        self.harness
            .try_run()
            .map_err(|error| format!("{} keeps asking to be redrawn: {error}", spec.id))?;
        let texts = self.texts();
        for label in spec.required {
            if !texts.iter().any(|text| text == label) {
                return Err(format!("{} does not show {label:?}", spec.id));
            }
        }
        if let Some((text, needle)) = super::environment::find_leak(&texts, forbidden) {
            return Err(format!(
                "{} draws {text:?}, which contains {needle:?} of this process",
                spec.id
            ));
        }
        Ok(())
    }

    pub fn render(&mut self) -> Result<RgbaImage, String> {
        self.harness.render()
    }
}

/// The screen of `id`, settled and ready to be drawn or inspected.
pub fn build(id: &str, forbidden: &[String]) -> Result<Built, String> {
    let spec = SCENARIOS
        .iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| format!("unknown scenario {id:?}"))?;
    let mut built = Built::new(id == "attention");
    built.settle()?;
    if id == "settings-models" {
        built.open_model_settings()?;
    }
    // No pointer: no hover highlight, no tooltip and no cursor drawn into the picture.
    built.harness.event(egui::Event::PointerGone);
    built.harness.run_steps(2);
    built.check_ready(spec, forbidden)?;
    Ok(built)
}
