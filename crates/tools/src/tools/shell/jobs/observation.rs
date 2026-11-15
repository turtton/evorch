//! Snapshot and Wait never acknowledge results or send a process signal.
use super::*;
use crate::{ShellJobIdentity, ShellJobSnapshot, ShellJobState};

impl JobRegistry {
    fn exact_job(&self, identity: &ShellJobIdentity) -> Result<Arc<Job>, ToolError> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&identity.id)
            .filter(|job| job.owner == identity.owner)
            .cloned()
            .ok_or_else(|| {
                invalid("shell job identity is no longer available; commands are never replayed")
            })
    }
    pub(in crate::tools::shell) fn interrupt(
        &self,
        identity: &ShellJobIdentity,
    ) -> Result<(), ToolError> {
        self.exact_job(identity)?.cancel_now();
        Ok(())
    }
    pub(in crate::tools::shell) fn terminal_result(
        &self,
        identity: &ShellJobIdentity,
    ) -> Result<crate::ShellJobResult, ToolError> {
        let job = self.exact_job(identity)?;
        let mut state = job
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.completion.is_none() || !state.teardown_confirmed {
            return Err(invalid("shell process teardown is not confirmed"));
        }
        let snapshot = job.observation_locked(&state);
        let (result, _) = job.snapshot_locked(&mut state, 0, false, false);
        Ok(crate::ShellJobResult { snapshot, result })
    }
    pub(in crate::tools::shell) fn acknowledge_result(
        &self,
        identity: &ShellJobIdentity,
    ) -> Result<(), ToolError> {
        let job = self.exact_job(identity)?;
        let mut state = job
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.completion.is_none() || !state.teardown_confirmed {
            return Err(invalid("cannot acknowledge an unreaped shell job"));
        }
        state.observed = true;
        state.completion_notified = true;
        job.observation_changed();
        Ok(())
    }

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
                self.terminal_result(identity)?;
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
        self.observation_locked(&state)
    }

    fn observation_locked(&self, state: &State) -> ShellJobSnapshot {
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
            teardown_confirmed: state.teardown_confirmed,
        }
    }

    pub(super) fn observation_changed(&self) {
        self.registry_changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}
