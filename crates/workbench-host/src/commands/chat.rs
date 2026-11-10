//! Sending a message to a thread, and resuming one.

use config::agent_categories::CategoryId;
use runtime::{Role, RunConfig, RunId};

use crate::lane::TeamSource;

use super::{ChatInput, CommandService, ComposerRole, HostEvent, NoticeOutcome, RejectKind};

#[cfg(test)]
mod tests;

impl CommandService {
    /// A user `/continue` renews the automatic-wake budget; a runtime-requested
    /// resume only delivers the reports/questions held for the root.
    pub(super) fn resume_chat(
        &self,
        run_id: RunId,
        authority: RunConfig,
        observation: Option<runtime::ChatResumeObservation>,
    ) -> Result<Option<RunId>, runtime::RuntimeError> {
        if self.automatic_resume {
            if !self.runtime.prepare_held_wake(run_id, &authority)? {
                return Ok(None);
            }
            self.runtime.wake_chat(run_id, authority).map(Some)
        } else {
            let observation =
                observation.unwrap_or_else(|| self.runtime.observe_chat_resume(run_id));
            self.runtime
                .resume_chat_observed(run_id, authority, observation)
                .map(Some)
        }
    }

    pub(super) fn renew_team_authority(
        &self,
        thread: &str,
        run: RunId,
        prompt: Option<&str>,
        authority: &mut RunConfig,
    ) -> Result<(), String> {
        TeamSource {
            runtime: &self.runtime,
            memory_config: self.memory_config.as_ref(),
            team_writer: self.team_writer.as_ref(),
            goal_project: self.bindings.goal_projects.get(thread).map(String::as_str),
        }
        .renew(thread, run, prompt, authority)
    }

    pub(super) fn submit_chat(
        &mut self,
        submission: ChatInput,
        permit: Option<runtime::ownership::OwnerPermit>,
        resume_only: bool,
    ) -> Vec<HostEvent> {
        self.submit_chat_config(submission, permit, resume_only, RunConfig::default())
    }

    pub(super) fn submit_chat_config(
        &mut self,
        submission: ChatInput,
        permit: Option<runtime::ownership::OwnerPermit>,
        resume_only: bool,
        mut initial: RunConfig,
    ) -> Vec<HostEvent> {
        if !resume_only {
            // Host-generated goal/answer chats have no GUI submission row; give
            // them their own ID rather than storing the empty string.
            initial.initial_prompt_id = Some(if submission.submission_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                submission.submission_id.clone()
            });
        }

        // The runtime reads the mentioned skills with the new prompt; a resume
        // sends no prompt and attaches nothing.
        if !resume_only {
            initial.skill_mentions.clone_from(&submission.skills);
        }
        let thread_id = submission.thread_id;
        match &submission.model_preference {
            Some(preference) => {
                self.bindings
                    .model_preferences
                    .insert(thread_id.clone(), preference.clone());
            }
            None => {
                self.bindings.model_preferences.remove(&thread_id);
            }
        }
        // New runs of a thread work in its project, even after the active project moved.
        if let Some(root) = submission.project_root {
            self.bindings.thread_roots.insert(thread_id.clone(), root);
        }
        if initial.project_root.is_none() {
            initial.project_root = self.bindings.thread_roots.get(&thread_id).cloned();
        }
        initial.memory = self.capture_memory(initial.project_root.as_deref());
        let mut resume_observation = None;
        if resume_only {
            if !self.bindings.chat_runs.contains_key(&thread_id)
                && !self.bindings.goal_runs.contains_key(&thread_id)
            {
                match self.runtime.latest_chat_run(&thread_id) {
                    Ok(Some(run)) => {
                        self.bindings.chat_runs.insert(thread_id.clone(), run);
                    }
                    Ok(None) => {
                        return vec![HostEvent::ChatNotice {
                            outcome: NoticeOutcome::NotApplied,
                            thread_id,
                            text: "No conversation to continue; send a message first".into(),
                        }];
                    }
                    Err(error) => {
                        return vec![HostEvent::ChatRejected {
                            kind: RejectKind::Failed,
                            thread_id,
                            reason: error.to_string(),
                        }];
                    }
                }
            }
            let run = self
                .bindings
                .chat_runs
                .get(&thread_id)
                .or_else(|| self.bindings.goal_runs.get(&thread_id))
                .copied()
                .expect("resolved conversation root");
            let observation = self.runtime.observe_chat_resume(run);
            let graceful_stop = observation.graceful_stop_requested();
            resume_observation = Some(observation);
            // A graceful Stop can still publish a live phase while its terminal
            // snapshot commits. Only an explicit user Continue bypasses this
            // live-run guard; runtime owns deferring it until that snapshot.
            let phase = self
                .runtime
                .inspect_agent(run)
                .ok()
                .filter(|_| self.automatic_resume || !graceful_stop)
                .map(|run| run.phase);
            if phase == Some(event_bus::AgentRunPhase::Waiting) {
                return vec![HostEvent::ChatNotice {
                    outcome: NoticeOutcome::NotApplied,
                    thread_id,
                    text: "Run is already waiting for input".into(),
                }];
            }
            if phase.is_some_and(|phase| {
                matches!(
                    phase,
                    event_bus::AgentRunPhase::Pending | event_bus::AgentRunPhase::Running
                )
            }) {
                return vec![HostEvent::ChatNotice {
                    outcome: NoticeOutcome::NotApplied,
                    thread_id,
                    text: "Run is still running or stopping; try /continue once it settles".into(),
                }];
            }
        }

        #[cfg(test)]
        if resume_only {
            tests::after_resume_guard();
        }

        if let Some(&run_id) = self.bindings.goal_runs.get(&thread_id) {
            // A missing-context fallback may have replaced the original goal root.
            let run_id = self
                .bindings
                .chat_runs
                .get(&thread_id)
                .copied()
                .unwrap_or(run_id);
            let mut authority = RunConfig {
                ownership: permit.clone(),
                images: submission.images.clone(),
                model_preference: submission.model_preference.clone(),
                ..initial.clone()
            };
            if self
                .runtime
                .restore_diagnostics(run_id)
                .ok()
                .flatten()
                .is_some_and(|d| d.renewable_team.is_some())
            {
                let Some(project) = self.bindings.goal_projects.get(&thread_id) else {
                    return vec![HostEvent::ChatRejected {
                        kind: RejectKind::Conflict,
                        thread_id,
                        reason: "Current project binding is required to continue this team".into(),
                    }];
                };
                let (Some(config), Some(writer)) = (&self.memory_config, &self.team_writer) else {
                    return vec![HostEvent::ChatRejected {
                        kind: RejectKind::Unsupported,
                        thread_id,
                        reason: "Current team storage is unavailable".into(),
                    }];
                };
                authority.team_store = Some(runtime::team_context::TeamStore {
                    config: config.clone(),
                    writer: writer.clone(),
                    id: format!("{project}:{thread_id}"),
                });
                authority.topology = runtime::CoordinationTopology::DynamicTeam { max_workers: 3 };
                authority.delegation_value = Some(if resume_only {
                    "Resume the existing team coordination".into()
                } else {
                    submission.text.clone()
                });
                authority.finding_store = Some(config.db_path.clone());
            }
            let _guard = self.handle.enter();
            let continuation = if resume_only {
                self.resume_chat(run_id, authority, resume_observation.clone())
            } else {
                self.runtime
                    .continue_goal(run_id, submission.text.clone(), authority)
                    .map(Some)
            };
            match continuation {
                Ok(Some(run_id)) => {
                    self.bindings.chat_runs.insert(thread_id.clone(), run_id);
                    self.bindings.stop_marked.remove(&thread_id);
                    self.bindings.stopped_by_us.remove(&thread_id);
                    return vec![HostEvent::ChatAccepted {
                        thread_id,
                        run_id: run_id.to_string(),
                    }];
                }
                Ok(None) => {
                    return vec![HostEvent::ChatNotice {
                        outcome: NoticeOutcome::NotApplied,
                        thread_id,
                        text: "Automatic continuation was not applied: the conversation is stopped or its pending report/question was already handled. Continue explicitly if needed.".into(),
                    }];
                }
                Err(error) if !resume_only && self.can_restart_stopped(&thread_id, &error) => {
                    self.bindings.stop_marked.remove(&thread_id);
                    self.bindings.stopped_by_us.remove(&thread_id);
                    self.bindings.chat_runs.remove(&thread_id);
                }
                Err(error) => {
                    return vec![HostEvent::ChatRejected {
                        kind: RejectKind::Failed,
                        thread_id,
                        reason: error.to_string(),
                    }];
                }
            }
        }
        if let Some(permit) = &permit {
            if !resume_only
                && self
                    .bindings
                    .chat_permits
                    .get(&thread_id)
                    .is_some_and(|previous| {
                        previous.registry_path != permit.registry_path
                            || previous.lease.owner_id != permit.lease.owner_id
                            || previous.lease.generation != permit.lease.generation
                    })
            {
                self.bindings.chat_runs.remove(&thread_id);
            }
            self.bindings
                .chat_permits
                .insert(thread_id.clone(), permit.clone());
        }
        if let Some(&run_id) = self.bindings.chat_runs.get(&thread_id) {
            // continue_goal preserves the saved role, regardless of the current composer.
            let saved_role = self
                .runtime
                .restore_diagnostics(run_id)
                .ok()
                .flatten()
                .map(|saved| saved.role_name);
            let conversation = saved_role.as_deref().is_some_and(|role| {
                role == Role::Worker.name() || role == Role::Orchestrator.name()
            });
            let worker = saved_role.as_deref() == Some(Role::Worker.name());
            let mut authority = RunConfig {
                conversation,
                category: worker.then(|| CategoryId::Conversation.to_string()),
                ownership: permit.clone(),
                images: submission.images.clone(),
                model_preference: submission.model_preference.clone(),
                ..initial.clone()
            };
            if let Err(reason) = self.renew_team_authority(
                &thread_id,
                run_id,
                (!resume_only).then_some(submission.text.as_str()),
                &mut authority,
            ) {
                return vec![HostEvent::ChatRejected {
                    kind: RejectKind::Failed,
                    thread_id,
                    reason,
                }];
            }
            let _guard = self.handle.enter();
            let continuation = if resume_only {
                self.resume_chat(run_id, authority, resume_observation.clone())
            } else {
                self.runtime
                    .continue_goal(run_id, submission.text.clone(), authority)
                    .map(Some)
            };
            match continuation {
                Ok(Some(run_id)) => {
                    self.bindings.stop_marked.remove(&thread_id);
                    self.bindings.stopped_by_us.remove(&thread_id);
                    return vec![HostEvent::ChatAccepted {
                        thread_id,
                        run_id: run_id.to_string(),
                    }];
                }
                Ok(None) => {
                    return vec![HostEvent::ChatNotice {
                        outcome: NoticeOutcome::NotApplied,
                        thread_id,
                        text: "Automatic continuation was not applied: the conversation is stopped or its pending report/question was already handled. Continue explicitly if needed.".into(),
                    }];
                }
                Err(error) if !resume_only && self.can_restart_stopped(&thread_id, &error) => {
                    self.bindings.stop_marked.remove(&thread_id);
                    self.bindings.stopped_by_us.remove(&thread_id);
                    self.bindings.chat_runs.remove(&thread_id);
                }
                Err(error) => {
                    return vec![HostEvent::ChatRejected {
                        kind: RejectKind::Failed,
                        thread_id,
                        reason: error.to_string(),
                    }];
                }
            }
        }
        if resume_only {
            return vec![HostEvent::ChatNotice {
                outcome: NoticeOutcome::NotApplied,
                thread_id,
                text: "No conversation to continue; send a message first".into(),
            }];
        }
        let worker = submission.composer_role == ComposerRole::Worker;
        let _guard = self.handle.enter();
        let run_id = self.runtime.delegate_chat_seeded(
            &thread_id,
            match submission.composer_role {
                ComposerRole::Worker => Role::Worker,
                ComposerRole::Orchestrator => Role::Orchestrator,
            },
            submission.text,
            RunConfig {
                conversation: true,
                category: worker.then(|| CategoryId::Conversation.to_string()),
                images: submission.images,
                ownership: permit,
                interactive: true,
                keep_alive: true,
                model_preference: submission.model_preference,
                ..initial
            },
            submission.fork_seed,
        );
        let run_id = match run_id {
            Ok(run_id) => run_id,
            Err(error) => {
                return vec![HostEvent::ChatRejected {
                    kind: RejectKind::Failed,
                    thread_id,
                    reason: error.to_string(),
                }];
            }
        };
        self.bindings.chat_runs.insert(thread_id.clone(), run_id);
        self.bindings.stop_marked.remove(&thread_id);
        self.bindings.stopped_by_us.remove(&thread_id);
        vec![HostEvent::ChatAccepted {
            thread_id,
            run_id: run_id.to_string(),
        }]
    }
}
