//! Answering a question an agent asked.

use runtime::{AnswerOutcome, Role};

use super::{ChatInput, CommandService, ComposerRole, HostCommand, HostEvent, RejectKind};

impl CommandService {
    pub(super) fn answer_user_question(
        &mut self,
        thread_id: String,
        question_id: String,
        answer: String,
        permit: Option<runtime::ownership::OwnerPermit>,
    ) -> Vec<HostEvent> {
        let question = match self.runtime.user_question(&question_id) {
            Ok(Some(q)) => q,
            Ok(None) => {
                return vec![HostEvent::CommandRejected {
                    kind: RejectKind::NotFound,
                    reason: "Unknown question".into(),
                }];
            }
            Err(reason) => {
                return vec![HostEvent::CommandRejected {
                    kind: RejectKind::Failed,
                    reason,
                }];
            }
        };
        let chat_role = [Role::Worker, Role::Orchestrator]
            .into_iter()
            .find(|role| question.root_name == format!("chat:{}:{thread_id}", role.name()));
        let bound_runs = self
            .bindings
            .goal_runs
            .get(&thread_id)
            .into_iter()
            .chain(self.bindings.chat_runs.get(&thread_id))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        // Consumer routing is not execution ownership. The shared helper
        // matches UI projection; the caller and live recipients are fenced
        // separately before any durable answer is written.
        if !question.belongs_to_thread(&thread_id, &bound_runs) {
            return vec![HostEvent::CommandRejected { kind: RejectKind::Conflict, reason: "Question belongs to a different or inactive conversation; resume its goal first".into() }];
        }
        if permit
            .as_ref()
            .is_some_and(|permit| permit.thread_id != thread_id)
        {
            return vec![HostEvent::CommandRejected {
                kind: RejectKind::Fenced,
                reason: runtime::ownership::OwnershipError::Fenced.to_string(),
            }];
        }
        // The storage write decides who answered first, so two answers that arrive
        // together cannot both believe they saved it.
        let question = match self.runtime.answer_user_question_if_pending_from_host(
            &question_id,
            &answer,
            permit.as_ref(),
        ) {
            Ok(AnswerOutcome::Recorded(question)) => *question,
            Ok(AnswerOutcome::AlreadyAnswered) => {
                let same = matches!(
                    self.runtime.user_question(&question_id),
                    Ok(Some(stored)) if stored.answer.as_deref() == Some(answer.as_str())
                );
                return repeated_answer(question_id, same);
            }
            Err(error) => {
                return vec![HostEvent::CommandRejected {
                    kind: match &error {
                        runtime::AnswerQuestionError::Ownership(_) => RejectKind::Fenced,
                        runtime::AnswerQuestionError::Failed(_) => RejectKind::Failed,
                    },
                    reason: error.to_string(),
                }];
            }
        };
        let saved = HostEvent::UserAnswerSaved {
            question_id: question_id.clone(),
            newly_saved: true,
        };
        let active = match self.runtime.has_active_question_recipient(&question_id) {
            Ok(active) => active,
            Err(reason) => {
                return vec![
                    saved,
                    HostEvent::CommandRejected {
                        kind: RejectKind::Failed,
                        reason: format!(
                            "Answer saved, but its recipient could not be confirmed: {reason}"
                        ),
                    },
                ];
            }
        };
        if active {
            return vec![saved];
        }
        let text = question.answer_message();
        let mut events = self.submit_authorized(
            HostCommand::SendChat(ChatInput {
                submission_id: String::new(),
                fork_seed: None,
                project_root: None,
                thread_id,
                text,
                images: Vec::new(),
                model_preference: None,
                composer_role: if chat_role == Some(Role::Worker) {
                    ComposerRole::Worker
                } else {
                    ComposerRole::Orchestrator
                },
                skills: Vec::new(),
            }),
            permit,
        );
        events.insert(0, saved);
        for event in &mut events {
            if let HostEvent::ChatRejected { reason, .. } = event {
                *reason = format!(
                    "Answer saved, but the conversation could not resume: {reason}. Fix the cause and resume it."
                );
            }
        }
        events
    }
}

/// The answer was stored before this command: the same text again is a repeat
/// that saves nothing, another text is refused.
fn repeated_answer(question_id: String, same: bool) -> Vec<HostEvent> {
    if same {
        vec![HostEvent::UserAnswerSaved {
            question_id,
            newly_saved: false,
        }]
    } else {
        vec![HostEvent::CommandRejected {
            kind: RejectKind::AlreadyResolved,
            reason: "Question was already answered".into(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
    use workbench_protocol::errors::{ErrorCode, RetryAdvice};

    use super::*;

    struct HeldModel;

    #[async_trait::async_trait]
    impl runtime::AgentModel for HeldModel {
        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            std::future::pending().await
        }

        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "held".into()
        }
    }

    #[tokio::test]
    async fn a_failed_caller_guard_remains_a_permission_refusal_and_stores_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("answers.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(event_bus::EventBus::new(64));
        let runtime = runtime::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(HeldModel),
        )
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime
            .delegate_chat("thread-1", Role::Worker, "Task".into(), Default::default())
            .unwrap();
        let question = runtime
            .request_user_question(run, "Continue?".into(), vec![], false)
            .unwrap();
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();

        let registry_path = dir.path().join("owners.db");
        let owner = ThreadOwner::new(
            "thread-1".into(),
            Lease {
                owner_id: "owner".into(),
                generation: 2,
                expires_at: u64::MAX,
            },
        );
        Registry::open(&registry_path)
            .unwrap()
            .start(&owner)
            .unwrap();
        let mut stale = OwnerPermit {
            registry_path,
            thread_id: owner.thread_id,
            lease: owner.lease,
            run_id: None,
        };
        stale.lease.generation = 1;
        let mut unavailable = stale.clone();
        unavailable.registry_path = dir.path().join("missing.db");
        let mut service = CommandService::new(runtime, tokio::runtime::Handle::current());
        // Start after ingress preflight: authority can change before the guarded
        // write, including when the original requester is no longer running.
        for permit in [stale, unavailable] {
            let events = service.submit_authorized(
                HostCommand::AnswerUserQuestion {
                    thread_id: "thread-1".into(),
                    question_id: question.id.clone(),
                    answer: "yes".into(),
                },
                Some(permit),
            );
            let [HostEvent::CommandRejected { kind, reason }] = events.as_slice() else {
                panic!("expected one refusal, got {events:?}");
            };
            assert_eq!(*kind, RejectKind::Fenced);
            let error = crate::ingress::reject_error(*kind, reason);
            assert_eq!(error.code, ErrorCode::PermissionDenied);
            assert_eq!(error.retry, RetryAdvice::UserActionRequired);
            assert_eq!(
                service
                    .runtime
                    .user_question(&question.id)
                    .unwrap()
                    .unwrap()
                    .answer,
                None
            );
        }
        assert!(matches!(
            service.runtime.answer_user_question_if_pending_from_host(
                "unknown-question",
                "yes",
                None
            ),
            Err(runtime::AnswerQuestionError::Failed(_))
        ));
    }
}
