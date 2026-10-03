//! goal 投入を runtime の background run 起動 + GoalSupervisor へ接続する
//! production CommandSink (issue #71, #73)。

// allow: SIZE_OK - RuntimeCommandSink 本体に、pinned された 9 件の振る舞いテスト
// (stub モデル込み) が inline テスト慣習どおり同居するため分割不可能。
// テストを別ファイルへ分離すると impl+test ペアリング規約に反する。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use event_bus::{ApprovalDecision, Event, EventBus, GoalReference};
use runtime::orchestration::supervisor::SupervisorError;
use runtime::{AgentRuntime, GoalSpec, Role, RunConfig, RunId, SupervisorHandle};

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
    event_bus: Option<Arc<EventBus>>,
    team_writer: Option<storage::StorageHandle>,
    memory_config: Option<storage::StorageConfig>,
    runtime: AgentRuntime,
    shell_cwd: Option<PathBuf>,
    handle: tokio::runtime::Handle,
    supervisor: SupervisorHandle,
    accepted_goals: u64,
    repo_identity: OnceLock<RepoIdentity>,
    chat_runs: BTreeMap<String, RunId>,
    goal_runs: BTreeMap<String, RunId>,
    goal_ids: BTreeMap<String, String>,
    running_children: BTreeMap<String, usize>,
    // A second press must escalate before the async root phase changes.
    stop_marked: BTreeSet<String>,
    // Retain resume eligibility even after stage two clears stop_marked.
    stopped_by_us: BTreeSet<String>,
    goal_projects: BTreeMap<String, String>,
    chat_permits: BTreeMap<String, runtime::ownership::OwnerPermit>,
    ownership: Option<std::sync::Arc<runtime::ownership::OwnerHost>>,
    events_tx: std::sync::mpsc::Sender<LoopEvent>,
    events_rx: std::sync::mpsc::Receiver<LoopEvent>,
}

impl RuntimeCommandSink {
    pub fn start_background_run(&self, text: String) -> RunId {
        let _guard = self.handle.enter();
        self.runtime.delegate_background(
            Role::Worker,
            text,
            RunConfig {
                interactive: false,
                keep_alive: false,
                ..RunConfig::default()
            },
        )
    }

    /// runtime, tokio ハンドル, supervisor handle から sink を生成する。
    pub fn new(
        runtime: AgentRuntime,
        handle: tokio::runtime::Handle,
        supervisor: SupervisorHandle,
    ) -> Self {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        Self {
            event_bus: None,
            team_writer: None,
            memory_config: None,
            runtime,
            shell_cwd: None,
            handle,
            supervisor,
            accepted_goals: 0,
            repo_identity: OnceLock::new(),
            chat_runs: BTreeMap::new(),
            goal_runs: BTreeMap::new(),
            goal_ids: BTreeMap::new(),
            running_children: BTreeMap::new(),
            stop_marked: BTreeSet::new(),
            stopped_by_us: BTreeSet::new(),
            goal_projects: BTreeMap::new(),
            chat_permits: BTreeMap::new(),
            ownership: None,
            events_tx,
            events_rx,
        }
    }

    pub fn with_web_tools_enabled(self, enabled: bool) -> Self {
        self.runtime.set_web_tools_enabled(enabled);
        self
    }

    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    pub fn with_ownership(
        mut self,
        ownership: std::sync::Arc<runtime::ownership::OwnerHost>,
    ) -> Self {
        self.ownership = Some(ownership);
        self
    }

    pub fn with_memory_storage(mut self, config: storage::StorageConfig) -> Self {
        self.memory_config = Some(config);
        self
    }

    pub fn with_team_writer(mut self, writer: storage::StorageHandle) -> Self {
        self.team_writer = Some(writer);
        self
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

    fn can_restart_stopped(&self, thread: &str, error: &runtime::RuntimeError) -> bool {
        (self.stop_marked.contains(thread) || self.stopped_by_us.contains(thread))
            && matches!(
                error,
                runtime::RuntimeError::RunRestoreFailed {
                    reason: runtime::RunRestoreFailure::MissingContext
                        | runtime::RunRestoreFailure::StorageNotConfigured,
                    ..
                }
            )
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
    fn follow_up_status(&self, thread: &str) -> Option<runtime::FollowUpStatus> {
        let run = self
            .chat_runs
            .get(thread)
            .or_else(|| self.goal_runs.get(thread))?;
        self.runtime.follow_up_status(*run).ok()
    }
    fn bind_goal_id(&mut self, thread: &str, goal: &str) {
        self.goal_ids.insert(thread.into(), goal.into());
    }

    fn running_children(&mut self, thread: &str) -> Option<usize> {
        let run = *self
            .chat_runs
            .get(thread)
            .or_else(|| self.goal_runs.get(thread))?;
        let count = self.runtime.live_descendants(run).len();
        self.running_children.insert(thread.into(), count);
        Some(count)
    }

    fn observe_lifecycle(&mut self, event: &Event) {
        if matches!(event.kind, event_bus::EventKind::Lifecycle(_)) {
            // Refresh all bound roots: a stopped/completed child is no longer in
            // live_descendants, so filtering by the live set would miss it.
            let threads: std::collections::BTreeSet<_> = self
                .chat_runs
                .keys()
                .chain(self.goal_runs.keys())
                .cloned()
                .collect();
            for thread in threads {
                self.running_children(&thread);
            }
        }
    }

    fn bind_goal_context(&mut self, thread: &str, project: &str, run: &str) {
        if let Some(id) = run.strip_prefix("run-").and_then(|s| s.parse::<u64>().ok()) {
            self.goal_runs.insert(thread.into(), RunId::new(id));
            self.goal_projects.insert(thread.into(), project.into());
        }
    }
    fn restore_diagnostics(
        &self,
        run: &str,
    ) -> Result<Option<runtime::restore::RunRestoreDiagnostics>, String> {
        let id = run
            .strip_prefix("run-")
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or("invalid run ID")?;
        self.runtime
            .restore_diagnostics(RunId::new(id))
            .map_err(|e| e.to_string())
    }

    fn set_default_cwd(&mut self, cwd: Option<PathBuf>) -> Result<(), String> {
        // cwd 未指定時は起動済み executor を維持し、不要な sandbox 構築を避ける。
        let Some(root) = cwd else {
            self.shell_cwd = None;
            return Ok(());
        };
        if self.shell_cwd.as_ref() == Some(&root) {
            return Ok(());
        }
        self.runtime
            .set_default_cwd(root.clone())
            .map_err(|error| error.to_string())?;
        self.shell_cwd = Some(root);
        Ok(())
    }

    fn start_background_run(&self, text: String) -> Option<RunId> {
        Some(RuntimeCommandSink::start_background_run(self, text))
    }

    fn submit_chat_with_permit(
        &mut self,
        chat: crate::model::commands::ChatSubmission,
        permit: runtime::ownership::OwnerPermit,
    ) -> Vec<LoopEvent> {
        let validation = (|| {
            if permit.thread_id != chat.thread_id {
                return Err(runtime::ownership::OwnershipError::Fenced.into());
            }
            if let Some(host) = &self.ownership {
                let current = host.owned_permit(&chat.thread_id)?;
                if current.registry_path != permit.registry_path
                    || current.lease.owner_id != permit.lease.owner_id
                    || current.lease.generation != permit.lease.generation
                {
                    return Err(runtime::ownership::OwnershipError::Fenced.into());
                }
            }
            permit.validate_generation()
        })();
        match validation {
            Ok(()) => self.submit_authorized(WorkbenchCommand::SendChat(chat), Some(permit)),
            Err(error) => vec![LoopEvent::ChatRejected {
                thread_id: chat.thread_id,
                reason: error.to_string(),
            }],
        }
    }

    fn poll(&mut self) -> Vec<LoopEvent> {
        self.events_rx.try_iter().collect()
    }

    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        let permit = if let Some(host) = &self.ownership {
            let thread = match &command {
                WorkbenchCommand::SendChat(value) => Some(value.thread_id.as_str()),
                WorkbenchCommand::ContinueChat(value) => Some(value.thread_id.as_str()),
                WorkbenchCommand::SubmitGoal(value) => Some(value.thread_id.as_str()),
                WorkbenchCommand::StopChat { thread_id }
                | WorkbenchCommand::DeliverFollowUpsNextTurn { thread_id }
                | WorkbenchCommand::CancelChat { thread_id }
                | WorkbenchCommand::AnswerUserQuestion { thread_id, .. } => {
                    Some(thread_id.as_str())
                }
                WorkbenchCommand::DecideMerge(value) => Some(value.thread_id.as_str()),
                WorkbenchCommand::RestoreSnapshot { .. } => None,
                WorkbenchCommand::PauseGoal { .. }
                | WorkbenchCommand::DecideToolApproval { .. }
                | WorkbenchCommand::SetWebToolsEnabled { .. }
                | WorkbenchCommand::ResumeGoal { .. }
                | WorkbenchCommand::CancelGoal { .. } => None,
            };
            match thread.map(|thread| host.owned_permit(thread)).transpose() {
                Ok(permit) => permit,
                Err(error) => {
                    return vec![LoopEvent::CommandRejected {
                        reason: error.to_string(),
                    }];
                }
            }
        } else {
            None
        };
        self.submit_authorized(command, permit)
    }
}

impl RuntimeCommandSink {
    fn submit_authorized(
        &mut self,
        command: WorkbenchCommand,
        permit: Option<runtime::ownership::OwnerPermit>,
    ) -> Vec<LoopEvent> {
        match command {
            WorkbenchCommand::AnswerUserQuestion {
                thread_id,
                question_id,
                answer,
            } => {
                let question = match self.runtime.user_question(&question_id) {
                    Ok(Some(q)) => q,
                    Ok(None) => {
                        return vec![LoopEvent::CommandRejected {
                            reason: "Unknown question".into(),
                        }];
                    }
                    Err(reason) => return vec![LoopEvent::CommandRejected { reason }],
                };
                let chat_role = [Role::Worker, Role::Orchestrator]
                    .into_iter()
                    .find(|role| question.root_name == format!("chat:{}:{thread_id}", role.name()));
                let bound_runs = self
                    .goal_runs
                    .get(&thread_id)
                    .into_iter()
                    .chain(self.chat_runs.get(&thread_id))
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                // Consumer routing is not execution ownership. The shared helper
                // matches UI projection; the caller and live recipients are fenced
                // separately before any durable answer is written.
                if !question.belongs_to_thread(&thread_id, &bound_runs) {
                    return vec![LoopEvent::CommandRejected { reason:"Question belongs to a different or inactive conversation; resume its goal first".into() }];
                }
                if permit
                    .as_ref()
                    .is_some_and(|permit| permit.thread_id != thread_id)
                {
                    return vec![LoopEvent::CommandRejected {
                        reason: runtime::ownership::OwnershipError::Fenced.to_string(),
                    }];
                }
                // Offline requesters have no live permit for the runtime to guard.
                // Keep the submitting host's generation stable through durable storage.
                let caller_guard = match permit.as_ref().map(|p| p.mutation_guard()).transpose() {
                    Ok(guard) => guard,
                    Err(error) => {
                        return vec![LoopEvent::CommandRejected {
                            reason: error.to_string(),
                        }];
                    }
                };
                if let Some(previous) = &question.answer {
                    return if previous == &answer {
                        vec![LoopEvent::UserAnswerSaved { question_id }]
                    } else {
                        vec![LoopEvent::CommandRejected {
                            reason: "Question was already answered".into(),
                        }]
                    };
                }
                let question = match self.runtime.answer_user_question(&question_id, &answer) {
                    Ok(q) => q,
                    Err(reason) => return vec![LoopEvent::CommandRejected { reason }],
                };
                // Resumption writes the ownership registry, so release its read guard first.
                drop(caller_guard);
                let saved = LoopEvent::UserAnswerSaved {
                    question_id: question_id.clone(),
                };
                let active = match self.runtime.has_active_question_recipient(&question_id) {
                    Ok(active) => active,
                    Err(reason) => {
                        return vec![
                            saved,
                            LoopEvent::CommandRejected {
                                reason: format!(
                                    "回答は保存済みですが受信先を確認できません: {reason}"
                                ),
                            },
                        ];
                    }
                };
                if active {
                    return vec![saved];
                }
                let text = format!(
                    "[user-answer id={}]\nQuestion: {}\nAnswer: {}",
                    question.id, question.title, answer
                );
                let mut events = self.submit_authorized(
                    WorkbenchCommand::SendChat(crate::model::commands::ChatSubmission {
                        thread_id,
                        text,
                        images: Vec::new(),
                        model_preference: None,
                        composer_role: if chat_role == Some(Role::Worker) {
                            crate::model::composer::ComposerRole::Worker
                        } else {
                            crate::model::composer::ComposerRole::Orchestrator
                        },
                    }),
                    permit,
                );
                events.insert(0, saved);
                for event in &mut events {
                    if let LoopEvent::ChatRejected { reason, .. } = event {
                        *reason = format!(
                            "回答は保存済みですが会話の再開に失敗しました: {reason}。原因を解消して会話を再開してください。"
                        );
                    }
                }
                events
            }
            WorkbenchCommand::SetWebToolsEnabled { enabled } => {
                self.runtime.set_web_tools_enabled(enabled);
                Vec::new()
            }
            WorkbenchCommand::DecideToolApproval { call_id, approved } => {
                if let Some(bus) = &self.event_bus {
                    bus.emit(Event::new(event_bus::ToolEvent::ApprovalResolved {
                        call_id,
                        approved,
                    }));
                }
                Vec::new()
            }
            WorkbenchCommand::RestoreSnapshot { thread_id, redo } => {
                let Some(&run) = self.chat_runs.get(&thread_id) else {
                    return vec![LoopEvent::ChatRejected {
                        thread_id,
                        reason: "No chat snapshot available".into(),
                    }];
                };
                let runtime = self.runtime.clone();
                let tx = self.events_tx.clone();
                self.handle.spawn(async move {
                    let event = match runtime.restore_snapshot(run, redo).await {
                        Ok(diff) => LoopEvent::SnapshotRestored { thread_id, diff },
                        Err(reason) => LoopEvent::ChatRejected { thread_id, reason },
                    };
                    let _ = tx.send(event);
                });
                Vec::new()
            }
            WorkbenchCommand::DeliverFollowUpsNextTurn { thread_id } => {
                let result = self
                    .chat_runs
                    .get(&thread_id)
                    .or_else(|| self.goal_runs.get(&thread_id))
                    .ok_or_else(|| "No conversation to deliver follow-ups to".to_owned())
                    .and_then(|run| {
                        self.runtime
                            .deliver_follow_ups_next_turn(*run)
                            .map_err(|error| error.to_string())
                    });
                match result {
                    Ok(()) => Vec::new(),
                    Err(reason) => vec![LoopEvent::ChatRejected { thread_id, reason }],
                }
            }
            WorkbenchCommand::StopChat { thread_id } => {
                let Some(&run_id) = self
                    .chat_runs
                    .get(&thread_id)
                    .or_else(|| self.goal_runs.get(&thread_id))
                else {
                    return vec![LoopEvent::ChatNotice {
                        thread_id,
                        text: "No chat run to stop".into(),
                    }];
                };
                let scope = if self.stop_marked.contains(&thread_id) {
                    runtime::StopScope::Subtree
                } else {
                    match self.runtime.inspect_agent(run_id) {
                        Ok(run) => {
                            match run.phase {
                                event_bus::AgentRunPhase::Pending
                                | event_bus::AgentRunPhase::Running
                                | event_bus::AgentRunPhase::Waiting => runtime::StopScope::SelfOnly,
                                event_bus::AgentRunPhase::Stopped
                                    if !self.runtime.live_descendants(run_id).is_empty() =>
                                {
                                    runtime::StopScope::Subtree
                                }
                                _ => {
                                    return vec![LoopEvent::ChatNotice {
                                    thread_id,
                                    text: "Already stopped; use /continue or send a message to resume".into(),
                                }];
                                }
                            }
                        }
                        // Admission-pending runs have no inspection yet. stop validates
                        // the ID against admissions before accepting this first press.
                        Err(runtime::RuntimeError::UnknownRun { .. }) => {
                            runtime::StopScope::SelfOnly
                        }
                        Err(error) => {
                            return vec![LoopEvent::ChatRejected {
                                thread_id,
                                reason: error.to_string(),
                            }];
                        }
                    }
                };
                let mut events = Vec::new();
                // Fence supervisor dispatch before a stopped root/child can complete.
                if matches!(scope, runtime::StopScope::SelfOnly)
                    && let Some(goal_id) = self.goal_ids.get(&thread_id)
                    && let Err(error) = self.supervisor.pause(goal_id)
                {
                    events.push(LoopEvent::ChatNotice {
                        thread_id: thread_id.clone(),
                        text: format!("Run stopped; goal pause failed: {error}"),
                    });
                }
                if let Err(error) = self.runtime.stop(run_id, scope) {
                    return vec![LoopEvent::ChatRejected {
                        thread_id,
                        reason: error.to_string(),
                    }];
                }
                match scope {
                    runtime::StopScope::SelfOnly => {
                        self.stop_marked.insert(thread_id.clone());
                    }
                    runtime::StopScope::Subtree => {
                        self.stop_marked.remove(&thread_id);
                    }
                }
                self.stopped_by_us.insert(thread_id.clone());
                let running_children = self.running_children(&thread_id).unwrap_or(0);
                events.push(LoopEvent::ChatStopped {
                    thread_id,
                    run_id: run_id.to_string(),
                    running_children,
                });
                events
            }
            WorkbenchCommand::CancelChat { thread_id } => {
                let Some(&run_id) = self
                    .chat_runs
                    .get(&thread_id)
                    .or_else(|| self.goal_runs.get(&thread_id))
                else {
                    return vec![LoopEvent::ChatRejected {
                        thread_id,
                        reason: "No chat run to cancel".into(),
                    }];
                };
                match self.runtime.cancel_subtree(run_id) {
                    Ok(()) => {
                        self.chat_runs.remove(&thread_id);
                        self.stop_marked.remove(&thread_id);
                        self.stopped_by_us.remove(&thread_id);
                        self.running_children.remove(&thread_id);
                        Vec::new()
                    }
                    Err(error) => vec![LoopEvent::ChatRejected {
                        thread_id,
                        reason: error.to_string(),
                    }],
                }
            }
            WorkbenchCommand::SubmitGoal(submission) => {
                let team_store = match submission.delegation_value.as_deref() {
                    Some(value) if value.trim().is_empty() => {
                        return vec![LoopEvent::CommandRejected {
                            reason: "team mode requires explicit delegation value".into(),
                        }];
                    }
                    Some(_) => match (&self.memory_config, &self.team_writer) {
                        (Some(config), Some(writer)) => Some(runtime::team_context::TeamStore {
                            config: config.clone(),
                            writer: writer.clone(),
                            id: format!("{}:{}", submission.project_id, submission.thread_id),
                        }),
                        _ => {
                            return vec![LoopEvent::CommandRejected {
                                reason: "team storage is unavailable".into(),
                            }];
                        }
                    },
                    None => None,
                };
                self.goal_projects
                    .insert(submission.thread_id.clone(), submission.project_id.clone());
                let delegation_value = submission.delegation_value.clone();
                let memory = match self
                    .memory_config
                    .as_ref()
                    .map(|config| {
                        runtime::memory::MemoryBoundary::capture(config, &submission.project_id)
                    })
                    .transpose()
                {
                    Ok(memory) => memory,
                    Err(error) => {
                        return vec![LoopEvent::CommandRejected {
                            reason: error.to_string(),
                        }];
                    }
                };
                self.accepted_goals = self.accepted_goals.saturating_add(1);
                let goal_id = format!("goal-{}", self.accepted_goals);
                let prompt = render_entry_prompt(&submission);
                let runtime = self.runtime.clone();
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
                let finding_store = self
                    .memory_config
                    .as_ref()
                    .map(|config| config.db_path.clone());
                let root_run = runtime.reserve_run_id();
                self.goal_runs.insert(thread_id.clone(), root_run);
                // Supervisor IDs are durable unique IDs, not the local goal-N
                // acknowledgement/run label. Bind the real ID before spawning.
                let supervisor_goal_id = self.supervisor.create_goal(spec, root_run);
                self.goal_ids.insert(thread_id.clone(), supervisor_goal_id);
                self.handle.spawn(async move {
                    let decision = runtime.entry_router().classify(&goal_for_log).await;
                    runtime.spawn_reserved(
                        root_run,
                        None,
                        if team_store.is_some() {
                            Role::Orchestrator
                        } else {
                            decision.role()
                        },
                        prompt,
                        RunConfig {
                            name: Some(goal_id_for_run),
                            topology: if team_store.is_some() {
                                runtime::CoordinationTopology::DynamicTeam { max_workers: 3 }
                            } else {
                                runtime::CoordinationTopology::Single
                            },
                            team_store,
                            delegation_value,
                            finding_store,
                            memory,
                            ownership: permit,
                            ..RunConfig::default()
                        },
                    );
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
            WorkbenchCommand::SendChat(submission) => self.submit_chat(submission, permit, false),
            WorkbenchCommand::ContinueChat(continuation) => self.submit_chat(
                crate::model::commands::ChatSubmission {
                    thread_id: continuation.thread_id,
                    composer_role: continuation.composer_role,
                    model_preference: continuation.model_preference,
                    images: Vec::new(),
                    // The command is current human intent to continue, not replayed
                    // history or permission to repeat interrupted side effects.
                    text: AgentRuntime::CHAT_CONTINUE_PROMPT.into(),
                },
                permit,
                true,
            ),
        }
    }

    fn submit_chat(
        &mut self,
        submission: crate::model::commands::ChatSubmission,
        permit: Option<runtime::ownership::OwnerPermit>,
        resume_only: bool,
    ) -> Vec<LoopEvent> {
        let thread_id = submission.thread_id;
        if resume_only {
            if !self.chat_runs.contains_key(&thread_id) && !self.goal_runs.contains_key(&thread_id)
            {
                match self.runtime.latest_chat_run(&thread_id) {
                    Ok(Some(run)) => {
                        self.chat_runs.insert(thread_id.clone(), run);
                    }
                    Ok(None) => {
                        return vec![LoopEvent::ChatNotice {
                            thread_id,
                            text: "No conversation to continue; send a message first".into(),
                        }];
                    }
                    Err(error) => {
                        return vec![LoopEvent::ChatRejected {
                            thread_id,
                            reason: error.to_string(),
                        }];
                    }
                }
            }
            let run = self
                .chat_runs
                .get(&thread_id)
                .or_else(|| self.goal_runs.get(&thread_id))
                .copied()
                .expect("resolved conversation root");
            if self.runtime.inspect_agent(run).ok().is_some_and(|run| {
                matches!(
                    run.phase,
                    event_bus::AgentRunPhase::Pending | event_bus::AgentRunPhase::Running
                )
            }) {
                return vec![LoopEvent::ChatNotice {
                    thread_id,
                    text: "Run is still running or stopping; try /continue once it settles".into(),
                }];
            }
        }

        if let Some(&run_id) = self.goal_runs.get(&thread_id) {
            // A missing-context fallback may have replaced the original goal root.
            let run_id = self.chat_runs.get(&thread_id).copied().unwrap_or(run_id);
            let mut authority = RunConfig {
                ownership: permit.clone(),
                images: submission.images.clone(),
                model_preference: submission.model_preference.clone(),
                ..RunConfig::default()
            };
            if self
                .runtime
                .restore_diagnostics(run_id)
                .ok()
                .flatten()
                .is_some_and(|d| d.renewable_team.is_some())
            {
                let Some(project) = self.goal_projects.get(&thread_id) else {
                    return vec![LoopEvent::ChatRejected {
                        thread_id,
                        reason: "Current project binding is required to continue this team".into(),
                    }];
                };
                let (Some(config), Some(writer)) = (&self.memory_config, &self.team_writer) else {
                    return vec![LoopEvent::ChatRejected {
                        thread_id,
                        reason: "Current team storage is unavailable".into(),
                    }];
                };
                authority.team_store = Some(runtime::team_context::TeamStore {
                    config: config.clone(),
                    writer: writer.clone(),
                    id: format!("{project}:{thread_id}"),
                });
                authority.topology = runtime::CoordinationTopology::DynamicTeam { max_workers: 3 };
                authority.delegation_value = Some(submission.text.clone());
                authority.finding_store = Some(config.db_path.clone());
                authority.memory = match runtime::memory::MemoryBoundary::capture(config, project) {
                    Ok(memory) => Some(memory),
                    Err(error) => {
                        return vec![LoopEvent::ChatRejected {
                            thread_id,
                            reason: error.to_string(),
                        }];
                    }
                };
            }
            let _guard = self.handle.enter();
            match self
                .runtime
                .continue_goal(run_id, submission.text.clone(), authority)
            {
                Ok(run_id) => {
                    self.chat_runs.insert(thread_id.clone(), run_id);
                    self.stop_marked.remove(&thread_id);
                    self.stopped_by_us.remove(&thread_id);
                    let mut events = vec![LoopEvent::ChatAccepted {
                        thread_id: thread_id.clone(),
                        run_id: run_id.to_string(),
                    }];
                    // continue_goal re-registers the root before the supervisor
                    // can dispatch another continuation.
                    if let Some(goal_id) = self.goal_ids.get(&thread_id)
                        && self.supervisor.snapshot(goal_id).is_some_and(|goal| {
                            // Detached Resume would recover a second root after continue_goal.
                            !goal.detached && goal.state == event_bus::GoalState::Paused
                        })
                        && let Err(error) = self.supervisor.resume(goal_id)
                    {
                        events.push(LoopEvent::ChatNotice {
                            thread_id,
                            text: format!("Run resumed; goal resume failed: {error}"),
                        });
                    }
                    return events;
                }
                Err(error) if !resume_only && self.can_restart_stopped(&thread_id, &error) => {
                    self.stop_marked.remove(&thread_id);
                    self.stopped_by_us.remove(&thread_id);
                    self.chat_runs.remove(&thread_id);
                }
                Err(error) => {
                    return vec![LoopEvent::ChatRejected {
                        thread_id,
                        reason: error.to_string(),
                    }];
                }
            }
        }
        if let Some(permit) = &permit {
            if !resume_only
                && self.chat_permits.get(&thread_id).is_some_and(|previous| {
                    previous.registry_path != permit.registry_path
                        || previous.lease.owner_id != permit.lease.owner_id
                        || previous.lease.generation != permit.lease.generation
                })
            {
                self.chat_runs.remove(&thread_id);
            }
            self.chat_permits.insert(thread_id.clone(), permit.clone());
        }
        if let Some(&run_id) = self.chat_runs.get(&thread_id) {
            // continue_goal preserves the saved role, regardless of the current composer.
            let conversation = self
                .runtime
                .restore_diagnostics(run_id)
                .ok()
                .flatten()
                .is_some_and(|saved| saved.role_name == Role::Worker.name());
            let _guard = self.handle.enter();
            match self.runtime.continue_goal(
                run_id,
                submission.text.clone(),
                RunConfig {
                    conversation,
                    category: conversation.then(|| "conversation".into()),
                    ownership: permit.clone(),
                    images: submission.images.clone(),
                    model_preference: submission.model_preference.clone(),
                    ..RunConfig::default()
                },
            ) {
                Ok(run_id) => {
                    self.stop_marked.remove(&thread_id);
                    self.stopped_by_us.remove(&thread_id);
                    return vec![LoopEvent::ChatAccepted {
                        thread_id,
                        run_id: run_id.to_string(),
                    }];
                }
                Err(error) if !resume_only && self.can_restart_stopped(&thread_id, &error) => {
                    self.stop_marked.remove(&thread_id);
                    self.stopped_by_us.remove(&thread_id);
                    self.chat_runs.remove(&thread_id);
                }
                Err(error) => {
                    return vec![LoopEvent::ChatRejected {
                        thread_id,
                        reason: error.to_string(),
                    }];
                }
            }
        }
        if resume_only {
            return vec![LoopEvent::ChatNotice {
                thread_id,
                text: "No conversation to continue; send a message first".into(),
            }];
        }
        let conversation = submission.composer_role == crate::model::composer::ComposerRole::Worker;
        let _guard = self.handle.enter();
        let run_id = self.runtime.delegate_chat(
            &thread_id,
            match submission.composer_role {
                crate::model::composer::ComposerRole::Worker => Role::Worker,
                crate::model::composer::ComposerRole::Orchestrator => Role::Orchestrator,
            },
            submission.text,
            RunConfig {
                conversation,
                category: conversation.then(|| "conversation".into()),
                images: submission.images,
                ownership: permit,
                interactive: true,
                keep_alive: true,
                model_preference: submission.model_preference,
                ..RunConfig::default()
            },
        );
        let run_id = match run_id {
            Ok(run_id) => run_id,
            Err(error) => {
                return vec![LoopEvent::ChatRejected {
                    thread_id,
                    reason: error.to_string(),
                }];
            }
        };
        self.chat_runs.insert(thread_id.clone(), run_id);
        self.stop_marked.remove(&thread_id);
        self.stopped_by_us.remove(&thread_id);
        vec![LoopEvent::ChatAccepted {
            thread_id,
            run_id: run_id.to_string(),
        }]
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
        build_sink_on(rt, Arc::new(HeldModel::default()))
    }

    fn build_sink_on(
        rt: tokio::runtime::Runtime,
        model: Arc<dyn AgentModel>,
    ) -> (
        tokio::runtime::Runtime,
        RuntimeCommandSink,
        AgentRuntime,
        SupervisorHandle,
    ) {
        let bus = Arc::new(EventBus::new(64));
        let executor = Arc::new(ToolExecutor::new(bus.clone()));
        let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
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
        // Given: 承認結果を購読する bus を注入した sink。
        let (rt, sink, _, _) = build_sink();
        let bus = Arc::new(EventBus::new(64));
        let mut subscriber = bus.subscribe();
        let mut sink = sink.with_event_bus(Arc::clone(&bus));

        for approved in [true, false] {
            // When: 相関 ID を含む承認・拒否を提出する。
            let events = sink.submit(WorkbenchCommand::DecideToolApproval {
                call_id: "run-2:call-1:17".into(),
                approved,
            });

            // Then: 即応イベントはなく、相関 ID と判断をそのまま配送する。
            assert!(events.is_empty());
            let event =
                rt.block_on(async { subscriber.recv().await.expect("approval resolved event") });
            assert!(matches!(
                event.kind,
                EventKind::Tool(event_bus::ToolEvent::ApprovalResolved {
                    call_id,
                    approved: actual,
                }) if call_id.as_bytes() == b"run-2:call-1:17" && actual == approved
            ));
        }
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
        // Persisted question from a stopped requester, as after restarting the GUI.
        let question = event_bus::UserQuestion {
            id: "question-offline".into(),
            run_id: "run-99".into(),
            root_run_id: "run-99".into(),
            root_name: "chat:Worker:thread-1".into(),
            recipient_run_ids: Vec::new(),
            title: "Which format?".into(),
            options: vec!["JSON".into()],
            blocking: true,
            answer: None,
        };
        storage.handle().create_user_question(&question).unwrap();
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
        let run = sink.chat_runs["thread-1"];
        runtime.cancel(run).unwrap();
        rt.block_on(async {
            runtime.wait(run).await.unwrap();
        });
    }

    #[test]
    fn inherited_question_requires_registered_recipient_and_current_thread_ownership() {
        let (_rt, mut sink, runtime, _) = build_sink();
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("questions.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        sink.submit(chat_command("recipient"));
        let recipient = sink.chat_runs["recipient"];
        let question = event_bus::UserQuestion {
            id: "inherited".into(),
            run_id: "run-99".into(),
            root_run_id: "run-99".into(),
            root_name: "chat:Worker:source".into(),
            recipient_run_ids: Vec::new(),
            title: "Which format?".into(),
            options: vec![],
            blocking: true,
            answer: None,
        };
        storage.handle().create_user_question(&question).unwrap();
        storage
            .handle()
            .bind_user_questions(
                "run-99",
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
            id: "child".into(),
            root_run_id: "run-98".into(),
            ..question.clone()
        };
        storage.handle().create_user_question(&child).unwrap();
        storage
            .handle()
            .bind_user_questions(
                "run-99",
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
            composer_role: crate::model::composer::ComposerRole::Orchestrator,
            images: Vec::new(),
            thread_id: thread.into(),
            text: "hello again".into(),
            model_preference: None,
        })
    }

    fn continue_command(thread: &str) -> WorkbenchCommand {
        WorkbenchCommand::ContinueChat(crate::model::commands::ChatContinuation {
            thread_id: thread.into(),
            composer_role: crate::model::composer::ComposerRole::Worker,
            model_preference: None,
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
        let run = sink.chat_runs["no-store"];
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
        assert_eq!(sink.chat_runs["no-store"], run);
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
            self.started.notify_one();
            if self.fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(RuntimeError::Model {
                    reason: "test failure".into(),
                });
            }
            if messages.len() > 1 {
                assert!(messages.iter().any(|m| m.content.iter().any(|block|
                    matches!(block, providers::ContentBlock::Text { text } if text == AgentRuntime::CHAT_CONTINUE_PROMPT))));
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
                });
                let (rt, mut sink, runtime, supervisor) =
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
                let run = sink.chat_runs["resume"];
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
                    sink =
                        RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor);
                }
                let events = sink.submit(continue_command("resume"));
                assert!(
                    matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == run.to_string()),
                    "{events:?}"
                );
                assert_eq!(sink.chat_runs["resume"], run);
                assert_eq!(runtime.list_agents().len(), 1);
                assert!(!sink.stopped_by_us.contains("resume"));
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
        let root = sink.chat_runs["rapid"];
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
            sink.stop_marked.contains("rapid"),
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
            !sink.stop_marked.contains("rapid"),
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
        assert_ne!(sink.chat_runs["rapid"], root);
        assert!(!sink.stopped_by_us.contains("rapid"));
        assert!(matches!(
            sink.submit(WorkbenchCommand::StopChat {
                thread_id: "rapid".into()
            })
            .as_slice(),
            [LoopEvent::ChatStopped { .. }]
        ));
        assert!(
            sink.stop_marked.contains("rapid"),
            "new run restarts at stage one"
        );
    }

    #[test]
    fn cancel_chat_cancels_descendants_and_fences_late_spawns() {
        let (rt, mut sink, runtime, _) = build_sink();
        sink.submit(chat_command("discard"));
        let root = sink.chat_runs["discard"];
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
        // Stage one has been requested but discard must still reach the descendants.
        sink.stop_marked.insert("discard".into());
        sink.stopped_by_us.insert("discard".into());
        assert!(
            sink.submit(WorkbenchCommand::CancelChat {
                thread_id: "discard".into()
            })
            .is_empty()
        );
        assert!(!sink.chat_runs.contains_key("discard"));
        assert!(!sink.stop_marked.contains("discard"));
        assert!(!sink.stopped_by_us.contains("discard"));
        rt.block_on(async {
            let late = runtime.spawn_reserved(
                runtime.reserve_run_id(),
                Some(grandchild),
                Role::Worker,
                "late",
                RunConfig::default(),
            );
            for run in [root, child, grandchild, late] {
                assert_eq!(
                    runtime.wait(run).await.unwrap(),
                    event_bus::AgentRunPhase::Error
                );
            }
        });
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
            let root = sink.chat_runs["no-context"];
            if goal {
                sink.chat_runs.remove("no-context");
                sink.bind_goal_context("no-context", "project", &root.to_string());
            }
            assert!(matches!(
                sink.submit(WorkbenchCommand::StopChat {
                    thread_id: "no-context".into()
                })
                .as_slice(),
                [LoopEvent::ChatStopped { .. }]
            ));
            assert!(sink.stop_marked.contains("no-context"));
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
            let fresh = sink.chat_runs["no-context"];
            assert_ne!(fresh, root);
            assert!(!sink.stop_marked.contains("no-context"));
            assert!(!sink.stopped_by_us.contains("no-context"));
            // Both goal and plain-chat paths use the new binding on subsequent stops.
            sink.submit(WorkbenchCommand::StopChat {
                thread_id: "no-context".into(),
            });
            assert!(sink.stop_marked.contains("no-context"));
        }
    }

    #[test]
    fn resume_fallback_requires_our_stop_and_only_absent_context() {
        let (_rt, mut sink, _, _) = build_sink();
        for goal in [false, true] {
            let id = runtime::RunId::new(999);
            if goal {
                sink.goal_runs.insert("unmarked".into(), id);
            } else {
                sink.chat_runs.insert("unmarked".into(), id);
            }
            assert!(matches!(
                sink.submit(chat_command("unmarked")).as_slice(),
                [LoopEvent::ChatRejected { .. }]
            ));
        }
        sink.stop_marked.insert("marked".into());
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
                composer_role: crate::model::composer::ComposerRole::Orchestrator,
                images: Vec::new(),
                thread_id: "chat-thread".into(),
                text: "hello".into(),
                model_preference: None,
            },
        ));
        let root = sink.chat_runs["chat-thread"];
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
        assert_eq!(sink.chat_runs["chat-thread"], root);
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
        assert_eq!(sink.running_children["chat-thread"], 0);
        assert_eq!(sink.chat_runs["chat-thread"], root);
        assert!(
            matches!(sink.submit(command).as_slice(), [LoopEvent::ChatNotice { text, .. }] if text == "Already stopped; use /continue or send a message to resume")
        );
    }

    #[test]
    fn stop_goal_before_first_followup_pauses_and_send_resumes_supervisor() {
        assert_goal_stop_resume(false, false);
    }

    #[test]
    fn stopped_detached_goal_send_does_not_dispatch_recovery_root() {
        assert_goal_stop_resume(true, false);
    }

    #[test]
    fn continue_resumes_goal_without_a_duplicate_root() {
        for detached in [false, true] {
            assert_goal_stop_resume(detached, true);
        }
    }

    fn assert_goal_stop_resume(detached: bool, continue_only: bool) {
        let model = Arc::new(HeldModel::default());
        let (rt, mut sink, runtime, supervisor) =
            build_sink_on(tokio::runtime::Runtime::new().unwrap(), model.clone());
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("stop.sqlite3"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "implement stop",
            vec![],
            vec![],
        )));
        let root = sink.goal_runs["thread-1"];
        let goal_id = sink.goal_ids["thread-1"].clone();
        wait_for_goal_state(&rt, &supervisor, &goal_id, GoalState::Active);
        // Registration happens before the initial conversation is constructed.
        // Model entry follows context initialization and checkpoint persistence,
        // so stopping here must leave a restorable root.
        rt.block_on(model.started.notified());
        let events = sink.submit(WorkbenchCommand::StopChat {
            thread_id: "thread-1".into(),
        });
        assert!(
            matches!(events.as_slice(), [LoopEvent::ChatStopped { .. }]),
            "{events:?}"
        );
        rt.block_on(async { runtime.wait(root).await.unwrap() });
        wait_for_goal_state(&rt, &supervisor, &goal_id, GoalState::Paused);
        assert_eq!(sink.goal_runs["thread-1"], root);
        if detached {
            supervisor
                .adopt(vec![(supervisor.snapshot(&goal_id).unwrap(), Vec::new())])
                .unwrap();
            rt.block_on(supervisor.synchronize()).unwrap();
            assert!(supervisor.snapshot(&goal_id).unwrap().detached);
        }
        let events = sink.submit(if continue_only {
            continue_command("thread-1")
        } else {
            WorkbenchCommand::SendChat(crate::model::commands::ChatSubmission {
                composer_role: crate::model::composer::ComposerRole::Orchestrator,
                images: Vec::new(),
                thread_id: "thread-1".into(),
                text: "resume with authority".into(),
                model_preference: None,
            })
        });
        assert!(
            matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string()),
            "{events:?}"
        );
        wait_for_goal_state(
            &rt,
            &supervisor,
            &goal_id,
            if detached {
                GoalState::Paused
            } else {
                GoalState::Active
            },
        );
        assert_eq!(sink.chat_runs["thread-1"], root);
        assert!(!sink.stop_marked.contains("thread-1"));
        assert!(!sink.stopped_by_us.contains("thread-1"));
        rt.block_on(model.started.notified());
        rt.block_on(supervisor.synchronize()).unwrap();
        assert_eq!(
            runtime.list_agents().len(),
            1,
            "resume must not spawn a duplicate continuation"
        );
        runtime.cancel(root).unwrap();
    }

    #[test]
    fn stop_before_goal_initialization_restarts_with_a_fresh_root() {
        // Keep the executor idle until Stop is submitted, making the missing
        // conversation case deterministic and separate from in-place restore.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (rt, mut sink, runtime, supervisor) = build_sink_on(rt, Arc::new(HeldModel::default()));
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
        let goal_id = supervisor.create_goal(spec(), root);
        sink.bind_goal_context("thread-1", "evorch", &root.to_string());
        sink.bind_goal_id("thread-1", &goal_id);
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
        wait_for_goal_state(&rt, &supervisor, &goal_id, GoalState::Paused);
        let events = sink.submit(chat_command("thread-1"));
        assert!(matches!(
            events.as_slice(),
            [LoopEvent::ChatAccepted { .. }]
        ));
        assert_ne!(sink.chat_runs["thread-1"], root);
    }

    #[test]
    fn stop_chat_without_run_is_an_informational_notice() {
        let (_rt, mut sink, _, _) = build_sink();
        assert!(
            matches!(sink.submit(WorkbenchCommand::StopChat { thread_id: "missing".into() }).as_slice(),
            [LoopEvent::ChatNotice { text, .. }] if text == "No chat run to stop")
        );
    }

    #[test]
    fn cancel_chat_cancels_running_run_and_forgets_it() {
        // Given: a model that holds the run until cancellation.
        let (rt, mut sink, runtime, _) = build_sink();
        let chat = WorkbenchCommand::SendChat(crate::model::commands::ChatSubmission {
            composer_role: crate::model::composer::ComposerRole::Worker,
            images: Vec::new(),
            thread_id: "chat-thread".into(),
            text: "hello".into(),
            model_preference: None,
        });
        sink.submit(chat.clone());
        let first = sink.chat_runs["chat-thread"];
        // When: cancel and immediately submit again, before waiting for termination.
        assert!(
            sink.submit(WorkbenchCommand::CancelChat {
                thread_id: "chat-thread".into(),
            })
            .is_empty()
        );
        sink.submit(chat);
        // Then: the old run terminates and the next message uses a fresh run.
        assert_ne!(sink.chat_runs["chat-thread"], first);
        let phase = rt.block_on(async { runtime.wait(first).await.expect("run exists") });
        assert_eq!(phase, event_bus::AgentRunPhase::Error);
    }

    #[test]
    fn cancel_bound_orchestrator_before_first_follow_up() {
        let (rt, mut sink, runtime, _) = build_sink();
        let root = rt.block_on(async {
            runtime.delegate_background(Role::Orchestrator, "inspect".into(), RunConfig::default())
        });
        sink.bind_goal_context("child-thread", "project", &root.to_string());
        assert!(
            sink.submit(WorkbenchCommand::CancelChat {
                thread_id: "child-thread".into()
            })
            .is_empty()
        );
        let phase = rt.block_on(async { runtime.wait(root).await.unwrap() });
        assert_eq!(phase, event_bus::AgentRunPhase::Error);
        assert_eq!(sink.goal_runs["child-thread"], root);
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
            LoopEvent::GoalAccepted { .. }
        ));
        rt.block_on(model.started.notified());
        assert_eq!(runtime.team_tasks().len(), 1);
    }

    #[test]
    fn cancel_chat_without_run_is_rejected() {
        // Given
        let (_rt, mut sink, _, _) = build_sink();
        // When
        let events = sink.submit(WorkbenchCommand::CancelChat {
            thread_id: "missing".into(),
        });
        // Then
        assert!(
            matches!(events.as_slice(), [LoopEvent::ChatRejected { thread_id, .. }] if thread_id == "missing")
        );
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
                        if let EventKind::Orchestrator(OrchestratorEvent::GoalCreated {
                            goal_id,
                            root_run_id,
                            ..
                        }) = event.kind
                            && let Some(persisted) = persisted.take()
                        {
                            let _ = persisted.send((goal_id, root_run_id));
                        }
                    }
                    Err(error) => panic!("storage bridge event: {error:?}"),
                }
            }
        });
        (task, completed)
    }

    /// Subscribe before inspecting so registration cannot be lost between the
    /// state check and the next lifecycle notification.
    fn wait_for_agents(
        rt: &tokio::runtime::Runtime,
        runtime: &AgentRuntime,
        supervisor: &SupervisorHandle,
        predicate: impl Fn(&AgentSummary) -> bool,
    ) -> Vec<AgentSummary> {
        let mut events = supervisor.subscribe();
        rt.block_on(async {
            loop {
                let agents = runtime.list_agents();
                if agents.iter().any(&predicate) {
                    return agents;
                }
                events.recv().await.expect("agent registration event");
            }
        })
    }

    fn wait_for_goal_state(
        rt: &tokio::runtime::Runtime,
        supervisor: &SupervisorHandle,
        goal_id: &str,
        expected: GoalState,
    ) {
        let mut events = supervisor.subscribe();
        rt.block_on(async {
            loop {
                if let Some(snapshot) = supervisor.snapshot(goal_id)
                    && snapshot.state == expected
                {
                    return;
                }
                events.recv().await.expect("goal state event");
            }
        });
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
        let runtime = AgentRuntime::new(Arc::clone(&bus), executor, Arc::new(HeldModel::default()));
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
        let (bridge, persisted) = spawn_test_bridge(&rt, Arc::clone(&bus), storage.handle());

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

        let (goal_id, root_run_id) = rt.block_on(persisted).expect("durable GoalCreated");
        wait_for_agents(&rt, &runtime, &supervisor, |agent| {
            agent.run_id.to_string() == root_run_id
        });
        let stored = Database::open(&storage_config)
            .expect("reader")
            .events_all_ordered()
            .expect("stored events");
        assert!(stored.iter().any(|stored| matches!(
            &stored.event.kind,
            EventKind::Orchestrator(OrchestratorEvent::GoalCreated {
                goal_id: created, root_run_id: root, ..
            }) if created == &goal_id && root == &root_run_id
        )));
        let snapshot = supervisor
            .snapshot(&goal_id)
            .expect("supervisor knows the persisted goal");
        assert_eq!(snapshot.root_run_id, root_run_id);
        assert_eq!(snapshot.state, GoalState::Active);

        bridge.abort();
        let _ = rt.block_on(bridge);
        storage.close();
    }

    // Given: 実 runtime を接続した sink
    // When: direct キーワードつき goal を submit する
    // Then: role Worker・名前 goal-1 の run が現れ、Orchestrator run は現れない
    #[test]
    fn direct_goal_starts_a_worker_run_named_after_the_goal_id() {
        let (rt, mut sink, runtime, supervisor) = build_sink();

        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "direct: fix the typo in README",
            Vec::new(),
            Vec::new(),
        )));

        let agents = wait_for_agents(&rt, &runtime, &supervisor, |agent| {
            agent.role_name == "Worker" && agent.name == "goal-1"
        });
        assert!(!agents.iter().any(|agent| agent.role_name == "Orchestrator"));
    }

    // Given: 実 runtime を接続した sink
    // When: direct キーワードを含まない goal を submit する
    // Then: role Orchestrator・名前 goal-1 の run が現れ、Worker run は現れない
    #[test]
    fn plain_goal_starts_an_orchestrator_run() {
        let (rt, mut sink, runtime, supervisor) = build_sink();

        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "implement issue #65",
            Vec::new(),
            Vec::new(),
        )));

        let agents = wait_for_agents(&rt, &runtime, &supervisor, |agent| {
            agent.role_name == "Orchestrator" && agent.name == "goal-1"
        });
        assert!(!agents.iter().any(|agent| agent.role_name == "Worker"));
    }

    // Given: 実 runtime を接続した sink
    // When: token なし DecideMerge を submit する
    // Then: CommandRejected{reason} が返り、run も 1 つも起動されない
    #[test]
    fn decide_merge_without_token_is_rejected_without_starting_a_run() {
        let (rt, mut sink, runtime, supervisor) = build_sink();

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
        rt.block_on(supervisor.synchronize()).unwrap();
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
        wait_for_goal_state(&rt, &supervisor, &goal_id, GoalState::Active);

        assert!(
            sink.submit(WorkbenchCommand::PauseGoal {
                goal_id: goal_id.clone(),
            })
            .is_empty()
        );
        wait_for_goal_state(&rt, &supervisor, &goal_id, GoalState::Paused);

        assert!(
            sink.submit(WorkbenchCommand::ResumeGoal {
                goal_id: goal_id.clone(),
            })
            .is_empty()
        );
        wait_for_goal_state(&rt, &supervisor, &goal_id, GoalState::Active);

        assert!(
            sink.submit(WorkbenchCommand::CancelGoal {
                goal_id: goal_id.clone(),
            })
            .is_empty()
        );
        wait_for_goal_state(&rt, &supervisor, &goal_id, GoalState::Cancelled);
    }
}
