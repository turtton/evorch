use super::super::{AgentRuntime, RunId, lock_runs};

impl AgentRuntime {
    /// Whether consumed input has not yet reached its published execution phase.
    /// Observation consumes no input, completion notice or receipt.
    pub fn is_adopting_user_input(&self, run: RunId) -> bool {
        lock_runs(&self.shared.runs)
            .get(&run)
            .is_some_and(|entry| entry.user_inbox.adopting())
    }

    /// Hold the next claim after consumption but before its execution phase.
    /// This runtime/run's gate is absent from product builds. Claim is synchronous:
    /// drive it on a separate worker/thread, then send or drop `resume` to release.
    #[cfg(any(test, feature = "idle-test-support"))]
    pub fn gate_next_user_input_adoption(
        &self,
        run: RunId,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> Result<(), crate::RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        let entry = runs
            .get(&run)
            .ok_or_else(|| super::super::unknown_run(run))?;
        entry.user_inbox.gate_next_adoption(reached, resume);
        Ok(())
    }
}
