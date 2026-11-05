//! Forwarding changes responder routing, never question ownership or inheritance.
use super::*;
use crate::lanes::LaneError;
use event_bus::lane::{LaneNotificationKind, QuestionRouting};

impl AgentRuntime {
    pub(crate) fn forward_lane_question(
        &self,
        caller: RunId,
        question_id: &str,
        operation_id: &str,
    ) -> Result<QuestionRouting, LaneError> {
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "question storage is not configured")
        })?;
        // Same completion/ownership lock order as parent answers and lead answers.
        let _completion = store
            .restore_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.validate_run_mutation(caller)
            .map_err(|e| LaneError::new("ownership_lost", e))?;
        let entry = self
            .entry(caller)
            .map_err(|e| LaneError::new("ownership_lost", e))?;
        if entry.parent.is_some()
            || entry.role != crate::Role::Orchestrator
            || entry.config.lane_scope.is_none()
        {
            return Err(LaneError::new(
                "not_attached",
                "only an attached lane root can forward a direct child's question",
            ));
        }
        if !matches!(
            *entry.phase_rx.borrow(),
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
        ) || entry.execution.cancel_tx.borrow().is_interrupted()
        {
            return Err(LaneError::new("ownership_lost", "lane root is not live"));
        }
        let config = entry.config.clone();
        drop(entry);
        let _authority = config
            .ownership
            .as_ref()
            .map(|p| p.mutation_guard())
            .transpose()
            .map_err(|e| LaneError::new("ownership_lost", e))?;
        let thread = self
            .goal_thread(caller)
            .ok_or_else(|| LaneError::new("not_attached", "lane thread is unavailable"))?;
        let lane = store
            .handle
            .lane_by_thread(&thread)?
            .ok_or_else(|| LaneError::new("not_attached", "lane record is unavailable"))?;
        let route = store
            .handle
            .forward_lane_question(storage::lane_control::ForwardLaneQuestion {
                caller_root: caller.to_string(),
                question_id: question_id.into(),
                expected_lane_generation: lane.generation,
                expected_management_revision: lane.management_revision,
                operation_id: operation_id.into(),
                at_ns: crate::run_store::review_time_ns(),
            })
            .map_err(super::routing::question_error)?;
        // A historical receipt must not republish its old Lead projection after rerouting.
        let question = store
            .user_question(question_id)?
            .ok_or_else(|| LaneError::new("unknown_question", "question disappeared"))?;
        drop(_authority);
        drop(_completion);
        self.shared
            .bus
            .emit(Event::new(ToolEvent::UserQuestionUpdated { question }));
        self.shared
            .question_version
            .send_modify(|v| *v = v.wrapping_add(1));
        if let event_bus::lane::QuestionResponder::Lead { thread_id } = &route.responder {
            self.schedule_lane_deliveries(thread_id);
        }
        Ok(route)
    }

    /// After durable lane input acceptance, reconnect answers to the normal child
    /// continuation path. Also scan received history before claiming: a crash after
    /// receipt but before this hook must not lose the continuation behind ACK repair.
    /// Caller authority, direct ownership, Stop and snapshot-consumed fences are
    /// rechecked by resume_answered_child; inheritance alone never renews authority.
    pub(crate) async fn resume_forwarded_lane_answers(
        &self,
        caller: RunId,
        thread: &str,
    ) -> Result<(), LaneError> {
        let Some(store) = self.shared.run_store.get() else {
            return Ok(());
        };
        let mut after = String::new();
        loop {
            let notices = store.handle.lane_inbox(thread.to_owned(), after.clone())?;
            if notices.is_empty() {
                return Ok(());
            }
            after = notices
                .last()
                .expect("nonempty page")
                .notification_id
                .clone();
            let ids = notices
                .iter()
                .map(|n| format!("lane-notification:{}", n.notification_id))
                .collect::<Vec<_>>();
            let receipts = store.handle.host_input_receipts(&ids)?;
            for notice in notices {
                let LaneNotificationKind::Answer { question_id, .. } = &notice.kind else {
                    continue;
                };
                if !receipts.iter().any(|r| {
                    r.status == storage::HostInputStatus::Delivered
                        && r.provenance
                            == storage::HostInputProvenance::LaneNotification {
                                notification_id: notice.notification_id.clone(),
                                destination_thread_id: thread.to_owned(),
                            }
                }) {
                    continue;
                }
                let Some(question) = self
                    .user_question(question_id)
                    .map_err(|error| LaneError::new("storage_failed", error))?
                else {
                    continue;
                };
                if question.answer.is_none() {
                    continue;
                }
                let Some(route) = store.handle.lane_question_routing(question_id.clone())? else {
                    continue;
                };
                if route.requester.thread_id != thread {
                    continue;
                }
                let recipients = store.user_question_recipients(question_id)?;
                for recipient in std::iter::once(question.run_id.clone()).chain(recipients) {
                    let Ok(child) = crate::meta::parse_run_id(&recipient) else {
                        continue;
                    };
                    // Root-owned questions need no child continuation; live roots
                    // already consume question_version through flush_user_answers.
                    if self
                        .entry(child)
                        .is_ok_and(|entry| entry.parent == Some(caller))
                        && let Err(error) =
                            self.resume_answered_child(caller, child, &question).await
                    {
                        self.lane_state_changed(
                            thread,
                            event_bus::DiagnosticSeverity::Warning,
                            "lane_answer_continuation_unavailable",
                            &error,
                        );
                    }
                }
            }
        }
    }
}
