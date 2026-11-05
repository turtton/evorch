//! Answering a question an agent asked.

use runtime::Role;

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
        // Offline requesters have no live permit for the runtime to guard.
        // Keep the submitting host's generation stable through durable storage.
        let caller_guard = match permit.as_ref().map(|p| p.mutation_guard()).transpose() {
            Ok(guard) => guard,
            Err(error) => {
                return vec![HostEvent::CommandRejected {
                    kind: RejectKind::Fenced,
                    reason: error.to_string(),
                }];
            }
        };
        if let Some(previous) = &question.answer {
            return if previous == &answer {
                vec![HostEvent::UserAnswerSaved { question_id }]
            } else {
                vec![HostEvent::CommandRejected {
                    kind: RejectKind::AlreadyResolved,
                    reason: "Question was already answered".into(),
                }]
            };
        }
        let question = match self.runtime.answer_user_question(&question_id, &answer) {
            Ok(q) => q,
            Err(reason) => {
                return vec![HostEvent::CommandRejected {
                    kind: RejectKind::Failed,
                    reason,
                }];
            }
        };
        // Resumption writes the ownership registry, so release its read guard first.
        drop(caller_guard);
        let saved = HostEvent::UserAnswerSaved {
            question_id: question_id.clone(),
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
