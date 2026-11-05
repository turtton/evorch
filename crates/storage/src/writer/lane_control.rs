//! Public single-writer entry points for Q/B/C/D. Every mutation is one IMMEDIATE
//! transaction. External hashing, PR refresh and tool-purpose checks stay in runtime.
use super::StorageHandle;
use crate::repo::lane_control as ledger;
use crate::{StorageError, lane_control::*};
impl StorageHandle {
    pub fn forward_lane_question(
        &self,
        request: ForwardLaneQuestion,
    ) -> Result<QuestionRouting, StorageError> {
        self.lane_job(true, move |c| ledger::forward(c, request))
    }
    pub fn lane_progress_anchor(
        &self,
        lane_id: String,
    ) -> Result<LaneProgressAnchor, StorageError> {
        self.lane_job(false, move |c| ledger::progress_anchor(c, &lane_id))
    }
    pub fn observe_lane_stall(
        &self,
        request: ObserveLaneStall,
    ) -> Result<LaneStallEpisode, StorageError> {
        self.lane_job(true, move |c| ledger::observe_stall(c, request))
    }
    pub fn lane_stall_episode(
        &self,
        episode_id: String,
    ) -> Result<Option<LaneStallEpisode>, StorageError> {
        self.lane_job(false, move |c| ledger::episode(c, &episode_id))
    }
    pub fn request_lane_nudge(
        &self,
        request: ObserveLaneStall,
        episode_id: String,
        expected_revision: u64,
    ) -> Result<LaneStallEpisode, StorageError> {
        self.lane_job(true, move |c| {
            ledger::request_nudge(c, request, &episode_id, expected_revision)
        })
    }
    pub fn record_lane_nudge_outcome(
        &self,
        request: ObserveLaneStall,
        episode_id: String,
        expected_revision: u64,
        state: LaneNudgeState,
    ) -> Result<LaneStallEpisode, StorageError> {
        self.lane_job(true, move |c| {
            ledger::nudge_outcome(c, request, &episode_id, expected_revision, state)
        })
    }
    /// Current close projection only; past closes remain immutable after reopening.
    pub fn current_lane_close(
        &self,
        lane_id: String,
    ) -> Result<Option<LaneCloseResult>, StorageError> {
        self.lane_job(false, move |c| ledger::current_close(c, &lane_id))
    }
    pub fn lane_verification_target(
        &self,
        lane_id: String,
    ) -> Result<LaneVerificationTarget, StorageError> {
        self.lane_job(false, move |c| ledger::target(c, &lane_id))
    }
    /// No fetch, freshness classification or inference that a brief needs PR facts.
    pub fn lane_pr_evidence(
        &self,
        thread_id: String,
    ) -> Result<Option<LanePrEvidence>, StorageError> {
        self.lane_job(false, move |c| ledger::pr_evidence(c, &thread_id))
    }
    pub fn begin_lane_verification(
        &self,
        request: BeginLaneVerification,
    ) -> Result<LaneVerification, StorageError> {
        self.lane_job(true, move |c| ledger::begin_verification(c, request))
    }
    pub fn lane_verification(
        &self,
        id: VerificationId,
    ) -> Result<Option<LaneVerification>, StorageError> {
        self.lane_job(false, move |c| ledger::verification(c, id))
    }
    /// Runtime supplies the authenticated run ID, never a tool argument.
    pub fn submit_lane_verification(
        &self,
        id: VerificationId,
        run_id: String,
        result: LaneVerificationSubmission,
        at_ns: i64,
    ) -> Result<LaneVerification, StorageError> {
        self.lane_job(true, move |c| ledger::submit(c, id, &run_id, result, at_ns))
    }
    /// Persist the normal Reviewer Done snapshot before calling this method.
    pub fn complete_lane_verification(
        &self,
        id: VerificationId,
        run_id: String,
        at_ns: i64,
    ) -> Result<LaneVerification, StorageError> {
        self.lane_job(true, move |c| {
            ledger::complete_verification(c, id, &run_id, at_ns)
        })
    }
    /// Runtime re-observes external artifacts and freshness before this CAS.
    pub fn close_lane(&self, request: CloseLane) -> Result<LaneCloseResult, StorageError> {
        self.lane_job(true, move |c| ledger::close_lane(c, request))
    }
    /// Derived unresolved state; does not mark anything read or append input.
    pub fn lane_attention(
        &self,
        filter: LaneAttentionFilter,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<LaneAttentionPage, StorageError> {
        self.lane_job(false, move |c| ledger::attention(c, filter, cursor, limit))
    }
}

impl StorageHandle {
    /// Owned observation CAS; no new episode or renewed nudge allowance.
    pub fn reclassify_lane_stall(
        &self,
        request: ObserveLaneStall,
        episode_id: String,
        expected_revision: u64,
    ) -> Result<LaneStallEpisode, StorageError> {
        self.lane_job(true, move |c| {
            ledger::reclassify_stall(c, request, &episode_id, expected_revision)
        })
    }
}
