//! Explicit, audited Interrupt of exactly one opaque #710 job. Wait never signals.
use super::*;
use runtime::quiesce::RestartShellJob;
use sha2::{Digest, Sha256};
use std::{future::Future, task::Poll};
use workbench_protocol::namespaces::lifecycle::restart::{
    RestartPhase, ShellBlocker, ShellBlockers,
};
fn opaque(domain: &str, value: &str) -> Result<AdminId, AdminFailure> {
    admin_id(&format!("{domain}.{:x}", Sha256::digest(value.as_bytes())))
}
fn allowed(phase: super::super::RestartPhase) -> bool {
    matches!(
        phase,
        super::super::RestartPhase::PreparingDrains | super::super::RestartPhase::WaitingSafePoint
    )
}
impl PublicRestartCoordinator {
    pub(super) fn shell_blockers(
        &self,
        connection: &VerifiedConnection,
    ) -> Result<ShellBlockers, AdminFailure> {
        let state = self.core.0.state.lock().unwrap();
        if !allowed(state.phase) {
            return Ok(ShellBlockers {
                blockers: vec![],
                total: Revision(0),
                omitted: Revision(0),
            });
        }
        let id = state.id.ok_or_else(conflict)?;
        let Some(attempt) = &state.attempt else {
            return Ok(ShellBlockers {
                blockers: vec![],
                total: Revision(0),
                omitted: Revision(0),
            });
        };
        let jobs = self
            .core
            .0
            .runtime
            .restart_shell_jobs(
                attempt,
                public_id(id)?.as_str(),
                &self.epoch.get().to_string(),
            )
            .map_err(|_| conflict())?;
        let total = jobs.len() as u64;
        let phase = match state.phase {
            super::super::RestartPhase::PreparingDrains => RestartPhase::PreparingDrains,
            _ => RestartPhase::WaitingSafePoint,
        };
        let blockers = jobs
            .into_iter()
            .take(64)
            .map(|job| {
                Ok(ShellBlocker {
                    run: workbench_protocol::ids::RunId::new(job.run_id().to_string())
                        .map_err(|_| failed())?,
                    teardown_confirmed: job.snapshot().teardown_confirmed,
                    observed: job.snapshot().observed,
                    binding: self.job_binding(
                        id,
                        Revision(state.revision),
                        phase,
                        &job,
                        connection,
                    )?,
                })
            })
            .collect::<Result<Vec<_>, AdminFailure>>()?;
        Ok(ShellBlockers {
            omitted: Revision(total - blockers.len() as u64),
            total: Revision(total),
            blockers,
        })
    }
    fn job_binding(
        &self,
        id: RestartId,
        revision: Revision,
        phase: RestartPhase,
        job: &RestartShellJob,
        connection: &VerifiedConnection,
    ) -> Result<InterruptBinding, AdminFailure> {
        Ok(InterruptBinding {
            restart_id: public_id(id)?,
            core_epoch: self.epoch,
            connection_epoch: connection.epoch(),
            run_incarnation: opaque("incarnation", job.incarnation())?,
            blocker_identity: opaque("shell", job.snapshot().identity.id())?,
            revision,
            phase,
        })
    }
    fn select_job(
        &self,
        state: &super::super::State,
        binding: &InterruptBinding,
        connection: &VerifiedConnection,
    ) -> Result<RestartShellJob, AdminFailure> {
        if !allowed(state.phase)
            || state.capture_started
            || binding.core_epoch != self.epoch
            || !connection.matches(binding.connection_epoch)
            || binding.revision.get() != state.revision
        {
            return Err(conflict());
        }
        let phase = match state.phase {
            super::super::RestartPhase::PreparingDrains => RestartPhase::PreparingDrains,
            _ => RestartPhase::WaitingSafePoint,
        };
        if phase != binding.phase {
            return Err(conflict());
        }
        let id = state.id.ok_or_else(conflict)?;
        if public_id(id)? != binding.restart_id {
            return Err(conflict());
        }
        let attempt = state.attempt.as_ref().ok_or_else(conflict)?;
        // Only exact handle equality is accepted. Never search for a replacement job.
        let jobs = self
            .core
            .0
            .runtime
            .restart_shell_jobs(
                attempt,
                binding.restart_id.as_str(),
                &self.epoch.get().to_string(),
            )
            .map_err(|_| conflict())?;
        let mut matches = jobs.into_iter().filter(|job| {
            self.job_binding(id, binding.revision, phase, job, connection)
                .as_ref()
                == Ok(binding)
        });
        let job = matches.next().ok_or_else(conflict)?;
        if matches.next().is_some() {
            return Err(conflict());
        }
        Ok(job)
    }
    pub(super) fn inspect_interrupt(
        &self,
        binding: &InterruptBinding,
        connection: &VerifiedConnection,
    ) -> Result<(), AdminFailure> {
        self.select_job(&self.core.0.state.lock().unwrap(), binding, connection)
            .map(|_| ())
    }
    pub(super) fn commit_interrupt(
        &self,
        binding: InterruptBinding,
        admission: CommitAdmission,
        connection: &VerifiedConnection,
    ) -> Result<AdminAccepted, AdminFailure> {
        let (attempt, job) = {
            let state = self.core.0.state.lock().unwrap();
            let job = self.select_job(&state, &binding, connection)?;
            (state.attempt.as_ref().unwrap().clone(), job)
        };
        let core = self.core.clone();
        let connection = connection.clone();
        let epoch = self.epoch;
        Ok(admission.register(Box::pin(async move {
            let runtime = core.0.runtime.clone();
            let epoch_string = epoch.get().to_string();
            let mut interrupt = Box::pin(runtime.interrupt_restart_shell_job(
                &attempt,
                binding.restart_id.as_str(),
                &epoch_string,
                &job,
            ));
            let mut signalled = false;
            std::future::poll_fn(|cx| {
                if signalled {
                    return interrupt
                        .as_mut()
                        .poll(cx)
                        .map(|r| r.map_err(|_| conflict()));
                }
                let state = core.0.state.lock().unwrap();
                let phase = match state.phase {
                    super::super::RestartPhase::PreparingDrains => RestartPhase::PreparingDrains,
                    super::super::RestartPhase::WaitingSafePoint => RestartPhase::WaitingSafePoint,
                    _ => return Poll::Ready(Err(conflict())),
                };
                if state.capture_started
                    || state.id.and_then(|id| public_id(id).ok()).as_ref()
                        != Some(&binding.restart_id)
                    || state.revision != binding.revision.get()
                    || phase != binding.phase
                    || !connection.matches(binding.connection_epoch)
                    || epoch != binding.core_epoch
                {
                    return Poll::Ready(Err(conflict()));
                }
                // #710 validates run incarnation + opaque job and sends its signal
                // before its first await. First poll is inside the same mutex as
                // cancel/capture; subsequent polls only await terminal reap, unlocked.
                signalled = true;
                interrupt
                    .as_mut()
                    .poll(cx)
                    .map(|r| r.map_err(|_| conflict()))
            })
            .await?;
            Ok(AdminExecutionOutcome::Complete(AdminOutcome::Succeeded))
        })))
    }
}
