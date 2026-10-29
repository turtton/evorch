use super::*;
use crate::RunRestoreFailure;
use crate::restore::{RestoredState, RunRestoreDescriptor};
pub(crate) use reservation::SnapshotReservation;

mod renewal;
mod reservation;
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
    /// A resume the runtime requested for held messages, not by the user.
    Wake,
}

/// A new chat run whose prompt is accepted, waiting to be started.
struct SeededChat {
    run_id: RunId,
    provenance: HostProvenance,
    review: crate::escalation_review::ReviewRunContext,
    restored: Option<RestoredState>,
    restored_source: Option<RunId>,
    route_pins: Vec<crate::RoutePinRecord>,
    prompt_id: Option<String>,
}

impl ChatContinuation {
    fn prompt(&self) -> Option<&str> {
        match self {
            Self::Followup(prompt) => Some(prompt),
            Self::Resume | Self::Wake => None,
        }
    }
}

impl AgentRuntime {
    /// Restore a stopped chat using only its saved history and current authority.
    /// A completed turn returns to input waiting; an unfinished turn continues.
    /// Already live runs are unchanged. Images and goal creation belong to a new
    /// human submission through `continue_goal`, not to this scheduling operation.
    pub fn resume_chat(&self, run_id: RunId, authority: RunConfig) -> Result<RunId, RuntimeError> {
        // An explicit continue renews the automatic-wake budget.
        self.reset_auto_wakes(run_id);
        self.resume(run_id, ChatContinuation::Resume, authority)
    }

    /// Resume a root the runtime asked the host to wake
    /// ([`event_bus::LifecycleEvent::RootWakeRequested`]) so that it receives
    /// the messages held for it. Like [`Self::resume_chat`] it reuses saved
    /// history under current authority, but it is not user input.
    pub fn wake_chat(&self, run_id: RunId, authority: RunConfig) -> Result<RunId, RuntimeError> {
        self.resume(run_id, ChatContinuation::Wake, authority)
    }

    fn resume(
        &self,
        run_id: RunId,
        continuation: ChatContinuation,
        mut authority: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        authority.images.clear();
        authority.initial_thread_goal = None;
        self.continue_chat(run_id, continuation, authority)
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
        mut authority: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        let prompt = continuation.prompt();
        // Only a new prompt takes mentions; a resume attaches nothing.
        let mentions = std::mem::take(&mut authority.skill_mentions);
        let mentions: &[String] = if prompt.is_some() { &mentions } else { &[] };
        // The host's ID for this prompt, or a fresh one: its receipt is tracked.
        let prompt_id = prompt.map(|_| {
            authority
                .initial_prompt_id
                .take()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| providers::RequestTurnContext::unattributed(false).prompt_id)
        });
        let operator_resume = !matches!(continuation, ChatContinuation::Wake);
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
        let stop_gate = self.stop_restore_gate(run_id);
        let _stop_guard = stop_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.ensure_no_saved_compaction(run_id)?;
        // Stop shares the run-local gate through registration/admission reservation:
        // a delayed automatic wake cannot replace an operator-stopped entry.
        if !operator_resume && self.operator_stopped(run_id) {
            return Err(fail(RunRestoreFailure::StoppedByOperator));
        }
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
            let project_root = lock_runs(&self.shared.runs)
                .get(&run_id)
                .and_then(|entry| entry.config.project_root.clone());
            // Read the skills before anything is recorded: a failure refuses the whole message.
            let host = self.expand_host_prompt(prompt.into(), mentions, project_root.as_deref())?;
            self.set_model_preference(run_id, authority.model_preference)?;
            self.send_inbox_message(
                run_id,
                host,
                authority.images,
                true,
                authority.initial_thread_goal,
                prompt_id,
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
        // Expand once, before any evidence, receipt or snapshot is written.
        let host = prompt
            .map(|prompt| {
                self.expand_host_prompt(
                    prompt.into(),
                    mentions,
                    descriptor
                        .project_root
                        .as_deref()
                        .or(authority.project_root.as_deref()),
                )
            })
            .transpose()?;
        let sent = host.as_ref().map(|host| host.prompt.as_str());
        #[cfg(test)]
        if let Ok(execution) = self.execution_snapshot(run_id) {
            self.execution_hook(
                super::cancellation::HookPoint::RootRestoreValidated,
                &execution,
            );
        }
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
        // Escalated roots retain thread_id in ordinary snapshots, but their
        // display name is not a chat identity. Rebind even without a saved goal.
        let thread = self
            .goal_thread(run_id)
            .or_else(|| descriptor.thread_id.clone())
            .or_else(|| named_thread.clone());
        if let Some((objective, criteria)) = &authority.initial_thread_goal {
            let thread = thread.as_deref().ok_or_else(|| {
                fail(RunRestoreFailure::CorruptContext(
                    "missing thread binding".into(),
                ))
            })?;
            self.validate_new_thread_goal(thread, objective, criteria)
                .map_err(|reason| fail(RunRestoreFailure::CorruptContext(reason)))?;
        }
        let mut restored = store.restore_conversation(&record)?;
        let mut host_input = storage::HostInputWrite {
            reconcile_reason: Some("restored".into()),
            ..Default::default()
        };
        self.reserve_restored_shell_handles(run_id, &restored)?;
        self.shared
            .model
            .restore_route_pins(&run_id.to_string(), &descriptor.route_pins);
        let pending_handoff = descriptor.pending_escalation.clone();
        let pending_request = pending_handoff.as_ref().and_then(|pending| {
            let original = pending
                .trusted_request
                .as_deref()
                .filter(|_| pending.trusted_request_complete);
            match (original, prompt) {
                (Some(original), Some(prompt)) => Some(
                    crate::thread_goals::continued_thread_request(original, prompt),
                ),
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
        if let (Some(prompt), Some(sent)) = (prompt, sent) {
            // Save accepted human input even when no provider is registered.
            // The in-memory continuation still appends it exactly once at start.
            // The history holds what the agent was sent, skills included.
            let mut history = restored.messages.clone();
            let mut content = vec![providers::ContentBlock::Text { text: sent.into() }];
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
            // The saved history now holds the prompt, so the same commit delivers it.
            if let Some(prompt_id) = &prompt_id {
                restored.host_inputs.push(prompt_id.clone());
                record.host_inputs_json = serde_json::to_string(&restored.host_inputs)
                    .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
                let now = crate::run_store::review_time_ns();
                host_input.inserts.push(storage::HostInputReceipt {
                    prompt_id: prompt_id.clone(),
                    run_id: run_id.to_string(),
                    root_run_id: String::new(),
                    text: prompt.into(),
                    image_count: u32::try_from(authority.images.len()).unwrap_or(u32::MAX),
                    status: storage::HostInputStatus::Delivered,
                    reason: None,
                    accepted_at_ns: now,
                    status_at_ns: now,
                });
            }
            // Persist atomically with the notices, retaining call diagnostics and
            // disk-delivery restrictions even when admission returns the snapshot.
            descriptor.outcome_notices_persisted = true;
            if let Some(pending) = &mut descriptor.pending_escalation {
                pending.trusted_request = pending_request.clone();
                pending.trusted_request_complete = true;
            }
            descriptor.turn = Some(crate::request_turn::human(
                prompt_id.clone().unwrap_or_default(),
                Default::default(),
                false,
                restored.turn.as_ref(),
            ));
        }
        let reservation = if pending_handoff.is_none() {
            let reservation = SnapshotReservation::new(store, run_id, &record, &descriptor)?;
            descriptor.admission_reservation_id = Some(reservation.id().into());
            descriptor.restorable = false;
            descriptor.non_restorable_reason = Some("snapshot_consumed".into());
            record.restorable = false;
            Some(reservation)
        } else {
            None
        };
        record.config_json = serde_json::to_string(&descriptor)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
        let review = self.chat_review_context(store, run_id)?;
        for receipt in &mut host_input.inserts {
            receipt.root_run_id.clone_from(&review.root_run_id);
        }
        let pending_reference =
            self.resolve_pending_reference(descriptor.pending_reference_id.as_deref())?;
        let mut evidence = host
            .as_ref()
            .map(|host| {
                crate::run_store::evidence_row_with_attachments(
                    &review.root_run_id,
                    &run_id.to_string(),
                    "host_chat",
                    &host.typed,
                    host.attachments.clone(),
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
            let (_, reconciled) = store
                .handle
                .save_snapshot_bundle_with_evidence_and_host_inputs(
                    &restored.provenance.write(record.clone()).map_err(|error| {
                        fail(RunRestoreFailure::CorruptContext(error.to_string()))
                    })?,
                    &evidence,
                    &[],
                    prompt
                        .and(pending_reference.as_ref())
                        .map(|pending| pending.reference_id.as_str()),
                    &host_input,
                )
                .map_err(|error| {
                    Self::review_error(run_id, crate::restore::provenance::storage_error(error))
                })?;
            for row in evidence {
                shared.add_request(crate::run_store::request_from_evidence(row));
            }
            self.emit_host_input_transitions(run_id, &reconciled);
            let delivered: Vec<_> = host_input
                .inserts
                .iter()
                .map(|receipt| {
                    storage::HostInputTransition::new(
                        receipt.prompt_id.clone(),
                        storage::HostInputStatus::Delivered,
                        None,
                    )
                })
                .collect();
            self.emit_host_input_transitions(run_id, &delivered);
        }
        restored.admission_reservation =
            reservation.map(|reservation| reservation.arm(&descriptor));
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
            initial_prompt_id: prompt_id,
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
        #[cfg(test)]
        if let Ok(execution) = self.execution_snapshot(run_id) {
            self.execution_hook(super::cancellation::HookPoint::RootGoalUpdated, &execution);
        }
        if let Some(request) = pending_request {
            self.remember_thread_request(run_id, &request);
        }
        // Resuming the root is the operator's go-ahead for the tree they
        // stopped. A wake requested by the runtime is not.
        if operator_resume {
            self.lift_operator_stops(run_id);
        }
        Ok(self.spawn_run_with_handoff(
            run_id,
            None,
            role,
            sent.unwrap_or_default().into(),
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
        let mentions = std::mem::take(&mut config.skill_mentions);
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
                    None => match seed.as_ref() {
                        None => None,
                        Some(seed) => Some(self.fork_seed_history(store, seed, role, &config)?),
                    },
                    Some(record) => {
                        let source = crate::meta::parse_run_id(&record.run_id)
                            .map_err(|reason| fail(RunRestoreFailure::CorruptContext(reason)))?;
                        self.ensure_no_saved_compaction(source)?;
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
                        let mut restored = store.restore_conversation(&record)?;
                        let source = crate::meta::parse_run_id(&record.run_id)
                            .map_err(|reason| fail(RunRestoreFailure::CorruptContext(reason)))?;
                        if let Some(bundle) = store.restore_bundle(source).map_err(|error| {
                            fail(RunRestoreFailure::CorruptContext(error.to_string()))
                        })? {
                            if bundle.record.messages_json == record.messages_json
                                && bundle.record.checkpoints_json == record.checkpoints_json
                            {
                                let messages: Vec<providers::Message> = serde_json::from_str(
                                    &record.messages_json,
                                )
                                .map_err(|error| {
                                    fail(RunRestoreFailure::CorruptContext(error.to_string()))
                                })?;
                                let context_len = messages
                                    .iter()
                                    .filter(|message| message.role != providers::Role::System)
                                    .count();
                                restored.provenance.fork(&bundle, context_len);
                                restored
                                    .provenance
                                    .generated_since(&restored.messages, messages.len());
                            } else {
                                restored.provenance =
                                    crate::restore::provenance::ConversationProvenance::unknown();
                            }
                        }
                        Some(restored)
                    }
                }
            }
        };
        config.name = Some(name);
        // Expand once, before any evidence or receipt is written; a failure
        // refuses the message whole.
        let host = self.expand_host_prompt(prompt, &mentions, config.project_root.as_deref())?;
        let run_id = self.shared.run_ids.next();
        let prompt_id = Some(
            config
                .initial_prompt_id
                .take()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| providers::RequestTurnContext::unattributed(false).prompt_id),
        );
        let provenance = restored_source.map_or(HostProvenance::FreshHostSubmission, |run_id| {
            HostProvenance::PersistedHostSubmission { run_id }
        });
        let review =
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
        let mut row = crate::run_store::evidence_row_with_attachments(
            &review.root_run_id,
            &run_id.to_string(),
            kind,
            &host.typed,
            host.attachments.clone(),
        );
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
            let consumed = pending_reference
                .as_ref()
                .map(|pending| pending.reference_id.as_str());
            match &prompt_id {
                // The run ID is fixed before spawning, so the receipt is
                // accepted with the prompt's evidence.
                Some(prompt_id) => {
                    let now = crate::run_store::review_time_ns();
                    store.handle.append_host_input_accepted(
                        std::slice::from_ref(&row),
                        edges.as_slice(),
                        consumed,
                        &storage::HostInputReceipt {
                            prompt_id: prompt_id.clone(),
                            run_id: run_id.to_string(),
                            root_run_id: review.root_run_id.clone(),
                            text: host.typed.clone(),
                            image_count: u32::try_from(config.images.len()).unwrap_or(u32::MAX),
                            status: storage::HostInputStatus::Accepted,
                            reason: None,
                            accepted_at_ns: now,
                            status_at_ns: now,
                        },
                    )
                }
                None => store
                    .handle
                    .append_review_evidence_with_reference_consumption(
                        std::slice::from_ref(&row),
                        edges.as_slice(),
                        consumed,
                    ),
            }
            .map_err(|error| Self::review_error(run_id, error))?;
        }
        // Accepted: a failure before the run starts drops the prompt.
        let started = self.start_seeded_chat(
            thread_id,
            role,
            host,
            config,
            seed,
            SeededChat {
                run_id,
                provenance,
                review,
                restored,
                restored_source,
                route_pins,
                prompt_id: prompt_id.clone(),
            },
        );
        if started.is_err()
            && let Some(prompt_id) = prompt_id
        {
            self.settle_host_inputs(
                run_id,
                &[storage::HostInputTransition::new(
                    prompt_id,
                    storage::HostInputStatus::Dropped,
                    Some("spawn_failed"),
                )],
            );
        }
        started
    }

    fn start_seeded_chat(
        &self,
        thread_id: &str,
        role: Role,
        host: HostPrompt,
        mut config: RunConfig,
        seed: Option<crate::restore::ChatForkSeed>,
        chat: SeededChat,
    ) -> Result<RunId, RuntimeError> {
        let SeededChat {
            run_id,
            provenance,
            mut review,
            restored,
            restored_source,
            route_pins,
            prompt_id,
        } = chat;
        if let Some(store) = self.shared.run_store.get() {
            // Materialize the committed graph, including depth/unique-root checks.
            // An invalid graph still fails closed before the run is installed.
            review = self.chat_review_context(store, run_id)?;
        } else {
            review.add_user_request_with_skills(
                &run_id.to_string(),
                &host.typed,
                host.attachments.clone(),
            );
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
        crate::thread_lineage::record_chat_root(
            self,
            thread_id,
            run_id,
            role,
            &config,
            seed.as_ref().filter(|_| restored_source.is_none()),
        );
        if let Some((objective, criteria)) = config.initial_thread_goal.take() {
            self.create_thread_goal(thread_id, run_id, objective, criteria)
                .map_err(|reason| RuntimeError::RunRestoreFailed {
                    run_id: run_id.to_string(),
                    reason: RunRestoreFailure::CorruptContext(reason),
                })?;
        }
        self.goal_user_input(run_id, &host.typed);
        config.initial_prompt_id = prompt_id;
        Ok(self.spawn_run_with_handoff(
            run_id,
            None,
            role,
            host.prompt,
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
        let mut restored = RestoredState::for_fork_seed(&record, seed.context_len)?;
        let bundle = store
            .restore_bundle(source)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?
            .ok_or_else(|| fail(RunRestoreFailure::MissingContext))?;
        if bundle.record.messages_json == record.messages_json
            && bundle.record.checkpoints_json == record.checkpoints_json
        {
            restored.provenance =
                crate::restore::provenance::ConversationProvenance::from_bundle(&bundle);
            restored.provenance.fork(&bundle, seed.context_len as usize);
        }
        Ok(restored)
    }
}
