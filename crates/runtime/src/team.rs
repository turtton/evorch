use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use crate::ownership::{Lease, OwnerState};
use serde::{Deserialize, Serialize};
mod persistence;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub id: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimState {
    Ready,
    Claimed(Lease),
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTask {
    pub spec: TaskSpec,
    pub state: ClaimState,
    generation: u64,
}

#[derive(Debug, Default)]
pub struct TeamBoard {
    tasks: Mutex<BTreeMap<String, TeamTask>>,
    persistence: Option<persistence::Persistence>,
}

#[derive(Debug, thiserror::Error)]
pub enum TeamError {
    #[error("team persistence failed: {0}")]
    Persistence(String),
    #[error("team state lock is poisoned")]
    Poisoned,
    #[error("task is missing, duplicated, or not ready")]
    NotReady,
    #[error("task lease expired or owner was fenced")]
    Fenced,
    #[error("artifact paths overlap with another active task")]
    Conflict,
    #[error("artifact path must be a non-empty relative path without parent traversal")]
    InvalidPath,
    #[error("task generation exhausted")]
    Exhausted,
}

impl TeamBoard {
    pub fn enqueue(&self, spec: TaskSpec) -> Result<(), TeamError> {
        if spec.id.trim().is_empty() {
            return Err(TeamError::NotReady);
        }
        for path in &spec.paths {
            validate_path(path)?;
        }
        self.update(|tasks| {
            if tasks.contains_key(&spec.id) {
                return Err(TeamError::NotReady);
            }
            tasks.insert(
                spec.id.clone(),
                TeamTask {
                    spec,
                    state: ClaimState::Ready,
                    generation: 0,
                },
            );
            Ok(())
        })
    }

    pub fn claim(&self, id: &str, worker: &str, now: u64) -> Result<Lease, TeamError> {
        self.update(|tasks| {
            let task = tasks.get(id).ok_or(TeamError::NotReady)?;
            if task.state != ClaimState::Ready {
                return Err(TeamError::NotReady);
            }
            for other in tasks.values() {
                if matches!(other.state, ClaimState::Claimed(_))
                    && task.spec.paths.iter().any(|path| {
                        other
                            .spec
                            .paths
                            .iter()
                            .any(|owned| path.starts_with(owned) || owned.starts_with(path))
                    })
                {
                    return Err(TeamError::Conflict);
                }
            }
            let task = tasks.get_mut(id).ok_or(TeamError::NotReady)?;
            task.generation = task.generation.checked_add(1).ok_or(TeamError::Exhausted)?;
            let lease = Lease {
                owner_id: worker.into(),
                generation: task.generation,
                expires_at: now.saturating_add(5_000),
            };
            task.state = ClaimState::Claimed(lease.clone());
            Ok(lease)
        })
    }

    pub fn heartbeat(&self, id: &str, token: &Lease, now: u64) -> Result<(), TeamError> {
        self.update(|tasks| {
            let task = tasks.get_mut(id).ok_or(TeamError::NotReady)?;
            let lease = live_lease(task, token, now)?;
            lease.expires_at = now.saturating_add(5_000);
            Ok(())
        })
    }

    pub fn complete(&self, id: &str, token: &Lease, now: u64) -> Result<(), TeamError> {
        self.update(|tasks| {
            let task = tasks.get_mut(id).ok_or(TeamError::NotReady)?;
            live_lease(task, token, now)?;
            task.state = ClaimState::Complete;
            Ok(())
        })
    }

    pub fn expire(&self, now: u64) -> Result<Vec<String>, TeamError> {
        self.update(|tasks| {
            let mut expired = Vec::new();
            for (id, task) in tasks.iter_mut() {
                if let ClaimState::Claimed(lease) = &task.state
                    && lease.observe(now, 0) == OwnerState::Stale
                {
                    task.state = ClaimState::Ready;
                    expired.push(id.clone());
                }
            }
            Ok(expired)
        })
    }

    pub fn snapshot(&self) -> Result<Vec<TeamTask>, TeamError> {
        Ok(self
            .tasks
            .lock()
            .map_err(|_| TeamError::Poisoned)?
            .values()
            .cloned()
            .collect())
    }

    pub fn authorize_path(&self, worker: &str, path: &Path, now: u64) -> Result<(), TeamError> {
        validate_path(path)?;
        let mut prefix = PathBuf::new();
        for component in path.components() {
            prefix.push(component);
            match std::fs::symlink_metadata(&prefix) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(TeamError::InvalidPath);
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(TeamError::InvalidPath),
            }
        }
        let tasks = self.tasks.lock().map_err(|_| TeamError::Poisoned)?;
        for task in tasks.values() {
            if let ClaimState::Claimed(lease) = &task.state
                && lease.owner_id == worker
                && lease.observe(now, 0) == OwnerState::Running
                && task.spec.paths.iter().any(|owned| path.starts_with(owned))
            {
                return Ok(());
            }
        }
        Err(TeamError::Fenced)
    }
}

fn live_lease<'a>(
    task: &'a mut TeamTask,
    token: &Lease,
    now: u64,
) -> Result<&'a mut Lease, TeamError> {
    match &mut task.state {
        ClaimState::Claimed(lease)
            if lease.owner_id == token.owner_id
                && lease.generation == token.generation
                && lease.observe(now, 0) == OwnerState::Running =>
        {
            Ok(lease)
        }
        ClaimState::Claimed(_) | ClaimState::Ready | ClaimState::Complete => Err(TeamError::Fenced),
    }
}

fn validate_path(path: &Path) -> Result<(), TeamError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(TeamError::InvalidPath);
    }
    Ok(())
}
