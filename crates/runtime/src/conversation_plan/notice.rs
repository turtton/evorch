//! One-time current-plan notice for conversations that existed before the plan tools
//! replaced the retired todo tool. A conversation started after the switch never
//! needs it, so its first request is unchanged.
use crate::agent_loop::LoopState;
use serde::{Deserialize, Serialize};

/// Recovery state: whether the notice is still owed to this conversation. A saved
/// run that predates the field, and a fork or rewind seed, owe it; a run started
/// after the switch does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanNotice {
    Pending,
    /// A verified tools-only restart boundary still owes its first response.
    /// Persisting this state keeps an interrupted boundary retry notice-free.
    Deferred,
    #[default]
    Done,
}

impl PlanNotice {
    pub(crate) const fn pending() -> Self {
        Self::Pending
    }

    pub(crate) fn response_received(&mut self) {
        if *self == Self::Deferred {
            *self = Self::Pending;
        }
    }
}

impl LoopState {
    /// Called with the other boundary context, right before a provider request
    /// that is needed anyway. Existing history is never edited, nothing is sent
    /// as a request of its own, and a prepared-but-unsent input or an exact
    /// retry stays byte-identical. Marked done only once the text is appended.
    pub(crate) fn append_plan_notice(&mut self) {
        if self.plan_notice != PlanNotice::Pending
            || self.boundary_context_deferred
            || self.boundary_pending
            || !super::conversation_root(self)
        {
            return;
        }
        let Some(runtime) = self.runtime() else {
            return;
        };
        let Some(thread) = runtime.goal_thread(self.caller_run_id()) else {
            return;
        };
        let Ok(summary) = runtime.conversation_plan_summary(&thread) else {
            return;
        };
        self.plan_notice = PlanNotice::Done;
        let progress = &summary.progress;
        let state = if summary.revision == 0 || progress.node_count == 0 {
            "No plan exists yet for this conversation.".to_string()
        } else {
            format!(
                "Plan revision {}: {} tasks, leaves pending {}, in_progress {}, completed {}, blocked {}, interrupted {}.",
                summary.revision,
                progress.node_count,
                progress.leaf_counts.pending,
                progress.leaf_counts.in_progress,
                progress.leaf_counts.completed,
                progress.leaf_counts.blocked,
                progress.leaf_counts.interrupted,
            )
        };
        self.context.push_user(&format!(
            "[runtime conversation plan state]\nThe retired todo tool and its lists are gone and were not carried over; older todo lists in this history are obsolete. Use plan_read and plan_update for the current plan. {state}"
        ));
        self.publish_message_count();
    }
}
