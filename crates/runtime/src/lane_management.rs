//! Runtime management ingress. These trusted contexts are never decoded from JSON.
//! Hosts authenticate human input and hold their sidebar/family lock across reattach
//! and projection save. Management does not change execution authority or wake runs.
//!
//! Human ingress MUST use the delivery runtime's clone (the same RunStore and
//! restore gate), and must own the lane, its current lead and any new lead.
//! Never route a request to a second runtime merely because it opens the same
//! SQLite file. A host without that ownership rejects it rather than claiming
//! execution authority. `manage_lane_from_host` fences the supplied permits
//! under the completion gate, in the same lock order as delivery and restore.
use crate::{AgentRuntime, RunId, lanes::LaneError};
use event_bus::{Event, ToolEvent};
use storage::lane_management::LaneManagementActor;
pub use storage::lane_management::{
    LaneAttachGuard, LaneManagementAction, LaneManagementRequest, LaneManagementResult,
    ManagedLanePolicy,
};

/// Assertion by an authenticated host ingress, not model/client supplied data.
#[derive(Debug, Clone, Copy)]
pub struct LaneHumanContext;

/// Dispatcher supplies the actual caller, never a model-supplied run ID.
#[derive(Debug, Clone, Copy)]
pub enum LaneManagementContext {
    User(LaneHumanContext),
    Run(RunId),
}

#[derive(Debug, serde::Serialize)]
pub struct LaneManagementOutcome {
    /// Original commit snapshot on replay. Never project this over a newer revision.
    pub operation: LaneManagementResult,
    /// Fresh ledger observation, which may be newer than operation.lane.
    pub current_lane: Option<storage::lanes::LaneRecord>,
    /// Commit succeeded but a post-commit read/notification needs repair/re-query.
    pub refresh_pending: bool,
}

fn management_error(error: storage::StorageError) -> LaneError {
    match error {
        storage::StorageError::LaneManagement(reason) => {
            LaneError::new(reason.as_str(), reason.as_str())
        }
        other => other.into(),
    }
}

impl AgentRuntime {
    pub fn set_lane_policy(
        &self,
        context: LaneManagementContext,
        request: LaneManagementRequest,
    ) -> Result<LaneManagementOutcome, LaneError> {
        if !matches!(request.action, LaneManagementAction::SetPolicy { .. }) {
            return Err(LaneError::new("invalid_input", "expected SetPolicy"));
        }
        self.manage_lane(context, request, None)
    }

    pub fn detach_lane(
        &self,
        context: LaneManagementContext,
        request: LaneManagementRequest,
    ) -> Result<LaneManagementOutcome, LaneError> {
        if !matches!(request.action, LaneManagementAction::Detach) {
            return Err(LaneError::new("invalid_input", "expected Detach"));
        }
        self.manage_lane(context, request, None)
    }

    /// Only authenticated human ingress; guard is the authoritative family state
    /// checked under the host lock (including its expected family revision).
    /// Storage revalidates project, current target root, pin/archive and lane CAS.
    /// Host must save the *current* pending projection, then ACK that revision.
    pub fn reattach_lane(
        &self,
        human: LaneHumanContext,
        request: LaneManagementRequest,
        host_guard: LaneAttachGuard,
    ) -> Result<LaneManagementOutcome, LaneError> {
        if !matches!(request.action, LaneManagementAction::Reattach { .. }) {
            return Err(LaneError::new("invalid_input", "expected Reattach"));
        }
        self.manage_lane(
            LaneManagementContext::User(human),
            request,
            Some(host_guard),
        )
    }

    fn manage_lane(
        &self,
        context: LaneManagementContext,
        request: LaneManagementRequest,
        attach_guard: Option<LaneAttachGuard>,
    ) -> Result<LaneManagementOutcome, LaneError> {
        self.manage_lane_with_permits(context, request, attach_guard, &[])
    }

    /// Trusted human ingress only. Permits are resolved by the host for the
    /// lane, current lead and reattach target; never decoded from client JSON.
    pub fn manage_lane_from_host(
        &self,
        _: LaneHumanContext,
        request: LaneManagementRequest,
        attach_guard: Option<LaneAttachGuard>,
        permits: &[crate::ownership::OwnerPermit],
    ) -> Result<LaneManagementOutcome, LaneError> {
        self.manage_lane_with_permits(
            LaneManagementContext::User(LaneHumanContext),
            request,
            attach_guard,
            permits,
        )
    }

    /// The authenticated lead for a model-callable lane operation: live, a lane-external
    /// root Orchestrator, and the thread's durable current root. The caller holds the
    /// restore gate; the storage transaction re-validates the same facts.
    pub(crate) fn lane_lead_actor(
        &self,
        store: &crate::RunStore,
        caller: RunId,
    ) -> Result<(LaneManagementActor, crate::RunConfig), LaneError> {
        if !self.inspect_agent(caller).is_ok_and(|r| {
            matches!(
                r.phase,
                event_bus::AgentRunPhase::Pending
                    | event_bus::AgentRunPhase::Running
                    | event_bus::AgentRunPhase::Waiting
            )
        }) {
            return Err(LaneError::new("not_current_lead", "caller is not live"));
        }
        let (thread, config) = self
            .lane_lead(caller)
            .map_err(|e| LaneError::new("not_current_lead", e))?;
        if store
            .thread(&thread)?
            .and_then(|t| t.root_run_id)
            .as_deref()
            != Some(&caller.to_string())
        {
            return Err(LaneError::new(
                "not_current_lead",
                "caller is not the current root",
            ));
        }
        Ok((
            LaneManagementActor::Lead {
                thread_id: thread,
                root_run_id: caller.to_string(),
            },
            config,
        ))
    }

    fn manage_lane_with_permits(
        &self,
        context: LaneManagementContext,
        request: LaneManagementRequest,
        attach_guard: Option<LaneAttachGuard>,
        permits: &[crate::ownership::OwnerPermit],
    ) -> Result<LaneManagementOutcome, LaneError> {
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "lane ledger is not configured")
        })?;
        // Same lock order as question resolution/restore: completion, then owner.
        let completion = store
            .restore_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (actor, config) = match context {
            LaneManagementContext::User(_) => (LaneManagementActor::User, None),
            LaneManagementContext::Run(caller) => {
                let (actor, config) = self.lane_lead_actor(store, caller)?;
                (actor, Some(config))
            }
        };
        let authority = config
            .as_ref()
            .and_then(|c| c.ownership.as_ref())
            .map(|p| p.mutation_guard())
            .transpose()
            .map_err(|e| LaneError::new("ownership_lost", e))?;
        let host_authority = permits
            .iter()
            .map(|permit| permit.mutation_guard())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| LaneError::new("ownership_lost", e))?;
        let operation = store
            .handle
            .apply_lane_management_transition(
                request,
                actor,
                attach_guard,
                crate::run_store::review_time_ns(),
            )
            .map_err(management_error)?;
        drop(host_authority);
        drop(authority);
        // Do not turn a committed mutation into a retryable write failure. Pending
        // projections are durable; a lost event is repaired by re-reading the ledger.
        let current_lane = store.handle.lane(&operation.lane.lane_id).ok().flatten();
        let mut refresh_pending = current_lane.is_none();
        let questions = store.with_database(|db| db.pending_user_questions());
        match questions {
            Ok(questions) => {
                for question in questions {
                    match store.handle.lane_question_routing(question.id.clone()) {
                        Ok(Some(route)) if route.lane_id == operation.lane.lane_id => {
                            self.shared
                                .bus
                                .emit(Event::new(ToolEvent::UserQuestionUpdated { question }));
                        }
                        Err(_) => refresh_pending = true,
                        _ => {}
                    }
                }
            }
            Err(_) => refresh_pending = true,
        }
        self.shared
            .question_version
            .send_modify(|v| *v = v.wrapping_add(1));
        drop(completion);
        // UI re-query hint only: no mailbox, conversation input, delivery scheduling,
        // progress signal, Stop reset, admission, or automatic wake.
        self.lane_state_changed(
            &operation.lane.thread_id,
            event_bus::DiagnosticSeverity::Info,
            "lane_management_changed",
            "re-query current lane management and pending projections",
        );
        Ok(LaneManagementOutcome {
            operation,
            current_lane,
            refresh_pending,
        })
    }
}
