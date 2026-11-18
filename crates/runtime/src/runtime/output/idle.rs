//! Read-only classification of a live interactive boundary. A Waiting phase
//! alone also covers tool, child, lane, question and escalation waits.
use super::super::{AgentRunPhase, AgentRuntime, RunEntry, RunId, lock_runs};
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

fn between_turns(entry: &RunEntry) -> bool {
    let input = entry.user_inbox.status();
    *entry.phase_rx.borrow() == AgentRunPhase::Waiting
        && entry.idle_turn.is_some()
        && !entry.compaction_busy.load(Ordering::Acquire)
        && input.pending == 0
        && !input.next_turn_requested
        && entry.inbox_tx.capacity() == entry.inbox_tx.max_capacity()
        && !entry.mailbox.has_interrupting_message()
        // Every wake guards before consuming. Read adoption AFTER all queues:
        // an earlier zero could race a later dequeue. The run-map lock prevents
        // public Running from releasing this guard during the observation.
        && !entry.user_inbox.adopting()
}
