//! What the review is told about the thread's pull request (ADR 0043, "審査への供給").
//!
//! The reviewer gets structured *state evidence* the harness fetched: never a pull request's
//! body, comments, CI logs, the stderr of `gh` or any raw response. These types are separate
//! from the storage rows on purpose: what crosses to the model is decided here, field by field,
//! and the digest in the review manifest is computed from exactly this value.
//!
//! The item is absent, not null, when the thread has no active link or the feature is off. It
//! never changes [`EvidenceStamp`](super::EvidenceStamp): that stamp names the user's evidence
//! only. A change of the link itself is detected by the thread's lifecycle revision
//! ([`LinkAnchor`]), which also moves when a link comes and goes during one review.

use serde::Serialize;
use storage::pull_requests::{CheckConclusion, CheckStatus, PullRequestCheck};

use crate::pull_requests::{
    Freshness, PullRequestService, ReviewLookup, ReviewPullRequest, StaleReason,
};

/// Most checks listed to the reviewer. Failing and unfinished ones come first, so the cut falls
/// on passing checks; `checks_total` and `checks_complete` say that it happened.
pub(crate) const REVIEW_CHECKS_MAX: usize = 50;

/// The `harness_facts` item of the review payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HarnessFacts {
    pull_request: PullRequestFact,
    /// The thread's checkout HEAD, only when exactly one checkout matched the pull request.
    #[serde(skip_serializing_if = "Option::is_none")]
    local_worktree_head: Option<LocalWorktreeHead>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PullRequestFact {
    repository: String,
    number: u64,
    /// `fresh` or `stale`; stale facts are weak evidence.
    freshness: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale_reason: Option<&'static str>,
    /// The class of the newest failed fetch, if the newest fetch failed. Never its message.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_fetch_failure: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fetched_at_ns: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    age_seconds: Option<u64>,
    /// Absent while no fetch has ever succeeded.
    #[serde(flatten)]
    observed: Option<ObservedState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ObservedState {
    state: &'static str,
    base_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_repository: Option<String>,
    head_branch: String,
    head_sha: String,
    review_decision: &'static str,
    /// False when the list is cut or paged by the provider, or cut here. An incomplete or empty
    /// list is not evidence that the checks passed.
    checks_complete: bool,
    checks_total: usize,
    checks: Vec<CheckFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CheckFact {
    name: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    conclusion: Option<&'static str>,
    /// Whether the result belongs to the pull request's current head commit.
    for_current_head: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LocalWorktreeHead {
    head_sha: String,
    observed_at_ns: i64,
}

fn stale_reason(reason: StaleReason) -> &'static str {
    match reason {
        StaleReason::NoFacts => "no_facts",
        StaleReason::TooOld => "too_old",
        StaleReason::LastFetchFailed => "last_fetch_failed",
    }
}

fn is_passing(check: &PullRequestCheck) -> bool {
    check.status == CheckStatus::Completed && check.conclusion == Some(CheckConclusion::Success)
}

impl HarnessFacts {
    /// The item for a linked pull request. Pure: every time-dependent value was read when the
    /// lookup was made, so a retry of the same review sends the same item.
    fn of(review: &ReviewPullRequest) -> Self {
        let (freshness, reason) = match review.freshness {
            Freshness::Fresh => ("fresh", None),
            Freshness::Stale(reason) => ("stale", Some(stale_reason(reason))),
        };
        let observed = review.facts.as_ref().map(|facts| {
            let mut checks: Vec<&PullRequestCheck> = facts.checks.iter().collect();
            // Stable: the provider's order is kept within each group.
            checks.sort_by_key(|check| is_passing(check));
            let total = checks.len();
            checks.truncate(REVIEW_CHECKS_MAX);
            ObservedState {
                state: facts.state.as_str(),
                base_branch: facts.base_branch.clone(),
                head_repository: facts.head_repository.clone(),
                head_branch: facts.head_branch.clone(),
                head_sha: facts.head_sha.clone(),
                review_decision: facts.review_decision.as_str(),
                checks_complete: facts.checks_complete && total <= REVIEW_CHECKS_MAX,
                checks_total: total,
                checks: checks
                    .into_iter()
                    .map(|check| CheckFact {
                        name: check.name.clone(),
                        status: check.status.as_str(),
                        conclusion: check.conclusion.map(CheckConclusion::as_str),
                        for_current_head: check.head_sha == facts.head_sha,
                    })
                    .collect(),
            }
        });
        Self {
            pull_request: PullRequestFact {
                repository: review.link.key.repository.clone(),
                number: review.link.key.number,
                freshness,
                stale_reason: reason,
                last_fetch_failure: review
                    .last_failure
                    .as_ref()
                    .map(|failure| failure.kind.as_str()),
                fetched_at_ns: review.fetched_at_ns,
                age_seconds: review.age.map(|age| age.as_secs()),
                observed,
            },
            local_worktree_head: review.local_head.as_ref().map(|head| LocalWorktreeHead {
                head_sha: head.head_sha.clone(),
                observed_at_ns: head.observed_at_ns,
            }),
        }
    }
}

/// Which state of the thread's pull request lifecycle a review used: the thread's persistent
/// lifecycle revision, which moves on every Link and Unlink. It is held whether or not the
/// thread had an active link, so a link that came and went during the review is noticed. `None`
/// when the review saw no pull request item at all (feature off, or the run is in no thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkAnchor(Option<u64>);

/// What a review took from pull request tracking: the item and the lifecycle it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequestInput {
    pub(crate) facts: Option<HarnessFacts>,
    pub(crate) anchor: LinkAnchor,
}

impl PullRequestInput {
    /// A review that read nothing.
    pub(crate) const fn none() -> Self {
        Self {
            facts: None,
            anchor: LinkAnchor(None),
        }
    }

    fn of(revision: Option<u64>, lookup: &ReviewLookup) -> Self {
        Self {
            facts: match lookup {
                ReviewLookup::Linked(review) => Some(HarnessFacts::of(review)),
                _ => None,
            },
            anchor: LinkAnchor(revision),
        }
    }

    /// The stored facts as they are, marked stale when they are: no process, no network. For
    /// the reviews that never wait (web and upload).
    ///
    /// The revision is read before the facts: if the lifecycle moves in between, the anchor is
    /// the older one and the final check fails.
    pub(crate) async fn snapshot(service: Option<&PullRequestService>, run_id: &str) -> Self {
        let Some(service) = service else {
            return Self::none();
        };
        match Self::read(service, run_id).await {
            Ok(input) => input,
            Err(error) => {
                tracing::warn!(%error, run_id, "pull request facts were not read for a review");
                Self::none()
            }
        }
    }

    async fn read(
        service: &PullRequestService,
        run_id: &str,
    ) -> Result<Self, crate::pull_requests::PullRequestError> {
        let revision = service.review_lifecycle_revision(run_id).await?;
        let lookup = service.snapshot_for_review(run_id).await?;
        Ok(Self::of(revision, &lookup))
    }

    /// The facts after a bounded refresh. The caller holds no lock: the refresh may wait for a
    /// `gh` fetch up to the configured deadline. A fetch that failed, timed out or found
    /// nothing is not an error of the review: it goes on with the stored facts, marked stale,
    /// or with none.
    ///
    /// If the lifecycle moved while the refresh waited, the facts that came back belong to a
    /// link the review did not start with. They are dropped, and the anchor stays on the
    /// revision the review started with, so the final check treats the review as outdated.
    pub(crate) async fn refreshed(service: Option<&PullRequestService>, run_id: &str) -> Self {
        let Some(service) = service else {
            return Self::none();
        };
        let started = match service.review_lifecycle_revision(run_id).await {
            Ok(revision) => LinkAnchor(revision),
            Err(error) => {
                tracing::warn!(%error, run_id, "pull request facts were not read for a review");
                return Self::none();
            }
        };
        let supply = match service.refresh_for_review(run_id).await {
            Ok(supply) => supply,
            Err(error) => {
                tracing::warn!(%error, run_id, "pull request facts were not refreshed for a review");
                return Self::snapshot(Some(service), run_id).await;
            }
        };
        let after = match service.review_lifecycle_revision(run_id).await {
            Ok(revision) => LinkAnchor(revision),
            Err(error) => {
                tracing::warn!(%error, run_id, "the pull request link could not be checked");
                return Self {
                    facts: None,
                    anchor: started,
                };
            }
        };
        if after == started {
            Self::of(started.0, &supply.lookup)
        } else {
            Self {
                facts: None,
                anchor: started,
            }
        }
    }
}

impl LinkAnchor {
    /// Whether the run's thread is still at the lifecycle revision `self` names. A failed read
    /// counts as changed: an approval that cannot be checked is not kept.
    pub(crate) async fn still_holds(
        self,
        service: Option<&PullRequestService>,
        run_id: &str,
    ) -> bool {
        let Some(service) = service else {
            return true;
        };
        match service.review_lifecycle_revision(run_id).await {
            Ok(revision) => LinkAnchor(revision) == self,
            Err(error) => {
                tracing::warn!(%error, run_id, "the pull request link could not be checked after a review");
                false
            }
        }
    }
}

#[cfg(test)]
#[path = "facts_tests.rs"]
mod tests;
