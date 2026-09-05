//! goal lifecycle を所有する supervisor actor。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, ApprovalDecision,
    CompactionEvent, Event, EventBus, EventKind, EventReceiver, GoalReference, GoalState,
    InvalidationReason, LifecycleEvent, OrchestratorEvent, ProviderEvent, RecvError, RunPurpose,
    SuppressReason, ToolEvent,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

use crate::{AgentRuntime, Role, RunConfig, RunId};

use super::continuation::{self, ContinuationDecision};
use super::delivery::DeliveryPort;
use super::gate::GateVerdict;
use super::ledger::{GoalLedger, GoalSnapshot, LedgerError, OrchestrationSettings};
use super::prompts::{render_continuation_prompt, render_recovery_prompt};
use super::registry::GoalRegistry;
use super::stall::{self, ProgressTrack};

static NEXT_GOAL_ID: AtomicU64 = AtomicU64::new(1);

/// goal 作成時の不変属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalSpec {
    /// 永続化セッション ID。
    pub session_id: String,
    /// project ID。
    pub project_id: String,
    /// thread ID。
    pub thread_id: String,
    /// goal 本文。
    pub goal: String,
    /// goal の参照元。
    pub references: Vec<GoalReference>,
    /// goal の制約。
    pub constraints: Vec<String>,
    /// 対象リポジトリ。
    pub repo: String,
    /// マージ先ブランチ。
    pub base_ref: String,
}

/// supervisor command の受付失敗。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SupervisorError {
    /// supervisor task が終了済み。
    #[error("goal supervisor is not running")]
    Closed,
    /// 指定 goal が存在しない。
    #[error("unknown goal: {0}")]
    UnknownGoal(String),
    /// ledger が操作を拒否した。
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

/// 同期呼び出し元から利用できる supervisor handle。
#[derive(Clone)]
pub struct SupervisorHandle {
    tx: Arc<mpsc::UnboundedSender<SupervisorCommand>>,
    bus: Arc<EventBus>,
    ledgers: Arc<Mutex<BTreeMap<String, GoalLedger>>>,
}

impl SupervisorHandle {
    /// goal を作成し、割り当てた ID を返す。
    pub fn create_goal(&self, spec: GoalSpec, root_run: RunId) -> String {
        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let sequence = NEXT_GOAL_ID.fetch_add(1, Ordering::Relaxed);
        let goal_id = format!("goal-{wall_ms}-{sequence}");
        let _ = self.tx.send(SupervisorCommand::Create {
            goal_id: goal_id.clone(),
            spec,
            root_run,
        });
        goal_id
    }

    /// Active goal を一時停止する。
    pub fn pause(&self, goal_id: &str) -> Result<(), SupervisorError> {
        self.send_goal(goal_id, GoalCommand::Pause)
    }

    /// Paused または Blocked goal を再開する。
    pub fn resume(&self, goal_id: &str) -> Result<(), SupervisorError> {
        self.send_goal(goal_id, GoalCommand::Resume)
    }

    /// goal を取り消す。
    pub fn cancel(&self, goal_id: &str) -> Result<(), SupervisorError> {
        self.send_goal(goal_id, GoalCommand::Cancel)
    }

    /// run を止め、goal は Paused にする。
    pub fn stop(&self, goal_id: &str) -> Result<(), SupervisorError> {
        self.send_goal(goal_id, GoalCommand::Stop)
    }

    /// マージ判断を supervisor へ転送する。
    pub fn decide_merge(
        &self,
        token_id: impl Into<String>,
        decision: ApprovalDecision,
    ) -> Result<(), SupervisorError> {
        self.tx
            .send(SupervisorCommand::DecideMerge {
                token_id: token_id.into(),
                decision,
            })
            .map_err(|_| SupervisorError::Closed)
    }

    /// 現在の goal snapshot を返す。
    pub fn snapshot(&self, goal_id: &str) -> Option<GoalSnapshot> {
        self.ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(goal_id)
            .map(|ledger| ledger.snapshot().clone())
    }

    /// 永続化スナップショットと transcript を移管する。
    pub fn adopt(
        &self,
        goals: Vec<(GoalSnapshot, Vec<AgentMessage>)>,
    ) -> Result<(), SupervisorError> {
        self.tx
            .send(SupervisorCommand::Adopt(goals))
            .map_err(|_| SupervisorError::Closed)
    }

    /// snapshot から parent を持たない recovery run を開始する。
    pub fn recover(
        &self,
        snapshot: GoalSnapshot,
        transcript: Vec<AgentMessage>,
    ) -> Result<(), SupervisorError> {
        self.tx
            .send(SupervisorCommand::Recover {
                snapshot,
                transcript,
            })
            .map_err(|_| SupervisorError::Closed)
    }

    /// supervisor と同じ EventBus を購読する。
    pub fn subscribe(&self) -> EventReceiver {
        self.bus.subscribe()
    }

    fn send_goal(&self, goal_id: &str, command: GoalCommand) -> Result<(), SupervisorError> {
        if !self
            .ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(goal_id)
        {
            return Err(SupervisorError::UnknownGoal(goal_id.to_string()));
        }
        self.tx
            .send(SupervisorCommand::Goal {
                goal_id: goal_id.to_string(),
                command,
            })
            .map_err(|_| SupervisorError::Closed)
    }
}

/// goal supervisor の生成入口。
pub struct GoalSupervisor;

impl GoalSupervisor {
    /// actor loop を起動し、同期 command handle を返す。
    pub fn spawn(
        runtime: AgentRuntime,
        bus: Arc<EventBus>,
        delivery: Arc<dyn DeliveryPort>,
        settings: OrchestrationSettings,
    ) -> SupervisorHandle {
        let ledgers = Arc::new(Mutex::new(BTreeMap::new()));
        let registry = GoalRegistry::new(
            Arc::clone(&ledgers),
            Arc::clone(&delivery),
            settings.clone(),
        );
        let (tx, commands) = mpsc::unbounded_channel();
        let handle = SupervisorHandle {
            tx: Arc::new(tx),
            bus: Arc::clone(&bus),
            ledgers: Arc::clone(&ledgers),
        };
        tokio::spawn(
            SupervisorActor {
                runtime,
                bus: Arc::clone(&bus),
                settings,
                registry,
                ledgers,
                commands,
                events: bus.subscribe(),
                progress: HashMap::new(),
                terminal_runs: HashSet::new(),
                deferred: HashSet::new(),
                transcripts: HashMap::new(),
            }
            .run(),
        );
        handle
    }
}

enum GoalCommand {
    Pause,
    Resume,
    Cancel,
    Stop,
}

enum SupervisorCommand {
    Create {
        goal_id: String,
        spec: GoalSpec,
        root_run: RunId,
    },
    Goal {
        goal_id: String,
        command: GoalCommand,
    },
    DecideMerge {
        token_id: String,
        decision: ApprovalDecision,
    },
    Adopt(Vec<(GoalSnapshot, Vec<AgentMessage>)>),
    Recover {
        snapshot: GoalSnapshot,
        transcript: Vec<AgentMessage>,
    },
}

struct SupervisorActor {
    runtime: AgentRuntime,
    bus: Arc<EventBus>,
    settings: OrchestrationSettings,
    registry: GoalRegistry,
    ledgers: Arc<Mutex<BTreeMap<String, GoalLedger>>>,
    commands: mpsc::UnboundedReceiver<SupervisorCommand>,
    events: EventReceiver,
    progress: HashMap<String, ProgressTrack>,
    terminal_runs: HashSet<String>,
    deferred: HashSet<String>,
    transcripts: HashMap<String, Vec<AgentMessage>>,
}

impl SupervisorActor {
    async fn run(mut self) {
        let mut stall_tick =
            tokio::time::interval(Duration::from_secs(self.settings.stall_check_secs.max(1)));
        stall_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => self.handle_command(command).await,
                    None => return,
                },
                event = self.events.recv() => match event {
                    Ok(event) => self.handle_bus_event(event).await,
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
                _ = stall_tick.tick() => self.sample_stalls().await,
            }
        }
    }

    async fn handle_command(&mut self, command: SupervisorCommand) {
        match command {
            SupervisorCommand::Create {
                goal_id,
                spec,
                root_run,
            } => self.create(goal_id, spec, root_run),
            SupervisorCommand::Goal { goal_id, command } => {
                self.change_goal(&goal_id, command).await
            }
            SupervisorCommand::Adopt(goals) => self.adopt(goals),
            SupervisorCommand::Recover {
                snapshot,
                transcript,
            } => self.recover_snapshot(snapshot, transcript).await,
            SupervisorCommand::DecideMerge { token_id, decision } => {
                let _ = (token_id, decision);
            }
        }
    }

    fn create(&mut self, goal_id: String, spec: GoalSpec, root_run: RunId) {
        let event = OrchestratorEvent::GoalCreated {
            goal_id: goal_id.clone(),
            session_id: spec.session_id,
            project_id: spec.project_id,
            thread_id: spec.thread_id,
            goal: spec.goal,
            references: spec.references,
            constraints: spec.constraints,
            repo: spec.repo,
            base_ref: spec.base_ref,
            root_run_id: root_run.to_string(),
        };
        self.ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(goal_id, GoalLedger::new(&event));
        self.progress.insert(
            root_run.to_string(),
            ProgressTrack::new(AgentRunPhase::Pending),
        );
        self.bus.emit(Event::new(event));
    }

    async fn change_goal(&mut self, goal_id: &str, command: GoalCommand) {
        let Some(snapshot) = self.snapshot(goal_id) else {
            return;
        };
        match command {
            GoalCommand::Pause => {
                let _ = self.transition(goal_id, GoalState::Paused, "paused by operator");
            }
            GoalCommand::Cancel => {
                self.cancel_attached(&snapshot);
                let _ = self.transition(goal_id, GoalState::Cancelled, "cancelled by operator");
            }
            GoalCommand::Stop => {
                self.cancel_attached(&snapshot);
                let _ = self.transition(goal_id, GoalState::Paused, "stopped by operator");
            }
            GoalCommand::Resume => {
                if self
                    .transition(goal_id, GoalState::Active, "resumed by operator")
                    .is_err()
                {
                    return;
                }
                let Some(resumed) = self.snapshot(goal_id) else {
                    return;
                };
                if resumed.detached {
                    let transcript = self.transcripts.remove(goal_id).unwrap_or_default();
                    self.recover_snapshot(resumed, transcript).await;
                } else {
                    self.try_dispatch(goal_id).await;
                }
            }
        }
    }

    fn adopt(&mut self, goals: Vec<(GoalSnapshot, Vec<AgentMessage>)>) {
        for (mut snapshot, transcript) in goals {
            let goal_id = snapshot.goal_id.clone();
            let active = snapshot.state == GoalState::Active;
            snapshot.detached = true;
            self.transcripts.insert(goal_id.clone(), transcript);
            self.ledgers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(goal_id.clone(), GoalLedger::from_snapshot(snapshot));
            if active {
                let _ = self.transition(&goal_id, GoalState::Paused, "recovered-after-restart");
            }
            self.invalidate_pending_approvals(&goal_id);
        }
    }

    async fn recover_snapshot(&mut self, snapshot: GoalSnapshot, transcript: Vec<AgentMessage>) {
        let goal_id = snapshot.goal_id.clone();
        let epoch = snapshot.epoch;
        let run = self.runtime.delegate_background(
            Role::Orchestrator,
            render_recovery_prompt(&snapshot, &transcript),
            RunConfig {
                name: Some(format!("{goal_id}/recover{epoch}")),
                ..RunConfig::default()
            },
        );
        self.emit_for_goal(
            &goal_id,
            OrchestratorEvent::RunAttached {
                goal_id: goal_id.clone(),
                run_id: run.to_string(),
                parent_run_id: None,
                role: "orchestrator".into(),
                purpose: RunPurpose::Recovery { epoch },
            },
        );
        self.emit_for_goal(
            &goal_id,
            OrchestratorEvent::ContinuationDispatched {
                goal_id: goal_id.clone(),
                epoch,
                trigger_run_id: snapshot.root_run_id,
                new_run_id: run.to_string(),
                unmet: snapshot.last_rejections,
            },
        );
        if let Some(ledger) = self
            .ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&goal_id)
        {
            ledger.set_detached(false);
        }
        self.progress
            .insert(run.to_string(), ProgressTrack::new(AgentRunPhase::Pending));
    }

    async fn handle_bus_event(&mut self, event: Event) {
        match event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                self.on_phase(run_id, to).await
            }
            EventKind::Tool(tool) => self.on_tool(tool),
            EventKind::Provider(provider) => self.on_provider(provider),
            EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => {
                if message.kind != AgentMessageKind::Steering {
                    self.mark_progress(&message.sender_run_id);
                    self.mark_progress(&message.recipient_run_id);
                }
            }
            EventKind::Compaction(CompactionEvent::Compacted { run_id, .. }) => {
                self.mark_progress(&run_id)
            }
            EventKind::Orchestrator(orchestrator) => {
                self.on_external_orchestrator(orchestrator).await
            }
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Usage(_)
            | EventKind::Fault(_) => {}
        }
    }

    async fn on_phase(&mut self, run_id: String, phase: AgentRunPhase) {
        if let Some(track) = self.progress.get_mut(&run_id) {
            track.phase = phase;
            if phase == AgentRunPhase::Running {
                track.progress(Instant::now());
            }
        }
        if !matches!(phase, AgentRunPhase::Done | AgentRunPhase::Error) {
            return;
        }
        let first_terminal = self.terminal_runs.insert(run_id.clone());
        for goal_id in self.goals_for_run(&run_id) {
            let Some(snapshot) = self.snapshot(&goal_id) else {
                continue;
            };
            if snapshot.current_orchestrator_run_id == run_id && first_terminal {
                if let Some(ledger) = self
                    .ledgers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&goal_id)
                {
                    ledger.advance_epoch();
                }
                self.try_dispatch(&goal_id).await;
            } else if !first_terminal {
                self.suppress(&goal_id, snapshot.epoch, SuppressReason::Duplicate);
            }
            if self.deferred.contains(&goal_id) && !self.pipeline_busy(&goal_id) {
                self.try_dispatch(&goal_id).await;
            }
        }
    }

    fn on_tool(&mut self, event: ToolEvent) {
        match event {
            ToolEvent::ToolStarted {
                run_id: Some(run_id),
                ..
            } => {
                let now = Instant::now();
                if let Some(track) = self.progress.get_mut(&run_id) {
                    track.progress(now);
                    track.tool_in_flight = Some(now);
                }
            }
            ToolEvent::ToolCompleted {
                is_error,
                run_id: Some(run_id),
                ..
            } => {
                let now = Instant::now();
                if let Some(track) = self.progress.get_mut(&run_id) {
                    track.progress(now);
                    track.tool_in_flight = None;
                    track.consecutive_tool_errors = if is_error {
                        track.consecutive_tool_errors.saturating_add(1)
                    } else {
                        0
                    };
                }
            }
            ToolEvent::ToolStarted { run_id: None, .. }
            | ToolEvent::ToolCompleted { run_id: None, .. }
            | ToolEvent::ApprovalRequested { .. }
            | ToolEvent::ApprovalResolved { .. }
            | ToolEvent::ExecutionDenied { .. } => {}
        }
    }

    fn on_provider(&mut self, event: ProviderEvent) {
        let run_id = match event {
            ProviderEvent::RequestStarted { run_id, .. }
            | ProviderEvent::FirstTokenObserved { run_id, .. }
            | ProviderEvent::RequestCompleted { run_id, .. }
            | ProviderEvent::RequestFailed { run_id, .. } => run_id,
            ProviderEvent::ProviderFallback { .. } | ProviderEvent::FallbackTriggered { .. } => {
                None
            }
        };
        if let Some(run_id) = run_id {
            self.mark_progress(&run_id);
        }
    }

    async fn on_external_orchestrator(&mut self, event: OrchestratorEvent) {
        let Some(goal_id) = orchestrator_goal_id(&event).map(str::to_string) else {
            return;
        };
        if !self.event_is_applied(&goal_id, &event) {
            if let Some(ledger) = self
                .ledgers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&goal_id)
            {
                let _ = ledger.apply(&event);
            }
        }
        if let OrchestratorEvent::RunAttached { run_id, .. } = &event {
            self.progress
                .entry(run_id.clone())
                .or_insert_with(|| ProgressTrack::new(AgentRunPhase::Pending));
        }
    }

    async fn try_dispatch(&mut self, goal_id: &str) {
        let Some(snapshot) = self.snapshot(goal_id) else {
            return;
        };
        let terminal = self
            .terminal_runs
            .contains(&snapshot.current_orchestrator_run_id)
            || snapshot.epoch > 0;
        let Some(decision) = continuation::decide(
            &snapshot,
            terminal,
            self.pipeline_busy(goal_id),
            &self.settings,
        ) else {
            return;
        };
        match decision {
            ContinuationDecision::Suppress(SuppressReason::LimitReached { max }) => {
                self.suppress(
                    goal_id,
                    snapshot.epoch,
                    SuppressReason::LimitReached { max },
                );
                let _ = self.transition(goal_id, GoalState::Blocked, "continuation limit");
            }
            ContinuationDecision::Suppress(reason) => {
                if reason == SuppressReason::PipelineBusy {
                    self.deferred.insert(goal_id.to_string());
                }
                self.suppress(goal_id, snapshot.epoch, reason);
            }
            ContinuationDecision::Dispatch => {
                self.deferred.remove(goal_id);
                self.dispatch_continuation(snapshot).await;
            }
        }
    }

    async fn dispatch_continuation(&mut self, snapshot: GoalSnapshot) {
        let unmet = match self
            .registry
            .evaluate_finish_for_goal(&snapshot.goal_id)
            .await
        {
            Some(GateVerdict::Reject(unmet)) => unmet,
            Some(GateVerdict::Accept(_)) => return,
            None => vec![event_bus::GateRejection::NoGoalBound],
        };
        let Some(parent) = self.find_run(&snapshot.root_run_id) else {
            return;
        };
        let run = match self.runtime.delegate_background_as_child(
            parent,
            Role::Orchestrator,
            render_continuation_prompt(&snapshot, &unmet, &[], snapshot.nudges.len() as u32),
            RunConfig {
                name: Some(format!("{}/c{}", snapshot.goal_id, snapshot.epoch)),
                ..RunConfig::default()
            },
        ) {
            Ok(run) => run,
            Err(_) => return,
        };
        self.emit_for_goal(
            &snapshot.goal_id,
            OrchestratorEvent::RunAttached {
                goal_id: snapshot.goal_id.clone(),
                run_id: run.to_string(),
                parent_run_id: Some(parent.to_string()),
                role: "orchestrator".into(),
                purpose: RunPurpose::Continuation {
                    epoch: snapshot.epoch,
                },
            },
        );
        self.emit_for_goal(
            &snapshot.goal_id,
            OrchestratorEvent::ContinuationDispatched {
                goal_id: snapshot.goal_id.clone(),
                epoch: snapshot.epoch,
                trigger_run_id: snapshot.current_orchestrator_run_id,
                new_run_id: run.to_string(),
                unmet,
            },
        );
        self.progress
            .insert(run.to_string(), ProgressTrack::new(AgentRunPhase::Pending));
    }

    async fn sample_stalls(&mut self) {
        let now = Instant::now();
        let stalled = self
            .progress
            .iter()
            .filter_map(|(run_id, track)| {
                stall::judge(track, now, &self.settings).map(|signal| {
                    (
                        run_id.clone(),
                        signal,
                        now.saturating_duration_since(track.last_progress),
                    )
                })
            })
            .collect::<Vec<_>>();
        for (run_id, signal, idle) in stalled {
            for goal_id in self.goals_for_run(&run_id) {
                self.emit_for_goal(
                    &goal_id,
                    OrchestratorEvent::StallDetected {
                        goal_id: goal_id.clone(),
                        run_id: run_id.clone(),
                        idle_ms: idle.as_millis().try_into().unwrap_or(u64::MAX),
                        signal,
                    },
                );
                self.nudge_or_cancel(&goal_id, &run_id);
            }
        }
    }

    fn nudge_or_cancel(&mut self, goal_id: &str, run_id: &str) {
        let sent = self
            .progress
            .get(run_id)
            .map_or(0, |track| track.nudges_sent);
        let Some(run) = self.find_run(run_id) else {
            return;
        };
        let parent = self
            .parent_for(goal_id, run_id)
            .and_then(|id| self.find_run(&id));
        if sent >= self.settings.max_nudges {
            let _ = self.runtime.cancel(run);
            if parent.is_some() {
                let _ = self.transition(goal_id, GoalState::Blocked, format!("stalled {run_id}"));
            }
            return;
        }
        let Some(parent) = parent else {
            let _ = self.runtime.cancel(run);
            return;
        };
        if let Ok(message_id) = self.runtime.send_agent_message(
            parent,
            run,
            AgentMessageKind::Steering,
            "Progress has stalled. Report the blocker or take the next concrete action.",
            None,
        ) {
            let index = sent.saturating_add(1);
            if let Some(track) = self.progress.get_mut(run_id) {
                track.nudges_sent = index;
                track.last_progress = Instant::now();
            }
            self.emit_for_goal(
                goal_id,
                OrchestratorEvent::NudgeSent {
                    goal_id: goal_id.to_string(),
                    run_id: run_id.to_string(),
                    nudge_index: index,
                    message_id,
                },
            );
        }
    }

    fn transition(
        &mut self,
        goal_id: &str,
        to: GoalState,
        reason: impl Into<String>,
    ) -> Result<(), LedgerError> {
        let event = {
            let ledgers = self
                .ledgers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(ledger) = ledgers.get(goal_id) else {
                return Ok(());
            };
            ledger.transition(to, reason)?
        };
        self.emit_for_goal(goal_id, event);
        Ok(())
    }

    fn emit_for_goal(&self, goal_id: &str, event: OrchestratorEvent) {
        let applied = self
            .ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(goal_id)
            .is_some_and(|ledger| ledger.apply(&event).is_ok());
        if applied {
            self.bus.emit(Event::new(event));
        }
    }

    fn suppress(&self, goal_id: &str, epoch: u64, reason: SuppressReason) {
        self.emit_for_goal(
            goal_id,
            OrchestratorEvent::ContinuationSuppressed {
                goal_id: goal_id.to_string(),
                epoch,
                reason,
            },
        );
    }

    fn snapshot(&self, goal_id: &str) -> Option<GoalSnapshot> {
        self.ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(goal_id)
            .map(|ledger| ledger.snapshot().clone())
    }

    fn goals_for_run(&self, run_id: &str) -> Vec<String> {
        self.ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|(goal_id, ledger)| {
                ledger
                    .snapshot()
                    .attached_runs
                    .iter()
                    .any(|attached| attached.run_id == run_id)
                    .then(|| goal_id.clone())
            })
            .collect()
    }

    fn pipeline_busy(&self, goal_id: &str) -> bool {
        self.snapshot(goal_id).is_some_and(|snapshot| {
            snapshot.attached_runs.iter().any(|attached| {
                matches!(
                    attached.purpose,
                    RunPurpose::Review { .. } | RunPurpose::Repair { .. }
                ) && !self.terminal_runs.contains(&attached.run_id)
            })
        })
    }

    fn parent_for(&self, goal_id: &str, run_id: &str) -> Option<String> {
        self.snapshot(goal_id)?
            .attached_runs
            .into_iter()
            .find_map(|attached| {
                (attached.run_id == run_id)
                    .then_some(attached.parent_run_id)
                    .flatten()
            })
    }

    fn find_run(&self, run_id: &str) -> Option<RunId> {
        self.runtime
            .list_agents()
            .into_iter()
            .find_map(|summary| (summary.run_id.to_string() == run_id).then_some(summary.run_id))
    }

    fn cancel_attached(&self, snapshot: &GoalSnapshot) {
        for attached in &snapshot.attached_runs {
            if let Some(run) = self.find_run(&attached.run_id) {
                let _ = self.runtime.cancel(run);
            }
        }
    }

    fn mark_progress(&mut self, run_id: &str) {
        if let Some(track) = self.progress.get_mut(run_id) {
            track.progress(Instant::now());
        }
    }

    fn invalidate_pending_approvals(&self, goal_id: &str) {
        let Some(snapshot) = self.snapshot(goal_id) else {
            return;
        };
        for binding in snapshot.approvals_issued {
            let resolved = snapshot
                .approval_resolutions
                .iter()
                .any(|(token, _)| token == &binding.token_id);
            let invalidated = snapshot
                .approval_invalidations
                .iter()
                .any(|(token, _)| token == &binding.token_id);
            if !resolved && !invalidated {
                self.emit_for_goal(
                    goal_id,
                    OrchestratorEvent::MergeApprovalInvalidated {
                        goal_id: goal_id.to_string(),
                        token_id: binding.token_id,
                        reason: InvalidationReason::GoalNotActive,
                    },
                );
            }
        }
    }

    fn event_is_applied(&self, goal_id: &str, event: &OrchestratorEvent) -> bool {
        let Some(snapshot) = self.snapshot(goal_id) else {
            return false;
        };
        match event {
            OrchestratorEvent::GoalCreated { .. } => true,
            OrchestratorEvent::GoalStateChanged { to, .. } => snapshot.state == *to,
            OrchestratorEvent::GoalStageChanged { to, .. } => snapshot.stage == *to,
            OrchestratorEvent::RunAttached { run_id, .. } => snapshot
                .attached_runs
                .iter()
                .any(|run| run.run_id == *run_id),
            OrchestratorEvent::ContinuationDispatched { epoch, .. } => {
                snapshot.dispatched_epochs.contains(epoch)
            }
            OrchestratorEvent::ContinuationSuppressed { epoch, reason, .. } => {
                snapshot.continuation_suppressions.get(epoch) == Some(reason)
            }
            OrchestratorEvent::NudgeSent { message_id, .. } => snapshot
                .nudges
                .iter()
                .any(|nudge| nudge.message_id == *message_id),
            OrchestratorEvent::StallDetected { .. }
            | OrchestratorEvent::DeliverableBranchBound { .. }
            | OrchestratorEvent::EvidenceRecorded { .. }
            | OrchestratorEvent::FinishRejected { .. }
            | OrchestratorEvent::FinishAccepted { .. }
            | OrchestratorEvent::ReviewRoundStarted { .. }
            | OrchestratorEvent::RepairDispatched { .. }
            | OrchestratorEvent::MergeApprovalRequested { .. }
            | OrchestratorEvent::MergeApprovalResolved { .. }
            | OrchestratorEvent::MergeApprovalInvalidated { .. }
            | OrchestratorEvent::MergeExecuted { .. }
            | OrchestratorEvent::CloseoutStepRecorded { .. }
            | OrchestratorEvent::ShellCommandDenied { .. } => false,
        }
    }
}

fn orchestrator_goal_id(event: &OrchestratorEvent) -> Option<&str> {
    match event {
        OrchestratorEvent::GoalCreated { goal_id, .. }
        | OrchestratorEvent::GoalStateChanged { goal_id, .. }
        | OrchestratorEvent::GoalStageChanged { goal_id, .. }
        | OrchestratorEvent::RunAttached { goal_id, .. }
        | OrchestratorEvent::DeliverableBranchBound { goal_id, .. }
        | OrchestratorEvent::EvidenceRecorded { goal_id, .. }
        | OrchestratorEvent::FinishRejected { goal_id, .. }
        | OrchestratorEvent::FinishAccepted { goal_id, .. }
        | OrchestratorEvent::ContinuationDispatched { goal_id, .. }
        | OrchestratorEvent::ContinuationSuppressed { goal_id, .. }
        | OrchestratorEvent::ReviewRoundStarted { goal_id, .. }
        | OrchestratorEvent::RepairDispatched { goal_id, .. }
        | OrchestratorEvent::StallDetected { goal_id, .. }
        | OrchestratorEvent::NudgeSent { goal_id, .. }
        | OrchestratorEvent::MergeApprovalRequested { goal_id, .. }
        | OrchestratorEvent::MergeApprovalResolved { goal_id, .. }
        | OrchestratorEvent::MergeApprovalInvalidated { goal_id, .. }
        | OrchestratorEvent::MergeExecuted { goal_id, .. }
        | OrchestratorEvent::CloseoutStepRecorded { goal_id, .. } => Some(goal_id),
        OrchestratorEvent::ShellCommandDenied { goal_id, .. } => goal_id.as_deref(),
    }
}
