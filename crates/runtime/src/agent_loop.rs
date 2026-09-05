//! 単一 AgentRun のTokio実行ループ。

// allow: SIZE_OK — select 駆動の単一 AgentRun 実行ループとその状態 (LoopState) が
// 一体の状態機械であり、分割すると遷移・注入・wake の相互関係が追えなくなる。

mod messages;
mod tool_calls;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};

use agents::Role;
use event_bus::{AgentRunPhase, CompactionReason, Event, EventBus, LifecycleEvent};
use providers::{ContentBlock, FinishReason, ToolSpec, Usage};
use tokio::sync::{mpsc, watch};
use tools::ToolExecutor;

use crate::compaction;
use crate::compaction::policy::{
    CompactionLoopState, CompactionSettings, TriggerDecision, compaction_policy_text,
};
use crate::escalation::{EscalationMemo, EscalationSettings};
use crate::network::isolated_mounts;
use crate::prompt::{SystemPromptCatalog, SystemPromptCatalogError, classify};
use crate::rules::{self, RulesSession, RulesSource};
use crate::runtime::{Shared, WorkspaceContext, loop_shared};
use crate::skill::{SkillLoadError, SkillRegistry, render_skills_section};
use crate::workspace::OwnedWorktree;
use crate::{
    AgentContext, AgentInvocationContext, AgentModel, ExecutionPolicy, RunConfig, RunId,
    RunMailbox, RunState, WorkspaceInspection, WorkspaceMode,
};
use tool_calls::{standard_tool_specs, visible_tool_specs};

pub(crate) struct RunTask {
    pub(crate) run_id: RunId,
    pub(crate) role: Role,
    pub(crate) prompt: String,
    pub(crate) config: RunConfig,
    pub(crate) parent: Option<RunId>,
    pub(crate) mailbox: Arc<RunMailbox>,
}

pub(crate) struct LoopChannels {
    pub(crate) phase_tx: watch::Sender<AgentRunPhase>,
    pub(crate) message_count_tx: watch::Sender<usize>,
    pub(crate) inbox_rx: mpsc::Receiver<String>,
    pub(crate) cancel_rx: watch::Receiver<bool>,
    pub(crate) mailbox_version_rx: watch::Receiver<u64>,
    pub(crate) compact_rx: watch::Receiver<u64>,
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
    #[expect(
        dead_code,
        reason = "エスカレーション検出器は後続タスクで LoopShared からこの設定を消費する"
    )]
    pub(crate) escalation: EscalationSettings,
    pub(crate) runtime: Weak<Shared>,
}

pub(crate) struct LoopState {
    task: RunTask,
    pub(crate) shared: LoopShared,
    pub(crate) channels: LoopChannels,
    run_state: RunState,
    pub(crate) context: AgentContext,
    policy: ExecutionPolicy,
    tool_specs: Vec<ToolSpec>,
    pub(crate) rules_session: Option<RulesSession>,
    pub(crate) compaction: CompactionLoopState,
    pub(crate) last_usage: Option<Usage>,
    resumed: bool,
    pending_escalation: Option<EscalationMemo>,
}

pub(crate) async fn run_agent(shared: Weak<Shared>, task: RunTask, channels: LoopChannels) {
    let Some(loop_shared) = loop_shared(&shared) else {
        return;
    };
    let policy = ExecutionPolicy::for_role(task.role);
    let context = AgentContext::new(task.run_id, task.role);
    let mut state = LoopState {
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
        resumed: false,
        pending_escalation: None,
    };
    // tool_specs は state.policy と skill 接続状態 (state.skills()) の両方から
    // 決まるため、LoopState 構築後に確定させる。
    state.tool_specs = visible_tool_specs(
        standard_tool_specs(),
        &state.policy,
        state.skills().is_some(),
    );
    let mut owned_worktree = match state.task.config.workspace_mode {
        WorkspaceMode::Shared => None,
        WorkspaceMode::Isolated => {
            let Some(runtime_shared) = shared.upgrade() else {
                return;
            };
            let Some(workspace) = runtime_shared.workspace.as_ref() else {
                state.finish_error("workspace isolation requires workspace context".to_string());
                return;
            };
            match setup_isolated_workspace(workspace, &runtime_shared, &state).await {
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
        None => state
            .shared
            .rules
            .as_ref()
            .and_then(|source| source.project_root().map(std::path::Path::to_path_buf)),
    };
    if let Some(source) = state.shared.rules.as_ref() {
        state.rules_session = Some(RulesSession::new(Arc::clone(source), active_root));
    }
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
    state.context.push_user(&state.task.prompt);
    state.publish_message_count();
    if state.transition(AgentRunPhase::Running, None).is_err() {
        cleanup_worktree(&state.shared, state.task.run_id, owned_worktree.take()).await;
        return;
    }
    state.execute().await;
    // cancel() は cooperative で task abort しない契約への依存。JoinHandle::abort 導入は禁止。
    cleanup_worktree(&state.shared, state.task.run_id, owned_worktree.take()).await;
}

async fn setup_isolated_workspace(
    workspace: &WorkspaceContext,
    runtime_shared: &Arc<Shared>,
    state: &LoopState,
) -> Result<(OwnedWorktree, Arc<ToolExecutor>), String> {
    let run_id = state.task.run_id;
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
                worktree_path: Some(planned_path),
                merge_mode: state.task.config.merge_mode,
            },
        );
    let branch_override = state.task.config.workspace_branch.clone();
    let manager = workspace.manager.clone();
    let owned = match tokio::task::spawn_blocking(move || match branch_override {
        Some(branch) => manager.create_on_branch(run_id, &branch),
        None => manager.create(run_id),
    })
    .await
    {
        Ok(Ok(owned)) => owned,
        Ok(Err(error)) => {
            remove_workspace_inspection(runtime_shared, run_id);
            return Err(format!("workspace setup failed: {error}"));
        }
        Err(error) => {
            remove_workspace_inspection(runtime_shared, run_id);
            return Err(format!("workspace setup failed: {error}"));
        }
    };
    let manager = workspace.manager.clone();
    let git_common_dir = match tokio::task::spawn_blocking(move || manager.git_common_dir()).await {
        Ok(Ok(path)) => path,
        Ok(Err(error)) => {
            cleanup_failed_setup(runtime_shared, run_id, owned).await;
            return Err(format!("workspace setup failed: {error}"));
        }
        Err(error) => {
            cleanup_failed_setup(runtime_shared, run_id, owned).await;
            return Err(format!("workspace setup failed: {error}"));
        }
    };
    let mounts = isolated_mounts(&owned, &git_common_dir);
    let sandbox = match workspace.factory.build(&state.policy, &mounts) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            cleanup_failed_setup(runtime_shared, run_id, owned).await;
            return Err(format!("workspace sandbox setup failed: {error}"));
        }
    };
    let executor = match ToolExecutor::with_standard_tools(Arc::clone(&runtime_shared.bus), sandbox)
        .with_web_tools()
    {
        Ok(executor) => Arc::new(executor),
        Err(error) => {
            cleanup_failed_setup(runtime_shared, run_id, owned).await;
            return Err(format!("workspace web tool setup failed: {error}"));
        }
    };
    Ok((owned, executor))
}

fn remove_workspace_inspection(runtime_shared: &Arc<Shared>, run_id: RunId) {
    runtime_shared
        .workspaces
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&run_id);
}

async fn cleanup_failed_setup(runtime_shared: &Arc<Shared>, run_id: RunId, owned: OwnedWorktree) {
    remove_workspace_inspection(runtime_shared, run_id);
    let _ = tokio::task::spawn_blocking(move || owned.cleanup()).await;
}

async fn cleanup_worktree(shared: &LoopShared, run_id: RunId, owned: Option<OwnedWorktree>) {
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
    }
}

/// 初期 System メッセージの合成で失敗しうるエラー (issue #53 / AC6)。
///
/// Display は識別子のみを運び、skill 本文や frontmatter 値を漏らさない
/// ([`SystemPromptCatalogError`] / [`SkillLoadError`] と同一規約)。
#[derive(Debug, thiserror::Error)]
enum InitialSystemPromptError {
    /// カタログ参照の失敗 (既存のカタログエラーをそのまま運ぶ)。
    #[error(transparent)]
    Catalog(#[from] SystemPromptCatalogError),
    /// skill 本文の読み込み失敗 ([`SkillLoadError`] は識別子のみを運ぶ)。
    #[error(transparent)]
    Skills(#[from] SkillLoadError),
    /// load_skills が指定されたが skill レジストリが接続されていない。
    #[error("skill registry is not configured")]
    SkillsNotConfigured,
}

/// run 開始時の初期 System メッセージを履歴へ push する (単一 System 不変条件)。
///
/// カタログテキスト・skills セクション・project rules を**1 件の** System
/// メッセージへ合成する。各セクションは空行 1 つを挟んで連結する。すべて
/// 無指定なら何もせず v0.1 の履歴構成を保つ。解決に失敗した場合は型付き
/// エラーを返し、履歴へは System メッセージを追加しない (fail-closed)。
fn push_initial_system_message(
    shared: &LoopShared,
    task: &RunTask,
    rules_session: Option<&RulesSession>,
    context: &mut AgentContext,
) -> Result<(), InitialSystemPromptError> {
    let catalog_text = match shared.system_prompts.as_ref() {
        Some(catalog) => {
            let model_id = shared.model.selected_model(task.role);
            Some(catalog.system_prompt_for(
                task.role,
                task.config.category.as_deref(),
                &model_id,
            )?)
        }
        None => None,
    };
    let skills_text = resolve_skills_section(shared, &task.config.load_skills)?;
    let compaction_text = shared.compaction_configured.then(|| {
        let model_id = shared.model.selected_model(task.role);
        compaction_policy_text(&shared.compaction, classify(&model_id))
    });
    let estimated_history_bytes = u64::try_from(
        catalog_text.as_ref().map_or(0, String::len)
            + skills_text.as_ref().map_or(0, String::len)
            + compaction_text.as_ref().map_or(0, String::len)
            + task.prompt.len(),
    )
    .unwrap_or(u64::MAX);
    let rules_text = shared.rules.as_ref().and_then(|source| {
        rules::startup_snapshot(
            source,
            rules_session.and_then(|session| session.active_root.as_deref()),
            None,
            estimated_history_bytes,
        )
    });
    let mut composed = String::new();
    for section in [catalog_text, skills_text, rules_text, compaction_text]
        .into_iter()
        .flatten()
    {
        if !composed.is_empty() {
            composed.push_str("\n\n");
        }
        composed.push_str(&section);
    }
    if composed.is_empty() {
        return Ok(());
    }
    context.push_system(&composed);
    Ok(())
}

/// load_skills 指定時、レジストリから本文を読み込み skills セクション文字列を
/// 返す。load_skills が空なら None (セクションなし)。
fn resolve_skills_section(
    shared: &LoopShared,
    load_skills: &[String],
) -> Result<Option<String>, InitialSystemPromptError> {
    if load_skills.is_empty() {
        return Ok(None);
    }
    let Some(registry) = shared.skills.as_ref() else {
        return Err(InitialSystemPromptError::SkillsNotConfigured);
    };
    let mut loaded = Vec::with_capacity(load_skills.len());
    for name in load_skills {
        loaded.push((name.clone(), registry.load_body(name)?));
    }
    Ok(Some(render_skills_section(&loaded)))
}

impl LoopState {
    pub(crate) fn runtime(&self) -> Option<crate::AgentRuntime> {
        crate::AgentRuntime::from_weak(&self.shared.runtime)
    }

    /// メタ操作の呼び出し元 (このループの run) の RunId を返す。
    pub(crate) fn caller_run_id(&self) -> RunId {
        self.task.run_id
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
            if self.cancelled() {
                self.finish_cancelled();
                return;
            }
            self.inject_parent_messages();
            self.compaction.turn_counter = self.compaction.turn_counter.saturating_add(1);
            self.compaction.compacted_this_boundary = false;
            let requested_gen = *self.channels.compact_rx.borrow();
            if requested_gen > self.compaction.last_handled_gen {
                self.compaction.last_handled_gen = requested_gen;
                if let Err(error) = compaction::compact_now(self, CompactionReason::Manual).await {
                    tracing::warn!(%error, "manual compaction failed");
                }
            } else {
                let visible = self.context.visible_messages();
                let estimated =
                    compaction::estimator::estimate_visible(&visible, self.last_usage.as_ref());
                let window = compaction::policy::resolve_window(
                    &self.shared.compaction,
                    &self.shared.model.selected_model(self.task.role),
                );
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
                run_id: self.task.run_id.to_string(),
            };
            let visible_messages = self.context.visible_messages();
            let completion = tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => {
                    if changed.is_ok() && self.cancelled() {
                        self.finish_cancelled();
                        return;
                    }
                    continue;
                }
                result = self.shared.model.complete(
                    &invocation,
                    self.task.role,
                    &visible_messages,
                    &self.tool_specs,
                ) => result,
            };
            let response = match completion {
                Ok(response) => response,
                Err(error) => {
                    let reason = error.to_string();
                    let _ = self.transition(AgentRunPhase::Error, Some(reason));
                    return;
                }
            };
            if let Some(session) = &mut self.rules_session {
                session.set_last_usage(response.usage);
            }
            self.last_usage = Some(response.usage);
            let finish_reason = response.finish_reason;
            let tool_uses: Vec<(String, String, serde_json::Value)> = response
                .message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    ContentBlock::Text { .. }
                    | ContentBlock::Reasoning { .. }
                    | ContentBlock::ToolResult { .. } => None,
                })
                .collect();
            let has_tool_uses = !tool_uses.is_empty();
            self.context.push_assistant(response.message);
            self.publish_message_count();
            if !self.execute_tools(tool_uses).await {
                return;
            }
            if has_tool_uses {
                continue;
            }

            match finish_reason {
                FinishReason::ToolUse => continue,
                FinishReason::Stop => {
                    if self.cancelled() {
                        self.finish_cancelled();
                        return;
                    }
                    if self.flush_aside() {
                        continue;
                    }
                    if !self.task.config.interactive || self.resumed {
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
        if self.transition(AgentRunPhase::Waiting, None).is_err() {
            return false;
        }
        loop {
            tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => {
                    if changed.is_ok() && self.cancelled() {
                        self.finish_cancelled();
                    }
                    return false;
                }
                message = self.channels.inbox_rx.recv() => {
                    let Some(message) = message else {
                        self.finish_error("interactive inbox closed".to_string());
                        return false;
                    };
                    self.context.push_user(&message);
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
        if phase == AgentRunPhase::Done || phase == AgentRunPhase::Error {
            self.task.mailbox.close();
        }
        let event = self
            .run_state
            .transition(self.task.run_id, phase, reason)
            .map_err(|_| ())?;
        self.shared.bus.emit(Event::new(event));
        self.channels.phase_tx.send_replace(phase);
        Ok(())
    }

    fn publish_message_count(&self) {
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
    }

    fn cancelled(&self) -> bool {
        *self.channels.cancel_rx.borrow()
    }

    pub(crate) fn finish_success(&mut self) {
        // 位相遷移より先に公開する。wait() は Done 位相で return するため、
        // run_result() を wait 後に読む利用者に最終テキストが確定済みであることを
        // 保証するためである。
        if let Some(text) = self.final_assistant_text() {
            self.channels.result_tx.send_replace(Some(text));
        }
        if self.transition(AgentRunPhase::Done, None).is_ok() {
            self.shared
                .bus
                .emit(Event::new(LifecycleEvent::BackgroundTaskCompleted {
                    task_id: self.task.run_id.to_string(),
                }));
        }
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
        if self
            .transition(AgentRunPhase::Done, Some("escalated".to_string()))
            .is_ok()
        {
            self.shared
                .bus
                .emit(Event::new(LifecycleEvent::BackgroundTaskCompleted {
                    task_id: self.task.run_id.to_string(),
                }));
        }
    }

    /// 記録済みのエスカレーションメモを取り出す (取り出し後は `None`)。
    #[expect(
        dead_code,
        reason = "エスカレーション handoff (後続タスク) がこの回収口を呼び出す"
    )]
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
                ContentBlock::Reasoning { .. }
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

    fn finish_cancelled(&mut self) {
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::BackgroundTaskCancelled {
                task_id: self.task.run_id.to_string(),
            }));
        self.finish_error("cancelled".to_string());
    }
}
