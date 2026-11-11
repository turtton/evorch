//! What a person sees and decides when ending a lane by hand (ADR 0044 §9).
//!
//! The ledger records `human_closed` ("completed by human decision, unverified") or
//! `discarded`. Both only end the ledger entry: they stop no run, delete no thread or
//! worktree and detach nothing. Reviews, evidence and the pull request are shown as
//! reference; none of them decides whether the person may close.

use event_bus::lane::{
    LaneCloseKind, LaneClosePrContext, LaneCloseResult, LanePrContextStatus, LaneReadyState,
};
use storage::lane_control::LaneClosePreview;

/// What closing means, shown in the confirmation and as the tooltip of its buttons.
pub const MEANING: &str = "This only records the lane as ended in the ledger. It does not stop its run, delete its thread or worktree, or detach it. It is your decision, not a verification.";

/// Longest note the ledger accepts, in bytes.
pub const NOTE_MAX_BYTES: usize = storage::lane_control::LANE_CLOSE_NOTE_MAX_BYTES;

/// Which ending the person chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseChoice {
    /// The person judges the lane done, with no verification behind it.
    Complete,
    /// The person ends the lane without claiming it is done.
    Discard,
}

impl CloseChoice {
    /// The button on the attention row.
    #[must_use]
    pub const fn button(self) -> &'static str {
        match self {
            Self::Complete => "Close…",
            Self::Discard => "Discard…",
        }
    }

    /// The confirm button of the dialog.
    #[must_use]
    pub const fn confirm(self) -> &'static str {
        match self {
            Self::Complete => "Close lane",
            Self::Discard => "Discard lane",
        }
    }

    /// The dialog's title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Complete => "Close lane (unverified)",
            Self::Discard => "Discard lane",
        }
    }

    /// How the ledger will word the result.
    #[must_use]
    pub const fn recorded_as(self) -> &'static str {
        match self {
            Self::Complete => "Recorded as: completed by human decision (unverified)",
            Self::Discard => "Recorded as: discarded",
        }
    }

    /// Whether the request ends the lane without claiming it is done.
    #[must_use]
    pub const fn discards(self) -> bool {
        matches!(self, Self::Discard)
    }
}

/// The review of the Ready in a sentence.
#[must_use]
pub const fn review_line(readiness: LaneReadyState) -> &'static str {
    match readiness {
        LaneReadyState::NoEvidence => "Review: none, and no fixed artifact is named.",
        LaneReadyState::Unreviewed => "Review: none yet; a fixed artifact is named.",
        LaneReadyState::ReviewInProgress => "Review: under way.",
        LaneReadyState::ReviewRejected => "Review: rejected this Ready.",
        LaneReadyState::ReviewIncomplete => "Review: ended without a complete result.",
        LaneReadyState::AwaitingClose => "Review: approved.",
    }
}

/// What the ledger holds about the lane's pull request, in a sentence.
#[must_use]
pub fn pr_line(context: &LaneClosePrContext) -> String {
    match (context.status, &context.evidence) {
        (LanePrContextStatus::NotLinked, _) => "Pull request: none linked.".to_owned(),
        (LanePrContextStatus::NoSnapshot, _) => {
            "Pull request: linked; no facts are stored yet.".to_owned()
        }
        (LanePrContextStatus::LastFetchFailed, evidence) => format!(
            "Pull request: the last fetch failed{}.",
            if evidence.is_some() {
                "; older facts are stored"
            } else {
                ""
            }
        ),
        (LanePrContextStatus::Available, Some(evidence)) => format!(
            "Pull request: {}#{} at {} (stored facts).",
            evidence.repository,
            evidence.number,
            evidence.head_sha.chars().take(12).collect::<String>()
        ),
        (LanePrContextStatus::Available, None) => "Pull request: stored facts exist.".to_owned(),
    }
}

/// The artifact the Ready names, in a sentence.
#[must_use]
pub fn evidence_line(ready: &event_bus::lane::LaneReport) -> String {
    let refs = ready.artifact_refs.len();
    match (&ready.artifact_revision, refs) {
        (None, 0) => "Evidence: no fixed artifact and no references.".to_owned(),
        (None, n) => format!("Evidence: {n} reference(s), no fixed artifact revision."),
        (Some(_), n) => format!("Evidence: a fixed artifact revision and {n} reference(s)."),
    }
}

/// Why a screen that was drawn earlier cannot decide: the lane is not what the list item
/// said. `None` when it still matches. The ledger repeats this check with the revisions the
/// dialog sends; this one only keeps the person from deciding on something they did not see.
#[must_use]
pub fn moved_since_listed(
    preview: &LaneClosePreview,
    listed_management_revision: u64,
    listed_ready_seq: Option<u64>,
) -> Option<String> {
    if preview.context.management_revision != listed_management_revision {
        return Some("The lane's management changed since the list was drawn.".to_owned());
    }
    if let Some(seq) = listed_ready_seq
        && preview.context.latest_ready_seq != Some(seq)
    {
        return Some("The lane reported again since the list was drawn.".to_owned());
    }
    None
}

/// The badge text of a lane's close, which never reads like a verified one.
#[must_use]
pub const fn close_label(close: &LaneCloseResult) -> &'static str {
    match close.kind {
        LaneCloseKind::LeadClosed => "closed by lead",
        LaneCloseKind::HumanClosed => "completed by human decision (unverified)",
        LaneCloseKind::Discarded => "discarded",
    }
}

/// Who decided and why, as a line under the badge.
#[must_use]
pub fn close_detail(close: &LaneCloseResult) -> String {
    use event_bus::lane::LaneCloseActor;
    let who = match &close.closed_by {
        Some(LaneCloseActor::Lead { .. }) => "the lead",
        Some(LaneCloseActor::Human { surface, .. }) if surface == "desktop" => "you (desktop)",
        Some(LaneCloseActor::Human { .. }) => "a person",
        None => "unknown",
    };
    let mut line = format!("Ended by {who}; no run was stopped and nothing was deleted.");
    if !close.note.is_empty() {
        line.push_str(&format!(" Note: {}", close.note));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::lane::{LaneCloseActor, LaneCloseOutcome};

    fn close(kind: LaneCloseKind, actor: Option<LaneCloseActor>) -> LaneCloseResult {
        LaneCloseResult {
            lane_id: "lane".into(),
            close_id: "lc".into(),
            kind,
            ready_seq: None,
            closed_revision: 1,
            management_revision: 1,
            brief_revision: 1,
            report_brief_revision: None,
            artifact_refs: vec![],
            artifact_revision: None,
            note: "because".into(),
            review: None,
            pr_context: None,
            closed_by: actor,
            at_ns: 0,
            outcome: LaneCloseOutcome::Closed,
        }
    }

    #[test]
    fn a_person_ending_a_lane_never_reads_like_a_lead_close_or_a_verified_one() {
        let human = |kind| {
            close(
                kind,
                Some(LaneCloseActor::Human {
                    surface: "desktop".into(),
                    client_id: None,
                }),
            )
        };
        let labels = [
            close_label(&close(LaneCloseKind::LeadClosed, None)),
            close_label(&human(LaneCloseKind::HumanClosed)),
            close_label(&human(LaneCloseKind::Discarded)),
        ];
        assert_eq!(
            labels,
            [
                "closed by lead",
                "completed by human decision (unverified)",
                "discarded"
            ]
        );
        assert!(close_detail(&human(LaneCloseKind::HumanClosed)).starts_with("Ended by you"));
        assert!(
            close_detail(&close(
                LaneCloseKind::LeadClosed,
                Some(LaneCloseActor::Lead {
                    thread_id: "t".into(),
                    root_run_id: "r".into()
                })
            ))
            .starts_with("Ended by the lead")
        );
    }

    #[test]
    fn closing_says_it_is_a_ledger_entry_only_and_discarding_does_not_claim_completion() {
        assert!(MEANING.contains("does not stop its run"));
        assert!(CloseChoice::Complete.recorded_as().contains("unverified"));
        assert!(!CloseChoice::Discard.recorded_as().contains("completed"));
        assert!(CloseChoice::Discard.discards() && !CloseChoice::Complete.discards());
    }
}
