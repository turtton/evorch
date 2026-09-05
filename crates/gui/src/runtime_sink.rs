//! goal 投入を runtime の background run 起動 + GoalSupervisor へ接続する
//! production CommandSink (issue #71, #73)。

// allow: SIZE_OK - RuntimeCommandSink 本体に、pinned された 9 件の振る舞いテスト
// (stub モデル込み) が inline テスト慣習どおり同居するため分割不可能。
// テストを別ファイルへ分離すると impl+test ペアリング規約に反する。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use event_bus::{ApprovalDecision, GoalReference};
use runtime::orchestration::supervisor::SupervisorError;
use runtime::{AgentRuntime, GoalSpec, RunConfig, SupervisorHandle};

use crate::model::commands::{
    CommandSink, GoalSubmission, LoopEvent, MergeDecision, ReferenceKind, WorkbenchCommand,
};

/// storage bridge と [`GoalSpec::session_id`] で共有する永続化セッション ID。
///
/// 固定値にすることで、再起動後の `Database::agent_messages_by_session` が
/// 前セッションの transcript を引き続き復元できる。
pub const STORAGE_SESSION_ID: &str = "evorch-gui";

/// token なし DecideMerge を拒否する理由。
const MISSING_TOKEN_REASON: &str =
    "merge decision requires an approval token issued by MergeApprovalRequested";

/// goal の配送先リポジトリ識別子。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoIdentity {
    repo: String,
    base_ref: String,
}

/// goal 投入を runtime の background run 起動へ接続する production CommandSink。
///
/// SubmitGoal ごとに goal-N を採番し、entry pre-routing (EntryRouter) で判定した
/// role (Direct→Worker / Coordinated→Orchestrator) の background run を起動し、
/// その root run に紐付けて supervisor へ goal を登録する (issue #71, #73)。
/// DecideMerge / PauseGoal / ResumeGoal / CancelGoal は supervisor へ転送する。
pub struct RuntimeCommandSink {
    runtime: AgentRuntime,
    handle: tokio::runtime::Handle,
    supervisor: SupervisorHandle,
    accepted_goals: u64,
    repo_identity: OnceLock<RepoIdentity>,
}

impl RuntimeCommandSink {
    /// runtime, tokio ハンドル, supervisor handle から sink を生成する。
    pub fn new(
        runtime: AgentRuntime,
        handle: tokio::runtime::Handle,
        supervisor: SupervisorHandle,
    ) -> Self {
        Self {
            runtime,
            handle,
            supervisor,
            accepted_goals: 0,
            repo_identity: OnceLock::new(),
        }
    }

    /// goal の配送先リポジトリ識別子を初回提出時に 1 度だけ解決する。
    fn repo_identity(&self) -> &RepoIdentity {
        self.repo_identity.get_or_init(|| {
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            RepoIdentity {
                repo: derive_repo_slug(&root),
                base_ref: derive_base_ref(&root),
            }
        })
    }

    fn route_goal_command(
        &mut self,
        route: impl FnOnce(&SupervisorHandle) -> Result<(), SupervisorError>,
    ) -> Vec<LoopEvent> {
        match route(&self.supervisor) {
            Ok(()) => Vec::new(),
            Err(error) => vec![LoopEvent::CommandRejected {
                reason: error.to_string(),
            }],
        }
    }
}

impl CommandSink for RuntimeCommandSink {
    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        match command {
            WorkbenchCommand::SubmitGoal(submission) => {
                self.accepted_goals = self.accepted_goals.saturating_add(1);
                let goal_id = format!("goal-{}", self.accepted_goals);
                let prompt = render_entry_prompt(&submission);
                let runtime = self.runtime.clone();
                let supervisor = self.supervisor.clone();
                let goal_for_log = submission.goal.clone();
                let thread_id = submission.thread_id.clone();
                let goal_id_for_run = goal_id.clone();
                let spec = GoalSpec {
                    session_id: STORAGE_SESSION_ID.to_owned(),
                    project_id: submission.project_id,
                    thread_id: submission.thread_id,
                    goal: submission.goal,
                    references: submission
                        .references
                        .iter()
                        .map(|reference| GoalReference {
                            kind: reference_kind_label(&reference.kind).to_owned(),
                            value: reference.value.clone(),
                        })
                        .collect(),
                    constraints: submission.constraints,
                    repo: self.repo_identity().repo.clone(),
                    base_ref: self.repo_identity().base_ref.clone(),
                };
                self.handle.spawn(async move {
                    let decision = runtime.entry_router().classify(&goal_for_log).await;
                    let root_run = runtime.delegate_background(
                        decision.role(),
                        prompt,
                        RunConfig {
                            name: Some(goal_id_for_run),
                            ..RunConfig::default()
                        },
                    );
                    supervisor.create_goal(spec, root_run);
                });
                vec![LoopEvent::GoalAccepted { thread_id, goal_id }]
            }
            WorkbenchCommand::DecideMerge(command) => {
                let Some(token_id) = command.token_id.clone() else {
                    return vec![LoopEvent::CommandRejected {
                        reason: MISSING_TOKEN_REASON.to_owned(),
                    }];
                };
                let decision = supervisor_decision(command.decision.clone());
                match self.supervisor.decide_merge(token_id, decision) {
                    Ok(()) => vec![LoopEvent::MergeResolved {
                        thread_id: command.thread_id,
                        decision: command.decision,
                    }],
                    Err(error) => vec![LoopEvent::CommandRejected {
                        reason: error.to_string(),
                    }],
                }
            }
            WorkbenchCommand::PauseGoal { goal_id } => {
                self.route_goal_command(|supervisor| supervisor.pause(&goal_id))
            }
            WorkbenchCommand::ResumeGoal { goal_id } => {
                self.route_goal_command(|supervisor| supervisor.resume(&goal_id))
            }
            WorkbenchCommand::CancelGoal { goal_id } => {
                self.route_goal_command(|supervisor| supervisor.cancel(&goal_id))
            }
        }
    }
}

/// GUI 側の merge 判断を supervisor の承認判断へ写像する。
fn supervisor_decision(decision: MergeDecision) -> ApprovalDecision {
    match decision {
        MergeDecision::Approve => ApprovalDecision::Approved,
        MergeDecision::Reject { reason } => ApprovalDecision::Rejected { reason },
    }
}

/// GoalSubmission を background run へ渡す entry prompt として整形する。
///
/// goal 本文を先頭に置き、references / constraints は空でない場合のみ
/// `References:` / `Constraints:` セクションとして 1 行 1 項目で続ける。
/// 分類 (`EntryRouter::classify`) は goal 本文のみを受け、references /
/// constraints は起動される run の prompt 側にのみ載る。
pub fn render_entry_prompt(submission: &GoalSubmission) -> String {
    let mut prompt = submission.goal.clone();
    if !submission.references.is_empty() {
        prompt.push_str("\n\nReferences:\n");
        let lines: Vec<String> = submission
            .references
            .iter()
            .map(|reference| {
                let kind_label = reference_kind_label(&reference.kind);
                format!("- {kind_label}: {}", reference.value)
            })
            .collect();
        prompt.push_str(&lines.join("\n"));
    }
    if !submission.constraints.is_empty() {
        prompt.push_str("\n\nConstraints:\n");
        let lines: Vec<String> = submission
            .constraints
            .iter()
            .map(|constraint| format!("- {constraint}"))
            .collect();
        prompt.push_str(&lines.join("\n"));
    }
    prompt
}

/// 参照元種別のラベル。
fn reference_kind_label(kind: &ReferenceKind) -> &'static str {
    match kind {
        ReferenceKind::Packet => "packet",
        ReferenceKind::Issue => "issue",
    }
}

/// `git remote get-url origin` から `owner/name` 形式のリポジトリ識別子を
/// 解決する。取得に失敗した場合はリポジトリディレクトリ名へ fallback する。
pub fn derive_repo_slug(repo_root: &Path) -> String {
    git_output(repo_root, ["remote", "get-url", "origin"])
        .as_deref()
        .and_then(parse_remote_slug)
        .unwrap_or_else(|| fallback_slug(repo_root))
}

/// `git symbolic-ref --short refs/remotes/origin/HEAD` からマージ先ブランチを
/// 解決する。取得に失敗した場合は `main` へ fallback する。
pub fn derive_base_ref(repo_root: &Path) -> String {
    git_output(
        repo_root,
        ["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .as_deref()
    .and_then(|output| output.trim().strip_prefix("origin/").map(str::to_owned))
    .unwrap_or_else(|| String::from("main"))
}

fn git_output(repo_root: &Path, args: [&str; 3]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// origin URL (https / ssh / scp-like) から `owner/name` を抽出する。
fn parse_remote_slug(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches('/');
    let without_suffix = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    if let Some(rest) = without_suffix.strip_prefix("git@") {
        let (_host, path) = rest.split_once(':')?;
        return slug_from_path(path);
    }
    if let Some((_scheme, remainder)) = without_suffix.split_once("://") {
        let path = remainder.split_once('/').map(|(_, path)| path)?;
        return slug_from_path(path);
    }
    slug_from_path(without_suffix)
}

fn slug_from_path(path: &str) -> Option<String> {
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let owner = segments.next()?;
    let name = segments.next()?;
    segments.next().is_none().then(|| format!("{owner}/{name}"))
}

fn fallback_slug(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("unknown"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use event_bus::{EventBus, EventKind, GoalReference, GoalState, OrchestratorEvent};
    use providers::{ChatResponse, Message, ToolSpec};
    use runtime::{
        AgentInvocationContext, AgentModel, AgentRuntime, AgentSummary, FixtureDeliveryAdapter,
        GoalSpec, GoalSupervisor, OrchestrationSettings, Role, RunConfig, RuntimeError,
        SupervisorHandle,
    };
    use storage::{Database, Storage, StorageConfig, StorageHandle};
    use tools::ToolExecutor;

    use super::{RuntimeCommandSink, STORAGE_SESSION_ID, render_entry_prompt};
    use crate::model::commands::{
        CommandSink, GoalSubmission, LoopEvent, MergeCommand, MergeDecision, PacketReference,
        ReferenceKind, WorkbenchCommand,
    };

    /// どんなプロンプトにも応答せず run を走らせ続けるテスト用 stub モデル。
    ///
    /// supervisor を接続すると run の terminal 遷移が continuation / delivery
    /// 起動に繋がるため、行アサーションとの競合を避べるよう run を終端させない。
    struct HeldModel;

    #[async_trait]
    impl AgentModel for HeldModel {
        async fn complete(
            &self,
            _invocation: &AgentInvocationContext,
            _role: Role,
            _messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            std::future::pending().await
        }

        fn selected_model(&self, role: Role) -> String {
            format!("test-{}", role.name().to_lowercase())
        }
    }

    fn submission(
        goal: &str,
        references: Vec<PacketReference>,
        constraints: Vec<String>,
    ) -> GoalSubmission {
        GoalSubmission {
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: goal.into(),
            references,
            constraints,
        }
    }

    fn spec() -> GoalSpec {
        GoalSpec {
            session_id: STORAGE_SESSION_ID.into(),
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: "implement issue 73".into(),
            references: vec![GoalReference {
                kind: "issue".into(),
                value: "73".into(),
            }],
            constraints: Vec::new(),
            repo: "turtton/evorch".into(),
            base_ref: "main".into(),
        }
    }

    /// マルチスレッド tokio runtime 上に実 AgentRuntime + supervisor を接続した
    /// sink を組み立てる。
    fn build_sink() -> (
        tokio::runtime::Runtime,
        RuntimeCommandSink,
        AgentRuntime,
        SupervisorHandle,
    ) {
        let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
        let bus = Arc::new(EventBus::new(64));
        let executor = Arc::new(ToolExecutor::new(bus.clone()));
        let runtime = AgentRuntime::new(Arc::clone(&bus), executor, Arc::new(HeldModel));
        let supervisor = rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                bus,
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings::default(),
            )
        });
        let sink =
            RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor.clone());
        (rt, sink, runtime, supervisor)
    }

    /// storage bridge をテスト用に起動する (本番と同一の session ID で永続化する)。
    fn spawn_test_bridge(
        rt: &tokio::runtime::Runtime,
        bus: Arc<EventBus>,
        handle: StorageHandle,
    ) -> tokio::task::JoinHandle<()> {
        let mut subscriber = bus.subscribe();
        rt.spawn(async move {
            loop {
                match subscriber.recv().await {
                    Ok(event) => {
                        let _ = handle.append_event(Some(STORAGE_SESSION_ID), &event);
                    }
                    Err(_) => return,
                }
            }
        })
    }

    /// predicate を満たす agent 行が現れるまで 50ms 間隔で最大 5 秒待つ。
    fn wait_for_agents(
        runtime: &AgentRuntime,
        predicate: impl Fn(&AgentSummary) -> bool,
    ) -> Vec<AgentSummary> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let agents = runtime.list_agents();
            if agents.iter().any(&predicate) {
                return agents;
            }
            assert!(
                Instant::now() < deadline,
                "agent row did not appear within 5s: {:?}",
                runtime.list_agents()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 永続化済みイベント列に最初の GoalCreated が現れるまで待ち、
    /// (goal_id, root_run_id) を返す。
    fn wait_for_persisted_goal_created(config: &StorageConfig) -> (String, String) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < deadline,
                "durable GoalCreated did not appear within 5s"
            );
            let database = Database::open(config).expect("reader を開ける");
            let events = database.events_all_ordered().expect("events を読める");
            drop(database);
            for stored in &events {
                if let EventKind::Orchestrator(OrchestratorEvent::GoalCreated {
                    goal_id,
                    root_run_id,
                    ..
                }) = &stored.event.kind
                {
                    return (goal_id.clone(), root_run_id.clone());
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 指定 goal の状態が期待値へ遷移するまで 50ms 間隔で最大 5 秒待つ。
    fn wait_for_goal_state(supervisor: &SupervisorHandle, goal_id: &str, expected: GoalState) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < deadline,
                "goal {goal_id} did not reach {expected:?} within 5s"
            );
            if let Some(snapshot) = supervisor.snapshot(goal_id) {
                if snapshot.state == expected {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Given: references も constraints も空の GoalSubmission
    // When: render_entry_prompt する
    // Then: goal 本文そのものが返る
    #[test]
    fn render_entry_prompt_is_goal_only_when_no_references_or_constraints() {
        let input = submission("fix the typo in README", Vec::new(), Vec::new());

        let prompt = render_entry_prompt(&input);

        assert_eq!(prompt, "fix the typo in README");
    }

    // Given: references 2 件・constraints 1 件の GoalSubmission
    // When: render_entry_prompt する
    // Then: goal の後に References: / Constraints: セクションが順に付く
    #[test]
    fn render_entry_prompt_appends_reference_and_constraint_sections() {
        let input = submission(
            "implement issue #65",
            vec![
                PacketReference {
                    kind: ReferenceKind::Packet,
                    value: "v02-entry-pre-routing".into(),
                },
                PacketReference {
                    kind: ReferenceKind::Issue,
                    value: "65".into(),
                },
            ],
            vec!["model only".into()],
        );

        let prompt = render_entry_prompt(&input);

        assert_eq!(
            prompt,
            "implement issue #65\n\nReferences:\n- packet: v02-entry-pre-routing\n- issue: 65\n\nConstraints:\n- model only"
        );
    }

    // Given: origin URL 由来のリポジトリ識別子が必要なとき
    // When: parse_remote_slug する
    // Then: https / ssh 両形式から owner/name を抽出し、解釈不能なら None を返す
    #[test]
    fn parse_remote_slug_extracts_owner_and_name() {
        assert_eq!(
            super::parse_remote_slug("https://github.com/turtton/evorch.git").as_deref(),
            Some("turtton/evorch")
        );
        assert_eq!(
            super::parse_remote_slug("git@github.com:turtton/evorch.git").as_deref(),
            Some("turtton/evorch")
        );
        assert_eq!(
            super::parse_remote_slug("ssh://git@github.com/turtton/evorch").as_deref(),
            Some("turtton/evorch")
        );
        assert_eq!(super::parse_remote_slug("not a remote"), None);
        assert_eq!(super::parse_remote_slug("https://github.com/only"), None);
    }

    // Given: 実 runtime を接続した sink
    // When: SubmitGoal を 2 回 submit する
    // Then: goal-1 / goal-2 の GoalAccepted がそれぞれちょうど 1 件ずつ返る
    #[test]
    fn submit_goal_returns_goal_accepted_with_sequential_ids_and_nothing_else() {
        let (_rt, mut sink, _runtime, _supervisor) = build_sink();

        let first = sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "implement issue #65",
            Vec::new(),
            Vec::new(),
        )));
        let second = sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "direct: fix the typo in README",
            Vec::new(),
            Vec::new(),
        )));

        assert_eq!(
            first,
            vec![LoopEvent::GoalAccepted {
                thread_id: "thread-1".into(),
                goal_id: "goal-1".into(),
            }]
        );
        assert_eq!(
            second,
            vec![LoopEvent::GoalAccepted {
                thread_id: "thread-1".into(),
                goal_id: "goal-2".into(),
            }]
        );
    }

    // Given: storage bridge と supervisor を接続した実 runtime の sink
    // When: SubmitGoal する
    // Then: 永続化された GoalCreated の root_run_id が実在する root run と一致し、
    //       supervisor の ledger にも同じ root が Active 状態で記録される
    #[test]
    fn submit_goal_creates_durable_goal_bound_to_root_run() {
        let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
        let temp = tempfile::TempDir::new().expect("tempdir");
        let storage_config = StorageConfig {
            db_path: temp.path().join("events.db"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(storage_config.clone()).expect("storage を開ける");
        let bus = Arc::new(EventBus::new(256));
        let executor = Arc::new(ToolExecutor::new(Arc::clone(&bus)));
        let runtime = AgentRuntime::new(Arc::clone(&bus), executor, Arc::new(HeldModel));
        let supervisor = rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                Arc::clone(&bus),
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings::default(),
            )
        });
        let mut sink =
            RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor.clone());
        let bridge = spawn_test_bridge(&rt, Arc::clone(&bus), storage.handle());

        let events = sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "direct: durable goal",
            Vec::new(),
            Vec::new(),
        )));

        assert_eq!(
            events,
            vec![LoopEvent::GoalAccepted {
                thread_id: "thread-1".into(),
                goal_id: "goal-1".into(),
            }]
        );

        let (goal_id, root_run_id) = wait_for_persisted_goal_created(&storage_config);
        let root_exists = {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if runtime
                    .list_agents()
                    .iter()
                    .any(|agent| agent.run_id.to_string() == root_run_id)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            runtime
                .list_agents()
                .iter()
                .any(|agent| agent.run_id.to_string() == root_run_id)
        };
        assert!(root_exists, "root run {root_run_id} was not started");
        let snapshot = supervisor
            .snapshot(&goal_id)
            .expect("supervisor knows the persisted goal");
        assert_eq!(snapshot.root_run_id, root_run_id);
        assert_eq!(snapshot.state, GoalState::Active);

        bridge.abort();
        storage.close();
    }

    // Given: 実 runtime を接続した sink
    // When: direct キーワードつき goal を submit する
    // Then: role Worker・名前 goal-1 の run が現れ、Orchestrator run は現れない
    #[test]
    fn direct_goal_starts_a_worker_run_named_after_the_goal_id() {
        let (_rt, mut sink, runtime, _supervisor) = build_sink();

        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "direct: fix the typo in README",
            Vec::new(),
            Vec::new(),
        )));

        let agents = wait_for_agents(&runtime, |agent| {
            agent.role_name == "Worker" && agent.name == "goal-1"
        });
        assert!(!agents.iter().any(|agent| agent.role_name == "Orchestrator"));
    }

    // Given: 実 runtime を接続した sink
    // When: direct キーワードを含まない goal を submit する
    // Then: role Orchestrator・名前 goal-1 の run が現れ、Worker run は現れない
    #[test]
    fn plain_goal_starts_an_orchestrator_run() {
        let (_rt, mut sink, runtime, _supervisor) = build_sink();

        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "implement issue #65",
            Vec::new(),
            Vec::new(),
        )));

        let agents = wait_for_agents(&runtime, |agent| {
            agent.role_name == "Orchestrator" && agent.name == "goal-1"
        });
        assert!(!agents.iter().any(|agent| agent.role_name == "Worker"));
    }

    // Given: supervisor を接続した sink
    // When: token_id なしの DecideMerge を submit する
    // Then: CommandRejected が 1 件返る
    #[test]
    fn decide_merge_without_token_is_rejected() {
        let (_rt, mut sink, _runtime, _supervisor) = build_sink();

        let events = sink.submit(WorkbenchCommand::DecideMerge(MergeCommand {
            thread_id: "thread-1".into(),
            pr: None,
            token_id: None,
            decision: MergeDecision::Approve,
        }));

        assert!(
            matches!(&events[..], [LoopEvent::CommandRejected { reason }] if !reason.is_empty()),
            "unexpected events: {events:?}"
        );
    }

    // Given: 実 runtime を接続した sink
    // When: token なし DecideMerge を submit する
    // Then: CommandRejected{reason} が返り、run も 1 つも起動されない
    #[test]
    fn decide_merge_emits_no_loop_events_and_starts_no_run() {
        let (_rt, mut sink, runtime, _supervisor) = build_sink();

        let events = sink.submit(WorkbenchCommand::DecideMerge(MergeCommand {
            thread_id: "thread-1".into(),
            pr: None,
            token_id: None,
            decision: MergeDecision::Approve,
        }));

        assert!(
            matches!(&events[..], [LoopEvent::CommandRejected { reason }] if !reason.is_empty()),
            "unexpected events: {events:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(runtime.list_agents().is_empty());
    }

    // Given: HeldModel で root run を走らせたままの goal
    // When: PauseGoal / ResumeGoal / CancelGoal を sink 経由で送る
    // Then: いずれも LoopEvent を返さず、goal 状態が順に Paused → Active → Cancelled へ遷移する
    #[test]
    fn pause_resume_cancel_route_to_supervisor() {
        let (rt, mut sink, runtime, supervisor) = build_sink();
        let root = rt.block_on(async {
            runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default())
        });
        let goal_id = supervisor.create_goal(spec(), root);
        wait_for_goal_state(&supervisor, &goal_id, GoalState::Active);

        assert!(
            sink.submit(WorkbenchCommand::PauseGoal {
                goal_id: goal_id.clone(),
            })
            .is_empty()
        );
        wait_for_goal_state(&supervisor, &goal_id, GoalState::Paused);

        assert!(
            sink.submit(WorkbenchCommand::ResumeGoal {
                goal_id: goal_id.clone(),
            })
            .is_empty()
        );
        wait_for_goal_state(&supervisor, &goal_id, GoalState::Active);

        assert!(
            sink.submit(WorkbenchCommand::CancelGoal {
                goal_id: goal_id.clone(),
            })
            .is_empty()
        );
        wait_for_goal_state(&supervisor, &goal_id, GoalState::Cancelled);
    }
}
