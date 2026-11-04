//! Responder routing is independent of continuation inheritance.
use crate::{AgentRuntime, RunId, lanes::LaneError};
use event_bus::lane::LaneDeliveryBinding;
use event_bus::{AnswerOrigin, Event, ToolEvent, UserQuestion};
use storage::lane_delivery::{
    LaneQuestionAction, LaneQuestionCursor, LaneQuestionMutation, LaneQuestionPage,
};

/// PR C connects these hooks to its installed durable delivery host. Until then
/// an unbound host preserves Phase 1b user routing, and scheduling is a no-op.
pub(crate) trait LaneQuestionBindingSource {
    fn lane_question_binding(&self) -> LaneDeliveryBinding {
        LaneDeliveryBinding::Unbound
    }
    fn schedule_lane_question_delivery(&self, _thread_id: &str) {}
    fn record_lane_question_report_progress(
        &self,
        _report: &event_bus::LaneReport,
    ) -> Result<(), LaneError> {
        Ok(())
    }
}
impl LaneQuestionBindingSource for AgentRuntime {}

pub(crate) struct LaneAnswerRequest {
    pub question_id: String,
    pub management_revision: u64,
    pub routing_revision: u64,
    pub operation_id: String,
    /// None routes the original question to the user; Some answers as an agent.
    pub answer: Option<String>,
}

impl AgentRuntime {
    fn current_question_lead(
        &self,
        caller: RunId,
    ) -> Result<(String, crate::RunConfig), LaneError> {
        let (thread, config) = self.lane_lead(caller)?;
        let entry = self
            .entry(caller)
            .map_err(|e| LaneError::new("not_current_lead", e))?;
        if !matches!(
            *entry.phase_rx.borrow(),
            event_bus::AgentRunPhase::Pending
                | event_bus::AgentRunPhase::Running
                | event_bus::AgentRunPhase::Waiting
        ) || entry.execution.cancel_tx.borrow().is_interrupted()
        {
            return Err(LaneError::new("not_current_lead", "lead is not live"));
        }
        drop(entry);
        if let Some(store) = self.shared.run_store.get()
            && let Some(destination) = store.handle.lane_delivery_destination(thread.clone())?
            && let Some(owner) = destination.owner
            && owner.root_run_id != caller.to_string()
        {
            return Err(LaneError::new(
                "not_current_lead",
                "a different lead root is bound",
            ));
        }
        Ok((thread, config))
    }

    pub(crate) fn list_lane_questions(
        &self,
        caller: RunId,
        cursor: Option<LaneQuestionCursor>,
    ) -> Result<LaneQuestionPage, LaneError> {
        let (thread, _) = self.current_question_lead(caller)?;
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "question storage is not configured")
        })?;
        store
            .handle
            .lane_questions(thread, cursor)
            .map_err(question_error)
    }

    /// Authenticate once, hold the caller's generation guard through the storage
    /// CAS, then publish the committed projection. Never impersonate a parent or
    /// write trusted-user evidence. Offline continuation belongs to the outbox.
    pub(crate) fn resolve_lane_question(
        &self,
        caller: RunId,
        request: LaneAnswerRequest,
    ) -> Result<UserQuestion, LaneError> {
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "question storage is not configured")
        })?;
        // Match parent-answer/restore lock order: completion gate, then ownership.
        let _completion = store
            .restore_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (thread, config) = self.current_question_lead(caller)?;
        let _guard = config
            .ownership
            .as_ref()
            .map(|p| p.mutation_guard())
            .transpose()
            .map_err(|e| LaneError::new("ownership_lost", e.to_string()))?;
        let route = store
            .handle
            .lane_question_routing(request.question_id.clone())?
            .ok_or_else(|| LaneError::new("missing_routing", "question has no lane routing"))?;
        let destination = route.requester.thread_id;
        let action = match request.answer {
            Some(text) => LaneQuestionAction::Answer {
                text,
                origin: AnswerOrigin::LaneLead {
                    run_id: caller.to_string(),
                    thread_id: thread.clone(),
                },
            },
            None => LaneQuestionAction::RouteToUser,
        };
        let question = store
            .handle
            .mutate_lane_question(LaneQuestionMutation {
                lane_id: route.lane_id,
                question_id: request.question_id,
                lead_thread_id: thread,
                expected_management_revision: request.management_revision,
                expected_routing_revision: request.routing_revision,
                operation_id: request.operation_id,
                action,
                at_ns: crate::run_store::review_time_ns(),
            })
            .map_err(question_error)?;
        drop(_guard);
        drop(_completion);
        self.shared
            .bus
            .emit(Event::new(ToolEvent::UserQuestionUpdated {
                question: question.clone(),
            }));
        self.shared
            .question_version
            .send_modify(|v| *v = v.wrapping_add(1));
        // Live recipients observe question_version; C restores non-live recipients.
        if question.answer.is_some() {
            LaneQuestionBindingSource::schedule_lane_question_delivery(self, &destination);
        }
        Ok(question)
    }
}

fn question_error(error: storage::StorageError) -> LaneError {
    use storage::StorageError;
    let code = match &error {
        StorageError::QuestionAlreadyAnswered => "already_answered",
        StorageError::StaleRevision {
            kind: "routing", ..
        } => "stale_routing",
        StorageError::StaleRevision {
            kind: "management", ..
        } => "stale_management",
        StorageError::LaneDeliveryConflict(code) => code,
        _ => "storage_error",
    };
    LaneError::new(code, error.to_string())
}
