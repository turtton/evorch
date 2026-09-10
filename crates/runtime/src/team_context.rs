use crate::{RunId, team::TeamBoard};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct TeamContext {
    pub coordinator: RunId,
    pub board: Arc<TeamBoard>,
    workers: Arc<Semaphore>,
    clock: Instant,
    pub finding_store: Option<std::path::PathBuf>,
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
            board: Arc::new(TeamBoard::default()),
            workers: Arc::new(Semaphore::new(usize::from(limit.clamp(1, 3)))),
            clock: Instant::now(),
            finding_store: None,
        }
    }

    pub fn now(&self) -> u64 {
        u64::try_from(self.clock.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn reserve_worker(&self) -> Result<OwnedSemaphorePermit, String> {
        self.workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| "team worker limit reached".into())
    }
}
