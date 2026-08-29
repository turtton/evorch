//! AgentRun の登録と公開操作を提供するランタイム表層。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, LifecycleEvent};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tools::ToolExecutor;

use crate::agent_loop::{LoopChannels, LoopShared, RunTask, run_agent};
use crate::{AgentInspection, AgentModel, AgentSummary, RunConfig, RunId, RuntimeError};

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
    next_run_id: AtomicU64,
    runs: Mutex<HashMap<RunId, RunEntry>>,
}

struct RunEntry {
    role: Role,
    phase_rx: watch::Receiver<AgentRunPhase>,
    message_count_rx: watch::Receiver<usize>,
    inbox_tx: mpsc::Sender<String>,
    cancel_tx: watch::Sender<bool>,
    _join: Option<JoinHandle<()>>,
}

impl AgentRuntime {
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
                next_run_id: AtomicU64::new(1),
                runs: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// run を登録してバックグラウンド実行を開始し、その ID を返す。
    pub fn delegate_background(&self, role: Role, prompt: String, config: RunConfig) -> RunId {
        let run_id = RunId::new(self.shared.next_run_id.fetch_add(1, Ordering::Relaxed));
        let (phase_tx, phase_rx) = watch::channel(AgentRunPhase::Pending);
        let (message_count_tx, message_count_rx) = watch::channel(0);
        let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = RunTask {
            run_id,
            role,
            prompt,
            config,
        };
        let channels = LoopChannels {
            phase_tx,
            message_count_tx,
            inbox_rx,
            cancel_rx,
        };
        lock_runs(&self.shared.runs).insert(
            run_id,
            RunEntry {
                role,
                phase_rx,
                message_count_rx,
                inbox_tx,
                cancel_tx,
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
    pub async fn delegate(
        &self,
        role: Role,
        prompt: String,
    ) -> Result<AgentRunPhase, RuntimeError> {
        let run_id = self.delegate_background(role, prompt, RunConfig::default());
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
                role_name: entry.role.name().to_string(),
                phase: *entry.phase_rx.borrow(),
            })
            .collect();
        summaries.sort_by_key(|summary| summary.run_id.get());
        summaries
    }

    /// run の位相と会話履歴件数を返す。
    pub fn inspect_agent(&self, run_id: RunId) -> Result<AgentInspection, RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        let entry = runs.get(&run_id).ok_or_else(|| unknown_run(run_id))?;
        Ok(AgentInspection {
            run_id,
            role_name: entry.role.name().to_string(),
            phase: *entry.phase_rx.borrow(),
            message_count: *entry.message_count_rx.borrow(),
        })
    }

    fn entry(&self, run_id: RunId) -> Result<RunEntryView<'_>, RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        if !runs.contains_key(&run_id) {
            return Err(unknown_run(run_id));
        }
        Ok(RunEntryView { runs, run_id })
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
    })
}
