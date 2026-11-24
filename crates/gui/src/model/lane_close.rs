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
use workbench_protocol::lane_management::LaneCloseRecordKind;

use crate::panes::lane_widgets;
pub use crate::panes::lane_widgets::{
    ARTIFACT_REFS_SHOWN, CloseChoice, MEANING, NO_READY_WARNING, OLDER_BRIEF_WARNING, REMAINS,
    RUNNING_WARNING, open_questions_line,
};

/// Whether the Ready a preview shows answers the lane's current brief. `None` when there is
/// no Ready for the current management at all.
#[must_use]
pub fn ready_answers_current_brief(preview: &LaneClosePreview) -> Option<bool> {
    preview.ready.as_ref()?;
    Some(preview.context.ready_brief_revision == Some(preview.context.brief_revision))
}

/// The warning a person ending a lane as done reads about its Ready, if any.
#[must_use]
pub fn ready_warning(preview: &LaneClosePreview) -> Option<&'static str> {
    match ready_answers_current_brief(preview) {
        None => Some(NO_READY_WARNING),
        Some(false) => Some(OLDER_BRIEF_WARNING),
        Some(true) => None,
    }
}

/// Longest note the ledger accepts, in bytes.
pub const NOTE_MAX_BYTES: usize = storage::lane_control::LANE_CLOSE_NOTE_MAX_BYTES;

/// The protocol's word for the ledger's kind of close; the variants are the same.
#[must_use]
pub(crate) const fn record_kind(kind: LaneCloseKind) -> LaneCloseRecordKind {
    match kind {
        LaneCloseKind::LeadClosed => LaneCloseRecordKind::LeadClosed,
        LaneCloseKind::HumanClosed => LaneCloseRecordKind::HumanClosed,
        LaneCloseKind::Discarded => LaneCloseRecordKind::Discarded,
    }
}

/// The review of the Ready in a sentence.
#[must_use]
pub const fn review_line(readiness: LaneReadyState) -> &'static str {
    use workbench_protocol::lane_attention::LaneReadyState as Wire;
    lane_widgets::review_line(match readiness {
        LaneReadyState::NoEvidence => Wire::NoEvidence,
        LaneReadyState::Unreviewed => Wire::Unreviewed,
        LaneReadyState::ReviewInProgress => Wire::ReviewInProgress,
        LaneReadyState::ReviewRejected => Wire::ReviewRejected,
        LaneReadyState::ReviewIncomplete => Wire::ReviewIncomplete,
        LaneReadyState::AwaitingClose => Wire::AwaitingClose,
    })
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

/// The artifact the Ready names, exactly as the lane reported it: the fixed revision, then at
/// most [`ARTIFACT_REFS_SHOWN`] references (a pull request URL, a commit SHA, a file), then
/// how many more there are. Nothing is checked or fetched, and nothing here decides whether the
/// person may close.
#[must_use]
pub fn artifact_lines(ready: &event_bus::lane::LaneReport) -> Vec<String> {
    use event_bus::lane::ArtifactRevision;
    let revision = ready
        .artifact_revision
        .as_ref()
        .map(|revision| match revision {
            ArtifactRevision::CommitSha(value) => ("commit", value.as_str()),
            ArtifactRevision::Sha256(value) => ("sha256", value.as_str()),
        });
    lane_widgets::artifact_lines(revision, &ready.artifact_refs, ready.artifact_refs.len())
}

/// The artifact the Ready names, in a sentence.
#[must_use]
pub fn evidence_line(ready: &event_bus::lane::LaneReport) -> String {
    lane_widgets::evidence_line(ready.artifact_revision.is_some(), ready.artifact_refs.len())
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

/// What a lane's row in the sidebar shows once its close stands: a short word, and the same
/// words the lane view uses on hover. It is a record of the ledger, never a state of any run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneCloseBadge {
    pub text: &'static str,
    pub hover: String,
}

impl LaneCloseBadge {
    #[must_use]
    pub fn of(close: &LaneCloseResult) -> Self {
        Self {
            text: lane_widgets::close_badge_text(record_kind(close.kind)),
            hover: format!("{}\n{}", close_label(close), close_detail(close)),
        }
    }
}

/// The badge text of a lane's close, which never reads like a verified one.
#[must_use]
pub const fn close_label(close: &LaneCloseResult) -> &'static str {
    lane_widgets::close_kind_label(record_kind(close.kind))
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
    fn open_questions_are_counted_for_the_person_and_a_badge_never_reads_as_a_verified_close() {
        assert_eq!(open_questions_line(0), None);
        assert!(open_questions_line(1).unwrap().starts_with("1 question "));
        assert!(open_questions_line(3).unwrap().starts_with("3 questions "));
        let human = LaneCloseBadge::of(&close(LaneCloseKind::HumanClosed, None));
        assert_eq!(human.text, "closed (unverified)");
        assert_eq!(
            LaneCloseBadge::of(&close(LaneCloseKind::Discarded, None)).text,
            "discarded"
        );
    }

    fn ready_with(
        refs: &[&str],
        revision: Option<event_bus::lane::ArtifactRevision>,
    ) -> event_bus::lane::LaneReport {
        event_bus::lane::LaneReport {
            artifact_revision: revision,
            failure_episode: None,
            lane_id: "lane".into(),
            report_seq: 1,
            status: event_bus::lane::LaneStatus::Ready,
            summary: "done".into(),
            artifact_refs: refs.iter().map(|r| (*r).to_owned()).collect(),
            needs_human: false,
            question_refs: vec![],
            reason_code: None,
            producer: event_bus::lane::LaneReportProducer::LaneAgent,
            management_revision: 1,
            brief_revision: 1,
            created_at_ns: 0,
        }
    }

    #[test]
    fn the_artifacts_are_shown_as_reported_capped_and_clipped() {
        use event_bus::lane::ArtifactRevision;
        assert!(artifact_lines(&ready_with(&[], None)).is_empty());

        let few = artifact_lines(&ready_with(
            &["https://example.test/pull/1"],
            Some(ArtifactRevision::Sha256("b".repeat(64))),
        ));
        assert_eq!(
            few,
            [
                format!("revision (sha256): {}", "b".repeat(64)),
                "https://example.test/pull/1".to_owned()
            ]
        );

        let many: Vec<String> = (0..ARTIFACT_REFS_SHOWN + 3)
            .map(|n| format!("ref-{n}"))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let lines = artifact_lines(&ready_with(&many, None));
        assert_eq!(lines.len(), ARTIFACT_REFS_SHOWN + 1);
        assert_eq!(lines.last().unwrap(), "… and 3 more");

        let long = "x".repeat(500);
        let clipped = artifact_lines(&ready_with(&[&long], None));
        assert!(clipped[0].chars().count() <= 201 && clipped[0].ends_with('…'));
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
