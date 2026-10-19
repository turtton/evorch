//! AgentRun の登録と公開操作を提供するランタイム表層。

mod admission;
mod artifacts;
mod benchmark;
pub(crate) use benchmark::production_executor as benchmark_executor;
mod cancellation;
mod chat_restore;
mod completion_relay;
mod output;
mod project;
mod questions;
pub use project::{ProjectModelResolver, ProjectSlugResolver};
mod restore_delivery;
mod run_panic;
mod shell_handle_restore;
use chat_restore::RunContinuation;

use std::collections::{HashMap, HashSet};
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
use crate::prompt::{
    CatalogBuildInput, PromptCompositionError, SystemPromptCatalog, build_catalog,
};
use crate::rules::RulesSource;
use crate::run::{RunConfig, WorkspaceInspection};
use crate::skill::{SkillRegistry, SkillScope, discover_skills};
use crate::skill_source::SkillCatalogSource;
use crate::state::{RunInterrupt, StopScope};
use crate::{AgentInspection, AgentModel, AgentSummary, ExecutionPolicy, RunId, RuntimeError};

pub(crate) mod user_inbox;
pub use user_inbox::FollowUpStatus;
use user_inbox::UserInbox;

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
    pub(crate) thread_goals: Mutex<crate::thread_goals::ThreadGoals>,
    pub(crate) benchmark_recording: OnceLock<crate::benchmark::Recording>,
    pub(crate) benchmark_replays: Mutex<HashMap<RunId, crate::benchmark::BenchmarkCheckpoint>>,
    pub(crate) benchmark_executors: Mutex<HashMap<RunId, Arc<ToolExecutor>>>,
    pub(crate) question_version: watch::Sender<u64>,
    pub(crate) lesson_staging: Mutex<crate::learning::LearningStaging>,
    admissions: admission::Admissions,
    spawn_intents: Mutex<HashMap<RunId, cancellation::SpawnIntent>>,
    pub(crate) learning: OnceLock<crate::memory_queue::LearningSettings>,
    pub(crate) self_improvement: OnceLock<crate::self_improvement::ImprovementSettings>,
    pub(crate) self_improvement_task: OnceLock<crate::self_improvement::lifecycle::ObserverTask>,
    pub(crate) learning_runs: LearningRunReceivers,
    topology: OnceLock<crate::CoordinationTopology>,
    pub(crate) bus: Arc<EventBus>,
    pub(crate) executor: Mutex<Arc<ToolExecutor>>,
    pub(crate) artifacts: OnceLock<Arc<crate::artifacts::ArtifactStore>>,
    pub(crate) artifact_pages: OnceLock<Arc<dyn crate::artifacts::ArtifactPages>>,
    pub(crate) model: Arc<dyn AgentModel>,
    pub(crate) system_prompts: OnceLock<Arc<SystemPromptCatalog>>,
    pub(crate) skills: OnceLock<Arc<SkillRegistry>>,
    pub(crate) skill_source: OnceLock<Arc<SkillCatalogSource>>,
    pub(crate) rules: Mutex<Option<Arc<RulesSource>>>,
    pub(crate) compaction: OnceLock<CompactionSettings>,
    budget: OnceLock<crate::budget_tracker::BudgetSettings>,
    pub(crate) run_store: OnceLock<crate::RunStore>,
    pub(crate) model_resolution: OnceLock<crate::model_resolve::ModelResolution>,
    pub(crate) comment_checker: OnceLock<config::CommentCheckerConfig>,
    pub(crate) orchestration: OnceLock<config::OrchestrationConfig>,
    pub(crate) web_tools_enabled: AtomicBool,
    pub(crate) sandbox_escalation: Arc<Mutex<(config::EscalationApproval, bool)>>,
    pub(crate) sandbox_root: Mutex<Option<PathBuf>>,
    pub(crate) thread_scratch: Mutex<Option<Arc<crate::scratch::ThreadScratch>>>,
    pub(crate) compaction_configured: AtomicBool,
    pub(crate) escalation_settings: OnceLock<EscalationSettings>,
    pub(crate) escalations: Mutex<HashMap<RunId, EscalationMemo>>,
    pub(crate) review_runs: Mutex<HashMap<RunId, crate::escalation_review::ReviewRunContext>>,
    pub(crate) sandbox_factory: Option<Arc<dyn SandboxFactory>>,
    pub(crate) workspaces: Mutex<HashMap<RunId, WorkspaceInspection>>,
    pub(crate) run_ids: crate::run_ids::RunIds,
    /// Project that root runs bind to when their config names none.
    pub(crate) active_project_root: Mutex<Option<PathBuf>>,
    pub(crate) projects: Mutex<HashMap<PathBuf, Arc<project::ProjectContext>>>,
    pub(crate) project_models: OnceLock<project::ProjectModelResolver>,
    pub(crate) project_slugs: OnceLock<project::ProjectSlugResolver>,
    /// Trust the host declared per canonical project root; undeclared roots keep the shared rules' trust.
    pub(crate) project_trust: Mutex<HashMap<PathBuf, crate::rules::ProjectTrust>>,
    /// Allowed directories per canonical project root; runs hold the same live list.
    pub(crate) allowed_directories: Mutex<HashMap<PathBuf, sandbox::LivePaths>>,
    next_message_id: AtomicU64,
    next_reference_token: AtomicU64,
    runs: Mutex<HashMap<RunId, RunEntry>>,
    sent: Mutex<HashMap<String, SentRecord>>,
}

impl Shared {
    pub(crate) fn rules(&self) -> Option<Arc<RulesSource>> {
        self.rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// run の sandbox を構築するための mount policy 入力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMounts {
    /// run が作業するプロジェクトルート。
    pub workspace_root: PathBuf,
    /// 読み取り専用で公開する path。
    pub ro_binds: Vec<PathBuf>,
    /// 読み書き可能で公開する path。
    pub rw_binds: Vec<PathBuf>,
    /// shell の `TMPDIR`。`None` なら sandbox ごとの一時領域を使う。
    pub scratch: Option<PathBuf>,
    /// The project's allowed directories, read again on every command, with this run's access.
    pub allowed: Option<sandbox::AllowedRegions>,
}

impl WorkspaceMounts {
    /// A run in the project root without extra mounts.
    pub fn shared(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            ro_binds: Vec::new(),
            rw_binds: Vec::new(),
            scratch: None,
            allowed: None,
        }
    }
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
        mounts: &WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum HostProvenance {
    FreshHostSubmission,
    PersistedHostSubmission { run_id: RunId },
}

pub(crate) struct ReviewSeed {
    provenance: HostProvenance,
    context: crate::escalation_review::ReviewRunContext,
}

/// An unconsumed assistant reply reference. Its token prevents consuming a newer reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReference {
    pub token: u64,
    pub context: String,
    pub reference_id: String,
    pub source_run_id: String,
    pub turn_end: usize,
    storage: ReferenceStorage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceStorage {
    Ephemeral { created_at_ns: i64 },
    Durable,
}

impl PendingReference {
    fn evidence_context(&self) -> crate::escalation_review::ReferenceContext {
        crate::escalation_review::ReferenceContext {
            text: self.context.clone(),
            source_run_id: self.source_run_id.clone(),
            turn_end: self.turn_end,
        }
    }

    fn annotate(&self, row: &mut storage::ReviewEvidenceRow) {
        use sha2::{Digest, Sha256};
        row.reference_kind = Some("assistant_turn".into());
        let reference =
            serde_json::to_string(&self.evidence_context()).expect("reference serializes");
        row.id = format!("{:x}", Sha256::digest(format!("{}|{reference}", row.id)));
        row.reference_context = Some(reference);
    }
}

struct RunEntry {
    role: Role,
    name: String,
    model: String,
    config: RunConfig,
    parent: Option<RunId>,
    escalated_from: Option<RunId>,
    completion_relayed: bool,
    terminal_reason: Option<String>,
    phase_tx: watch::Sender<AgentRunPhase>,
    phase_rx: watch::Receiver<AgentRunPhase>,
    message_count_rx: watch::Receiver<usize>,
    inbox_tx: mpsc::Sender<crate::runtime::user_inbox::UserInput>,
    user_inbox: Arc<UserInbox>,
    pending_reference: Arc<Mutex<Option<PendingReference>>>,
    cancel_tx: watch::Sender<RunInterrupt>,
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
    fn review_error(run_id: RunId, reason: impl ToString) -> RuntimeError {
        RuntimeError::ReviewEvidence {
            run_id: run_id.to_string(),
            reason: reason.to_string(),
        }
    }

    /// Store a new reference and issue a fresh token while holding the same mutex.
    pub fn capture_pending_reference(
        &self,
        run_id: RunId,
        context: String,
    ) -> Result<u64, RuntimeError> {
        self.capture_pending_reference_at(run_id, context, 0)
    }

    pub(crate) fn capture_pending_reference_at(
        &self,
        run_id: RunId,
        context: String,
        turn_end: usize,
    ) -> Result<u64, RuntimeError> {
        let entry = self.entry(run_id)?;
        let mut pending = entry
            .pending_reference
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let token = self
            .shared
            .next_reference_token
            .fetch_add(1, Ordering::Relaxed);
        let created_at_ns = crate::run_store::review_time_ns();
        let reference_id = format!("assistant:{run_id}:{created_at_ns}:{token}");
        let store = self.shared.run_store.get();
        if let Some(store) = store {
            let saved = (|| {
                if let Some(previous) = pending.as_ref()
                    && store
                        .handle
                        .assistant_reference(&previous.reference_id)?
                        .is_some_and(|row| !row.consumed)
                {
                    store
                        .handle
                        .append_review_evidence_with_reference_consumption(
                            &[],
                            &[],
                            Some(&previous.reference_id),
                        )?;
                }
                store
                    .handle
                    .save_assistant_reference(&storage::AssistantReferenceRow {
                        reference_id: reference_id.clone(),
                        source_run_id: run_id.to_string(),
                        turn_end,
                        text: context.clone(),
                        consumed: false,
                        created_at_ns,
                    })
            })();
            if let Err(error) = saved {
                *pending = None;
                drop(pending);
                drop(entry);
                if let Some(review) = self
                    .shared
                    .review_runs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&run_id)
                {
                    review.mark_unsynced();
                }
                return Err(Self::review_error(run_id, error));
            }
        }
        *pending = Some(PendingReference {
            token,
            context,
            reference_id,
            source_run_id: run_id.to_string(),
            turn_end,
            storage: if store.is_some() {
                ReferenceStorage::Durable
            } else {
                ReferenceStorage::Ephemeral { created_at_ns }
            },
        });
        Ok(token)
    }

    fn resolve_pending_reference(
        &self,
        id: Option<&str>,
    ) -> Result<Option<PendingReference>, RuntimeError> {
        let (Some(id), Some(store)) = (id, self.shared.run_store.get()) else {
            return Ok(None);
        };
        let row =
            store
                .handle
                .assistant_reference(id)
                .map_err(|error| RuntimeError::ReviewEvidence {
                    run_id: id.into(),
                    reason: error.to_string(),
                })?;
        let row = row.ok_or_else(|| RuntimeError::ReviewEvidence {
            run_id: id.into(),
            reason: "assistant reference is missing".into(),
        })?;
        Ok((!row.consumed).then(|| PendingReference {
            token: self
                .shared
                .next_reference_token
                .fetch_add(1, Ordering::Relaxed),
            context: row.text,
            reference_id: row.reference_id,
            source_run_id: row.source_run_id,
            turn_end: row.turn_end,
            storage: ReferenceStorage::Durable,
        }))
    }

    /// A mismatching token leaves the newer reference unconsumed.
    pub fn take_pending_reference(
        &self,
        run_id: RunId,
        token: u64,
    ) -> Result<Option<PendingReference>, RuntimeError> {
        let entry = self.entry(run_id)?;
        let mut pending = entry
            .pending_reference
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(
            if pending
                .as_ref()
                .is_some_and(|pending| pending.token == token)
            {
                if let Some(store) = self.shared.run_store.get() {
                    let id = &pending.as_ref().expect("matched token").reference_id;
                    if store
                        .handle
                        .assistant_reference(id)
                        .map_err(|error| Self::review_error(run_id, error))?
                        .is_some_and(|row| !row.consumed)
                    {
                        store
                            .handle
                            .append_review_evidence_with_reference_consumption(&[], &[], Some(id))
                            .map_err(|error| Self::review_error(run_id, error))?;
                    }
                }
                pending.take()
            } else {
                None
            },
        )
    }

    pub fn peek_pending_reference(
        &self,
        run_id: RunId,
    ) -> Result<Option<PendingReference>, RuntimeError> {
        let entry = self.entry(run_id)?;
        let pending = entry
            .pending_reference
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(pending.clone())
    }

    /// Materialize under the map fence and reuse an existing lineage's shared Arc.
    /// Promote a runtime-only capture before referring to it from durable state.
    pub(crate) fn persist_pending_reference(
        &self,
        run_id: RunId,
    ) -> Result<Option<PendingReference>, RuntimeError> {
        let entry = self.entry(run_id)?;
        let mut pending = entry
            .pending_reference
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let (Some(reference), Some(store)) = (pending.as_mut(), self.shared.run_store.get())
            && let ReferenceStorage::Ephemeral { created_at_ns } = reference.storage
        {
            store
                .handle
                .save_assistant_reference(&storage::AssistantReferenceRow {
                    reference_id: reference.reference_id.clone(),
                    source_run_id: reference.source_run_id.clone(),
                    turn_end: reference.turn_end,
                    text: reference.context.clone(),
                    consumed: false,
                    created_at_ns,
                })
                .map_err(|error| Self::review_error(run_id, error))?;
            // A later evidence failure must not make this row eligible for recreation.
            reference.storage = ReferenceStorage::Durable;
        }
        Ok(pending.clone())
    }

    fn materialize_review_locked(
        &self,
        store: &crate::RunStore,
        run_id: RunId,
        runs: &mut HashMap<RunId, crate::escalation_review::ReviewRunContext>,
    ) -> Result<crate::escalation_review::ReviewRunContext, RuntimeError> {
        let mut review =
            crate::run_store::materialize_review_context(store, run_id).map_err(|error| {
                if let Some(run) = runs.get(&run_id) {
                    run.mark_unsynced();
                }
                Self::review_error(run_id, error)
            })?;
        if let Some(existing) = runs
            .values()
            .find(|run| run.root_run_id == review.root_run_id)
        {
            let mut lineage = review.lineage_run_ids.clone();
            for run in runs
                .values()
                .filter(|run| Arc::ptr_eq(&run.user_requests, &existing.user_requests))
            {
                lineage.extend(run.lineage_run_ids.clone());
            }
            lineage.sort();
            lineage.dedup();
            crate::run_store::resync_review_context(store, existing, &lineage)
                .map_err(|error| Self::review_error(run_id, error))?;
            review.user_requests = existing.user_requests.clone();
        }
        Ok(review)
    }

    fn record_host_input(
        &self,
        run_id: RunId,
        text: &str,
        reference: Option<&PendingReference>,
    ) -> Result<(), RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| Self::review_error(run_id, "storage is not configured"))?;
        let mut runs = self
            .shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let review = match runs.get(&run_id) {
            Some(run) if run.stamp().synced => run.clone(),
            _ => {
                let review = self.materialize_review_locked(store, run_id, &mut runs)?;
                runs.insert(run_id, review.clone());
                review
            }
        };
        let mut row = crate::run_store::evidence_row(
            &review.root_run_id,
            &run_id.to_string(),
            "host_chat",
            text,
        );
        if let Some(reference) = reference {
            row.reference_kind = Some("assistant_turn".into());
            row.reference_context = Some(
                serde_json::to_string(&reference.evidence_context())
                    .map_err(|error| Self::review_error(run_id, error))?,
            );
        }
        // Equal text responding to different assistant turns is distinct evidence.
        // Keep plain host input's key unchanged; references are non-authoritative.
        if let Some(reference) = &row.reference_context {
            use sha2::{Digest, Sha256};
            row.id = format!("{:x}", Sha256::digest(format!("{}|{reference}", row.id)));
        }
        let mut shared = review
            .user_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store
            .handle
            .append_review_evidence_with_reference_consumption(
                std::slice::from_ref(&row),
                &[],
                reference.map(|pending| pending.reference_id.as_str()),
            )
            .map_err(|error| Self::review_error(run_id, error))?;
        shared.add_request(crate::run_store::request_from_evidence(row));
        Ok(())
    }

    pub fn set_default_cwd(&self, root: PathBuf) -> Result<(), RuntimeError> {
        if let Some(current) = self
            .shared
            .sandbox_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            *current = root.clone();
        }
        self.shared
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_default_cwd(root);
        Ok(())
    }

    /// Rebind every project-scoped input to `root` for runs started afterwards:
    /// the shell sandbox and cwd, project rules and repository skills. Running
    /// runs keep the root they started with.
    ///
    /// # Errors
    /// Propagates [`AgentRuntime::set_default_cwd`] failures.
    pub fn set_project_root(&self, root: PathBuf) -> Result<(), RuntimeError> {
        self.set_default_cwd(root.clone())?;
        *self
            .shared
            .active_project_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root.clone());
        {
            let mut rules = self
                .shared
                .rules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(current) = rules.as_ref() {
                *rules = Some(Arc::new(current.with_project_root(Some(root.clone()))));
            }
        }
        if let Some(source) = self.shared.skill_source.get() {
            source.set_repo_root(&root);
        }
        Ok(())
    }

    pub fn team_tasks(&self) -> Vec<(RunId, Vec<crate::team::TeamTask>)> {
        lock_runs(&self.shared.runs)
            .iter()
            .filter_map(|(id, entry)| {
                let team = entry.config.team.as_ref()?;
                (*id == team.coordinator).then(|| (*id, team.board.snapshot().unwrap_or_default()))
            })
            .collect()
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
                thread_goals: Mutex::new(crate::thread_goals::ThreadGoals::default()),
                benchmark_recording: OnceLock::new(),
                benchmark_replays: Mutex::new(HashMap::new()),
                benchmark_executors: Mutex::new(HashMap::new()),
                question_version: watch::channel(0).0,
                lesson_staging: Mutex::new(crate::learning::LearningStaging::default()),
                admissions: Mutex::new(HashMap::new()),
                spawn_intents: Mutex::new(HashMap::new()),
                topology: OnceLock::new(),
                learning: OnceLock::new(),
                self_improvement: OnceLock::new(),
                self_improvement_task: OnceLock::new(),
                learning_runs: Mutex::new(HashMap::new()),
                bus,
                executor: Mutex::new(executor),
                artifacts: OnceLock::new(),
                artifact_pages: OnceLock::new(),
                model,
                system_prompts: OnceLock::new(),
                skills: OnceLock::new(),
                skill_source: OnceLock::new(),
                rules: Mutex::new(None),
                compaction: OnceLock::new(),
                budget: OnceLock::new(),
                run_store: OnceLock::new(),
                model_resolution: OnceLock::new(),
                comment_checker: OnceLock::new(),
                orchestration: OnceLock::new(),
                web_tools_enabled: AtomicBool::new(true),
                sandbox_escalation: Arc::new(Mutex::new((config::EscalationApproval::Auto, false))),
                sandbox_root: Mutex::new(None),
                thread_scratch: Mutex::new(None),
                compaction_configured: AtomicBool::new(false),
                escalation_settings: OnceLock::new(),
                escalations: Mutex::new(HashMap::new()),
                review_runs: Mutex::new(HashMap::new()),
                sandbox_factory: None,
                workspaces: Mutex::new(HashMap::new()),
                run_ids: crate::run_ids::RunIds::default(),
                active_project_root: Mutex::new(None),
                projects: Mutex::new(HashMap::new()),
                project_trust: Mutex::new(HashMap::new()),
                allowed_directories: Mutex::new(HashMap::new()),
                project_models: OnceLock::new(),
                project_slugs: OnceLock::new(),
                next_message_id: AtomicU64::new(1),
                next_reference_token: AtomicU64::new(1),
                runs: Mutex::new(HashMap::new()),
                sent: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// 新規 run に `run-1` からの連番 ID を割り当てる。
    ///
    /// スクリプト化したテストやデモで ID を固定する用途。run の採番や
    /// [`AgentRuntime::with_run_store`] より前に呼ぶ必要があり、後から呼んだ場合は無視する。
    #[must_use]
    pub fn with_sequential_run_ids(self) -> Self {
        let _ = self.shared.run_ids.use_sequential();
        self
    }

    /// 終端保存先を接続する。設定済みの場合は先勝ちで変更しない。
    pub fn with_run_store(self, store: crate::RunStore) -> Self {
        if let Some(latest) = store.latest_run_id {
            self.shared.run_ids.observe(latest);
        }
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

    pub(crate) fn with_model_resolution(
        self,
        config: &config::Config,
        credential_store: Option<Arc<dyn sandbox::CredentialStore>>,
    ) -> Self {
        let _ = self
            .shared
            .comment_checker
            .set(config.comment_checker.clone());
        let _ = self.shared.orchestration.set(config.orchestration.clone());
        let _ = self.shared.budget.set((&config.budget).into());
        self.set_web_tools_enabled(config.sandbox.web_tools_enabled);
        self.set_sandbox_escalation(
            config.sandbox.escalation_approval,
            config.sandbox.escalate_to_user_on_deny,
        );
        let _ = self
            .shared
            .topology
            .set(crate::CoordinationTopology::from_config(&config.team));
        let _ = self
            .shared
            .model_resolution
            .set(crate::model_resolve::ModelResolution::new(
                config,
                credential_store,
            ));
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

    /// run 境界で再読み込みする skill/catalog 供給元を接続する (先勝ち)。
    ///
    /// 既存の `with_skills` / `with_system_prompts` とは独立して設定でき、
    /// 両方が接続されている場合はこの供給元が優先する。
    pub fn with_skill_source(self, source: Arc<SkillCatalogSource>) -> Self {
        let _ = self.shared.skill_source.set(source);
        self
    }

    /// プロジェクトルール読み込み元を設定したランタイムを返すビルダーメソッド。
    ///
    /// 設定済みの場合は 2 回目以降の呼び出しを無視する先勝ち契約で、実際のルール
    /// 注入は agent loop 統合を行う後続 Wave が担う。
    pub fn with_project_rules(self, rules: Arc<RulesSource>) -> Self {
        self.shared
            .rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(rules);
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

    /// production sandbox と run ごとの sandbox factory を持つランタイムを生成する。
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
            production_executor(Arc::clone(&bus), policy, seam.project_root().to_path_buf())?;
        Ok(Self::with_sandbox_factory(
            bus,
            executor,
            model,
            seam.into_factory(),
        ))
    }

    /// run ごとの sandbox を明示的な factory で構築するランタイムを生成する。
    ///
    /// production は [`AgentRuntime::production_with_project`] を使用する。隔離なし sandbox
    /// は既存の [`AgentRuntime::new`] とこの明示的 seam のテスト実装でのみ許可する。
    pub fn with_sandbox_factory(
        bus: Arc<EventBus>,
        executor: Arc<ToolExecutor>,
        model: Arc<dyn AgentModel>,
        factory: Arc<dyn SandboxFactory>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                thread_goals: Mutex::new(crate::thread_goals::ThreadGoals::default()),
                benchmark_recording: OnceLock::new(),
                benchmark_replays: Mutex::new(HashMap::new()),
                benchmark_executors: Mutex::new(HashMap::new()),
                question_version: watch::channel(0).0,
                admissions: Mutex::new(HashMap::new()),
                spawn_intents: Mutex::new(HashMap::new()),
                bus,
                executor: Mutex::new(executor),
                model,
                topology: OnceLock::new(),
                system_prompts: OnceLock::new(),
                skills: OnceLock::new(),
                skill_source: OnceLock::new(),
                rules: Mutex::new(None),
                compaction: OnceLock::new(),
                budget: OnceLock::new(),
                run_store: OnceLock::new(),
                model_resolution: OnceLock::new(),
                comment_checker: OnceLock::new(),
                orchestration: OnceLock::new(),
                web_tools_enabled: AtomicBool::new(true),
                sandbox_escalation: Arc::new(Mutex::new((config::EscalationApproval::Auto, false))),
                sandbox_root: Mutex::new(None),
                thread_scratch: Mutex::new(None),
                compaction_configured: AtomicBool::new(false),
                escalation_settings: OnceLock::new(),
                escalations: Mutex::new(HashMap::new()),
                review_runs: Mutex::new(HashMap::new()),
                lesson_staging: Mutex::new(crate::learning::LearningStaging::default()),
                sandbox_factory: Some(factory),
                learning: OnceLock::new(),
                self_improvement: OnceLock::new(),
                self_improvement_task: OnceLock::new(),
                learning_runs: Mutex::new(HashMap::new()),
                artifacts: OnceLock::new(),
                artifact_pages: OnceLock::new(),
                workspaces: Mutex::new(HashMap::new()),
                run_ids: crate::run_ids::RunIds::default(),
                active_project_root: Mutex::new(None),
                projects: Mutex::new(HashMap::new()),
                project_trust: Mutex::new(HashMap::new()),
                allowed_directories: Mutex::new(HashMap::new()),
                project_models: OnceLock::new(),
                project_slugs: OnceLock::new(),
                next_message_id: AtomicU64::new(1),
                next_reference_token: AtomicU64::new(1),
                runs: Mutex::new(HashMap::new()),
                sent: Mutex::new(HashMap::new()),
            }),
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
    /// child 起動前に run ID を確定させ、イベント発行との happens-before を
    /// 組む用途 (issue #83)。採番後は
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
        self.shared.run_ids.next()
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
        self.spawn_run_with_handoff(
            run_id,
            parent,
            role,
            prompt.into(),
            config,
            RunContinuation::Fresh,
            None,
        )
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
        let run_id = self.shared.run_ids.next();
        self.spawn_run_with_handoff(
            run_id,
            parent,
            role,
            prompt,
            config,
            RunContinuation::Fresh,
            None,
        )
    }

    // Keep host provenance explicit at this installation boundary.
    #[allow(clippy::too_many_arguments)]
    fn spawn_run_with_handoff(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        mut config: RunConfig,
        continuation: RunContinuation,
        review_seed: Option<ReviewSeed>,
    ) -> RunId {
        let escalated_from = match &continuation {
            RunContinuation::Handoff(handoff) => Some(handoff.source_run_id),
            RunContinuation::Resume {
                handoff: Some(handoff),
                ..
            } => Some(handoff.source_run_id),
            _ => None,
        };
        if let Some(store) = self.shared.run_store.get() {
            let edges = parent
                .into_iter()
                .chain(escalated_from)
                .filter(|id| *id != run_id)
                .map(|source| storage::LineageEdge {
                    child_run_id: run_id.to_string(),
                    parent_run_id: source.to_string(),
                    created_at_ns: crate::run_store::review_time_ns(),
                })
                .collect::<Vec<_>>();
            if !edges.is_empty()
                && let Err(error) = store.handle.record_review_lineage_edge(&edges)
            {
                self.fail_review_admission(run_id, parent, &config, error.to_string());
                return run_id;
            }
        }
        self.shared.bind_project(parent, &mut config);
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // An explicitly validated conversation restore starts a new generation of
        // this root. Keep old descendants fenced, but preserve discard-then-resume
        // in place; fresh/in-flight spawns must still inherit cancellation.
        if parent.is_none()
            && (matches!(
                &continuation,
                RunContinuation::Restored(_) | RunContinuation::Resume { .. }
            ) || matches!(&continuation, RunContinuation::Handoff(handoff) if handoff.restored.is_some()))
            && let Some(intent) = intents.get_mut(&run_id)
        {
            intent.cancelled = false;
        }
        let cancelled = intents.get(&run_id).is_some_and(|intent| intent.cancelled)
            || parent
                .and_then(|parent| intents.get(&parent))
                .is_some_and(|intent| intent.cancelled);
        let owner = intents
            .get(&run_id)
            .and_then(|intent| intent.parent)
            .or(parent);
        intents.insert(
            run_id,
            cancellation::SpawnIntent {
                parent: owner,
                cancelled,
            },
        );
        let run = if self.shared.model_for(&config).requires_admission() {
            self.admit_run(
                run_id,
                parent,
                role,
                prompt,
                config,
                continuation,
                review_seed,
            )
        } else {
            self.register_without_admission(
                run_id,
                parent,
                role,
                prompt,
                config,
                continuation,
                review_seed,
            )
        };
        if cancelled {
            let _ = self.cancel(run);
        }
        run
    }

    // Keep host provenance explicit at this installation boundary.
    #[allow(clippy::too_many_arguments)]
    fn register_run(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        mut config: RunConfig,
        continuation: RunContinuation,
        review_seed: Option<ReviewSeed>,
    ) -> RunId {
        let completion_relayed = matches!(continuation, RunContinuation::Awaited);
        let history_only = matches!(continuation, RunContinuation::Resume { .. });
        let original_prompt = prompt.clone();
        let (handoff, restored) = match continuation {
            RunContinuation::Fresh => (None, None),
            RunContinuation::Awaited => (None, None),
            RunContinuation::Handoff(mut handoff) => {
                let restored = handoff.restored.take();
                (Some(handoff), restored)
            }
            RunContinuation::Restored(restored) => (None, Some(restored)),
            RunContinuation::Resume { restored, handoff } => (handoff, Some(*restored)),
        };
        let ancestry = crate::request_turn::RequestAncestry::new(
            parent,
            parent.is_some_and(|parent| {
                lock_runs(&self.shared.runs)
                    .get(&parent)
                    .is_some_and(|entry| entry.parent.is_some())
            }),
        );
        let turn = crate::request_turn::initial(
            ancestry,
            !history_only && review_seed.is_some(),
            restored.is_none() && handoff.is_none(),
            history_only,
            restored.as_ref().and_then(|state| state.turn.as_ref()),
        );
        if config.budget == crate::budget_tracker::BudgetSettings::default()
            && let Some(budget) = self.shared.budget.get()
        {
            config.budget = budget.clone();
        }
        self.shared.bind_project(parent, &mut config);
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
        self.install_run_ownership_guard(run_id, &mut config);
        let escalated_from = handoff.as_ref().map(|handoff| handoff.source_run_id);
        if parent.is_some() && escalated_from.is_some() {
            tracing::debug!(%run_id, ?parent, ?escalated_from,
                "review context has two live parents; parent takes precedence until rematerialization");
        }
        let inherited_review = parent.or(escalated_from).and_then(|source| {
            self.shared
                .review_runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&source)
                .cloned()
        });
        // Installation only: all durable preparation belongs to the Result entry points.
        let review_run = if let Some(seed) = review_seed {
            match seed.provenance {
                HostProvenance::FreshHostSubmission => {}
                HostProvenance::PersistedHostSubmission { run_id: source } => {
                    debug_assert!(seed.context.lineage_run_ids.contains(&source.to_string()));
                }
            }
            // chat_review_context already registered this same Arc before any
            // admission await, so answers received while waiting remain visible.
            seed.context
        } else if let Some(mut inherited) = inherited_review {
            inherited.lineage_run_ids.push(run_id.to_string());
            if !history_only {
                inherited
                    .delegation_chain
                    .push(crate::escalation_review::bounded(&original_prompt, 1500));
            }
            if inherited.delegation_chain.len() > 4 {
                inherited.delegation_chain.remove(0);
            }
            inherited
        } else {
            crate::escalation_review::ReviewRunContext::empty(&run_id.to_string())
        };
        self.shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run_id, review_run);
        let prompt = match &config.memory {
            Some(memory) if handoff.is_none() && !history_only => memory.augment(prompt),
            Some(_) | None => prompt,
        };
        let prompt = match (&config.team, &config.team_task) {
            (Some(_), Some(task)) if !history_only => format!(
                "{prompt}\nTeam task id: {:?}. Claim it with task_claim before editing; pass its generation to task_complete. Owned paths: {:?}",
                task.id, task.paths
            ),
            _ => prompt,
        };
        let name = config
            .name
            .clone()
            .unwrap_or_else(|| role.name().to_string());
        let model = self
            .shared
            .model_for(&config)
            .selected_model(role, config.category.as_deref());
        let (phase_tx, phase_rx) = watch::channel(AgentRunPhase::Pending);
        let (message_count_tx, message_count_rx) = watch::channel(0);
        let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
        let user_inbox = Arc::new(UserInbox::default());
        let (cancel_tx, cancel_rx) = watch::channel(RunInterrupt::None);
        let (compact_tx, compact_rx) = watch::channel(0_u64);
        let (model_preference_tx, model_preference_rx) =
            watch::channel(config.model_preference.clone());
        let (result_tx, result_rx) = watch::channel(None::<String>);
        let compaction_busy = Arc::new(AtomicBool::new(false));
        let phase_tx_entry = phase_tx.clone();
        let mailbox = Arc::new(RunMailbox::new());
        let mailbox_version_rx = mailbox.subscribe_version();
        let pending_reference_id = restored
            .as_ref()
            .and_then(|state| state.pending_reference_id.as_deref())
            .or_else(|| {
                handoff
                    .as_ref()
                    .and_then(|handoff| handoff.pending_reference_id.as_deref())
            });
        let pending_reference = match self.resolve_pending_reference(pending_reference_id) {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%run_id, %error, "assistant reference restore failed");
                None
            }
        };
        let task = RunTask {
            ancestry,
            turn,
            run_id,
            role,
            prompt,
            start: if history_only {
                crate::agent_loop::RunStart::Resume
            } else {
                crate::agent_loop::RunStart::WithInput
            },
            config: config.clone(),
            parent,
            mailbox: Arc::clone(&mailbox),
            handoff,
            restored: None,
        };
        let task = match restored {
            Some(restored) => restored.attach_to(task),
            None => task,
        };
        let channels = LoopChannels {
            phase_tx,
            message_count_tx,
            inbox_rx,
            user_inbox: Arc::clone(&user_inbox),
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
                terminal_reason: None,
                role,
                name: name.clone(),
                model,
                config: config.clone(),
                parent,
                escalated_from,
                completion_relayed,
                phase_tx: phase_tx_entry,
                phase_rx,
                message_count_rx,
                inbox_tx,
                user_inbox,
                pending_reference: Arc::new(Mutex::new(pending_reference)),
                cancel_tx,
                compact_tx,
                model_preference_tx,
                result_rx,
                compaction_busy: Arc::clone(&compaction_busy),
                mailbox: Arc::clone(&mailbox),
                _join: None,
            },
        );
        if task.restored.is_some() {
            self.shared
                .bus
                .emit(Event::new(LifecycleEvent::AgentRunRestored {
                    run_id: run_id.to_string(),
                    restored_by: config
                        .ownership
                        .as_ref()
                        .map_or_else(|| "user".into(), |permit| permit.lease.owner_id.clone()),
                    message_id: format!(
                        "msg-{}",
                        self.shared.next_message_id.fetch_add(1, Ordering::Relaxed)
                    ),
                }));
        }
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::AgentRunStarted {
                run_id: run_id.to_string(),
                parent_run_id: parent.map(|parent| parent.to_string()),
                agent_name: name.clone(),
                role: role.name().to_lowercase(),
            }));
        // 復元時は保存済みの指示イベントを再生するため、ここでは重複発行しない。
        if task.restored.is_none() {
            self.shared
                .bus
                .emit(Event::new(LifecycleEvent::TaskPromptPublished {
                    run_id: run_id.to_string(),
                    parent_run_id: parent.map(|parent| parent.to_string()),
                    agent_name: name,
                    role: role.name().to_lowercase(),
                    prompt: task.prompt.clone(),
                }));
        }
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
                        AgentRuntime { shared }.publish_terminal(
                            task.run_id,
                            LifecycleEvent::AgentRunStateChanged {
                                run_id: task.run_id.to_string(),
                                from: AgentRunPhase::Pending,
                                to: AgentRunPhase::Error,
                                reason: Some(reason),
                            },
                        );
                    }
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
            match crate::panic_capture::catch_panic(run_agent(weak.clone(), task, channels)).await {
                Ok(()) => {
                    if let Some(learning) = learning {
                        learning.complete(weak, run_id).await;
                    }
                }
                // No lessons from a run whose loop state is unknown.
                Err(panic) => {
                    if let Some(shared) = weak.upgrade() {
                        AgentRuntime { shared }.run_panicked(run_id, panic).await;
                    }
                }
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
        continuity: crate::continuity::Continuity,
        before_spawn: impl FnOnce(),
    ) -> Result<RunId, String> {
        let carries_thread_goal = self.goal_for_root(memo.source_run_id).is_some();
        let conversation = source_config.conversation
            && source_config.purpose == crate::RunPurpose::General
            && self.goal_thread(memo.source_run_id).is_some();
        let mut config = RunConfig {
            budget: source_config.budget.clone(),
            conversation,
            interactive: (carries_thread_goal || conversation) && source_config.interactive,
            keep_alive: (carries_thread_goal || conversation) && source_config.keep_alive,
            name: Some("escalation-orchestrator".to_string()),
            category: None,
            load_skills: Vec::new(),
            project_root: source_config.project_root.clone(),
            ..RunConfig::default()
        };
        let source_run_id = memo.source_run_id;
        let run_id = self.shared.run_ids.next();
        // Escalation starts from the memo, not the source conversation. Every
        // source question/answer therefore still needs delivery to this root.
        // Persist the explicit recipient before any provider admission or spawn.
        self.inherit_user_questions(source_run_id, run_id, &[])?;
        let questions = self
            .shared
            .run_store
            .get()
            .map(|store| store.user_questions(run_id))
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        // Publish durable routing without changing the ID or requester provenance.
        for question in &questions {
            self.shared
                .bus
                .emit(Event::new(event_bus::ToolEvent::UserQuestionUpdated {
                    question: question.clone(),
                }));
        }
        let summary = memo.summary();
        let prompt = crate::escalation::prompt::render_escalation_prompt(&memo, &questions);
        let pending_reference_id = self
            .persist_pending_reference(source_run_id)
            .map_err(|error| error.to_string())?
            .map(|pending| pending.reference_id);
        crate::restore::persist_escalation_seed(
            self,
            run_id,
            source_run_id,
            &prompt,
            &config,
            source_config.ownership.is_some(),
            &continuity,
            pending_reference_id.clone(),
        )
        .map_err(|error| error.to_string())?;
        config.ownership = self.handoff_thread_goal(
            source_run_id,
            run_id,
            source_config.ownership.as_ref(),
            |owner| {
                self.reserve_admission(run_id, None, owner.clone());
                before_spawn();
                self.shared
                    .bus
                    .emit(Event::new(LifecycleEvent::EscalationRequested {
                        source_run_id: source_run_id.to_string(),
                        new_run_id: run_id.to_string(),
                        summary,
                    }));
            },
        )?;
        Ok(self.spawn_run_with_handoff(
            run_id,
            None,
            Role::Orchestrator,
            prompt,
            config,
            RunContinuation::Handoff(RunHandoff {
                continuity,
                pending_reference_id,
                source_run_id,
                restored: None,
            }),
            None,
        ))
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
        self.send_inbox_message(run_id, text, images, true, None)
    }

    pub(crate) fn send_internal_message(
        &self,
        run_id: RunId,
        text: String,
    ) -> Result<(), RuntimeError> {
        self.send_inbox_message(run_id, text, Vec::new(), false, None)
    }

    fn send_inbox_message(
        &self,
        run_id: RunId,
        text: String,
        images: Vec<crate::DelegateImage>,
        trusted_user: bool,
        initial_goal: Option<(String, Vec<String>)>,
    ) -> Result<(), RuntimeError> {
        self.validate_run_mutation(run_id)?;
        let phase = *self.entry(run_id)?.phase_rx.borrow();
        if matches!(
            phase,
            AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error
        ) {
            return Err(RuntimeError::RunTerminated {
                run_id: run_id.to_string(),
            });
        }
        let (sender, inbox) = {
            let entry = self.entry(run_id)?;
            (entry.inbox_tx.clone(), Arc::clone(&entry.user_inbox))
        };
        // Serialize the synchronous send API per inbox through persistence,
        // review reflection and enqueue. The send mutex is separate from inbox
        // observation to avoid a run-map -> status / send -> run-map lock cycle.
        // Nothing in this closure re-enters the trusted-send serializer.
        let send = || {
            let permit = sender
                .try_reserve()
                .map_err(|_| RuntimeError::RunTerminated {
                    run_id: run_id.to_string(),
                })?;
            if trusted_user {
                let pending = self.persist_pending_reference(run_id)?;
                self.record_host_input(run_id, &text, pending.as_ref())?;
                if let Some(pending) = pending {
                    match self.take_pending_reference(run_id, pending.token) {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            tracing::debug!(%run_id, "newer assistant reference remains pending")
                        }
                        Err(error) => {
                            tracing::warn!(%run_id, %error, "assistant reference take failed")
                        }
                    }
                }
            }
            if let Some((objective, criteria)) = initial_goal {
                let thread =
                    self.goal_thread(run_id)
                        .ok_or_else(|| RuntimeError::RunRestoreFailed {
                            run_id: run_id.to_string(),
                            reason: crate::RunRestoreFailure::CorruptContext(
                                "missing thread binding".into(),
                            ),
                        })?;
                self.create_thread_goal(&thread, run_id, objective, criteria)
                    .map_err(|reason| RuntimeError::RunRestoreFailed {
                        run_id: run_id.to_string(),
                        reason: crate::RunRestoreFailure::CorruptContext(reason),
                    })?;
            }
            // Invalidate the old check before the model can consume new host input.
            if trusted_user {
                self.goal_user_input(run_id, &text);
            }
            let enqueue = || {
                let prompt_id = trusted_user
                    .then(|| providers::RequestTurnContext::unattributed(false).prompt_id);
                permit.send((text.clone(), images, trusted_user, prompt_id));
                Ok::<_, RuntimeError>(())
            };
            if trusted_user {
                inbox.enqueue(enqueue)?;
            } else {
                enqueue()?;
            }
            Ok::<_, RuntimeError>(())
        };
        if trusted_user {
            inbox.serialize_trusted_send(send)?;
        } else {
            send()?;
        }
        Ok(())
    }

    /// Observe accepted follow-ups that have not entered the agent's context.
    pub fn follow_up_status(&self, run_id: RunId) -> Result<FollowUpStatus, RuntimeError> {
        let entry = self.entry(run_id)?;
        let mut status = entry.user_inbox.status();
        status.closed = entry.inbox_tx.is_closed();
        Ok(status)
    }

    /// Deliver queued follow-ups before the next model request, without cancelling
    /// an in-flight response or tool. This does not send a new message.
    pub fn deliver_follow_ups_next_turn(&self, run_id: RunId) -> Result<(), RuntimeError> {
        self.validate_run_mutation(run_id)?;
        let entry = self.entry(run_id)?;
        if entry.inbox_tx.is_closed() {
            return Err(RuntimeError::RunTerminated {
                run_id: run_id.to_string(),
            });
        }
        entry.user_inbox.request_next_turn();
        Ok(())
    }

    /// run を停止する。停止後は復元により再開できる。
    ///
    /// admission 待機中の run は未登録のため、停止の代わりに admission をキャンセルする。
    ///
    /// # Errors
    /// run_id が存在しない場合 [`RuntimeError::UnknownRun`] を返す。
    pub fn stop(&self, run_id: RunId, scope: StopScope) -> Result<(), RuntimeError> {
        self.goal_work_stopped(run_id);
        // Collect before taking admissions: spawning locks intents before admissions.
        let targets = match scope {
            StopScope::SelfOnly => HashSet::from([run_id]),
            StopScope::Subtree => self.run_subtree_ids(run_id),
        };
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Keep the same admissions -> runs fence as cancel and registration.
        let runs = lock_runs(&self.shared.runs);
        if !runs.contains_key(&run_id)
            && !admissions
                .get(&run_id)
                .is_some_and(|admission| admission.result.borrow().is_none())
        {
            return Err(unknown_run(run_id));
        }
        for target in targets {
            if let Some(admission) = admissions.get_mut(&target)
                && admission.result.borrow().is_none()
            {
                admission.cancelled = true;
            } else if let Some(entry) = runs.get(&target) {
                entry.cancel_tx.send_replace(RunInterrupt::Stop);
            }
        }
        Ok(())
    }

    /// Hard-cancel a root and all descendants, fencing even in-flight child spawns.
    /// Unlike [`Self::stop`], cancellation is inherited by future descendants.
    ///
    /// # Errors
    /// Returns [`RuntimeError::UnknownRun`] if the root has no run or spawn intent.
    pub fn cancel_subtree(&self, run_id: RunId) -> Result<(), RuntimeError> {
        // Keep intents locked from collection through cancellation. Spawning uses
        // the same intents -> admissions -> runs order and inherits this fence.
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        if !intents.contains_key(&run_id)
            && !admissions.contains_key(&run_id)
            && !runs.contains_key(&run_id)
        {
            return Err(unknown_run(run_id));
        }
        for target in Self::collect_subtree_ids(run_id, &intents, &runs) {
            intents.entry(target).or_default().cancelled = true;
            if let Some(admission) = admissions.get_mut(&target)
                && admission.result.borrow().is_none()
            {
                admission.cancelled = true;
            } else if let Some(entry) = runs.get(&target) {
                entry.cancel_tx.send_replace(RunInterrupt::Cancel);
            }
        }
        Ok(())
    }

    /// 指定された run 自身を除き、実行中または admission 待機中の全子孫を ID 順で返す。
    pub fn live_descendants(&self, run_id: RunId) -> Vec<RunId> {
        let descendants = self.run_subtree_ids(run_id);
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        let mut live: Vec<_> = descendants
            .into_iter()
            .filter(|id| {
                *id != run_id
                    && (admissions
                        .get(id)
                        .is_some_and(|admission| admission.result.borrow().is_none())
                        || runs.get(id).is_some_and(|entry| {
                            matches!(
                                *entry.phase_rx.borrow(),
                                AgentRunPhase::Pending
                                    | AgentRunPhase::Running
                                    | AgentRunPhase::Waiting
                            )
                        }))
            })
            .collect();
        live.sort();
        live
    }

    /// Follow both pre-registration intents and registered parents to a fixed point.
    fn run_subtree_ids(&self, run_id: RunId) -> HashSet<RunId> {
        let intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        Self::collect_subtree_ids(run_id, &intents, &runs)
    }

    fn collect_subtree_ids(
        run_id: RunId,
        intents: &HashMap<RunId, cancellation::SpawnIntent>,
        runs: &HashMap<RunId, RunEntry>,
    ) -> HashSet<RunId> {
        let mut descendants = HashSet::from([run_id]);
        loop {
            let before = descendants.len();
            for (id, parent) in intents
                .iter()
                .map(|(id, intent)| (id, intent.parent))
                .chain(runs.iter().map(|(id, entry)| (id, entry.parent)))
            {
                if parent.is_some_and(|parent| descendants.contains(&parent)) {
                    descendants.insert(*id);
                }
            }
            if descendants.len() == before {
                return descendants;
            }
        }
    }

    /// run が終端位相になるまで待機し、最終位相を返す。
    pub async fn wait(&self, run_id: RunId) -> Result<AgentRunPhase, RuntimeError> {
        self.wait_admission(run_id).await?;
        let mut phase_rx = self.entry(run_id)?.phase_rx.clone();
        loop {
            let phase = *phase_rx.borrow_and_update();
            match phase {
                AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error => {
                    // Terminal publication holds runs until its completion relay finishes.
                    drop(self.entry(run_id)?);
                    return Ok(phase);
                }
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
    /// Error / cancel 終端の run は `None` のままである。
    ///
    /// # Errors
    /// run_id が存在しない場合 [`RuntimeError::UnknownRun`] を返す。
    pub fn run_result(&self, run_id: RunId) -> Result<Option<String>, RuntimeError> {
        let entry = self.entry(run_id)?;
        Ok(entry.result_rx.borrow().clone())
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
        self.wait_admission(run_id).await?;
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
                parent_run_id: entry.parent,
                name: entry.name.clone(),
                role_name: entry.role.name().to_string(),
                phase: *entry.phase_rx.borrow(),
                model: entry.model.clone(),
                category: entry.config.category.clone(),
            })
            .collect();
        summaries.sort_by_key(|summary| summary.run_id);
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
            .unwrap_or(WorkspaceInspection { active_root: None });
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

    fn install_run_ownership_guard(&self, run_id: RunId, config: &mut RunConfig) {
        if let Some(permit) = &mut config.ownership {
            permit.run_id = Some(run_id.to_string());
            let token = permit.clone();
            let nonblocking_token = permit.clone();
            self.shared.bus.register_nonblocking_mutation_guard(
                run_id.to_string(),
                Arc::new(move || {
                    token
                        .mutation_guard()
                        .ok()
                        .map(|guard| Box::new(guard) as Box<dyn event_bus::MutationGuard>)
                }),
                Arc::new(move || match nonblocking_token.try_mutation_guard() {
                    Ok(Some(guard)) => event_bus::MutationGuardAttempt::Acquired(Box::new(guard)),
                    Ok(None) => event_bus::MutationGuardAttempt::Busy,
                    Err(_) => event_bus::MutationGuardAttempt::Rejected,
                }),
            );
        }
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
        if matches!(
            phase,
            AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error
        ) {
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
            AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error => unreachable!(),
        };

        Ok((message_id, message, disposition))
    }

    /// run 自身の待機で mailbox を非消費のまま監視するための参照を返す。
    pub(crate) fn run_mailbox(&self, run_id: RunId) -> Result<Arc<RunMailbox>, RuntimeError> {
        Ok(Arc::clone(&self.entry(run_id)?.mailbox))
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
            if matches!(
                current_replier_phase,
                AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error
            ) {
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
    production_executor_with_config(bus, policy, workspace_root, &config::Config::default())
}

pub fn production_executor_with_config(
    bus: Arc<EventBus>,
    policy: &ExecutionPolicy,
    workspace_root: PathBuf,
    config: &config::Config,
) -> Result<Arc<ToolExecutor>, RuntimeError> {
    let sandbox_error = |error: sandbox::SandboxError| RuntimeError::Sandbox {
        detail: error.to_string(),
    };
    let sandbox =
        crate::network::build_sandbox(policy, workspace_root.clone()).map_err(sandbox_error)?;
    let boundary =
        crate::network::sandbox_boundary(&WorkspaceMounts::shared(workspace_root.clone()))
            .map_err(sandbox_error)?;
    configured_executor(
        bus,
        sandbox,
        &boundary,
        &config.comment_checker,
        &[workspace_root],
        sandbox::ApprovalPolicy::allow_all(),
        None,
    )
}

/// Production executor composition, also used after a run builds its own sandbox.
/// File tools are confined to `boundary`, the list the sandbox was built from.
/// A run's executor reuses `parent`'s agent browser, so one Chromium serves the process.
pub(crate) fn configured_executor(
    bus: Arc<EventBus>,
    sandbox: Arc<dyn sandbox::Sandbox>,
    boundary: &sandbox::SandboxBoundary,
    checker_config: &config::CommentCheckerConfig,
    forbidden_roots: &[PathBuf],
    approval_policy: sandbox::ApprovalPolicy,
    parent: Option<&ToolExecutor>,
) -> Result<Arc<ToolExecutor>, RuntimeError> {
    let hook = tools::post_edit::CommentChecker::resolve_with_roots(
        Arc::clone(&sandbox),
        checker_config,
        forbidden_roots,
    )
    .map(|checker| Arc::new(checker) as Arc<dyn tools::post_edit::PostEditHook>);
    let executor = ToolExecutor::with_standard_tools_in(
        bus,
        sandbox,
        Some(boundary.workspace_root().to_path_buf()),
    )
    .with_post_edit_hook(hook)
    .with_policy(approval_policy)
    .with_web_tools()
    .and_then(
        |executor| match parent.and_then(ToolExecutor::agent_browser) {
            Some(browser) => Ok(executor.with_agent_browser(browser)),
            None => executor.with_browser_tools(),
        },
    )
    .map_err(|error| RuntimeError::NetworkGuard {
        detail: error.to_string(),
    })?;
    executor.set_file_boundary(boundary);
    Ok(Arc::new(executor))
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

/// Snapshot of runtime inputs for one run; `project` scopes rules, skills and model.
pub(crate) fn loop_shared(
    shared: &Weak<Shared>,
    project: Option<&project::ProjectContext>,
) -> Option<LoopShared> {
    shared.upgrade().map(|shared| {
        let (system_prompts, skills) = match shared.skill_source.get() {
            Some(source) => {
                let snapshot = source.snapshot_for(
                    project.map(|project| project.root.as_path()),
                    project.is_none_or(|project| project.repo_skills),
                );
                (snapshot.catalog, Some(snapshot.registry))
            }
            None => (
                shared.system_prompts.get().cloned(),
                shared.skills.get().cloned(),
            ),
        };
        LoopShared {
            bus: Arc::clone(&shared.bus),
            executor: Arc::clone(
                &shared
                    .executor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ),
            model: project.map_or_else(
                || Arc::clone(&shared.model),
                |project| Arc::clone(&project.model),
            ),
            system_prompts,
            skills,
            rules: match project {
                Some(project) => project.rules.clone(),
                None => shared.rules(),
            },
            compaction: shared.compaction.get().cloned().unwrap_or_default(),
            compaction_configured: shared.compaction_configured.load(Ordering::Acquire),
            escalation: shared
                .escalation_settings
                .get()
                .copied()
                .unwrap_or_default(),
            runtime: Arc::downgrade(&shared),
        }
    })
}

#[cfg(test)]
mod review_provenance_tests {
    use super::*;

    struct HangingModel;

    #[async_trait::async_trait]
    impl AgentModel for HangingModel {
        fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
            "hanging-test-model".into()
        }

        async fn complete(
            &self,
            _invocation: &crate::AgentInvocationContext,
            _role: Role,
            _messages: &[providers::Message],
            _tools: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            std::future::pending().await
        }
    }

    fn bare_runtime() -> AgentRuntime {
        let bus = Arc::new(EventBus::new(32));
        AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(HangingModel),
        )
    }

    #[tokio::test]
    async fn ram_only_chat_rejects_secrets_and_oversized_host_input_before_reflection() {
        let runtime = bare_runtime();
        for text in [format!("sk-{}", "a".repeat(32)), "x".repeat(256 * 1024 + 1)] {
            assert!(matches!(
                runtime.delegate_chat("invalid", Role::Worker, text, RunConfig::default(),),
                Err(RuntimeError::ReviewEvidence { .. })
            ));
            assert!(runtime.shared.review_runs.lock().unwrap().is_empty());
            assert!(lock_runs(&runtime.shared.runs).is_empty());
        }
        let run = runtime
            .delegate_chat(
                "valid",
                Role::Worker,
                "x".repeat(256 * 1024),
                RunConfig::default(),
            )
            .unwrap();
        assert_eq!(
            runtime.shared.review_runs.lock().unwrap()[&run]
                .requests()
                .0[0]
                .text
                .len(),
            256 * 1024
        );
        runtime.cancel(run).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_trusted_send_persistence_reflection_and_delivery_have_the_same_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("ordered.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime = bare_runtime()
            .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime
            .delegate_chat(
                "ordered",
                Role::Worker,
                "initial".into(),
                RunConfig::default(),
            )
            .unwrap();
        // Observe actual channel delivery independently of model scheduling.
        let (sender, mut receiver) = mpsc::channel(INBOX_CAPACITY);
        lock_runs(&runtime.shared.runs)
            .get_mut(&run)
            .unwrap()
            .inbox_tx = sender;
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let sends = (0..16)
            .map(|i| {
                let runtime = runtime.clone();
                let barrier = barrier.clone();
                tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    runtime.send_message(run, format!("followup-{i}")).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for send in sends {
            send.await.unwrap();
        }
        let mut delivered = Vec::new();
        for _ in 0..16 {
            let (text, images, trusted, prompt_id) = receiver.recv().await.unwrap();
            assert!(trusted && images.is_empty());
            assert!(prompt_id.is_some());
            delivered.push(text);
        }
        let persisted = runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .review_evidence_for_lineage(&[run.to_string()])
            .unwrap()
            .into_iter()
            .filter(|row| row.kind == "host_chat")
            .map(|row| row.text)
            .collect::<Vec<_>>();
        let reflected = runtime.shared.review_runs.lock().unwrap()[&run]
            .requests()
            .0
            .iter()
            .skip(1)
            .map(|request| request.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(persisted, delivered);
        assert_eq!(reflected, delivered);
        assert_eq!(runtime.follow_up_status(run).unwrap().pending, 16);
        runtime.cancel(run).unwrap();
    }

    #[tokio::test]
    async fn equal_replies_to_distinct_references_survive_rematerialization() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("references.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime = bare_runtime()
            .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime
            .delegate_chat(
                "references",
                Role::Worker,
                "initial".into(),
                RunConfig::default(),
            )
            .unwrap();
        for reference in ["First assistant turn", "Second assistant turn"] {
            runtime
                .capture_pending_reference(run, reference.into())
                .unwrap();
            runtime.send_message(run, "いいよ".into()).unwrap();
        }
        let live = runtime.shared.review_runs.lock().unwrap()[&run]
            .requests()
            .0
            .clone();
        runtime.cancel(run).unwrap();
        drop(runtime);
        storage.close();
        let storage = storage::Storage::open(config.clone()).unwrap();
        let store = crate::RunStore::open(&config, storage.handle()).unwrap();
        let rows = store
            .review_evidence_for_lineage(&[run.to_string()])
            .unwrap();
        let replies = rows
            .iter()
            .filter(|row| row.kind == "host_chat")
            .collect::<Vec<_>>();
        assert_eq!(replies.len(), 2);
        assert_ne!(replies[0].id, replies[1].id);
        let restored = crate::run_store::materialize_review_context(&store, run).unwrap();
        assert_eq!(
            serde_json::to_value(restored.requests().0).unwrap(),
            serde_json::to_value(live).unwrap()
        );
        assert_eq!(
            replies
                .iter()
                .map(
                    |row| serde_json::from_str::<crate::escalation_review::ReferenceContext>(
                        row.reference_context.as_ref().unwrap()
                    )
                    .unwrap()
                    .text
                )
                .collect::<Vec<_>>(),
            ["First assistant turn", "Second assistant turn"]
        );
    }

    #[tokio::test]
    async fn trusted_send_without_storage_never_enqueues_or_changes_evidence() {
        let runtime = bare_runtime();
        let run = runtime
            .delegate_chat("fresh", Role::Worker, "first".into(), RunConfig::default())
            .unwrap();
        let before = runtime.shared.review_runs.lock().unwrap()[&run].stamp();
        assert!(matches!(
            runtime.send_message(run, "not accepted".into()),
            Err(RuntimeError::ReviewEvidence { .. })
        ));
        assert_eq!(
            runtime.shared.review_runs.lock().unwrap()[&run].stamp(),
            before
        );
        assert_eq!(runtime.follow_up_status(run).unwrap().pending, 0);
        runtime.cancel(run).unwrap();
    }

    #[tokio::test]
    async fn restored_context_materializes_before_trusted_enqueue_and_reuses_live_arc() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("review.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime = bare_runtime()
            .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let root = runtime
            .delegate_chat("fresh", Role::Worker, "first".into(), RunConfig::default())
            .unwrap();
        let child = runtime.spawn_reserved_child(
            root,
            runtime.reserve_run_id(),
            Role::Worker,
            "delegated",
            RunConfig::default(),
        );
        // register_restored deliberately leaves no review entry: model that state here.
        runtime.shared.review_runs.lock().unwrap().remove(&child);
        runtime.send_message(child, "child only".into()).unwrap();
        let runs = runtime.shared.review_runs.lock().unwrap();
        assert!(Arc::ptr_eq(
            &runs[&root].user_requests,
            &runs[&child].user_requests
        ));
        assert_eq!(runs[&root].requests().0.len(), 1);
        assert_eq!(runs[&child].requests().0.len(), 2);
        let store = runtime.shared.run_store.get().unwrap();
        assert_eq!(
            store
                .review_evidence_for_lineage(&[child.to_string()])
                .unwrap()[0]
                .text,
            "child only"
        );
        drop(runs);
        let fresh = bare_runtime()
            .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let mut empty = fresh.shared.review_runs.lock().unwrap();
        assert!(empty.is_empty());
        let materialized = fresh
            .materialize_review_locked(fresh.shared.run_store.get().unwrap(), child, &mut empty)
            .unwrap();
        assert_eq!(materialized.requests().0.len(), 2);
        assert_eq!(
            crate::run_store::materialize_review_context(store, root)
                .unwrap()
                .requests()
                .0
                .len(),
            1
        );
        runtime.cancel_subtree(root).unwrap();
    }

    #[tokio::test]
    async fn internal_root_is_not_user_evidence_and_pending_tokens_are_compare_and_take() {
        let runtime = bare_runtime();
        let run = runtime.delegate_background(
            Role::Worker,
            "agent authored".into(),
            RunConfig::default(),
        );
        assert!(
            runtime.shared.review_runs.lock().unwrap()[&run]
                .requests()
                .0
                .is_empty()
        );
        let first = runtime
            .capture_pending_reference(run, "old".into())
            .unwrap();
        let second = runtime
            .capture_pending_reference(run, "new".into())
            .unwrap();
        assert_ne!(first, second);
        assert!(
            runtime
                .take_pending_reference(run, first)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            runtime
                .peek_pending_reference(run)
                .unwrap()
                .unwrap()
                .context,
            "new"
        );
        assert_eq!(
            runtime
                .take_pending_reference(run, second)
                .unwrap()
                .unwrap()
                .token,
            second
        );
        assert!(runtime.peek_pending_reference(run).unwrap().is_none());
        runtime.cancel(run).unwrap();
    }

    #[tokio::test]
    async fn host_question_answer_reaches_delegated_review_but_agent_answer_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = storage::StorageConfig {
            db_path: dir.path().join("questions.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).expect("storage");
        let bus = Arc::new(EventBus::new(32));
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(bus, executor, Arc::new(HangingModel))
            .with_run_store(crate::RunStore::open(&config, storage.handle()).expect("run store"));
        let root = runtime.reserve_run_id();
        runtime.spawn_reserved(
            root,
            None,
            Role::Orchestrator,
            "Investigate the sandbox failure",
            RunConfig::default(),
        );
        let worker = runtime.reserve_run_id();
        runtime.spawn_reserved_child(
            root,
            worker,
            Role::Worker,
            "Run the validation command",
            RunConfig::default(),
        );

        let question = runtime
            .request_user_question(
                root,
                "May validation write temporary files outside the sandbox?".into(),
                vec![],
                true,
            )
            .expect("question");
        runtime
            .inherit_user_questions(root, worker, &[])
            .expect("question link");
        runtime
            .answer_user_question(&question.id, "Allow temporary test files")
            .expect("user answer");

        let agent_question = runtime
            .request_user_question(worker, "Which implementation?".into(), vec![], true)
            .expect("worker question");
        runtime
            .answer_subagent_question(root, &agent_question.id, "Use option A")
            .await
            .expect("orchestrator answer");

        let runs = runtime.shared.review_runs.lock().expect("review runs");
        let requests = runs
            .get(&worker)
            .expect("worker review context")
            .requests()
            .0;
        let answers = requests
            .iter()
            .filter(|request| request.in_reply_to.is_some())
            .collect::<Vec<_>>();
        assert_eq!(answers.len(), 1, "linked runs share one user answer");
        assert_eq!(answers[0].text, "Allow temporary test files");
        assert_eq!(answers[0].target_run_id, root.to_string());
        let context = answers[0].in_reply_to.as_ref().expect("question context");
        assert_eq!(context.id, question.id);
        assert_eq!(context.title, question.title);
        assert!(
            !requests
                .iter()
                .any(|request| request.text == "Use option A")
        );
        drop(runs);
        let _ = runtime.cancel(root);
        let _ = runtime.cancel(worker);
    }

    #[tokio::test]
    async fn child_inherits_only_real_user_requests_not_internal_notifications() {
        let bus = Arc::new(EventBus::new(32));
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let model = Arc::new(HangingModel);
        let runtime = AgentRuntime::new(bus, executor, model);
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("review.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let root = runtime
            .delegate_chat(
                "review",
                Role::Orchestrator,
                "Investigate the cause".into(),
                RunConfig::default(),
            )
            .unwrap();
        let child = runtime.reserve_run_id();
        runtime.spawn_reserved_child(
            root,
            child,
            Role::Worker,
            "git push origin main",
            RunConfig::default(),
        );

        let grandchild = runtime.reserve_run_id();
        runtime.spawn_reserved_child(
            child,
            grandchild,
            Role::Worker,
            "Verify and push",
            RunConfig::default(),
        );

        let sibling = runtime.spawn_reserved_child(
            root,
            runtime.reserve_run_id(),
            Role::Worker,
            "Sibling",
            RunConfig::default(),
        );
        runtime
            .send_internal_message(root, "Team leases expired".into())
            .expect("internal message");
        runtime
            .send_message(root, "Implement the fix".into())
            .expect("real user message");
        runtime
            .send_message(child, "Only the child may do this".into())
            .expect("child-directed message");

        let runs = runtime.shared.review_runs.lock().expect("review runs");
        let parent = runs.get(&root).expect("parent");
        let delegated = runs.get(&child).expect("child");
        let nested = runs.get(&grandchild).expect("grandchild");
        assert!(Arc::ptr_eq(&parent.user_requests, &delegated.user_requests));
        assert!(Arc::ptr_eq(&parent.user_requests, &nested.user_requests));
        assert_eq!(
            parent
                .requests()
                .0
                .iter()
                .map(|request| request.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Investigate the cause", "Implement the fix"],
        );
        assert_eq!(
            nested
                .requests()
                .0
                .iter()
                .map(|request| request.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Investigate the cause",
                "Implement the fix",
                "Only the child may do this"
            ],
        );
        assert_eq!(runs[&sibling].requests().0.len(), 2);
        let store = runtime.shared.run_store.get().unwrap();
        let restored_parent = crate::run_store::materialize_review_context(store, root).unwrap();
        let restored_nested =
            crate::run_store::materialize_review_context(store, grandchild).unwrap();
        assert_eq!(restored_parent.requests().0.len(), 2);
        assert_eq!(restored_nested.requests().0.len(), 3);
        assert_eq!(delegated.delegation_chain, vec!["git push origin main"]);
        assert_eq!(
            nested.delegation_chain,
            vec!["git push origin main", "Verify and push"]
        );
        drop(runs);
        let _ = runtime.cancel(root);
        let _ = runtime.cancel(child);
        let _ = runtime.cancel(grandchild);
    }
}

#[cfg(test)]
mod stop_cancel_tests {
    use super::*;

    struct HeldModel {
        admission: bool,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl AgentModel for HeldModel {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "stop-cancel-test".into()
        }
        fn requires_admission(&self) -> bool {
            self.admission
        }
        async fn admit(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
        ) -> Result<(), RuntimeError> {
            self.release.notified().await;
            Ok(())
        }
        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            std::future::pending().await
        }
    }

    fn fixture(admission: bool) -> (AgentRuntime, Arc<HeldModel>) {
        let bus = Arc::new(EventBus::new(64));
        let model = Arc::new(HeldModel {
            admission,
            release: tokio::sync::Notify::new(),
        });
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());
        (runtime, model)
    }

    fn spawn(runtime: &AgentRuntime, parent: Option<RunId>) -> RunId {
        runtime.spawn_reserved(
            runtime.reserve_run_id(),
            parent,
            Role::Worker,
            "held",
            RunConfig::default(),
        )
    }

    #[tokio::test]
    async fn cancel_subtree_follows_registered_parents_and_unregistered_intents() {
        let (runtime, _) = fixture(false);
        let root = spawn(&runtime, None);
        let child = spawn(&runtime, Some(root));
        let grandchild = spawn(&runtime, Some(child));
        let unrelated = spawn(&runtime, None);
        let reserved = runtime.reserve_run_id();
        runtime.seed_spawn_intent(reserved, &grandchild.to_string());
        // Registered parent links must also be traversed, even without an intent.
        runtime.shared.spawn_intents.lock().unwrap().remove(&child);
        runtime.cancel_subtree(root).unwrap();
        for id in [root, child, grandchild, reserved] {
            assert!(runtime.spawn_cancelled(id));
        }
        assert!(!runtime.spawn_cancelled(unrelated));
        assert_eq!(
            *runtime.entry(unrelated).unwrap().cancel_tx.borrow(),
            RunInterrupt::None
        );
        // Both pre-recorded intents and children spawned after cancellation are fenced.
        runtime.spawn_reserved_child(
            grandchild,
            reserved,
            Role::Worker,
            "reserved",
            RunConfig::default(),
        );
        let late = spawn(&runtime, Some(reserved));
        for id in [root, child, grandchild, reserved, late] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), runtime.wait(id))
                    .await
                    .unwrap()
                    .unwrap(),
                AgentRunPhase::Error
            );
        }
        runtime.cancel_subtree(unrelated).unwrap();
        let unknown = RunId::new(u64::MAX);
        assert!(matches!(
            runtime.cancel_subtree(unknown),
            Err(RuntimeError::UnknownRun { .. })
        ));
        assert!(!runtime.spawn_cancelled(unknown));
    }

    #[tokio::test]
    async fn explicit_root_restore_preserves_old_descendant_cancellation_fences() {
        let (runtime, _) = fixture(false);
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("discard-resume.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let root = spawn(&runtime, None);
        let child = spawn(&runtime, Some(root));
        let reserved = runtime.reserve_run_id();
        runtime.seed_spawn_intent(reserved, &root.to_string());
        tokio::task::yield_now().await;
        runtime.cancel_subtree(root).unwrap();
        for run in [root, child] {
            assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
        }
        assert_eq!(
            runtime
                .continue_goal(root, "explicit followup".into(), RunConfig::default())
                .unwrap(),
            root
        );
        assert!(!runtime.spawn_cancelled(root));
        assert!(runtime.spawn_cancelled(child));
        assert!(runtime.spawn_cancelled(reserved));
        runtime.spawn_reserved_child(
            root,
            reserved,
            Role::Worker,
            "old generation",
            RunConfig::default(),
        );
        assert_eq!(runtime.wait(reserved).await.unwrap(), AgentRunPhase::Error);
        let fresh = spawn(&runtime, Some(root));
        assert!(!runtime.spawn_cancelled(fresh));
        runtime.cancel_subtree(root).unwrap();
    }

    #[tokio::test]
    async fn cancel_subtree_fences_all_pending_admissions_and_late_descendants() {
        let (runtime, model) = fixture(true);
        let root = spawn(&runtime, None);
        let child = spawn(&runtime, Some(root));
        let grandchild = spawn(&runtime, Some(child));
        runtime.cancel_subtree(root).unwrap();
        let late = spawn(&runtime, Some(grandchild));
        for id in [root, child, grandchild, late] {
            assert!(runtime.spawn_cancelled(id));
            assert!(runtime.shared.admissions.lock().unwrap()[&id].cancelled);
        }
        tokio::task::yield_now().await;
        model.release.notify_waiters();
        for id in [root, child, grandchild, late] {
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(5), runtime.wait(id))
                    .await
                    .unwrap(),
                Err(RuntimeError::RunTerminated { .. })
            ));
        }
        assert!(runtime.list_agents().is_empty());
        // Repeated discard is harmless after admission cancellation completed.
        runtime.cancel_subtree(root).unwrap();
    }

    #[tokio::test]
    async fn stop_subtree_with_pending_root_covers_children_without_poisoning_spawns() {
        let (runtime, model) = fixture(true);
        let root = spawn(&runtime, None);
        let child = spawn(&runtime, Some(root));
        runtime.stop(root, StopScope::SelfOnly).unwrap();
        assert!(!runtime.shared.admissions.lock().unwrap()[&child].cancelled);
        runtime.stop(root, StopScope::Subtree).unwrap();
        for id in [root, child] {
            assert!(runtime.shared.admissions.lock().unwrap()[&id].cancelled);
            assert!(!runtime.spawn_cancelled(id));
        }
        let fresh = spawn(&runtime, Some(root));
        assert!(!runtime.shared.admissions.lock().unwrap()[&fresh].cancelled);
        tokio::task::yield_now().await;
        model.release.notify_waiters();
        runtime.wait_admission(fresh).await.unwrap();
        runtime.cancel_subtree(fresh).unwrap();
        assert_eq!(runtime.wait(fresh).await.unwrap(), AgentRunPhase::Error);
    }

    #[tokio::test]
    async fn agent_summaries_expose_parent_run_ids() {
        let (runtime, _) = fixture(false);
        let root = spawn(&runtime, None);
        let child = spawn(&runtime, Some(root));
        let parent_of = |run| {
            runtime
                .list_agents()
                .into_iter()
                .find(|summary| summary.run_id == run)
                .and_then(|summary| summary.parent_run_id)
        };
        assert_eq!(parent_of(root), None);
        assert_eq!(parent_of(child), Some(root));
        runtime.cancel_subtree(root).unwrap();
        for run in [root, child] {
            assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
        }
    }
}

#[cfg(test)]
#[path = "runtime/comment_checker_tests.rs"]
mod comment_checker_tests;

#[cfg(test)]
#[path = "runtime/project_switch_tests.rs"]
mod project_switch_tests;

#[cfg(test)]
use crate::escalation_review::support as comment_checker_support;

#[cfg(test)]
mod review_registration_tests {
    use super::*;

    #[tokio::test]
    async fn registration_installs_seed_without_reading_a_closed_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("review.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(crate::escalation_review::support::ScriptedModel::new([])),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime.reserve_run_id();
        let context = crate::escalation_review::ReviewRunContext::empty(&run.to_string());
        context.add_user_request(&run.to_string(), "prepared evidence");
        drop(storage);
        runtime.register_run(
            run,
            None,
            Role::Worker,
            "not evidence".into(),
            RunConfig::default(),
            RunContinuation::Fresh,
            Some(ReviewSeed {
                provenance: HostProvenance::FreshHostSubmission,
                context,
            }),
        );
        assert_eq!(
            runtime.shared.review_runs.lock().unwrap()[&run]
                .requests()
                .0[0]
                .text,
            "prepared evidence"
        );
        runtime.cancel(run).unwrap();
        let rejected = runtime.delegate_chat(
            "rejected",
            Role::Worker,
            "cannot persist".into(),
            RunConfig::default(),
        );
        assert!(matches!(rejected, Err(RuntimeError::ReviewEvidence { .. })));
        let child = runtime.spawn_reserved_child(
            run,
            runtime.reserve_run_id(),
            Role::Worker,
            "no edge",
            RunConfig::default(),
        );
        assert!(matches!(
            runtime.wait_admission(child).await,
            Err(RuntimeError::ReviewEvidence { .. })
        ));
        assert!(!lock_runs(&runtime.shared.runs).contains_key(&child));
        assert!(
            !runtime
                .shared
                .review_runs
                .lock()
                .unwrap()
                .contains_key(&child)
        );
    }
}

#[cfg(test)]
mod agent_browser_sharing_tests {
    use super::*;

    struct AllowAll;

    #[async_trait::async_trait]
    impl browser::agent::RequestPolicy for AllowAll {
        async fn check(&self, _url: &url::Url) -> Result<(), String> {
            Ok(())
        }

        async fn resolve(
            &self,
            _host: &str,
            _port: u16,
            _tls: bool,
        ) -> Result<Vec<std::net::SocketAddr>, String> {
            Err("unused".into())
        }
    }

    #[test]
    fn run_executors_drive_the_parent_agent_browser() {
        // Given: a runtime executor holding the process's agent browser.
        let bus = Arc::new(EventBus::new(16));
        let browser = Arc::new(browser::agent::AgentBrowser::new(
            browser::agent::LaunchOptions {
                executable: Some("/nonexistent/chromium".into()),
                policy: Arc::new(AllowAll),
                max_pages: 1,
            },
        ));
        let parent = ToolExecutor::new(Arc::clone(&bus)).with_agent_browser(Arc::clone(&browser));
        let workspace = tempfile::tempdir().unwrap();

        // When: a run composes its own executor from that parent.
        let run = configured_executor(
            bus,
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
            &sandbox::SandboxBoundary::new(workspace.path().to_path_buf()),
            &config::CommentCheckerConfig::default(),
            &[],
            sandbox::ApprovalPolicy::allow_all(),
            Some(&parent),
        )
        .unwrap();

        // Then: its browser tools use the same browser, so observers see its page.
        assert!(Arc::ptr_eq(&run.agent_browser().unwrap(), &browser));
        assert!(run.tool_permissions("browser_navigate").is_some());
    }
}
