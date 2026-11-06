//! Fetching the state of a GitHub pull request for the harness (ADR 0043 Phase 1, PR2).
//!
//! This module is the *adapter* layer only: how a pull request's facts are obtained
//! ([`PullRequestSource`], with [`source::GhSource`] running the `gh` CLI), how a pull
//! request is found in a local checkout ([`checkout`]), and a scriptable [`fake`] for the
//! services that come later. Nothing here schedules, stores, publishes or shows anything.
//!
//! What crosses the boundary is structured: the [`storage::pull_requests`] DTOs. A pull
//! request's body, comments, review texts, CI logs and any counts are never requested; the
//! title is, for display only.

pub mod candidates;
pub mod checkout;
pub mod diff;
pub mod fake;
pub mod locator;
pub mod log_source;
pub mod ownership;
pub mod review;
mod runtime;
pub mod service;
pub mod source;

use std::time::Duration;

use async_trait::async_trait;
use storage::pull_requests::{
    FetchErrorKind, FetchFailure, PULL_REQUEST_ERROR_MAX_CHARS, PullRequestDisplay,
    PullRequestFacts, PullRequestKey, sanitize_single_line,
};

pub use checkout::{CheckoutMiss, CheckoutResolution, resolve_checkout};
pub use fake::{FakeProcessRunner, FakePullRequestSource, FetchGate};
pub use locator::{LocatorError, parse_locator};
pub use log_source::{
    CiLogBundle, CiLogRun, CiLogSkip, FakePullRequestLogSource, GhLogSource, LogLimits, LogRequest,
    LogSkipKind, LogSkipReason, PullRequestLogSource, minimal_ci_log_data, render_ci_log_data,
};
pub use ownership::{OwnershipView, ThreadEnvironment};
pub use review::{
    Freshness, ReviewLookup, ReviewPullRequest, ReviewRefresh, ReviewSupply, StaleReason,
};
pub use service::{
    DerivedThreadKind, InheritanceScheduling, PullRequestError, PullRequestService, ServiceParts,
    SystemWallClock, WallClock,
};
pub use source::{GhSource, ProcessError, ProcessOutput, ProcessRunner, ProcessSpec};

/// The facts of one successful fetch, as the source reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPullRequest {
    /// The pull request that was asked for.
    pub requested: PullRequestKey,
    /// The pull request GitHub answered with. Differs from `requested` only in the
    /// repository (a renamed or transferred repository is followed by GitHub).
    pub canonical: PullRequestKey,
    /// The pull request's web URL on github.com.
    pub url: String,
    /// The facts in the form [`StorageHandle::commit_pull_request_fetch`] stores.
    ///
    /// [`StorageHandle::commit_pull_request_fetch`]: storage::StorageHandle::commit_pull_request_fetch
    pub facts: PullRequestFacts,
    /// What the same fetch gave for a display only (the cleaned title). Stored with the
    /// facts but never part of them.
    pub display: PullRequestDisplay,
    /// When the fetch ended, in nanoseconds since the Unix epoch.
    pub fetched_at_ns: i64,
}

/// Gets the facts of a pull request. Implementations are cancel-safe: dropping the future
/// ends the fetch and leaves no work behind (the `gh` child is killed).
#[async_trait]
pub trait PullRequestSource: Send + Sync {
    /// Fetches `key`, giving up after `deadline`.
    ///
    /// The service confirms that tracking is on and that the thread still has its lease
    /// immediately before this call and waits for nothing after it, so an implementation that
    /// starts a process starts it before its first `await`.
    ///
    /// # Errors
    /// A [`FetchError`]; see its variants. A partial or unrecognized answer is an error,
    /// never a success with less in it.
    async fn fetch(
        &self,
        key: &PullRequestKey,
        deadline: Duration,
    ) -> Result<FetchedPullRequest, FetchError>;
}

/// Why a fetch gave no facts. The text of every variant is single-line, bounded and
/// redacted, so it is safe to show and to store.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The `gh` program does not exist (not installed, or not on `PATH`).
    #[error("the gh CLI was not found")]
    GhNotFound,
    /// `gh` is not signed in to github.com, or its token is no longer valid.
    #[error("gh is not authenticated for github.com: {0}")]
    Unauthenticated(String),
    /// The token is valid but not allowed to read this pull request (scope, SAML, or a
    /// private repository).
    #[error("gh is not permitted to read the pull request: {0}")]
    PermissionDenied(String),
    /// The pull request or its repository does not exist (or is hidden from the token).
    #[error("the pull request was not found: {0}")]
    NotFound(String),
    /// GitHub's rate limit was hit.
    #[error("GitHub rate limit: {0}")]
    RateLimited(String),
    /// The network failed before GitHub answered.
    #[error("network error: {0}")]
    Network(String),
    /// The deadline passed.
    #[error("the fetch did not finish in {0:?}")]
    Timeout(Duration),
    /// `gh` answered, but not with something that can be trusted: malformed JSON, an
    /// unknown state or status, a missing field, an inconsistent check, too much output.
    #[error("unusable gh output: {0}")]
    InvalidResponse(String),
    /// The pull request is not on github.com (Enterprise and lookalike hosts are refused).
    #[error("only github.com pull requests are supported: {0}")]
    InvalidHost(String),
    /// The pull request key or locator is malformed.
    #[error("invalid pull request reference: {0}")]
    InvalidReference(String),
    /// `gh` failed in a way that fits no other variant.
    #[error("gh failed: {0}")]
    Other(String),
}

/// A third-party text made safe to keep: the whole text is redacted first (so a credential
/// that crosses the limit is masked before anything is cut), then made single-line and
/// bounded.
pub(crate) fn clean_text(text: &str, max_chars: usize) -> String {
    let redacted = secret_guard::SecretRedactor::from_env().redact(text).text;
    sanitize_single_line(&redacted, max_chars)
}

impl FetchError {
    pub(crate) fn unauthenticated(detail: &str) -> Self {
        Self::Unauthenticated(clean_text(detail, PULL_REQUEST_ERROR_MAX_CHARS))
    }

    pub(crate) fn permission_denied(detail: &str) -> Self {
        Self::PermissionDenied(clean_text(detail, PULL_REQUEST_ERROR_MAX_CHARS))
    }

    pub(crate) fn not_found(detail: &str) -> Self {
        Self::NotFound(clean_text(detail, PULL_REQUEST_ERROR_MAX_CHARS))
    }

    pub(crate) fn rate_limited(detail: &str) -> Self {
        Self::RateLimited(clean_text(detail, PULL_REQUEST_ERROR_MAX_CHARS))
    }

    pub(crate) fn network(detail: &str) -> Self {
        Self::Network(clean_text(detail, PULL_REQUEST_ERROR_MAX_CHARS))
    }

    pub(crate) fn invalid_response(detail: &str) -> Self {
        Self::InvalidResponse(clean_text(detail, PULL_REQUEST_ERROR_MAX_CHARS))
    }

    pub(crate) fn other(detail: &str) -> Self {
        Self::Other(clean_text(detail, PULL_REQUEST_ERROR_MAX_CHARS))
    }

    /// The class stored with a failed fetch.
    #[must_use]
    pub fn kind(&self) -> FetchErrorKind {
        match self {
            Self::Unauthenticated(_) | Self::PermissionDenied(_) => FetchErrorKind::Unauthenticated,
            Self::NotFound(_) | Self::InvalidHost(_) | Self::InvalidReference(_) => {
                FetchErrorKind::NotFound
            }
            Self::RateLimited(_) => FetchErrorKind::RateLimited,
            Self::Network(_) => FetchErrorKind::Network,
            Self::Timeout(_) => FetchErrorKind::Timeout,
            Self::InvalidResponse(_) => FetchErrorKind::InvalidResponse,
            Self::GhNotFound | Self::Other(_) => FetchErrorKind::Other,
        }
    }

    /// The failure as [`StorageHandle::fail_pull_request_fetch`] takes it. The message is
    /// the (already bounded and redacted) text of the error.
    ///
    /// [`StorageHandle::fail_pull_request_fetch`]: storage::StorageHandle::fail_pull_request_fetch
    #[must_use]
    pub fn failure(&self) -> FetchFailure {
        FetchFailure {
            kind: self.kind(),
            message: clean_text(&self.to_string(), PULL_REQUEST_ERROR_MAX_CHARS),
        }
    }

    /// Whether trying again later can succeed (the scheduler backs off on these and stops
    /// on the others until something changes).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::RateLimited(_) | Self::Network(_) | Self::Timeout(_) | Self::Other(_)
        )
    }
}
