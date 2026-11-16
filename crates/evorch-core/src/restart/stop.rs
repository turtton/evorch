use super::*;
use runtime::quiesce::{RestartAttemptState, StopBoundary, StopDelivery};
use std::sync::Weak;

// Weak ownership avoids runtime -> boundary -> coordinator -> runtime cycles.
pub(super) struct CoreStopBoundary(pub(super) Weak<Inner>);
impl StopBoundary for CoreStopBoundary {
    fn stop(&self, delivery: StopDelivery<'_>) -> Result<(), RuntimeError> {
        let core = RestartCoordinator(self.0.upgrade().ok_or(RuntimeError::RestartBusy)?);
        core.stop_at_boundary(delivery)
    }
}
impl RestartCoordinator {
    fn stop_at_boundary(&self, delivery: StopDelivery<'_>) -> Result<(), RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        if state.phase == RestartPhase::ShuttingDown {
            // Host shutdown also uses Stop. Commit terminal-only admission before
            // delivery, without cancelling/replaying the retained restart attempt.
            return delivery.deliver_for_shutdown();
        }
        // D-2c-2 must begin capture and publish Capturing under this mutex;
        // subsequent Commit I/O stays outside it. Delivery rejects a Stop after
        // runtime's capture boundary, without reopening anything.
        // Do not publish Cancelling before terminal admission accepts the Stop.
        let result = delivery.deliver();
        if let Some(attempt) = state.attempt.clone() {
            let status = self.synchronize_attempt(&mut state, &attempt);
            // A successful Stop cancels preparation even before runtime sealing.
            // After sealing, delivery may already have finished that cancellation.
            if result.is_ok() && status == RestartAttemptState::Active {
                let cancellation = self.0.runtime.cancel_restart_quiesce(&attempt);
                self.synchronize_attempt(&mut state, &attempt);
                cancellation?;
            }
        } else if result.is_ok() && state.phase == RestartPhase::PreparingDrains {
            Self::abort_worker(&mut state);
            // No runtime attempt exists: only our learning/evaluation pauses resume.
            self.reopened(&mut state);
        }
        result
    }
}
