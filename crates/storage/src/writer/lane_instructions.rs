//! Durable instruction writes through the single writer.
use crate::{StorageError, lane_instructions::*};

impl crate::StorageHandle {
    /// Accepts a lead's supplement or brief revision in one IMMEDIATE transaction. A retry
    /// with the same operation ID and request returns the original result
    /// (`replayed = true`). No wake, run start or conversation change happens here: the
    /// outbox row is delivered by the runtime at a safe boundary.
    ///
    /// # Errors
    /// `NotCurrentLead`, `StaleManagementRevision`, `OperationIdConflict`, `LaneNotStarted`,
    /// `InvalidInput`, `StaleRevision { kind: "brief" }`, an invalid brief, suspended writes
    /// or a closed writer.
    pub fn apply_lane_instruction(
        &self,
        request: LaneInstructionRequest,
        actor: LaneInstructionActor,
        at_ns: i64,
    ) -> Result<LaneInstructionResult, StorageError> {
        self.lane_job(true, move |c| {
            crate::repo::lane_instructions::apply(c, &request, &actor, at_ns)
        })
    }

    /// The immutable text of one accepted instruction; `None` once its lane is gone.
    ///
    /// # Errors
    /// The ledger cannot be read.
    pub fn lane_instruction(
        &self,
        lane_id: &str,
        instruction_seq: u64,
    ) -> Result<Option<LaneInstructionRecord>, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_job(false, move |c| {
            crate::repo::lane_instructions::get(c, &lane_id, instruction_seq)
        })
    }

    /// Accepted instructions after `after_seq` (at most 200), in sequence order.
    ///
    /// # Errors
    /// The ledger cannot be read.
    pub fn lane_instructions(
        &self,
        lane_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<LaneInstructionRecord>, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_job(false, move |c| {
            crate::repo::lane_instructions::list(c, &lane_id, after_seq, limit)
        })
    }

    /// Whether the lane conversation stored the instruction's full text yet.
    ///
    /// # Errors
    /// The ledger cannot be read.
    pub fn lane_instruction_delivery(
        &self,
        lane_id: &str,
        instruction_seq: u64,
    ) -> Result<Option<LaneInstructionDelivery>, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_job(false, move |c| {
            crate::repo::lane_instructions::delivery(c, &lane_id, instruction_seq)
        })
    }
}
