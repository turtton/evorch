//! A lane's place in the sidebar (ADR 0044 §10, §15).
//!
//! The lane ledger is the source of whether a thread is a lane, who manages it and under
//! which policy; the sidebar only keeps a projection of that so it can draw and save
//! without the ledger. The projection separates two things:
//!
//! - the **origin** (`origin_thread_id`): the thread that opened the lane. It never
//!   changes and is not a family relation;
//! - the **management** (`lead_thread_id`, `policy`, `management_revision`): who leads the
//!   lane now. This alone decides where the lane hangs in the tree and which family it
//!   belongs to, through [`crate::ThreadRecord::tree_parent`].
//!
//! `management_revision` is the ledger's counter. Every merge or repair keeps the higher
//! one, so a stale sidebar file can never bring a ended management relation back.

use serde::{Deserialize, Serialize};

use crate::ThreadId;

/// The report policy of a lane, as the ledger records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanePolicy {
    /// The lead manages the lane and wants milestone reports.
    Attached,
    /// The lead manages the lane and wants a final report only.
    Summary,
    /// No lead: an independent top-level thread.
    Detached,
}

impl LanePolicy {
    /// Whether a lead manages a lane under this policy.
    #[must_use]
    pub const fn is_managed(self) -> bool {
        matches!(self, Self::Attached | Self::Summary)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Summary => "summary",
            Self::Detached => "detached",
        }
    }
}

/// What the sidebar keeps of a lane's ledger record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneLink {
    pub lane_id: String,
    /// The thread that opened the lane. Immutable; never a family relation.
    pub origin_thread_id: ThreadId,
    /// The lead that manages the lane now; `None` once detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_thread_id: Option<ThreadId>,
    pub policy: LanePolicy,
    /// The ledger's management revision this projection was made at.
    pub management_revision: u64,
}

impl LaneLink {
    /// The lead the lane hangs under: only a managed lane has one.
    #[must_use]
    pub fn managing_lead(&self) -> Option<&ThreadId> {
        self.policy
            .is_managed()
            .then_some(self.lead_thread_id.as_ref())
            .flatten()
    }

    /// Whether `self` may replace `current`: the identity is the same and the ledger's
    /// revision is not older. At an equal revision the ledger is authoritative.
    #[must_use]
    pub fn supersedes(&self, current: &Self) -> bool {
        self.lane_id == current.lane_id && self.management_revision >= current.management_revision
    }
}

/// The projection to keep when two copies of one thread disagree: the higher management
/// revision, `first` on a tie. A lane is never un-made by a copy that has none.
#[must_use]
pub(crate) fn newest<'a>(
    candidates: impl IntoIterator<Item = Option<&'a LaneLink>>,
) -> Option<&'a LaneLink> {
    candidates
        .into_iter()
        .flatten()
        .fold(None, |best: Option<&LaneLink>, candidate| match best {
            Some(best) if best.management_revision >= candidate.management_revision => Some(best),
            _ => Some(candidate),
        })
}
