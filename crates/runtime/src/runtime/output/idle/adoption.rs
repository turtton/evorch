use super::super::{AgentRuntime, RunId, lock_runs};

impl AgentRuntime {
    /// Recheck retention on release even without a phase change. Notifications
    /// coalesce and may be spurious; the single lifecycle observer re-reads state.
    pub fn input_adoption_notifier(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.shared.input_adoption_changed.clone()
    }

    /// Whether an input wake or consumed input awaits its published execution phase.
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

    /// Hold the next resumed input after handling, before publishing Running.
    /// This covers every interactive wake, including mailbox and answer delivery.
    /// The synchronous test gate holds no runtime/inbox/observation lock; drive
    /// the run on another worker/thread and send or drop `resume` to release it.
    #[cfg(any(test, feature = "idle-test-support"))]
    pub fn gate_next_input_wake_adoption(
        &self,
        run: RunId,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> Result<(), crate::RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        let entry = runs
            .get(&run)
            .ok_or_else(|| super::super::unknown_run(run))?;
        entry.user_inbox.gate_next_wake_adoption(reached, resume);
        Ok(())
    }

    /// Generate a message-free mailbox version wake and hold it after its no-op
    /// handler, before releasing adoption. Arrange a passive run first.
    #[cfg(any(test, feature = "idle-test-support"))]
    pub fn gate_ignored_mailbox_wake(
        &self,
        run: RunId,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> Result<(), crate::RuntimeError> {
        let mailbox = {
            let runs = lock_runs(&self.shared.runs);
            let entry = runs
                .get(&run)
                .ok_or_else(|| super::super::unknown_run(run))?;
            entry.user_inbox.gate_ignored_wake_adoption(reached, resume);
            entry.mailbox.clone()
        };
        // An empty drain advances the real watch without consuming any input.
        mailbox.drain_where(|_| false);
        Ok(())
    }

    /// Hold a truly passive poll AFTER its transient guard is released. The
    /// message-free watch kick makes this deterministic for already-waiting runs.
    /// No runtime/inbox lock is held while either test gate blocks.
    #[cfg(any(test, feature = "idle-test-support"))]
    pub fn gate_next_passive_input_wait(
        &self,
        run: RunId,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> Result<(), crate::RuntimeError> {
        let mailbox = {
            let runs = lock_runs(&self.shared.runs);
            let entry = runs
                .get(&run)
                .ok_or_else(|| super::super::unknown_run(run))?;
            entry.user_inbox.gate_passive_wait(reached, resume);
            entry.mailbox.clone()
        };
        mailbox.drain_where(|_| false);
        Ok(())
    }
}
