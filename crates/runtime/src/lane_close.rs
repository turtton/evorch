//! Closing a lane (ADR 0044): the lead's (or a human's) management judgement.
//!
//! A close is not a verified outcome. It requires only that the caller is the current
//! lead root of an attached or summary lane and that the revisions and the latest Ready
//! the caller saw are still current. Artifacts, Git, GitHub and Reviewer runs are
//! never observed here; a Reviewer record may be attached for reference. Closing does
//! not stop a run, delete a thread or worktree, or detach the lane.
use crate::lane_management::LaneHumanContext;
use crate::lane_verification::{operation, storage_error};
use crate::lanes::LaneError;
use crate::runtime::questions::routing::LaneQuestionBindingSource;
use crate::{AgentRuntime, RunId};
use event_bus::lane::*;
use storage::lane_control::{CloseLane, LaneCloser};

#[cfg(test)]
thread_local! {
    /// Test-only barrier run after the caller is authorized and before the close commits,
    /// to model a Stop, restart or detach that lands in between.
    pub(crate) static BEFORE_CLOSE_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// What a lead says about the close; the lane's identity and its own authority come
/// from the runtime, not from here.
#[derive(Debug, Clone)]
pub struct LaneCloseRequest {
    pub lane_id: String,
    pub ready_seq: u64,
    pub expected_management_revision: u64,
    pub expected_request_revision: u64,
    pub note: String,
    /// A Reviewer record of the same lane to attach for reference. Any state is accepted.
    pub verification_id: Option<VerificationId>,
}
/// A human ending the lane's ledger entry from a trusted host path.
#[derive(Debug, Clone)]
pub struct HumanLaneCloseRequest {
    pub lane_id: String,
    pub ready_seq: Option<u64>,
    pub expected_management_revision: u64,
    pub expected_request_revision: u64,
    /// `true` ends the lane without claiming it is done.
    pub discard: bool,
    pub note: String,
}

/// A rejected close for a moved lane says what the lane looks like now, so the lead can
/// look again and decide; the revisions are an observation, never a license to retry
/// blindly.
fn with_current_revisions(store: &crate::RunStore, lane_id: &str, error: LaneError) -> LaneError {
    let stale = matches!(
        error.code,
        "stale_management" | "stale_ready" | "stale_brief" | "stale_request"
    );
    if !stale {
        return error;
    }
    match store.handle.lane_close_context(lane_id.to_owned()) {
        Ok(context) => LaneError::new(
            error.code,
            format!(
                "{}; current: {}",
                error.message,
                serde_json::to_string(&context).unwrap_or_default()
            ),
        ),
        Err(_) => error,
    }
}

impl AgentRuntime {
    /// Explicit lead operation; monitoring never calls it. The caller is the run the
    /// dispatcher is serving, never a tool argument, and storage compares it with the
    /// lead thread's current root inside the close transaction.
    ///
    /// # Errors
    /// `not_current_lead`, `not_attached`, `stale_management`, `stale_ready`,
    /// `stale_brief`, `stale_request`, `invalid_input`, `ownership_lost` and storage failures.
    pub fn close_lane(
        &self,
        caller: RunId,
        request: LaneCloseRequest,
    ) -> Result<LaneCloseResult, LaneError> {
        let (lead, config, _) = self.verification_lead(caller, &request.lane_id)?;
        let store = self.verification_store()?;
        let lane_id = request.lane_id.clone();
        // A Stop or restart that replaced the lead's root after the authorization above is
        // not raced here: storage compares the caller's root with the lead thread's
        // current root inside the close transaction.
        let guard = config
            .ownership
            .as_ref()
            .map(|p| p.mutation_guard())
            .transpose()
            .map_err(|_| operation(LaneOperationError::OwnershipLost))?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_CLOSE_COMMIT.with(|h| h.borrow_mut().take()) {
            hook();
        }
        let result = store
            .handle
            .close_lane(CloseLane {
                closer: LaneCloser::Lead {
                    thread_id: lead.clone(),
                    root_run_id: caller.to_string(),
                },
                kind: LaneCloseKind::LeadClosed,
                lane_id: request.lane_id,
                ready_seq: Some(request.ready_seq),
                expected_management_revision: request.expected_management_revision,
                expected_request_revision: request.expected_request_revision,
                note: request.note,
                verification_id: request.verification_id,
                at_ns: crate::run_store::review_time_ns(),
            })
            .map_err(|error| {
                let error = storage_error(error);
                with_current_revisions(store, &lane_id, error)
            })?;
        drop(guard);
        LaneQuestionBindingSource::schedule_lane_question_delivery(self, &lead);
        Ok(result)
    }

    /// Trusted human ingress only: reachable from a host command, never from an agent
    /// tool, MCP or dispatch. `permits` are resolved by the host for the lane and its
    /// lead, as for `manage_lane_from_host`.
    ///
    /// The result is recorded as `human_closed` or `discarded`, distinct from a lead
    /// close, and ends the ledger entry only.
    ///
    /// # Errors
    /// As [`Self::close_lane`], without the lead checks.
    pub fn close_lane_from_host(
        &self,
        _: LaneHumanContext,
        request: HumanLaneCloseRequest,
        permits: &[crate::ownership::OwnerPermit],
    ) -> Result<LaneCloseResult, LaneError> {
        let store = self.verification_store()?;
        let host_authority = permits
            .iter()
            .map(|permit| permit.mutation_guard())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| operation(LaneOperationError::OwnershipLost))?;
        let result = store
            .handle
            .close_lane(CloseLane {
                closer: LaneCloser::Human,
                kind: if request.discard {
                    LaneCloseKind::Discarded
                } else {
                    LaneCloseKind::HumanClosed
                },
                lane_id: request.lane_id.clone(),
                ready_seq: request.ready_seq,
                expected_management_revision: request.expected_management_revision,
                expected_request_revision: request.expected_request_revision,
                note: request.note,
                verification_id: None,
                at_ns: crate::run_store::review_time_ns(),
            })
            .map_err(storage_error)?;
        drop(host_authority);
        if let Ok(Some(lane)) = store.handle.lane(&request.lane_id)
            && let Some(lead) = lane.lead_thread_id
        {
            LaneQuestionBindingSource::schedule_lane_question_delivery(self, &lead);
        }
        Ok(result)
    }
}
