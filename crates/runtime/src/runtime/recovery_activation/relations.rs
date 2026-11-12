//! Private queues and shared observation receipts; preparation has no side effects.
use super::*;
use crate::mailbox::{CompletionNotice, QuestionNotice, QueuedMessage};
use crate::quiesce::{DurableDelivery, DurableDeliveryState, DurableNotice};
use crate::restore::RunRestoreDescriptor;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

pub(super) struct PreparedRelations {
    completions: BTreeMap<RunId, Arc<CompletionNotice>>,
    questions: BTreeMap<String, Arc<QuestionNotice>>,
    deliveries: Vec<DurableDelivery>,
    queues: BTreeMap<RunId, Vec<(DurableDeliveryState, QueuedMessage)>>,
    wakes: BTreeMap<RunId, (u32, u64)>,
    question_states: BTreeMap<String, (bool, bool, bool)>,
    next_message_id: u64,
    ancestries: BTreeMap<RunId, crate::request_turn::RequestAncestry>,
}

fn merge_generation(
    generations: &mut BTreeMap<RunId, (u64, bool)>,
    child: RunId,
    generation: u64,
    observed: bool,
) -> Result<(), RuntimeError> {
    if generation == u64::MAX {
        return Err(error("completion generation overflow"));
    }
    let previous = generations.entry(child).or_insert((generation, false));
    if previous.0 != generation {
        return Err(error("completion generation conflict"));
    }
    previous.1 |= observed;
    Ok(())
}

impl PreparedRelations {
    pub(super) fn prepare(sources: &[RecoverySource]) -> Result<Self, RuntimeError> {
        let ids = sources
            .iter()
            .map(|s| s.record.run_id)
            .collect::<BTreeSet<_>>();
        let mut deliveries = BTreeMap::<String, DurableDelivery>::new();
        let mut next_message_id = 1;
        let mut generations = BTreeMap::new();
        let mut questions = BTreeMap::<String, Arc<QuestionNotice>>::new();
        let mut wakes = BTreeMap::new();
        for source in sources {
            let r = &source.record;
            if r.parent.is_some_and(|parent| !ids.contains(&parent)) || !ids.contains(&r.root) {
                return Err(error("missing recovery observation endpoint"));
            }
            if let Some(old) = wakes.insert(r.root, (r.auto_wakes, r.user_reset_generation))
                && old != (r.auto_wakes, r.user_reset_generation)
            {
                return Err(error("conflicting recovery wake/reset counters"));
            }
            if let Some(generation) = r.completion_generation {
                merge_generation(&mut generations, r.run_id, generation, false)?;
            }
            if let Some(observed) = &r.completion {
                if observed.id != r.run_id.to_string() {
                    return Err(error("invalid completion observation identity"));
                }
                merge_generation(&mut generations, r.run_id, observed.generation, true)?;
            }
            for delivery in &r.deliveries {
                if let Some(number) = delivery.message_id.strip_prefix("msg-") {
                    let number: u64 = number.parse().map_err(error)?;
                    next_message_id = next_message_id.max(
                        number
                            .checked_add(1)
                            .ok_or_else(|| error("recovery message ID overflow"))?,
                    );
                }
                if delivery.recipient != r.run_id || !ids.contains(&delivery.recipient) {
                    return Err(error("invalid delivery recipient"));
                }
                match delivery.state {
                    DurableDeliveryState::Consumed => {
                        if delivery.consumed_receipt.as_deref()
                            != Some(delivery.message_id.as_str())
                        {
                            return Err(error("consumed delivery has no exact receipt"));
                        }
                    }
                    _ => {
                        if delivery.consumed_receipt.is_some() || !ids.contains(&delivery.sender) {
                            return Err(error("invalid unconsumed delivery endpoint/receipt"));
                        }
                        let message = delivery
                            .message
                            .as_ref()
                            .ok_or_else(|| error("missing delivery body"))?;
                        if message.message_id != delivery.message_id
                            || message.sender_run_id
                                != match &delivery.notice {
                                    DurableNotice::Question { .. } => "runtime".to_string(),
                                    _ => delivery.sender.to_string(),
                                }
                            || message.recipient_run_id != delivery.recipient.to_string()
                        {
                            return Err(error("delivery body identity mismatch"));
                        }
                    }
                }
                if let Some(old) = deliveries.insert(delivery.message_id.clone(), delivery.clone())
                    && old != *delivery
                {
                    return Err(error("conflicting duplicate recovery message"));
                }
                match &delivery.notice {
                    DurableNotice::Explicit => {}
                    DurableNotice::Completion {
                        child,
                        recipient,
                        generation,
                        observed,
                    } => {
                        let child_source = sources
                            .iter()
                            .find(|s| s.record.run_id == *child)
                            .ok_or_else(|| error("missing completion endpoint"))?;
                        if *child != delivery.sender
                            || *recipient != Some(delivery.recipient)
                            || *recipient != child_source.record.parent
                            || *generation == u64::MAX
                        {
                            return Err(error("completion notice identity mismatch"));
                        }
                        match generations.entry(*child) {
                            std::collections::btree_map::Entry::Vacant(e) => {
                                e.insert((*generation, *observed));
                            }
                            std::collections::btree_map::Entry::Occupied(mut e) => {
                                if e.get().0 != *generation {
                                    return Err(error("completion generation conflict"));
                                }
                                e.get_mut().1 |= *observed;
                            }
                        }
                    }
                    DurableNotice::Question {
                        id,
                        parent,
                        child,
                        blocking,
                        observed,
                        admitted,
                        paused,
                    } => {
                        if *parent != delivery.recipient
                            || *child != delivery.sender
                            || !sources.iter().any(|s| {
                                s.record.run_id == *child && s.record.parent == Some(*parent)
                            })
                        {
                            return Err(error("question notice endpoint mismatch"));
                        }
                        let notice = questions.entry(id.clone()).or_insert_with(|| {
                            Arc::new(QuestionNotice::new(id.clone(), *parent, *child, *blocking))
                        });
                        if notice.parent != *parent
                            || notice.child != *child
                            || notice.blocking != *blocking
                        {
                            return Err(error("question notice identity conflict"));
                        }
                        if *observed {
                            notice.observe();
                        }
                        notice.admitted.fetch_or(*admitted, Ordering::AcqRel);
                        notice.paused.fetch_or(*paused, Ordering::AcqRel);
                    }
                }
            }
        }
        let completions = sources
            .iter()
            .map(|source| {
                let r = &source.record;
                let (generation, observed) =
                    generations.get(&r.run_id).copied().unwrap_or_default();
                let notice = CompletionNotice::new(r.parent, r.run_id, generation);
                if observed {
                    notice.observe();
                }
                (r.run_id, notice)
            })
            .collect::<BTreeMap<_, _>>();
        let mut deliveries = deliveries.into_values().collect::<Vec<_>>();
        deliveries.sort_by(|a, b| {
            (a.recipient, a.sequence, &a.message_id).cmp(&(b.recipient, b.sequence, &b.message_id))
        });
        let mut queues = BTreeMap::<RunId, Vec<_>>::new();
        for delivery in &deliveries {
            if delivery.state == DurableDeliveryState::Consumed {
                continue;
            }
            let message = delivery.message.clone().expect("validated body");
            let queued = match &delivery.notice {
                DurableNotice::Explicit => QueuedMessage::explicit(message),
                DurableNotice::Completion { child, .. } => {
                    QueuedMessage::completion(message, completions[child].clone())
                }
                DurableNotice::Question { id, .. } => {
                    QueuedMessage::question(message, questions[id].clone())
                }
            };
            queues
                .entry(delivery.recipient)
                .or_default()
                .push((delivery.state.clone(), queued));
        }
        if queues.values().any(|q| {
            q.iter().filter(|(_, m)| !m.observed()).count() > crate::mailbox::MAILBOX_CAPACITY
        }) {
            return Err(error("recovery mailbox would overflow"));
        }
        let question_states = questions
            .iter()
            .map(|(id, n)| {
                (
                    id.clone(),
                    (
                        n.observed(),
                        n.admitted.load(Ordering::Acquire),
                        n.paused.load(Ordering::Acquire),
                    ),
                )
            })
            .collect();
        Ok(Self {
            completions,
            questions,
            deliveries,
            queues,
            wakes,
            question_states,
            next_message_id,
            ancestries: sources
                .iter()
                .map(|source| {
                    (
                        source.record.run_id,
                        crate::request_turn::RequestAncestry::new(
                            source.record.parent,
                            source.record.parent.is_some_and(|parent| {
                                sources
                                    .iter()
                                    .any(|s| s.record.run_id == parent && s.record.parent.is_some())
                            }),
                        ),
                    )
                })
                .collect(),
        })
    }

    pub(super) fn reconcile_current(&mut self, runtime: &AgentRuntime) -> Result<(), RuntimeError> {
        // Reserve the ID range before checking live sent records. In-flight sends
        // hold the sent lock, so that check also sees allocations preceding this fence.
        runtime
            .shared
            .next_message_id
            .fetch_max(self.next_message_id, Ordering::AcqRel);
        for (id, notice) in &mut self.questions {
            let question = runtime
                .user_question(id)
                .map_err(error)?
                .ok_or_else(|| error("missing durable recovery question"))?;
            if question.run_id != notice.child.to_string() || question.blocking != notice.blocking {
                return Err(error("durable recovery question identity conflict"));
            }
            let (existing, reported) = runtime
                .shared
                .question_delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recovery_receipt(id);
            self.question_states.get_mut(id).expect("known question").0 |= reported;
            if let Some(existing) = existing {
                if existing.parent != notice.parent
                    || existing.child != notice.child
                    || existing.blocking != notice.blocking
                {
                    return Err(error("current question identity conflicts with recovery"));
                }
                *notice = existing;
            }
        }
        for queues in self.queues.values_mut() {
            for (_, queued) in queues {
                if let crate::mailbox::completion::MessageOrigin::QuestionNotice(notice) =
                    &mut queued.origin
                {
                    *notice = self.questions[&notice.id].clone();
                }
            }
        }
        let sent = runtime
            .shared
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for d in &self.deliveries {
            if sent
                .get(&d.message_id)
                .is_some_and(|old| old.sender != d.sender || old.recipient != d.recipient)
            {
                return Err(error("current delivery identity conflicts with recovery"));
            }
        }
        drop(sent);
        let resets = runtime
            .shared
            .wake_reset_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (root, (_, generation)) in &self.wakes {
            if resets.get(root).is_some_and(|old| old != generation) {
                return Err(error("recovery reset generation changed"));
            }
        }
        Ok(())
    }

    fn connect(&self, run: RunId, entry: &mut RunEntry) -> Result<(), RuntimeError> {
        entry.completion_notice = self.completions[&run].clone();
        for (state, message) in self.queues.get(&run).into_iter().flatten() {
            if *state == DurableDeliveryState::Mailbox {
                entry
                    .mailbox
                    .try_push_queued(message.clone())
                    .map_err(|_| error("recovery mailbox rejected message"))?;
            }
        }
        Ok(())
    }

    pub(super) fn connect_candidate(
        &self,
        candidate: &mut PreparedRunCandidate,
    ) -> Result<(), RuntimeError> {
        // Use the validated component, not the not-yet-published runs registry.
        let ancestry = self.ancestries[&candidate.task.run_id];
        candidate.task.ancestry = ancestry;
        candidate.task.turn.is_subagent = ancestry.is_subagent;
        candidate.task.turn.parent_subagent_id =
            ancestry.parent_subagent_id.map(|id| id.to_string());
        self.connect(candidate.task.run_id, &mut candidate.entry)
    }

    /// Terminal members retain observation and mailbox endpoints, but no loop,
    /// provider admission, worker slot, or monitor is constructed.
    pub(super) fn terminal_endpoint(
        &self,
        source: &RecoverySource,
    ) -> Result<(RunId, RunEntry), RuntimeError> {
        let record = source
            .bundle
            .as_ref()
            .map(|b| &b.record)
            .ok_or_else(|| error("missing terminal snapshot"))?;
        let phase = match record.terminal_phase.as_str() {
            "Done" => AgentRunPhase::Done,
            "Error" => AgentRunPhase::Error,
            "Stopped" => AgentRunPhase::Stopped,
            _ => return Err(error("nonterminal observation-only member")),
        };
        let role = Role::from_name(&source.record.role).map_err(error)?;
        let descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(error)?;
        let (phase_tx, phase_rx) = watch::channel(phase);
        let (_, message_count_rx) = watch::channel(0);
        let (inbox_tx, _) = mpsc::channel(INBOX_CAPACITY);
        let (compact_tx, _) = watch::channel(0);
        let (model_preference_tx, _) = watch::channel(None);
        let messages: Vec<providers::Message> =
            serde_json::from_str(&record.messages_json).map_err(error)?;
        let result = messages
            .iter()
            .rev()
            .find(|m| m.role == providers::Role::Assistant)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        providers::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        let (_, result_rx) = watch::channel(result);
        let mut entry = RunEntry {
            terminal_reason: None,
            idle_turn: None,
            role,
            name: descriptor.name.unwrap_or_else(|| role.name().into()),
            model: source
                .intent
                .provider_input
                .as_ref()
                .map(|p| p.request.model.clone())
                .unwrap_or_default(),
            config: RunConfig::default(),
            parent: source.record.parent,
            escalated_from: None,
            completion_relayed: true,
            completion_notice: self.completions[&source.record.run_id].clone(),
            phase_tx,
            phase_rx,
            message_count_rx,
            inbox_tx,
            user_inbox: Arc::new(UserInbox::default()),
            pending_reference: Arc::new(Mutex::new(None)),
            execution: ExecutionControl::new(false),
            compact_tx,
            model_preference_tx,
            result_rx,
            compaction_busy: Arc::new(AtomicBool::new(false)),
            mailbox: Arc::new(RunMailbox::new()),
            _join: None,
        };
        self.connect(source.record.run_id, &mut entry)?;
        Ok((source.record.run_id, entry))
    }

    pub(super) fn publish(
        &mut self,
        runtime: &AgentRuntime,
        endpoints: &mut Vec<(RunId, RunEntry)>,
    ) {
        // Publication is infallible: queues/capacity/identities were checked
        // before ACK. No recovery_* writer is involved in later consumption.
        runtime
            .shared
            .next_message_id
            .fetch_max(self.next_message_id, Ordering::AcqRel);
        let mut runs = lock_runs(&runtime.shared.runs);
        for (id, entry) in endpoints.drain(..) {
            runs.insert(id, entry);
        }
        let mut held = runtime
            .shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (run, queues) in &self.queues {
            for (state, queued) in queues {
                if *state == DurableDeliveryState::Held {
                    held.entry(*run).or_default().push(queued.clone());
                }
            }
        }
        let mut sent = runtime
            .shared
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for delivery in &self.deliveries {
            if delivery.state != DurableDeliveryState::Held
                && !matches!(delivery.notice, DurableNotice::Question { .. })
                && delivery
                    .message
                    .as_ref()
                    .is_none_or(|m| m.kind != AgentMessageKind::Reply)
            {
                sent.insert(
                    delivery.message_id.clone(),
                    SentRecord {
                        sender: delivery.sender,
                        recipient: delivery.recipient,
                    },
                );
            }
        }
        for notice in self.completions.values() {
            runtime
                .shared
                .next_completion_generation
                .fetch_max(notice.generation + 1, Ordering::AcqRel);
        }
        for (id, notice) in &self.questions {
            let (observed, admitted, paused) = self.question_states[id];
            if observed {
                notice.observe();
            }
            notice.admitted.fetch_or(admitted, Ordering::AcqRel);
            notice.paused.fetch_or(paused, Ordering::AcqRel);
        }
        runtime
            .shared
            .question_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .adopt_recovery(self.questions.values().cloned());
        let mut wakes = runtime
            .shared
            .auto_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut resets = runtime
            .shared
            .wake_reset_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (root, (count, generation)) in &self.wakes {
            let existing = wakes.entry(*root).or_default();
            *existing = (*existing).max(*count);
            resets.insert(*root, *generation);
        }
    }
}
