//! Events about the pull request a thread tracks (ADR 0043).
//!
//! They tell a display that *something* changed; the facts themselves are in the
//! database, which a reader opens again on receipt, so the payload is never the source of
//! truth. An event is published only after the change it names was committed, once per
//! change: the periodic fetches that find nothing new publish nothing.
//!
//! The events are scoped to a thread, carry no run, and never reach a model: they are
//! not part of any transcript, history or prompt.
//!
//! **Consumers.** Facts are stored once per pull request, and the process that owns a thread
//! publishes the change for *its* threads. When several processes (or threads of different
//! owners) track one pull request, only the one that first sees the difference publishes a
//! `Changed`. So a reader must not count on one event per thread: it invalidates what it shows by
//! the pull request key (`pull_request`) as well as by `thread_id`, and reads the database
//! again.

use serde::{Deserialize, Serialize};

/// Which pull request an event is about: the `(provider, host, repository, number)` key,
/// where `repository` is the base repository as `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PullRequestRef {
    /// `github` in Phase 1.
    pub provider: String,
    /// `github.com` in Phase 1.
    pub host: String,
    /// The base repository, `owner/name`, lowercase.
    pub repository: String,
    /// The pull request number.
    pub number: u64,
}

/// How a link came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestLinkOrigin {
    /// A person linked it, or confirmed a candidate.
    Explicit,
    /// A derived thread took it from its parent.
    Inherited,
}

/// Why a link ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestLinkEnd {
    /// Another link replaced it.
    Relinked,
    /// A person unlinked it.
    Unlinked,
}

/// Where a candidate stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestCandidateState {
    /// Detected in a tool output and shown; nobody decided yet.
    Presented,
    /// The person ignored it.
    Dismissed,
    /// The person linked it.
    Confirmed,
}

/// A part of the tracked facts that differs from the previous fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestChangedField {
    /// Open, closed or merged.
    State,
    /// The head commit.
    HeadSha,
    /// The checks (a check appeared, finished or changed its conclusion).
    Checks,
    /// Whether the list of checks is the whole list.
    ChecksComplete,
    /// The structured review decision.
    ReviewDecision,
    /// The base branch, or the head repository or branch.
    Refs,
    /// The commit the thread's local checkout is at.
    LocalHead,
}

/// Whether fetching the facts works. Published when the class of the problem changes, not
/// on every failed attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "kind", rename_all = "snake_case")]
pub enum PullRequestFetchStatus {
    /// Fetching fails; the value is the class of the failure (`unauthenticated`,
    /// `rate_limited`, `network`, ...).
    Failing(String),
    /// Fetching works again after a failure.
    Recovered,
}

/// A change to the pull request a thread tracks. Every variant names the thread, the link
/// revision and the transition sequence it happened at (for a candidate, the current ones),
/// so a reader can tell a late event from a current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum PullRequestEvent {
    /// The thread was linked to a pull request.
    Linked {
        thread_id: String,
        link_revision: u64,
        transition_seq: u64,
        pull_request: PullRequestRef,
        origin: PullRequestLinkOrigin,
    },
    /// The thread's link ended.
    Unlinked {
        thread_id: String,
        link_revision: u64,
        transition_seq: u64,
        pull_request: PullRequestRef,
        reason: PullRequestLinkEnd,
    },
    /// A pull request was detected in a tool output, or the person decided about one.
    CandidateChanged {
        thread_id: String,
        link_revision: u64,
        transition_seq: u64,
        pull_request: PullRequestRef,
        candidate_id: u64,
        state: PullRequestCandidateState,
    },
    /// A fetch found different facts. `notifications` lists the kinds this change gives
    /// reason to tell the person about (`merged`, `closed`, `checks_failed`, ...), each
    /// claimed exactly once for `(thread, link revision, transition sequence, kind)`.
    Changed {
        thread_id: String,
        link_revision: u64,
        transition_seq: u64,
        pull_request: PullRequestRef,
        fields: Vec<PullRequestChangedField>,
        notifications: Vec<String>,
    },
    /// Fetching started to fail, failed differently, or recovered.
    FetchStatusChanged {
        thread_id: String,
        link_revision: u64,
        transition_seq: u64,
        pull_request: PullRequestRef,
        status: PullRequestFetchStatus,
    },
}

impl PullRequestEvent {
    /// Every variant's tag, for readers that look rows up by the `subkind` column.
    pub const SUBKINDS: [&'static str; 5] = [
        "Linked",
        "Unlinked",
        "CandidateChanged",
        "Changed",
        "FetchStatusChanged",
    ];

    /// The thread the event is about.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        match self {
            Self::Linked { thread_id, .. }
            | Self::Unlinked { thread_id, .. }
            | Self::CandidateChanged { thread_id, .. }
            | Self::Changed { thread_id, .. }
            | Self::FetchStatusChanged { thread_id, .. } => thread_id,
        }
    }

    /// The pull request the event is about.
    #[must_use]
    pub fn pull_request(&self) -> &PullRequestRef {
        match self {
            Self::Linked { pull_request, .. }
            | Self::Unlinked { pull_request, .. }
            | Self::CandidateChanged { pull_request, .. }
            | Self::Changed { pull_request, .. }
            | Self::FetchStatusChanged { pull_request, .. } => pull_request,
        }
    }

    /// The link revision the event was published at.
    #[must_use]
    pub fn link_revision(&self) -> u64 {
        match self {
            Self::Linked { link_revision, .. }
            | Self::Unlinked { link_revision, .. }
            | Self::CandidateChanged { link_revision, .. }
            | Self::Changed { link_revision, .. }
            | Self::FetchStatusChanged { link_revision, .. } => *link_revision,
        }
    }

    /// The transition sequence the event was published at.
    #[must_use]
    pub fn transition_seq(&self) -> u64 {
        match self {
            Self::Linked { transition_seq, .. }
            | Self::Unlinked { transition_seq, .. }
            | Self::CandidateChanged { transition_seq, .. }
            | Self::Changed { transition_seq, .. }
            | Self::FetchStatusChanged { transition_seq, .. } => *transition_seq,
        }
    }

    /// The variant's tag, as written in the serialized payload.
    #[must_use]
    pub const fn subkind_name(&self) -> &'static str {
        match self {
            Self::Linked { .. } => "Linked",
            Self::Unlinked { .. } => "Unlinked",
            Self::CandidateChanged { .. } => "CandidateChanged",
            Self::Changed { .. } => "Changed",
            Self::FetchStatusChanged { .. } => "FetchStatusChanged",
        }
    }
}

impl From<PullRequestEvent> for crate::EventKind {
    fn from(event: PullRequestEvent) -> Self {
        Self::PullRequest(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKind;

    fn reference() -> PullRequestRef {
        PullRequestRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: "turtton/evorch".into(),
            number: 7,
        }
    }

    // Given: every variant / When: serialized inside an event kind / Then: it round-trips
    // and the thread can be found at `$.payload.payload.thread_id`, where retention looks
    #[test]
    fn every_variant_round_trips_and_names_its_thread_where_retention_reads_it() {
        let events = [
            PullRequestEvent::Linked {
                thread_id: "t".into(),
                link_revision: 1,
                transition_seq: 2,
                pull_request: reference(),
                origin: PullRequestLinkOrigin::Inherited,
            },
            PullRequestEvent::Unlinked {
                thread_id: "t".into(),
                link_revision: 2,
                transition_seq: 3,
                pull_request: reference(),
                reason: PullRequestLinkEnd::Unlinked,
            },
            PullRequestEvent::CandidateChanged {
                thread_id: "t".into(),
                link_revision: 2,
                transition_seq: 3,
                pull_request: reference(),
                candidate_id: 9,
                state: PullRequestCandidateState::Dismissed,
            },
            PullRequestEvent::Changed {
                thread_id: "t".into(),
                link_revision: 2,
                transition_seq: 4,
                pull_request: reference(),
                fields: vec![
                    PullRequestChangedField::State,
                    PullRequestChangedField::Checks,
                ],
                notifications: vec!["merged".into()],
            },
            PullRequestEvent::FetchStatusChanged {
                thread_id: "t".into(),
                link_revision: 2,
                transition_seq: 5,
                pull_request: reference(),
                status: PullRequestFetchStatus::Failing("rate_limited".into()),
            },
            PullRequestEvent::FetchStatusChanged {
                thread_id: "t".into(),
                link_revision: 2,
                transition_seq: 6,
                pull_request: reference(),
                status: PullRequestFetchStatus::Recovered,
            },
        ];
        for event in events {
            let kind = EventKind::from(event.clone());
            let json = serde_json::to_value(&kind).unwrap();
            assert_eq!(json["kind"], "PullRequest");
            assert_eq!(json["payload"]["kind"], event.subkind_name());
            assert_eq!(json["payload"]["payload"]["thread_id"], "t");
            assert_eq!(serde_json::from_value::<EventKind>(json).unwrap(), kind);
            assert_eq!(kind.subkind_name(), event.subkind_name());
            assert!(PullRequestEvent::SUBKINDS.contains(&event.subkind_name()));
            assert_eq!(kind.primary_run_id(), None);
        }
    }
}
