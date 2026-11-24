//! The core client reads the same facts as protocol labels: its verdict, glyph and CI words
//! must be the embedded workbench's for every fact set.
use super::tests::{check, facts, view};
use super::*;
use crate::core_client::features::pull_requests::view as core_view;
use crate::panes::pull_request_widgets::{Verdict, verdict_of_labels};
use workbench_protocol::ids::ThreadId;
use workbench_protocol::namespaces::pull_requests as wire;
use workbench_protocol::wire::{Revision, Sequence, UnixMillis};

const STATUSES: [CheckStatus; 3] = [
    CheckStatus::Queued,
    CheckStatus::InProgress,
    CheckStatus::Completed,
];

const CONCLUSIONS: [Option<CheckConclusion>; 11] = [
    None,
    Some(CheckConclusion::Success),
    Some(CheckConclusion::Failure),
    Some(CheckConclusion::Neutral),
    Some(CheckConclusion::Cancelled),
    Some(CheckConclusion::Skipped),
    Some(CheckConclusion::TimedOut),
    Some(CheckConclusion::ActionRequired),
    Some(CheckConclusion::Stale),
    Some(CheckConclusion::StartupFailure),
    Some(CheckConclusion::Other),
];

fn singles() -> Vec<storage::pull_requests::PullRequestCheck> {
    STATUSES
        .iter()
        .flat_map(|status| {
            CONCLUSIONS
                .iter()
                .map(move |conclusion| check("c", *status, *conclusion))
        })
        .collect()
}

fn lists() -> Vec<Vec<storage::pull_requests::PullRequestCheck>> {
    let singles = singles();
    let mut lists = vec![Vec::new()];
    for a in &singles {
        lists.push(vec![a.clone()]);
        for b in &singles {
            lists.push(vec![a.clone(), b.clone()]);
        }
    }
    for a in &singles {
        for b in singles.iter().step_by(3) {
            for c in singles.iter().step_by(5) {
                lists.push(vec![a.clone(), b.clone(), c.clone()]);
            }
        }
    }
    lists
}

#[test]
fn the_verdict_over_labels_is_the_runtimes_verdict() {
    for checks in lists() {
        for complete in [true, false] {
            let facts = facts(PullRequestState::Open, checks.clone(), complete);
            let expected = match checks_verdict(&facts) {
                ChecksVerdict::Unknown => Verdict::Unknown,
                ChecksVerdict::Pending => Verdict::Pending,
                ChecksVerdict::Failing => Verdict::Failing,
                ChecksVerdict::Passing => Verdict::Passing,
                ChecksVerdict::Unsettled => Verdict::Unsettled,
            };
            assert_eq!(
                verdict_of_labels(&check_rows(&facts), complete),
                expected,
                "{checks:?} complete={complete}"
            );
        }
    }
}

fn as_protocol(legacy: &ThreadPullRequest) -> wire::ThreadPullRequest {
    let link = legacy.link.as_ref().map(|link| wire::LinkView {
        pull_request: wire::PullRequestRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: link.key.repository.clone(),
            number: link.key.number,
        },
        link_revision: Revision(link.link_revision),
        origin: wire::LinkOrigin::Explicit,
        linked_at: UnixMillis(0),
    });
    let facts = legacy.facts.as_ref().map(|facts| wire::FactsView {
        state: protocol_state(facts.state),
        base_branch: facts.base_branch.clone(),
        head_repository: facts.head_repository.clone(),
        head_branch: facts.head_branch.clone(),
        head_sha: facts.head_sha.clone(),
        checks: facts
            .checks
            .iter()
            .map(|check| wire::CheckView {
                name: check.name.clone(),
                kind: check.kind.as_str().to_owned(),
                status: check.status.as_str().to_owned(),
                conclusion: check.conclusion.map(|c| c.as_str().to_owned()),
            })
            .collect(),
        checks_complete: facts.checks_complete,
        review_decision: protocol_review(facts.review_decision),
    });
    wire::ThreadPullRequest {
        thread: ThreadId::new(legacy.thread_id.as_str()).unwrap(),
        tracking_enabled: true,
        owned_here: legacy.tracked_here,
        lifecycle_revision: Revision(legacy.lifecycle_revision),
        transition_seq: Sequence(legacy.transition_seq),
        link,
        local_head: None,
        snapshot: Some(wire::SnapshotView {
            facts,
            title: None,
            fetched_at: None,
            last_failure: legacy
                .failure
                .as_ref()
                .map(|failure| wire::FetchFailureView {
                    kind: failure.kind.as_str().to_owned(),
                    message: failure.message.clone(),
                }),
            consecutive_failures: 0,
        }),
        candidates: Vec::new(),
        auto_fix: None,
        head_attribution: wire::HeadAttribution::Unknown,
    }
}

#[test]
fn the_core_clients_glyph_and_ci_words_are_the_embedded_ones() {
    let failure = FetchFailure {
        kind: storage::pull_requests::FetchErrorKind::Network,
        message: "offline".into(),
    };
    let mut cases = Vec::new();
    for state in [
        PullRequestState::Open,
        PullRequestState::Closed,
        PullRequestState::Merged,
    ] {
        for checks in lists().into_iter().step_by(7) {
            for complete in [true, false] {
                cases.push(view(Some(facts(state, checks.clone(), complete))));
            }
        }
    }
    cases.push(view(None));
    let failed: Vec<_> = cases
        .iter()
        .cloned()
        .map(|mut case| {
            case.failure = Some(failure.clone());
            case
        })
        .collect();
    cases.extend(failed);
    let mut unlinked = view(None);
    unlinked.link = None;
    cases.push(unlinked);
    for case in cases {
        let core = as_protocol(&case);
        assert_eq!(core_view::glyph(&core), glyph(&case), "{case:?}");
        assert_eq!(core_view::ci(&core), ci_summary(&case), "{case:?}");
    }
}
