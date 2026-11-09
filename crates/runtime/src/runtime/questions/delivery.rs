//! Ordinary delegation uses the existing mailbox/held path, not the lane outbox.
//! No storage IO is performed while runs, delivery state or held queues are locked.
use super::super::{is_live, lock_runs};
use crate::mailbox::{QuestionNotice, QueuedMessage};
use crate::{AgentRuntime, RunId};
use event_bus::{AgentMessage, AgentMessageKind, UserQuestion};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, atomic::Ordering};

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;

pub(crate) const QUESTION_WAKE_LIMIT: u32 = 20;
#[derive(Default)]
pub(crate) struct QuestionDelivery {
    notices: HashMap<String, Arc<QuestionNotice>>,
    reported: HashSet<String>,
    recovered_parents: HashSet<RunId>,
    wakes: HashMap<(RunId, RunId), u32>, // (conversation root, direct child)
    idle_batches: HashMap<(RunId, RunId), u64>, // (parent, child) -> idle generation
    pub(in crate::runtime) root_requests: HashSet<RunId>,
}

impl AgentRuntime {
    pub(crate) fn question_reported(&self, id: &str) -> bool {
        let state = self
            .shared
            .question_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reported.contains(id) || state.notices.get(id).is_some_and(|n| n.observed())
    }

    /// A model-facing snapshot acknowledges exactly its question IDs. This also
    /// handles the narrow save -> receipt-registration race without reading storage.
    pub(crate) fn observe_questions(&self, ids: impl IntoIterator<Item = String>) {
        let mut state = self
            .shared
            .question_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in ids {
            if let Some(notice) = state.notices.get(&id) {
                notice.observe();
            }
            state.reported.insert(id);
        }
    }

    pub(in crate::runtime) fn queue_subagent_question(&self, question: &UserQuestion) {
        let Ok(child) = question.run_id.parse::<RunId>() else {
            return;
        };
        let parent = lock_runs(&self.shared.runs)
            .get(&child)
            .and_then(|entry| entry.parent);
        if let Some(parent) = parent {
            self.queue_question_for_parent(parent, child, question);
        }
    }

    fn queue_question_for_parent(&self, parent: RunId, child: RunId, question: &UserQuestion) {
        if question.answer.is_some() || question.responder.is_some() {
            return;
        }
        let runs = lock_runs(&self.shared.runs);
        let Some(entry) = runs
            .get(&parent)
            .filter(|e| e.role == crate::Role::Orchestrator)
        else {
            return;
        };
        let notice = {
            let mut state = self
                .shared
                .question_delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.reported.contains(&question.id) || state.notices.contains_key(&question.id) {
                return;
            }
            let notice = Arc::new(QuestionNotice::new(
                question.id.clone(),
                parent,
                child,
                question.blocking,
            ));
            state.notices.insert(question.id.clone(), notice.clone());
            notice
        };
        let queued = QueuedMessage::question(
            AgentMessage {
                message_id: format!("question-notice-{}", question.id),
                // No SentRecord is created for runtime notices: reply is not an answer.
                sender_run_id: "runtime".into(),
                recipient_run_id: parent.to_string(),
                kind: AgentMessageKind::Steering,
                content: format!(
                    "[subagent-question question_id={} child_run_id={child} blocking={}]\nRuntime notification, not user input or permission. Read subagent_questions(run_id=\"{child}\") and answer with answer_subagent_question(question_id=\"{}\", answer=...). Do not reply to this notification.",
                    question.id, question.blocking, question.id
                ),
                reply_to: None,
            },
            notice.clone(),
        );
        let stopped = self.operator_stopped(parent);
        let active = is_live(entry) && !entry.mailbox.is_closed() && !stopped;
        let idle = entry.idle_turn.is_some();
        let root = conversation_root(&runs, parent);
        let admitted = if active && idle {
            let generation = entry
                .idle_turn
                .as_ref()
                .expect("idle checked")
                .notice
                .generation;
            let already = self
                .shared
                .question_delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .idle_batches
                .get(&(parent, child))
                == Some(&generation);
            if already {
                notice.admitted.store(true, Ordering::Release);
                true
            } else {
                let admitted =
                    self.admit_question_batch(root, parent, std::slice::from_ref(&notice), false);
                if admitted {
                    self.shared
                        .question_delivery
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .idle_batches
                        .insert((parent, child), generation);
                }
                admitted
            }
        } else {
            true
        };
        if active && admitted && entry.mailbox.try_push_queued(queued.clone()).is_ok() {
            // Accepted by a live mailbox = delivery, not yet model observation.
            return;
        }
        self.hold_queued_message(parent, queued);
        drop(runs);
        if !stopped {
            self.schedule_wake(parent);
        }
    }

    /// Call only after classifying a held queue, with runs locked. A mixed batch
    /// uses AUTO_WAKE if available; its fallback is this child-specific budget.
    pub(in crate::runtime) fn admit_question_batch(
        &self,
        root: RunId,
        parent: RunId,
        notices: &[Arc<QuestionNotice>],
        terminal: bool,
    ) -> bool {
        let mut state = self
            .shared
            .question_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending: Vec<_> = notices.iter().filter(|n| !n.observed()).collect();
        if terminal && !pending.iter().any(|n| n.blocking) {
            return false;
        }
        let children: HashSet<_> = pending.iter().map(|n| n.child).collect();
        let mut any = false;
        for child in children {
            let batch: Vec<_> = pending.iter().filter(|n| n.child == child).collect();
            if batch.iter().any(|n| n.admitted.load(Ordering::Acquire)) {
                for notice in &batch {
                    notice.admitted.store(true, Ordering::Release);
                    notice.paused.store(false, Ordering::Release);
                }
                any |= batch.iter().any(|n| !terminal || n.blocking);
                continue;
            }
            let count = state.wakes.entry((root, child)).or_default();
            if *count >= QUESTION_WAKE_LIMIT {
                let mut announce = false;
                for notice in &batch {
                    // Visit every receipt; `any` would leave later ones unpaused.
                    announce |= !notice.paused.swap(true, Ordering::AcqRel);
                }
                if announce {
                    tracing::warn!(%parent, %child, question_id = %batch[0].id, "question wake limit reached; send user input or explicitly resume the conversation");
                    self.shared.bus.emit(event_bus::Event::new(
                        event_bus::LifecycleEvent::AutoContinuationPaused {
                            run_id: parent.to_string(),
                            limit: QUESTION_WAKE_LIMIT,
                        },
                    ));
                }
                continue;
            }
            *count += 1;
            for notice in &batch {
                notice.admitted.store(true, Ordering::Release);
            }
            any |= batch.iter().any(|n| !terminal || n.blocking);
        }
        any
    }

    pub(in crate::runtime) fn reset_question_wakes(&self, root: RunId) {
        let mut state = self
            .shared
            .question_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let children: HashSet<_> = state
            .wakes
            .keys()
            .filter(|(r, _)| *r == root)
            .map(|(_, child)| *child)
            .collect();
        state.wakes.retain(|(r, _), _| *r != root);
        state
            .idle_batches
            .retain(|(parent, child), _| *parent != root && !children.contains(child));
        state.root_requests.remove(&root);
        // Receipt identity survives resumption; neither answers nor new IDs reset guard.
        for notice in state.notices.values() {
            if notice.parent == root || children.contains(&notice.child) {
                notice.paused.store(false, Ordering::Release);
                notice.admitted.store(false, Ordering::Release);
            }
        }
    }

    /// Validate a delayed host wake again. Authority and Stop are also fenced by
    /// wake_chat's existing restore/ownership gates, without changing its module.
    pub fn prepare_held_wake(
        &self,
        run: RunId,
        authority: &crate::RunConfig,
    ) -> Result<bool, crate::RuntimeError> {
        if self.operator_stopped(run) {
            return Ok(false);
        }
        if let Some(permit) = &authority.ownership {
            permit
                .mutation_guard()
                .map_err(|_| crate::RuntimeError::StaleOwnership {
                    run_id: run.to_string(),
                })?;
        }
        let _runs = lock_runs(&self.shared.runs);
        if self.operator_stopped(run) {
            return Ok(false);
        }
        let mut held = self
            .shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let eligible = held.get_mut(&run).is_some_and(|queue| {
            queue.retain(|q| !q.observed());
            queue.iter().any(|q| {
                q.question_notice()
                    .is_none_or(|n| n.blocking && !n.paused.load(Ordering::Acquire))
            })
        });
        drop(held);
        if !eligible {
            self.shared
                .question_delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .root_requests
                .remove(&run);
        }
        Ok(eligible)
    }

    /// Retry backpressure at an actual tool/turn boundary, without store IO.
    pub(crate) fn refill_question_mailbox(&self, run: RunId) {
        if self.operator_stopped(run) {
            return;
        }
        let runs = lock_runs(&self.shared.runs);
        let Some(entry) = runs.get(&run).filter(|e| is_live(e)) else {
            return;
        };
        let pending = self
            .shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .is_some_and(|q| {
                q.iter().any(|m| {
                    !m.observed()
                        && m.question_notice()
                            .is_some_and(|n| !n.paused.load(Ordering::Acquire))
                })
            });
        if pending {
            self.release_held_messages(run, &entry.mailbox);
        }
    }

    /// Event-driven retry: a parent/ancestor registration or real input, never a
    /// timer. Restart recovery starts only when that parent is explicitly resumed.
    pub(in crate::runtime) fn schedule_question_recovery(&self, run: RunId) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let runtime = self.clone();
        handle.spawn(async move {
            runtime.recover_question_delivery(run);
        });
    }

    fn recover_question_delivery(&self, run: RunId) {
        if self.operator_stopped(run) {
            return;
        }
        let orchestrator = lock_runs(&self.shared.runs)
            .get(&run)
            .is_some_and(|entry| entry.role == crate::Role::Orchestrator);
        let recover = orchestrator
            && self.shared.run_store.get().is_some()
            && self
                .shared
                .question_delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recovered_parents
                .insert(run);
        if let Some(store) = self.shared.run_store.get().filter(|_| recover) {
            // Read the store before any runtime lock; missing children can be
            // restored lazily under the resumed parent's authority when answered.
            let questions = store.pending_questions().unwrap_or_else(|error| {
                tracing::warn!(%run, %error, "could not recover question notices");
                self.shared
                    .question_delivery
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .recovered_parents
                    .remove(&run);
                Vec::new()
            });
            for question in questions {
                let Ok(child) = question.run_id.parse::<RunId>() else {
                    continue;
                };
                if store
                    .restore_parent(child)
                    .ok()
                    .flatten()
                    .flatten()
                    .as_deref()
                    == Some(&run.to_string())
                {
                    self.queue_question_for_parent(run, child, &question);
                }
            }
        }
        let pending: Vec<_> = {
            let runs = lock_runs(&self.shared.runs);
            let held = self
                .shared
                .held_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            held.keys()
                .copied()
                .filter(|id| *id == run || runs.get(id).and_then(|e| e.parent) == Some(run))
                .collect()
        };
        for recipient in pending {
            let runs = lock_runs(&self.shared.runs);
            if self.operator_stopped(recipient) {
                continue;
            }
            if let Some(entry) = runs
                .get(&recipient)
                .filter(|e| is_live(e) && !e.mailbox.is_closed())
            {
                self.release_held_messages(recipient, &entry.mailbox);
            } else {
                drop(runs);
                self.schedule_wake(recipient);
            }
        }
    }
}

pub(in crate::runtime) fn conversation_root(
    runs: &HashMap<RunId, super::super::RunEntry>,
    mut root: RunId,
) -> RunId {
    let mut visited = HashSet::new();
    while visited.insert(root) {
        let Some(parent) = runs.get(&root).and_then(|entry| entry.parent) else {
            break;
        };
        root = parent;
    }
    root
}
