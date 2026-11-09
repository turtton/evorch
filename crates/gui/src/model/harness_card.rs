//! The card that shows a harness notice in the transcript (ADR 0043 Phase 2).
//!
//! A notice is a durable record the harness appended to the conversation; the transcript
//! holds only its identity (the notice ID, from a delivery event or verified input provenance)
//! and the card is filled in
//! from the stored notice. What the card shows is the `harness` tag, the pull request and
//! commit, the facts and the log excerpts, never the wrapper the agent reads and never a
//! caution about the data.

use runtime::lane_delivery::view_pr_notice_payload;
use storage::pull_request_auto_fix::{PrNotice, PrNoticeMode, PrNoticePhase};

/// What the card shows, read from one adopted notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessCard {
    pub notice_id: String,
    /// `owner/repo`, as the pull request is named everywhere.
    pub repository: String,
    pub number: u64,
    /// The full head commit the notice is about.
    pub head_sha: String,
    /// Whether a person asked for it (otherwise the automatic path sent it).
    pub manual: bool,
    /// The failed checks.
    pub checks: Vec<String>,
    /// When the failure was observed (RFC 3339), as stored.
    pub observed_at: Option<String>,
    pub logs: Vec<HarnessLog>,
    /// Whether anything was left out of the logs.
    pub logs_truncated: bool,
}

/// The excerpt of one run's failed-step log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessLog {
    pub checks: Vec<String>,
    pub excerpt: String,
}

impl HarnessCard {
    /// The card of a notice that is part of the conversation; `None` for a notice that is
    /// not (yet) adopted or has no body.
    #[must_use]
    pub fn from_notice(notice: &PrNotice) -> Option<Self> {
        if notice.phase != PrNoticePhase::Adopted {
            return None;
        }
        let view = view_pr_notice_payload(notice.payload.as_deref()?);
        Some(Self {
            notice_id: notice.notice_id.clone(),
            repository: notice.key.repository.clone(),
            number: notice.key.number,
            head_sha: notice.head_sha.clone(),
            manual: notice.mode == PrNoticeMode::Manual,
            checks: view.checks,
            observed_at: view.observed_at,
            logs: view
                .logs
                .into_iter()
                .map(|log| HarnessLog {
                    checks: log.checks,
                    excerpt: log.excerpt,
                })
                .collect(),
            logs_truncated: view.logs_truncated,
        })
    }
}
