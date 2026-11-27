//! Scoped usage and the live usage window (ADR 0045 E4, PR 1).
//!
//! Costs here are the ones the ledger recorded with each request (the price of
//! that time), never recomputed from today's prices. A value is a
//! [`UsageTotals`] plus whether the ledger may be missing requests of its scope.
//!
//! The core answers these queries as read operations (see [`ReadOutcome`]); it
//! keeps no usage state between two queries. A client that wants the current
//! value asks again, and `usage_changed` only says that asking again may give
//! another answer: it carries no value and no delta.
//!
//! [`ReadOutcome`]: super::ReadOutcome
use super::UsageTotals;
use crate::{
    ids::{OperationId, RunId, ThreadId},
    limits::MAX_NAME_BYTES,
    validation::{ValidationError, check_count, check_line},
};
use serde::{Deserialize, Serialize};

/// Most run ids one `usage_changed` event names; more is announced as a change
/// of every run.
pub const MAX_USAGE_CHANGED_RUNS: usize = 64;
/// Minute buckets of [`LiveUsage::minutes`]: the longest window the live view shows.
pub const LIVE_USAGE_MINUTES: usize = 180;
/// Most quota windows one [`LiveUsage`] sums.
pub const MAX_QUOTA_WINDOW_USAGES: usize = 32;

/// What a scope used, and whether the ledger may lack requests of that scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageValue {
    pub totals: UsageTotals,
    /// `true` when requests of this scope may have left the ledger (rolled up
    /// into days after the retention period, which drops their run and thread),
    /// so the totals are a lower bound.
    pub incomplete: bool,
}

/// The usage of one thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadUsage {
    pub thread: ThreadId,
    /// Every run of the thread: conversation roots, continued roots and the
    /// runs they started.
    pub total: UsageValue,
    /// The conversation roots only (the runs that have no parent run).
    pub conversation: UsageValue,
}

/// The usage of one run alone: the runs it started are not included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunUsage {
    pub run: RunId,
    pub total: UsageValue,
}

/// What the requests of one profile used inside one quota window. The core
/// fills these in once it knows the quota windows; until then the list is empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaWindowUsage {
    pub profile: String,
    pub window: String,
    pub totals: UsageTotals,
}

/// The usage a live view shows, as of one moment. Averages, burn rates and
/// forecasts are the client's to compute from these totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveUsage {
    /// The moment the minute buckets end at, in Unix milliseconds.
    pub as_of_ms: u64,
    /// Exactly [`LIVE_USAGE_MINUTES`] buckets, oldest first: bucket `i` covers
    /// `[as_of - (180 - i) minutes, as_of - (179 - i) minutes)`. A minute
    /// without a request is an empty bucket.
    pub minutes: Vec<UsageTotals>,
    /// The ledger's local day (`YYYY-MM-DD`) and its bounds in Unix milliseconds.
    pub local_day: String,
    pub day_start_ms: u64,
    pub day_end_ms: u64,
    /// Everything of the local day so far.
    pub today: UsageTotals,
    /// Everything since the first of the local day's month.
    pub month: UsageTotals,
    pub quota_windows: Vec<QuotaWindowUsage>,
}

impl LiveUsage {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.minutes.len() != LIVE_USAGE_MINUTES {
            return Err(ValidationError::Inconsistent {
                field: "minutes",
                reason: "must hold exactly one bucket per minute of the window",
            });
        }
        if self.day_start_ms >= self.day_end_ms {
            return Err(ValidationError::Inconsistent {
                field: "day_end_ms",
                reason: "must be after the start of the day",
            });
        }
        check_line("local_day", &self.local_day, 16, false)?;
        check_count(
            "quota_windows",
            self.quota_windows.len(),
            MAX_QUOTA_WINDOW_USAGES,
        )?;
        for window in &self.quota_windows {
            check_line("profile", &window.profile, MAX_NAME_BYTES, false)?;
            check_line("window", &window.window, MAX_NAME_BYTES, false)?;
        }
        Ok(())
    }
}

/// The usage of a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetThreadUsage {
    pub operation_id: OperationId,
    pub thread: ThreadId,
}

/// The usage of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRunUsage {
    pub operation_id: OperationId,
    pub run: RunId,
}

/// The live usage. Every question reads the ledger once; nothing is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetLiveUsage {
    pub operation_id: OperationId,
}

/// Which usage a `usage_changed` event says may have changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageChange {
    /// The runs that have new requests, at most [`MAX_USAGE_CHANGED_RUNS`];
    /// empty when `all_runs` is set.
    pub runs: Vec<RunId>,
    /// Any run may have changed: more runs than `runs` can name, or a request
    /// that names no run. The live usage changes with every request, whatever
    /// this says.
    pub all_runs: bool,
}

impl UsageChange {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("runs", self.runs.len(), MAX_USAGE_CHANGED_RUNS)?;
        // Either the runs are named, or all of them are meant; never both or neither.
        if self.all_runs != self.runs.is_empty() {
            return Err(ValidationError::Inconsistent {
                field: "runs",
                reason: "either names the changed runs or says that all runs changed",
            });
        }
        Ok(())
    }
}
