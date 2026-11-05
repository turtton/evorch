//! What changed between two fetches of a pull request, and what is worth telling a person.
//!
//! Pure functions over the stored facts, so the rules are testable without a service. A
//! fetch that finds nothing different yields nothing: no event, no notification.

use event_bus::PullRequestChangedField;
use storage::pull_requests::{
    CheckConclusion, CheckStatus, PullRequestFacts, PullRequestState, ReviewDecision,
};

/// The notification kinds a change can give reason for (the `kind` of a notification key).
pub mod notification {
    /// The pull request was merged.
    pub const MERGED: &str = "merged";
    /// The pull request was closed without merging.
    pub const CLOSED: &str = "closed";
    /// A closed pull request was reopened.
    pub const REOPENED: &str = "reopened";
    /// Checks finished and some failed.
    pub const CHECKS_FAILED: &str = "checks_failed";
    /// Checks finished and all passed.
    pub const CHECKS_PASSED: &str = "checks_passed";
    /// The review decision became "approved".
    pub const REVIEW_APPROVED: &str = "review_approved";
    /// The review decision became "changes requested".
    pub const REVIEW_CHANGES_REQUESTED: &str = "review_changes_requested";
}

/// How the checks of a pull request stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksVerdict {
    /// No check is known, or the list is known to be partial and nothing failed in it.
    Unknown,
    /// Some check has not finished.
    Pending,
    /// A finished check failed.
    Failing,
    /// Every check finished and passed (skipped and neutral ones count as passed).
    Passing,
    /// All finished, none failed, but not all passed (cancelled, stale, needs a person, …).
    Unsettled,
}

fn failed(conclusion: Option<CheckConclusion>) -> bool {
    matches!(
        conclusion,
        Some(
            CheckConclusion::Failure | CheckConclusion::TimedOut | CheckConclusion::StartupFailure
        )
    )
}

fn passed(conclusion: Option<CheckConclusion>) -> bool {
    matches!(
        conclusion,
        Some(CheckConclusion::Success | CheckConclusion::Neutral | CheckConclusion::Skipped)
    )
}

/// The verdict of a fact set. A failure counts even when the list is partial: a known
/// failure does not become less true because more checks exist.
#[must_use]
pub fn checks_verdict(facts: &PullRequestFacts) -> ChecksVerdict {
    if facts
        .checks
        .iter()
        .any(|check| check.status == CheckStatus::Completed && failed(check.conclusion))
    {
        return ChecksVerdict::Failing;
    }
    if facts
        .checks
        .iter()
        .any(|check| check.status != CheckStatus::Completed)
    {
        return ChecksVerdict::Pending;
    }
    if facts.checks.is_empty() || !facts.checks_complete {
        return ChecksVerdict::Unknown;
    }
    if facts.checks.iter().all(|check| passed(check.conclusion)) {
        ChecksVerdict::Passing
    } else {
        ChecksVerdict::Unsettled
    }
}

/// Whether the next fetch should come soon: nothing is known yet, a check is still running,
/// or the list of checks is not the whole list.
#[must_use]
pub fn is_pending(facts: Option<&PullRequestFacts>) -> bool {
    facts.is_none_or(|facts| {
        !facts.checks_complete
            || facts
                .checks
                .iter()
                .any(|check| check.status != CheckStatus::Completed)
    })
}

/// Whether the pull request will not change by itself any more.
#[must_use]
pub fn is_terminal(facts: Option<&PullRequestFacts>) -> bool {
    facts.is_some_and(|facts| facts.state != PullRequestState::Open)
}

/// The difference between two fetches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FactsChange {
    /// The parts that differ.
    pub fields: Vec<PullRequestChangedField>,
    /// The notification kinds the change gives reason for. Empty for the first fetch: it
    /// is a baseline, not a transition.
    pub notifications: Vec<&'static str>,
}

/// Compares the facts of the previous successful fetch (if any) with the new ones.
#[must_use]
pub fn compare(previous: Option<&PullRequestFacts>, next: &PullRequestFacts) -> FactsChange {
    let Some(previous) = previous else {
        return FactsChange {
            fields: vec![
                PullRequestChangedField::State,
                PullRequestChangedField::HeadSha,
                PullRequestChangedField::Refs,
                PullRequestChangedField::Checks,
                PullRequestChangedField::ChecksComplete,
                PullRequestChangedField::ReviewDecision,
            ],
            notifications: Vec::new(),
        };
    };
    let mut change = FactsChange::default();
    if previous.state != next.state {
        change.fields.push(PullRequestChangedField::State);
        change.notifications.push(match next.state {
            PullRequestState::Merged => notification::MERGED,
            PullRequestState::Closed => notification::CLOSED,
            PullRequestState::Open => notification::REOPENED,
        });
    }
    if previous.head_sha != next.head_sha {
        change.fields.push(PullRequestChangedField::HeadSha);
    }
    if previous.base_branch != next.base_branch
        || previous.head_repository != next.head_repository
        || previous.head_branch != next.head_branch
    {
        change.fields.push(PullRequestChangedField::Refs);
    }
    if previous.checks != next.checks {
        change.fields.push(PullRequestChangedField::Checks);
    }
    if previous.checks_complete != next.checks_complete {
        change.fields.push(PullRequestChangedField::ChecksComplete);
    }
    if previous.review_decision != next.review_decision {
        change.fields.push(PullRequestChangedField::ReviewDecision);
        match next.review_decision {
            ReviewDecision::Approved => change.notifications.push(notification::REVIEW_APPROVED),
            ReviewDecision::ChangesRequested => change
                .notifications
                .push(notification::REVIEW_CHANGES_REQUESTED),
            ReviewDecision::ReviewRequired | ReviewDecision::None => {}
        }
    }
    if next.state == PullRequestState::Open {
        let (before, after) = (checks_verdict(previous), checks_verdict(next));
        let new_head = previous.head_sha != next.head_sha;
        if after == ChecksVerdict::Failing && (before != ChecksVerdict::Failing || new_head) {
            change.notifications.push(notification::CHECKS_FAILED);
        } else if after == ChecksVerdict::Passing && (before != ChecksVerdict::Passing || new_head)
        {
            change.notifications.push(notification::CHECKS_PASSED);
        }
    }
    change
}

#[cfg(test)]
mod tests {
    use storage::pull_requests::{CheckKind, PullRequestCheck};

    use super::*;

    fn facts() -> PullRequestFacts {
        PullRequestFacts {
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_repository: Some("o/r".into()),
            head_branch: "feat".into(),
            head_sha: "a".repeat(40),
            checks: Vec::new(),
            checks_complete: true,
            review_decision: ReviewDecision::None,
        }
    }

    fn check(
        name: &str,
        status: CheckStatus,
        conclusion: Option<CheckConclusion>,
    ) -> PullRequestCheck {
        PullRequestCheck {
            provider_id: name.into(),
            kind: CheckKind::CheckRun,
            name: name.into(),
            status,
            conclusion,
            head_sha: "a".repeat(40),
        }
    }

    // Given: the same facts twice / When: compared / Then: nothing changed, nothing to tell
    #[test]
    fn identical_facts_change_nothing() {
        let mut one = facts();
        one.checks = vec![check(
            "ci",
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
        )];

        assert_eq!(compare(Some(&one), &one.clone()), FactsChange::default());
    }

    // Given: no previous fetch / When: compared / Then: every field is new but nothing is
    // announced: the first fetch is a baseline
    #[test]
    fn the_first_fetch_is_a_baseline_without_notifications() {
        let mut merged = facts();
        merged.state = PullRequestState::Merged;

        let change = compare(None, &merged);

        assert_eq!(change.fields.len(), 6);
        assert!(change.notifications.is_empty());
    }

    // Given: the state moving between open, closed and merged / When: compared / Then: each
    // move names its own notification
    #[test]
    fn state_moves_are_announced() {
        let open = facts();
        let mut merged = facts();
        merged.state = PullRequestState::Merged;
        let mut closed = facts();
        closed.state = PullRequestState::Closed;

        assert_eq!(
            compare(Some(&open), &merged).notifications,
            vec![notification::MERGED]
        );
        assert_eq!(
            compare(Some(&open), &closed).notifications,
            vec![notification::CLOSED]
        );
        assert_eq!(
            compare(Some(&closed), &open).notifications,
            vec![notification::REOPENED]
        );
    }

    // Given: checks going pending → failing → failing on a new head → passing / When:
    // compared / Then: failure is announced once per failing head, and passing once
    #[test]
    fn check_verdicts_announce_transitions_not_repeats() {
        let mut pending = facts();
        pending.checks = vec![check("ci", CheckStatus::InProgress, None)];
        let mut failing = facts();
        failing.checks = vec![check(
            "ci",
            CheckStatus::Completed,
            Some(CheckConclusion::Failure),
        )];
        let mut failing_again = failing.clone();
        failing_again.checks[0].provider_id = "ci-2".into();
        let mut new_head = failing.clone();
        new_head.head_sha = "b".repeat(40);
        let mut passing = facts();
        passing.checks = vec![check(
            "ci",
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
        )];

        assert_eq!(
            compare(Some(&pending), &failing).notifications,
            vec![notification::CHECKS_FAILED]
        );
        assert!(
            compare(Some(&failing), &failing_again)
                .notifications
                .is_empty()
        );
        assert_eq!(
            compare(Some(&failing), &new_head).notifications,
            vec![notification::CHECKS_FAILED]
        );
        assert_eq!(
            compare(Some(&failing), &passing).notifications,
            vec![notification::CHECKS_PASSED]
        );
    }

    // Given: review decisions / When: compared / Then: approval and a change request are
    // announced, a withdrawn decision is only a field change
    #[test]
    fn review_decisions_are_announced_when_they_decide() {
        let none = facts();
        let mut approved = facts();
        approved.review_decision = ReviewDecision::Approved;
        let mut changes = facts();
        changes.review_decision = ReviewDecision::ChangesRequested;

        assert_eq!(
            compare(Some(&none), &approved).notifications,
            vec![notification::REVIEW_APPROVED]
        );
        assert_eq!(
            compare(Some(&approved), &changes).notifications,
            vec![notification::REVIEW_CHANGES_REQUESTED]
        );
        let withdrawn = compare(Some(&approved), &none);
        assert_eq!(
            withdrawn.fields,
            vec![PullRequestChangedField::ReviewDecision]
        );
        assert!(withdrawn.notifications.is_empty());
    }

    // Given: a merged pull request whose checks change / When: compared / Then: no check
    // notification: the pull request is done
    #[test]
    fn a_finished_pull_request_announces_no_check_results() {
        let mut before = facts();
        before.state = PullRequestState::Merged;
        before.checks = vec![check("ci", CheckStatus::InProgress, None)];
        let mut after = before.clone();
        after.checks = vec![check(
            "ci",
            CheckStatus::Completed,
            Some(CheckConclusion::Failure),
        )];

        assert!(compare(Some(&before), &after).notifications.is_empty());
    }

    // Given: the verdict rules / When: evaluated / Then: an empty or partial list is never
    // "passing", and an explicit failure wins over a partial list
    #[test]
    fn only_a_complete_list_that_all_passed_is_passing() {
        let mut empty = facts();
        assert_eq!(checks_verdict(&empty), ChecksVerdict::Unknown);
        empty.checks = vec![check(
            "ci",
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
        )];
        assert_eq!(checks_verdict(&empty), ChecksVerdict::Passing);
        empty.checks_complete = false;
        assert_eq!(checks_verdict(&empty), ChecksVerdict::Unknown);
        empty.checks.push(check(
            "bad",
            CheckStatus::Completed,
            Some(CheckConclusion::TimedOut),
        ));
        assert_eq!(checks_verdict(&empty), ChecksVerdict::Failing);
        let mut cancelled = facts();
        cancelled.checks = vec![check(
            "ci",
            CheckStatus::Completed,
            Some(CheckConclusion::Cancelled),
        )];
        assert_eq!(checks_verdict(&cancelled), ChecksVerdict::Unsettled);
    }

    // Given: a branch that moved without anything else changing / When: compared / Then: that
    // alone is a change
    #[test]
    fn a_changed_branch_or_repository_is_a_change() {
        let before = facts();
        let mut moved = facts();
        moved.head_branch = "other".into();
        let mut renamed = facts();
        renamed.head_repository = None;
        let mut rebased = facts();
        rebased.base_branch = "release".into();

        for next in [moved, renamed, rebased] {
            let change = compare(Some(&before), &next);
            assert_eq!(change.fields, vec![PullRequestChangedField::Refs]);
            assert!(change.notifications.is_empty());
        }
    }

    // Given: facts / When: asked whether to poll soon / Then: unknown, running and partial
    // are pending; settled is not
    #[test]
    fn pending_means_unknown_running_or_partial() {
        let mut settled = facts();
        settled.checks = vec![check(
            "ci",
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
        )];
        let mut running = settled.clone();
        running.checks[0].status = CheckStatus::Queued;
        running.checks[0].conclusion = None;
        let mut partial = settled.clone();
        partial.checks_complete = false;

        assert!(is_pending(None));
        assert!(is_pending(Some(&running)));
        assert!(is_pending(Some(&partial)));
        assert!(!is_pending(Some(&settled)));
        assert!(!is_terminal(Some(&settled)));
        let mut merged = settled;
        merged.state = PullRequestState::Merged;
        assert!(is_terminal(Some(&merged)));
    }
}
