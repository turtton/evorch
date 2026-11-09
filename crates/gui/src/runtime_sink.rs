//! Conversation commands and generic thread goals.
//!
//! The command logic lives in the `workbench-host` crate, shared with the
//! protocol ingress (ADR 0045). This module is the desktop adapter: it turns
//! the GUI's `WorkbenchCommand`s into host commands, tags them with desktop
//! provenance, and turns the host's events back into `LoopEvent`s.

// allow: SIZE_OK - RuntimeCommandSink の振る舞いテスト (stub モデル込み) が
// inline テスト慣習どおり同居するため分割不可能。
// テストを別ファイルへ分離すると impl+test ペアリング規約に反する。

use std::path::PathBuf;
use std::sync::Arc;

use event_bus::{Event, EventBus};
use runtime::{AgentRuntime, RunId};
use workbench_host::{CommandHost, commands::CommandService};
use workbench_protocol::envelope::Provenance;

use crate::model::commands::{CommandSink, GoalSubmission, LoopEvent, WorkbenchCommand};

mod host_adapter;
mod lane_host;
mod lane_management;
mod pull_requests;
pub use lane_host::GuiLaneDeliveryHost;
pub use lane_management::GuiLaneManagementHost;

/// Prefix of the stream ids GUI processes persist their events under.
pub const STORAGE_SESSION_ID: &str = "evorch-gui";

/// A stream id for this process: [`STORAGE_SESSION_ID`], the process id and a
/// random part. Processes sharing one database persist under distinct streams,
/// so each recognises and skips its own rows when following the others.
pub fn new_stream_id() -> String {
    let random = uuid::Uuid::new_v4().simple().to_string();
    format!(
        "{STORAGE_SESSION_ID}:{}-{}",
        std::process::id(),
        &random[..8]
    )
}

pub fn finish_chat_start(
    host: &runtime::ownership::OwnerHost,
    thread: &str,
    result: Result<runtime::ownership::OwnerPermit, runtime::ownership::RegistryError>,
) -> Result<runtime::ownership::OwnerPermit, runtime::ownership::RegistryError> {
    match result {
        Err(runtime::ownership::RegistryError::Exists) => host.owned_permit(thread),
        other => other,
    }
}

/// Persistent conversations and generic objectives share this production command sink.
///
/// SubmitGoal joins the normal persistent conversation and creates a generic objective.
pub struct RuntimeCommandSink {
    host: CommandHost,
    /// Attached to every command this window issues.
    provenance: Provenance,
    search_jobs: crate::model::latest_job::LatestJob<SearchJob>,
    /// Pull request operations that run on the Tokio runtime and their refusals.
    pull_request_jobs: pull_requests::Jobs,
}

/// Reply channel, project, query, result limit and continuation cursor of a thread search.
type SearchJob = (
    std::sync::mpsc::Sender<Result<runtime::HostThreadSearch, String>>,
    PathBuf,
    String,
    usize,
    Option<String>,
);

impl RuntimeCommandSink {
    pub fn start_background_run(&self, text: String) -> RunId {
        self.host.service().start_background_run(text)
    }

    /// runtime と tokio ハンドルから sink を生成する。
    pub fn new(runtime: AgentRuntime, handle: tokio::runtime::Handle) -> Self {
        Self {
            host: CommandHost::new(CommandService::new(runtime, handle)),
            provenance: workbench_host::provenance::local_desktop(),
            search_jobs: crate::model::latest_job::LatestJob::default(),
            pull_request_jobs: pull_requests::Jobs::default(),
        }
    }

    /// Installs the runtime's durable lane delivery (ADR 0044 Phase 1c) with this sink's
    /// bindings and ownership as the host. Call it after `with_ownership`,
    /// `with_memory_storage` and `with_team_writer`: the host copies them. Inside the Tokio
    /// handle the runtime's recovery worker starts; without one delivery stays unbound and
    /// questions keep going to the user.
    pub fn with_lane_delivery(self) -> Self {
        self.host.service().bind_lane_delivery();
        self
    }

    pub fn with_web_tools_enabled(mut self, enabled: bool) -> Self {
        self.host.service_mut().set_web_tools_enabled(enabled);
        self
    }

    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.host.service_mut().set_event_bus(event_bus);
        self
    }

    pub fn with_ownership(mut self, ownership: Arc<runtime::ownership::OwnerHost>) -> Self {
        self.host.service_mut().set_ownership(Some(ownership));
        self
    }

    pub fn with_memory_storage(mut self, config: storage::StorageConfig) -> Self {
        self.host.service_mut().set_memory_storage(config);
        self
    }

    pub fn with_team_writer(mut self, writer: storage::StorageHandle) -> Self {
        self.host.service_mut().set_team_writer(writer);
        self
    }
}

impl CommandSink for RuntimeCommandSink {
    fn poll(&mut self) -> Vec<LoopEvent> {
        let mut events: Vec<LoopEvent> = self
            .host
            .service_mut()
            .poll()
            .into_iter()
            .map(host_adapter::loop_event)
            .collect();
        events.extend(self.pull_request_jobs.poll());
        events
    }

    fn has_background_work(&self) -> bool {
        self.host.service().has_background_work() || self.pull_request_jobs.busy()
    }

    fn pull_requests_enabled(&self) -> bool {
        self.pull_requests_on()
    }

    fn has_host_input_receipts(&self) -> bool {
        true
    }

    fn host_input_receipts(
        &self,
        ids: &[String],
    ) -> Option<Result<Vec<storage::HostInputReceipt>, storage::StorageError>> {
        Some(self.host.service().runtime().host_input_receipts(ids))
    }

    fn host_input_origins(
        &self,
        ids: &[String],
    ) -> Option<Result<Vec<storage::HostInputOrigin>, storage::StorageError>> {
        Some(self.host.service().runtime().host_input_origins(ids))
    }

    fn mark_unreachable_host_inputs(
        &self,
        run: &str,
    ) -> Result<Vec<String>, storage::StorageError> {
        if let Ok(run) = run.parse() {
            return self
                .host
                .service()
                .runtime()
                .reconcile_orphaned_host_inputs(run);
        }
        Ok(Vec::new())
    }

    fn withdraw_follow_up(
        &mut self,
        thread: &str,
        prompt_id: &str,
    ) -> Result<(), runtime::RuntimeError> {
        self.host
            .service_mut()
            .withdraw_follow_up(thread, prompt_id)
    }

    fn follow_up_status(&self, thread: &str) -> Option<runtime::FollowUpStatus> {
        self.host.service().follow_up_status(thread)
    }

    fn running_children(&mut self, thread: &str) -> Option<usize> {
        self.host.service_mut().running_children(thread)
    }

    fn observe_lifecycle(&mut self, event: &Event) {
        self.host.service_mut().observe_lifecycle(event);
    }

    fn bind_thread_todo(&mut self, snapshot: &event_bus::ThreadTodoSnapshot) {
        self.host.service_mut().bind_thread_todo(snapshot);
    }

    fn bind_thread_goal(&mut self, snapshot: &event_bus::ThreadGoalSnapshot, project: &str) {
        self.host.service_mut().bind_thread_goal(snapshot, project);
    }

    fn bind_lane_root(&mut self, thread: &str, run: &str) {
        self.host.service_mut().bind_lane_root(thread, run);
    }

    fn bind_goal_context(&mut self, thread: &str, project: &str, run: &str) {
        self.host
            .service_mut()
            .bind_goal_context(thread, project, run);
    }

    fn lane_delivery_bound(&self) -> bool {
        self.host.service().runtime().lane_delivery_binding()
            == event_bus::lane::LaneDeliveryBinding::Bound
    }

    fn schedule_lane_deliveries(&mut self, thread: &str) {
        let _guard = self.host.service().handle().enter();
        self.host
            .service()
            .runtime()
            .schedule_lane_deliveries(thread);
    }

    fn restore_diagnostics(
        &self,
        run: &str,
    ) -> Result<Option<runtime::restore::RunRestoreDiagnostics>, String> {
        let id = run.parse::<RunId>().map_err(|e| e.to_string())?;
        self.host
            .service()
            .runtime()
            .restore_diagnostics(id)
            .map_err(|e| e.to_string())
    }

    fn preview_base_context(
        &self,
        mut request: runtime::base_context::BaseContextRequest,
    ) -> Option<crate::model::commands::ContextPreviewReceiver> {
        // Chat runs capture lessons the same way; a read failure only hides the section.
        request.memory = self
            .host
            .service()
            .capture_memory(request.project_root.as_deref());
        let (sender, receiver) = std::sync::mpsc::channel();
        let runtime = self.host.service().runtime().clone();
        self.host.service().handle().spawn(async move {
            let report = runtime
                .preview_base_context(request)
                .await
                .map_err(|error| error.to_string());
            let _ = sender.send(report);
        });
        Some(receiver)
    }

    fn start_btw(
        &self,
        thread: &str,
        question: String,
    ) -> Result<crate::model::commands::BtwJob, crate::model::commands::BtwStartError> {
        use crate::model::commands::{BtwCancel, BtwJob, BtwStartError};
        let service = self.host.service();
        let run = service
            .bindings
            .root_of(thread)
            .ok_or(BtwStartError::Refused(
                runtime::btw::BtwError::NoLocalContext,
            ))?;
        let snapshot = service
            .runtime()
            .btw_snapshot(run)
            .map_err(BtwStartError::Refused)?;
        let source = snapshot.source();
        let (sender, receiver) = std::sync::mpsc::channel();
        let task = service.handle().spawn(async move {
            let _ = sender.send(snapshot.ask(&question).await);
        });
        let abort = task.abort_handle();
        Ok(BtwJob {
            source,
            receiver,
            cancel: BtwCancel::new(move || abort.abort()),
        })
    }

    fn search_threads(
        &self,
        project: PathBuf,
        query: String,
        limit: usize,
        cursor: Option<String>,
    ) -> Option<crate::model::thread_search::ThreadSearchReceiver> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let runtime = self.host.service().runtime().clone();
        let handle = self.host.service().handle().clone();
        // One storage read at a time, newest request first: a search left over
        // from a closed modal is replaced, and its dropped sender tells the
        // modal it was superseded.
        self.search_jobs.submit(
            (sender, project, query, limit, cursor),
            move |(sender, project, query, limit, cursor)| {
                let found =
                    runtime.search_threads_for_host(&project, &query, limit, cursor.as_deref());
                let _ = sender.send(found);
            },
            move |worker| {
                // Storage reads block, so they stay off the UI and async worker threads.
                handle.spawn_blocking(worker);
            },
        );
        Some(receiver)
    }

    fn run_context_view(
        &self,
        run: &str,
    ) -> Result<Option<runtime::base_context::RunContextView>, String> {
        let id = run.parse::<RunId>().map_err(|e| e.to_string())?;
        self.host
            .service()
            .runtime()
            .run_context_view(id)
            .map_err(|e| e.to_string())
    }

    fn set_default_cwd(&mut self, cwd: Option<PathBuf>) -> Result<(), String> {
        self.host.service_mut().set_default_cwd(cwd)
    }

    fn set_project_trust(&mut self, root: PathBuf, trusted: bool) {
        self.host.service().runtime().set_project_trust(
            root,
            if trusted {
                runtime::ProjectTrust::Approved
            } else {
                runtime::ProjectTrust::Unapproved
            },
        );
    }

    fn set_project_allowed_directories(&mut self, root: PathBuf, directories: Vec<PathBuf>) {
        self.host
            .service()
            .runtime()
            .set_project_allowed_directories(root, directories);
    }

    fn set_thread_read_only_directories(&mut self, thread_id: String, directories: Vec<PathBuf>) {
        self.host
            .service()
            .runtime()
            .set_thread_read_only_directories(&thread_id, directories);
    }

    fn start_background_run(&self, text: String) -> Option<RunId> {
        Some(RuntimeCommandSink::start_background_run(self, text))
    }

    fn submit_chat_with_permit(
        &mut self,
        chat: crate::model::commands::ChatSubmission,
        permit: runtime::ownership::OwnerPermit,
    ) -> Vec<LoopEvent> {
        self.host
            .submit_local_chat_with_permit(host_adapter::chat_input(chat), permit, &self.provenance)
            .into_iter()
            .map(host_adapter::loop_event)
            .collect()
    }

    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        if command.is_pull_request() {
            // Pull request operations are not host commands: they act through the runtime's
            // pull request service, but still under the thread's ownership permit.
            let permit = match self.host.service().permit_for_thread(command.thread()) {
                Ok(permit) => permit,
                Err(error) => {
                    return vec![LoopEvent::CommandRejected {
                        reason: error.to_string(),
                    }];
                }
            };
            return self.pull_request_command(command, permit);
        }
        self.host
            .submit_local(host_adapter::host_command(command), &self.provenance)
            .into_iter()
            .map(host_adapter::loop_event)
            .collect()
    }
}

/// GoalSubmission を background run へ渡す entry prompt として整形する。
///
/// goal 本文を先頭に置き、references / constraints は空でない場合のみ
/// `References:` / `Constraints:` セクションとして 1 行 1 項目で続ける。
/// 参照と制約は通常会話へ渡す文脈であり、PR や固定フローの起動条件ではない。
pub fn render_entry_prompt(submission: &GoalSubmission) -> String {
    workbench_host::commands::render_entry_prompt(&host_adapter::goal_input(submission.clone()))
}

/// The `owner/name` label of a project (see [`runtime::repository::derive_repo_slug`]).
pub use runtime::repository::derive_repo_slug;

#[cfg(test)]
impl RuntimeCommandSink {
    fn submit_authorized(
        &mut self,
        command: WorkbenchCommand,
        permit: Option<runtime::ownership::OwnerPermit>,
    ) -> Vec<LoopEvent> {
        let provenance = self.provenance.clone();
        self.host
            .submit_local_authorized(host_adapter::host_command(command), permit, &provenance)
            .into_iter()
            .map(host_adapter::loop_event)
            .collect()
    }

    fn can_restart_stopped(&self, thread: &str, error: &runtime::RuntimeError) -> bool {
        self.host.service().can_restart_stopped(thread, error)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use event_bus::{EventBus, EventKind, OrchestratorEvent};
    use providers::{ChatResponse, Message, ToolSpec};
    use runtime::{
        AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError,
    };
    use storage::{Database, Storage, StorageConfig, StorageHandle};
    use tools::ToolExecutor;

    use super::{RuntimeCommandSink, STORAGE_SESSION_ID, render_entry_prompt};
    use crate::model::commands::{
        CommandSink, GoalSubmission, LoopEvent, PacketReference, ReferenceKind, WorkbenchCommand,
    };

    /// どんなプロンプトにも応答せず run を走らせ続けるテスト用 stub モデル。
    #[derive(Default)]
    struct HeldModel {
        started: tokio::sync::Notify,
    }

    #[async_trait]
    impl AgentModel for HeldModel {
        async fn complete(
            &self,
            _invocation: &AgentInvocationContext,
            _role: Role,
            _messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            self.started.notify_one();
            std::future::pending().await
        }

        fn selected_model(&self, role: Role, _category: Option<&str>) -> String {
            format!("test-{}", role.name().to_lowercase())
        }
    }

    fn submission(
        goal: &str,
        references: Vec<PacketReference>,
        constraints: Vec<String>,
    ) -> GoalSubmission {
        GoalSubmission {
            delegation_value: None,
            project_root: None,
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: goal.into(),
            references,
            constraints,
        }
    }

    /// マルチスレッド tokio runtime 上に実 AgentRuntime を接続した sink を組み立てる。
    fn build_sink() -> (
        tokio::runtime::Runtime,
        RuntimeCommandSink,
        AgentRuntime,
        Arc<EventBus>,
    ) {
        let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
        build_sink_on(rt, Arc::new(HeldModel::default()))
    }

    fn build_sink_on(
        rt: tokio::runtime::Runtime,
        model: Arc<dyn AgentModel>,
    ) -> (
        tokio::runtime::Runtime,
        RuntimeCommandSink,
        AgentRuntime,
        Arc<EventBus>,
    ) {
        build_sink_with(rt, model, |runtime| runtime)
    }

    fn build_sink_with(
        rt: tokio::runtime::Runtime,
        model: Arc<dyn AgentModel>,
        configure: impl FnOnce(AgentRuntime) -> AgentRuntime,
    ) -> (
        tokio::runtime::Runtime,
        RuntimeCommandSink,
        AgentRuntime,
        Arc<EventBus>,
    ) {
        let bus = Arc::new(EventBus::new(64));
        let executor = Arc::new(ToolExecutor::new(bus.clone()));
        let runtime = configure(AgentRuntime::new(Arc::clone(&bus), executor, model));
        let sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone());
        (rt, sink, runtime, bus)
    }

    #[test]
    fn escalation_root_binding_keeps_worker_followups_independent() {
        let (_rt, mut sink, _, _) = build_sink();
        let worker = "run-1".parse::<runtime::RunId>().unwrap();
        let orchestrator = "run-2".parse::<runtime::RunId>().unwrap();
        sink.host
            .service_mut()
            .bindings
            .chat_runs
            .insert("parent".into(), worker);
        sink.bind_goal_context("parent", "project", "run-1");
        // Repair a duplicate run index without removing the worker's own root.
        sink.host
            .service_mut()
            .bindings
            .chat_runs
            .insert("stale-owner".into(), orchestrator);
        sink.bind_goal_context("stale-owner", "project", "run-2");
        sink.bind_goal_context("escalation-run-2", "project", "run-2");
        assert_eq!(
            sink.host.service_mut().bindings.chat_runs.get("parent"),
            Some(&worker)
        );
        assert_eq!(
            sink.host.service_mut().bindings.goal_runs.get("parent"),
            Some(&worker)
        );
        assert_eq!(
            sink.host
                .service_mut()
                .bindings
                .goal_runs
                .get("escalation-run-2"),
            Some(&orchestrator)
        );
        assert!(
            !sink
                .host
                .service_mut()
                .bindings
                .chat_runs
                .contains_key("stale-owner")
        );
        assert!(
            !sink
                .host
                .service_mut()
                .bindings
                .goal_runs
                .contains_key("stale-owner")
        );
    }

    #[test]
    fn web_tool_setting_updates_the_shared_runtime() {
        let (_rt, sink, runtime, _) = build_sink();
        let mut sink = sink.with_web_tools_enabled(false);
        assert!(!runtime.web_tools_enabled());
        assert!(
            sink.submit(WorkbenchCommand::SetWebToolsEnabled { enabled: true })
                .is_empty()
        );
        assert!(runtime.web_tools_enabled());
        sink.submit(WorkbenchCommand::SetWebToolsEnabled { enabled: false });
        assert!(!runtime.web_tools_enabled());
    }

    #[test]
    fn tool_approval_preserves_correlation_when_bus_is_configured() {
        use sandbox::approval::{
            ApprovalGate, ApprovalOutcome, ApprovalProvenance, ApprovalRegistry, ApprovalRequest,
        };
        let (rt, sink, _, _) = build_sink();
        let bus = Arc::new(EventBus::new(64));
        let registry = ApprovalRegistry::for_bus(&bus);
        let mut subscriber = bus.subscribe();
        let mut sink = sink.with_event_bus(bus.clone());
        for approved in [true, false] {
            let gate = ApprovalGate::new(bus.clone(), std::time::Duration::from_secs(300));
            let task = rt.spawn(async move {
                gate.request_for(ApprovalRequest {
                    run_id: "run-2",
                    thread_id: Some("thread-2"),
                    tool_name: "shell",
                    call_id: "call-1",
                    input: None,
                })
                .await
            });
            let token = rt.block_on(async {
                loop {
                    if let EventKind::Tool(event_bus::ToolEvent::ApprovalRequested {
                        call_id,
                        ..
                    }) = subscriber.recv().await.unwrap().kind
                    {
                        break call_id;
                    }
                }
            });
            let snapshot = registry.list_pending(None, 1).pop().unwrap();
            assert_eq!(snapshot.id.value, token);
            assert!(
                sink.submit(WorkbenchCommand::DecideToolApproval {
                    call_id: token.clone(),
                    approved
                })
                .is_empty()
            );
            assert_eq!(
                rt.block_on(task).unwrap(),
                if approved {
                    ApprovalOutcome::Approved
                } else {
                    ApprovalOutcome::Denied
                }
            );
            assert_eq!(
                registry.get(&snapshot.id).unwrap().decided_by,
                Some(ApprovalProvenance::DESKTOP)
            );
            assert!(matches!(
                sink.submit(WorkbenchCommand::DecideToolApproval {
                    call_id: token,
                    approved
                })
                .as_slice(),
                [LoopEvent::CommandRejected { .. }]
            ));
        }
        // Historical/provider IDs are not live authorization tokens.
        assert!(matches!(
            sink.submit(WorkbenchCommand::DecideToolApproval {
                call_id: "run-2:call-1:17".into(),
                approved: true
            })
            .as_slice(),
            [LoopEvent::CommandRejected { .. }]
        ));
    }

    #[test]
    fn tool_approval_returns_empty_when_bus_is_unconfigured() {
        // Given: bus を注入していない sink。
        let (_rt, mut sink, _, _) = build_sink();

        // When: tool 承認を提出する。
        let events = sink.submit(WorkbenchCommand::DecideToolApproval {
            call_id: "run-2:call-1:17".into(),
            approved: true,
        });

        // Then: panic せず、即応イベントも返さない。
        assert!(events.is_empty());
    }

    /// Persist a real requester's evidence, then leave it absent from the answering runtime.
    fn offline_question_fixture(
        rt: &tokio::runtime::Runtime,
        config: &StorageConfig,
        handle: StorageHandle,
        thread: &str,
        options: Vec<String>,
    ) -> event_bus::UserQuestion {
        let bus = Arc::new(EventBus::new(64));
        let requester = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(HeldModel::default()),
        )
        .with_run_store(runtime::RunStore::open(config, handle).unwrap());
        rt.block_on(async {
            let run = requester
                .delegate_chat(
                    thread,
                    Role::Worker,
                    "Prepare the output".into(),
                    RunConfig::default(),
                )
                .unwrap();
            let question = requester
                .request_user_question(run, "Which format?".into(), options, true)
                .unwrap();
            requester.cancel(run).unwrap();
            requester.wait(run).await.unwrap();
            question
        })
    }

    #[test]
    fn offline_question_rejects_caller_fenced_after_command_authorization() {
        let (rt, mut sink, runtime, _) = build_sink();
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("questions.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let question = offline_question_fixture(
            &rt,
            &config,
            storage.handle(),
            "thread-1",
            vec!["JSON".into()],
        );
        assert!(!runtime.has_active_question_recipient(&question.id).unwrap());
        let bus = Arc::new(EventBus::new(64));
        let first =
            runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus.clone())
                .unwrap();
        first.start("thread-1").unwrap();
        let authorized = first.owned_permit("thread-1").unwrap();
        let successor =
            runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus).unwrap();
        // Ownership changes between submit's initial check and durable answer write.
        first.handoff(&authorized, &successor).unwrap();
        let events = sink.submit_authorized(
            WorkbenchCommand::AnswerUserQuestion {
                thread_id: "thread-1".into(),
                question_id: question.id.clone(),
                answer: "JSON".into(),
            },
            Some(authorized),
        );
        assert!(
            matches!(events.as_slice(), [LoopEvent::CommandRejected { .. }]),
            "{events:?}"
        );
        assert!(
            runtime
                .user_question(&question.id)
                .unwrap()
                .unwrap()
                .answer
                .is_none()
        );
        assert!(runtime.list_agents().is_empty());

        // The current caller may save and resume; no registry read lock crosses begin_turn.
        let events = sink.submit_authorized(
            WorkbenchCommand::AnswerUserQuestion {
                thread_id: "thread-1".into(),
                question_id: question.id.clone(),
                answer: "JSON".into(),
            },
            Some(successor.owned_permit("thread-1").unwrap()),
        );
        assert!(
            matches!(
                events.as_slice(),
                [
                    LoopEvent::UserAnswerSaved { .. },
                    LoopEvent::ChatAccepted { .. }
                ]
            ),
            "{events:?}"
        );
        assert_eq!(
            runtime
                .user_question(&question.id)
                .unwrap()
                .unwrap()
                .answer
                .as_deref(),
            Some("JSON")
        );
        let run = sink.host.service_mut().bindings.chat_runs["thread-1"];
        runtime.cancel(run).unwrap();
        rt.block_on(async {
            runtime.wait(run).await.unwrap();
        });
    }

    #[test]
    fn inherited_question_requires_registered_recipient_and_current_thread_ownership() {
        let (rt, mut sink, runtime, _) = build_sink();
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("questions.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        sink.submit(chat_command("recipient"));
        let recipient = sink.host.service_mut().bindings.chat_runs["recipient"];
        let question = offline_question_fixture(&rt, &config, storage.handle(), "source", vec![]);
        storage
            .handle()
            .bind_user_questions(
                &question.run_id,
                &recipient.to_string(),
                std::slice::from_ref(&question.id),
            )
            .unwrap();
        let command = |thread: &str| WorkbenchCommand::AnswerUserQuestion {
            thread_id: thread.into(),
            question_id: question.id.clone(),
            answer: "JSON".into(),
        };
        assert!(matches!(
            sink.submit(command("unrelated")).as_slice(),
            [LoopEvent::CommandRejected { .. }]
        ));
        let bus = Arc::new(EventBus::new(64));
        let first =
            runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus.clone())
                .unwrap();
        first.start("recipient").unwrap();
        first.start("unrelated").unwrap();
        let stale = first.owned_permit("recipient").unwrap();
        let successor =
            runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus).unwrap();
        first.handoff(&stale, &successor).unwrap();
        for permit in [stale, first.owned_permit("unrelated").unwrap()] {
            assert!(matches!(
                sink.submit_authorized(command("recipient"), Some(permit))
                    .as_slice(),
                [LoopEvent::CommandRejected { .. }]
            ));
            assert!(
                runtime
                    .user_question(&question.id)
                    .unwrap()
                    .unwrap()
                    .answer
                    .is_none()
            );
        }
        // Even a bound consumer may not turn a child-to-parent question into a user prompt.
        let child = event_bus::UserQuestion {
            responder: None,
            routing_revision: 0,
            management_revision: None,
            id: "child".into(),
            root_run_id: "run-98".into(),
            ..question.clone()
        };
        storage.handle().create_user_question(&child).unwrap();
        storage
            .handle()
            .bind_user_questions(
                &question.run_id,
                &recipient.to_string(),
                std::slice::from_ref(&child.id),
            )
            .unwrap();
        assert!(matches!(
            sink.submit(WorkbenchCommand::AnswerUserQuestion {
                thread_id: "recipient".into(),
                question_id: child.id.clone(),
                answer: "JSON".into(),
            })
            .as_slice(),
            [LoopEvent::CommandRejected { .. }]
        ));
        assert!(
            runtime
                .user_question(&child.id)
                .unwrap()
                .unwrap()
                .answer
                .is_none()
        );
        assert!(matches!(
            sink.submit_authorized(
                command("recipient"),
                Some(successor.owned_permit("recipient").unwrap())
            )
            .as_slice(),
            [LoopEvent::UserAnswerSaved { .. }]
        ));
        assert_eq!(
            runtime
                .user_question(&question.id)
                .unwrap()
                .unwrap()
                .answer
                .as_deref(),
            Some("JSON")
        );
        // The original thread can still answer the same ID idempotently.
        assert!(matches!(
            sink.submit(command("source")).as_slice(),
            [LoopEvent::UserAnswerSaved { .. }]
        ));
    }

    fn chat_command(thread: &str) -> WorkbenchCommand {
        WorkbenchCommand::SendChat(crate::model::commands::ChatSubmission {
            submission_id: String::new(),
            project_root: None,
            fork_seed: None,
            composer_role: crate::model::composer::ComposerRole::Orchestrator,
            images: Vec::new(),
            thread_id: thread.into(),
            text: "hello again".into(),
            model_preference: None,
            skills: Vec::new(),
        })
    }

    fn continue_command(thread: &str) -> WorkbenchCommand {
        WorkbenchCommand::ContinueChat(crate::model::commands::ChatContinuation {
            project_root: None,
            thread_id: thread.into(),
            composer_role: crate::model::composer::ComposerRole::Worker,
            model_preference: None,
            automatic: false,
        })
    }

    #[test]
    fn continue_never_starts_an_empty_or_missing_context_conversation() {
        let (rt, mut sink, runtime, _) = build_sink();
        assert!(matches!(
            sink.submit(continue_command("empty")).as_slice(),
            [LoopEvent::ChatNotice { .. }]
        ));
        assert!(runtime.list_agents().is_empty());
        sink.submit(chat_command("no-store"));
        let run = sink.host.service_mut().bindings.chat_runs["no-store"];
        sink.submit(WorkbenchCommand::StopChat {
            thread_id: "no-store".into(),
        });
        rt.block_on(async {
            runtime.wait(run).await.unwrap();
        });
        assert!(matches!(
            sink.submit(continue_command("no-store")).as_slice(),
            [LoopEvent::ChatRejected { .. }]
        ));
        assert_eq!(sink.host.service_mut().bindings.chat_runs["no-store"], run);
        assert_eq!(runtime.list_agents().len(), 1);
    }

    #[test]
    fn continue_does_not_queue_duplicate_turns_while_running() {
        let (_rt, mut sink, runtime, _) = build_sink();
        sink.submit(chat_command("active"));
        assert!(matches!(
            sink.submit(continue_command("active")).as_slice(),
            [LoopEvent::ChatNotice { .. }]
        ));
        assert_eq!(runtime.list_agents().len(), 1);
    }

    struct ErrorOnceModel {
        fail: std::sync::atomic::AtomicBool,
        started: tokio::sync::Notify,
        requests: std::sync::Mutex<Vec<Vec<Message>>>,
    }
    #[async_trait]
    impl AgentModel for ErrorOnceModel {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "error-once".into()
        }
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            messages: &[Message],
            _: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            self.requests.lock().unwrap().push(messages.to_vec());
            self.started.notify_one();
            if self.fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(RuntimeError::Model {
                    reason: "test failure".into(),
                });
            }
            std::future::pending().await
        }
    }

    #[test]
    fn continue_restores_stopped_and_failed_chats_in_place_even_after_restart() {
        for error in [false, true] {
            for restart in [false, true] {
                let model = Arc::new(ErrorOnceModel {
                    fail: std::sync::atomic::AtomicBool::new(error),
                    started: tokio::sync::Notify::new(),
                    requests: Default::default(),
                });
                let (rt, mut sink, runtime, _) =
                    build_sink_on(tokio::runtime::Runtime::new().unwrap(), model.clone());
                let dir = tempfile::tempdir().unwrap();
                let config = StorageConfig {
                    db_path: dir.path().join("continue.db"),
                    ..Default::default()
                };
                let storage = Storage::open(config.clone()).unwrap();
                let runtime = runtime
                    .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
                sink.submit(chat_command("resume"));
                let run = sink.host.service_mut().bindings.chat_runs["resume"];
                rt.block_on(async {
                    model.started.notified().await;
                });
                if !error {
                    sink.submit(WorkbenchCommand::StopChat {
                        thread_id: "resume".into(),
                    });
                }
                let phase = rt.block_on(async { runtime.wait(run).await.unwrap() });
                assert_eq!(
                    phase,
                    if error {
                        event_bus::AgentRunPhase::Error
                    } else {
                        event_bus::AgentRunPhase::Stopped
                    }
                );
                let before = Database::open(&config)
                    .unwrap()
                    .run_context(&run.to_string())
                    .unwrap()
                    .unwrap();
                if restart {
                    sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone());
                }
                let events = sink.submit(continue_command("resume"));
                assert!(
                    matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == run.to_string()),
                    "{events:?}"
                );
                assert_eq!(sink.host.service_mut().bindings.chat_runs["resume"], run);
                assert_eq!(runtime.list_agents().len(), 1);
                assert!(
                    !sink
                        .host
                        .service_mut()
                        .bindings
                        .stopped_by_us
                        .contains("resume")
                );
                rt.block_on(async {
                    model.started.notified().await;
                });
                let requests = model.requests.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[1], requests[0], "resume must not add model input");
                drop(requests);
                let after = Database::open(&config)
                    .unwrap()
                    .run_context(&run.to_string())
                    .unwrap()
                    .unwrap();
                let before: Vec<Message> = serde_json::from_str(&before.messages_json).unwrap();
                let after: Vec<Message> = serde_json::from_str(&after.messages_json).unwrap();
                assert!(
                    after.starts_with(&before),
                    "continuation must retain the saved prefix"
                );
                runtime.cancel(run).unwrap();
                rt.block_on(async {
                    runtime.wait(run).await.unwrap();
                });
            }
        }
    }

    #[test]
    fn rapid_stop_presses_escalate_before_root_phase_changes() {
        // No executor progress between the two commands: phase-based staging fails here.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (rt, mut sink, runtime, _) = build_sink_on(rt, Arc::new(HeldModel::default()));
        sink.submit(chat_command("rapid"));
        let root = sink.host.service_mut().bindings.chat_runs["rapid"];
        let child = {
            let _guard = rt.enter();
            runtime
                .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
                .unwrap()
        };
        let stop = WorkbenchCommand::StopChat {
            thread_id: "rapid".into(),
        };
        assert!(matches!(
            sink.submit(stop.clone()).as_slice(),
            [LoopEvent::ChatStopped {
                running_children: 1,
                ..
            }]
        ));
        assert!(
            sink.host
                .service_mut()
                .bindings
                .stop_marked
                .contains("rapid"),
            "first press is SelfOnly"
        );
        assert_eq!(
            runtime.inspect_agent(root).unwrap().phase,
            event_bus::AgentRunPhase::Pending
        );
        assert!(matches!(
            sink.submit(stop).as_slice(),
            [LoopEvent::ChatStopped { .. }]
        ));
        assert!(
            !sink
                .host
                .service_mut()
                .bindings
                .stop_marked
                .contains("rapid"),
            "second press is Subtree"
        );
        assert_eq!(
            runtime.inspect_agent(root).unwrap().phase,
            event_bus::AgentRunPhase::Pending
        );
        rt.block_on(async {
            for run in [root, child] {
                assert_eq!(
                    runtime.wait(run).await.unwrap(),
                    event_bus::AgentRunPhase::Stopped
                );
            }
        });
        // Stage two consumed the stage marker, but missing-storage resume still works.
        assert!(matches!(
            sink.submit(chat_command("rapid")).as_slice(),
            [LoopEvent::ChatAccepted { .. }]
        ));
        assert_ne!(sink.host.service_mut().bindings.chat_runs["rapid"], root);
        assert!(
            !sink
                .host
                .service_mut()
                .bindings
                .stopped_by_us
                .contains("rapid")
        );
        assert!(matches!(
            sink.submit(WorkbenchCommand::StopChat {
                thread_id: "rapid".into()
            })
            .as_slice(),
            [LoopEvent::ChatStopped { .. }]
        ));
        assert!(
            sink.host
                .service_mut()
                .bindings
                .stop_marked
                .contains("rapid"),
            "new run restarts at stage one"
        );
    }

    struct AdmissionHeldModel;

    #[async_trait]
    impl AgentModel for AdmissionHeldModel {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "admission-held".into()
        }
        fn requires_admission(&self) -> bool {
            true
        }
        async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
            std::future::pending().await
        }
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[Message],
            _: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            std::future::pending().await
        }
    }

    #[test]
    fn stopped_missing_context_delegates_fresh_for_chat_and_goal() {
        assert_stopped_without_context_restarts(true);
    }

    #[test]
    fn stopped_without_storage_delegates_fresh_for_chat_and_goal() {
        assert_stopped_without_context_restarts(false);
    }

    fn assert_stopped_without_context_restarts(with_storage: bool) {
        for goal in [false, true] {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let (_rt, mut sink, runtime, _) = build_sink_on(rt, Arc::new(AdmissionHeldModel));
            let dir = tempfile::tempdir().unwrap();
            let config = StorageConfig {
                db_path: dir.path().join("missing.db"),
                ..Default::default()
            };
            let storage = Storage::open(config.clone()).unwrap();
            let runtime = if with_storage {
                runtime.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap())
            } else {
                runtime
            };
            sink.submit(chat_command("no-context"));
            let root = sink.host.service_mut().bindings.chat_runs["no-context"];
            if goal {
                sink.host
                    .service_mut()
                    .bindings
                    .chat_runs
                    .remove("no-context");
                sink.bind_goal_context("no-context", "project", &root.to_string());
            }
            assert!(matches!(
                sink.submit(WorkbenchCommand::StopChat {
                    thread_id: "no-context".into()
                })
                .as_slice(),
                [LoopEvent::ChatStopped { .. }]
            ));
            assert!(
                sink.host
                    .service_mut()
                    .bindings
                    .stop_marked
                    .contains("no-context")
            );
            let error = runtime
                .continue_goal(root, "probe".into(), RunConfig::default())
                .unwrap_err();
            assert!(
                matches!(error, RuntimeError::RunRestoreFailed { reason, .. }
                if reason == if with_storage { runtime::RunRestoreFailure::MissingContext }
                else { runtime::RunRestoreFailure::StorageNotConfigured })
            );
            let events = sink.submit(chat_command("no-context"));
            assert!(
                matches!(events.as_slice(), [LoopEvent::ChatAccepted { .. }]),
                "{events:?}"
            );
            let fresh = sink.host.service_mut().bindings.chat_runs["no-context"];
            assert_ne!(fresh, root);
            assert!(
                !sink
                    .host
                    .service_mut()
                    .bindings
                    .stop_marked
                    .contains("no-context")
            );
            assert!(
                !sink
                    .host
                    .service_mut()
                    .bindings
                    .stopped_by_us
                    .contains("no-context")
            );
            // Both goal and plain-chat paths use the new binding on subsequent stops.
            sink.submit(WorkbenchCommand::StopChat {
                thread_id: "no-context".into(),
            });
            assert!(
                sink.host
                    .service_mut()
                    .bindings
                    .stop_marked
                    .contains("no-context")
            );
        }
    }

    #[test]
    fn resume_fallback_requires_our_stop_and_only_absent_context() {
        let (_rt, mut sink, _, _) = build_sink();
        for goal in [false, true] {
            let id = runtime::RunId::new(999);
            if goal {
                sink.host
                    .service_mut()
                    .bindings
                    .goal_runs
                    .insert("unmarked".into(), id);
            } else {
                sink.host
                    .service_mut()
                    .bindings
                    .chat_runs
                    .insert("unmarked".into(), id);
            }
            assert!(matches!(
                sink.submit(chat_command("unmarked")).as_slice(),
                [LoopEvent::ChatRejected { .. }]
            ));
        }
        sink.host
            .service_mut()
            .bindings
            .stop_marked
            .insert("marked".into());
        for reason in [
            runtime::RunRestoreFailure::CorruptContext("bad".into()),
            runtime::RunRestoreFailure::UnsupportedConfig("bad".into()),
            runtime::RunRestoreFailure::SnapshotConsumeFailed("bad".into()),
        ] {
            assert!(!sink.can_restart_stopped(
                "marked",
                &RuntimeError::RunRestoreFailed {
                    run_id: "run-999".into(),
                    reason,
                }
            ));
        }
    }

    #[test]
    fn stop_chat_only_stops_parent_then_subtree_and_retains_binding() {
        let (rt, mut sink, runtime, _) = build_sink();
        sink.submit(WorkbenchCommand::SendChat(
            crate::model::commands::ChatSubmission {
                submission_id: String::new(),
                project_root: None,
                fork_seed: None,
                composer_role: crate::model::composer::ComposerRole::Orchestrator,
                images: Vec::new(),
                thread_id: "chat-thread".into(),
                text: "hello".into(),
                model_preference: None,
                skills: Vec::new(),
            },
        ));
        let root = sink.host.service_mut().bindings.chat_runs["chat-thread"];
        let (child, grandchild) = rt.block_on(async {
            let child = runtime
                .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
                .unwrap();
            let grandchild = runtime
                .delegate_background_as_child(
                    child,
                    Role::Worker,
                    "grandchild",
                    RunConfig::default(),
                )
                .unwrap();
            (child, grandchild)
        });
        let command = WorkbenchCommand::StopChat {
            thread_id: "chat-thread".into(),
        };
        let events = sink.submit(command.clone());
        assert!(
            matches!(
                events.as_slice(),
                [LoopEvent::ChatStopped {
                    running_children: 2,
                    ..
                }]
            ),
            "{events:?}"
        );
        let phase = rt.block_on(async { runtime.wait(root).await.unwrap() });
        assert_eq!(phase, event_bus::AgentRunPhase::Stopped);
        assert_eq!(
            sink.host.service_mut().bindings.chat_runs["chat-thread"],
            root
        );
        assert_eq!(runtime.live_descendants(root), vec![child, grandchild]);
        assert!(matches!(
            sink.submit(command.clone()).as_slice(),
            [LoopEvent::ChatStopped { .. }]
        ));
        rt.block_on(async {
            for run in [child, grandchild] {
                let phase = runtime.wait(run).await.unwrap();
                assert_eq!(phase, event_bus::AgentRunPhase::Stopped);
            }
        });
        sink.observe_lifecycle(&event_bus::Event::new(
            event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id: grandchild.to_string(),
                from: event_bus::AgentRunPhase::Running,
                to: event_bus::AgentRunPhase::Stopped,
                reason: None,
            },
        ));
        assert_eq!(
            sink.host.service_mut().bindings.running_children["chat-thread"],
            0
        );
        assert_eq!(
            sink.host.service_mut().bindings.chat_runs["chat-thread"],
            root
        );
        assert!(
            matches!(sink.submit(command).as_slice(), [LoopEvent::ChatNotice { text, .. }] if text == "Already stopped; use /continue or send a message to resume")
        );
    }

    fn stop_run_command(thread: &str, run: runtime::RunId) -> WorkbenchCommand {
        WorkbenchCommand::StopRun {
            thread_id: thread.into(),
            run_id: run.to_string(),
        }
    }

    struct FailedRootModel;

    #[async_trait]
    impl AgentModel for FailedRootModel {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "failed-root".into()
        }

        async fn complete(
            &self,
            _: &AgentInvocationContext,
            role: Role,
            _: &[Message],
            _: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            if role == Role::Orchestrator {
                Err(RuntimeError::Model {
                    reason: "root failure".into(),
                })
            } else {
                std::future::pending().await
            }
        }
    }

    #[test]
    fn stop_run_works_after_root_error_and_preserves_other_runs_and_thread_state() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (rt, mut sink, runtime, _) = build_sink_on(rt, Arc::new(FailedRootModel));
        sink.submit(chat_command("failed-root"));
        let root = sink.host.service_mut().bindings.chat_runs["failed-root"];
        let (child, grandchild, sibling) = rt.block_on(async {
            let child = runtime
                .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
                .unwrap();
            let grandchild = runtime
                .delegate_background_as_child(
                    child,
                    Role::Worker,
                    "grandchild",
                    RunConfig::default(),
                )
                .unwrap();
            let sibling = runtime
                .delegate_background_as_child(root, Role::Worker, "sibling", RunConfig::default())
                .unwrap();
            assert_eq!(
                runtime.wait(root).await.unwrap(),
                event_bus::AgentRunPhase::Error
            );
            (child, grandchild, sibling)
        });
        assert!(
            sink.submit(stop_run_command("failed-root", child))
                .is_empty()
        );
        rt.block_on(async {
            assert_eq!(
                runtime.wait(child).await.unwrap(),
                event_bus::AgentRunPhase::Stopped
            );
        });
        assert_eq!(
            runtime.inspect_agent(root).unwrap().phase,
            event_bus::AgentRunPhase::Error
        );
        assert_eq!(runtime.live_descendants(root), vec![grandchild, sibling]);
        assert_eq!(
            sink.host.service_mut().bindings.chat_runs["failed-root"],
            root
        );
        assert!(sink.host.service_mut().bindings.stop_marked.is_empty());
        assert!(sink.host.service_mut().bindings.stopped_by_us.is_empty());
        runtime.cancel_subtree(root).unwrap();
        rt.block_on(async {
            for run in [grandchild, sibling] {
                runtime.wait(run).await.unwrap();
            }
        });
    }

    #[test]
    fn stop_run_rejects_other_threads_roots_terminal_runs_and_stale_owners() {
        let (rt, mut sink, runtime, _) = build_sink();
        sink.submit(chat_command("thread"));
        sink.submit(chat_command("other"));
        let root = sink.host.service_mut().bindings.chat_runs["thread"];
        let other_root = sink.host.service_mut().bindings.chat_runs["other"];
        let child = rt.block_on(async {
            runtime
                .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
                .unwrap()
        });
        for command in [
            stop_run_command("other", child),
            stop_run_command("thread", root),
            stop_run_command("thread", other_root),
            stop_run_command("missing", child),
            WorkbenchCommand::StopRun {
                thread_id: "thread".into(),
                run_id: "invalid".into(),
            },
        ] {
            assert!(matches!(
                sink.submit(command).as_slice(),
                [LoopEvent::ChatNotice { .. }]
            ));
        }
        let dir = tempfile::tempdir().unwrap();
        let bus = Arc::new(EventBus::new(64));
        let owner =
            runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus.clone())
                .unwrap();
        owner.start("thread").unwrap();
        owner.start("other").unwrap();
        let stale = owner.owned_permit("thread").unwrap();
        let successor =
            runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus).unwrap();
        owner.handoff(&stale, &successor).unwrap();
        sink.host.service_mut().set_ownership(Some(Arc::new(owner)));
        let command = stop_run_command("thread", child);
        for permit in [
            Some(stale),
            Some(
                sink.host
                    .service()
                    .ownership()
                    .unwrap()
                    .owned_permit("other")
                    .unwrap(),
            ),
            None,
        ] {
            assert!(matches!(
                sink.submit_authorized(command.clone(), permit).as_slice(),
                [LoopEvent::CommandRejected { .. }]
            ));
        }
        assert!(matches!(
            sink.submit(command.clone()).as_slice(),
            [LoopEvent::CommandRejected { .. }]
        ));
        assert_eq!(runtime.live_descendants(root), vec![child]);
        assert!(
            sink.submit_authorized(
                command.clone(),
                Some(successor.owned_permit("thread").unwrap())
            )
            .is_empty()
        );
        rt.block_on(async {
            assert_eq!(
                runtime.wait(child).await.unwrap(),
                event_bus::AgentRunPhase::Stopped
            );
        });
        sink.host
            .service_mut()
            .set_ownership(Some(Arc::new(successor)));
        assert!(matches!(
            sink.submit(command).as_slice(),
            [LoopEvent::ChatNotice { .. }]
        ));
        assert!(!matches!(
            runtime.inspect_agent(root).unwrap().phase,
            event_bus::AgentRunPhase::Stopped | event_bus::AgentRunPhase::Error
        ));
        assert!(!matches!(
            runtime.inspect_agent(other_root).unwrap().phase,
            event_bus::AgentRunPhase::Stopped | event_bus::AgentRunPhase::Error
        ));
        for root in [root, other_root] {
            runtime.cancel_subtree(root).unwrap();
            rt.block_on(async {
                runtime.wait(root).await.unwrap();
            });
        }
    }

    #[test]
    fn generic_goal_controls_require_current_thread_owner() {
        let (rt, mut sink, runtime, _) = build_sink();
        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "Verify current behavior",
            vec![],
            vec![],
        )));
        let goal = runtime.thread_goal("thread-1").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bus = Arc::new(EventBus::new(64));
        let previous =
            runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus.clone())
                .unwrap();
        previous.start("thread-1").unwrap();
        previous.start("other").unwrap();
        let stale = previous.owned_permit("thread-1").unwrap();
        let successor =
            runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus).unwrap();
        previous.handoff(&stale, &successor).unwrap();
        sink.host
            .service_mut()
            .set_ownership(Some(Arc::new(previous)));
        let control = WorkbenchCommand::SetGoalChecksPaused {
            thread_id: "thread-1".into(),
            goal_id: goal.goal_id,
            paused: true,
        };
        for permit in [
            None,
            Some(stale),
            Some(
                sink.host
                    .service()
                    .ownership()
                    .unwrap()
                    .owned_permit("other")
                    .unwrap(),
            ),
        ] {
            assert!(matches!(
                sink.submit_authorized(control.clone(), permit).as_slice(),
                [LoopEvent::CommandRejected { .. }]
            ));
            assert!(!runtime.thread_goal("thread-1").unwrap().checks_paused);
        }
        assert!(
            sink.submit_authorized(control, Some(successor.owned_permit("thread-1").unwrap()))
                .is_empty()
        );
        assert!(runtime.thread_goal("thread-1").unwrap().checks_paused);
        let root = sink.host.service_mut().bindings.chat_runs["thread-1"];
        runtime.cancel(root).unwrap();
        rt.block_on(runtime.wait(root)).unwrap();
    }

    struct TextThenHold(std::sync::atomic::AtomicBool);

    #[async_trait]
    impl AgentModel for TextThenHold {
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[Message],
            _: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::future::pending().await
            } else {
                Ok(ChatResponse {
                    usage_reported: true,
                    message: Message {
                        role: providers::Role::Assistant,
                        content: vec![providers::ContentBlock::Text {
                            text: "Initial response".into(),
                        }],
                    },
                    usage: Default::default(),
                    finish_reason: providers::FinishReason::Stop,
                })
            }
        }
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "fixture".into()
        }
    }

    #[test]
    fn explicit_goal_joins_waiting_or_stopped_chat_with_the_current_request() {
        for stopped in [false, true] {
            let (rt, mut sink, runtime, bus) = build_sink_on(
                tokio::runtime::Runtime::new().unwrap(),
                Arc::new(TextThenHold(false.into())),
            );
            let dir = tempfile::tempdir().unwrap();
            let config = StorageConfig {
                db_path: dir.path().join("chat.sqlite"),
                ..Default::default()
            };
            let storage = Storage::open(config.clone()).unwrap();
            let runtime =
                runtime.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
            let mut events = bus.subscribe();
            sink.submit(chat_command("thread-1"));
            let root = sink.host.service_mut().bindings.chat_runs["thread-1"];
            rt.block_on(async {
                while runtime.inspect_agent(root).unwrap().phase
                    != event_bus::AgentRunPhase::Waiting
                {
                    events.recv().await.unwrap();
                }
            });
            if stopped {
                sink.submit(WorkbenchCommand::StopChat {
                    thread_id: "thread-1".into(),
                });
                rt.block_on(runtime.wait(root)).unwrap();
            }
            let objective = "Document the observed cause";
            let events = sink.submit(WorkbenchCommand::SubmitGoal(submission(
                objective,
                vec![],
                vec![],
            )));
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        LoopEvent::ChatAccepted { .. },
                        LoopEvent::GoalAccepted { .. }
                    ]
                ),
                "{events:?}"
            );
            let goal = runtime.thread_goal("thread-1").unwrap();
            assert_eq!(goal.root_run_id, root.to_string());
            assert!(goal.original_request.contains(objective));
            assert!(!goal.review_enabled);
            runtime.stop(root, runtime::StopScope::SelfOnly).unwrap();
            rt.block_on(runtime.wait(root)).unwrap();
            let rejection = sink.submit(WorkbenchCommand::SubmitGoal(submission(
                "Replace the active objective",
                vec![],
                vec![],
            )));
            assert!(
                matches!(
                    rejection.as_slice(),
                    [LoopEvent::ChatRejected { .. } | LoopEvent::CommandRejected { .. }]
                ),
                "{rejection:?}"
            );
            assert_eq!(
                runtime.thread_goal("thread-1").unwrap().goal_id,
                goal.goal_id
            );
            assert!(matches!(
                sink.submit(continue_command("thread-1")).as_slice(),
                [LoopEvent::ChatAccepted { .. }]
            ));
            runtime.cancel(root).unwrap();
            rt.block_on(runtime.wait(root)).unwrap();
        }
    }

    #[test]
    fn generic_goal_checks_pause_keeps_work_running_and_survives_chat_resume() {
        let model = Arc::new(HeldModel::default());
        let (rt, mut sink, runtime, _) =
            build_sink_on(tokio::runtime::Runtime::new().unwrap(), model.clone());
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("goal.sqlite3"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "investigate a slow query",
            vec![],
            vec![],
        )));
        let root = sink.host.service_mut().bindings.chat_runs["thread-1"];
        rt.block_on(model.started.notified());
        let goal = runtime.thread_goal("thread-1").unwrap();
        assert!(
            sink.submit(WorkbenchCommand::SetGoalChecksPaused {
                thread_id: "thread-1".into(),
                goal_id: goal.goal_id.clone(),
                paused: true
            })
            .is_empty()
        );
        assert!(runtime.thread_goal("thread-1").unwrap().checks_paused);
        assert_eq!(
            runtime.inspect_agent(root).unwrap().phase,
            event_bus::AgentRunPhase::Running
        );
        assert!(
            sink.submit(WorkbenchCommand::SetGoalReview {
                thread_id: "thread-1".into(),
                goal_id: goal.goal_id.clone(),
                enabled: true
            })
            .is_empty()
        );
        assert!(runtime.thread_goal("thread-1").unwrap().review_enabled);
        assert!(matches!(
            sink.submit(WorkbenchCommand::StopChat {
                thread_id: "thread-1".into()
            })
            .as_slice(),
            [LoopEvent::ChatStopped { .. }]
        ));
        rt.block_on(runtime.wait(root)).unwrap();
        assert!(runtime.thread_goal("thread-1").unwrap().work_stopped);
        assert!(matches!(
            sink.submit(continue_command("thread-1")).as_slice(),
            [LoopEvent::ChatAccepted { .. }]
        ));
        rt.block_on(model.started.notified());
        let resumed = runtime.thread_goal("thread-1").unwrap();
        assert!(resumed.checks_paused);
        assert!(resumed.review_enabled);
        assert!(!resumed.work_stopped);
        assert_eq!(sink.host.service_mut().bindings.chat_runs["thread-1"], root);
        runtime.cancel(root).unwrap();
        rt.block_on(runtime.wait(root)).unwrap();
    }

    #[test]
    fn stop_before_goal_initialization_restarts_with_a_fresh_root() {
        // Keep the executor idle until Stop is submitted, making the missing
        // conversation case deterministic and separate from in-place restore.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (rt, mut sink, runtime, _) = build_sink_on(rt, Arc::new(HeldModel::default()));
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("early-stop.sqlite3"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let root = {
            let _guard = rt.enter();
            runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default())
        };
        sink.bind_goal_context("thread-1", "evorch", &root.to_string());
        assert_eq!(
            runtime.inspect_agent(root).unwrap().phase,
            event_bus::AgentRunPhase::Pending
        );
        assert!(matches!(
            sink.submit(WorkbenchCommand::StopChat {
                thread_id: "thread-1".into()
            })
            .as_slice(),
            [LoopEvent::ChatStopped { .. }]
        ));
        assert_eq!(
            rt.block_on(runtime.wait(root)).unwrap(),
            event_bus::AgentRunPhase::Stopped
        );
        let events = sink.submit(chat_command("thread-1"));
        assert!(matches!(
            events.as_slice(),
            [LoopEvent::ChatAccepted { .. }]
        ));
        assert_ne!(sink.host.service_mut().bindings.chat_runs["thread-1"], root);
    }

    #[test]
    fn stop_chat_without_run_is_an_informational_notice() {
        let (_rt, mut sink, _, _) = build_sink();
        assert!(
            matches!(sink.submit(WorkbenchCommand::StopChat { thread_id: "missing".into() }).as_slice(),
            [LoopEvent::ChatNotice { text, .. }] if text == "No chat run to stop")
        );
    }

    // A thread's chats work in its project even while another project is active,
    // including host follow-ups that carry no project of their own.
    #[test]
    fn chat_runs_bind_the_thread_project_instead_of_the_active_one() {
        let (thread_project, active_project) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let resolved = Arc::new(std::sync::Mutex::new(Vec::<std::path::PathBuf>::new()));
        let record = Arc::clone(&resolved);
        let (rt, mut sink, runtime, _) = build_sink_with(
            tokio::runtime::Runtime::new().unwrap(),
            Arc::new(HeldModel::default()),
            |runtime| {
                runtime.with_project_models(Arc::new(move |root| {
                    record.lock().unwrap().push(root.to_path_buf());
                    None
                }))
            },
        );
        runtime
            .set_project_root(active_project.path().to_path_buf())
            .unwrap();
        let chat = |project_root| {
            WorkbenchCommand::SendChat(crate::model::commands::ChatSubmission {
                submission_id: String::new(),
                project_root,
                fork_seed: None,
                composer_role: crate::model::composer::ComposerRole::Worker,
                images: Vec::new(),
                thread_id: "chat-thread".into(),
                text: "hello".into(),
                model_preference: None,
                skills: Vec::new(),
            })
        };

        sink.submit(chat(Some(thread_project.path().to_path_buf())));
        let first = sink.host.service_mut().bindings.chat_runs["chat-thread"];
        // A stopped chat without saved context restarts as a fresh run.
        sink.submit(WorkbenchCommand::StopChat {
            thread_id: "chat-thread".into(),
        });
        rt.block_on(async {
            runtime.wait(first).await.unwrap();
        });
        sink.submit(chat(None));
        let second = sink.host.service_mut().bindings.chat_runs["chat-thread"];

        assert_ne!(first, second);
        assert_eq!(*resolved.lock().unwrap(), [thread_project.path()]);
        runtime.cancel(second).unwrap();
        rt.block_on(async {
            runtime.wait(second).await.unwrap();
        });
    }

    #[test]
    fn team_submission_reaches_runtime_with_shared_storage() {
        let model = Arc::new(HeldModel::default());
        let (rt, sink, runtime, _) =
            build_sink_on(tokio::runtime::Runtime::new().unwrap(), model.clone());
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("team.db"),
            ..Default::default()
        };
        let writer = Storage::open(config.clone()).unwrap();
        let mut sink = sink
            .with_memory_storage(config)
            .with_team_writer(writer.handle());
        let mut goal = submission("implement independent tasks", vec![], vec![]);
        goal.delegation_value = Some("independent paths".into());
        assert!(matches!(
            sink.submit(WorkbenchCommand::SubmitGoal(goal))[0],
            LoopEvent::ChatAccepted { .. }
        ));
        rt.block_on(model.started.notified());
        assert_eq!(runtime.team_tasks().len(), 1);
    }

    fn sink_with_promoted_lesson() -> (
        tokio::runtime::Runtime,
        RuntimeCommandSink,
        Arc<EventBus>,
        tempfile::TempDir,
    ) {
        let (rt, sink, _, bus) = build_sink_with(
            tokio::runtime::Runtime::new().unwrap(),
            Arc::new(HeldModel::default()),
            |runtime| runtime.with_project_slugs(Arc::new(|_| "owner/name".into())),
        );
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("memory.db"),
            ..Default::default()
        };
        let store = Storage::open(config.clone()).unwrap();
        store
            .handle()
            .append_lesson(&storage::memory::Lesson {
                id: "l".into(),
                project: "owner/name".into(),
                task_id: "t".into(),
                content: "Read lessons by repository slug".into(),
                evidence: "test:slug".into(),
                scope: storage::memory::LessonScope::Project,
            })
            .unwrap();
        store.handle().validate_lesson("l", "test:slug").unwrap();
        store.handle().promote_lesson("l").unwrap();
        let sink = sink
            .with_memory_storage(config)
            .with_team_writer(store.handle());
        (rt, sink, bus, dir)
    }

    fn published_prompt(
        rt: &tokio::runtime::Runtime,
        events: &mut event_bus::EventReceiver,
    ) -> String {
        rt.block_on(async {
            loop {
                if let EventKind::Lifecycle(event_bus::LifecycleEvent::TaskPromptPublished {
                    prompt,
                    ..
                }) = events.recv().await.unwrap().kind
                {
                    return prompt;
                }
            }
        })
    }

    #[test]
    fn chat_prompt_carries_lessons_learned_under_the_project_slug() {
        // Given: a promoted lesson filed under the slug, not the sidebar project id.
        let (rt, mut sink, bus, dir) = sink_with_promoted_lesson();
        let mut events = bus.subscribe();
        // When: an ordinary chat starts in that project.
        sink.submit(WorkbenchCommand::SendChat(
            crate::model::commands::ChatSubmission {
                submission_id: String::new(),
                project_root: Some(dir.path().to_path_buf()),
                fork_seed: None,
                composer_role: crate::model::composer::ComposerRole::Worker,
                images: Vec::new(),
                thread_id: "chat-thread".into(),
                text: "hello".into(),
                model_preference: None,
                skills: Vec::new(),
            },
        ));
        // Then: the lesson reaches the run's first prompt.
        assert!(published_prompt(&rt, &mut events).contains("Read lessons by repository slug"));
    }

    #[test]
    fn team_goal_prompt_carries_lessons_learned_under_the_project_slug() {
        // Given: a promoted lesson under the slug while the sidebar names the project `evorch`.
        let (rt, mut sink, bus, dir) = sink_with_promoted_lesson();
        let mut events = bus.subscribe();
        let mut goal = submission("implement independent tasks", vec![], vec![]);
        goal.project_root = Some(dir.path().to_path_buf());
        goal.delegation_value = Some("independent paths".into());
        // When: a team goal starts.
        sink.submit(WorkbenchCommand::SubmitGoal(goal));
        // Then: the coordinator's prompt carries the lesson.
        assert!(published_prompt(&rt, &mut events).contains("Read lessons by repository slug"));
    }

    /// storage bridge をテスト用に起動する (本番と同一の session ID で永続化する)。
    fn spawn_test_bridge(
        rt: &tokio::runtime::Runtime,
        bus: Arc<EventBus>,
        handle: StorageHandle,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<(String, String)>,
    ) {
        let mut subscriber = bus.subscribe();
        let (persisted, completed) = tokio::sync::oneshot::channel();
        let mut persisted = Some(persisted);
        let task = rt.spawn(async move {
            loop {
                match subscriber.recv().await {
                    Ok(event) => {
                        handle
                            .append_event(Some(STORAGE_SESSION_ID), &event)
                            .expect("persist event");
                        if let EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated {
                            snapshot,
                        }) = event.kind
                            && let Some(persisted) = persisted.take()
                        {
                            let _ = persisted.send((snapshot.goal_id, snapshot.root_run_id));
                        }
                    }
                    Err(error) => panic!("storage bridge event: {error:?}"),
                }
            }
        });
        (task, completed)
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

    // Given: origin を持つリポジトリと、持たないディレクトリ
    // When: derive_repo_slug する
    // Then: origin の owner/name を返し、無ければディレクトリ名へ fallback する
    #[test]
    fn derive_repo_slug_uses_origin_and_falls_back_to_the_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "--quiet"]);
        assert_eq!(super::derive_repo_slug(&repo), "checkout");
        git(&[
            "remote",
            "add",
            "origin",
            "git@github.com:turtton/evorch.git",
        ]);
        assert_eq!(super::derive_repo_slug(&repo), "turtton/evorch");
        git(&[
            "remote",
            "set-url",
            "origin",
            "https://github.com/turtton/evorch",
        ]);
        assert_eq!(super::derive_repo_slug(&repo), "turtton/evorch");
    }

    #[test]
    fn submit_goal_joins_chat_and_rejects_replacing_an_active_objective() {
        let (_rt, mut sink, runtime, _) = build_sink();
        let first = sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "research a bottleneck",
            vec![],
            vec![],
        )));
        let snapshot = runtime.thread_goal("thread-1").unwrap();
        assert!(
            matches!(first.as_slice(), [LoopEvent::ChatAccepted { .. }, LoopEvent::GoalAccepted { goal_id, .. }] if goal_id == &snapshot.goal_id)
        );
        assert_eq!(
            snapshot.root_run_id,
            sink.host.service_mut().bindings.chat_runs["thread-1"].to_string()
        );
        assert!(!snapshot.review_enabled);
        assert!(matches!(
            sink.submit(WorkbenchCommand::SubmitGoal(submission(
                "replace it",
                vec![],
                vec![]
            )))
            .as_slice(),
            [LoopEvent::CommandRejected { .. } | LoopEvent::ChatRejected { .. }]
        ));
        assert_eq!(
            runtime.thread_goal("thread-1").unwrap().goal_id,
            snapshot.goal_id
        );
    }

    #[test]
    fn submit_goal_creates_durable_generic_goal_bound_to_root_chat() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bus = Arc::new(EventBus::new(256));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            Arc::new(HeldModel::default()),
        );
        let mut sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone());
        let temp = tempfile::tempdir().unwrap();
        let storage_config = StorageConfig {
            db_path: temp.path().join("events.db"),
            ..Default::default()
        };
        let storage = Storage::open(storage_config.clone()).unwrap();
        let (bridge, persisted) = spawn_test_bridge(&rt, bus, storage.handle());
        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "investigate without a PR",
            vec![],
            vec![],
        )));
        let (goal_id, root_run_id) = rt.block_on(persisted).unwrap();
        let snapshot = runtime.thread_goal("thread-1").unwrap();
        assert_eq!(snapshot.goal_id, goal_id);
        assert_eq!(snapshot.root_run_id, root_run_id);
        let stored = Database::open(&storage_config)
            .unwrap()
            .events_all_ordered()
            .unwrap();
        assert!(stored.iter().any(|stored| matches!(&stored.event.kind, EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) if snapshot.goal_id == goal_id)));
        bridge.abort();
        let _ = rt.block_on(bridge);
        runtime
            .cancel(sink.host.service_mut().bindings.chat_runs["thread-1"])
            .unwrap();
        rt.block_on(runtime.wait(sink.host.service_mut().bindings.chat_runs["thread-1"]))
            .unwrap();
        storage.close();
    }
}
