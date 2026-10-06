//! 単一 AgentRun のTokio実行ループ。

// allow: SIZE_OK — select 駆動の単一 AgentRun 実行ループとその状態 (LoopState) が
// 一体の状態機械であり、分割すると遷移・注入・wake の相互関係が追えなくなる。

mod benchmark;
mod budget;
#[cfg(test)]
mod delegate_cleanup_tests;
mod durable;
mod finalization;
mod identical_calls;
mod messages;
mod observability;
mod questions;
mod snapshots;
mod team;
mod tool_calls;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};

use agents::Role;
use event_bus::{
    AgentRunPhase, CompactionReason, EscalationMemoSummary, Event, EventBus, LifecycleEvent,
    MessageEvent,
};
use providers::{ContentBlock, FinishReason, ToolSpec, Usage};
use tokio::sync::{mpsc, watch};
use tools::ToolExecutor;

use crate::base_context::{self, InitialSystemPromptError};
use crate::compaction;
use crate::compaction::policy::{CompactionLoopState, CompactionSettings, TriggerDecision};
use crate::escalation::detector::EscalationDetector;
use crate::escalation::{EscalationMemo, EscalationSettings};
use crate::network::isolated_mounts;
use crate::prompt::{SystemPromptCatalog, classify};
use crate::rules::{RulesSession, RulesSource};
use crate::runtime::{IsolatedWorkspace, Shared, loop_shared};
use crate::skill::SkillRegistry;
use crate::workspace::OwnedWorktree;
use crate::{
    AgentContext, AgentInvocationContext, AgentModel, ExecutionPolicy, InterruptKind, RunConfig,
    RunId, RunInterrupt, RunMailbox, RunState, WorkspaceInspection, WorkspaceMode,
};
pub(crate) use tool_calls::{
    append_subagent_context_note, standard_tool_specs, visible_tool_specs,
};

pub(crate) struct RunTask {
    pub(crate) run_id: RunId,
    pub(crate) role: Role,
    pub(crate) prompt: String,
    pub(crate) config: RunConfig,
    pub(crate) parent: Option<RunId>,
    pub(crate) mailbox: Arc<RunMailbox>,
    pub(crate) handoff: Option<RunHandoff>,
    pub(crate) restored: Option<crate::restore::RestoredState>,
}

/// 終端済み run から新規 root run へ排他的に移す workspace 所有権。
///
/// `OwnedWorktree` は clone せず値で移動するため、移譲元と移譲先が同時に cleanup
/// できない。Shared mode では移す worktree がないため `worktree` は `None` となる。
pub(crate) struct RunHandoff {
    pub(crate) source_run_id: RunId,
    pub(crate) worktree: Option<OwnedWorktree>,
    pub(crate) summary: EscalationMemoSummary,
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
    pub(crate) skills: Option<Arc<SkillRegistry>>,
    pub(crate) rules: Option<Arc<RulesSource>>,
    pub(crate) compaction: CompactionSettings,
    pub(crate) compaction_configured: bool,
    pub(crate) escalation: EscalationSettings,
    pub(crate) runtime: Weak<Shared>,
}

pub(crate) struct LoopState {
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
    pending_user_messages: Vec<crate::runtime::user_inbox::UserInput>,
    pub(crate) goal_wake_pending: bool,
    pending_escalation: Option<EscalationMemo>,
    escalation_detector: EscalationDetector,
    pub(crate) budget: crate::budget_tracker::BudgetCounters,
    identical_calls: identical_calls::IdenticalCalls,
    durable_task: Option<storage::entity::TaskContinuation>,
    pending_terminal: Option<(RunState, LifecycleEvent)>,
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
    let policy = runtime
        .execution_policy(task.role)
        .for_run_config(&task.config, task.parent.is_none());
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
                if let Some(runtime) = shared.upgrade()
                    && let Some(store) = runtime.run_store.get()
                {
                    match store.ledger_entries(task.run_id) {
                        Ok(entries) => {
                            if !entries.is_empty() {
                                let mut text = String::from("[run-ledger]");
                                for entry in entries {
                                    text.push_str(&format!(
                                        "\n- seq {}: {}",
                                        entry.seq, entry.body
                                    ));
                                }
                                context.push_user(&text);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(run_id = %task.run_id, %error, "restored run ledger read failed")
                        }
                    }
                }
                match restored.trigger {
                    Some(trigger) => context.push_user(&messages::format_agent_message(&trigger)),
                    None => {
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
                }
                context
            }
            None => AgentContext::new(task.run_id, task.role),
        }
    };
    let mut state = LoopState {
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
        pending_user_messages: Vec::new(),
        goal_wake_pending: false,
        pending_escalation: None,
        escalation_detector: EscalationDetector::default(),
        budget: crate::budget_tracker::BudgetCounters::default(),
        identical_calls: identical_calls::IdenticalCalls::default(),
        durable_task: None,
        pending_terminal: None,
    };
    let mut owned_worktree = None;
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
        if state.task.config.workspace_mode == WorkspaceMode::Shared
            && let Some(root) = sandbox_root.clone()
        {
            let executor = if benchmark_mode {
                crate::runtime::benchmark_executor(Arc::clone(&state.shared.bus), root)
            } else {
                let checker_config = state
                    .shared
                    .runtime
                    .upgrade()
                    .and_then(|shared| shared.comment_checker.get().cloned())
                    .unwrap_or_default();
                // Reuse the existing explicit workspace sandbox seam for shared runs too.
                // Production's factory has the same fail-closed base config as build_sandbox.
                let sandbox = match state.shared.runtime.upgrade().and_then(|runtime| {
                    runtime
                        .workspace
                        .as_ref()
                        .map(|workspace| Arc::clone(&workspace.factory))
                }) {
                    Some(factory) => factory.build(
                        &state.policy,
                        &crate::IsolatedMounts {
                            workspace_root: root.clone(),
                            ro_binds: Vec::new(),
                            rw_binds: Vec::new(),
                        },
                    ),
                    None => crate::network::build_sandbox(&state.policy, root.clone()),
                };
                sandbox
                    .map_err(|error| crate::RuntimeError::Sandbox {
                        detail: error.to_string(),
                    })
                    .and_then(|sandbox| {
                        crate::runtime::configured_executor(
                            Arc::clone(&state.shared.bus),
                            sandbox,
                            root.clone(),
                            &checker_config,
                            &[root],
                            state.shared.executor.approval_policy(),
                        )
                    })
            };
            match executor {
                Ok(executor) => {
                    if let Some(runtime) = state.runtime() {
                        runtime.configure_shell_escalation(&executor);
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
        owned_worktree = match state.task.config.workspace_mode {
            WorkspaceMode::Shared => None,
            WorkspaceMode::Isolated => {
                let Some(runtime_shared) = shared.upgrade() else {
                    return;
                };
                let Some(workspace) =
                    runtime_shared
                        .workspace
                        .as_ref()
                        .and_then(|workspace| match &project {
                            Some(project) => project.isolated(workspace),
                            None => workspace.isolated(),
                        })
                else {
                    state.finish_error(crate::RuntimeError::WorkspaceContextRequired.to_string());
                    return;
                };
                let adopted = state
                    .task
                    .handoff
                    .as_mut()
                    .and_then(|handoff| handoff.worktree.take());
                let setup = match adopted {
                    Some(owned) => {
                        attach_adopted_workspace(&workspace, &runtime_shared, &state, owned).await
                    }
                    None => setup_isolated_workspace(&workspace, &runtime_shared, &state).await,
                };
                match setup {
                    Ok((owned, executor)) => {
                        state.shared.executor = executor;
                        Some(owned)
                    }
                    Err(reason) => {
                        state.finish_error(reason);
                        return;
                    }
                }
            }
        };
        let active_root = match owned_worktree.as_ref() {
            Some(owned) => Some(owned.path.clone()),
            None => shared_active_root(state.shared.rules.as_deref(), sandbox_root).or_else(|| {
                benchmark_mode
                    .then(|| state.shared.executor.default_cwd())
                    .flatten()
            }),
        };
        if benchmark_mode
            && let Some(root) = &active_root
            && let Err(error) = state.shared.executor.set_workspace_boundary(root.clone())
        {
            state.finish_error(error.to_string());
            return;
        }
        if state.task.config.workspace_mode == WorkspaceMode::Shared
            && let Some(runtime_shared) = shared.upgrade()
        {
            runtime_shared
                .workspaces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    state.task.run_id,
                    WorkspaceInspection {
                        mode: WorkspaceMode::Shared,
                        branch: None,
                        worktree_path: None,
                        active_root: active_root.clone(),
                        merge_mode: state.task.config.merge_mode,
                    },
                );
        }
        if let Some(source) = state.shared.rules.as_ref() {
            state.rules_session = Some(RulesSession::new(Arc::clone(source), active_root.clone()));
        }
        if !is_restored && !benchmark_replay {
            if let Err(error) = push_initial_system_message(
                &state.shared,
                &state.task,
                state.rules_session.as_ref(),
                &mut state.context,
            ) {
                // fail-closed: System プロンプトの解決に失敗した run はモデル呼び出し前に
                // Error へ遷移する。reason はカタログ / skill の型付きエラー Display であり、
                // 識別子 (ロール名・キー名・カテゴリ名・skill 名) のみを運ぶ。
                state.finish_error(error.to_string());
                cleanup_worktree(&state.shared, state.task.run_id, owned_worktree.take()).await;
                return;
            }
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
        if let Some(root) = &active_root
            && !benchmark_replay
        {
            update_workspace_system_message(&mut state.context, root);
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
            cleanup_worktree(&state.shared, state.task.run_id, owned_worktree.take()).await;
            return;
        }
        state.save_checkpoint();
        state.execute().await;
    }
    .await;
    state.finalize(owned_worktree).await;
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

async fn setup_isolated_workspace(
    workspace: &IsolatedWorkspace,
    runtime_shared: &Arc<Shared>,
    state: &LoopState,
) -> Result<(OwnedWorktree, Arc<ToolExecutor>), String> {
    let owned = create_worktree(workspace, runtime_shared, state).await?;
    match attach_worktree_executor(workspace, runtime_shared, state, &owned).await {
        Ok(executor) => Ok((owned, executor)),
        Err(reason) => {
            cleanup_failed_setup(runtime_shared, state.task.run_id, owned).await;
            Err(reason)
        }
    }
}

async fn create_worktree(
    workspace: &IsolatedWorkspace,
    runtime_shared: &Arc<Shared>,
    state: &LoopState,
) -> Result<OwnedWorktree, String> {
    let run_id = state.task.run_id;
    if state.resumed {
        let manager = workspace.manager.clone();
        match tokio::task::spawn_blocking(move || manager.open_existing(run_id)).await {
            Ok(Ok(owned)) => {
                runtime_shared
                    .workspaces
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        run_id,
                        WorkspaceInspection {
                            mode: WorkspaceMode::Isolated,
                            branch: Some(owned.branch.clone()),
                            worktree_path: Some(owned.path.clone()),
                            active_root: Some(owned.path.clone()),
                            merge_mode: state.task.config.merge_mode,
                        },
                    );
                return Ok(owned);
            }
            Ok(Err(crate::workspace::WorkspaceError::PathMissing { .. })) => {}
            Ok(Err(error)) => {
                remove_workspace_inspection(runtime_shared, run_id);
                return Err(format!("workspace setup failed: {error}"));
            }
            Err(error) => {
                remove_workspace_inspection(runtime_shared, run_id);
                return Err(format!("workspace setup failed: {error}"));
            }
        }
    }
    // inspection は sandbox build より先に登録する。factory.build の完了を観測してから
    // inspect する利用者が Shared fallback を読まないようにするため (issue #71 CI 失敗の
    // root cause: 旧実装は build 完了後に登録しており、その間の inspect が Shared を返した)。
    // workspace_branch 指定時は既存 branch の checkout 用に、path 導出だけを run 名で行う
    // (issue #73 D2)。
    let (planned_branch, planned_path) = match state.task.config.workspace_branch.as_deref() {
        Some(branch) => workspace.manager.planned_on_branch(run_id, branch),
        None => workspace.manager.planned(run_id),
    };
    runtime_shared
        .workspaces
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            run_id,
            WorkspaceInspection {
                mode: WorkspaceMode::Isolated,
                branch: Some(planned_branch),
                worktree_path: Some(planned_path.clone()),
                active_root: Some(planned_path),
                merge_mode: state.task.config.merge_mode,
            },
        );
    let branch_override = state.task.config.workspace_branch.clone();
    let manager = workspace.manager.clone();
    match tokio::task::spawn_blocking(move || match branch_override {
        Some(branch) => manager.create_on_branch(run_id, &branch),
        None => manager.create(run_id),
    })
    .await
    {
        Ok(Ok(owned)) => Ok(owned),
        Ok(Err(error)) => {
            remove_workspace_inspection(runtime_shared, run_id);
            Err(format!("workspace setup failed: {error}"))
        }
        Err(error) => {
            remove_workspace_inspection(runtime_shared, run_id);
            Err(format!("workspace setup failed: {error}"))
        }
    }
}

async fn attach_adopted_workspace(
    workspace: &IsolatedWorkspace,
    runtime_shared: &Arc<Shared>,
    state: &LoopState,
    owned: OwnedWorktree,
) -> Result<(OwnedWorktree, Arc<ToolExecutor>), String> {
    let run_id = state.task.run_id;
    let _source_run_id = state
        .task
        .handoff
        .as_ref()
        .map(|handoff| handoff.source_run_id)
        .ok_or_else(|| "adopted workspace requires handoff context".to_string())?;
    {
        let mut workspaces = runtime_shared
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        workspaces.insert(
            run_id,
            WorkspaceInspection {
                mode: WorkspaceMode::Isolated,
                branch: Some(owned.branch.clone()),
                worktree_path: Some(owned.path.clone()),
                active_root: Some(owned.path.clone()),
                merge_mode: state.task.config.merge_mode,
            },
        );
    }

    match attach_worktree_executor(workspace, runtime_shared, state, &owned).await {
        Ok(executor) => Ok((owned, executor)),
        Err(reason) => {
            cleanup_failed_setup(runtime_shared, run_id, owned).await;
            Err(reason)
        }
    }
}

async fn attach_worktree_executor(
    workspace: &IsolatedWorkspace,
    runtime_shared: &Arc<Shared>,
    state: &LoopState,
    owned: &OwnedWorktree,
) -> Result<Arc<ToolExecutor>, String> {
    let manager = workspace.manager.clone();
    let git_common_dir = tokio::task::spawn_blocking(move || manager.git_common_dir())
        .await
        .map_err(|error| format!("workspace setup failed: {error}"))?
        .map_err(|error| format!("workspace setup failed: {error}"))?;
    let mounts = isolated_mounts(owned, &git_common_dir);
    let sandbox = workspace
        .factory
        .build(&state.policy, &mounts)
        .map_err(|error| format!("workspace sandbox setup failed: {error}"))?;
    let checker_config = runtime_shared
        .comment_checker
        .get()
        .cloned()
        .unwrap_or_default();
    let forbidden_roots = [
        workspace.manager.repo_root().to_path_buf(),
        owned.path.clone(),
    ];
    let executor = crate::runtime::configured_executor(
        Arc::clone(&runtime_shared.bus),
        sandbox,
        owned.path.clone(),
        &checker_config,
        &forbidden_roots,
        state.shared.executor.approval_policy(),
    )
    .map_err(|error| format!("workspace tool setup failed: {error}"))?;
    crate::AgentRuntime {
        shared: runtime_shared.clone(),
    }
    .configure_shell_escalation(&executor);
    Ok(executor)
}

fn remove_workspace_inspection(runtime_shared: &Arc<Shared>, run_id: RunId) {
    runtime_shared
        .workspaces
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&run_id);
}

async fn cleanup_failed_setup(runtime_shared: &Arc<Shared>, run_id: RunId, owned: OwnedWorktree) {
    // Setup failure is the exception: discard the inspection rather than retain/clear its roots.
    remove_workspace_inspection(runtime_shared, run_id);
    let _ = tokio::task::spawn_blocking(move || owned.cleanup()).await;
}

pub(crate) async fn cleanup_worktree(
    shared: &LoopShared,
    run_id: RunId,
    owned: Option<OwnedWorktree>,
) {
    let Some(owned) = owned else {
        return;
    };
    let cleanup = tokio::task::spawn_blocking(move || owned.cleanup()).await;
    // cleanup failure 用 lifecycle event は追加せず、inspection の path を残して回収対象を可視にする。
    if matches!(cleanup, Ok(Ok(())))
        && let Some(runtime_shared) = shared.runtime.upgrade()
        && let Some(inspection) = runtime_shared
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&run_id)
    {
        inspection.worktree_path = None;
        inspection.active_root = None;
    }
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
            if let Some(permit) = &self.task.config.ownership
                && let Err(error) = permit.begin_turn()
            {
                self.finish_error(error.to_string());
                return;
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
            if requested_gen > self.compaction.last_handled_gen {
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
            } else {
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
            let mut visible_messages = self.context.visible_messages();
            let mut estimated = self.estimated_context_tokens(&visible_messages);
            // A cooldown or the post-compaction latch must not terminate a run
            // when one more compaction can still make the next request fit.
            while estimated >= window {
                if self.benchmark.is_some() {
                    self.finish_error(
                        "benchmark unsupported: selected leaf requires context compaction".into(),
                    );
                    return;
                }
                match compaction::compact_now(self, CompactionReason::Automatic).await {
                    Ok(_) => {
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
            let completion = tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => {
                    if changed.is_ok() && let Some(kind) = self.interrupted() {
                        self.finish_interrupted(kind);
                        return;
                    }
                    continue;
                }
                result = self.shared.model.complete_streaming(
                    &invocation,
                    self.task.role,
                    &visible_messages,
                    &self.tool_specs,
                    &self.shared.bus,
                ) => result,
            };
            self.activity(event_bus::RunActivity::Idle);
            let response = match completion {
                Ok(response) => response,
                Err(error) => {
                    let reason = error.to_string();
                    tracing::error!(run_id = ?self.task.run_id, reason = %reason, "agent run failed");
                    let _ = self.transition(AgentRunPhase::Error, Some(reason));
                    return;
                }
            };
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
                    | ContentBlock::ToolResult { .. } => None,
                })
                .collect();
            let has_tool_uses = !tool_uses.is_empty();
            if !self.guard_identical_calls(&tool_uses) {
                return;
            }
            if let Some(permit) = &self.task.config.ownership
                && let Err(error) = permit.validate_mutation()
            {
                self.finish_error(error.to_string());
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
            if !self.execute_tools(tool_uses).await {
                return;
            }
            if let Some(permit) = &self.task.config.ownership
                && let Err(error) = permit.checkpoint(&self.context.visible_messages())
            {
                self.finish_error(error.to_string());
                return;
            }
            self.save_checkpoint();
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
                        self.finish_success();
                        return;
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

    fn save_checkpoint(&self) {
        if let Err(error) = crate::restore::persist_checkpoint(self) {
            tracing::warn!(run_id = %self.task.run_id, %error, "context checkpoint failed");
            self.snapshot_diagnostic(&error);
        } else {
            self.snapshot_saved_diagnostic();
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
                code: "ContextCheckpointSaved".into(),
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
            code: "ContextSnapshotFailed".into(),
            detail: format!("復元用コンテキストを保存できませんでした。以前の有効なチェックポイントがあれば保持します: {error}"),
            run_id: Some(self.task.run_id.to_string()), thread_id: None, call_id: None,
        }));
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

    pub(crate) fn finish_success(&mut self) {
        // 位相遷移より先に公開する。wait() は Done 位相で return するため、
        // run_result() を wait 後に読む利用者に最終テキストが確定済みであることを
        // 保証するためである。
        if let Some(text) = self.final_assistant_text() {
            self.channels.result_tx.send_replace(Some(text));
        }
        let _ = self.transition(AgentRunPhase::Done, None);
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

    fn finish_stopped(&mut self) {
        let _ = self.transition(AgentRunPhase::Stopped, Some("stopped".into()));
    }

    fn finish_cancelled(&mut self) {
        self.finish_error("cancelled".to_string());
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
    fn workspace_system_message_preserves_restored_checkpoints_and_visible_prefix() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_user("old");
        context.push_assistant(text_message(MessageRole::Assistant, "answer"));
        context.push_user("recent");
        context.apply_checkpoint(CompactionCheckpoint {
            id: "checkpoint".to_string(),
            summary: text_message(MessageRole::User, "summary"),
            range: (0, 2),
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
