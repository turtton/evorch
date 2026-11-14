//! Host service of the `teams` area: a thread's team board, read from the
//! coordinator that still holds it or, once the run is gone, from the persisted
//! snapshot. The service only reads; claiming and editing stay with the agents
//! and with the team configuration's own owner.
use std::collections::BTreeMap;

use event_bus::RecvError;
use event_bus::{EventKind, MessageEvent};
use runtime::AgentRuntime;
use runtime::team::{ClaimState, TeamTask};
use workbench_protocol::errors::{ErrorCode, WorkbenchError};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{RunId, ThreadId};
use workbench_protocol::namespaces::teams::{
    TeamMember, TeamOverview, TeamSource, TeamTaskState, TeamTaskView, TeamsEvent, TeamsQuery,
    TeamsReply,
};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::Revision;

use super::{AreaHandles, ProjectionSource};
use crate::bootstrap::BootstrapError;
use crate::queries::{Pager, error, revision, storage_error, text};

/// The `teams` service, reached from `ingress/namespaces/teams.rs` as
/// `self.areas.teams`.
#[derive(Default)]
pub struct TeamsArea {
    runtime: Option<AgentRuntime>,
    pager: Pager,
}

/// One board as read: where from, who coordinates it, and its tasks.
struct Board {
    source: TeamSource,
    coordinator: Option<RunId>,
    tasks: BTreeMap<String, TeamTask>,
}

impl TeamsArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let area = Self::with_runtime(handles.runtime.clone());
        let source = handles.source("teams")?;
        let runtime = handles.runtime.clone();
        let mut events = handles.bus.subscribe();
        handles
            .tasks
            .spawn("teams-events", move |mut stop| async move {
                let mut published: BTreeMap<String, Revision> = BTreeMap::new();
                let mut threads: BTreeMap<String, Option<ThreadId>> = BTreeMap::new();
                loop {
                    let event = tokio::select! {
                        () = stop.stopped() => break,
                        event = events.recv() => event,
                    };
                    match event {
                        Ok(event) => {
                            if matches!(
                                event.kind,
                                EventKind::Message(
                                    MessageEvent::MessageDelta { .. }
                                        | MessageEvent::ReasoningDelta { .. }
                                        | MessageEvent::ToolCallDelta { .. }
                                )
                            ) {
                                continue;
                            }
                            publish_live_changes(&runtime, &source, &mut published, &mut threads);
                        }
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => break,
                    }
                }
                Ok(())
            })
            .map_err(|error| BootstrapError::from(error.to_string()))?;
        Ok(area)
    }

    /// A service over `runtime` with no event task.
    pub fn with_runtime(runtime: AgentRuntime) -> Self {
        Self {
            runtime: Some(runtime),
            pager: Pager::default(),
        }
    }

    /// Answer a query of the teams namespace.
    ///
    /// # Errors
    /// `NotFound` for an unknown thread, a thread without a team or an unknown
    /// task; `Conflict` for a cursor of another query or snapshot.
    pub fn query(&self, query: &Query, q: &TeamsQuery) -> Result<QueryReply, WorkbenchError> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| error(ErrorCode::NotFound, "the teams service is not wired"))?;
        let reply = match q {
            TeamsQuery::GetTeam(target) => {
                let board = board(runtime, &target.thread)?
                    .ok_or_else(|| error(ErrorCode::NotFound, "this thread has no team"))?;
                TeamsReply::Team(overview(&target.thread, &board))
            }
            TeamsQuery::ListTeamTasks(list) => {
                let tasks = board(runtime, &list.thread)?
                    .map(|board| task_views(&board))
                    .unwrap_or_default();
                let mut key = query.clone();
                if let Query::Teams(TeamsQuery::ListTeamTasks(list)) = &mut key {
                    list.page.cursor = None;
                }
                TeamsReply::Tasks(self.pager.page(
                    &serde_json::to_string(&key).expect("serializable query"),
                    &tasks,
                    &list.page,
                    revision(&tasks),
                )?)
            }
            TeamsQuery::GetTeamTask(reference) => {
                let board = board(runtime, &reference.thread)?
                    .ok_or_else(|| error(ErrorCode::NotFound, "this thread has no team"))?;
                TeamsReply::Task(
                    task_views(&board)
                        .into_iter()
                        .find(|task| task.task_id == reference.task_id)
                        .ok_or_else(|| error(ErrorCode::NotFound, "unknown task"))?,
                )
            }
        };
        Ok(QueryReply::Teams(reply))
    }
}

fn publish_live_changes(
    runtime: &AgentRuntime,
    source: &ProjectionSource,
    published: &mut BTreeMap<String, Revision>,
    threads: &mut BTreeMap<String, Option<ThreadId>>,
) {
    for (run, tasks) in runtime.team_tasks() {
        let key = run.to_string();
        let thread = threads
            .entry(key.clone())
            .or_insert_with(|| {
                runtime
                    .with_storage_reader(|db| db.thread_of_run(&key))
                    .ok()
                    .flatten()
                    .flatten()
                    .and_then(|row| ThreadId::new(row.thread_id).ok())
            })
            .clone();
        let (Some(thread), Ok(coordinator)) = (thread, RunId::new(&key)) else {
            continue;
        };
        let board = Board {
            source: TeamSource::Live,
            coordinator: Some(coordinator),
            tasks: tasks
                .into_iter()
                .map(|task| (task.spec.id.clone(), task))
                .collect(),
        };
        let team = overview(&thread, &board);
        if published.insert(key, team.revision) == Some(team.revision) {
            continue;
        }
        if let Err(error) = source.publish(UiEvent::Teams(TeamsEvent::TeamChanged { team })) {
            tracing::warn!(?error.code, "teams event was not published");
        }
    }
}

/// The board of `thread`: the coordinator's own when the run still holds it,
/// else the last persisted snapshot. `None` when the thread has no team.
fn board(runtime: &AgentRuntime, thread: &ThreadId) -> Result<Option<Board>, WorkbenchError> {
    let thread_str = thread.as_str();
    let live = runtime.team_tasks();
    let read = runtime
        .with_storage_reader(|db| {
            let Some(row) = db.thread(thread_str)? else {
                return Ok(None);
            };
            for (run, tasks) in &live {
                let owner = db.thread_of_run(&run.to_string())?;
                if owner.is_some_and(|owner| owner.thread_id == thread_str) {
                    return Ok(Some(Board {
                        source: TeamSource::Live,
                        coordinator: RunId::new(run.to_string()).ok(),
                        tasks: tasks
                            .iter()
                            .map(|task| (task.spec.id.clone(), task.clone()))
                            .collect(),
                    }));
                }
            }
            let Some(project) = row.project_id else {
                return Ok(Some(Board {
                    source: TeamSource::Persisted,
                    coordinator: None,
                    tasks: BTreeMap::new(),
                }));
            };
            let snapshot = db.team_snapshot(&format!("{project}:{thread_str}"))?;
            Ok(Some(Board {
                source: TeamSource::Persisted,
                coordinator: None,
                tasks: snapshot
                    .and_then(|snapshot| serde_json::from_str(&snapshot.state).ok())
                    .unwrap_or_default(),
            }))
        })
        .map_err(storage_error)?;
    match read {
        None => Err(error(
            ErrorCode::StorageUnavailable,
            "storage is unavailable",
        )),
        Some(None) => Err(error(ErrorCode::NotFound, "unknown thread")),
        Some(Some(board)) if board.tasks.is_empty() && board.coordinator.is_none() => Ok(None),
        Some(Some(board)) => Ok(Some(board)),
    }
}

fn owner_run(owner: &str) -> Option<RunId> {
    owner
        .parse::<runtime::RunId>()
        .ok()
        .and_then(|run| RunId::new(run.to_string()).ok())
}

fn task_views(board: &Board) -> Vec<TeamTaskView> {
    board
        .tasks
        .values()
        .map(|task| {
            let (state, owner, generation) = match &task.state {
                ClaimState::Ready => (TeamTaskState::Ready, None, 0),
                ClaimState::Claimed(lease) => (
                    TeamTaskState::Claimed,
                    Some(lease.owner_id.clone()),
                    lease.generation,
                ),
                ClaimState::Complete => (TeamTaskState::Complete, None, 0),
            };
            let mut view = TeamTaskView {
                task_id: text(&task.spec.id, 256),
                state,
                paths: task
                    .spec
                    .paths
                    .iter()
                    .take(workbench_protocol::namespaces::teams::MAX_TEAM_TASK_PATHS)
                    .map(|path| text(&path.to_string_lossy(), 512))
                    .collect(),
                owner_run: owner.as_deref().and_then(owner_run),
                owner: owner.map(|owner| text(&owner, 256)),
                claim_generation: Revision(generation),
                revision: Revision(0),
            };
            view.revision = revision(&view);
            view
        })
        .collect()
}

fn overview(thread: &ThreadId, board: &Board) -> TeamOverview {
    let tasks = task_views(board);
    let count = |state| tasks.iter().filter(|task| task.state == state).count() as u32;
    let mut claims: BTreeMap<&str, (Option<RunId>, u32)> = BTreeMap::new();
    for task in &tasks {
        if let Some(owner) = &task.owner {
            let entry = claims
                .entry(owner.as_str())
                .or_insert((task.owner_run.clone(), 0));
            entry.1 += 1;
        }
    }
    let mut team = TeamOverview {
        thread: thread.clone(),
        source: board.source,
        coordinator_run: board.coordinator.clone(),
        ready: count(TeamTaskState::Ready),
        claimed: count(TeamTaskState::Claimed),
        complete: count(TeamTaskState::Complete),
        members: claims
            .into_iter()
            .take(workbench_protocol::namespaces::teams::MAX_TEAM_MEMBERS)
            .map(|(owner, (run, claimed_tasks))| TeamMember {
                owner: owner.to_owned(),
                run,
                claimed_tasks,
            })
            .collect(),
        revision: Revision(0),
    };
    team.revision = revision(&(&team, &tasks));
    team
}
