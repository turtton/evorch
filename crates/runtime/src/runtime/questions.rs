//! Durable, non-blocking clarification requests. These never grant tool permissions.
use crate::{AgentRuntime, RunId};
use event_bus::{AgentRunPhase, Event, ToolEvent, UserQuestion};

impl AgentRuntime {
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
            title,
            options,
            blocking,
            answer: None,
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
    pub fn answer_subagent_question(
        &self,
        caller: RunId,
        id: &str,
        answer: &str,
    ) -> Result<UserQuestion, String> {
        let question = self.user_question(id)?.ok_or("unknown question")?;
        let child = crate::meta::parse_run_id(&question.run_id)?;
        self.validate_question_parent(caller, child)?;
        self.answer_user_question(id, answer)
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
        let question = self.user_question(id)?.ok_or("unknown question")?;
        if question.answer.as_deref() == Some(answer) {
            return Ok(question);
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("question storage is not configured")?;
        let recipients = store
            .user_question_recipients(id)
            .map_err(|e| e.to_string())?;
        let mut permits = Vec::new();
        for recipient in recipients {
            let run = crate::meta::parse_run_id(&recipient)?;
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
        store
            .handle
            .answer_user_question(id, answer)
            .map_err(|e| e.to_string())?;
        drop(guards);
        let question = store
            .user_question(id)
            .map_err(|e| e.to_string())?
            .ok_or("question disappeared")?;
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
        let text = format!(
            "[user-answer id={}]\nQuestion: {}\nAnswer: {}",
            question.id,
            question.title,
            question.answer.as_deref().unwrap_or_default()
        );
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
        pending.sort_by_key(|id| id.get());
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
}
