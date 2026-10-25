//! 単一 AgentRun のTokio実行ループ。

// allow: SIZE_OK — select 駆動の単一 AgentRun 実行ループとその状態 (LoopState) が
// 一体の状態機械であり、分割すると遷移・注入・wake の相互関係が追えなくなる。

mod benchmark;
mod budget;
#[cfg(test)]
mod continuity_tests;
#[cfg(test)]
mod delegate_cleanup_tests;
mod durable;
mod finalization;
mod identical_calls;
mod messages;
mod observability;
mod questions;
mod retracted_delegates;
#[cfg(test)]
mod review_evidence_tests;
mod stop_context;
mod team;
mod tool_calls;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};

use agents::Role;
use event_bus::event::diagnostic_codes;
use event_bus::{AgentRunPhase, CompactionReason, Event, EventBus, LifecycleEvent, MessageEvent};
use providers::{ContentBlock, FinishReason, ToolSpec, Usage};
use tokio::sync::{mpsc, watch};
use tools::ToolExecutor;

use crate::base_context::{self, InitialSystemPromptError};
use crate::compaction;
use crate::compaction::policy::{CompactionLoopState, CompactionSettings, TriggerDecision};
use crate::escalation::detector::EscalationDetector;
use crate::escalation::{EscalationMemo, EscalationSettings};
use crate::prompt::{SystemPromptCatalog, classify};
use crate::rules::{RulesSession, RulesSource};
use crate::runtime::{SandboxFactory, Shared, loop_shared};
use crate::skill::SkillRegistry;
use crate::{
    AgentContext, AgentInvocationContext, AgentModel, ExecutionPolicy, InterruptKind, RunConfig,
    RunId, RunInterrupt, RunMailbox, RunState, WorkspaceInspection,
};
pub(crate) use tool_calls::{
    append_subagent_context_note, retain_backed_tools, standard_tool_specs, visible_tool_specs,
};

pub(crate) struct RunTask {
    pub(crate) run_id: RunId,
    pub(crate) role: Role,
    pub(crate) prompt: String,
    pub(crate) start: RunStart,
    pub(crate) config: RunConfig,
    pub(crate) parent: Option<RunId>,
    pub(crate) ancestry: crate::request_turn::RequestAncestry,
    pub(crate) mailbox: Arc<RunMailbox>,
    pub(crate) handoff: Option<RunHandoff>,
    pub(crate) restored: Option<crate::restore::RestoredState>,
    pub(crate) turn: providers::RequestTurnContext,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunStart {
    WithInput,
    Resume,
}

/// 終端済み run から新規 root run への引き継ぎ。
pub(crate) struct RunHandoff {
    pub(crate) continuity: crate::continuity::Continuity,
    pub(crate) pending_reference_id: Option<String>,
    pub(crate) source_run_id: RunId,
    pub(crate) restored: Option<crate::restore::RestoredState>,
}

pub(crate) struct LoopChannels {
    pub(crate) phase_tx: watch::Sender<AgentRunPhase>,
    pub(crate) message_count_tx: watch::Sender<usize>,
    pub(crate) inbox_rx: mpsc::Receiver<crate::runtime::user_inbox::UserInput>,
    pub(crate) user_inbox: Arc<crate::runtime::user_inbox::UserInbox>,
    pub(crate) cancel_rx: watch::Receiver<RunInterrupt>,
    pub(crate) mailbox_version_rx: watch::Receiver<u64>,
    pub(crate) compact_rx: watch::Receiver<u64>,
    pub(crate) model_preference_rx: watch::Receiver<Option<crate::ModelPreference>>,
    /// 実行中の圧縮を runtime 側 (AgentRuntime::compact) と共有するフラグ。
    pub(crate) compaction_busy: Arc<AtomicBool>,
    /// run の最終 assistant テキストを runtime 表層 (AgentRuntime::run_result)
    /// へ公開する channel。正常終了時のみ `Some` になる。
    pub(crate) result_tx: watch::Sender<Option<String>>,
}

pub(crate) struct LoopShared {
    pub(crate) bus: Arc<EventBus>,
    pub(crate) executor: Arc<ToolExecutor>,
    pub(crate) model: Arc<dyn AgentModel>,
    pub(crate) system_prompts: Option<Arc<SystemPromptCatalog>>,
    pub(crate) system_prompt_preamble: Option<String>,
    pub(crate) skills: Option<Arc<SkillRegistry>>,
    pub(crate) rules: Option<Arc<RulesSource>>,
    pub(crate) compaction: CompactionSettings,
    pub(crate) compaction_configured: bool,
    pub(crate) escalation: EscalationSettings,
    pub(crate) runtime: Weak<Shared>,
}

/// Model switches the loop prepares before a response arrives (ADR 0030). Each one
/// moves the pin along the chain, so more than this means availability keeps flapping.
const MAX_MODEL_SWITCHES: u32 = 16;
/// A registry wait this long is reported as the run's owner-registry activity.
const OWNERSHIP_WAIT_NOTICE: std::time::Duration = std::time::Duration::from_secs(3);

pub(crate) struct LoopState {
    pub(crate) thread_id: Option<String>,
    pub(crate) turn: providers::RequestTurnContext,
    pub(crate) boundary_pending: bool,
    pub(crate) continuity: crate::continuity::Continuity,
    benchmark: Option<crate::benchmark::BenchmarkCheckpoint>,
    benchmark_checked: bool,
    benchmark_replay: bool,
    pub(crate) task: RunTask,
    pub(crate) shared: LoopShared,
    pub(crate) channels: LoopChannels,
    pub(crate) run_state: RunState,
    pub(crate) context: AgentContext,
    policy: ExecutionPolicy,
    pub(crate) tool_specs: Vec<ToolSpec>,
    pub(crate) rules_session: Option<RulesSession>,
    pub(crate) compaction: CompactionLoopState,
    pub(crate) last_usage: Option<Usage>,
    answered_questions: std::collections::HashSet<String>,
    resumed: bool,
    pub(crate) completed_turn_end: Option<usize>,
    pending_user_messages: Vec<crate::runtime::user_inbox::UserInput>,
    pub(crate) goal_wake_pending: bool,
    pub(crate) todo_context_pending: bool,
    pub(crate) todo_context_deferred: bool,
    /// Model switches prepared since the last response; bounds a switch that never settles.
    model_switches: u32,
    /// Whether this run's turn on its thread owner is open: begun at the loop head and
    /// ended only by the checkpoint after a response's tools.
    owner_turn_open: bool,
    /// The activity last reported, restored after an owner-registry wait.
    current_activity: std::sync::Mutex<event_bus::RunActivity>,
    pending_escalation: Option<EscalationMemo>,
    escalation_detector: EscalationDetector,
    pub(crate) budget: crate::budget_tracker::BudgetCounters,
    identical_calls: identical_calls::IdenticalCalls,
    /// The last tool result, named in the reason when a loop guard stops the run.
    last_tool: Option<stop_context::ToolOutcome>,
    retracted_delegates: retracted_delegates::RetractedDelegates,
    durable_task: Option<storage::entity::TaskContinuation>,
    pending_terminal: Option<(RunState, LifecycleEvent)>,
    pub(crate) scratch: Option<crate::scratch::RunScratch>,
    /// The run project's live allowed directories with this run's access, read again
    /// by every command and file tool call. Benchmark runs have none.
    pub(crate) allowed_directories: Option<sandbox::AllowedRegions>,
    /// The thread's read-only directories this run mounted when its sandbox was built
    /// (ADR 0037); fixed for the run's lifetime.
    pub(crate) thread_read_dirs: Vec<std::path::PathBuf>,
    /// What a delegated Worker's sandbox gives besides allowed and thread directories:
    /// system paths, caches, tool output, the thread's scratch and the project root that
    /// thread directories are mounted under. None for runs without their own sandbox.
    pub(crate) worker_base: Option<sandbox::SandboxBoundary>,
    pub(crate) delegated_thread_dirs: crate::thread_read_dirs::DelegatedThreadDirs,
    pub(crate) told_directories: crate::allowed_directories::ToldDirectories,
    pub(crate) allowed_instructions: crate::allowed_instructions::AllowedInstructions,
    /// Allowed directory each run-local skill came from, checked before every load.
    pub(crate) allowed_skill_origins: std::collections::BTreeMap<String, std::path::PathBuf>,
}

pub(crate) async fn run_agent(shared: Weak<Shared>, mut task: RunTask, channels: LoopChannels) {
    let project = shared
        .upgrade()
        .and_then(|runtime| runtime.run_project(&task.config));
    let Some(mut loop_shared) = loop_shared(&shared, project.as_deref()) else {
        return;
    };
    if let Some(runtime) = shared.upgrade()
        && let Some(resolution) = runtime.model_resolution.get()
    {
        resolution.apply(&mut loop_shared.compaction).await;
    }
    let Some(runtime) = crate::AgentRuntime::from_weak(&shared) else {
        return;
    };
    let mut policy = runtime
        .execution_policy(task.role)
        .for_run_config(&task.config, task.parent.is_none());
    // Trusted host binding is checked once for the fixed tool set, and again
    // on every dispatch in case this conversation hands off its ownership.
    if runtime.goal_thread(task.run_id).is_none() {
        policy.capabilities.allowed_tools.remove("todo_write");
    }
    let thread_id = runtime.thread_of_live_run(task.run_id);
    drop(runtime);
    // A sandboxed runtime mounts the run's own project instead of the active one.
    let sandbox_root = shared.upgrade().and_then(|runtime| {
        runtime
            .sandbox_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(|root| {
                project
                    .as_ref()
                    .map_or(root, |project| project.root.clone())
            })
    });
    let benchmark = shared.upgrade().and_then(|runtime| {
        runtime
            .benchmark_replays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&task.run_id)
    });
    let benchmark_replay = benchmark.is_some();
    let benchmark_mode = benchmark_replay
        || shared
            .upgrade()
            .is_some_and(|runtime| runtime.benchmark_recording.get().is_some());
    let restored = task.restored.take();
    let is_restored = restored.is_some();
    let continuity = restored
        .as_ref()
        .map(|state| state.continuity.clone())
        .or_else(|| {
            task.handoff
                .as_ref()
                .map(|handoff| handoff.continuity.clone())
        })
        .unwrap_or_default();
    let told_directories = restored
        .as_ref()
        .map(|restored| (*restored.allowed_directories).clone())
        .unwrap_or_default();
    let allowed_instructions = restored
        .as_ref()
        .map(|restored| (*restored.allowed_instructions).clone())
        .unwrap_or_default();
    let history_only = task.start == RunStart::Resume;
    let boundary_pending = restored
        .as_ref()
        .is_some_and(|state| state.boundary_pending)
        || task.handoff.is_some();
    // An interrupted provider request retries its exact saved input. Restore
    // procedure context only after that response, or a new compaction boundary.
    // A pending handoff seed has never reached its provider and needs it now.
    let todo_context_deferred = history_only
        && !boundary_pending
        && task.handoff.is_none()
        && restored.as_ref().is_some_and(|state| !state.turn_completed);
    let completed_turn_end = restored.as_ref().and_then(|restored| {
        (history_only && restored.turn_completed).then_some(restored.messages.len())
    });
    let context = if let Some(checkpoint) = &benchmark {
        AgentContext::from_restored(
            task.run_id,
            task.role,
            checkpoint.messages.clone(),
            Vec::new(),
        )
    } else {
        match restored {
            Some(restored) => {
                let mut context = AgentContext::from_restored(
                    task.run_id,
                    task.role,
                    restored.messages,
                    restored.checkpoints,
                );
                match restored.trigger {
                    Some(trigger) => context.push_user(&messages::format_agent_message(&trigger)),
                    None if !history_only => {
                        context.push_user(&task.prompt);
                        if let Some(message) = context.messages.last_mut() {
                            message
                                .content
                                .extend(task.config.images.iter().map(|image| {
                                    ContentBlock::Image {
                                        media_type: image.media_type.clone(),
                                        data: image.data.clone(),
                                    }
                                }));
                        }
                    }
                    None => {}
                }
                context
            }
            None => AgentContext::new(task.run_id, task.role),
        }
    };
    let mut state = LoopState {
        thread_id,
        turn: task.turn.clone(),
        boundary_pending,
        continuity,
        benchmark,
        benchmark_checked: benchmark_replay,
        benchmark_replay,
        task,
        shared: loop_shared,
        channels,
        run_state: RunState::new(),
        context,
        policy,
        tool_specs: Vec::new(),
        rules_session: None,
        compaction: CompactionLoopState::default(),
        last_usage: None,
        answered_questions: Default::default(),
        resumed: is_restored,
        completed_turn_end,
        pending_user_messages: Vec::new(),
        goal_wake_pending: false,
        todo_context_pending: true,
        todo_context_deferred,
        model_switches: 0,
        owner_turn_open: false,
        current_activity: std::sync::Mutex::new(event_bus::RunActivity::Idle),
        pending_escalation: None,
        escalation_detector: EscalationDetector::default(),
        budget: crate::budget_tracker::BudgetCounters::default(),
        identical_calls: identical_calls::IdenticalCalls::default(),
        last_tool: None,
        retracted_delegates: retracted_delegates::RetractedDelegates::default(),
        durable_task: None,
        pending_terminal: None,
        scratch: None,
        allowed_directories: None,
        thread_read_dirs: Vec::new(),
        worker_base: None,
        delegated_thread_dirs: Default::default(),
        told_directories,
        allowed_instructions,
        allowed_skill_origins: Default::default(),
    };
    // One boundary covers legacy completion, streaming and official compaction without
    // changing the authorized tools persisted in checkpoints.
    if !state.shared.model.narrows_edit_tools() {
        state.shared.model = Arc::new(crate::compose::SwitchableModel::new(Arc::clone(
            &state.shared.model,
        )));
    }
    // Every setup/execute return flows through one finalization boundary.
    async {
        if state.cancelled()
            || state
                .runtime()
                .is_some_and(|runtime| runtime.spawn_cancelled(state.task.run_id))
        {
            state.finish_cancelled();
            return;
        }
        if state.interrupted() == Some(InterruptKind::Stop) {
            state.finish_stopped();
            return;
        }
        if benchmark_mode
            && (state.task.config.initial_thread_goal.is_some()
                || state
                    .runtime()
                    .is_some_and(|runtime| runtime.goal_owner_for_run(state.task.run_id).is_some()))
        {
            state.finish_error(
                "benchmark unsupported: thread-goal-bound runs require live goal state".into(),
            );
            return;
        }
        // Runs that get their own sandbox share their thread's scratch (ADR 0033).
        if sandbox_root.is_some() && !benchmark_mode {
            match state.prepare_scratch(is_restored) {
                Ok(scratch) => state.scratch = scratch,
                Err(error) => {
                    state.finish_error(format!("scratch setup failed: {error}"));
                    return;
                }
            }
        }
        if let Some(root) = sandbox_root.clone() {
            let executor = if benchmark_mode {
                crate::runtime::benchmark_executor(Arc::clone(&state.shared.bus), root)
            } else {
                state.allowed_directories =
                    state
                        .shared
                        .runtime
                        .upgrade()
                        .map(|runtime| sandbox::AllowedRegions {
                            paths: runtime.project_allowed_directories(&root),
                            access: state.policy.allowed_access(),
                        });
                state.thread_read_dirs = state.thread_read_dirs_to_mount(&root);
                // A Worker it delegates gets the same base, sharing the thread's scratch.
                state.worker_base = crate::network::sandbox_boundary(&crate::WorkspaceMounts {
                    scratch: state.scratch_dir().map(std::path::Path::to_path_buf),
                    ..crate::WorkspaceMounts::shared(root.clone())
                })
                .ok();
                let checker_config = state
                    .shared
                    .runtime
                    .upgrade()
                    .and_then(|shared| shared.comment_checker.get().cloned())
                    .unwrap_or_default();
                let thread_ro_identities: Vec<_> = state
                    .thread_read_dirs
                    .iter()
                    .filter_map(|path| {
                        sandbox::ThreadReadDirectory::capture(path)
                            .ok()
                            .filter(|dir| dir.path() == path)
                    })
                    .collect();
                // Failed captures never fall back to ordinary (un-pinned) binds.
                state.thread_read_dirs = thread_ro_identities
                    .iter()
                    .map(|dir| dir.path().to_path_buf())
                    .collect();
                // Production's factory has the same fail-closed base config as build_sandbox.
                let mounts = crate::WorkspaceMounts {
                    workspace_root: root.clone(),
                    ro_binds: state.thread_read_dirs.clone(),
                    thread_ro_identities,
                    rw_binds: Vec::new(),
                    scratch: state.scratch_dir().map(std::path::Path::to_path_buf),
                    allowed: state.allowed_directories.clone(),
                };
                let sandbox = match state
                    .shared
                    .runtime
                    .upgrade()
                    .and_then(|runtime| runtime.sandbox_factory.clone())
                {
                    Some(factory) => factory.build(&state.policy, &mounts),
                    None => crate::network::BwrapFactory.build(&state.policy, &mounts),
                };
                sandbox
                    .and_then(|sandbox| {
                        crate::network::sandbox_boundary(&mounts)
                            .map(|boundary| (sandbox, boundary))
                    })
                    .map_err(|error| crate::RuntimeError::Sandbox {
                        detail: error.to_string(),
                    })
                    .and_then(|(sandbox, boundary)| {
                        crate::runtime::configured_executor(
                            Arc::clone(&state.shared.bus),
                            sandbox,
                            &boundary,
                            &checker_config,
                            &[root],
                            state.shared.executor.approval_policy(),
                            crate::runtime::BrowserTools::Inherit(&state.shared.executor),
                        )
                        .map(Arc::new)
                    })
            };
            match executor {
                Ok(executor) => {
                    if let Some(runtime) = state.runtime() {
                        runtime.configure_shell_escalation(&executor, state.scratch_dir());
                        runtime.configure_shell_environment(&executor);
                    }
                    state.shared.executor = executor;
                }
                Err(error) => {
                    state.finish_error(error.to_string());
                    return;
                }
            }
        }
        // tool_specs は state.policy と skill 接続状態 (state.skills()) の両方から
        // 決まるため、LoopState 構築後に確定させる。
        let selected_model = state
            .benchmark
            .as_ref()
            .map(|checkpoint| checkpoint.selected_model.clone())
            .unwrap_or_else(|| {
                state
                    .shared
                    .model
                    .selected_model(state.task.role, state.task.config.category.as_deref())
            });
        state.tool_specs = visible_tool_specs(
            standard_tool_specs(&state.shared.executor),
            &state.policy,
            state.skills().is_some(),
            state.task.parent.is_some() || benchmark_replay,
            state
                .runtime()
                .is_some_and(|runtime| runtime.web_tools_enabled()),
        );
        let artifact_pages = state
            .runtime()
            .is_some_and(|runtime| runtime.artifact_pages().is_some());
        retain_backed_tools(&mut state.tool_specs, artifact_pages);
        if !is_restored {
            state.add_team_tools();
        }
        // Family-scoped description variation keeps the request prefix stable within a model family,
        // so prompt-cache hit rates are unaffected.
        append_subagent_context_note(&mut state.tool_specs, classify(&selected_model));
        if benchmark_mode {
            state.filter_benchmark_tools();
        }
        if let Some(checkpoint) = &state.benchmark {
            if checkpoint.tools != state.tool_specs {
                state.finish_error(
                    "benchmark unsupported: available tool schemas/order differ from checkpoint"
                        .into(),
                );
                return;
            }
            match Arc::clone(&state.shared.model).freeze_for_benchmark(checkpoint.model.clone()) {
                Ok(model) => {
                    state.shared.model = model;
                    state.shared.compaction.enabled = false;
                }
                Err(error) => {
                    state.finish_error(error.to_string());
                    return;
                }
            }
        }
        let active_root =
            shared_active_root(state.shared.rules.as_deref(), sandbox_root).or_else(|| {
                benchmark_mode
                    .then(|| state.shared.executor.default_cwd())
                    .flatten()
            });
        if benchmark_mode
            && let Some(root) = &active_root
            && let Err(error) = state.shared.executor.set_workspace_boundary(root.clone())
        {
            state.finish_error(error.to_string());
            return;
        }
        if let Some(runtime_shared) = shared.upgrade() {
            runtime_shared
                .workspaces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    state.task.run_id,
                    WorkspaceInspection {
                        active_root: active_root.clone(),
                    },
                );
        }
        if let Some(source) = state.shared.rules.as_ref() {
            state.rules_session = Some(RulesSession::new(Arc::clone(source), active_root.clone()));
        }
        // Runs without a sandbox are told the same list (ADR 0032); only mounts need one.
        if state.allowed_directories.is_none()
            && !benchmark_mode
            && let Some(root) = &active_root
            && let Some(runtime) = shared.upgrade()
        {
            state.allowed_directories = Some(crate::allowed_directories::regions(
                &runtime,
                root,
                &state.policy,
            ));
        }
        let skill_roots = std::mem::take(&mut state.allowed_instructions.skill_roots);
        let inherited = state.task.config.allowed_skill_roots.clone();
        state.restore_allowed_skills(skill_roots.into_iter().chain(inherited));
        if !is_restored && !benchmark_replay {
            let allowed = state.directory_list();
            if let Err(error) = push_initial_system_message(
                &state.shared,
                &state.task,
                state.rules_session.as_ref(),
                &allowed,
                &mut state.context,
            ) {
                // fail-closed: System プロンプトの解決に失敗した run はモデル呼び出し前に
                // Error へ遷移する。reason はカタログ / skill の型付きエラー Display であり、
                // 識別子 (ロール名・キー名・カテゴリ名・skill 名) のみを運ぶ。
                state.finish_error(error.to_string());
                return;
            }
            state.told_directories = crate::allowed_directories::ToldDirectories::started(allowed);
            if benchmark_mode {
                state.append_benchmark_instructions();
            }
            state.context.push_user(&state.task.prompt);
            if let Some(message) = state.context.messages.last_mut() {
                message
                    .content
                    .extend(
                        state
                            .task
                            .config
                            .images
                            .iter()
                            .map(|image| ContentBlock::Image {
                                media_type: image.media_type.clone(),
                                data: image.data.clone(),
                            }),
                    );
            }
        }
        if state.task.handoff.is_some() && !is_restored {
            match state
                .continuity
                .blocks(state.continuity.inherited_note.as_ref())
            {
                Ok(content) if !content.is_empty() => {
                    state.context.messages.push(providers::Message {
                        role: providers::Role::User,
                        content,
                    })
                }
                Ok(_) => {}
                Err(error) => {
                    state.finish_error(error.to_string());
                    return;
                }
            }
        }
        if let Some(root) = &active_root
            && !benchmark_replay
            && !history_only
        {
            update_workspace_system_message(&mut state.context, root);
        }
        if let Some(scratch) = state.scratch.as_mut() {
            scratch.attach(&mut state.context, is_restored);
        }
        if let Some(runtime) = shared.upgrade()
            && benchmark_mode
        {
            runtime
                .benchmark_executors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(state.task.run_id, Arc::clone(&state.shared.executor));
            // Startup rules are already in the frozen context. Dynamic host-side
            // discovery could follow a trial-created symlink outside the sandbox.
            state.rules_session = None;
        }
        state.publish_message_count();
        if state.transition(AgentRunPhase::Running, None).is_err() {
            return;
        }
        if let Err(error) = crate::restore::persist_checkpoint(&state) {
            state.finish_error(format!("Cannot persist prepared context: {error}"));
            return;
        }
        if completed_turn_end.is_some() && !state.wait_for_input().await {
            return;
        }
        state.execute().await;
    }
    .await;
    state.finalize().await;
}

/// workspace 情報は既存 System 内で更新し、System がなければ履歴末尾へ追加する。
/// 先頭へ挿入すると既存 prefix と圧縮チェックポイントの位置が変わるため避ける。
fn update_workspace_system_message(context: &mut AgentContext, root: &std::path::Path) {
    let workspace_note = base_context::workspace_note(root);
    if let Some(message) = context
        .messages
        .iter_mut()
        .find(|message| message.role == providers::Role::System)
    {
        if let Some(ContentBlock::Text { text }) = message.content.iter_mut().find(|block| {
            matches!(block, ContentBlock::Text { text } if base_context::is_workspace_note(text))
        }) {
            *text = workspace_note;
        } else {
            message.content.push(ContentBlock::Text {
                text: workspace_note,
            });
        }
    } else {
        context.push_system(&workspace_note);
    }
}

/// Workspace advertised to a shared run. The sandbox root wins because it is
/// the only project the shell sandbox mounts; advertising another root makes
/// the model pass a `cwd` that does not exist inside the sandbox.
pub(crate) fn shared_active_root(
    rules: Option<&RulesSource>,
    sandbox_root: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    sandbox_root.or_else(|| {
        rules.and_then(|source| source.project_root().map(std::path::Path::to_path_buf))
    })
}

/// run 開始時の初期 System メッセージを履歴へ push する (単一 System 不変条件)。
///
/// カタログテキスト・skills セクション・project rules を**1 件の** System
/// メッセージへ合成する ([`crate::base_context::compose_system_sections`] が
/// Context Inspector と共有する単一の組立経路)。すべて無指定なら何もせず
/// v0.1 の履歴構成を保つ。解決に失敗した場合は型付きエラーを返し、履歴へは
/// System メッセージを追加しない (fail-closed)。
fn push_initial_system_message(
    shared: &LoopShared,
    task: &RunTask,
    rules_session: Option<&RulesSession>,
    allowed_directories: &[crate::allowed_directories::ToldDirectory],
    context: &mut AgentContext,
) -> Result<(), InitialSystemPromptError> {
    let sections = base_context::compose_system_sections(
        shared,
        &base_context::SystemInputs {
            role: task.role,
            category: task.config.category.as_deref(),
            load_skills: &task.config.load_skills,
            prompt_len: task.prompt.len(),
            active_root: rules_session.and_then(|session| session.active_root.as_deref()),
            allowed_directories,
        },
    )?;
    let composed = base_context::join_system_sections(&sections);
    if composed.is_empty() {
        return Ok(());
    }
    context.push_system(&composed);
    Ok(())
}

impl LoopState {
    pub(crate) fn directory_list(&mut self) -> Vec<crate::allowed_directories::ToldDirectory> {
        let delegated_thread_dirs = if crate::allowed_directories::can_delegate(&self.policy) {
            self.thread_read_dirs_for_delegates()
        } else {
            Vec::new()
        };
        crate::allowed_directories::told_list(
            self.allowed_directories.as_ref(),
            &self.thread_read_dirs,
            &self.policy,
            &delegated_thread_dirs,
            self.worker_base.as_ref(),
        )
    }

    pub(crate) fn runtime(&self) -> Option<crate::AgentRuntime> {
        crate::AgentRuntime::from_weak(&self.shared.runtime)
    }

    /// メタ操作の呼び出し元 (このループの run) の RunId を返す。
    pub(crate) fn caller_run_id(&self) -> RunId {
        self.task.run_id
    }

    pub(crate) fn run_config(&self) -> &RunConfig {
        &self.task.config
    }

    pub(crate) fn phase(&self) -> AgentRunPhase {
        self.run_state.phase()
    }

    pub(crate) fn run_role(&self) -> Role {
        self.task.role
    }

    /// この run から参照できる skill レジストリを返す (未設定なら None)。
    pub(crate) fn skills(&self) -> Option<&Arc<SkillRegistry>> {
        self.shared.skills.as_ref()
    }

    /// 委譲の記録として Delegated イベントを発行する。
    pub(crate) fn emit_delegated(&self, session_id: &str, target: &str) {
        self.shared.bus.emit(Event::new(LifecycleEvent::Delegated {
            session_id: session_id.to_string(),
            target: target.to_string(),
        }));
    }

    async fn execute(&mut self) {
        loop {
            if let Some(kind) = self.interrupted() {
                self.finish_interrupted(kind);
                return;
            }
            // A model switch comes back here inside the turn it already began, and
            // the owner rejects beginning a run's turn again before its checkpoint.
            if let Some(permit) = self.task.config.ownership.clone() {
                let result = if self.owner_turn_open {
                    // Still fence a stale generation, without opening a second turn or
                    // rejecting a quiescing owner's already-active turn.
                    self.validate_owner_mutation().await
                } else {
                    self.await_ownership(
                        permit
                            .begin_turn_off_thread(self.ownership_retry(), self.interrupt_probe()),
                    )
                    .await
                };
                if let Err(error) = result {
                    self.finish_owner_error("begin_turn", error);
                    return;
                }
                self.owner_turn_open = true;
            }
            // Ordinary follow-ups wait for Stop unless the host explicitly
            // requests delivery at this safe (post-tools) turn boundary.
            if self.has_pending_user_messages()
                || self.channels.user_inbox.status().next_turn_requested
            {
                self.flush_user_messages();
            }
            if let Err(error) = self.flush_user_answers() {
                self.finish_error(error);
                return;
            }
            self.inject_parent_messages();
            for notice in self
                .shared
                .executor
                .take_shell_job_notifications(&self.task.run_id.to_string())
            {
                // Append without rewriting the already-sent provider prefix.
                self.context.push_user(&notice);
                self.publish_message_count();
            }
            match self.publish_budget() {
                crate::budget_tracker::BudgetDecision::Continue => {}
                crate::budget_tracker::BudgetDecision::Exhausted(_) => return,
            }
            self.compaction.turn_counter = self.compaction.turn_counter.saturating_add(1);
            self.compaction.compacted_this_boundary = false;
            let requested_gen = *self.channels.compact_rx.borrow();
            if !self.todo_context_deferred && requested_gen > self.compaction.last_handled_gen {
                if self.benchmark.is_some() {
                    self.finish_error(
                        "benchmark unsupported: selected leaf requested manual compaction".into(),
                    );
                    return;
                }
                self.compaction.last_handled_gen = requested_gen;
                if let Err(error) = compaction::compact_now(self, CompactionReason::Manual).await {
                    tracing::warn!(%error, "manual compaction failed");
                }
            } else if !self.todo_context_deferred {
                let visible = self.context.visible_messages();
                let estimated = self.estimated_context_tokens(&visible);
                let (window, _) = self.resolved_context_window();
                // 閾値未満の境界を観測したら自動トリガを再武装する (ラチェット解除)。
                if (estimated as f64) < window as f64 * self.shared.compaction.threshold {
                    self.compaction.auto_suspended = false;
                }
                if compaction::policy::should_trigger(
                    &self.compaction,
                    &self.shared.compaction,
                    estimated,
                    window,
                ) == TriggerDecision::Trigger
                {
                    match compaction::compact_now(self, CompactionReason::Automatic).await {
                        Ok(outcome) if outcome.still_above_threshold => tracing::warn!(
                            estimated_tokens_before = outcome.estimated_tokens_before,
                            estimated_tokens_after = outcome.estimated_tokens_after,
                            context_window_tokens = window,
                            "automatic compaction remains above threshold"
                        ),
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(%error, "automatic compaction skipped or failed");
                        }
                    }
                }
            }
            let invocation = AgentInvocationContext {
                turn: Some(self.turn.clone()),
                category: self.task.config.category.clone(),
                run_id: self.task.run_id.to_string(),
                model_preference: self.channels.model_preference_rx.borrow().clone(),
                purpose: event_bus::RequestPurpose::Agent,
            };
            match self.publish_budget() {
                crate::budget_tracker::BudgetDecision::Continue => {}
                crate::budget_tracker::BudgetDecision::Exhausted(_) => return,
            }
            let (window, _) = self.resolved_context_window();
            self.append_boundary_context();
            let mut visible_messages = self.context.visible_messages();
            let mut estimated = self.estimated_context_tokens(&visible_messages);
            // A cooldown or the post-compaction latch must not terminate a run
            // when one more compaction can still make the next request fit.
            while estimated >= window && !self.todo_context_deferred {
                if self.benchmark.is_some() {
                    self.finish_error(
                        "benchmark unsupported: selected leaf requires context compaction".into(),
                    );
                    return;
                }
                match compaction::compact_now(self, CompactionReason::Automatic).await {
                    Ok(_) => {
                        self.append_boundary_context();
                        visible_messages = self.context.visible_messages();
                        let after = self.estimated_context_tokens(&visible_messages);
                        if after >= estimated {
                            estimated = after;
                            break;
                        }
                        estimated = after;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "context limit compaction could not proceed");
                        break;
                    }
                }
            }
            if estimated >= window {
                self.finish_error(format!("ContextWindowExhausted: compaction could not reduce the projected context ({estimated} tokens) below model window {window}; conversation checkpoint retained. Reduce context or change the compaction/model settings before resuming."));
                return;
            }
            if self.boundary_pending {
                self.boundary_pending = false;
                if let Err(error) = crate::restore::persist_checkpoint(self) {
                    self.boundary_pending = true;
                    self.finish_error(format!("Cannot persist prepared provider input: {error}"));
                    return;
                }
            }
            self.publish_context(&visible_messages, estimated, window);
            if let Some(runtime) = self.runtime()
                && !runtime.goal_model_request(
                    self.task.run_id,
                    self.task.config.purpose,
                    self.task.config.budget.max_tokens,
                )
            {
                self.finish_error("Goal cumulative budget exhausted".into());
                return;
            }
            if let Err(error) = self.capture_benchmark(&invocation, &visible_messages).await {
                self.finish_error(error.to_string());
                return;
            }
            // Benchmark capture can replace the model with its frozen settings.
            // Retain that effective model for both chat and its hosted tools.
            let invocation_model = self
                .shared
                .model
                .invocation_snapshot()
                .unwrap_or_else(|| Arc::clone(&self.shared.model));
            let completion = tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => {
                    if changed.is_ok() && let Some(kind) = self.interrupted() {
                        self.finish_interrupted(kind);
                        return;
                    }
                    continue;
                }
                result = invocation_model.complete_streaming(
                    &invocation,
                    self.task.role,
                    &visible_messages,
                    &self.tool_specs,
                    &self.shared.bus,
                ) => result,
            };
            self.activity(event_bus::RunActivity::Idle);
            let response = match completion {
                Ok(response) => {
                    self.model_switches = 0;
                    response
                }
                Err(crate::RuntimeError::ModelSwitch {
                    selected,
                    resummarize,
                }) if self.model_switches < MAX_MODEL_SWITCHES => {
                    // The router already moved the pin; prepare the history and send again.
                    self.model_switches += 1;
                    if let Err(error) =
                        compaction::prepare_model_switch(self, &selected, resummarize).await
                    {
                        self.finish_error(error.to_string());
                        return;
                    }
                    invocation_model.model_switch_prepared(&invocation, self.task.role, &selected);
                    continue;
                }
                Err(error) => {
                    let reason = error.to_string();
                    tracing::error!(run_id = ?self.task.run_id, reason = %reason, "agent run failed");
                    let _ = self.transition(AgentRunPhase::Error, Some(reason));
                    return;
                }
            };
            self.todo_context_deferred = false;
            if let Some(session) = &mut self.rules_session {
                session.set_last_usage(response.usage);
            }
            self.last_usage = Some(response.usage);
            self.budget.usage(response.usage);
            if let Some(runtime) = self.runtime() {
                runtime.goal_usage(self.task.run_id, self.task.config.purpose, response.usage);
            }
            match self.publish_budget() {
                crate::budget_tracker::BudgetDecision::Continue => {}
                crate::budget_tracker::BudgetDecision::Exhausted(_) => return,
            }
            let finish_reason = response.finish_reason;
            let tool_uses: Vec<(String, String, serde_json::Value)> = response
                .message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    ContentBlock::Compaction { .. } => {
                        tracing::warn!("この処理では compaction block をスキップします");
                        None
                    }
                    ContentBlock::Image { .. }
                    | ContentBlock::Text { .. }
                    | ContentBlock::Reasoning { .. }
                    | ContentBlock::Anthropic { .. }
                    | ContentBlock::ToolResult { .. } => None,
                })
                .collect();
            let has_tool_uses = !tool_uses.is_empty();
            if !self.guard_identical_calls(&tool_uses) {
                return;
            }
            self.observe_retracted_delegates(&tool_uses);
            if let Err(error) = self.validate_owner_mutation().await {
                self.finish_owner_error("validate", error);
                return;
            }
            // Keep a complete observational copy: display/storage subscribers can
            // lose individual deltas, while the provider response remains intact.
            let text = response
                .message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    ContentBlock::Compaction { .. } => {
                        tracing::warn!("テキスト抽出では compaction block をスキップします");
                        None
                    }
                    _ => None,
                })
                .collect::<String>();
            self.context.push_assistant(response.message);
            self.completed_turn_end = (!has_tool_uses && finish_reason == FinishReason::Stop)
                .then_some(self.context.messages.len());
            if self.completed_turn_end.is_some()
                && let Some(runtime) = self.runtime()
                && let Err(error) = runtime.capture_pending_reference_at(
                    self.task.run_id,
                    text.clone(),
                    self.context.messages.len(),
                )
            {
                self.completed_turn_end = None;
                self.finish_error(format!("Cannot persist assistant explanation: {error}"));
                return;
            }
            self.shared
                .bus
                .emit(Event::new(MessageEvent::MessageCompleted {
                    run_id: self.task.run_id.to_string(),
                    text,
                }));
            self.compaction.last_usage_estimated_tokens = Some(
                compaction::estimator::estimate_tokens(&self.context.visible_messages()),
            );
            self.publish_message_count();
            if has_tool_uses && let Err(error) = crate::restore::persist_tool_intent(self) {
                self.finish_error(format!(
                    "Cannot record tool intent before execution: {error}"
                ));
                return;
            }
            self.activity(event_bus::RunActivity::Tools);
            if !self
                .execute_tools(tool_uses, &invocation, invocation_model.as_ref())
                .await
            {
                return;
            }
            if !self.checkpoint_preflight().await {
                return;
            }
            let snapshot = match self.save_checkpoint() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.finish_error(format!("Cannot persist required checkpoint state: {error}"));
                    return;
                }
            };
            if let Some(permit) = self.task.config.ownership.clone()
                && let Err(error) = self
                    .await_ownership(permit.checkpoint_off_thread(
                        crate::ownership::CheckpointBoundary::TurnEnd,
                        snapshot,
                        self.ownership_retry(),
                        self.interrupt_probe(),
                    ))
                    .await
            {
                // An interrupted checkpoint committed nothing; finalization
                // checkpoints the run again before publishing its terminal state.
                self.finish_owner_error("checkpoint", error);
                return;
            }
            self.owner_turn_open = false;
            if has_tool_uses {
                self.budget.finish_round();
                match self.publish_budget() {
                    crate::budget_tracker::BudgetDecision::Continue => {}
                    crate::budget_tracker::BudgetDecision::Exhausted(_) => return,
                }
                continue;
            }

            match finish_reason {
                FinishReason::ToolUse => continue,
                FinishReason::Stop => {
                    if let Some(kind) = self.interrupted() {
                        self.finish_interrupted(kind);
                        return;
                    }
                    match self.flush_user_answers() {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(error) => {
                            self.finish_error(error);
                            return;
                        }
                    }
                    if self
                        .shared
                        .executor
                        .has_unobserved_shell_jobs(&self.task.run_id.to_string())
                    {
                        self.context.push_user("Shell jobs are still running. Poll their results or stop them before finishing.");
                        continue;
                    }
                    if self.flush_aside() {
                        continue;
                    }
                    match self.user_question_readiness() {
                        Ok(questions::UserQuestionReadiness::Ready) => {}
                        Ok(questions::UserQuestionReadiness::AnswersAvailable) => continue,
                        Ok(questions::UserQuestionReadiness::Waiting) => {
                            if !self.wait_for_input().await {
                                return;
                            }
                            continue;
                        }
                        Err(error) => {
                            self.finish_error(error);
                            return;
                        }
                    }
                    if let Some(runtime) = self.runtime() {
                        match runtime.pending_direct_child_questions(self.task.run_id) {
                            Ok(children) if !children.is_empty() => {
                                self.context.push_user(&format!(
                                    "Direct children need answers: {}. Use subagent_questions, then answer_subagent_question or ask_user before finishing.",
                                    children.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
                                ));
                                self.publish_message_count();
                                continue;
                            }
                            Ok(_) => {}
                            Err(error) => {
                                self.finish_error(error);
                                return;
                            }
                        }
                    }
                    if self.thread_goal_boundary().await {
                        continue;
                    }
                    if let Some(kind) = self.interrupted() {
                        self.finish_interrupted(kind);
                        return;
                    }
                    if self.flush_aside() {
                        continue;
                    }
                    if !self.task.config.interactive
                        || (self.resumed && !self.task.config.keep_alive)
                    {
                        if self.finish_success() {
                            return;
                        }
                        continue;
                    }
                    if !self.wait_for_input().await {
                        return;
                    }
                }
                FinishReason::Length => {
                    self.finish_error("model response reached length limit".to_string());
                    return;
                }
                FinishReason::ContentFilter => {
                    self.finish_error("model response was blocked by content filter".to_string());
                    return;
                }
                FinishReason::Other(reason) => {
                    self.finish_error(format!("model stopped: {reason}"));
                    return;
                }
            }
        }
    }

    async fn wait_for_input(&mut self) -> bool {
        // Resume can arrive after the final goal boundary but before Waiting.
        // Keep the scheduling bit when the post-boundary inbox drain sees it.
        if self.goal_wake_pending {
            self.goal_wake_pending = false;
            if self.user_question_completion_check().is_ok() && self.thread_goal_boundary().await {
                return true;
            }
        }
        self.activity(event_bus::RunActivity::User);
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let mut answers = runtime.shared.question_version.subscribe();
        match self.flush_user_answers() {
            Ok(true) => return true,
            Ok(false) => {}
            Err(error) => {
                self.finish_error(error);
                return false;
            }
        }
        let turn_end = self.persist_turn_boundary();
        if self.transition(AgentRunPhase::Waiting, None).is_err() {
            return false;
        }
        if let Some(context_len) = turn_end {
            self.shared
                .bus
                .emit(Event::new(event_bus::LifecycleEvent::TurnCompleted {
                    run_id: self.task.run_id.to_string(),
                    context_len,
                }));
        }
        loop {
            tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => {
                    if changed.is_ok() && let Some(kind) = self.interrupted() {
                        self.finish_interrupted(kind);
                    }
                    return false;
                }
                changed = answers.changed() => {
                    if changed.is_err() { return false; }
                    match self.flush_user_answers() {
                        Ok(true) => return self.transition(AgentRunPhase::Running, None).is_ok(),
                        Ok(false) => {},
                        Err(error) => { self.finish_error(error); return false; }
                    }
                }
                message = self.channels.inbox_rx.recv() => {
                    let Some(message) = message else {
                        self.finish_error("interactive inbox closed".to_string());
                        return false;
                    };
                    if !message.2 && message.0 == crate::thread_goals::CHECKS_WAKE {
                        if self.user_question_completion_check().is_ok() && self.thread_goal_boundary().await {
                            return self.transition(AgentRunPhase::Running, None).is_ok();
                        }
                        continue;
                    }
                    self.accept_input_identity(message.2, message.3);
                        self.context.push_user(&message.0);
                        if let Some(user) = self.context.messages.last_mut() {
                            user.content.extend(message.1.into_iter().map(|image| ContentBlock::Image {
                                media_type: image.media_type,
                                data: image.data,
                            }));
                        }
                    if message.2 { self.channels.user_inbox.consumed(1); }
                    self.publish_message_count();
                    self.resumed = true;
                    return self.transition(AgentRunPhase::Running, None).is_ok();
                }
                changed = self.channels.mailbox_version_rx.changed() => {
                    if changed.is_err() {
                        continue;
                    }
                    if self.task.mailbox.is_empty() {
                        continue;
                    }
                    let messages = self.task.mailbox.drain_where(|_| true);
                    if messages.is_empty() {
                        continue;
                    }
                    self.inject_messages(messages);
                    self.resumed = true;
                    return self.transition(AgentRunPhase::Running, None).is_ok();
                }
            }
        }
    }

    pub(crate) fn transition(
        &mut self,
        phase: AgentRunPhase,
        reason: Option<String>,
    ) -> Result<(), ()> {
        let previous = self.run_state;
        let event = self
            .run_state
            .transition(self.task.run_id, phase, reason.clone())
            .map_err(|_| ())?;
        // Waiting 位相の時間は max_elapsed に計上しない (ユーザ回答待ちなど)。
        // 遷移失敗時は位相が変わっていないため時計も触らない。
        if phase == AgentRunPhase::Waiting {
            self.budget.pause();
        } else if previous.phase() == AgentRunPhase::Waiting {
            self.budget.resume();
        }
        if matches!(
            phase,
            AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
        ) {
            self.activity(event_bus::RunActivity::Idle);
            self.shared
                .executor
                .cancel_shell_jobs(&self.task.run_id.to_string());
            self.shared.executor.end_run(&self.task.run_id.to_string());
            if let Some(runtime) = self.runtime() {
                runtime.close_artifact_page(&self.task.run_id.to_string());
            }
            self.channels.inbox_rx.close();
            self.task.mailbox.close();
            // Public terminal state permits same-ID restoration. Publish only after
            // process teardown, final persistence, and workspace cleanup finish.
            self.pending_terminal = Some((previous, event));
        } else {
            self.publish_durable_task(phase, reason);
            self.channels.phase_tx.send_replace(phase);
            self.shared.bus.emit(Event::new(event));
        }
        Ok(())
    }

    /// Persist the finished turn before it becomes a fork boundary. Only an
    /// assistant reply without tool calls ends a turn; a failed write publishes none.
    fn persist_turn_boundary(&self) -> Option<u64> {
        let last = self.context.messages.last()?;
        if last.role != providers::Role::Assistant
            || last
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        {
            return None;
        }
        if let Err(error) = crate::restore::persist_checkpoint(self) {
            tracing::warn!(run_id = %self.task.run_id, %error, "turn checkpoint failed");
            self.snapshot_diagnostic(&error);
            return None;
        }
        u64::try_from(crate::restore::conversation_len(&self.context.messages)).ok()
    }

    fn save_checkpoint(
        &self,
    ) -> Result<crate::ownership::SnapshotReference, crate::restore::SnapshotError> {
        let snapshot = self.save_checkpoint_reference()?;
        // Unknown shell effects must be durable BEFORE TurnEnd clears active
        // authority, including absent stores and skipped snapshots. Leave
        // owner_turn_open set so finalization can retry without reopening a turn.
        if self.task.config.ownership.is_some()
            && self
                .shared
                .executor
                .has_unobserved_shell_jobs(&self.task.run_id.to_string())
            && !matches!(
                snapshot,
                crate::ownership::SnapshotReference::RunContext { .. }
            )
        {
            return Err(crate::restore::SnapshotError::UnrecordedShellEffects);
        }
        Ok(snapshot)
    }

    fn save_checkpoint_reference(
        &self,
    ) -> Result<crate::ownership::SnapshotReference, crate::restore::SnapshotError> {
        match crate::restore::persist_checkpoint(self) {
            Ok(snapshot) => {
                if matches!(
                    snapshot,
                    crate::ownership::SnapshotReference::RunContext { .. }
                ) {
                    self.snapshot_saved_diagnostic();
                }
                Ok(snapshot)
            }
            Err(error @ crate::restore::SnapshotError::AssistantReference(_)) => Err(error),
            Err(error) => {
                tracing::warn!(run_id = %self.task.run_id, %error, "context checkpoint failed");
                self.snapshot_diagnostic(&error);
                Ok(crate::ownership::SnapshotReference::none(
                    crate::ownership::SnapshotNoneReason::WriteFailed,
                ))
            }
        }
    }

    fn snapshot_saved_diagnostic(&self) {
        if self
            .runtime()
            .is_some_and(|r| r.shared.run_store.get().is_some())
        {
            self.shared.bus.emit(Event::new(event_bus::DiagnosticEvent {
                source: "run_context".into(),
                severity: event_bus::DiagnosticSeverity::Info,
                code: diagnostic_codes::CONTEXT_CHECKPOINT_SAVED.into(),
                detail: "Valid conversation checkpoint saved".into(),
                run_id: Some(self.task.run_id.to_string()),
                thread_id: None,
                call_id: None,
            }));
        }
    }

    fn snapshot_diagnostic(&self, error: &impl std::fmt::Display) {
        self.shared.bus.emit(Event::new(event_bus::DiagnosticEvent {
            source: "run_context".into(), severity: event_bus::DiagnosticSeverity::Warning,
            code: diagnostic_codes::CONTEXT_SNAPSHOT_FAILED.into(),
            detail: format!("復元用コンテキストを保存できませんでした。以前の有効なチェックポイントがあれば保持します: {error}"),
            run_id: Some(self.task.run_id.to_string()), thread_id: None, call_id: None,
        }));
    }

    /// Runtime state appended after a complete tool round, right before the next
    /// provider input, so the already-sent prefix is never edited.
    fn append_boundary_context(&mut self) {
        self.append_todo_context();
        self.append_scratch_context();
        self.append_allowed_directories_context();
    }

    pub(crate) fn publish_message_count(&self) {
        self.channels
            .message_count_tx
            .send_replace(self.context.messages.len());
    }

    pub(crate) fn push_final_result(&mut self, result: &str) {
        self.context.push_assistant(providers::Message {
            role: providers::Role::Assistant,
            content: vec![ContentBlock::Text {
                text: result.to_string(),
            }],
        });
        self.publish_message_count();
        self.shared
            .bus
            .emit(Event::new(MessageEvent::FinalResultPublished {
                run_id: self.task.run_id.to_string(),
                text: result.to_string(),
            }));
    }

    pub(crate) fn interrupted(&self) -> Option<InterruptKind> {
        self.channels.cancel_rx.borrow().kind()
    }

    fn cancelled(&self) -> bool {
        self.interrupted() == Some(InterruptKind::Cancel)
    }

    fn finish_interrupted(&mut self, kind: InterruptKind) {
        match kind {
            InterruptKind::Cancel => self.finish_cancelled(),
            InterruptKind::Stop => self.finish_stopped(),
        }
    }

    /// False keeps the loop alive for answers accepted before input closure.
    pub(crate) fn finish_success(&mut self) -> bool {
        let runtime = self.runtime();
        let _completion = runtime
            .as_ref()
            .and_then(|runtime| runtime.shared.run_store.get())
            .map(|store| {
                store
                    .restore_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
        match self.user_question_readiness() {
            Ok(questions::UserQuestionReadiness::Ready) => {}
            Ok(
                questions::UserQuestionReadiness::AnswersAvailable
                | questions::UserQuestionReadiness::Waiting,
            ) => return false,
            Err(error) => {
                self.finish_error(error);
                return true;
            }
        }
        // 位相遷移より先に公開する。wait() は Done 位相で return するため、
        // run_result() を wait 後に読む利用者に最終テキストが確定済みであることを
        // 保証するためである。
        if let Some(text) = self.final_assistant_text() {
            self.channels.result_tx.send_replace(Some(text));
        }
        let _ = self.transition(AgentRunPhase::Done, None);
        true
    }

    /// エスカレーションで run を終端させる。
    ///
    /// [`LoopState::finish_success`] と異なり最終テキストを `result_tx` へ公開しない。
    /// 昇格 run の成果物は自然文の final result ではなく [`EscalationMemo`] であり、
    /// 公開すべき final text を持たないためである。メモは `pending_escalation` に
    /// 退避され、引き継ぎ側 (handoff タスク) が [`LoopState::take_pending_escalation`]
    /// で回収する。
    pub(crate) fn finish_escalated(&mut self, memo: EscalationMemo) {
        self.pending_escalation = Some(memo);
        let _ = self.transition(AgentRunPhase::Done, Some("escalated".to_string()));
    }

    /// 記録済みのエスカレーションメモを取り出す (取り出し後は `None`)。
    pub(crate) fn take_pending_escalation(&mut self) -> Option<EscalationMemo> {
        self.pending_escalation.take()
    }

    /// 最終 assistant メッセージの Text ブロックを連結して返す。
    ///
    /// Text を持つ assistant メッセージが履歴になければ `None` (自然 Stop の
    /// model 応答と finish meta-op の push_final_result の両方が Text を持つため、
    /// 通常は `Some`)。Text 以外のブロック (ToolUse / Reasoning / ToolResult) は
    /// 対象外とする。
    fn final_assistant_text(&self) -> Option<String> {
        let message = self
            .context
            .messages
            .iter()
            .rev()
            .find(|message| message.role == providers::Role::Assistant)?;
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Compaction { .. } => {
                    tracing::warn!("この処理では compaction block をスキップします");
                    None
                }
                ContentBlock::Image { .. }
                | ContentBlock::Reasoning { .. }
                | ContentBlock::Anthropic { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn finish_error(&mut self, reason: String) {
        let _ = self.transition(AgentRunPhase::Error, Some(reason));
    }

    async fn checkpoint_preflight(&mut self) -> bool {
        if let Some(permit) = self.task.config.ownership.clone()
            && let Err(error) = self
                .await_ownership(
                    permit.validate_generation_off_thread(
                        self.ownership_retry(),
                        self.interrupt_probe(),
                    ),
                )
                .await
        {
            self.finish_owner_error("checkpoint preflight", error);
            return false;
        }
        true
    }

    /// Fence a stale generation (and renew a delayed lease) before the run
    /// changes anything. Contention is retried under [`Self::ownership_retry`].
    pub(super) async fn validate_owner_mutation(
        &self,
    ) -> Result<(), crate::ownership::RegistryError> {
        let Some(permit) = self.task.config.ownership.clone() else {
            return Ok(());
        };
        self.await_ownership(
            permit.validate_mutation_off_thread(self.ownership_retry(), self.interrupt_probe()),
        )
        .await
    }

    /// Await an ownership registry operation. Contention is not loss of
    /// authority, so the run waits rather than failing; once the wait is
    /// noticeable it reports the owner-registry activity (and records one
    /// diagnostic), then returns to the activity it interrupted.
    pub(super) async fn await_ownership<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T, crate::ownership::RegistryError>>,
    ) -> Result<T, crate::ownership::RegistryError> {
        let mut operation = std::pin::pin!(operation);
        tokio::select! {
            biased;
            result = &mut operation => return result,
            () = tokio::time::sleep(OWNERSHIP_WAIT_NOTICE) => {}
        }
        let resume = self.current_activity();
        self.activity(event_bus::RunActivity::Ownership);
        self.shared.bus.emit(Event::new(event_bus::DiagnosticEvent {
            source: "ownership".into(),
            severity: event_bus::DiagnosticSeverity::Warning,
            code: diagnostic_codes::OWNERSHIP_REGISTRY_WAIT.into(),
            detail: format!(
                "The owner registry stayed locked for over {} s; the run waits for it instead of failing.",
                OWNERSHIP_WAIT_NOTICE.as_secs()
            ),
            run_id: Some(self.task.run_id.to_string()),
            thread_id: self
                .task
                .config
                .ownership
                .as_ref()
                .map(|permit| permit.thread_id.clone()),
            call_id: None,
        }));
        let result = operation.await;
        self.activity(resume);
        result
    }

    /// The ownership contention budget: none in production, so registry
    /// contention never ends a run by itself. Tests may set a bounded one.
    fn ownership_retry(&self) -> crate::ownership::RetryPolicy {
        self.runtime()
            .and_then(|runtime| runtime.shared.ownership_retry.get().copied())
            .unwrap_or_else(crate::ownership::RetryPolicy::unbounded)
    }

    /// Lets blocking ownership retries observe Cancel/Stop without the loop.
    fn interrupt_probe(&self) -> impl Fn() -> bool + Send + 'static {
        let interrupts = self.channels.cancel_rx.clone();
        move || interrupts.borrow().kind().is_some()
    }

    /// End the run on an ownership registry failure. A retry stopped by an
    /// interrupt ends as that interrupt; other reasons keep the error text,
    /// which names the registry operation and phase for SQLite failures.
    fn finish_owner_error(&mut self, stage: &'static str, error: crate::ownership::RegistryError) {
        if matches!(error, crate::ownership::RegistryError::Interrupted { .. })
            && let Some(kind) = self.interrupted()
        {
            self.finish_interrupted(kind);
            return;
        }
        if let crate::ownership::RegistryError::Database {
            operation,
            phase,
            attempt,
            ..
        } = &error
        {
            tracing::warn!(
                target: crate::ownership::DB_TRACE_TARGET,
                run_id = %self.task.run_id,
                stage,
                operation = operation.as_str(),
                phase = phase.as_str(),
                attempt,
                sqlite_code = ?error.sqlite_code(),
                "ownership registry failure ends the run"
            );
        }
        self.finish_error(error.to_string());
    }

    fn finish_stopped(&mut self) {
        let _ = self.transition(AgentRunPhase::Stopped, Some("stopped".into()));
    }

    fn finish_cancelled(&mut self) {
        self.finish_error(event_bus::CANCELLED_REASON.to_owned());
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::{shared_active_root, update_workspace_system_message};
    use crate::{
        AgentContext, CompactionCheckpoint, ProjectTrust, Role, RulesSettings, RulesSource, RunId,
    };
    use providers::{ContentBlock, Message, Role as MessageRole};
    use std::path::{Path, PathBuf};

    fn text_message(role: MessageRole, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn workspace_system_message_appends_without_changing_existing_history() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_user("work");
        context.push_assistant(text_message(MessageRole::Assistant, "answer"));
        let before = context.messages.clone();

        update_workspace_system_message(&mut context, Path::new("/workspace"));

        assert_eq!(context.messages.len(), before.len() + 1);
        assert_eq!(&context.messages[..before.len()], before.as_slice());
        assert_eq!(
            context.messages.last().unwrap(),
            &text_message(
                MessageRole::System,
                "Current workspace (evorch): /workspace."
            ),
        );
        let after = context.clone();
        update_workspace_system_message(&mut context, Path::new("/workspace"));
        assert_eq!(context, after);
    }

    #[test]
    fn workspace_system_message_appends_to_empty_context() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);

        update_workspace_system_message(&mut context, Path::new("/workspace"));

        assert_eq!(
            context.messages,
            vec![text_message(
                MessageRole::System,
                "Current workspace (evorch): /workspace."
            )],
        );
    }

    #[test]
    fn workspace_system_message_appends_block_to_existing_system() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_system("stable instructions");
        context.messages[0].content.push(ContentBlock::Text {
            text: "Mention Current workspace (evorch): without replacing this block".to_string(),
        });
        context.push_user("work");
        let mut expected = context.clone();
        expected.messages[0].content.push(ContentBlock::Text {
            text: "Current workspace (evorch): /workspace.".to_string(),
        });

        update_workspace_system_message(&mut context, Path::new("/workspace"));
        assert_eq!(context, expected);
        update_workspace_system_message(&mut context, Path::new("/workspace"));
        assert_eq!(context, expected);
    }

    #[test]
    fn workspace_system_message_updates_only_matching_block_in_place() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_user("work");
        context.push_system("stable instructions");
        context.messages[1].content.extend([
            ContentBlock::Text {
                text: "Current workspace (evorch): /old.".to_string(),
            },
            ContentBlock::Text {
                text: "keep trailing instructions".to_string(),
            },
        ]);
        context.push_system("another system message");
        let mut expected = context.clone();
        expected.messages[1].content[1] = ContentBlock::Text {
            text: "Current workspace (evorch): /new.".to_string(),
        };

        update_workspace_system_message(&mut context, Path::new("/new"));

        assert_eq!(context, expected);
    }

    #[test]
    fn workspace_refresh_on_restore_keeps_the_allowed_directories_section() {
        // Given: a restored system message whose composed text ends with the allowed directories
        let allowed = crate::allowed_directories::section_text(&[
            crate::allowed_directories::ToldDirectory {
                path: "/shared".into(),
                access: crate::allowed_directories::DirectoryAccess::ReadOnly,
                delegated_access: None,
            },
        ])
        .unwrap();
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_system(&format!("stable instructions\n\n{allowed}"));
        context.messages[0].content.push(ContentBlock::Text {
            text: "Current workspace (evorch): /old.".to_string(),
        });
        context.push_system(&allowed);
        let mut expected = context.clone();
        expected.messages[0].content[1] = ContentBlock::Text {
            text: "Current workspace (evorch): /new.".to_string(),
        };

        // When: restore refreshes the workspace note
        update_workspace_system_message(&mut context, Path::new("/new"));

        // Then: only the workspace block changed
        assert_eq!(context, expected);
    }

    #[test]
    fn workspace_system_message_preserves_restored_checkpoints_and_visible_prefix() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_user("old");
        context.push_assistant(text_message(MessageRole::Assistant, "answer"));
        context.push_user("recent");
        context.apply_checkpoint(CompactionCheckpoint {
            id: "checkpoint".to_string(),
            summary: text_message(MessageRole::User, "summary"),
            range: (0, 2),
            protects_history: false,
        });
        let mut expected = context.clone();
        let mut expected_visible = context.visible_messages();
        let workspace = text_message(
            MessageRole::System,
            "Current workspace (evorch): /workspace.",
        );
        expected.messages.push(workspace.clone());
        expected_visible.push(workspace);

        update_workspace_system_message(&mut context, Path::new("/workspace"));

        assert_eq!(context, expected);
        assert_eq!(context.visible_messages(), expected_visible);
    }

    #[test]
    fn shared_active_root_prefers_mounted_sandbox_root_over_rules_root() {
        let rules_root = PathBuf::from("/rules-project");
        let sandbox_root = PathBuf::from("/sandbox-project");
        let rules = RulesSource::new(
            ProjectTrust::Approved,
            RulesSettings::from(&config::RulesConfig::default()),
            None,
            Some(rules_root.clone()),
            None,
        );
        assert_eq!(
            shared_active_root(Some(&rules), Some(sandbox_root.clone())),
            Some(sandbox_root),
        );
        assert_eq!(shared_active_root(Some(&rules), None), Some(rules_root));
    }

    #[test]
    fn shared_active_root_falls_back_to_sandbox_or_none() {
        let sandbox_root = PathBuf::from("/sandbox-project");
        let rules = RulesSource::new(
            ProjectTrust::Approved,
            RulesSettings::from(&config::RulesConfig::default()),
            None,
            None,
            None,
        );
        for source in [None, Some(&rules)] {
            assert_eq!(
                shared_active_root(source, Some(sandbox_root.clone())),
                Some(sandbox_root.clone()),
            );
            assert_eq!(shared_active_root(source, None), None);
        }
    }
}
