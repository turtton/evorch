//! What the GUI keeps of the lane ledger's attention list (ADR 0044 Phase 2).
//!
//! The list is derived by storage from the lane ledger; this board is only the last answer,
//! held per lead in the wire form. It makes no decision: it has no timer, never marks an item
//! handled, and an item leaves it only when a later read no longer reports it. Notifications
//! are a separate model, so clearing or acknowledging them changes nothing here.
//!
//! Reads can finish out of order, and other windows write the same ledger. Every read carries
//! the number it was started with, and an answer that is not newer than the one already held is
//! refused, so an old projection never replaces a newer one. An action is only run for an item
//! the latest answer still holds, unchanged.

use std::collections::BTreeMap;

use event_bus::UserQuestion;
use workbench_protocol::lane_attention::{
    LaneAttentionItem, LaneAttentionKind, LaneAttentionSnapshot, LaneQuestionResponder,
};
use workspace_ui::{ThreadId, ThreadRecord};

pub mod wire;

/// The text shown instead of an answer control while a lead owns a lane's question.
pub const LEAD_HANDLING_LABEL: &str = "The lead is handling this question";

/// One lead's attention list as one read of the ledger returned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadRead {
    pub items: Vec<LaneAttentionItem>,
    pub snapshot: LaneAttentionSnapshot,
    /// More items exist than the read was willing to fetch.
    pub truncated: bool,
}

#[derive(Debug, Default)]
struct LeadBoard {
    /// The number of the newest read applied.
    applied: u64,
    read: Option<LeadRead>,
    /// Why the newest read failed; the previous list stays shown beside it.
    error: Option<String>,
}

#[derive(Debug, Default)]
pub struct AttentionBoard {
    leads: BTreeMap<String, LeadBoard>,
}

impl AttentionBoard {
    /// Applies the answer of read number `seq` for `lead`. A read that is not newer than the one
    /// held is refused (`false`). Returns whether anything shown changed.
    pub fn apply(&mut self, lead: &str, seq: u64, result: Result<LeadRead, String>) -> bool {
        let board = self.leads.entry(lead.to_owned()).or_default();
        if seq <= board.applied {
            return false;
        }
        board.applied = seq;
        match result {
            Ok(read) => {
                let changed = board.read.as_ref() != Some(&read) || board.error.is_some();
                board.read = Some(read);
                board.error = None;
                changed
            }
            Err(error) => {
                let changed = board.error.as_ref() != Some(&error);
                board.error = Some(error);
                changed
            }
        }
    }

    /// Every held item, newest cause first.
    #[must_use]
    pub fn items(&self) -> Vec<&LaneAttentionItem> {
        let mut items: Vec<&LaneAttentionItem> = self
            .leads
            .values()
            .filter_map(|board| board.read.as_ref())
            .flat_map(|read| read.items.iter())
            .collect();
        items.sort_by(|a, b| b.since.cmp(&a.since).then_with(|| a.id.cmp(&b.id)));
        items
    }

    /// Whether any lead has been read, so an empty list can be told from not asked.
    #[must_use]
    pub fn has_reads(&self) -> bool {
        self.leads
            .values()
            .any(|board| board.read.is_some() || board.error.is_some())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items().is_empty()
    }

    /// Whether the latest answer for `lead` still holds `id` at `revision`.
    #[must_use]
    pub fn holds(&self, lead: &str, id: &str, revision: u64) -> bool {
        self.leads
            .get(lead)
            .and_then(|board| board.read.as_ref())
            .is_some_and(|read| {
                read.items
                    .iter()
                    .any(|item| item.id.as_str() == id && item.revision.get() == revision)
            })
    }

    /// The leads whose latest read failed, with the reason.
    #[must_use]
    pub fn errors(&self) -> Vec<(&str, &str)> {
        self.leads
            .iter()
            .filter_map(|(lead, board)| Some((lead.as_str(), board.error.as_deref()?)))
            .collect()
    }

    /// Whether a read stopped before the end of a lead's list.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.leads
            .values()
            .any(|board| board.read.as_ref().is_some_and(|read| read.truncated))
    }
}

/// An item with what the pane needs to draw it. Built from the sidebar and the question
/// projection at draw time; it holds no state.
#[derive(Debug)]
pub struct AttentionRow<'a> {
    pub item: &'a LaneAttentionItem,
    pub lane_title: String,
    pub lead_title: String,
    /// The lane's thread, when the sidebar has it.
    pub lane_thread: Option<ThreadId>,
    /// The question to answer inline: only for a question the ledger says the user answers.
    pub answerable: Option<&'a UserQuestion>,
}

/// Whether the ledger leaves this question to the lead: no answer control may be drawn.
#[must_use]
pub const fn lead_handles(kind: &LaneAttentionKind) -> bool {
    matches!(
        kind,
        LaneAttentionKind::QuestionPending {
            responder: LaneQuestionResponder::Lead { .. },
            ..
        }
    )
}

#[must_use]
pub fn rows<'a>(
    board: &'a AttentionBoard,
    threads: &[ThreadRecord],
    questions: &'a BTreeMap<String, UserQuestion>,
) -> Vec<AttentionRow<'a>> {
    let title_of = |thread: &str| {
        threads
            .iter()
            .find(|candidate| candidate.id.to_string() == thread)
            .map_or_else(|| thread.to_owned(), |candidate| candidate.title.clone())
    };
    board
        .items()
        .into_iter()
        .map(|item| {
            let lane_record = threads.iter().find(|thread| {
                thread
                    .lane
                    .as_ref()
                    .is_some_and(|lane| lane.lane_id == item.lane.as_str())
            });
            let answerable = match &item.kind {
                LaneAttentionKind::QuestionPending {
                    question,
                    responder: LaneQuestionResponder::User,
                    ..
                } if lane_record.is_some() => questions
                    .get(question.as_str())
                    .filter(|question| question.answer.is_none() && question.is_user_visible()),
                _ => None,
            };
            AttentionRow {
                item,
                lane_title: lane_record.map_or_else(
                    || item.lane.as_str().to_owned(),
                    |thread| thread.title.clone(),
                ),
                lead_title: title_of(item.lead_thread.as_str()),
                lane_thread: lane_record.map(|thread| thread.id.clone()),
                answerable,
            }
        })
        .collect()
}

/// What the user chose on an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionOp {
    /// Show the lane's thread.
    OpenLane(ThreadId),
    /// Continue the lead's conversation (the user's own path; held notices follow).
    ContinueLead,
    /// The user's explicit resume of the lead's held lane notices.
    ResumeDelivery,
    /// Read the lead's list again.
    Refresh,
}

/// An operation with the item it was chosen on, so a screen older than the latest read cannot
/// run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionAction {
    pub lead: String,
    /// The item's id and revision as drawn; `None` for a refresh.
    pub item: Option<(String, u64)>,
    pub op: AttentionOp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::ids::{QuestionId, ThreadId as WireThread};
    use workbench_protocol::lane_attention::{LaneAttentionAction, LaneAttentionItemId, LaneId};
    use workbench_protocol::wire::{Revision, UnixMillis};

    fn item(id: &str, revision: u64, since: u64, kind: LaneAttentionKind) -> LaneAttentionItem {
        LaneAttentionItem {
            id: LaneAttentionItemId::new(id).unwrap(),
            lane: LaneId::new("lane-1").unwrap(),
            lead_thread: WireThread::new("lead").unwrap(),
            management_revision: Revision(1),
            revision: Revision(revision),
            kind,
            since: UnixMillis(since),
            summary: id.into(),
            evidence_refs: vec![],
            available_actions: vec![LaneAttentionAction::Inspect],
        }
    }

    fn blocked() -> LaneAttentionKind {
        LaneAttentionKind::Blocked {
            report_seq: workbench_protocol::wire::Sequence(1),
        }
    }

    fn read(items: Vec<LaneAttentionItem>) -> LeadRead {
        LeadRead {
            items,
            snapshot: LaneAttentionSnapshot::new("snapshot").unwrap(),
            truncated: false,
        }
    }

    #[test]
    fn an_answer_that_is_not_newer_than_the_one_held_is_refused() {
        let mut board = AttentionBoard::default();
        assert!(board.apply("lead", 2, Ok(read(vec![item("new", 2, 5, blocked())]))));

        // An older read finishing late, and a repeat of the same number, change nothing.
        assert!(!board.apply("lead", 1, Ok(read(vec![item("old", 1, 4, blocked())]))));
        assert!(!board.apply("lead", 2, Ok(read(vec![]))));
        assert!(!board.apply("lead", 1, Err("late failure".into())));

        let ids: Vec<_> = board
            .items()
            .iter()
            .map(|item| item.id.to_string())
            .collect();
        assert_eq!(ids, ["new"]);
        assert!(board.errors().is_empty());
    }

    #[test]
    fn an_item_leaves_only_when_a_later_read_no_longer_reports_it() {
        let mut board = AttentionBoard::default();
        board.apply("lead", 1, Ok(read(vec![item("a", 1, 1, blocked())])));

        // A failed read keeps the last list and says why it is not fresh.
        assert!(board.apply("lead", 2, Err("database busy".into())));
        assert_eq!(board.items().len(), 1);
        assert_eq!(board.errors(), [("lead", "database busy")]);

        // The next good read replaces the list and clears the error.
        assert!(board.apply("lead", 3, Ok(read(vec![]))));
        assert!(board.is_empty());
        assert!(board.errors().is_empty());
        assert!(board.has_reads(), "an empty answer is not 'never asked'");
    }

    #[test]
    fn items_of_every_lead_are_listed_newest_cause_first() {
        let mut board = AttentionBoard::default();
        board.apply("lead-a", 1, Ok(read(vec![item("a", 1, 10, blocked())])));
        board.apply("lead-b", 1, Ok(read(vec![item("b", 1, 30, blocked())])));
        board.apply("lead-c", 1, Ok(read(vec![item("c", 1, 20, blocked())])));

        let ids: Vec<_> = board
            .items()
            .iter()
            .map(|item| item.id.to_string())
            .collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }

    #[test]
    fn an_action_is_only_for_the_item_the_latest_read_still_holds_unchanged() {
        let mut board = AttentionBoard::default();
        board.apply("lead", 1, Ok(read(vec![item("a", 3, 1, blocked())])));

        assert!(board.holds("lead", "a", 3));
        assert!(
            !board.holds("lead", "a", 2),
            "an older revision of the item"
        );
        assert!(!board.holds("lead", "b", 3));
        assert!(!board.holds("other-lead", "a", 3));
        board.apply("lead", 2, Ok(read(vec![item("a", 4, 1, blocked())])));
        assert!(
            !board.holds("lead", "a", 3),
            "the cause changed since the screen was drawn"
        );
    }

    #[test]
    fn only_the_user_routed_question_of_a_known_lane_can_be_answered_inline() {
        let question = |responder| LaneAttentionKind::QuestionPending {
            question: QuestionId::new("q").unwrap(),
            responder,
            routing_revision: Revision(1),
        };
        let lead_kind = question(LaneQuestionResponder::Lead {
            lead_thread: WireThread::new("lead").unwrap(),
        });
        assert!(lead_handles(&lead_kind));
        assert!(!lead_handles(&question(LaneQuestionResponder::User)));
        assert!(!lead_handles(&blocked()));

        let mut board = AttentionBoard::default();
        board.apply(
            "lead",
            1,
            Ok(read(vec![item(
                "q",
                1,
                1,
                question(LaneQuestionResponder::User),
            )])),
        );
        let mut questions = BTreeMap::new();
        let mut asked = UserQuestion {
            responder: Some(event_bus::lane::QuestionResponder::User),
            routing_revision: 1,
            management_revision: Some(1),
            id: "q".into(),
            run_id: "lane-root".into(),
            root_run_id: "lane-root".into(),
            root_name: "chat:Orchestrator:lane-thread".into(),
            recipient_run_ids: vec![],
            title: "scope".into(),
            options: vec![],
            blocking: false,
            answer: None,
            answer_origin: None,
        };
        questions.insert("q".to_owned(), asked.clone());
        let mut lane = ThreadRecord::new(
            ThreadId::new("lane-thread"),
            workspace_ui::ProjectId::new("p"),
            "Lane",
        );
        lane.lane = Some(workspace_ui::LaneLink {
            lane_id: "lane-1".into(),
            origin_thread_id: ThreadId::new("lead"),
            lead_thread_id: Some(ThreadId::new("lead")),
            policy: workspace_ui::LanePolicy::Attached,
            management_revision: 1,
        });

        // The ledger says User, the lane is known and the projection agrees: answerable.
        assert!(
            rows(&board, std::slice::from_ref(&lane), &questions)[0]
                .answerable
                .is_some()
        );
        // The lane's thread is unknown: only the lane id is shown and nothing is answerable.
        assert!(rows(&board, &[], &questions)[0].answerable.is_none());
        // The projection still says the lead owns it (event not yet seen): not answerable.
        asked.responder = Some(event_bus::lane::QuestionResponder::Lead {
            thread_id: "lead".into(),
        });
        questions.insert("q".to_owned(), asked.clone());
        assert!(
            rows(&board, std::slice::from_ref(&lane), &questions)[0]
                .answerable
                .is_none()
        );
        // Already answered: nothing to answer.
        asked.responder = Some(event_bus::lane::QuestionResponder::User);
        asked.answer = Some("done".into());
        questions.insert("q".to_owned(), asked);
        assert!(
            rows(&board, std::slice::from_ref(&lane), &questions)[0]
                .answerable
                .is_none()
        );
    }
}
