//! AgentRun の登録と公開操作を提供するランタイム表層。

mod restore_delivery;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

use crate::agent_loop::{LoopChannels, LoopShared, RunHandoff, RunTask, run_agent};
use crate::compaction::policy::CompactionSettings;
use crate::entry_routing::EntryRouter;
use crate::escalation::{EscalationMemo, EscalationSettings};
use crate::mailbox::{PushError, RunMailbox};
use crate::orchestration::GoalGate;
use crate::prompt::{
    CatalogBuildInput, PromptCompositionError, SystemPromptCatalog, build_catalog,
};
use crate::rules::RulesSource;
use crate::run::{RunConfig, WorkspaceInspection, WorkspaceMode};
use crate::skill::{SkillRegistry, SkillScope, discover_skills};
use crate::workspace::{OwnedWorktree, WorktreeManager};
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
    pub(crate) shared: Arc<Shared>,
}

type LearningRunReceivers = Mutex<HashMap<RunId, watch::Receiver<Option<Result<(), String>>>>>;

pub(crate) struct Shared {
    pub(crate) learning: OnceLock<crate::memory_queue::LearningSettings>,
    pub(crate) learning_runs: LearningRunReceivers,
    topology: OnceLock<crate::CoordinationTopology>,
    pub(crate) bus: Arc<EventBus>,
    pub(crate) executor: Arc<ToolExecutor>,
    pub(crate) snapshots: OnceLock<Arc<crate::snapshot::SnapshotService>>,
    pub(crate) model: Arc<dyn AgentModel>,
    pub(crate) system_prompts: OnceLock<Arc<SystemPromptCatalog>>,
    pub(crate) skills: OnceLock<Arc<SkillRegistry>>,
    pub(crate) rules: OnceLock<Arc<RulesSource>>,
    pub(crate) compaction: OnceLock<CompactionSettings>,
    pub(crate) run_store: OnceLock<crate::RunStore>,
    pub(crate) model_resolution: OnceLock<crate::model_resolve::ModelResolution>,
    pub(crate) compaction_configured: AtomicBool,
    pub(crate) escalation_settings: OnceLock<EscalationSettings>,
    pub(crate) escalations: Mutex<HashMap<RunId, EscalationMemo>>,
    /// goal gate (finish 判定 seam、issue #73 T1.3)。未設定なら finish は
    /// legacy の即時受理のまま残る (T3.1 が `meta::finish` から参照する)。
    pub(crate) goals: OnceLock<Arc<dyn GoalGate>>,
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
    escalated_from: Option<RunId>,
    phase_tx: watch::Sender<AgentRunPhase>,
    phase_rx: watch::Receiver<AgentRunPhase>,
    message_count_rx: watch::Receiver<usize>,
    inbox_tx: mpsc::Sender<(String, Vec<crate::DelegateImage>)>,
    cancel_tx: watch::Sender<bool>,
    compact_tx: watch::Sender<u64>,
    model_preference_tx: watch::Sender<Option<crate::ModelPreference>>,
    /// run の最終 assistant テキスト (loop 側 result_tx と対になる観測口)。
    result_rx: watch::Receiver<Option<String>>,
    /// 実行中の圧縮を loop 側と共有するフラグ (compact() の in-flight 拒否判定用)。
    compaction_busy: Arc<AtomicBool>,
    mailbox: Arc<RunMailbox>,
    _join: Option<JoinHandle<()>>,
}

struct SentRecord {
    sender: RunId,
    recipient: RunId,
}

impl AgentRuntime {
    pub fn team_tasks(&self) -> Vec<(RunId, Vec<crate::team::TeamTask>)> {
        lock_runs(&self.shared.runs)
            .iter()
            .filter_map(|(id, entry)| {
                let team = entry.config.team.as_ref()?;
                (*id == team.coordinator).then(|| (*id, team.board.snapshot().unwrap_or_default()))
            })
            .collect()
    }

    pub fn with_snapshots(self, service: Arc<crate::snapshot::SnapshotService>) -> Self {
        let _ = self.shared.snapshots.set(service);
        self
    }

    pub async fn restore_snapshot(
        &self,
        run_id: RunId,
        redo: bool,
    ) -> Result<Option<String>, String> {
        self.validate_run_mutation(run_id)
            .map_err(|error| error.to_string())?;
        let (owner, phase) = {
            let entry = self.entry(run_id).map_err(|error| error.to_string())?;
            (
                entry
                    .config
                    .name
                    .clone()
                    .unwrap_or_else(|| run_id.to_string()),
                *entry.phase_rx.borrow(),
            )
        };
        if phase == AgentRunPhase::Running || phase == AgentRunPhase::Pending {
            return Err("Wait for the run to become idle before restoring files".into());
        }
        let service = self
            .shared
            .snapshots
            .get()
            .ok_or("Snapshots are not configured")?;
        let root = self
            .shared
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .and_then(|workspace| workspace.worktree_path.clone());
        let mut workspace = service
            .lock(root.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        tokio::task::spawn_blocking(move || workspace.restore(&owner, redo))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }

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
                topology: OnceLock::new(),
                learning: OnceLock::new(),
                learning_runs: Mutex::new(HashMap::new()),
                bus,
                executor,
                snapshots: OnceLock::new(),
                model,
                system_prompts: OnceLock::new(),
                skills: OnceLock::new(),
                rules: OnceLock::new(),
                compaction: OnceLock::new(),
                run_store: OnceLock::new(),
                model_resolution: OnceLock::new(),
                compaction_configured: AtomicBool::new(false),
                escalation_settings: OnceLock::new(),
                escalations: Mutex::new(HashMap::new()),
                goals: OnceLock::new(),
                workspace: None,
                workspaces: Mutex::new(HashMap::new()),
                next_run_id: AtomicU64::new(1),
                next_message_id: AtomicU64::new(1),
                runs: Mutex::new(HashMap::new()),
                sent: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// 終端保存先を接続する。設定済みの場合は先勝ちで変更しない。
    pub fn with_run_store(self, store: crate::RunStore) -> Self {
        self.shared
            .next_run_id
            .fetch_max(store.next_run_id, Ordering::Relaxed);
        let _ = self.shared.run_store.set(store);
        self
    }

    /// コンテキスト圧縮設定を接続したランタイムを返す。
    pub fn with_compaction(self, settings: config::CompactionConfig) -> Self {
        let _ = self
            .shared
            .compaction
            .set(CompactionSettings::from(&settings));
        self.shared
            .compaction_configured
            .store(true, Ordering::Release);
        self
    }

    pub(crate) fn with_model_resolution(self, config: &config::Config) -> Self {
        let _ = self
            .shared
            .topology
            .set(crate::CoordinationTopology::from_config(&config.team));
        let _ = self
            .shared
            .model_resolution
            .set(crate::model_resolve::ModelResolution::new(config));
        let _ = self
            .shared
            .compaction
            .set(CompactionSettings::from(&config.compaction));
        self
    }

    /// エスカレーション検出設定を接続したランタイムを返す。
    ///
    /// 設定済みの場合は 2 回目以降の呼び出しを無視する先勝ち契約である。
    pub fn with_escalation_settings(self, settings: EscalationSettings) -> Self {
        let _ = self.shared.escalation_settings.set(settings);
        self
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

    /// プロジェクトルール読み込み元を設定したランタイムを返すビルダーメソッド。
    ///
    /// 設定済みの場合は 2 回目以降の呼び出しを無視する先勝ち契約で、実際のルール
    /// 注入は agent loop 統合を行う後続 Wave が担う。
    pub fn with_project_rules(self, rules: Arc<RulesSource>) -> Self {
        let _ = self.shared.rules.set(rules);
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

    /// config から system prompt catalog を組み立て、skill の 2 スコープ発見
    /// 結果を metadata として組み込み、registry と catalog を同時に接続する
    /// composition root (issue #53 / AC4)。run 開始時は name+description の
    /// metadata のみが Orchestrator の keyTriggers へ露出し、本体は load されない。
    ///
    /// skill metadata は [`discover_skills`] の発見結果から構成されるため、
    /// `input.available_skills` はこの経路では使用しない (呼び出し側の契約:
    /// この builder に渡した availability の skill 部分は発見結果で意図的に
    /// 置き換えられる)。診断の観測経路は [`AgentRuntime::with_skills`] の Fault
    /// 発行に一本化され、この builder で重複発行はしない。
    ///
    /// # Errors
    /// [`build_catalog`] の失敗をそのまま伝播する。
    pub fn with_config_prompts_and_skills(
        self,
        input: &CatalogBuildInput<'_>,
        skill_dirs: &[(SkillScope, PathBuf)],
    ) -> Result<Self, PromptCompositionError> {
        let registry = discover_skills(skill_dirs);
        let available_skills = registry.available_skills();
        let catalog = build_catalog(&CatalogBuildInput {
            config: input.config,
            user_presets_dir: input.user_presets_dir,
            available_agents: input.available_agents,
            available_skills: &available_skills,
        })?;
        Ok(self
            .with_system_prompts(Arc::new(catalog))
            .with_skills(Arc::new(registry)))
    }

    /// production 構成のランタイムを生成する。
    ///
    /// `build_sandbox(&ExecutionPolicy, workspace)` 経由で role の network
    /// capability を bwrap policy へ伝播し、標準ツールを持つ ToolExecutor に注入する
    /// composition root (PR #22 の fail-closed 経路 / implementation.md:48)。
    /// 標準ツールに加えて web_search / web_fetch を [`ToolExecutor::with_web_tools`]
    /// で登録し、NetworkGuard 初期化の失敗はフォールバックせずエラーを返す。
    /// bwrap の検出・検証に失敗した場合はエラーをそのまま伝播する。
    /// DirectSandbox へのフォールバック経路は存在しない (ADR 0021)。
    pub fn production(
        bus: Arc<EventBus>,
        policy: &ExecutionPolicy,
        workspace_root: PathBuf,
        model: Arc<dyn AgentModel>,
    ) -> Result<Self, RuntimeError> {
        let executor = production_executor(Arc::clone(&bus), policy, workspace_root)?;
        Ok(Self::new(bus, executor, model))
    }

    /// production sandbox と isolated workspace context を持つランタイムを生成する。
    ///
    /// baseline executor は [`AgentRuntime::production`] と同様に web_search /
    /// web_fetch を [`ToolExecutor::with_web_tools`] で登録する。
    ///
    /// # Errors
    /// project 検証、baseline sandbox 構築、または NetworkGuard 初期化に失敗した
    /// 場合に [`RuntimeError`] を返す。
    pub fn production_with_project(
        bus: Arc<EventBus>,
        policy: &ExecutionPolicy,
        project_root: PathBuf,
        model: Arc<dyn AgentModel>,
    ) -> Result<Self, RuntimeError> {
        let seam = crate::compose::WorkspaceSeam::production(project_root)?;
        let executor =
            production_executor(Arc::clone(&bus), policy, seam.repo_root().to_path_buf())?;
        let (manager, factory) = seam.into_manager_and_factory();
        Ok(Self::with_workspace_context(
            bus, executor, model, manager, factory,
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
                topology: OnceLock::new(),
                system_prompts: OnceLock::new(),
                skills: OnceLock::new(),
                rules: OnceLock::new(),
                compaction: OnceLock::new(),
                run_store: OnceLock::new(),
                model_resolution: OnceLock::new(),
                compaction_configured: AtomicBool::new(false),
                escalation_settings: OnceLock::new(),
                escalations: Mutex::new(HashMap::new()),
                goals: OnceLock::new(),
                workspace: Some(WorkspaceContext { manager, factory }),
                learning: OnceLock::new(),
                learning_runs: Mutex::new(HashMap::new()),
                snapshots: OnceLock::new(),
                workspaces: Mutex::new(HashMap::new()),
                next_run_id: AtomicU64::new(1),
                next_message_id: AtomicU64::new(1),
                runs: Mutex::new(HashMap::new()),
                sent: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// goal gate (finish 判定 seam) を設定したランタイムを返すビルダーメソッド。
    ///
    /// 設定済みの場合は 2 回目以降の呼び出しは無視される (先勝ち、compaction
    /// 設定と同一規約)。gate 未設定の finish は legacy の即時受理のまま残り、
    /// gate 経由の判定への差し替えは T3.1 が `meta::finish` で行う。
    pub fn with_goal_gate(self, gate: Arc<dyn GoalGate>) -> Self {
        let _ = self.shared.goals.set(gate);
        self
    }

    /// 接続済みの goal gate を返す (未設定なら `None`)。
    pub(crate) fn goal_gate(&self) -> Option<Arc<dyn GoalGate>> {
        self.shared.goals.get().cloned()
    }

    pub(crate) fn attach_goal_child(&self, parent: RunId, child: RunId, role: Role) {
        if let Some(gate) = self.goal_gate() {
            gate.attach_child(parent, child, role);
        }
    }

    pub(crate) fn record_escalation_memo(&self, run_id: RunId, memo: EscalationMemo) {
        self.shared
            .escalations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run_id, memo);
    }

    /// 指定 run に記録されたエスカレーションメモを返す。
    pub fn escalation_memo(&self, run_id: RunId) -> Option<EscalationMemo> {
        self.shared
            .escalations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .cloned()
    }

    /// entry pre-routing 判定器を返す。
    ///
    /// 再分類にはこの runtime のモデル (= 起動予定の Orchestrator と同じモデル) が
    /// 構造的に使われる (issue #71 / AC3)。
    pub fn entry_router(&self) -> EntryRouter {
        EntryRouter::new(Arc::clone(&self.shared.model), Arc::clone(&self.shared.bus))
            .with_topology(self.shared.topology.get().copied().unwrap_or_default())
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
        let run_id = self.reserve_child_run_id(parent)?;
        Ok(self.spawn_reserved_child(parent, run_id, role, prompt, config))
    }

    /// 親 run の存在を検証したうえで、次の run ID を事前採番する。
    ///
    /// supervisor が child 起動前に `RunAttached` / `ContinuationDispatched` を
    /// emit するための happens-before を組む用途 (issue #83)。採番後は
    /// [`AgentRuntime::spawn_reserved_child`] で必ず起動すること (採番だけして
    /// 起動しないと run ID に欠番が生じる)。
    ///
    /// # Errors
    /// 親 run が存在しない場合 [`RuntimeError::UnknownRun`] を返す。
    pub fn reserve_child_run_id(&self, parent: RunId) -> Result<RunId, RuntimeError> {
        {
            let runs = lock_runs(&self.shared.runs);
            if !runs.contains_key(&parent) {
                return Err(unknown_run(parent));
            }
        }
        Ok(self.reserve_run_id())
    }

    /// 次の run ID を事前採番する。
    ///
    /// root run のように親を持たない run でも、起動前に ID を確定させて
    /// イベント発行との happens-before を組む用途 (issue #83)。採番後は
    /// [`AgentRuntime::spawn_reserved`] で必ず起動すること。
    pub fn reserve_run_id(&self) -> RunId {
        RunId::new(self.shared.next_run_id.fetch_add(1, Ordering::Relaxed))
    }

    /// [`AgentRuntime::reserve_run_id`] / [`AgentRuntime::reserve_child_run_id`]
    /// で事前採番した ID を使って run を登録し、バックグラウンド実行を開始する。
    ///
    /// 呼び出し側が起動前に run ID 確定のイベントを emit 済みでも、bus 上の
    /// 順序が run の活動より必ず先行するよう、登録・起動はこの呼び出しで
    /// 一括して行う。
    pub fn spawn_reserved(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: impl Into<String>,
        config: RunConfig,
    ) -> RunId {
        self.spawn_run_with_handoff(run_id, parent, role, prompt.into(), config, None)
    }

    /// [`AgentRuntime::reserve_child_run_id`] で事前採番した ID を使って
    /// child run を登録し、バックグラウンド実行を開始する。
    /// parent の存在検証は reserve 時に済んでいる前提。
    pub fn spawn_reserved_child(
        &self,
        parent: RunId,
        run_id: RunId,
        role: Role,
        prompt: impl Into<String>,
        config: RunConfig,
    ) -> RunId {
        self.spawn_reserved(run_id, Some(parent), role, prompt, config)
    }

    fn spawn_run(
        &self,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        config: RunConfig,
    ) -> RunId {
        let run_id = RunId::new(self.shared.next_run_id.fetch_add(1, Ordering::Relaxed));
        self.spawn_run_with_handoff(run_id, parent, role, prompt, config, None)
    }

    fn spawn_run_with_handoff(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        mut config: RunConfig,
        handoff: Option<RunHandoff>,
    ) -> RunId {
        if let Some(parent) = parent {
            if let Some(entry) = lock_runs(&self.shared.runs).get(&parent) {
                config.topology = entry.config.topology;
                config.team = entry.config.team.clone();
                config.delegation_value = entry.config.delegation_value.clone();
            }
            config.ownership = lock_runs(&self.shared.runs)
                .get(&parent)
                .and_then(|entry| entry.config.ownership.clone())
                .or(config.ownership);
        }
        if parent.is_none() {
            config.team = None;
        }
        let team_setup = if (config.topology.worker_limit().is_some() || config.team.is_some())
            && config
                .delegation_value
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            Err("team mode requires explicit delegation value".to_string())
        } else if role == Role::Orchestrator
            && config.team.is_none()
            && let Some(limit) = config.topology.worker_limit()
        {
            match config.team_store.as_ref() {
                Some(store) => crate::team_context::TeamContext::persistent(run_id, store, limit)
                    .map(|team| config.team = Some(team))
                    .map_err(|error| error.to_string()),
                None => Err("team mode requires shared durable storage".to_string()),
            }
        } else {
            Ok(())
        };
        let worker_slot = match (team_setup, &config.team, role) {
            (Err(error), _, _) => Some(Err(error)),
            (Ok(()), Some(team), Role::Worker) => Some(team.reserve_worker().and_then(|slot| {
                if let Some(spec) = &config.team_task {
                    team.board
                        .enqueue(spec.clone())
                        .map_err(|error| error.to_string())?;
                }
                Ok(slot)
            })),
            _ => None,
        };
        if let Some(permit) = &mut config.ownership {
            permit.run_id = Some(run_id.to_string());
            let token = permit.clone();
            self.shared.bus.register_mutation_guard(
                run_id.to_string(),
                Arc::new(move || {
                    token
                        .mutation_guard()
                        .ok()
                        .map(|guard| Box::new(guard) as Box<dyn event_bus::MutationGuard>)
                }),
            );
        }
        let escalated_from = handoff.as_ref().map(|handoff| handoff.source_run_id);
        let prompt = match &config.memory {
            Some(memory) if handoff.is_none() => memory.augment(prompt),
            Some(_) | None => prompt,
        };
        let prompt = match (&config.team, &config.team_task) {
            (Some(_), Some(task)) => format!(
                "{prompt}\nTeam task id: {:?}. Claim it with task_claim before editing; pass its generation to task_complete. Owned paths: {:?}",
                task.id, task.paths
            ),
            _ => prompt,
        };
        let name = config
            .name
            .clone()
            .unwrap_or_else(|| role.name().to_string());
        let model = self.shared.model.selected_model(role);
        let (phase_tx, phase_rx) = watch::channel(AgentRunPhase::Pending);
        let (message_count_tx, message_count_rx) = watch::channel(0);
        let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (compact_tx, compact_rx) = watch::channel(0_u64);
        let (model_preference_tx, model_preference_rx) =
            watch::channel(config.model_preference.clone());
        let (result_tx, result_rx) = watch::channel(None::<String>);
        let compaction_busy = Arc::new(AtomicBool::new(false));
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
            handoff,
            restored: None,
        };
        let channels = LoopChannels {
            phase_tx,
            message_count_tx,
            inbox_rx,
            cancel_rx,
            mailbox_version_rx,
            compact_rx,
            model_preference_rx,
            compaction_busy: compaction_busy.clone(),
            result_tx,
        };
        lock_runs(&self.shared.runs).insert(
            run_id,
            RunEntry {
                role,
                name: name.clone(),
                model,
                config: config.clone(),
                parent,
                escalated_from,
                phase_tx: phase_tx_entry,
                phase_rx,
                message_count_rx,
                inbox_tx,
                cancel_tx,
                compact_tx,
                model_preference_tx,
                result_rx,
                compaction_busy: Arc::clone(&compaction_busy),
                mailbox: Arc::clone(&mailbox),
                _join: None,
            },
        );
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::AgentRunStarted {
                run_id: run_id.to_string(),
                parent_run_id: parent.map(|parent| parent.to_string()),
                agent_name: name,
                role: role.name().to_lowercase(),
            }));
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
        let learning = self.prepare_learning(&task);
        let join = tokio::spawn(async move {
            let _slot = match worker_slot {
                Some(Ok(slot)) => Some(slot),
                Some(Err(reason)) => {
                    if let Some(shared) = weak.upgrade() {
                        shared
                            .bus
                            .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
                                run_id: task.run_id.to_string(),
                                from: AgentRunPhase::Pending,
                                to: AgentRunPhase::Error,
                                reason: Some(reason),
                            }));
                    }
                    let _ = channels.phase_tx.send(AgentRunPhase::Error);
                    return;
                }
                None => None,
            };
            let _monitor = task.config.team.clone().map(|team| {
                crate::team_context::monitor(
                    weak.clone(),
                    team,
                    task.run_id,
                    channels.phase_tx.subscribe(),
                )
            });
            run_agent(weak.clone(), task, channels).await;
            if let Some(learning) = learning {
                learning.complete(weak, run_id).await;
            }
        });
        if let Some(entry) = lock_runs(&self.shared.runs).get_mut(&run_id) {
            entry._join = Some(join);
        }
        run_id
    }

    pub(crate) fn spawn_escalated_root(
        &self,
        memo: EscalationMemo,
        source_config: &RunConfig,
        worktree: Option<OwnedWorktree>,
    ) -> RunId {
        let config = RunConfig {
            interactive: false,
            name: Some("escalation-orchestrator".to_string()),
            category: None,
            load_skills: Vec::new(),
            workspace_mode: source_config.workspace_mode,
            merge_mode: source_config.merge_mode,
            network_access: Default::default(),
            workspace_branch: worktree.as_ref().map(|owned| owned.branch.clone()),
            ..RunConfig::default()
        };
        let source_run_id = memo.source_run_id;
        let run_id = RunId::new(self.shared.next_run_id.fetch_add(1, Ordering::Relaxed));
        self.spawn_run_with_handoff(
            run_id,
            None,
            Role::Orchestrator,
            crate::escalation::prompt::render_escalation_prompt(&memo),
            config,
            Some(RunHandoff {
                source_run_id,
                worktree,
            }),
        )
    }

    /// エスカレーションで生成された run の移譲元を返す。
    ///
    /// # Errors
    /// run_id が存在しない場合 [`RuntimeError::UnknownRun`] を返す。
    pub fn escalation_source(&self, run_id: RunId) -> Result<Option<RunId>, RuntimeError> {
        Ok(self.entry(run_id)?.escalated_from)
    }

    /// 対話待機中の run へユーザーメッセージを送る。
    pub fn send_message(&self, run_id: RunId, text: String) -> Result<(), RuntimeError> {
        self.send_message_with_images(run_id, text, Vec::new())
    }

    pub fn send_message_with_images(
        &self,
        run_id: RunId,
        text: String,
        images: Vec<crate::DelegateImage>,
    ) -> Result<(), RuntimeError> {
        self.validate_run_mutation(run_id)?;
        let phase = *self.entry(run_id)?.phase_rx.borrow();
        if phase == AgentRunPhase::Done || phase == AgentRunPhase::Error {
            return Err(RuntimeError::RunTerminated {
                run_id: run_id.to_string(),
            });
        }
        let sender = self.entry(run_id)?.inbox_tx.clone();
        sender
            .try_send((text, images))
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

    /// run の最終 assistant テキストを返す (未確定なら `None`)。
    ///
    /// 正常終了 (モデルの自然 Stop または finish meta-op) でのみ記録され、
    /// Error / cancel 終端の run は `None` のままである。reviewer 判定の解析
    /// のように run 出力を観測する supervisor 経路 (issue #73 §7) のための API である。
    ///
    /// # Errors
    /// run_id が存在しない場合 [`RuntimeError::UnknownRun`] を返す。
    pub fn run_result(&self, run_id: RunId) -> Result<Option<String>, RuntimeError> {
        let entry = self.entry(run_id)?;
        Ok(entry.result_rx.borrow().clone())
    }

    /// run へキャンセルを通知する。複数回の通知は同じ結果となる。
    pub fn cancel(&self, run_id: RunId) -> Result<(), RuntimeError> {
        let sender = self.entry(run_id)?.cancel_tx.clone();
        sender.send_replace(true);
        Ok(())
    }

    /// Changes the next completion's selection without interrupting an in-flight request.
    /// `None` restores normal routing and its existing session affinity.
    ///
    /// # Errors
    /// Returns [`RuntimeError::UnknownRun`] if the run is not registered.
    pub fn set_model_preference(
        &self,
        run_id: RunId,
        preference: Option<crate::ModelPreference>,
    ) -> Result<(), RuntimeError> {
        self.validate_run_mutation(run_id)?;
        self.entry(run_id)?
            .model_preference_tx
            .send_replace(preference);
        Ok(())
    }

    /// run の次のターン境界で手動コンテキスト圧縮を要求する。
    pub fn compact(&self, run_id: RunId) -> Result<(), RuntimeError> {
        self.validate_run_mutation(run_id)?;
        let entry = self.entry(run_id)?;
        if entry.compaction_busy.load(Ordering::Acquire) {
            return Err(RuntimeError::CompactionInFlight {
                run_id: run_id.to_string(),
            });
        }
        entry
            .compact_tx
            .send_modify(|generation| *generation = generation.saturating_add(1));
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

    fn validate_run_mutation(&self, run_id: RunId) -> Result<(), RuntimeError> {
        let permit = self.entry(run_id)?.config.ownership.clone();
        if let Some(permit) = permit {
            permit
                .validate_generation()
                .map_err(|_| RuntimeError::StaleOwnership {
                    run_id: run_id.to_string(),
                })?;
        }
        Ok(())
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
    /// - 終端・未登録 run の復元失敗: [`RuntimeError::RunRestoreFailed`]
    /// - mailbox 一杯: [`RuntimeError::MailboxFull`]
    pub fn send_agent_message(
        &self,
        sender: RunId,
        recipient: RunId,
        kind: AgentMessageKind,
        content: impl Into<String>,
        reply_to: Option<String>,
    ) -> Result<String, RuntimeError> {
        self.validate_run_mutation(sender)?;
        let live = lock_runs(&self.shared.runs)
            .get(&recipient)
            .is_some_and(|entry| {
                matches!(
                    *entry.phase_rx.borrow(),
                    AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                )
            });
        let (message_id, message, disposition) = if live {
            self.validate_run_mutation(recipient)?;
            self.prepare_delivery(sender, recipient, kind, content.into(), reply_to)?
        } else {
            self.restore_and_deliver(
                sender,
                recipient,
                AgentMessage {
                    message_id: String::new(),
                    sender_run_id: sender.to_string(),
                    recipient_run_id: recipient.to_string(),
                    kind,
                    content: content.into(),
                    reply_to,
                },
            )?
        };
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
        match self.try_live_delivery(
            sender,
            recipient,
            kind.clone(),
            content.clone(),
            reply_to.clone(),
        ) {
            Err(RuntimeError::RunTerminated { .. }) => self.restore_and_deliver(
                sender,
                recipient,
                AgentMessage {
                    message_id: String::new(),
                    sender_run_id: sender.to_string(),
                    recipient_run_id: recipient.to_string(),
                    kind,
                    content,
                    reply_to,
                },
            ),
            result => result,
        }
    }

    fn try_live_delivery(
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
        if matches!(phase, AgentRunPhase::Done | AgentRunPhase::Error) {
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
        // issue #98: LoopState::transition (agent_loop.rs) と同じく、位相 commit を
        // イベント発行に happens-before させる。逆順だと emit 直後の観測者が
        // phase_rx から旧位相を読みうる (chat_sink_runtime flake の根本原因)。
        let phase_tx = {
            let runs = lock_runs(&self.shared.runs);
            let entry = runs.get(&run_id).ok_or_else(|| unknown_run(run_id))?;
            entry.phase_tx.clone()
        };
        phase_tx.send_replace(to);
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: run_id.to_string(),
                from,
                to,
                reason: None,
            }));
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

/// production sandbox と標準ツール群を持つ executor を構築する。
///
/// # Errors
/// sandbox 構築または web tool の network guard 初期化に失敗した場合に返す。
pub fn production_executor(
    bus: Arc<EventBus>,
    policy: &ExecutionPolicy,
    workspace_root: PathBuf,
) -> Result<Arc<ToolExecutor>, RuntimeError> {
    let sandbox = crate::network::build_sandbox(policy, workspace_root).map_err(|error| {
        RuntimeError::Sandbox {
            detail: error.to_string(),
        }
    })?;
    ToolExecutor::with_standard_tools(bus, sandbox)
        .with_web_tools()
        .map(Arc::new)
        .map_err(|error| RuntimeError::NetworkGuard {
            detail: error.to_string(),
        })
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
        rules: shared.rules.get().cloned(),
        compaction: shared.compaction.get().cloned().unwrap_or_default(),
        compaction_configured: shared.compaction_configured.load(Ordering::Acquire),
        escalation: shared
            .escalation_settings
            .get()
            .copied()
            .unwrap_or_default(),
        runtime: Arc::downgrade(&shared),
    })
}
