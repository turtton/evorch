//! Durable, non-blocking clarification requests. These never grant tool permissions.
use crate::{AgentRuntime, RunId};
use event_bus::{AgentRunPhase, AnswerOrigin, Event, ToolEvent, UserQuestion};
use storage::{AnswerMode, StorageError};

#[cfg(test)]
#[path = "questions_tests.rs"]
mod tests;

mod continuation;

/// How [`AgentRuntime::wait_or_blocked_on_user`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunWait {
    /// The run reached a terminal phase.
    Ended(AgentRunPhase),
    /// The run is `Waiting` while blocking questions to the user have no answer.
    BlockedOnUser(Vec<UserQuestion>),
}

/// How an answer submitted with [`AgentRuntime::answer_user_question_if_pending`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// This call stored the answer.
    Recorded(Box<UserQuestion>),
    /// The question had been answered before this call, by anyone and with any text,
    /// and nothing was stored.
    AlreadyAnswered,
}

impl AgentRuntime {
    /// Waits for the run like [`AgentRuntime::wait`], but also returns when the run is
    /// `Waiting` with a blocking question unanswered: a host with no one to answer
    /// (a headless run) would otherwise wait for ever.
    ///
    /// This reads state, not events: the run's phase and the persisted questions, woken
    /// by changes of either. Events can be dropped for a slow subscriber; neither of
    /// these can be missed, and a caller that starts waiting late still sees the state.
    ///
    /// # Errors
    /// [`crate::RuntimeError::UnknownRun`] when the run is not registered.
    pub async fn wait_or_blocked_on_user(
        &self,
        run_id: RunId,
    ) -> Result<RunWait, crate::RuntimeError> {
        self.wait_admission(run_id).await?;
        let mut phase_rx = self.entry(run_id)?.phase_rx.clone();
        let mut question_changes = self.shared.question_version.subscribe();
        loop {
            // Mark both as seen before reading the state they announce, so a change
            // after the read wakes the wait again.
            let phase = *phase_rx.borrow_and_update();
            question_changes.borrow_and_update();
            match phase {
                AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error => {
                    // Terminal publication holds runs until its completion relay finishes.
                    drop(self.entry(run_id)?);
                    return Ok(RunWait::Ended(phase));
                }
                AgentRunPhase::Waiting => {
                    if let Ok(questions) = self.user_answers(run_id) {
                        let pending: Vec<UserQuestion> = questions
                            .into_iter()
                            .filter(|question| question.blocking && question.answer.is_none())
                            .collect();
                        if !pending.is_empty() {
                            return Ok(RunWait::BlockedOnUser(pending));
                        }
                    }
                }
                AgentRunPhase::Pending | AgentRunPhase::Running => {}
            }
            tokio::select! {
                changed = phase_rx.changed() => {
                    if changed.is_err() {
                        return Ok(RunWait::Ended(*phase_rx.borrow()));
                    }
                }
                _ = question_changes.changed() => {}
            }
        }
    }

    pub fn request_user_question(
        &self,
        run: RunId,
        title: String,
        options: Vec<String>,
        blocking: bool,
    ) -> Result<UserQuestion, String> {
        self.validate_run_mutation(run).map_err(|e| e.to_string())?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("durable question storage is not configured")?;
        let (root, name) = {
            let mut root = run;
            loop {
                let entry = self.entry(root).map_err(|e| e.to_string())?;
                match entry.parent {
                    Some(parent) => root = parent,
                    None => break (root, entry.name.clone()),
                }
            }
        };
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|e| e.to_string())?;
        let id = format!(
            "question-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let question = UserQuestion {
            id,
            run_id: run.to_string(),
            root_run_id: root.to_string(),
            root_name: name,
            recipient_run_ids: Vec::new(),
            title,
            options,
            blocking,
            answer: None,
            answer_origin: None,
        };
        let permit = self
            .entry(run)
            .map_err(|e| e.to_string())?
            .config
            .ownership
            .clone();
        let guard = permit
            .as_ref()
            .map(|permit| permit.mutation_guard())
            .transpose()
            .map_err(|e| e.to_string())?;
        store
            .handle
            .create_user_question(&question)
            .map_err(|e| e.to_string())?;
        drop(guard);
        self.shared
            .bus
            .emit(Event::new(ToolEvent::UserQuestionUpdated {
                question: question.clone(),
            }));
        self.shared
            .question_version
            .send_modify(|version| *version = version.wrapping_add(1));
        Ok(question)
    }

    /// Read clarification requests owned by one direct child. The parent can
    /// answer from task context or ask the user on its own run.
    pub fn subagent_questions(
        &self,
        caller: RunId,
        child: RunId,
    ) -> Result<Vec<UserQuestion>, String> {
        self.validate_question_parent(caller, child)?;
        self.user_answers(child)
    }

    /// Answer a direct child's question without granting any tool permission.
    /// The host-facing answer API remains the only way to answer root questions.
    /// If the child is finalizing, wait for cleanup before continuing its history.
    pub async fn answer_subagent_question(
        &self,
        caller: RunId,
        id: &str,
        answer: &str,
    ) -> Result<UserQuestion, String> {
        let (child, question) = self.persist_subagent_answer(caller, id, answer)?;
        self.resume_answered_child(caller, child, &question).await?;
        Ok(question)
    }

    fn persist_subagent_answer(
        &self,
        caller: RunId,
        id: &str,
        answer: &str,
    ) -> Result<(RunId, UserQuestion), String> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("question storage is not configured")?;
        // finish_success holds this same gate for its final answer check and
        // mailbox closure. An accepted answer is either live work or a restore.
        let _completion = store
            .restore_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let question = self.user_question(id)?.ok_or("unknown question")?;
        let child = crate::meta::parse_run_id(&question.run_id)?;
        self.validate_question_parent(caller, child)?;
        let ownership = self
            .entry(caller)
            .map_err(|error| error.to_string())?
            .config
            .ownership
            .clone();
        let _guard = ownership
            .as_ref()
            .map(|permit| permit.mutation_guard())
            .transpose()
            .map_err(|error| error.to_string())?;
        let question = self.answer_question(
            id,
            answer,
            AnswerOrigin::ParentAgent {
                answered_by_run_id: caller.to_string(),
            },
        )?;
        Ok((child, question))
    }

    fn validate_question_parent(&self, caller: RunId, child: RunId) -> Result<(), String> {
        self.validate_run_mutation(caller)
            .map_err(|e| e.to_string())?;
        let parent = self.entry(caller).map_err(|e| e.to_string())?;
        if parent.role != crate::Role::Orchestrator {
            return Err("only an orchestrator can resolve subagent questions".into());
        }
        drop(parent);
        if self.entry(child).map_err(|e| e.to_string())?.parent != Some(caller) {
            return Err("question requester is not a direct child of this orchestrator".into());
        }
        Ok(())
    }

    pub fn user_question(&self, id: &str) -> Result<Option<UserQuestion>, String> {
        self.shared
            .run_store
            .get()
            .ok_or("question storage is not configured")?
            .user_question(id)
            .map_err(|e| e.to_string())
    }

    /// Host-facing answer API. A live run must still have current mutation authority.
    /// Offline answers are persisted for explicit conversation resumption, never replaying tools.
    pub fn answer_user_question(&self, id: &str, answer: &str) -> Result<UserQuestion, String> {
        self.answer_question(id, answer, AnswerOrigin::Host)
    }

    /// Host-facing answer API for a caller that must not answer a question twice. Unlike
    /// [`Self::answer_user_question`], an earlier answer is reported as
    /// [`AnswerOutcome::AlreadyAnswered`] even when it has the same text, so the caller
    /// knows it did not give the answer the run receives. The decision is made by the
    /// storage write itself, not by a check before it, so an answer that lands at the
    /// same time cannot be mistaken for this one. Every other failure stays an `Err`.
    ///
    /// # Errors
    /// Unknown question, missing storage, a stale or unavailable mutation authority,
    /// and persistence failures.
    pub fn answer_user_question_if_pending(
        &self,
        id: &str,
        answer: &str,
    ) -> Result<AnswerOutcome, String> {
        self.answer_question_with_mode(id, answer, AnswerOrigin::Host, AnswerMode::FirstOnly)
    }

    fn answer_question(
        &self,
        id: &str,
        answer: &str,
        origin: AnswerOrigin,
    ) -> Result<UserQuestion, String> {
        match self.answer_question_with_mode(id, answer, origin, AnswerMode::Idempotent)? {
            AnswerOutcome::Recorded(question) => Ok(*question),
            AnswerOutcome::AlreadyAnswered => Err("question already answered".into()),
        }
    }

    fn answer_question_with_mode(
        &self,
        id: &str,
        answer: &str,
        origin: AnswerOrigin,
        mode: AnswerMode,
    ) -> Result<AnswerOutcome, String> {
        // Only a host answer is the user's word and becomes review evidence; a
        // parent agent's answer never does, whatever it relays.
        let trusted_user = origin == AnswerOrigin::Host;
        let question = self.user_question(id)?.ok_or("unknown question")?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("question storage is not configured")?;
        let recipients = store
            .user_question_recipients(id)
            .map_err(|e| e.to_string())?;
        let mut permits = Vec::new();
        for recipient in &recipients {
            let run = crate::meta::parse_run_id(recipient)?;
            if let Ok(entry) = self.entry(run)
                && matches!(
                    *entry.phase_rx.borrow(),
                    event_bus::AgentRunPhase::Pending
                        | event_bus::AgentRunPhase::Running
                        | event_bus::AgentRunPhase::Waiting
                )
                && let Some(permit) = &entry.config.ownership
            {
                permits.push(permit.clone());
            } else if let Some(admission) = self.admission_snapshot(run)
                && admission.result.is_none()
                && let Some(permit) = admission.ownership
            {
                permits.push(permit);
            }
        }
        let guards = permits
            .iter()
            .map(|permit| permit.mutation_guard())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        // Serialize host evidence writes and resync with the same map fence as chat.
        let reviews = self
            .shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut targets: Vec<(String, crate::escalation_review::ReviewRunContext)> = Vec::new();
        if trusted_user {
            for recipient in std::iter::once(&question.run_id).chain(recipients.iter()) {
                let run = crate::meta::parse_run_id(recipient)?;
                let Some(review) = reviews.get(&run) else {
                    continue;
                };
                if !targets.iter().any(|(_, existing)| {
                    std::sync::Arc::ptr_eq(&existing.user_requests, &review.user_requests)
                }) {
                    targets.push((recipient.clone(), review.clone()));
                }
            }
        }
        let resync = |review: &crate::escalation_review::ReviewRunContext| {
            let lineage = reviews
                .values()
                .filter(|candidate| {
                    std::sync::Arc::ptr_eq(&candidate.user_requests, &review.user_requests)
                })
                .flat_map(|candidate| candidate.lineage_run_ids.iter().cloned())
                .collect::<Vec<_>>();
            crate::run_store::resync_review_context(store, review, &lineage)
                .map_err(|error| error.to_string())
        };
        // Re-read under the fence: a concurrent identical host answer must not be
        // reflected twice. Plan A fences conflicting answers through the writer's
        // checked UPDATE of the answer (not recipient-specific UPDATEs), atomically
        // with evidence. The synced/resync protocol below repairs live recipients
        // after a failed post-commit reflection; an unsynced context cannot approve.
        let question = store
            .user_question(id)
            .map_err(|error| error.to_string())?
            .ok_or("question disappeared")?;
        if mode == AnswerMode::FirstOnly && question.answer.is_some() {
            return Ok(AnswerOutcome::AlreadyAnswered);
        }
        if question.answer.as_deref() == Some(answer) {
            let mut repaired = false;
            for (_, review) in &targets {
                if !review.stamp().synced {
                    resync(review)?;
                    repaired = true;
                }
            }
            // A failed post-commit reflection may not have published the answer.
            if repaired {
                self.shared
                    .bus
                    .emit(Event::new(ToolEvent::UserQuestionUpdated {
                        question: question.clone(),
                    }));
                self.shared
                    .question_version
                    .send_modify(|version| *version = version.wrapping_add(1));
            }
            return Ok(AnswerOutcome::Recorded(Box::new(question)));
        }
        if trusted_user {
            let mut recipients = recipients;
            recipients.push(question.run_id.clone());
            recipients.sort();
            recipients.dedup();
            let evidence = recipients
                .iter()
                .map(|recipient| {
                    use sha2::{Digest, Sha256};
                    let registered = reviews.contains_key(&crate::meta::parse_run_id(recipient)?);
                    let root = answer_evidence_root(store, recipient, registered)?;
                    let mut row =
                        crate::run_store::evidence_row(&root, recipient, "user_answer", answer);
                    row.id = format!(
                        "{:x}",
                        Sha256::digest(format!(
                            "{root}|{recipient}|user_answer|{answer}|{}",
                            question.id
                        ))
                    );
                    row.in_reply_to_question_id = Some(question.id.clone());
                    row.in_reply_to_title = Some(question.title.clone());
                    Ok(row)
                })
                .collect::<Result<Vec<_>, String>>()?;
            let stored = store
                .handle
                .answer_user_question_with_evidence_and_mode(id, answer, &origin, &evidence, mode);
            if matches!(stored, Err(StorageError::QuestionAlreadyAnswered)) {
                return Ok(AnswerOutcome::AlreadyAnswered);
            }
            stored.map_err(|error| error.to_string())?;
        } else {
            let stored = store
                .handle
                .answer_user_question_with_mode(id, answer, &origin, mode);
            if matches!(stored, Err(StorageError::QuestionAlreadyAnswered)) {
                return Ok(AnswerOutcome::AlreadyAnswered);
            }
            stored.map_err(|error| error.to_string())?;
        }
        drop(guards);
        let mut reflection_error = None;
        for (recipient, review) in &targets {
            if review.user_requests.is_poisoned() {
                review.mark_unsynced();
                reflection_error = Some("review context lock is poisoned".to_string());
            } else if !review.stamp().synced {
                if let Err(error) = resync(review) {
                    review.mark_unsynced();
                    reflection_error = Some(error);
                }
            } else {
                review.add_user_answer(recipient, &question, answer);
            }
        }
        drop(reviews);
        if let Some(error) = reflection_error {
            return Err(error);
        }
        let mut question = question;
        question.answer = Some(answer.into());
        question.answer_origin = Some(origin);
        self.shared
            .bus
            .emit(Event::new(ToolEvent::UserQuestionUpdated {
                question: question.clone(),
            }));
        self.shared
            .question_version
            .send_modify(|version| *version = version.wrapping_add(1));
        Ok(AnswerOutcome::Recorded(Box::new(question)))
    }

    /// True if the original requester or an explicitly linked continuation is live.
    pub fn has_active_question_recipient(&self, id: &str) -> Result<bool, String> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("question storage is not configured")?;
        for run in store
            .user_question_recipients(id)
            .map_err(|e| e.to_string())?
        {
            let run = crate::meta::parse_run_id(&run)?;
            if self.entry(run).is_ok_and(|entry| {
                matches!(
                    *entry.phase_rx.borrow(),
                    event_bus::AgentRunPhase::Pending
                        | event_bus::AgentRunPhase::Running
                        | event_bus::AgentRunPhase::Waiting
                )
            }) || self
                .admission_snapshot(run)
                .is_some_and(|admission| admission.result.is_none())
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn inherit_user_questions(
        &self,
        source: RunId,
        target: RunId,
        messages: &[providers::Message],
    ) -> Result<(), String> {
        let Some(store) = self.shared.run_store.get() else {
            return Ok(());
        };
        let questions = store.user_questions(source).map_err(|e| e.to_string())?;
        let ids = questions
            .iter()
            .filter(|question| {
                question.answer.is_none() || !Self::answer_is_in_history(messages, question)
            })
            .map(|question| question.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        store
            .handle
            .bind_user_questions(&source.to_string(), &target.to_string(), &ids)
            .map_err(|e| e.to_string())
    }

    pub(crate) fn answer_is_in_history(
        messages: &[providers::Message],
        question: &UserQuestion,
    ) -> bool {
        let text = question.answer_message();
        messages.iter().any(|message| message.role == providers::Role::User && message.content.iter().any(|block| matches!(block, providers::ContentBlock::Text {text:existing} if existing == &text)))
    }

    /// Blocking child questions that require this parent to make a decision.
    pub(crate) fn pending_direct_child_questions(
        &self,
        parent: RunId,
    ) -> Result<Vec<RunId>, String> {
        let children = {
            let runs = super::lock_runs(&self.shared.runs);
            runs.iter()
                .filter_map(|(id, entry)| {
                    (entry.parent == Some(parent)
                        && !matches!(
                            *entry.phase_rx.borrow(),
                            AgentRunPhase::Done | AgentRunPhase::Error
                        ))
                    .then_some(*id)
                })
                .collect::<Vec<_>>()
        };
        let mut pending = Vec::new();
        for child in children {
            if self
                .user_answers(child)?
                .iter()
                .any(|question| question.blocking && question.answer.is_none())
            {
                pending.push(child);
            }
        }
        pending.sort();
        Ok(pending)
    }

    /// Models can inspect their own questions and explicitly inherited questions.
    pub fn user_answers(&self, run: RunId) -> Result<Vec<UserQuestion>, String> {
        self.entry(run).map_err(|e| e.to_string())?;
        match self.shared.run_store.get() {
            Some(store) => store.user_questions(run).map_err(|e| e.to_string()),
            None => Ok(Vec::new()),
        }
    }
    pub(crate) fn has_pending_user_questions(&self, run: RunId) -> bool {
        self.user_answers(run)
            .is_ok_and(|questions| questions.iter().any(|q| q.blocking && q.answer.is_none()))
    }
    pub(crate) fn has_unanswered_questions(&self, run: RunId) -> bool {
        self.user_answers(run)
            .is_ok_and(|questions| questions.iter().any(|q| q.answer.is_none()))
    }
}

/// Resolve only explicit edges, rejecting an absent legacy lineage rather than
/// treating the question's cached root as authority.
fn answer_evidence_root(
    store: &crate::RunStore,
    recipient: &str,
    registered: bool,
) -> Result<String, String> {
    let mut lineage = store
        .review_lineage_ancestors(recipient)
        .map_err(|error| error.to_string())?;
    if !registered
        && lineage.is_empty()
        && store
            .review_evidence_for_lineage(&[recipient.to_string()])
            .map_err(|error| error.to_string())?
            .is_empty()
        && store
            .restore_record(crate::meta::parse_run_id(recipient)?)
            .map_err(|error| error.to_string())?
            .is_none()
    {
        return Err("review lineage is unavailable for question recipient".into());
    }
    lineage.push(recipient.to_string());
    let mut roots = Vec::new();
    for run in lineage {
        if store
            .review_lineage_edges(&run)
            .map_err(|error| error.to_string())?
            .is_empty()
        {
            roots.push(run);
        }
    }
    if roots.len() != 1 {
        return Err("review lineage has no unique root".into());
    }
    Ok(roots.remove(0))
}
