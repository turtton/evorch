//! Read-only classification of a live interactive boundary. A Waiting phase
//! alone also covers tool, child, lane, question and escalation waits.
use super::super::{AgentRunPhase, AgentRuntime, RunEntry, RunId, lock_runs};
use crate::{RuntimeError, runtime::unknown_run};
use std::sync::atomic::Ordering;

impl AgentRuntime {
    /// True only after a successful interactive turn, with no queued wake-up.
    /// This observes live authority; it never acknowledges messages or answers,
    /// changes provider input, or infers activity from saved/replayed events.
    pub fn is_between_chat_turns(&self, run: RunId) -> bool {
        lock_runs(&self.shared.runs)
            .get(&run)
            .is_some_and(between_turns)
    }

    /// Observe phases and turn-boundary authority together, rather than pairing
    /// an older list_agents snapshot with a newer boundary observation.
    pub fn has_active_runs(&self) -> bool {
        lock_runs(&self.shared.runs).values().any(|entry| {
            let phase = *entry.phase_rx.borrow();
            matches!(phase, AgentRunPhase::Pending | AgentRunPhase::Running)
                || (phase == AgentRunPhase::Waiting && !between_turns(entry))
        })
    }
}

pub(in crate::runtime) fn between_turns(entry: &RunEntry) -> bool {
    let input = entry.user_inbox.status();
    *entry.phase_rx.borrow() == AgentRunPhase::Waiting
        && entry.idle_turn.is_some()
        && !entry.compaction_busy.load(Ordering::Acquire)
        && !entry.user_inbox.sealed()
        && input.pending == 0
        && !input.next_turn_requested
        && entry.inbox_tx.capacity() == entry.inbox_tx.max_capacity()
        && !entry.mailbox.has_interrupting_message()
        // Every wake guards before consuming. Read adoption AFTER all queues:
        // an earlier zero could race a later dequeue. The run-map lock prevents
        // public Running from releasing this guard during the observation.
        && !entry.user_inbox.adopting()
}

impl AgentRuntime {
    /// Exact live protection for one member; no owner/claim lookup and no I/O.
    pub fn retention_run_protected(&self, run: RunId) -> bool {
        let pending = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&run)
            .is_some_and(|a| !a.cancelled && a.result.borrow().is_none());
        if pending {
            return true;
        }
        let runs = lock_runs(&self.shared.runs);
        runs.get(&run).is_some_and(|entry| {
            entry.compaction_busy.load(Ordering::Acquire)
                || match *entry.phase_rx.borrow() {
                    AgentRunPhase::Pending | AgentRunPhase::Running => true,
                    AgentRunPhase::Waiting => !between_turns(entry),
                    _ => false,
                }
        })
    }
}

impl AgentRuntime {
    /// Deletion requires proof that the entire task exited. No-join entries hold.
    pub fn retention_delete_protected(&self, run: RunId) -> bool {
        let pending = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&run)
            .is_some_and(|a| !a.cancelled && a.result.borrow().is_none());
        if pending {
            return true;
        }
        let runs = lock_runs(&self.shared.runs);
        runs.get(&run).is_some_and(|entry| {
            !matches!(
                *entry.phase_rx.borrow(),
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            ) || entry.compaction_busy.load(Ordering::Acquire)
                || entry._join.as_ref().is_none_or(|join| !join.is_finished())
                || entry.user_inbox.status().pending != 0
                || entry.mailbox.has_interrupting_message()
        })
    }
    /// Await the actual task tail without holding the run map across suspension.
    pub async fn wait_retention_task(&self, run: RunId) -> Result<(), RuntimeError> {
        futures_util::future::poll_fn(|cx| {
            let mut runs = lock_runs(&self.shared.runs);
            let Some(entry) = runs.get_mut(&run) else {
                return std::task::Poll::Ready(Ok(()));
            };
            let Some(join) = entry._join.as_mut() else {
                return std::task::Poll::Ready(Err(unknown_run(run)));
            };
            if join.is_finished() {
                return std::task::Poll::Ready(Ok(()));
            }
            std::future::Future::poll(std::pin::Pin::new(join), cx).map(|_| Ok(()))
        })
        .await
    }
}
