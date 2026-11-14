//! Snapshot and Wait never acknowledge results or send a process signal.
use super::*;
use crate::{ShellJobIdentity, ShellJobSnapshot, ShellJobState};

impl JobRegistry {
    pub(in crate::tools::shell) fn snapshots(&self, owner: &str) -> Vec<ShellJobSnapshot> {
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut snapshots: Vec<_> = jobs
            .values()
            .filter(|job| job.owner == owner)
            .map(|job| job.observation())
            .collect();
        snapshots.sort_by(|a, b| a.identity.id.cmp(&b.identity.id));
        snapshots
    }

    pub(in crate::tools::shell) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(in crate::tools::shell) async fn wait_terminal(
        &self,
        identity: &ShellJobIdentity,
    ) -> Result<ShellJobSnapshot, ToolError> {
        let job = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&identity.id)
            .filter(|job| job.owner == identity.owner)
            .cloned()
            .ok_or_else(|| invalid("shell job identity is no longer available"))?;
        let mut changes = job.changed.subscribe();
        loop {
            let snapshot = job.observation();
            if snapshot.state == ShellJobState::Terminal {
                return Ok(snapshot);
            }
            changes
                .changed()
                .await
                .map_err(|_| io_failed("shell completion notification closed"))?;
        }
    }
}

impl Job {
    fn observation(&self) -> ShellJobSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ShellJobSnapshot {
            identity: ShellJobIdentity {
                id: self.id.clone(),
                owner: self.owner.clone(),
            },
            owner_run: self.owner.clone(),
            call_id: self.call_id.clone(),
            state: if state.completion.is_some() {
                ShellJobState::Terminal
            } else {
                ShellJobState::Running
            },
            exit_code: state.completion.as_ref().and_then(|c| c.exit_code),
            observed: state.observed,
        }
    }

    pub(super) fn observation_changed(&self) {
        self.registry_changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}
