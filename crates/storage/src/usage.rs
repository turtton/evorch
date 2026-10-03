//! Per-request usage ledger records and their daily rollups.
//!
//! The ledger is written by the GUI storage bridge from provider attempt
//! terminal events. Thread and project ownership is attributed separately
//! because the GUI may bind a run to a thread after its requests complete.

/// Default number of days per-request rows are kept before being rolled up.
pub const USAGE_RETENTION_DAYS: u32 = 90;

/// Terminal outcome of one provider attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsageStatus {
    Ok,
    Failed,
}

impl UsageStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Self {
        if value == "failed" {
            Self::Failed
        } else {
            Self::Ok
        }
    }
}

/// One provider attempt, as written to the ledger.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRequestRecord {
    pub request_id: String,
    /// Wall-clock time of the terminal event in Unix nanoseconds.
    pub at_ns: i64,
    pub provider: String,
    pub profile: Option<String>,
    pub model: String,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub role: Option<String>,
    /// [`event_bus::RequestPurpose`] label, `None` when unknown.
    pub purpose: Option<String>,
    pub status: UsageStatus,
    pub failure: Option<String>,
    pub finish_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// `None` when the provider did not report a reasoning breakdown.
    pub reasoning_tokens: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub duration_ms: u64,
    /// Price at record time. `None` when the model had no known price.
    pub cost_usd: Option<f64>,
}

/// Thread and project that own a run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunAttribution {
    pub run_id: String,
    pub thread_id: String,
    pub project_id: Option<String>,
}

/// A ledger row joined with its run attribution.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRequestRow {
    pub record: UsageRequestRecord,
    /// Local calendar day of `record.at_ns`, `YYYY-MM-DD`, matching [`UsageDailyRow::day`].
    pub day: String,
    pub thread_id: Option<String>,
    pub project_id: Option<String>,
}

/// Totals for requests rolled up after the retention window. Unknown
/// dimensions are `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageDailyRow {
    /// Local calendar day, `YYYY-MM-DD`.
    pub day: String,
    pub provider: String,
    pub profile: Option<String>,
    pub model: String,
    pub project_id: Option<String>,
    pub role: Option<String>,
    pub purpose: Option<String>,
    pub request_count: u64,
    pub failed_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// Sum of the recorded costs. Tokens without one are in `unpriced_*`.
    pub cost_usd: f64,
    pub unpriced_input_tokens: u64,
    pub unpriced_output_tokens: u64,
    pub unpriced_cache_read_tokens: u64,
    pub unpriced_cache_write_tokens: u64,
    pub ttft_sum_ms: u64,
    pub ttft_count: u64,
    pub duration_sum_ms: u64,
}
