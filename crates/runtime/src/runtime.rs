//! AgentRun の登録と公開操作を提供するランタイム表層。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use agents::Role;
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, DeliveryDisposition, Event,
    EventBus, EventKind, FaultEvent, LifecycleEvent,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tools::ToolExecutor;

use crate::agent_loop::{LoopChannels, LoopShared, RunTask, run_agent};
use crate::mailbox::{PushError, RunMailbox};
use crate::prompt::{
    CatalogBuildInput, PromptCompositionError, SystemPromptCatalog, build_catalog,
};
use crate::run::{RunConfig, WorkspaceInspection, WorkspaceMode};
use crate::skill::SkillRegistry;
use crate::workspace::{Project, WorktreeManager};
use crate::{AgentInspection, AgentModel, AgentSummary, ExecutionPolicy, RunId, RuntimeError};

const INBOX_CAPACITY: usize = 32;

/// Tokio タスクとして AgentRun を実行するランタイム。
///
/// 呼び出し側は [`AgentRuntime::new`] に渡した同一の `Arc<EventBus>` を共有することで
/// ライフサイクルを観測する。run タスクは内部状態への `Weak` のみを保持するため循環
/// 参照は作らない。`AgentRuntime` の drop は実行中タスクを abort せず、run は正常終了
/// または明示的な [`AgentRuntime::cancel`] まで継続する。
#[derive(Clone)]
pub struct AgentRuntime {
    shared: Arc<Shared>,
}

pub(crate) struct Shared {
    pub(crate) bus: Arc<EventBus>,
    pub(crate) executor: Arc<ToolExecutor>,
    pub(crate) model: Arc<dyn AgentModel>,
    pub(crate) system_prompts: OnceLock<Arc<SystemPromptCatalog>>,
    pub(crate) skills: OnceLock<Arc<SkillRegistry>>,
    pub(crate) workspace: Option<WorkspaceContext>,
    pub(crate) workspaces: Mutex<HashMap<RunId, WorkspaceInspection>>,
    next_run_id: AtomicU64,
    next_message_id: AtomicU64,
    runs: Mutex<HashMap<RunId, RunEntry>>,
    sent: Mutex<HashMap<String, SentRecord>>,
}

/// isolated sandbox を構築するための mount policy 入力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedMounts {
    /// run 専用 worktree root。
    pub workspace_root: PathBuf,
    /// 読み取り専用で公開する path。
    pub ro_binds: Vec<PathBuf>,
    /// 読み書き可能で公開する path。
    pub rw_binds: Vec<PathBuf>,
}

/// run ごとの sandbox 構築境界。
pub trait SandboxFactory: Send + Sync {
    /// policy と mount set から sandbox を構築する。
    ///
    /// # Errors
    /// sandbox の検出または構成に失敗した場合に [`sandbox::SandboxError`] を返す。
    fn build(
        &self,
        policy: &ExecutionPolicy,
        mounts: &IsolatedMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError>;
}

pub(crate) struct WorkspaceContext {
    pub(crate) manager: WorktreeManager,
    pub(crate) factory: Arc<dyn SandboxFactory>,
}

struct RunEntry {
    role: Role,
    name: String,
    model: String,
    config: RunConfig,
    parent: Option<RunId>,
    phase_tx: watch::Sender<AgentRunPhase>,
    phase_rx: watch::Receiver<AgentRunPhase>,
    message_count_rx: watch::Receiver<usize>,
    inbox_tx: mpsc::Sender<String>,
    cancel_tx: watch::Sender<bool>,
    mailbox: Arc<RunMailbox>,
    _join: Option<JoinHandle<()>>,
}

struct SentRecord {
    sender: RunId,
    recipient: RunId,
}

impl AgentRuntime {
    pub(crate) fn from_weak(shared: &Weak<Shared>) -> Option<Self> {
        shared.upgrade().map(|shared| Self { shared })
    }

    /// 共有イベントバス・ツール実行器・モデル境界からランタイムを生成する。
    pub fn new(
        bus: Arc<EventBus>,
        executor: Arc<ToolExecutor>,
        model: Arc<dyn AgentModel>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                bus,
                executor,
                model,
                system_prompts: OnceLock::new(),
                skills: OnceLock::new(),
                workspace: None,
                workspaces: Mutex::new(HashMap::new()),
                next_run_id: AtomicU64::new(1),
                next_message_id: AtomicU64::new(1),
                runs: Mutex::new(HashMap::new()),
                sent: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// システムプロンプトカタログを設定したランタイムを返すビルダーメソッド。
    ///
    /// `new` / `production` に鎖でつなげて呼ぶ。設定済みの場合は 2 回目以降の
    /// 呼び出しは無視される (先勝ち)。カタログ未設定の run は v0.1 と同じ
    /// System メッセージなしの履歴で開始する。
    pub fn with_system_prompts(self, system_prompts: Arc<SystemPromptCatalog>) -> Self {
        let _ = self.shared.system_prompts.set(system_prompts);
        self
    }

    /// config から system prompt catalog を組み立てて接続するビルダーメソッド。
    ///
    /// fail-closed: プリセット解決やカタログ完全性検証に失敗した場合、カタログは
    /// ランタイムに接続されずにそのままエラーを返す。成功時は
    /// [`AgentRuntime::with_system_prompts`] と同じ先勝ちで接続する。
    ///
    /// # Errors
    /// [`build_catalog`] の失敗をそのまま伝播する。
    pub fn with_config_prompts(
        self,
        input: &CatalogBuildInput<'_>,
    ) -> Result<Self, PromptCompositionError> {
        let catalog = build_catalog(input)?;
        Ok(self.with_system_prompts(Arc::new(catalog)))
    }

    /// skill レジストリを設定したランタイムを返すビルダーメソッド。
    ///
    /// `new` / `production` に鎖でつなげて呼ぶ。設定済みの場合は 2 回目以降の
    /// 呼び出しは無視される (先勝ち)。初回接続時に限り、レジストリが発見時に
    /// 記録した診断 1 件ごとに [`FaultEvent::SkillDiagnostic`] を 1 件バスへ
    /// 発行する (ADR 0010: 失敗は静かにしない)。レジストリ未設定の run からは
    /// `skill_load` メタ操作はモデルに見せない (tool_specs 可視性フィルタ)。
    pub fn with_skills(self, skills: Arc<SkillRegistry>) -> Self {
        if self.shared.skills.set(Arc::clone(&skills)).is_ok() {
            for diagnostic in &skills.diagnostics {
                self.shared
                    .bus
                    .emit(Event::new(FaultEvent::SkillDiagnostic {
                        kind: diagnostic.kind.clone(),
                        skill: diagnostic.skill.clone(),
                        scope: diagnostic.scope.as_str().to_owned(),
                        detail: diagnostic.detail.clone(),
                    }));
            }
        }
        self
    }

    /// production 構成のランタイムを生成する。
    ///
    /// `build_sandbox(&ExecutionPolicy, workspace)` 経由で role の network
    /// capability を bwrap policy へ伝播し、標準ツールを持つ ToolExecutor に注入する
    /// composition root (PR #22 の fail-closed 経路 / implementation.md:48)。
    /// bwrap の検出・検証に失敗した場合はエラーをそのまま伝播する。
    /// DirectSandbox へのフォールバック経路は存在しない (ADR 0021)。
    pub fn production(
        bus: Arc<EventBus>,
        policy: &ExecutionPolicy,
        workspace_root: PathBuf,
        model: Arc<dyn AgentModel>,
    ) -> Result<Self, RuntimeError> {
        let sandbox = crate::network::build_sandbox(policy, workspace_root).map_err(|error| {
            RuntimeError::Sandbox {
                detail: error.to_string(),
            }
        })?;
        let executor = Arc::new(ToolExecutor::with_standard_tools(Arc::clone(&bus), sandbox));
        Ok(Self::new(bus, executor, model))
    }

    /// production sandbox と isolated workspace context を持つランタイムを生成する。
    ///
    /// # Errors
    /// project 検証または baseline sandbox 構築に失敗した場合に [`RuntimeError`] を返す。
    pub fn production_with_project(
        bus: Arc<EventBus>,
        policy: &ExecutionPolicy,
        project_root: PathBuf,
        model: Arc<dyn AgentModel>,
    ) -> Result<Self, RuntimeError> {
        let project = Project::new(project_root).map_err(|error| RuntimeError::Workspace {
            detail: error.to_string(),
        })?;
        let sandbox = crate::network::build_sandbox(policy, project.repo_root().to_path_buf())
            .map_err(|error| RuntimeError::Sandbox {
                detail: error.to_string(),
            })?;
        let executor = Arc::new(ToolExecutor::with_standard_tools(Arc::clone(&bus), sandbox));
        Ok(Self::with_workspace_context(
            bus,
            executor,
            model,
            WorktreeManager::new(project),
            Arc::new(crate::network::BwrapFactory),
        ))
    }

    /// 明示的な isolated workspace test seam を持つランタイムを生成する。
    ///
    /// production は [`AgentRuntime::production_with_project`] を使用する。隔離なし sandbox
    /// は既存の [`AgentRuntime::new`] とこの明示的 seam のテスト実装でのみ許可する。
    pub fn with_workspace_context(
        bus: Arc<EventBus>,
        executor: Arc<ToolExecutor>,
        model: Arc<dyn AgentModel>,
        manager: WorktreeManager,
        factory: Arc<dyn SandboxFactory>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                bus,
                executor,
                model,
                system_prompts: OnceLock::new(),
                skills: OnceLock::new(),
                workspace: Some(WorkspaceContext { manager, factory }),
                workspaces: Mutex::new(HashMap::new()),
                next_run_id: AtomicU64::new(1),
                next_message_id: AtomicU64::new(1),
                runs: Mutex::new(HashMap::new()),
                sent: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// run を登録してバックグラウンド実行を開始し、その ID を返す。
    pub fn delegate_background(&self, role: Role, prompt: String, config: RunConfig) -> RunId {
        self.spawn_run(None, role, prompt, config)
    }

    /// 指定した親 run の子として run を登録してバックグラウンド実行を開始し、その ID を返す。
    ///
    /// # Errors
    /// 親 run が存在しない場合 [`RuntimeError::UnknownRun`] を返す。
    pub fn delegate_background_as_child(
        &self,
        parent: RunId,
        role: Role,
        prompt: impl Into<String>,
        config: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        {
            let runs = lock_runs(&self.shared.runs);
            if !runs.contains_key(&parent) {
                return Err(unknown_run(parent));
            }
        }
        Ok(self.spawn_run(Some(parent), role, prompt.into(), config))
    }

    fn spawn_run(
        &self,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        config: RunConfig,
    ) -> RunId {
        let run_id = RunId::new(self.shared.next_run_id.fetch_add(1, Ordering::Relaxed));
        let name = config
            .name
            .clone()
            .unwrap_or_else(|| role.name().to_string());
        let model = self.shared.model.selected_model(role);
        let (phase_tx, phase_rx) = watch::channel(AgentRunPhase::Pending);
        let (message_count_tx, message_count_rx) = watch::channel(0);
        let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let phase_tx_entry = phase_tx.clone();
        let mailbox = Arc::new(RunMailbox::new());
        let mailbox_version_rx = mailbox.subscribe_version();
        let task = RunTask {
            run_id,
            role,
            prompt,
            config: config.clone(),
            parent,
            mailbox: Arc::clone(&mailbox),
        };
        let channels = LoopChannels {
            phase_tx,
            message_count_tx,
            inbox_rx,
            cancel_rx,
            mailbox_version_rx,
        };
        lock_runs(&self.shared.runs).insert(
            run_id,
            RunEntry {
                role,
                name,
                model,
                config: config.clone(),
                parent,
                phase_tx: phase_tx_entry,
                phase_rx,
                message_count_rx,
                inbox_tx,
                cancel_tx,
                mailbox: Arc::clone(&mailbox),
                _join: None,
            },
        );
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: run_id.to_string(),
                from: AgentRunPhase::Pending,
                to: AgentRunPhase::Pending,
                reason: Some("registered".to_string()),
            }));
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::BackgroundTaskStarted {
                task_id: run_id.to_string(),
            }));
        let weak = Arc::downgrade(&self.shared);
        let join = tokio::spawn(async move { run_agent(weak, task, channels).await });
        if let Some(entry) = lock_runs(&self.shared.runs).get_mut(&run_id) {
            entry._join = Some(join);
        }
        run_id
    }

    /// 対話待機中の run へユーザーメッセージを送る。
    pub fn send_message(&self, run_id: RunId, text: String) -> Result<(), RuntimeError> {
        let sender = self.entry(run_id)?.inbox_tx.clone();
        sender
            .try_send(text)
            .map_err(|_| RuntimeError::RunTerminated {
                run_id: run_id.to_string(),
            })
    }

    /// run が終端位相になるまで待機し、最終位相を返す。
    pub async fn wait(&self, run_id: RunId) -> Result<AgentRunPhase, RuntimeError> {
        let mut phase_rx = self.entry(run_id)?.phase_rx.clone();
        loop {
            let phase = *phase_rx.borrow_and_update();
            match phase {
                AgentRunPhase::Done | AgentRunPhase::Error => return Ok(phase),
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {}
            }
            if phase_rx.changed().await.is_err() {
                return Ok(*phase_rx.borrow());
            }
        }
    }

    /// run へキャンセルを通知する。複数回の通知は同じ結果となる。
    pub fn cancel(&self, run_id: RunId) -> Result<(), RuntimeError> {
        let sender = self.entry(run_id)?.cancel_tx.clone();
        sender.send_replace(true);
        Ok(())
    }

    /// run を開始して終端まで待つ簡易 foreground API。
    ///
    /// 委譲元セッションは v0.1 では固定文字列 `runtime` として記録する。
    /// 実行設定 (表示名など) は [`RunConfig`] で委譲先 run へ渡す。
    pub async fn delegate(
        &self,
        role: Role,
        prompt: String,
        config: RunConfig,
    ) -> Result<AgentRunPhase, RuntimeError> {
        let run_id = self.delegate_background(role, prompt, config);
        self.shared.bus.emit(Event::new(LifecycleEvent::Delegated {
            session_id: "runtime".to_string(),
            target: run_id.to_string(),
        }));
        self.wait(run_id).await
    }

    /// 登録済み run の要約を ID 順で返す。
    pub fn list_agents(&self) -> Vec<AgentSummary> {
        let runs = lock_runs(&self.shared.runs);
        let mut summaries: Vec<AgentSummary> = runs
            .iter()
            .map(|(run_id, entry)| AgentSummary {
                run_id: *run_id,
                name: entry.name.clone(),
                role_name: entry.role.name().to_string(),
                phase: *entry.phase_rx.borrow(),
                model: entry.model.clone(),
            })
            .collect();
        summaries.sort_by_key(|summary| summary.run_id.get());
        summaries
    }

    /// run の位相・会話履歴件数・workspace 情報を返す。
    pub fn inspect_agent(&self, run_id: RunId) -> Result<AgentInspection, RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        let entry = runs.get(&run_id).ok_or_else(|| unknown_run(run_id))?;
        let workspace = self
            .shared
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .cloned()
            .unwrap_or(WorkspaceInspection {
                mode: WorkspaceMode::Shared,
                branch: None,
                worktree_path: None,
                merge_mode: entry.config.merge_mode,
            });
        Ok(AgentInspection {
            run_id,
            role_name: entry.role.name().to_string(),
            phase: *entry.phase_rx.borrow(),
            message_count: *entry.message_count_rx.borrow(),
            workspace: Some(workspace),
        })
    }

    fn entry(&self, run_id: RunId) -> Result<RunEntryView<'_>, RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        if !runs.contains_key(&run_id) {
            return Err(unknown_run(run_id));
        }
        Ok(RunEntryView { runs, run_id })
    }

    /// AgentRun 間メッセージを配送する単一入口。
    ///
    /// 送信者・受信者の存否、自己宛防止、親子関係、メッセージ種別のルールを
    /// 臨界区内で検証し、受理できれば受信者の mailbox に追加してイベントを発行する。
    ///
    /// # Errors
    /// - 送信者または受信者が存在しない: [`RuntimeError::UnknownRun`]
    /// - 自己宛・sibling・無関係: [`RuntimeError::MessageDenied`]
    /// - Steering が親→子でない: [`RuntimeError::MessageDenied`]
    /// - Reply に `reply_to` なし: [`RuntimeError::MessageDenied`]
    /// - Reply の `reply_to` が相関関係と不一致: [`RuntimeError::UnknownMessage`]
    /// - 受信者が終端位相: [`RuntimeError::RunTerminated`]
    /// - mailbox 一杯: [`RuntimeError::MailboxFull`]
    pub fn send_agent_message(
        &self,
        sender: RunId,
        recipient: RunId,
        kind: AgentMessageKind,
        content: impl Into<String>,
        reply_to: Option<String>,
    ) -> Result<String, RuntimeError> {
        let (message_id, message, disposition) =
            self.prepare_delivery(sender, recipient, kind, content.into(), reply_to)?;
        self.shared.bus.emit(Event::new(EventKind::AgentMessage(
            AgentMessageEvent::Delivered {
                message,
                disposition,
            },
        )));
        Ok(message_id)
    }

    fn prepare_delivery(
        &self,
        sender: RunId,
        recipient: RunId,
        kind: AgentMessageKind,
        content: String,
        reply_to: Option<String>,
    ) -> Result<(String, AgentMessage, DeliveryDisposition), RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        let sender_entry = runs.get(&sender).ok_or_else(|| unknown_run(sender))?;
        let recipient_entry = runs.get(&recipient).ok_or_else(|| unknown_run(recipient))?;

        if sender == recipient {
            return Err(RuntimeError::MessageDenied {
                sender,
                recipient,
                detail: "自己宛てのメッセージは許可されていません".to_string(),
            });
        }

        let is_parent_to_child = sender_entry.parent == Some(recipient);
        let is_child_to_parent = recipient_entry.parent == Some(sender);

        if kind == AgentMessageKind::Steering && !is_child_to_parent {
            return Err(RuntimeError::MessageDenied {
                sender,
                recipient,
                detail: "steering は親から子へのみ許可されています".to_string(),
            });
        }

        if !is_parent_to_child && !is_child_to_parent {
            return Err(RuntimeError::MessageDenied {
                sender,
                recipient,
                detail: "親子関係のない run 間のメッセージは許可されていません".to_string(),
            });
        }

        let phase = *recipient_entry.phase_rx.borrow();
        if phase == AgentRunPhase::Done || phase == AgentRunPhase::Error {
            return Err(RuntimeError::RunTerminated {
                run_id: recipient.to_string(),
            });
        }

        let mut sent = self
            .shared
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let reply_correlation = if kind == AgentMessageKind::Reply {
            let reply_id = reply_to
                .as_ref()
                .ok_or_else(|| RuntimeError::MessageDenied {
                    sender,
                    recipient,
                    detail: "Reply には reply_to が必要です".to_string(),
                })?;
            let record = sent
                .get(reply_id)
                .ok_or_else(|| RuntimeError::UnknownMessage {
                    message_id: reply_id.clone(),
                })?;
            if record.recipient != sender || record.sender != recipient {
                return Err(RuntimeError::UnknownMessage {
                    message_id: reply_id.clone(),
                });
            }
            Some(reply_id.clone())
        } else {
            None
        };

        let message_id = format!(
            "msg-{}",
            self.shared.next_message_id.fetch_add(1, Ordering::Relaxed)
        );

        let message = AgentMessage {
            message_id: message_id.clone(),
            sender_run_id: sender.to_string(),
            recipient_run_id: recipient.to_string(),
            kind: kind.clone(),
            content: content.clone(),
            reply_to: reply_to.clone(),
        };

        let mailbox = Arc::clone(&recipient_entry.mailbox);
        if let Err(push_error) = mailbox.try_push(message.clone()) {
            return Err(match push_error {
                PushError::Full => RuntimeError::MailboxFull {
                    run_id: recipient.to_string(),
                },
                PushError::Closed => RuntimeError::RunTerminated {
                    run_id: recipient.to_string(),
                },
            });
        }

        if reply_correlation.is_none() {
            sent.insert(message_id.clone(), SentRecord { sender, recipient });
        }
        drop(sent);
        drop(runs);

        let disposition = match phase {
            AgentRunPhase::Waiting => DeliveryDisposition::Wake,
            AgentRunPhase::Pending | AgentRunPhase::Running => {
                if is_child_to_parent {
                    DeliveryDisposition::Steering
                } else {
                    DeliveryDisposition::Aside
                }
            }
            AgentRunPhase::Done | AgentRunPhase::Error => unreachable!(),
        };

        Ok((message_id, message, disposition))
    }

    /// `run_id` の inbox に届いているすべての AgentMessage を FIFO 順で取り出す。
    ///
    /// # Errors
    /// run_id が存在しない場合 [`RuntimeError::UnknownRun`] を返す。
    pub fn take_inbox(&self, run_id: RunId) -> Result<Vec<AgentMessage>, RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        let run_entry = runs.get(&run_id).ok_or_else(|| unknown_run(run_id))?;
        let mailbox = Arc::clone(&run_entry.mailbox);
        Ok(mailbox.drain_all())
    }

    /// `message_id` に対応する返信を最大 `timeout` まで待つ。
    ///
    /// 返信が到着すると返信メッセージを返す。相手 run が返信せずに終端した場合は
    /// [`RuntimeError::RunTerminated`]、制限時間を超えた場合は
    /// [`RuntimeError::ReplyTimeout`] を返す。タイムアウトした場合、遅延返信は
    /// 未読のまま inbox / 注入経路で後から観測される。
    ///
    /// # Errors
    /// - `message_id` が `run_id` が送信したものでない: [`RuntimeError::UnknownMessage`]
    /// - 返信元 run が終端: [`RuntimeError::RunTerminated`]
    /// - 待機時間超過: [`RuntimeError::ReplyTimeout`]
    pub async fn wait_reply(
        &self,
        run_id: RunId,
        message_id: &str,
        timeout: Duration,
    ) -> Result<AgentMessage, RuntimeError> {
        let (waiter_mailbox, mut replier_phase_rx, version_rx) = {
            let runs = lock_runs(&self.shared.runs);
            let waiter_entry = runs.get(&run_id).ok_or_else(|| unknown_run(run_id))?;
            let sent = self
                .shared
                .sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let record = sent
                .get(message_id)
                .ok_or_else(|| RuntimeError::UnknownMessage {
                    message_id: message_id.to_string(),
                })?;
            if record.sender != run_id {
                return Err(RuntimeError::UnknownMessage {
                    message_id: message_id.to_string(),
                });
            }
            let replier_entry =
                runs.get(&record.recipient)
                    .ok_or_else(|| RuntimeError::RunTerminated {
                        run_id: record.recipient.to_string(),
                    })?;
            let waiter_mailbox = Arc::clone(&waiter_entry.mailbox);
            let replier_phase_rx = replier_entry.phase_rx.clone();
            let version_rx = waiter_entry.mailbox.subscribe_version();
            (waiter_mailbox, replier_phase_rx, version_rx)
        };

        let mut version_rx = version_rx;
        let deadline = Instant::now() + timeout;

        let current_phase = *self.entry(run_id)?.phase_rx.borrow();
        if current_phase == AgentRunPhase::Running {
            let _ = self.transition_phase(run_id, AgentRunPhase::Waiting).await;
        }

        let result = loop {
            if let Some(reply) = waiter_mailbox.remove_first_where(|message| {
                message.kind == AgentMessageKind::Reply
                    && message.reply_to.as_deref() == Some(message_id)
            }) {
                self.remove_sent_record(message_id);
                break Ok(reply);
            }

            let current_replier_phase = *replier_phase_rx.borrow();
            if current_replier_phase == AgentRunPhase::Done
                || current_replier_phase == AgentRunPhase::Error
            {
                let replier_run_id = {
                    let _runs = lock_runs(&self.shared.runs);
                    self.shared
                        .sent
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(message_id)
                        .map(|record| record.recipient.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                };
                self.remove_sent_record(message_id);
                break Err(RuntimeError::RunTerminated {
                    run_id: replier_run_id,
                });
            }

            tokio::select! {
                changed = version_rx.changed() => {
                    if changed.is_err() {
                        break Err(RuntimeError::RunTerminated {
                            run_id: "unknown".to_string(),
                        });
                    }
                }
                changed = replier_phase_rx.changed() => {
                    if changed.is_err() {
                        self.remove_sent_record(message_id);
                        break Err(RuntimeError::RunTerminated {
                            run_id: run_id.to_string(),
                        });
                    }
                }
                _ = sleep_until(deadline) => {
                    self.remove_sent_record(message_id);
                    break Err(RuntimeError::ReplyTimeout {
                        message_id: message_id.to_string(),
                    });
                }
            }
        };

        let _ = self.transition_phase(run_id, AgentRunPhase::Running).await;
        result
    }

    async fn transition_phase(&self, run_id: RunId, to: AgentRunPhase) -> Result<(), RuntimeError> {
        let from = *self.entry(run_id)?.phase_rx.borrow();
        if from == to {
            return Ok(());
        }
        if !crate::state::is_valid_transition(from, to) {
            return Err(RuntimeError::InvalidTransition { from, to });
        }
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: run_id.to_string(),
                from,
                to,
                reason: None,
            }));
        let phase_tx = {
            let runs = lock_runs(&self.shared.runs);
            let entry = runs.get(&run_id).ok_or_else(|| unknown_run(run_id))?;
            entry.phase_tx.clone()
        };
        phase_tx.send_replace(to);
        Ok(())
    }

    fn remove_sent_record(&self, message_id: &str) {
        let mut sent = self
            .shared
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sent.remove(message_id);
    }
}

struct RunEntryView<'a> {
    runs: MutexGuard<'a, HashMap<RunId, RunEntry>>,
    run_id: RunId,
}

impl std::ops::Deref for RunEntryView<'_> {
    type Target = RunEntry;

    fn deref(&self) -> &Self::Target {
        &self.runs[&self.run_id]
    }
}

fn unknown_run(run_id: RunId) -> RuntimeError {
    RuntimeError::UnknownRun {
        run_id: run_id.to_string(),
    }
}

fn lock_runs(runs: &Mutex<HashMap<RunId, RunEntry>>) -> MutexGuard<'_, HashMap<RunId, RunEntry>> {
    runs.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn loop_shared(shared: &Weak<Shared>) -> Option<LoopShared> {
    shared.upgrade().map(|shared| LoopShared {
        bus: Arc::clone(&shared.bus),
        executor: Arc::clone(&shared.executor),
        model: Arc::clone(&shared.model),
        system_prompts: shared.system_prompts.get().cloned(),
        skills: shared.skills.get().cloned(),
        runtime: Arc::downgrade(&shared),
    })
}
