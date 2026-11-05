//! What the escalation review (PR4) reads about a thread's pull request.
//!
//! Separate from the storage rows: the review gets one value that already says how fresh
//! the facts are, so it never has to compare times or decide what "stale" means.

use std::time::Duration;

use storage::pull_requests::{
    FetchFailure, LocalHeadObservation, PullRequestFacts, PullRequestLink,
};

/// Why facts are weak evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// No fetch has succeeded yet.
    NoFacts,
    /// The newest successful fetch is older than `review_max_age_seconds`.
    TooOld,
    /// The newest fetch failed, so the facts may have moved on.
    LastFetchFailed,
}

/// How much the facts can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Fetched recently and the newest fetch succeeded.
    Fresh,
    /// Usable only as weak evidence.
    Stale(StaleReason),
}

/// The tracked pull request of a thread, as the review sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPullRequest {
    /// The thread.
    pub thread_id: String,
    /// The active link.
    pub link: PullRequestLink,
    /// The facts of the newest successful fetch, if any.
    pub facts: Option<PullRequestFacts>,
    /// When that fetch ended.
    pub fetched_at_ns: Option<i64>,
    /// How long before now that was.
    pub age: Option<Duration>,
    /// How far the facts can be trusted.
    pub freshness: Freshness,
    /// The failure of the newest fetch, if it failed.
    pub last_failure: Option<FetchFailure>,
    /// The thread's local checkout HEAD, only while it belongs to this link.
    pub local_head: Option<LocalHeadObservation>,
}

/// What the review finds for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewLookup {
    /// Pull request tracking is off: the review input has no pull request item.
    Disabled,
    /// Tracking is on but `include_in_review` is off.
    NotIncluded,
    /// The run's thread is unknown, or has no active link (never linked, or unlinked).
    NotLinked,
    /// The thread tracks a pull request.
    Linked(Box<ReviewPullRequest>),
}

/// What a refresh for the review did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewRefresh {
    /// The stored facts were recent enough; nothing was fetched.
    NotNeeded,
    /// A fetch finished and the facts are new.
    Refreshed,
    /// The fetch finished with an error; the stored facts are marked stale.
    FetchFailed,
    /// The fetch did not finish in `review_refresh_timeout_seconds`. It keeps running for
    /// the tracker; the review goes on with the stored facts.
    TimedOut,
    /// This process does not own the thread, so it fetched nothing.
    NotOwner,
}

/// The result of [`PullRequestService::refresh_for_review`].
///
/// [`PullRequestService::refresh_for_review`]: super::PullRequestService::refresh_for_review
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSupply {
    /// The facts after the refresh (or the stored ones when it did not happen).
    pub lookup: ReviewLookup,
    /// What the refresh did.
    pub refresh: ReviewRefresh,
}
