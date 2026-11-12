//! Host service of the `execution` area: runs, goals, todo lists and ledgers.
//!
//! The read side composes two sources into one answer. The runtime knows the
//! runs it still holds (with their model and live phase); storage knows every
//! run it ever saved. A run in both is shown once, with the runtime's state; a
//! run only storage remembers stays visible as history. Nothing a client asks
//! for here reaches a provider: the service only reads.
use std::collections::{BTreeMap, BTreeSet};

use event_bus::RecvError;
use event_bus::{AgentRunPhase, EventKind, OrchestratorEvent, ThreadGoalPhase, ThreadTodoStatus};
use runtime::AgentRuntime;
use workbench_protocol::errors::{ErrorCode, WorkbenchError};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{GoalId, RunId, ThreadId};
use workbench_protocol::namespaces::execution::{
    GoalCheckView, GoalPhase, GoalView, LedgerView, MAX_EXECUTION_ITEMS, MAX_ITEM_TEXT_BYTES,
    MAX_LEDGER_BYTES, RunDetail, RunDetailsQuery, RunLifecycle, RunRef, RunScope, RunSource,
    RunTerminalReason, ThreadExecution, TodoItemView, TodoStatus, TodoView,
};
use workbench_protocol::namespaces::{ExecutionEvent, ExecutionQuery, ExecutionReply};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::views::{Page, RunPhase, RunRole, RunSummary};
use workbench_protocol::wire::{Revision, Sequence, UnixMillis};

use super::{AreaHandles, ProjectionSource};
use crate::bootstrap::BootstrapError;
use crate::queries::{Pager, error, revision, storage_error, text};

/// The `execution` service, reached from `ingress/namespaces/execution.rs` as
/// `self.areas.execution`.
#[derive(Default)]
pub struct ExecutionArea {
    runtime: Option<AgentRuntime>,
    pager: Pager,
}

impl ExecutionArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let area = Self::with_runtime(handles.runtime.clone());
        let source = handles.source("execution")?;
        let runtime = handles.runtime.clone();
        let mut events = handles.bus.subscribe();
        handles
            .tasks
            .spawn("execution-events", move |mut stop| async move {
                loop {
                    let event = tokio::select! {
                        () = stop.stopped() => break,
                        event = events.recv() => event,
                    };
                    match event {
                        Ok(event) => publish_change(&runtime, &source, &event.kind),
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => break,
                    }
                }
                Ok(())
            })
            .map_err(|error| BootstrapError::from(error.to_string()))?;
        Ok(area)
    }

    /// A service over `runtime` with no event task: what a test of the read
    /// side needs.
    pub fn with_runtime(runtime: AgentRuntime) -> Self {
        Self {
            runtime: Some(runtime),
            pager: Pager::default(),
        }
    }

    fn runtime(&self) -> Result<&AgentRuntime, WorkbenchError> {
        self.runtime
            .as_ref()
            .ok_or_else(|| error(ErrorCode::NotFound, "the execution service is not wired"))
    }

    /// Answer a query this area owns; `query` is the whole request, which the
    /// paging cursor is checked against.
    ///
    /// # Errors
    /// `NotFound` for an unknown thread or run, `Conflict` for a cursor that
    /// belongs to another query or snapshot, `StorageUnavailable`.
    pub fn query(&self, query: &Query, q: &ExecutionQuery) -> Result<QueryReply, WorkbenchError> {
        let runtime = self.runtime()?;
        let reply = match q {
            ExecutionQuery::ListRuns(_) => {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "answered by the projection",
                ));
            }
            ExecutionQuery::ListRunDetails(q) => {
                ExecutionReply::RunDetails(self.list_run_details(runtime, query, q)?)
            }
            ExecutionQuery::GetRun(RunRef { thread, run }) => {
                let runs = thread_runs(runtime, thread)?;
                ExecutionReply::Run(Box::new(
                    runs.into_iter()
                        .find(|detail| detail.summary.id == *run)
                        .ok_or_else(|| error(ErrorCode::NotFound, "unknown run"))?,
                ))
            }
            ExecutionQuery::GetThreadExecution(target) => ExecutionReply::ThreadExecution(
                Box::new(thread_execution(runtime, &target.thread)?),
            ),
        };
        Ok(QueryReply::Execution(reply))
    }

    fn list_run_details(
        &self,
        runtime: &AgentRuntime,
        query: &Query,
        q: &RunDetailsQuery,
    ) -> Result<Page<RunDetail>, WorkbenchError> {
        let runs = thread_runs(runtime, &q.thread)?;
        let items: Vec<RunDetail> = runs
            .into_iter()
            .filter(|detail| match &q.scope {
                RunScope::Roots => detail.summary.parent_run.is_none(),
                RunScope::Children { parent } => detail.summary.parent_run.as_ref() == Some(parent),
            })
            .collect();
        let mut key = query.clone();
        if let Query::Execution(ExecutionQuery::ListRunDetails(q)) = &mut key {
            q.page.cursor = None;
        }
        self.pager.page(
            &serde_json::to_string(&key).expect("serializable query"),
            &items,
            &q.page,
            revision(&items),
        )
    }
}

/// The thread a run belongs to: the saved binding (or its ancestor's), else the thread named
/// by a live conversation root.
fn thread_of_run(runtime: &AgentRuntime, run_id: &str) -> Option<String> {
    let saved = runtime
        .with_storage_reader(|db| db.thread_of_run(run_id))
        .ok()
        .flatten()
        .flatten()
        .map(|row| row.thread_id);
    saved.or_else(|| {
        runtime
            .list_agents()
            .iter()
            .find(|agent| agent.run_id.to_string() == run_id)
            .and_then(|agent| storage::threads::parse_chat_run_name(&agent.name))
            .map(|(_, thread)| thread.to_owned())
    })
}

fn publish_change(runtime: &AgentRuntime, source: &ProjectionSource, kind: &EventKind) {
    let thread = match kind {
        EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) => {
            snapshot.thread_id.clone()
        }
        EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated { snapshot }) => {
            snapshot.thread_id.clone()
        }
        // A working note changes neither goal nor todo; the thread that owns the run is
        // told that its execution view (which carries the latest note) changed.
        EventKind::Ledger(event_bus::LedgerEvent::RunLedgerAppended { run_id, .. }) => {
            let Some(thread) = thread_of_run(runtime, run_id) else {
                return;
            };
            thread
        }
        _ => return,
    };
    let Ok(thread_id) = ThreadId::new(&thread) else {
        return;
    };
    let event = ExecutionEvent::ThreadExecutionChanged {
        thread: thread_id,
        goal: runtime
            .thread_goal(&thread)
            .and_then(|goal| goal_view(&goal)),
        todo: runtime.thread_todo(&thread).map(|todo| todo_view(&todo)),
    };
    if let Err(error) = source.publish(UiEvent::Execution(event)) {
        tracing::warn!(?error.code, "execution event was not published");
    }
}

fn run_phase(phase: AgentRunPhase) -> RunPhase {
    match phase {
        AgentRunPhase::Pending => RunPhase::Pending,
        AgentRunPhase::Running => RunPhase::Running,
        AgentRunPhase::Waiting => RunPhase::WaitingForUser,
        AgentRunPhase::Stopped => RunPhase::Cancelled,
        AgentRunPhase::Done => RunPhase::Done,
        AgentRunPhase::Error => RunPhase::Failed,
    }
}

fn run_role(value: &str) -> RunRole {
    match value.to_ascii_lowercase().as_str() {
        "orchestrator" => RunRole::Orchestrator,
        "worker" => RunRole::Worker,
        "planner" => RunRole::Planner,
        "reviewer" => RunRole::Reviewer,
        "advisor" | "oracle" => RunRole::Advisor,
        _ => RunRole::Other,
    }
}

/// What a saved run's last recorded phase says, and whether the run is waiting
/// for recovery: saved as active, yet not running here.
fn saved_state(value: &str) -> (RunPhase, RunLifecycle, Option<RunTerminalReason>) {
    match value.to_ascii_lowercase().as_str() {
        "pending" => (RunPhase::Pending, RunLifecycle::Recovering, None),
        "running" => (RunPhase::Running, RunLifecycle::Recovering, None),
        "waiting" => (RunPhase::WaitingForUser, RunLifecycle::Recovering, None),
        "done" => (
            RunPhase::Done,
            RunLifecycle::Terminal,
            Some(RunTerminalReason::Completed),
        ),
        "stopped" => (
            RunPhase::Cancelled,
            RunLifecycle::Stopped,
            Some(RunTerminalReason::Stopped),
        ),
        _ => (
            RunPhase::Failed,
            RunLifecycle::Terminal,
            Some(RunTerminalReason::Failed),
        ),
    }
}

fn live_state(phase: AgentRunPhase) -> (RunLifecycle, Option<RunTerminalReason>) {
    match phase {
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {
            (RunLifecycle::Active, None)
        }
        AgentRunPhase::Done => (RunLifecycle::Terminal, Some(RunTerminalReason::Completed)),
        AgentRunPhase::Error => (RunLifecycle::Terminal, Some(RunTerminalReason::Failed)),
        AgentRunPhase::Stopped => (RunLifecycle::Stopped, Some(RunTerminalReason::Stopped)),
    }
}

/// Every run of `thread`, saved and live together, ordered by run id (which
/// orders by creation). Runs only storage remembers are kept.
///
/// # Errors
/// `NotFound` when neither storage nor the runtime knows the thread.
pub(crate) fn thread_runs(
    runtime: &AgentRuntime,
    thread: &ThreadId,
) -> Result<Vec<RunDetail>, WorkbenchError> {
    let thread_str = thread.as_str();
    let live = runtime.list_agents();
    let read = runtime
        .with_storage_reader(|db| {
            let known = db.thread(thread_str)?.is_some();
            let mut details: BTreeMap<String, RunDetail> = BTreeMap::new();
            let rows = db.runs_of_thread(thread_str)?;
            let ids: Vec<String> = rows.iter().map(|row| row.run_id.clone()).collect();
            let deleted: BTreeSet<String> = db.tombstoned_runs(&ids)?.into_iter().collect();
            for row in rows.iter().filter(|row| !deleted.contains(&row.run_id)) {
                let Some(meta) = db.run_context_meta(&row.run_id)? else {
                    continue;
                };
                let (phase, lifecycle, terminal_reason) = saved_state(&meta.terminal_phase);
                let (Ok(id), Ok(thread)) = (RunId::new(&row.run_id), ThreadId::new(thread_str))
                else {
                    continue;
                };
                details.insert(
                    row.run_id.clone(),
                    detail(
                        RunSummary {
                            id,
                            thread,
                            parent_run: meta.parent_run_id.and_then(|id| RunId::new(id).ok()),
                            role: run_role(&meta.role),
                            phase,
                            revision: Revision(0),
                        },
                        &meta.name,
                        lifecycle,
                        RunSource::HistoryOnly,
                        None,
                        None,
                        terminal_reason,
                        Some(UnixMillis((meta.updated_at_ns.max(0) / 1_000_000) as u64)),
                    ),
                );
            }
            for agent in &live {
                let id = agent.run_id.to_string();
                if !db.tombstoned_runs(std::slice::from_ref(&id))?.is_empty() {
                    continue;
                }
                let owner = db.thread_of_run(&id)?.map(|row| row.thread_id).or_else(|| {
                    storage::threads::parse_chat_run_name(&agent.name)
                        .map(|(_, thread)| thread.to_owned())
                });
                if owner.as_deref() != Some(thread_str) {
                    continue;
                }
                let (lifecycle, terminal_reason) = live_state(agent.phase);
                let (Ok(run), Ok(thread)) = (RunId::new(&id), ThreadId::new(thread_str)) else {
                    continue;
                };
                let updated_at = details.get(&id).and_then(|saved| saved.updated_at);
                details.insert(
                    id,
                    detail(
                        RunSummary {
                            id: run,
                            thread,
                            parent_run: agent
                                .parent_run_id
                                .and_then(|parent| RunId::new(parent.to_string()).ok()),
                            role: run_role(&agent.role_name),
                            phase: run_phase(agent.phase),
                            revision: Revision(0),
                        },
                        &agent.name,
                        lifecycle,
                        RunSource::Live,
                        Some(agent.model.clone()),
                        agent.category.clone(),
                        terminal_reason,
                        updated_at,
                    ),
                );
            }
            Ok((known, details))
        })
        .map_err(storage_error)?;
    let Some((known, details)) = read else {
        return Err(error(
            ErrorCode::StorageUnavailable,
            "storage is unavailable",
        ));
    };
    if !known && details.is_empty() {
        return Err(error(ErrorCode::NotFound, "unknown thread"));
    }
    let mut runs: Vec<RunDetail> = details.into_values().collect();
    let mut children: BTreeMap<RunId, u32> = BTreeMap::new();
    for run in &runs {
        if let Some(parent) = &run.summary.parent_run {
            *children.entry(parent.clone()).or_default() += 1;
        }
    }
    for run in &mut runs {
        run.child_count = children.get(&run.summary.id).copied().unwrap_or(0);
        run.revision = Revision(0);
        run.revision = revision(run);
    }
    Ok(runs)
}

#[allow(clippy::too_many_arguments)]
fn detail(
    mut summary: RunSummary,
    name: &str,
    lifecycle: RunLifecycle,
    source: RunSource,
    model: Option<String>,
    category: Option<String>,
    terminal_reason: Option<RunTerminalReason>,
    updated_at: Option<UnixMillis>,
) -> RunDetail {
    summary.revision = revision(&summary);
    RunDetail {
        summary,
        name: text(name, 256),
        lifecycle,
        source,
        model: model.map(|model| text(&model, 256)),
        category: category.map(|category| text(&category, 256)),
        terminal_reason,
        child_count: 0,
        updated_at,
        revision: Revision(0),
    }
}

fn goal_view(goal: &event_bus::ThreadGoalSnapshot) -> Option<GoalView> {
    let bounded = |items: &[String]| -> Vec<String> {
        items
            .iter()
            .take(MAX_EXECUTION_ITEMS)
            .map(|item| text(item, MAX_ITEM_TEXT_BYTES))
            .collect()
    };
    Some(GoalView {
        goal: GoalId::new(&goal.goal_id).ok()?,
        root_run: RunId::new(&goal.root_run_id).ok()?,
        objective: text(
            &goal.objective,
            workbench_protocol::limits::MAX_OBJECTIVE_BYTES,
        ),
        criteria: bounded(&goal.criteria),
        checks: goal
            .checks
            .iter()
            .take(MAX_EXECUTION_ITEMS)
            .map(|check| GoalCheckView {
                criterion: u32::try_from(check.criterion).unwrap_or(u32::MAX),
                met: check.met,
                evidence: text(&check.evidence, MAX_ITEM_TEXT_BYTES),
            })
            .collect(),
        phase: match goal.phase {
            ThreadGoalPhase::Working => GoalPhase::Working,
            ThreadGoalPhase::Checking => GoalPhase::Checking,
            ThreadGoalPhase::Reviewing => GoalPhase::Reviewing,
            ThreadGoalPhase::Repairing => GoalPhase::Repairing,
            ThreadGoalPhase::Complete => GoalPhase::Complete,
            ThreadGoalPhase::Blocked => GoalPhase::Blocked,
        },
        review_enabled: goal.review_enabled,
        checks_paused: goal.checks_paused,
        work_stopped: goal.work_stopped,
        review_round: goal.review_round,
        max_review_rounds: goal.max_review_rounds,
        findings: bounded(&goal.findings),
        reason: goal
            .reason
            .as_deref()
            .map(|reason| text(reason, MAX_ITEM_TEXT_BYTES)),
        epoch: Revision(goal.epoch),
    })
}

fn todo_view(todo: &event_bus::ThreadTodoSnapshot) -> TodoView {
    TodoView {
        list_id: text(&todo.list_id, 256),
        revision: Revision(todo.revision),
        items: todo
            .items
            .iter()
            .take(MAX_EXECUTION_ITEMS)
            .map(|item| TodoItemView {
                content: text(&item.content, MAX_ITEM_TEXT_BYTES),
                status: match item.status {
                    ThreadTodoStatus::Pending => TodoStatus::Pending,
                    ThreadTodoStatus::InProgress => TodoStatus::InProgress,
                    ThreadTodoStatus::Completed => TodoStatus::Completed,
                    ThreadTodoStatus::Interrupted => TodoStatus::Interrupted,
                },
            })
            .collect(),
    }
}

fn thread_execution(
    runtime: &AgentRuntime,
    thread: &ThreadId,
) -> Result<ThreadExecution, WorkbenchError> {
    let runs = thread_runs(runtime, thread)?;
    let root = runs
        .iter()
        .rev()
        .find(|run| run.summary.parent_run.is_none())
        .map(|run| run.summary.id.clone());
    let ledger = match &root {
        Some(root) => runtime
            .with_storage_reader(|db| db.latest_run_ledger(root.as_str()))
            .map_err(storage_error)?
            .flatten()
            .map(|entry| {
                let body = text(&entry.body, MAX_LEDGER_BYTES);
                LedgerView {
                    run: root.clone(),
                    seq: Sequence(entry.seq),
                    truncated: body.len() < entry.body.len(),
                    body,
                }
            }),
        None => None,
    };
    Ok(ThreadExecution {
        thread: thread.clone(),
        root_run: root,
        goal: runtime
            .thread_goal(thread.as_str())
            .and_then(|goal| goal_view(&goal)),
        todo: runtime
            .thread_todo(thread.as_str())
            .map(|todo| todo_view(&todo)),
        ledger,
    })
}
