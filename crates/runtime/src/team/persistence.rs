use super::{TeamBoard, TeamError, TeamTask};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug)]
pub(super) struct Persistence {
    pub writer: storage::StorageHandle,
    pub id: String,
    pub revision: Mutex<u64>,
}

impl TeamBoard {
    pub fn restore(
        writer: storage::StorageHandle,
        snapshot: storage::team::TeamSnapshot,
    ) -> Result<Self, TeamError> {
        let tasks: BTreeMap<String, TeamTask> = serde_json::from_str(&snapshot.state)
            .map_err(|error| TeamError::Persistence(error.to_string()))?;
        for (id, task) in &tasks {
            if id != &task.spec.id || id.trim().is_empty() {
                return Err(TeamError::NotReady);
            }
            for path in &task.spec.paths {
                super::validate_path(path)?;
            }
        }
        Ok(Self {
            tasks: Mutex::new(tasks),
            persistence: Some(Persistence {
                writer,
                id: snapshot.team_id,
                revision: Mutex::new(snapshot.revision),
            }),
        })
    }

    pub fn durable(writer: storage::StorageHandle, id: String) -> Self {
        Self {
            tasks: Mutex::default(),
            persistence: Some(Persistence {
                writer,
                id,
                revision: Mutex::new(0),
            }),
        }
    }

    pub(super) fn update<T>(
        &self,
        action: impl FnOnce(&mut BTreeMap<String, TeamTask>) -> Result<T, TeamError>,
    ) -> Result<T, TeamError> {
        let mut current = self.tasks.lock().map_err(|_| TeamError::Poisoned)?;
        let mut next = current.clone();
        let result = action(&mut next)?;
        if next != *current {
            if let Some(store) = &self.persistence {
                let mut revision = store.revision.lock().map_err(|_| TeamError::Poisoned)?;
                let next_revision = revision.checked_add(1).ok_or(TeamError::Exhausted)?;
                store
                    .writer
                    .append_team_snapshot(storage::team::TeamSnapshot {
                        team_id: store.id.clone(),
                        revision: next_revision,
                        state: serde_json::to_string(&next)
                            .map_err(|error| TeamError::Persistence(error.to_string()))?,
                    })
                    .map_err(|error| TeamError::Persistence(error.to_string()))?;
                *revision = next_revision;
            }
            *current = next;
        }
        Ok(result)
    }
}
