use super::*;
use crate::RunRestoreFailure;
use crate::restore::{RestoredState, RunRestoreDescriptor};

mod renewal;
#[cfg(test)]
mod tests;

pub(super) enum RunContinuation {
    Fresh,
    /// The first root run of a new chat thread, which records the thread's start state.
    FreshThread(String),
    Awaited,
    Handoff(RunHandoff),
    /// History reused by a host chat entry point alongside a new human prompt.
    /// The restored state itself supplies no review authorization.
    Restored(RestoredState),
    /// Resume the saved turn without submitting any new model input.
    Resume {
        restored: Box<RestoredState>,
        handoff: Option<RunHandoff>,
    },
}

enum ChatContinuation {
    Followup(String),
    Resume,
}

impl ChatContinuation {
    fn prompt(&self) -> Option<&str> {
        match self {
            Self::Followup(prompt) => Some(prompt),
            Self::Resume => None,
        }
    }
}

impl AgentRuntime {
    /// Restore a stopped chat using only its saved history and current authority.
    /// A completed turn returns to input waiting; an unfinished turn continues.
    /// Already live runs are unchanged. Images and goal creation belong to a new
    /// human submission through `continue_goal`, not to this scheduling operation.
    pub fn resume_chat(
        &self,
        run_id: RunId,
        mut authority: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        authority.images.clear();
        authority.initial_thread_goal = None;
        self.continue_chat(run_id, ChatContinuation::Resume, authority)
    }

    /// Continue a goal root in place, including after process restart.
    /// Persisted root identity supplies the role; the caller supplies current authority.
    /// `prompt` must be a new human submission from the host, never replayed history
    /// or an agent-authored continuation. It establishes fresh review evidence.
    pub fn continue_goal(
        &self,
        run_id: RunId,
        prompt: String,
        authority: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        self.continue_chat(run_id, ChatContinuation::Followup(prompt), authority)
    }

    fn continue_chat(
        &self,
        run_id: RunId,
        continuation: ChatContinuation,
        authority: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        let prompt = continuation.prompt();
        let fail = |reason| RuntimeError::RunRestoreFailed {
            run_id: run_id.to_string(),
            reason,
        };
        let store = self.shared.run_store.get();
        let _guard = store.map(|store| {
            store
                .restore_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let previous = {
            let runs = lock_runs(&self.shared.runs);
            runs.get(&run_id)
                .map(|entry| (entry.role, *entry.phase_rx.borrow()))
        };
        if matches!(
            previous,
            Some((
                _,
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
            ))
        ) {
            let Some(prompt) = prompt else {
                return Ok(run_id);
            };
            self.set_model_preference(run_id, authority.model_preference)?;
            self.send_inbox_message(
                run_id,
                prompt.into(),
                authority.images,
                true,
                authority.initial_thread_goal,
            )?;
            return Ok(run_id);
        }
        let store = store.ok_or_else(|| fail(RunRestoreFailure::StorageNotConfigured))?;
        let mut record = store
            .restore_record(run_id)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?
            .ok_or_else(|| fail(RunRestoreFailure::MissingContext))?;
        let mut descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
        self.validate_history_restore(&record, &descriptor, &authority)?;
        let role = Role::from_name(&descriptor.role)
            .map_err(|error| fail(RunRestoreFailure::UnsupportedConfig(error.to_string())))?;
        if previous.is_some_and(|(previous_role, _)| previous_role != role)
            || record.role != descriptor.role
            || descriptor.parent_run_id.is_some()
            || record.parent_run_id.is_some()
        {
            return Err(fail(RunRestoreFailure::CorruptContext(
                "goal identity".into(),
            )));
        }
        let named_thread = descriptor.name.as_deref().and_then(|name| {
            let (saved_role, thread) = name.strip_prefix("chat:")?.split_once(':')?;
            (Role::from_name(saved_role) == Ok(role)).then(|| thread.to_owned())
        });
        let thread = self.goal_thread(run_id).or_else(|| named_thread.clone());
        if let Some((objective, criteria)) = &authority.initial_thread_goal {
            let thread = thread.as_deref().ok_or_else(|| {
                fail(RunRestoreFailure::CorruptContext(
                    "missing thread binding".into(),
                ))
            })?;
            self.validate_new_thread_goal(thread, objective, criteria)
                .map_err(|reason| fail(RunRestoreFailure::CorruptContext(reason)))?;
        }
        let mut restored = RestoredState::for_conversation(&record)?;
        self.reserve_restored_shell_handles(run_id, &restored)?;
        self.shared
            .model
            .restore_route_pins(&run_id.to_string(), &descriptor.route_pins);
        let pending_handoff = descriptor.pending_escalation.clone();
        let pending_request = pending_handoff.as_ref().and_then(|pending| {
            match (pending.trusted_request.as_deref(), prompt) {
                (Some(original), Some(prompt)) => {
                    Some(crate::thread_goals::continued_request(original, prompt))
                }
                (Some(original), None) => Some(original.into()),
                (None, Some(prompt)) => Some(prompt.into()),
                (None, None) => None,
            }
        });
        if let Some(pending) = &pending_handoff {
            let child = event_bus::escalation_thread_id(&run_id.to_string());
            if role != Role::Orchestrator
                || authority
                    .ownership
                    .as_ref()
                    .is_some_and(|permit| permit.thread_id != child)
                || (pending.requires_ownership && authority.ownership.is_none())
            {
                return Err(fail(RunRestoreFailure::UnsupportedConfig(
                    "current child-thread ownership is required to retry escalation".into(),
                )));
            }
        }
        if pending_handoff.is_some()
            && let Some(prompt) = prompt
        {
            // Keep a retryable seed until a provider actually starts. Another
            // failed admission must retain this new human submission as well.
            let mut history = restored.messages.clone();
            let mut content = vec![providers::ContentBlock::Text {
                text: prompt.into(),
            }];
            content.extend(
                authority
                    .images
                    .iter()
                    .map(|image| providers::ContentBlock::Image {
                        media_type: image.media_type.clone(),
                        data: image.data.clone(),
                    }),
            );
            history.push(providers::Message {
                role: providers::Role::User,
                content,
            });
            record.messages_json = serde_json::to_string(&history)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
            if let Some(pending) = &mut descriptor.pending_escalation {
                pending.trusted_request = pending_request.clone();
            }
        } else if pending_handoff.is_none() {
            descriptor.restorable = false;
            descriptor.non_restorable_reason = Some("snapshot_consumed".into());
            record.restorable = false;
        }
        record.config_json = serde_json::to_string(&descriptor)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
        let review = self.chat_review_context(store, run_id)?;
        let pending_reference =
            self.resolve_pending_reference(descriptor.pending_reference_id.as_deref())?;
        let mut evidence = prompt
            .map(|prompt| {
                crate::run_store::evidence_row(
                    &review.root_run_id,
                    &run_id.to_string(),
                    "host_chat",
                    prompt,
                )
            })
            .into_iter()
            .collect::<Vec<_>>();
        if let (Some(reference), Some(row)) = (&pending_reference, evidence.first_mut()) {
            reference.annotate(row);
            descriptor.pending_reference_id = None;
            restored.pending_reference_id = None;
            record.config_json = serde_json::to_string(&descriptor)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
        }
        {
            let mut shared = review
                .user_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            store
                .handle
                .upsert_run_context_with_evidence_and_reference_consumption(
                    &record,
                    &evidence,
                    &[],
                    prompt
                        .and(pending_reference.as_ref())
                        .map(|pending| pending.reference_id.as_str()),
                )
                .map_err(|error| Self::review_error(run_id, error))?;
            for row in evidence {
                shared.add_request(crate::run_store::request_from_evidence(row));
            }
        }
        let review_seed = Some(ReviewSeed {
            provenance: HostProvenance::PersistedHostSubmission { run_id },
            context: review,
        });
        // A continued conversation stays in the project it was started in.
        let project_root = descriptor
            .project_root
            .take()
            .or_else(|| authority.project_root.clone());
        let mut config = RunConfig {
            // Keep role and chat identity canonical together, including mixed
            // snapshots written by versions that only renamed the role.
            name: named_thread
                .as_ref()
                .map(|thread| format!("chat:{}:{thread}", role.name()))
                .or(descriptor.name),
            interactive: true,
            keep_alive: true,
            project_root,
            ..authority
        };
        let handoff = pending_handoff.map(|pending| RunHandoff {
            continuity: descriptor.continuity.clone(),
            pending_reference_id: descriptor.pending_reference_id.clone(),
            source_run_id: pending.source_run_id,
            restored: None,
        });
        let continuation = if prompt.is_none() {
            RunContinuation::Resume {
                restored: Box::new(restored),
                handoff,
            }
        } else if let Some(mut handoff) = handoff {
            handoff.restored = Some(restored);
            RunContinuation::Handoff(handoff)
        } else {
            RunContinuation::Restored(restored)
        };
        if let Some(thread) = &thread {
            self.bind_thread_root(thread, run_id)
                .map_err(|reason| fail(RunRestoreFailure::CorruptContext(reason)))?;
        }
        if let Some((objective, criteria)) = config.initial_thread_goal.take() {
            let thread = self.goal_thread(run_id).ok_or_else(|| {
                fail(RunRestoreFailure::CorruptContext(
                    "missing thread binding".into(),
                ))
            })?;
            self.create_thread_goal(&thread, run_id, objective, criteria)
                .map_err(|reason| fail(RunRestoreFailure::CorruptContext(reason)))?;
        }
        if let Some(prompt) = prompt {
            self.goal_user_input(run_id, prompt);
        }
        if let Some(request) = pending_request {
            self.remember_thread_request(run_id, &request);
        }
        Ok(self.spawn_run_with_handoff(
            run_id,
            None,
            role,
            prompt.unwrap_or_default().into(),
            config,
            continuation,
            review_seed,
        ))
    }

    /// Resolve a thread's latest saved chat root without starting a new run.
    /// The caller must still renew current authority through `resume_chat` or `continue_goal`.
    pub fn latest_chat_run(&self, thread_id: &str) -> Result<Option<RunId>, RuntimeError> {
        let Some(store) = self.shared.run_store.get() else {
            return Ok(None);
        };
        let fail = |reason| RuntimeError::RunRestoreFailed {
            run_id: format!("chat:{thread_id}"),
            reason: RunRestoreFailure::CorruptContext(reason),
        };
        let mut latest = None;
        for role in [Role::Worker, Role::Orchestrator] {
            if let Some(record) = store
                .latest_terminal_named(&format!("chat:{}:{thread_id}", role.name()))
                .map_err(|error| fail(error.to_string()))?
                && latest
                    .as_ref()
                    .is_none_or(|previous: &storage::RunContextRecord| {
                        record.updated_at_ns > previous.updated_at_ns
                    })
            {
                latest = Some(record);
            }
        }
        latest
            .map(|record| crate::meta::parse_run_id(&record.run_id).map_err(fail))
            .transpose()
    }

    /// Start a chat run with the thread's latest terminal context, if one exists.
    /// Current configuration supplies fresh execution authority; only history is reused.
    /// `prompt` must be a new human submission from the host and supplies fresh review
    /// evidence; neither persisted user-role messages nor summaries grant authority.
    ///
    /// # Errors
    /// Rejects unreadable or invalid snapshots rather than silently dropping history.
    pub fn delegate_chat(
        &self,
        thread_id: &str,
        role: Role,
        prompt: String,
        config: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        self.delegate_chat_seeded(thread_id, role, prompt, config, None)
    }

    /// Like [`Self::delegate_chat`], but a thread without saved history starts from
    /// `seed`: another root chat's context truncated at a completed turn.
    /// The source run keeps its execution, questions and saved context unchanged.
    ///
    /// # Errors
    /// Rejects a seed whose boundary or identity no longer matches saved history.
    pub fn delegate_chat_seeded(
        &self,
        thread_id: &str,
        role: Role,
        prompt: String,
        mut config: RunConfig,
        seed: Option<crate::restore::ChatForkSeed>,
    ) -> Result<RunId, RuntimeError> {
        if let Some((objective, criteria)) = &config.initial_thread_goal {
            self.validate_new_thread_goal(thread_id, objective, criteria)
                .map_err(|reason| RuntimeError::RunRestoreFailed {
                    run_id: format!("chat:{thread_id}"),
                    reason: RunRestoreFailure::CorruptContext(reason),
                })?;
        }
        let name = format!("chat:{}:{thread_id}", role.name());
        let mut restored_source = None;
        let mut route_pins = Vec::new();
        let mut restored = match self.shared.run_store.get() {
            None => None,
            Some(store) => {
                let _guard = store
                    .restore_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let fail = |reason| RuntimeError::RunRestoreFailed {
                    run_id: name.clone(),
                    reason,
                };
                let mut record = store
                    .latest_terminal_named(&name)
                    .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
                if record.is_none() && role == Role::Advisor {
                    for legacy in ["Oracle", "oracle"] {
                        record = store
                            .latest_terminal_named(&format!("chat:{legacy}:{thread_id}"))
                            .map_err(|error| {
                                fail(RunRestoreFailure::CorruptContext(error.to_string()))
                            })?;
                        if record.is_some() {
                            break;
                        }
                    }
                }
                match record {
                    None => match seed {
                        None => None,
                        Some(seed) => Some(self.fork_seed_history(store, &seed, role, &config)?),
                    },
                    Some(record) => {
                        let descriptor: RunRestoreDescriptor =
                            serde_json::from_str(&record.config_json).map_err(|error| {
                                fail(RunRestoreFailure::CorruptContext(error.to_string()))
                            })?;
                        self.validate_history_restore(&record, &descriptor, &config)?;
                        if Role::from_name(&record.role) != Ok(role)
                            || Role::from_name(&descriptor.role) != Ok(role)
                            || record.parent_run_id.is_some()
                            || descriptor.parent_run_id.is_some()
                        {
                            return Err(fail(RunRestoreFailure::UnsupportedConfig(
                                descriptor
                                    .non_restorable_reason
                                    .unwrap_or_else(|| "chat identity".into()),
                            )));
                        }
                        // A continued conversation stays in the project it was started in.
                        if let Some(root) = descriptor.project_root {
                            config.project_root = Some(root);
                        }
                        route_pins = descriptor.route_pins;
                        restored_source =
                            Some(crate::meta::parse_run_id(&record.run_id).map_err(|reason| {
                                fail(RunRestoreFailure::CorruptContext(reason))
                            })?);
                        Some(RestoredState::for_conversation(&record)?)
                    }
                }
            }
        };
        config.name = Some(name);
        let run_id = self.shared.run_ids.next();
        let provenance = restored_source.map_or(HostProvenance::FreshHostSubmission, |run_id| {
            HostProvenance::PersistedHostSubmission { run_id }
        });
        let mut review =
            if let (Some(source), Some(store)) = (restored_source, self.shared.run_store.get()) {
                // Validate the existing ancestry before extending it with a fresh ID.
                let mut review = self.chat_review_context(store, source)?;
                review.lineage_run_ids.push(run_id.to_string());
                review
            } else {
                crate::escalation_review::ReviewRunContext::empty(&run_id.to_string())
            };
        let kind = if restored_source.is_some() {
            "host_chat"
        } else {
            "initial_prompt"
        };
        let mut row =
            crate::run_store::evidence_row(&review.root_run_id, &run_id.to_string(), kind, &prompt);
        let pending_reference = self.resolve_pending_reference(
            restored
                .as_ref()
                .and_then(|state| state.pending_reference_id.as_deref()),
        )?;
        if let Some(reference) = &pending_reference {
            reference.annotate(&mut row);
            if let Some(state) = restored.as_mut() {
                state.pending_reference_id = None;
            }
        }
        // RAM-only host submissions have exactly the same validation contract.
        row.validate()
            .map_err(|error| Self::review_error(run_id, error))?;
        if let Some(store) = self.shared.run_store.get() {
            let edges = restored_source.map(|source| storage::LineageEdge {
                child_run_id: run_id.to_string(),
                parent_run_id: source.to_string(),
                created_at_ns: crate::run_store::review_time_ns(),
            });
            store
                .handle
                .append_review_evidence_with_reference_consumption(
                    std::slice::from_ref(&row),
                    edges.as_slice(),
                    pending_reference
                        .as_ref()
                        .map(|pending| pending.reference_id.as_str()),
                )
                .map_err(|error| Self::review_error(run_id, error))?;
            // Materialize the committed graph, including depth/unique-root checks.
            // An invalid graph still fails closed before the run is installed.
            review = self.chat_review_context(store, run_id)?;
        } else {
            review.add_user_request(&run_id.to_string(), &prompt);
        }
        let review_seed = Some(ReviewSeed {
            provenance,
            context: review,
        });
        // The continuation is a new run, so its chain pins move to the new ID.
        self.shared
            .model
            .restore_route_pins(&run_id.to_string(), &route_pins);
        if let Some(restored) = &restored {
            self.reserve_restored_shell_handles(run_id, restored)?;
        }
        if let (Some(source), Some(restored)) = (restored_source, restored.as_ref()) {
            self.inherit_user_questions(source, run_id, &restored.messages)
                .map_err(|reason| RuntimeError::RunRestoreFailed {
                    run_id: source.to_string(),
                    reason: RunRestoreFailure::CorruptContext(format!(
                        "question inheritance failed: {reason}"
                    )),
                })?;
        }
        let continuation = match restored {
            Some(restored) => RunContinuation::Restored(restored),
            None if self.shared.run_store.get().is_some() => {
                RunContinuation::FreshThread(thread_id.to_owned())
            }
            None => RunContinuation::Fresh,
        };
        self.bind_thread_root(thread_id, run_id).map_err(|reason| {
            RuntimeError::RunRestoreFailed {
                run_id: run_id.to_string(),
                reason: RunRestoreFailure::CorruptContext(reason),
            }
        })?;
        if let Some((objective, criteria)) = config.initial_thread_goal.take() {
            self.create_thread_goal(thread_id, run_id, objective, criteria)
                .map_err(|reason| RuntimeError::RunRestoreFailed {
                    run_id: run_id.to_string(),
                    reason: RunRestoreFailure::CorruptContext(reason),
                })?;
        }
        self.goal_user_input(run_id, &prompt);
        Ok(self.spawn_run_with_handoff(
            run_id,
            None,
            role,
            prompt,
            config,
            continuation,
            review_seed,
        ))
    }

    fn chat_review_context(
        &self,
        store: &crate::RunStore,
        run_id: RunId,
    ) -> Result<crate::escalation_review::ReviewRunContext, RuntimeError> {
        let mut runs = self
            .shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ancestors = store
            .review_lineage_ancestors(&run_id.to_string())
            .map_err(|error| Self::review_error(run_id, error))?;
        if let Some(review) = runs.get(&run_id) {
            if review.stamp().synced
                && review.lineage_run_ids.len() == ancestors.len() + 1
                && ancestors
                    .iter()
                    .all(|id| review.lineage_run_ids.contains(id))
            {
                return Ok(review.clone());
            }
            let review = self.materialize_review_locked(store, run_id, &mut runs)?;
            runs.insert(run_id, review.clone());
            return Ok(review);
        }
        // Legacy chat history can resume without authority; trusted send cannot.
        let id = run_id.to_string();
        if store
            .review_lineage_ancestors(&id)
            .map_err(|error| Self::review_error(run_id, error))?
            .is_empty()
            && store
                .review_evidence_for_lineage(std::slice::from_ref(&id))
                .map_err(|error| Self::review_error(run_id, error))?
                .is_empty()
        {
            let review = crate::escalation_review::ReviewRunContext::empty(&id);
            runs.insert(run_id, review.clone());
            return Ok(review);
        }
        let review = self.materialize_review_locked(store, run_id, &mut runs)?;
        // Publish under the materialization fence, before provider admission can
        // await. Host answers must reach this exact Arc even before run install.
        runs.insert(run_id, review.clone());
        Ok(review)
    }

    fn fork_seed_history(
        &self,
        store: &crate::run_store::RunStore,
        seed: &crate::restore::ChatForkSeed,
        role: Role,
        authority: &RunConfig,
    ) -> Result<RestoredState, RuntimeError> {
        let fail = |reason| RuntimeError::RunRestoreFailed {
            run_id: seed.source_run_id.clone(),
            reason,
        };
        if let Some(permit) = &authority.ownership {
            permit
                .validate_generation()
                .map_err(|_| RuntimeError::StaleOwnership {
                    run_id: seed.source_run_id.clone(),
                })?;
        }
        let source = crate::meta::parse_run_id(&seed.source_run_id)
            .map_err(|reason| fail(RunRestoreFailure::CorruptContext(reason)))?;
        let record = store
            .restore_record(source)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?
            .ok_or_else(|| fail(RunRestoreFailure::MissingContext))?;
        if Role::from_name(&record.role) != Ok(role) {
            return Err(fail(RunRestoreFailure::UnsupportedConfig(
                "fork_seed: chat role differs from the source conversation".into(),
            )));
        }
        RestoredState::for_fork_seed(&record, seed.context_len)
    }
}
