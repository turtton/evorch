//! Host service of the `diagnostics` area (PR 13): what the core has used and
//! what a run holds. Read only; the namespace names no command.
//!
//! * *Usage*: the ledger's requests and daily roll-ups of a range ending today
//!   (the ledger's local day), reduced to totals, per-day totals and per-model
//!   totals. Costs are the recorded ones in micro-USD; tokens of a request with
//!   no recorded price are counted as unpriced, never priced here.
//! * *Run context*: the last persisted checkpoint of a run (secret-redacted by
//!   the runtime), reduced to an overview, the token budget by role and one
//!   summary line per message. No message body is carried.
//!
//! * *Scoped and live usage* (ADR 0045 E4): the usage of one thread or one run,
//!   read from the ledger rows of its runs, and the live window (three hours by
//!   minute, today, the month). The core keeps nothing of them between two
//!   queries. A commit to the ledger is announced as a coalesced `usage_changed`
//!   event, which only tells a client to ask again.
//!
//! All of them are operations of [`read_ops`]; the database is opened read-only on a
//! blocking worker and the runtime's own connection is never locked.
use super::{AreaHandles, AreaTasks, ProjectionSource};
use crate::bootstrap::BootstrapError;
use crate::queries::{Pager, error, storage_error};
use providers::{ContentBlock, Message, Role as MessageRole};
use read_ops::{ReadOps, digest_of, first_line, page_of};
use runtime::AgentRuntime;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::SystemTime};
use storage::{
    Database, StorageHandle, UsageCommitObserver,
    usage::{UsageDailyRow, UsageRequestRow, UsageStatus},
};
use usage_notice::{USAGE_NOTICE_INTERVAL, UsageNotices};
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    events::UiEvent,
    ids::OperationId,
    namespaces::diagnostics::{
        ContextMessage, ContextMessageRole, ContextSegment, DiagnosticsEvent, GetLiveUsage,
        GetRunContext, GetRunUsage, GetThreadUsage, GetUsageSummary, LiveUsage,
        MAX_CONTEXT_SEGMENTS, MAX_CONTEXT_SUMMARY_BYTES, MAX_CONTEXT_TOOLS, MAX_USAGE_DAYS,
        MAX_USAGE_MODELS, ReadOutcome, RunContext, RunContextOverview, RunUsage, ThreadUsage,
        UsageDay, UsageModel, UsageRange, UsageSummary, UsageTotals,
    },
    operation::OperationView,
};

mod live;
pub mod quota;
pub mod read_ops;
mod usage;
mod usage_notice;

/// What a finished read holds, before the messages are cut into pages.
#[derive(Clone)]
pub enum DiagnosticsRead {
    Usage(Box<UsageSummary>),
    Context {
        overview: Box<RunContextOverview>,
        messages: Vec<ContextMessage>,
    },
    ThreadUsage(Box<ThreadUsage>),
    RunUsage(Box<RunUsage>),
    LiveUsage(Box<LiveUsage>),
}

/// The `diagnostics` service, reached from `ingress/namespaces/diagnostics.rs` as
/// `self.areas.diagnostics`. Without handles every read is refused.
#[derive(Default)]
pub struct DiagnosticsArea {
    reads: ReadOps<DiagnosticsRead>,
    pager: Pager,
    quotas: quota::Quotas,
    wired: Option<Wired>,
    /// Keeps the ledger's commit notices flowing to the task that announces them.
    _usage_commits: Option<Arc<UsageCommitObserver>>,
}

#[derive(Clone)]
struct Wired {
    runtime: AgentRuntime,
    database: PathBuf,
    /// How long the ledger keeps a request before it is rolled up into days.
    retention_days: u32,
}

impl DiagnosticsArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("diagnostics")
            .map_err(|e| BootstrapError::from(e.message))?;
        Ok(Self::from_parts(
            handles.tasks.clone(),
            source,
            handles.database.clone(),
            handles.runtime.clone(),
            &handles.storage,
            handles.config.metrics.retention_days,
        ))
    }

    /// The service over its parts: the host's tasks, the namespace's event
    /// source, the event database file, the runtime that holds run contexts, the
    /// storage writer whose usage commits it announces and the ledger's
    /// retention in days.
    pub fn from_parts(
        tasks: AreaTasks,
        source: ProjectionSource,
        database: PathBuf,
        runtime: AgentRuntime,
        storage: &StorageHandle,
        retention_days: u32,
    ) -> Self {
        let notices = Arc::new(UsageNotices::default());
        let observer: Arc<UsageCommitObserver> = {
            let notices = Arc::clone(&notices);
            Arc::new(move |runs: &[Option<String>]| notices.record(runs))
        };
        storage.observe_usage_commits(&observer);
        let announce = source.clone();
        let started = tasks.spawn("usage-notices", move |mut stop| async move {
            notices
                .run(
                    |change| {
                        let _ = announce.publish(UiEvent::Diagnostics(
                            DiagnosticsEvent::UsageChanged { change },
                        ));
                    },
                    USAGE_NOTICE_INTERVAL,
                    async move { stop.stopped().await },
                )
                .await;
            Ok(())
        });
        if let Err(error) = started {
            tracing::warn!(%error, "usage change notices are not announced");
        }
        Self {
            reads: ReadOps::new(tasks, move |view| {
                let _ = source.publish(UiEvent::Diagnostics(
                    DiagnosticsEvent::DiagnosticsOperationChanged { view },
                ));
            }),
            pager: Pager::default(),
            quotas: quota::Quotas::default(),
            wired: Some(Wired {
                runtime,
                database,
                retention_days,
            }),
            _usage_commits: Some(observer),
        }
    }

    pub(super) fn bind_quotas(
        &mut self,
        handles: &AreaHandles,
        settings: &super::settings::SettingsArea,
    ) -> Result<(), BootstrapError> {
        if let Some((changed, load)) = settings.quota_updates() {
            let source = handles
                .source("diagnostics")
                .map_err(|e| BootstrapError::from(e.message))?;
            self.quotas = quota::Quotas::new(quota::Parts {
                tasks: handles.tasks.clone(),
                announce: Arc::new(move || {
                    let _ = source.publish(UiEvent::Diagnostics(
                        DiagnosticsEvent::ProviderQuotasChanged {},
                    ));
                }),
                initial: quota::profiles(&handles.config),
                changed,
                load: Arc::new(move || load().map(|config| quota::profiles(&config))),
                fetch: quota::production(handles.credentials.clone()),
                gate: handles.restart_gate.clone(),
            })
            .map_err(|error| BootstrapError::from(error.to_string()))?;
        }
        Ok(())
    }

    /// Begin polling once, returning only the current memory snapshot.
    pub fn provider_quotas(
        &self,
    ) -> Result<workbench_protocol::namespaces::diagnostics::ProviderQuotas, WorkbenchError> {
        self.wired()?;
        Ok(self.quotas.get())
    }

    fn wired(&self) -> Result<&Wired, WorkbenchError> {
        self.wired.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "diagnostics is not available in this host",
            )
        })
    }

    /// Start or join the usage summary of a range.
    ///
    /// # Errors
    /// See [`ReadOps::submit`].
    pub fn usage(
        &self,
        request: &GetUsageSummary,
    ) -> Result<ReadOutcome<UsageSummary>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let range = request.range;
        let snapshot = self.reads.submit(
            &request.operation_id,
            digest_of(&("usage", range)),
            move || {
                let database = Database::open_read_only(&wired.database).map_err(storage_error)?;
                usage_summary(&database, range).map(|s| DiagnosticsRead::Usage(Box::new(s)))
            },
        )?;
        Ok(ReadOutcome {
            operation: snapshot.view,
            data: match snapshot.data {
                Some(DiagnosticsRead::Usage(summary)) => Some(*summary),
                _ => None,
            },
        })
    }

    /// Start or join the usage of a thread.
    ///
    /// # Errors
    /// See [`ReadOps::submit`]. The operation fails with `NotFound` for a thread
    /// the database does not know.
    pub fn thread_usage(
        &self,
        request: &GetThreadUsage,
    ) -> Result<ReadOutcome<ThreadUsage>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let thread = request.thread.clone();
        let snapshot = self.reads.submit(
            &request.operation_id,
            digest_of(&("thread_usage", &request.thread)),
            move || {
                let database = Database::open_read_only(&wired.database).map_err(storage_error)?;
                usage::thread_usage(&database, &thread, now_ns(), wired.retention_days)
                    .map(|usage| DiagnosticsRead::ThreadUsage(Box::new(usage)))
            },
        )?;
        Ok(ReadOutcome {
            operation: snapshot.view,
            data: match snapshot.data {
                Some(DiagnosticsRead::ThreadUsage(usage)) => Some(*usage),
                _ => None,
            },
        })
    }

    /// Start or join the usage of a run, without the runs it started.
    ///
    /// # Errors
    /// See [`ReadOps::submit`]. The operation fails with `NotFound` for a run
    /// with neither a request nor a thread.
    pub fn run_usage(
        &self,
        request: &GetRunUsage,
    ) -> Result<ReadOutcome<RunUsage>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let run = request.run.clone();
        let snapshot = self.reads.submit(
            &request.operation_id,
            digest_of(&("run_usage", &request.run)),
            move || {
                let database = Database::open_read_only(&wired.database).map_err(storage_error)?;
                usage::run_usage(&database, &run, now_ns(), wired.retention_days)
                    .map(|usage| DiagnosticsRead::RunUsage(Box::new(usage)))
            },
        )?;
        Ok(ReadOutcome {
            operation: snapshot.view,
            data: match snapshot.data {
                Some(DiagnosticsRead::RunUsage(usage)) => Some(*usage),
                _ => None,
            },
        })
    }

    /// Start or join one read of the live usage.
    ///
    /// # Errors
    /// See [`ReadOps::submit`].
    pub fn live_usage(
        &self,
        request: &GetLiveUsage,
    ) -> Result<ReadOutcome<LiveUsage>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let quotas = self.quotas.snapshot();
        let snapshot =
            self.reads
                .submit(&request.operation_id, digest_of(&"live_usage"), move || {
                    let database =
                        Database::open_read_only(&wired.database).map_err(storage_error)?;
                    live::live_usage_with_quotas(&database, now_ns(), &quotas, wired.retention_days)
                        .map(|usage| DiagnosticsRead::LiveUsage(Box::new(usage)))
                })?;
        Ok(ReadOutcome {
            operation: snapshot.view,
            data: match snapshot.data {
                Some(DiagnosticsRead::LiveUsage(usage)) => Some(*usage),
                _ => None,
            },
        })
    }

    /// Start or join the read of a run's saved context.
    ///
    /// # Errors
    /// See [`ReadOps::submit`]; `Conflict` also for a cursor that is not this
    /// operation's.
    pub fn context(
        &self,
        request: &GetRunContext,
    ) -> Result<ReadOutcome<RunContext>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let run = request
            .run
            .as_str()
            .parse::<runtime::RunId>()
            .map_err(|_| error(ErrorCode::InvalidRequest, "not a run id"))?;
        let wire_run = request.run.clone();
        let snapshot = self.reads.submit(
            &request.operation_id,
            digest_of(&("context", &request.run)),
            move || {
                let view = wired
                    .runtime
                    .run_context_view(run)
                    .map_err(|_| error(ErrorCode::StorageUnavailable, "the run cannot be read"))?
                    .ok_or_else(|| error(ErrorCode::NotFound, "this run has no saved context"))?;
                Ok(context_read(wire_run, &view))
            },
        )?;
        let data = match snapshot.data {
            Some(DiagnosticsRead::Context { overview, messages }) => Some(RunContext {
                overview: *overview,
                messages: page_of(&self.pager, &request.operation_id, &messages, &request.page)?,
            }),
            _ => None,
        };
        Ok(ReadOutcome {
            operation: snapshot.view,
            data,
        })
    }

    /// The view of an operation of this area.
    ///
    /// # Errors
    /// `NotFound` for an operation the core does not know.
    pub fn operation(&self, id: &OperationId) -> Result<OperationView, WorkbenchError> {
        self.reads.view(id)
    }
}

// ---- usage ------------------------------------------------------------------

/// The wall clock in Unix nanoseconds; the ledger's own clock for its rows.
fn now_ns() -> i64 {
    storage::system_time_to_ns(SystemTime::now()).unwrap_or_default()
}

/// A day as a number counted from 1970-01-01, for the calendar arithmetic of a
/// range (the ledger already labels every request with its local day).
fn day_number(label: &str) -> Option<i64> {
    let mut parts = label.splitn(3, '-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_index + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

fn day_label(days: i64) -> String {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// First day of `range` for `today`, `None` for no lower bound.
fn first_day(range: UsageRange, today: &str) -> Option<String> {
    let today_number = day_number(today)?;
    let back = |days: i64| Some(day_label(today_number - days));
    match range {
        UsageRange::Today => back(0),
        UsageRange::Last7Days => back(6),
        UsageRange::Last30Days => back(29),
        UsageRange::Last90Days => back(89),
        UsageRange::ThisMonth => Some(format!("{}-01", today.get(..7)?)),
        UsageRange::AllTime => None,
    }
}

fn micro_usd(usd: f64) -> u64 {
    if usd.is_finite() && usd > 0.0 {
        (usd * 1_000_000.0).round() as u64
    } else {
        0
    }
}

fn add_request(totals: &mut UsageTotals, row: &UsageRequestRow) {
    let record = &row.record;
    totals.requests += 1;
    totals.failed += u64::from(record.status == UsageStatus::Failed);
    totals.input_tokens += record.input_tokens;
    totals.output_tokens += record.output_tokens;
    totals.cache_read_tokens += record.cache_read_tokens;
    totals.cache_write_tokens += record.cache_write_tokens;
    totals.reasoning_tokens += record.reasoning_tokens.unwrap_or(0);
    match record.cost_usd {
        Some(cost) => totals.recorded_cost_micro_usd += micro_usd(cost),
        None if record.usage_reported => {
            totals.unpriced_tokens += record.input_tokens
                + record.output_tokens
                + record.cache_read_tokens
                + record.cache_write_tokens;
        }
        None => {}
    }
    totals.unreported_requests += u64::from(!record.usage_reported);
}

fn add_daily(totals: &mut UsageTotals, row: &UsageDailyRow) {
    totals.requests += row.request_count;
    totals.failed += row.failed_count;
    totals.input_tokens += row.input_tokens;
    totals.output_tokens += row.output_tokens;
    totals.cache_read_tokens += row.cache_read_tokens;
    totals.cache_write_tokens += row.cache_write_tokens;
    totals.reasoning_tokens += row.reasoning_tokens;
    totals.recorded_cost_micro_usd += micro_usd(row.cost_usd);
    totals.unpriced_tokens += row.unpriced_input_tokens
        + row.unpriced_output_tokens
        + row.unpriced_cache_read_tokens
        + row.unpriced_cache_write_tokens;
    totals.unreported_requests += row.unreported_count;
}

fn merge(into: &mut UsageTotals, from: &UsageTotals) {
    into.requests += from.requests;
    into.failed += from.failed;
    into.input_tokens += from.input_tokens;
    into.output_tokens += from.output_tokens;
    into.cache_read_tokens += from.cache_read_tokens;
    into.cache_write_tokens += from.cache_write_tokens;
    into.reasoning_tokens += from.reasoning_tokens;
    into.recorded_cost_micro_usd += from.recorded_cost_micro_usd;
    into.unpriced_tokens += from.unpriced_tokens;
    into.unreported_requests += from.unreported_requests;
}

/// How often the ledger is read again when a roll-up commits between its two parts.
const CONSISTENT_ATTEMPTS: usize = 3;

/// The ledger rows and the daily totals of one moment. A roll-up moves rows from
/// the first to the second in one transaction, so reading them separately could
/// count a moved row twice. The storage API has no read that spans both, and a daily
/// row only ever changes through a roll-up: the daily totals are read before and
/// after the rows, and the rows are used only if both reads agree, because then no
/// roll-up committed in between. Otherwise the whole read is repeated.
fn consistent_rows<D: PartialEq, R>(
    mut daily: impl FnMut() -> Result<D, storage::StorageError>,
    mut requests: impl FnMut() -> Result<R, storage::StorageError>,
) -> Result<(R, D), WorkbenchError> {
    for _ in 0..CONSISTENT_ATTEMPTS {
        let before = daily().map_err(storage_error)?;
        let rows = requests().map_err(storage_error)?;
        if daily().map_err(storage_error)? == before {
            return Ok((rows, before));
        }
    }
    Err(error(
        ErrorCode::StorageUnavailable,
        "the usage ledger kept changing while it was read",
    ))
}

fn usage_summary(database: &Database, range: UsageRange) -> Result<UsageSummary, WorkbenchError> {
    let clock = database.usage_local_clock().map_err(storage_error)?;
    let from = first_day(range, &clock.today).unwrap_or_else(|| "0000-01-01".to_owned());
    let (requests, daily) = consistent_rows(
        || database.usage_daily_between(&from, &clock.today),
        || database.usage_requests_in_days(&from, &clock.today),
    )?;
    Ok(reduce(range, &from, &clock.today, &requests, &daily))
}

/// The pure reduction of ledger rows; the days and models are cut to their bounds.
fn reduce(
    range: UsageRange,
    from: &str,
    today: &str,
    requests: &[UsageRequestRow],
    daily: &[UsageDailyRow],
) -> UsageSummary {
    let mut totals = UsageTotals::default();
    let mut days: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut models: BTreeMap<(String, String), UsageTotals> = BTreeMap::new();
    for row in requests {
        let mut one = UsageTotals::default();
        add_request(&mut one, row);
        merge(&mut totals, &one);
        merge(days.entry(row.day.clone()).or_default(), &one);
        merge(
            models
                .entry((row.record.provider.clone(), row.record.model.clone()))
                .or_default(),
            &one,
        );
    }
    for row in daily {
        let mut one = UsageTotals::default();
        add_daily(&mut one, row);
        merge(&mut totals, &one);
        merge(days.entry(row.day.clone()).or_default(), &one);
        merge(
            models
                .entry((row.provider.clone(), row.model.clone()))
                .or_default(),
            &one,
        );
    }
    let first_with_data = days.keys().next().cloned();
    let days_total = days.len();
    let mut days: Vec<UsageDay> = days
        .into_iter()
        .map(|(day, totals)| UsageDay {
            day: first_line(&day, 16),
            totals,
        })
        .collect();
    let days_omitted = days_total.saturating_sub(MAX_USAGE_DAYS);
    days.drain(..days_omitted);
    let models_total = models.len();
    let mut models: Vec<UsageModel> = models
        .into_iter()
        .map(|((provider, model), totals)| UsageModel {
            provider: first_line(&provider, 256),
            model: first_line(&model, 256),
            totals,
        })
        .collect();
    models.sort_by(|a, b| {
        b.totals
            .requests
            .cmp(&a.totals.requests)
            .then_with(|| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)))
    });
    models.truncate(MAX_USAGE_MODELS);
    UsageSummary {
        range,
        from_day: if range == UsageRange::AllTime {
            first_with_data.unwrap_or_else(|| today.to_owned())
        } else {
            from.to_owned()
        },
        to_day: today.to_owned(),
        totals,
        days,
        days_omitted: u32::try_from(days_omitted).unwrap_or(u32::MAX),
        models,
        models_omitted: u32::try_from(models_total.saturating_sub(MAX_USAGE_MODELS))
            .unwrap_or(u32::MAX),
    }
}

// ---- run context --------------------------------------------------------------

fn role_of(message: &Message) -> ContextMessageRole {
    let tool_result = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
    match message.role {
        MessageRole::System => ContextMessageRole::System,
        MessageRole::User if tool_result => ContextMessageRole::ToolResult,
        MessageRole::User => ContextMessageRole::User,
        MessageRole::Assistant => ContextMessageRole::Assistant,
    }
}

fn role_label(role: ContextMessageRole) -> &'static str {
    match role {
        ContextMessageRole::System => "system",
        ContextMessageRole::User => "user",
        ContextMessageRole::Assistant => "assistant",
        ContextMessageRole::ToolResult => "tool result",
    }
}

/// The text a person would read in a block; `None` for state that is replayed
/// to the provider and never shown.
fn block_text(block: &ContentBlock) -> Option<String> {
    Some(match block {
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.clone(),
        ContentBlock::ResponsesReasoning { .. } => return None,
        ContentBlock::Anthropic { .. } => block.reasoning_text().unwrap_or_default().to_owned(),
        ContentBlock::Image { media_type, .. } => format!("[image {media_type}]"),
        ContentBlock::ToolUse { name, input, .. } => format!("→ {name} {input}"),
        ContentBlock::ToolResult { content, .. } => content
            .iter()
            .map(providers::ToolResultContent::text_or_image_note)
            .collect::<Vec<_>>()
            .join("\n"),
        ContentBlock::Compaction { .. } => "[compaction summary]".into(),
    })
}

fn context_message(index: usize, message: &Message) -> ContextMessage {
    let text = message
        .content
        .iter()
        .filter_map(block_text)
        .collect::<Vec<_>>()
        .join("\n\n");
    let native_only = text.is_empty()
        && message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ResponsesReasoning { .. }));
    ContextMessage {
        index: u32::try_from(index).unwrap_or(u32::MAX),
        role: role_of(message),
        summary: first_line(&text, MAX_CONTEXT_SUMMARY_BYTES.min(120)),
        tokens: runtime::base_context::estimate_text_tokens(
            &serde_json::to_string(message).unwrap_or_default(),
        ),
        listed: !native_only,
    }
}

fn context_read(
    run: workbench_protocol::ids::RunId,
    view: &runtime::base_context::RunContextView,
) -> DiagnosticsRead {
    let messages: Vec<ContextMessage> = view
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| context_message(index, message))
        .collect();
    let mut segments: Vec<ContextSegment> = [
        ContextMessageRole::System,
        ContextMessageRole::User,
        ContextMessageRole::Assistant,
        ContextMessageRole::ToolResult,
    ]
    .into_iter()
    .map(|role| ContextSegment {
        label: role_label(role).to_owned(),
        tokens: messages
            .iter()
            .filter(|message| message.role == role)
            .map(|message| message.tokens)
            .sum(),
    })
    .filter(|segment| segment.tokens > 0)
    .collect();
    segments.sort_by_key(|segment| std::cmp::Reverse(segment.tokens));
    segments.truncate(MAX_CONTEXT_SEGMENTS);
    let tools_omitted = view.tool_names.len().saturating_sub(MAX_CONTEXT_TOOLS);
    DiagnosticsRead::Context {
        overview: Box::new(RunContextOverview {
            run,
            role: first_line(&view.role_name, 256),
            category: view.category.as_deref().map(|c| first_line(c, 256)),
            phase: first_line(&view.checkpoint_phase, 256),
            selected_model: view.selected_model.as_deref().map(|m| first_line(m, 256)),
            updated_at_ms: u64::try_from(view.updated_at_ns / 1_000_000).unwrap_or(0),
            tools: view
                .tool_names
                .iter()
                .take(MAX_CONTEXT_TOOLS)
                .map(|name| first_line(name, 256))
                .collect(),
            tools_omitted: u32::try_from(tools_omitted).unwrap_or(u32::MAX),
            message_count: u32::try_from(messages.len()).unwrap_or(u32::MAX),
            compaction_checkpoints: u32::try_from(view.compaction_checkpoint_count)
                .unwrap_or(u32::MAX),
            segments,
        }),
        messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ledger as a roll-up leaves it: a row is in `requests` or in `daily`, never both.
    #[test]
    fn a_roll_up_between_the_two_reads_is_not_counted_twice() {
        use std::cell::Cell;
        let rolled_up = Cell::new(false);
        let daily_reads = Cell::new(0);
        // A roll-up commits right after the rows were read, once.
        let (rows, daily) = consistent_rows(
            || {
                daily_reads.set(daily_reads.get() + 1);
                Ok(if rolled_up.get() { 1 } else { 0 })
            },
            || {
                let rows = if rolled_up.get() { 0 } else { 1 };
                rolled_up.set(true);
                Ok(rows)
            },
        )
        .unwrap();
        assert_eq!(rows + daily, 1, "the row is counted once");
        assert_eq!(
            (rows, daily),
            (0, 1),
            "the read was repeated after the roll-up"
        );
        assert_eq!(daily_reads.get(), 4);
    }

    #[test]
    fn a_ledger_that_never_settles_fails_instead_of_returning_a_torn_read() {
        let ticks = std::cell::Cell::new(0);
        let failed = consistent_rows(
            || {
                ticks.set(ticks.get() + 1);
                Ok(ticks.get())
            },
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(failed.code, ErrorCode::StorageUnavailable);
        assert_eq!(ticks.get(), 2 * CONSISTENT_ATTEMPTS);
    }

    #[test]
    fn a_range_starts_on_the_right_local_day() {
        let first = |range, today| first_day(range, today);
        assert_eq!(
            first(UsageRange::Today, "2026-03-01").as_deref(),
            Some("2026-03-01")
        );
        assert_eq!(
            first(UsageRange::Last7Days, "2026-03-03").as_deref(),
            Some("2026-02-25")
        );
        assert_eq!(
            first(UsageRange::Last30Days, "2024-03-10").as_deref(),
            Some("2024-02-10")
        );
        assert_eq!(
            first(UsageRange::Last90Days, "2026-01-05").as_deref(),
            Some("2025-10-08")
        );
        assert_eq!(
            first(UsageRange::ThisMonth, "2026-10-31").as_deref(),
            Some("2026-10-01")
        );
        assert_eq!(first(UsageRange::AllTime, "2026-10-31"), None);
        assert_eq!(first(UsageRange::Today, "garbage"), None);
        for label in ["1970-01-01", "2000-02-29", "2024-02-29", "2100-03-01"] {
            assert_eq!(day_label(day_number(label).unwrap()), label);
        }
    }

    #[test]
    fn costs_are_micro_usd_and_never_negative_or_non_finite() {
        assert_eq!(micro_usd(0.000_001_4), 1);
        assert_eq!(micro_usd(1.5), 1_500_000);
        assert_eq!(micro_usd(-3.0), 0);
        assert_eq!(micro_usd(f64::NAN), 0);
        assert_eq!(micro_usd(f64::INFINITY), 0);
    }

    #[test]
    fn rolled_up_days_and_requests_add_up_per_day_and_model() {
        let daily = |day: &str, model: &str, requests: u64| UsageDailyRow {
            day: day.into(),
            provider: "main".into(),
            model: model.into(),
            request_count: requests,
            input_tokens: requests * 10,
            cost_usd: 0.1,
            unpriced_input_tokens: 5,
            unreported_count: 1,
            ..Default::default()
        };
        let rows = [daily("2026-01-01", "a", 2), daily("2026-01-02", "a", 3)];
        let summary = reduce(UsageRange::AllTime, "0000-01-01", "2026-01-03", &[], &rows);
        assert_eq!(summary.from_day, "2026-01-01");
        assert_eq!(summary.to_day, "2026-01-03");
        assert_eq!(summary.totals.requests, 5);
        assert_eq!(summary.totals.input_tokens, 50);
        assert_eq!(summary.totals.recorded_cost_micro_usd, 200_000);
        assert_eq!(summary.totals.unpriced_tokens, 10);
        assert_eq!(summary.totals.unreported_requests, 2);
        assert_eq!(summary.days.len(), 2);
        assert_eq!(summary.models.len(), 1);
    }
}
