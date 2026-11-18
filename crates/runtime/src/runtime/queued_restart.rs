//! Process-local launch ownership. Never reconstruct a continuation from a recipe.
use super::*;

pub(in crate::runtime) struct QueuedLaunch {
    pub(in crate::runtime) parent: Option<RunId>,
    pub(in crate::runtime) role: Role,
    pub(in crate::runtime) prompt: String,
    pub(in crate::runtime) config: RunConfig,
    pub(in crate::runtime) continuation: RunContinuation,
    pub(in crate::runtime) review_seed: Option<ReviewSeed>,
}

impl AgentRuntime {
    /// I excludes insertion/replacement across durable invalidation and claiming.
    /// The closure must finish every durable update before granting live resumes.
    pub(crate) fn with_restart_launch_fence<T>(
        &self,
        grant: impl FnOnce() -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        let intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let receipt = match grant() {
            Ok(receipt) => receipt,
            Err(error) => {
                let failed = {
                    let mut admissions = self
                        .shared
                        .admissions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    admissions
                        .iter_mut()
                        .filter_map(|(run, admission)| {
                            admission
                                .queued
                                .take()
                                .map(|launch| (*run, Arc::clone(&admission.execution), launch))
                        })
                        .collect::<Vec<_>>()
                };
                drop(intents);
                for (run, execution, launch) in failed {
                    self.end_intent(
                        run,
                        &execution.publication().incarnation(),
                        crate::quiesce::StopReason::Terminal,
                    );
                    self.fail_reserved_admission(run, &execution, error.to_string());
                    drop(launch);
                    self.release_scratch_execution(run, &execution.publication().incarnation());
                }
                return Err(error);
            }
        };
        let launches = {
            let mut admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            admissions
                .iter_mut()
                .filter_map(|(run, admission)| {
                    admission
                        .queued
                        .take()
                        .map(|launch| (*run, Arc::clone(&admission.execution), launch))
                })
                .collect::<Vec<_>>()
        };
        let mut failure = None;
        let mut discarded = Vec::new();
        let mut unstarted = Vec::new();
        for (run, execution, launch) in launches {
            let current = intents
                .get(&run)
                .and_then(|intent| intent.execution.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, &execution));
            if !current || execution.cancelled() {
                self.fail_reserved_admission(
                    run,
                    &execution,
                    "queued restart launch was cancelled or replaced".into(),
                );
                discarded.push(launch);
                unstarted.push((run, execution.publication().incarnation()));
                continue;
            }
            let QueuedLaunch {
                parent,
                role,
                prompt,
                config,
                continuation,
                review_seed,
            } = launch;
            if self.shared.model_for(&config).requires_admission() {
                if let Err(error) = self.admit_run(
                    run,
                    parent,
                    role,
                    prompt,
                    config,
                    continuation,
                    review_seed,
                    Arc::clone(&execution),
                    &intents,
                ) {
                    self.end_intent(
                        run,
                        &execution.publication().incarnation(),
                        crate::quiesce::StopReason::Terminal,
                    );
                    self.fail_reserved_admission(run, &execution, error.to_string());
                    unstarted.push((run, execution.publication().incarnation()));
                    failure.get_or_insert(error);
                }
            } else {
                self.register_without_admission(
                    run,
                    parent,
                    role,
                    prompt,
                    config,
                    continuation,
                    review_seed,
                    execution,
                    &intents,
                );
            }
        }
        drop(intents);
        drop(discarded);
        for (run, incarnation) in unstarted {
            self.release_scratch_execution(run, &incarnation);
        }
        failure.map_or(Ok(receipt), Err)
    }

    pub(crate) fn discard_cancelled_restart_launches(&self) {
        let discarded = {
            let mut admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            admissions
                .iter_mut()
                .filter(|(_, admission)| admission.cancelled || admission.result.borrow().is_some())
                .filter_map(|(run, admission)| {
                    admission
                        .queued
                        .take()
                        .map(|launch| (*run, Arc::clone(&admission.execution), launch))
                })
                .collect::<Vec<_>>()
        };
        // Handoff owns a live reservation; destruction must be outside I/A/R.
        for (run, execution, launch) in discarded {
            drop(launch);
            // A queued launch has never reached LoopState finalization. Only
            // this proven-unstarted execution may release its own identity.
            self.release_scratch_execution(run, &execution.publication().incarnation());
        }
    }
}

#[cfg(test)]
#[path = "queued_restart_scratch_tests.rs"]
mod scratch_tests;
