use super::review::ReviewResult;
use crate::{AgentRuntime, RunId};

impl AgentRuntime {
    /// Returns the typed tool submission independently of final assistant text.
    pub fn reviewer_result(&self, run: RunId) -> Option<ReviewResult> {
        self.shared
            .reviewer_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .cloned()
    }

    pub(crate) fn submit_review(&self, run: RunId, result: ReviewResult) {
        self.shared
            .reviewer_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run, result);
    }
}
