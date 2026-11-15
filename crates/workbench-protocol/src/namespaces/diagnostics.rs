//! Diagnostics namespace (ADR 0045, PR 13): the usage ledger summary and the
//! context a run last saved. Everything here is read-only; no query writes,
//! generates or repairs anything.
//!
//! This module also owns the vocabulary the four read areas of PR 13
//! (`diagnostics`, `memory`, `improvement`, `evaluation`) share, because the
//! shared files of the protocol belong to the integration PRs:
//!
//! * [`ReadOutcome`] is how a *read operation* answers. Every query of these
//!   namespaces that may touch a file or a database names a client-chosen
//!   [`OperationId`]. The core answers at once with the operation's
//!   [`OperationView`] and, once it is `Ready`, with the data. Asking again with
//!   the same id and the same request joins the operation (and returns the
//!   data it kept); the same id with another request is a `Conflict`. The
//!   outcome of an operation is also announced as an `<area>_operation_changed` event
//!   of the namespace, so a client that sent the query need not poll.
//! * [`BoundedText`] is how a body is returned: never more than
//!   [`MAX_TEXT_BYTES`] bytes, with the full size and whether it was cut.
//! * [`OperationTarget`] names an operation for `get_<area>_operation`.
//!
//! The operation table is memory only: an operation does not survive a restart
//! of the core (the instance epoch changes) and a read is simply asked again.
use crate::{
    handshake::{Capability, WireLimits},
    ids::{OperationId, RunId},
    limits::MAX_NAME_BYTES,
    operation::OperationView,
    queries::PageRequest,
    validation::{ValidationError, check_bytes, check_count, check_line},
    views::Page,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[];
pub const QUERY_OPERATIONS: &[&str] = &[
    "get_usage_summary",
    "get_run_context",
    "get_diagnostics_operation",
];
pub const EVENT_TYPES: &[&str] = &["diagnostics_operation_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Longest body one reply carries (see [`BoundedText`]).
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Most days one [`UsageSummary`] lists (the newest).
pub const MAX_USAGE_DAYS: usize = 400;
/// Most models one [`UsageSummary`] lists (the most used).
pub const MAX_USAGE_MODELS: usize = 64;
/// Most tool names one [`RunContextOverview`] lists.
pub const MAX_CONTEXT_TOOLS: usize = 256;
/// Most budget segments of a [`RunContextOverview`].
pub const MAX_CONTEXT_SEGMENTS: usize = 16;
/// Longest [`ContextMessage::summary`], in bytes.
pub const MAX_CONTEXT_SUMMARY_BYTES: usize = 256;

no_commands!(DiagnosticsCommand);
no_results!(DiagnosticsResult);

// ---- shared read vocabulary -------------------------------------------------

/// How a read operation answers (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(
        serialize = "T: Serialize",
        deserialize = "T: serde::de::DeserializeOwned"
    )
)]
pub struct ReadOutcome<T> {
    pub operation: OperationView,
    /// Present only while the operation is `Ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> ReadOutcome<T> {
    /// Check the view, that only a ready operation carries data, and the data.
    ///
    /// # Errors
    /// An inconsistent view or data that fails `check`.
    pub fn validate_with(
        &self,
        check: impl FnOnce(&T) -> Result<(), ValidationError>,
    ) -> Result<(), ValidationError> {
        self.operation.validate()?;
        match &self.data {
            Some(_) if self.operation.state != crate::operation::OperationState::Ready => {
                Err(ValidationError::Inconsistent {
                    field: "data",
                    reason: "only a ready operation carries data",
                })
            }
            Some(data) => check(data),
            None => Ok(()),
        }
    }
}

/// An operation, by the id the client chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationTarget {
    pub operation_id: OperationId,
}

/// Text cut to a bound; the whole is never part of a snapshot or an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundedText {
    pub text: String,
    /// Size of the whole text in bytes.
    pub total_bytes: u64,
    pub truncated: bool,
}

impl BoundedText {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("text", &self.text, MAX_TEXT_BYTES, true)?;
        if self.total_bytes < self.text.len() as u64
            || (self.total_bytes > self.text.len() as u64 && !self.truncated)
        {
            return Err(ValidationError::Inconsistent {
                field: "total_bytes",
                reason: "must match the text and the truncated flag",
            });
        }
        Ok(())
    }
}

// ---- usage ------------------------------------------------------------------

/// The period of a usage summary, ending today (the ledger's local day).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageRange {
    Today,
    Last7Days,
    Last30Days,
    ThisMonth,
    Last90Days,
    AllTime,
}

/// What a set of requests used. Costs are the ones recorded with the requests,
/// in millionths of a US dollar; tokens of a request without a recorded price
/// are counted in `unpriced_tokens` and never priced here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageTotals {
    pub requests: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub recorded_cost_micro_usd: u64,
    pub unpriced_tokens: u64,
    /// Requests whose provider reported no usage; their token counts are zero.
    pub unreported_requests: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageDay {
    /// Local `YYYY-MM-DD`.
    pub day: String,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageModel {
    pub provider: String,
    pub model: String,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSummary {
    pub range: UsageRange,
    /// First and last day the summary covers (`YYYY-MM-DD`).
    pub from_day: String,
    pub to_day: String,
    pub totals: UsageTotals,
    /// Oldest first; at most [`MAX_USAGE_DAYS`] (the newest).
    pub days: Vec<UsageDay>,
    pub days_omitted: u32,
    /// Most requests first; at most [`MAX_USAGE_MODELS`].
    pub models: Vec<UsageModel>,
    pub models_omitted: u32,
}

impl UsageSummary {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("from_day", &self.from_day, 16, false)?;
        check_line("to_day", &self.to_day, 16, false)?;
        check_count("days", self.days.len(), MAX_USAGE_DAYS)?;
        check_count("models", self.models.len(), MAX_USAGE_MODELS)?;
        for day in &self.days {
            check_line("day", &day.day, 16, false)?;
        }
        for model in &self.models {
            check_line("provider", &model.provider, MAX_NAME_BYTES, true)?;
            check_line("model", &model.model, MAX_NAME_BYTES, true)?;
        }
        Ok(())
    }
}

// ---- run context ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMessageRole {
    System,
    User,
    Assistant,
    ToolResult,
}

/// One saved message, reduced to what a person scans: its role, the first line
/// and an estimate of its tokens. The text itself is not carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextMessage {
    pub index: u32,
    pub role: ContextMessageRole,
    pub summary: String,
    pub tokens: u64,
    /// `false` for a message that only holds state replayed to the provider; it
    /// is counted in the budget and has nothing to show.
    pub listed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSegment {
    pub label: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunContextOverview {
    pub run: RunId,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// `checkpoint` for a live run, otherwise the terminal phase.
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_model: Option<String>,
    pub updated_at_ms: u64,
    pub tools: Vec<String>,
    pub tools_omitted: u32,
    pub message_count: u32,
    pub compaction_checkpoints: u32,
    /// The saved history's tokens by message role, largest first.
    pub segments: Vec<ContextSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunContext {
    pub overview: RunContextOverview,
    pub messages: Page<ContextMessage>,
}

impl RunContext {
    pub fn validate(&self) -> Result<(), ValidationError> {
        let overview = &self.overview;
        check_line("role", &overview.role, MAX_NAME_BYTES, true)?;
        check_line("phase", &overview.phase, MAX_NAME_BYTES, true)?;
        check_count("tools", overview.tools.len(), MAX_CONTEXT_TOOLS)?;
        check_count("segments", overview.segments.len(), MAX_CONTEXT_SEGMENTS)?;
        for tool in &overview.tools {
            check_line("tool", tool, MAX_NAME_BYTES, false)?;
        }
        self.messages.validate_with(|message| {
            check_bytes("summary", &message.summary, MAX_CONTEXT_SUMMARY_BYTES, true)
        })
    }
}

// ---- wire -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetUsageSummary {
    pub operation_id: OperationId,
    pub range: UsageRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRunContext {
    pub operation_id: OperationId,
    pub run: RunId,
    /// Pages of the saved messages. Not part of the request's identity: the
    /// next page of the same operation names the same id with a cursor.
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DiagnosticsQuery {
    GetUsageSummary(GetUsageSummary),
    GetRunContext(GetRunContext),
    GetDiagnosticsOperation(OperationTarget),
}

impl DiagnosticsQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetUsageSummary(_) | Self::GetDiagnosticsOperation(_) => Ok(()),
            Self::GetRunContext(query) => query.page.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DiagnosticsReply {
    Usage(Box<ReadOutcome<UsageSummary>>),
    Context(Box<ReadOutcome<RunContext>>),
    Operation(OperationView),
}

impl DiagnosticsReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Usage(outcome) => outcome.validate_with(UsageSummary::validate),
            Self::Context(outcome) => outcome.validate_with(RunContext::validate),
            Self::Operation(view) => view.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DiagnosticsEvent {
    /// A read operation reached a final state; ask again for its data.
    DiagnosticsOperationChanged { view: OperationView },
}

impl DiagnosticsEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::DiagnosticsOperationChanged { view } => view.validate(),
        }
    }
}
