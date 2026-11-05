use super::*;
use storage::pull_requests::{CheckKind, LinkOrigin, PullRequestCheck, ReviewDecision};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub(crate) fn check(
    name: &str,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
) -> PullRequestCheck {
    PullRequestCheck {
        provider_id: name.to_owned(),
        kind: CheckKind::CheckRun,
        name: name.to_owned(),
        status,
        conclusion,
        head_sha: SHA_A.to_owned(),
    }
}

pub(crate) fn facts(
    state: PullRequestState,
    checks: Vec<PullRequestCheck>,
    complete: bool,
) -> PullRequestFacts {
    PullRequestFacts {
        state,
        base_branch: "main".into(),
        head_repository: Some("turtton/evorch".into()),
        head_branch: "feat/x".into(),
        head_sha: SHA_A.into(),
        checks,
        checks_complete: complete,
        review_decision: ReviewDecision::None,
    }
}

pub(crate) fn view(facts: Option<PullRequestFacts>) -> ThreadPullRequest {
    ThreadPullRequest {
        thread_id: "thread".into(),
        lifecycle_revision: 1,
        transition_seq: 1,
        link: Some(PullRequestLink {
            link_id: 1,
            thread_id: "thread".into(),
            key: PullRequestKey::github("turtton/evorch", 12).unwrap(),
            origin: LinkOrigin::Explicit,
            parent_link_id: None,
            created_at_ns: 0,
            link_revision: 1,
            end: None,
        }),
        local_head: Some(LocalHeadObservation {
            link_revision: 1,
            head_sha: SHA_A.into(),
            observed_at_ns: 0,
        }),
        facts,
        fetched_at_ns: Some(0),
        failure: None,
        candidates: Vec::new(),
        tracked_here: true,
    }
}

fn passing() -> PullRequestFacts {
    facts(
        PullRequestState::Open,
        vec![check(
            "a",
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
        )],
        true,
    )
}

fn failing() -> PullRequestFacts {
    facts(
        PullRequestState::Open,
        vec![
            check("a", CheckStatus::Completed, Some(CheckConclusion::Success)),
            check("b", CheckStatus::Completed, Some(CheckConclusion::Failure)),
        ],
        true,
    )
}

// Given: states that do not establish success / When: the glyph is chosen / Then: none is
// the passing glyph; only a complete, finished, all-passing list is
#[test]
fn only_a_complete_passing_list_is_passing() {
    let unknowns = [
        view(None),
        view(Some(facts(PullRequestState::Open, vec![], true))),
        view(Some(facts(
            PullRequestState::Open,
            vec![check(
                "a",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            )],
            false,
        ))),
        view(Some(facts(
            PullRequestState::Open,
            vec![check(
                "a",
                CheckStatus::Completed,
                Some(CheckConclusion::Cancelled),
            )],
            true,
        ))),
    ];
    for unknown in &unknowns {
        assert_eq!(glyph(unknown), Some(PrGlyph::Unknown), "{unknown:?}");
    }
    let mut failed_fetch = view(Some(passing()));
    failed_fetch.failure = Some(FetchFailure {
        kind: storage::pull_requests::FetchErrorKind::Network,
        message: "offline".into(),
    });
    assert_eq!(glyph(&failed_fetch), Some(PrGlyph::Unknown));
    assert_eq!(glyph(&view(Some(passing()))), Some(PrGlyph::Passing));
}

// Given: failing, pending, merged and closed pull requests / Then: each has its own glyph
#[test]
fn glyphs_follow_state_and_checks() {
    assert_eq!(glyph(&view(Some(failing()))), Some(PrGlyph::Failing));
    let pending = facts(
        PullRequestState::Open,
        vec![check("a", CheckStatus::InProgress, None)],
        true,
    );
    assert_eq!(glyph(&view(Some(pending))), Some(PrGlyph::Pending));
    let merged = facts(PullRequestState::Merged, vec![], true);
    assert_eq!(glyph(&view(Some(merged))), Some(PrGlyph::Merged));
    let closed = facts(PullRequestState::Closed, vec![], true);
    assert_eq!(glyph(&view(Some(closed))), Some(PrGlyph::Closed));
    let mut unlinked = view(Some(passing()));
    unlinked.link = None;
    assert_eq!(glyph(&unlinked), None);
    assert_eq!(badge(&unlinked), None);
}

#[test]
fn the_summary_counts_checks_and_never_claims_success_without_them() {
    assert_eq!(ci_summary(&view(Some(failing()))), "CI failing 1/2");
    assert_eq!(ci_summary(&view(Some(passing()))), "CI passing 1/1");
    assert_eq!(
        ci_summary(&view(Some(facts(PullRequestState::Open, vec![], true)))),
        "No checks"
    );
    assert_eq!(ci_summary(&view(None)), "Waiting for the first fetch");
}

// Given: a linked thread / When: the local HEAD is missing or elsewhere / Then: a warning
#[test]
fn a_local_checkout_that_disagrees_is_warned_about() {
    let mut shown = view(Some(passing()));
    assert!(mismatches(&shown).is_empty());
    shown.local_head.as_mut().unwrap().head_sha = SHA_B.into();
    assert_eq!(
        mismatches(&shown),
        [Mismatch::LocalHeadDiffers {
            local: SHA_B.into(),
            remote: SHA_A.into()
        }]
    );
    shown.local_head = None;
    assert_eq!(
        mismatches(&shown),
        [Mismatch::NoLocalCheckout {
            branch: "feat/x".into()
        }]
    );
    let mut merged = view(Some(facts(PullRequestState::Merged, vec![], true)));
    merged.local_head = None;
    assert!(
        mismatches(&merged).is_empty(),
        "a merged branch may move on"
    );
}

// Given: a bar that sees a pull request go from passing to failing / Then: it expands once,
// and a closed bar stays closed while the same failure is polled again
#[test]
fn a_new_failure_expands_the_bar_once() {
    let mut bar = BarState::default();
    bar.observe(&view(Some(passing())));
    assert!(!bar.expanded);
    bar.observe(&view(Some(failing())));
    assert!(bar.expanded, "the move into failure expands");
    bar.expanded = false;
    bar.observe(&view(Some(failing())));
    bar.observe(&view(Some(failing())));
    assert!(!bar.expanded, "the same failure never expands again");
    bar.observe(&view(Some(passing())));
    bar.observe(&view(Some(failing())));
    assert!(bar.expanded, "a later failure is a new transition");
}

// Given: a thread already failing when it is first seen / Then: that is a baseline
#[test]
fn a_failure_seen_first_is_a_baseline_not_a_transition() {
    let mut bar = BarState::default();
    bar.observe(&view(Some(failing())));
    assert!(!bar.expanded);
}

// Given: a view that is untracked, old and failed at once / Then: each status holds on its own
#[test]
fn tracking_staleness_and_a_failed_fetch_are_independent_statuses() {
    let mut shown = view(Some(passing()));
    assert_eq!(statuses(&shown, 1), []);
    let later = (STALE_AFTER_SECONDS + 1) * NANOS_PER_SECOND;
    assert_eq!(statuses(&shown, later), [Status::Stale]);
    shown.tracked_here = false;
    assert_eq!(statuses(&shown, 1), [Status::NotTracked]);
    shown.failure = Some(FetchFailure {
        kind: storage::pull_requests::FetchErrorKind::Network,
        message: "offline".into(),
    });
    assert_eq!(
        statuses(&shown, later),
        [
            Status::NotTracked,
            Status::Stale,
            Status::FetchFailed {
                kind: "network".into(),
                message: "offline".into()
            }
        ]
    );
}

// Given: a failing pull request whose bar the person closed / When: a fetch fails and the same
// failure returns / Then: the bar stays closed; only a pass in between makes a new transition
#[test]
fn a_failed_fetch_in_between_is_not_a_transition() {
    let mut bar = BarState::default();
    bar.observe(&view(Some(passing())));
    bar.observe(&view(Some(failing())));
    assert!(bar.expanded);
    bar.expanded = false;

    let mut unreachable = view(Some(failing()));
    unreachable.failure = Some(FetchFailure {
        kind: storage::pull_requests::FetchErrorKind::Network,
        message: "offline".into(),
    });
    bar.observe(&unreachable);
    bar.observe(&view(None));
    bar.observe(&view(Some(failing())));
    assert!(
        !bar.expanded,
        "recovering into the same failure must not expand"
    );

    bar.observe(&view(Some(passing())));
    bar.observe(&view(Some(failing())));
    assert!(bar.expanded, "a failure after a pass is a new transition");
}

#[test]
fn chips_follow_the_candidate_and_the_active_link() {
    let key = PullRequestKey::github("turtton/evorch", 12).unwrap();
    let other = PullRequestKey::github("turtton/evorch", 13).unwrap();
    let candidate = |id, key: &PullRequestKey, state| PullRequestCandidate {
        candidate_id: id,
        thread_id: "thread".into(),
        key: key.clone(),
        source_run_id: "run".into(),
        source_call_id: "call".into(),
        state,
        created_at_ns: 0,
        updated_at_ns: 0,
    };
    let mut shown = view(None);
    shown.link = None;
    shown.candidates = vec![
        candidate(1, &key, CandidateState::Presented),
        candidate(2, &other, CandidateState::Dismissed),
        candidate(3, &other, CandidateState::Confirmed),
    ];
    let chips = chips_by_call(&shown);
    let for_call = &chips[&("run".to_owned(), "call".to_owned())];
    assert_eq!(
        for_call.len(),
        1,
        "dismissed and unlinked confirmations show nothing"
    );
    assert_eq!(for_call[0].state, ChipState::Offered);
    assert_eq!(for_call[0].label, "turtton/evorch#12");

    let mut linked = view(Some(passing()));
    linked.candidates = shown.candidates.clone();
    linked.candidates[0].state = CandidateState::Confirmed;
    let chips = chips_by_call(&linked);
    assert_eq!(
        chips[&("run".to_owned(), "call".to_owned())][0].state,
        ChipState::Linked
    );
}

#[test]
fn a_disabled_model_shows_nothing() {
    let mut model = PullRequestsModel::default();
    model.set(view(Some(passing())));
    assert!(model.linked("thread").is_none());
    model.enabled = true;
    assert!(model.linked("thread").is_some());
    assert_eq!(model.linked_views().count(), 1);
}
