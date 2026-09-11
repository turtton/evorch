use crate::{RunId, team::TeamBoard};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct TeamContext {
    pub coordinator: RunId,
    pub id: String,
    pub board: Arc<TeamBoard>,
    workers: Arc<Semaphore>,
    pub writer: Option<storage::StorageHandle>,
}

pub(crate) fn monitor(
    runtime: std::sync::Weak<crate::runtime::Shared>,
    team: TeamContext,
    run: RunId,
    phase: tokio::sync::watch::Receiver<event_bus::AgentRunPhase>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            let Some(runtime) = crate::AgentRuntime::from_weak(&runtime) else {
                return;
            };
            let now = team.now();
            match team.board.expire(now) {
                Ok(expired) if !expired.is_empty() => {
                    let _ = runtime.send_message(
                        team.coordinator,
                        format!("Team leases expired; tasks ready: {}", expired.join(", ")),
                    );
                }
                Ok(_) => {}
                Err(_) => return,
            }
            let Ok(tasks) = team.board.snapshot() else {
                return;
            };
            let owns_claim = tasks.iter().any(|task| matches!(&task.state, crate::team::ClaimState::Claimed(lease) if lease.owner_id == run.to_string()));
            if matches!(
                *phase.borrow(),
                event_bus::AgentRunPhase::Done | event_bus::AgentRunPhase::Error
            ) && !owns_claim
            {
                return;
            }
            if *phase.borrow() == event_bus::AgentRunPhase::Running {
                for task in tasks {
                    if let crate::team::ClaimState::Claimed(lease) = task.state
                        && lease.owner_id == run.to_string()
                    {
                        let _ = team.board.heartbeat(&task.spec.id, &lease, now);
                    }
                }
            }
        }
    })
}

impl PartialEq for TeamContext {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.board, &other.board)
    }
}
impl Eq for TeamContext {}

impl TeamContext {
    pub fn new(coordinator: RunId, limit: u8) -> Self {
        Self {
            coordinator,
            id: coordinator.to_string(),
            board: Arc::new(TeamBoard::default()),
            workers: Arc::new(Semaphore::new(usize::from(limit.clamp(1, 3)))),
            writer: None,
        }
    }

    pub fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|value| u64::try_from(value.as_millis()).ok())
            .unwrap_or(u64::MAX)
    }

    pub fn persistent(
        coordinator: RunId,
        store: &TeamStore,
        limit: u8,
    ) -> Result<Self, crate::team::TeamError> {
        let snapshot = storage::Database::open(&store.config)
            .and_then(|db| db.team_snapshot(&store.id))
            .map_err(|error| crate::team::TeamError::Persistence(error.to_string()))?;
        let board = match snapshot {
            Some(snapshot) => TeamBoard::restore(store.writer.clone(), snapshot)?,
            None => TeamBoard::durable(store.writer.clone(), store.id.clone()),
        };
        Ok(Self {
            coordinator,
            id: store.id.clone(),
            board: Arc::new(board),
            workers: Arc::new(Semaphore::new(usize::from(limit.clamp(1, 3)))),
            writer: Some(store.writer.clone()),
        })
    }

    pub(crate) fn reserve_worker(&self) -> Result<OwnedSemaphorePermit, String> {
        self.workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| "team worker limit reached".into())
    }
}

#[derive(Debug, Clone)]
pub struct TeamStore {
    pub config: storage::StorageConfig,
    pub writer: storage::StorageHandle,
    pub id: String,
}

impl PartialEq for TeamStore {
    fn eq(&self, other: &Self) -> bool {
        self.config.db_path == other.config.db_path && self.id == other.id
    }
}
impl Eq for TeamStore {}
