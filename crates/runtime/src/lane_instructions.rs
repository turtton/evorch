//! A lane lead's supplements and brief revisions (ADR 0044 §5). The caller is the
//! dispatcher-supplied run, never a model-supplied ID. Acceptance is one storage commit;
//! the lane conversation receives the text later, at a safe boundary, through the
//! durable outbox.
use crate::{AgentRuntime, RunId, lanes::LaneError};
pub use storage::lane_instructions::{
    LaneInstructionAction, LaneInstructionDelivery, LaneInstructionRequest, LaneInstructionResult,
};
use storage::{lane_instructions::LaneInstructionActor, lane_management::LaneManagementActor};

#[derive(Debug, serde::Serialize)]
pub struct LaneInstructionOutcome {
    /// The original commit result, also on a replay after later edits.
    pub operation: LaneInstructionResult,
    /// A fresh observation: acceptance does not mean the lane has read the text.
    pub delivery: Option<LaneInstructionDelivery>,
}

fn instruction_error(error: storage::StorageError) -> LaneError {
    match error {
        storage::StorageError::LaneManagement(reason) => {
            LaneError::new(reason.as_str(), reason.as_str())
        }
        other => other.into(),
    }
}

impl AgentRuntime {
    /// Accept a supplement or brief revision for a lane the caller currently leads.
    ///
    /// # Errors
    /// The caller is not the lane's current lead root, a revision fence failed, the
    /// operation ID was reused for another request, the lane has not started, the input
    /// is invalid, or the ledger failed. Nothing is stored on an error.
    pub fn send_lane_instruction(
        &self,
        caller: RunId,
        request: LaneInstructionRequest,
    ) -> Result<LaneInstructionOutcome, LaneError> {
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "lane ledger is not configured")
        })?;
        // Same lock order as management, question resolution and restore.
        let completion = store
            .restore_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (actor, config) = self.lane_lead_actor(store, caller)?;
        let LaneManagementActor::Lead {
            thread_id,
            root_run_id,
        } = actor
        else {
            return Err(LaneError::new("not_current_lead", "caller is not a lead"));
        };
        let authority = config
            .ownership
            .as_ref()
            .map(|p| p.mutation_guard())
            .transpose()
            .map_err(|e| LaneError::new("ownership_lost", e))?;
        let lane_id = request.lane_id.clone();
        let operation = store
            .handle
            .apply_lane_instruction(
                request,
                LaneInstructionActor {
                    thread_id,
                    root_run_id,
                },
                crate::run_store::review_time_ns(),
            )
            .map_err(instruction_error)?;
        drop(authority);
        // The commit is the acceptance. A failed read-back must not turn it into a
        // retryable error: the receipt answers the retry.
        let delivery = operation.instruction_seq.and_then(|seq| {
            store
                .handle
                .lane_instruction_delivery(&lane_id, seq)
                .ok()
                .flatten()
        });
        let destination = store
            .handle
            .lane(&lane_id)
            .ok()
            .flatten()
            .map(|lane| lane.thread_id);
        drop(completion);
        if delivery == Some(LaneInstructionDelivery::Queued)
            && let Some(thread) = destination
        {
            self.schedule_lane_deliveries(&thread);
        }
        Ok(LaneInstructionOutcome {
            operation,
            delivery,
        })
    }
}
