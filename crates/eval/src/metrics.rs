//! What one replay did, read back from its own database (ADR 0036 §7): token
//! and tool counts, delegations, failures, a bounded event timeline for the
//! process analysis, and signs that it depended on unavailable resources.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::SystemTime;

use event_bus::{
    AgentRunPhase, Event, EventKind, LifecycleEvent, ProviderEvent, ToolEvent,
    event::diagnostic_codes,
};
use serde::{Deserialize, Serialize};

use crate::cost::{RequestUsage, Tokens};
use crate::driver::DeniedRequest;
use crate::output::Outcome;

const LINE_CHARS: usize = 240;
const INPUT_CHARS: usize = 160;
const TIMELINE_HEAD: usize = 150;
const TIMELINE_TAIL: usize = 150;
const EXTERNAL_FLAGS: usize = 10;
const WEB_TOOLS: [&str; 2] = ["web_search", "web_fetch"];
const NETWORK_ERRORS: [&str; 10] = [
    "could not resolve host",
    "temporary failure in name resolution",
    "name or service not known",
    "failed to lookup address",
    "dns error",
    "network is unreachable",
    "network unreachable",
    "no route to host",
    "failed to connect to",
    "connection timed out",
];
const NOTABLE_DIAGNOSTICS: [&str; 5] = [
    diagnostic_codes::NO_PROGRESS,
    diagnostic_codes::IDENTICAL_TOOL_CALLS,
    diagnostic_codes::DELEGATION_RETRACTED,
    diagnostic_codes::BUDGET_EXHAUSTED,
    diagnostic_codes::TOOL_ARGUMENTS_MALFORMED,
];

/// The persisted record of one replay. A replay that never started has none.
#[derive(Debug, Default)]
pub struct RunRecord {
    pub requests: Vec<RequestUsage>,
    pub events: Vec<Event>,
}

impl RunRecord {
    /// # Errors
    /// Fails when an existing database cannot be read.
    pub fn load(db: &Path) -> Result<Self, storage::StorageError> {
        if !db.exists() {
            return Ok(Self::default());
        }
        let database = storage::Database::open_read_only(db)?;
        let requests = database
            .usage_requests_between(i64::MIN, i64::MAX)?
            .iter()
            .map(|row| RequestUsage::from_record(&row.record))
            .collect();
        let events = database
            .events_all_ordered()?
            .into_iter()
            .map(|stored| stored.event)
            .collect();
        Ok(Self { requests, events })
    }

    /// The record of `run_ids` in the user's database: their usage ledger
    /// rows, and up to `max_events` events from `from_ns` on. Also returns
    /// whether events were left out.
    ///
    /// # Errors
    /// Fails when the database cannot be read.
    pub fn of_runs(
        database: &storage::Database,
        run_ids: &[String],
        from_ns: i64,
        max_events: usize,
    ) -> Result<(Self, bool), storage::StorageError> {
        let requests = database
            .usage_requests_for_runs(run_ids)?
            .iter()
            .map(|row| RequestUsage::from_record(&row.record))
            .collect();
        let (events, truncated) = database.events_for_runs(run_ids, from_ns, max_events)?;
        let events = events.into_iter().map(|stored| stored.event).collect();
        Ok((Self { requests, events }, truncated))
    }

    /// Wall-clock time from the first to the last recorded event.
    pub fn elapsed_ms(&self) -> u64 {
        let (Some(first), Some(last)) = (self.events.first(), self.events.last()) else {
            return 0;
        };
        last.meta
            .wall_clock
            .duration_since(first.meta.wall_clock)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }

    pub fn agent_requests(&self) -> impl Iterator<Item = &RequestUsage> {
        self.requests
            .iter()
            .filter(|request| !request.is_overhead())
    }

    pub fn overhead_requests(&self) -> impl Iterator<Item = &RequestUsage> {
        self.requests.iter().filter(|request| request.is_overhead())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    pub role: String,
    pub agent_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub model: String,
    pub requests: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMetrics {
    /// The agent's own requests; the evaluation's are in `overhead`.
    pub tokens: Tokens,
    pub cache_hit_rate_percent: f64,
    /// The simulated user's requests, by purpose.
    pub overhead: BTreeMap<String, Tokens>,
    pub models: Vec<ModelUse>,
    pub tool_calls: BTreeMap<String, u32>,
    pub tool_errors: BTreeMap<String, u32>,
    pub provider_failures: u32,
    pub provider_fallbacks: u32,
    pub delegations: Vec<Delegation>,
    pub denied_requests: usize,
    pub elapsed_ms: u64,
    pub user_turns: u32,
}

impl RunMetrics {
    pub fn of(record: &RunRecord, outcome: &Outcome) -> Self {
        Self::from_record(
            record,
            outcome.denied_requests.len(),
            outcome.elapsed_ms,
            outcome.user_turns,
        )
    }

    /// Metrics of a record without a replay outcome, such as a thread read
    /// from the user's database.
    pub fn from_record(
        record: &RunRecord,
        denied_requests: usize,
        elapsed_ms: u64,
        user_turns: u32,
    ) -> Self {
        let tokens = Tokens::of(record.agent_requests());
        let mut overhead: BTreeMap<String, Tokens> = BTreeMap::new();
        for request in record.overhead_requests() {
            overhead
                .entry(request.purpose.clone().unwrap_or_default())
                .or_default()
                .add(request);
        }
        let mut models: Vec<ModelUse> = Vec::new();
        for request in record.agent_requests() {
            match models.iter_mut().find(|known| {
                (&known.role, &known.provider, &known.profile, &known.model)
                    == (
                        &request.role,
                        &request.provider,
                        &request.profile,
                        &request.model,
                    )
            }) {
                Some(known) => known.requests += 1,
                None => models.push(ModelUse {
                    role: request.role.clone(),
                    provider: request.provider.clone(),
                    profile: request.profile.clone(),
                    model: request.model.clone(),
                    requests: 1,
                }),
            }
        }
        let mut metrics = Self {
            cache_hit_rate_percent: tokens.cache_hit_rate(),
            tokens,
            overhead,
            models,
            tool_calls: BTreeMap::new(),
            tool_errors: BTreeMap::new(),
            provider_failures: tokens.failed_requests,
            provider_fallbacks: 0,
            delegations: Vec::new(),
            denied_requests,
            elapsed_ms,
            user_turns,
        };
        for event in &record.events {
            match &event.kind {
                EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) => {
                    *metrics.tool_calls.entry(tool_name.clone()).or_default() += 1;
                }
                EventKind::Tool(ToolEvent::ToolCompleted {
                    tool_name,
                    is_error: true,
                    ..
                }) => {
                    *metrics.tool_errors.entry(tool_name.clone()).or_default() += 1;
                }
                EventKind::Provider(ProviderEvent::FallbackTriggered { .. }) => {
                    metrics.provider_fallbacks += 1;
                }
                EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                    run_id,
                    parent_run_id,
                    agent_name,
                    role,
                }) => metrics.delegations.push(Delegation {
                    run_id: run_id.clone(),
                    parent_run_id: parent_run_id.clone(),
                    role: role.clone(),
                    agent_name: agent_name.clone(),
                }),
                _ => {}
            }
        }
        metrics
    }
}

pub(crate) fn clip(text: &str, chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text,
    }
}

fn role_of(roles: &HashMap<&str, &str>, run: Option<&String>) -> String {
    run.and_then(|run| roles.get(run.as_str()).copied())
        .unwrap_or("?")
        .to_owned()
}

fn input_text(input: Option<&serde_json::Value>) -> String {
    clip(
        &input.map(ToString::to_string).unwrap_or_default(),
        INPUT_CHARS,
    )
}

/// Tool calls the policy or the user refused, with the input the approval
/// request showed.
pub fn denied_executions(record: &RunRecord) -> Vec<DeniedRequest> {
    let mut inputs: HashMap<&str, String> = HashMap::new();
    let mut denied = Vec::new();
    for event in &record.events {
        match &event.kind {
            EventKind::Tool(ToolEvent::ApprovalRequested { call_id, input, .. }) => {
                inputs.insert(call_id, input_text(input.as_ref()));
            }
            EventKind::Tool(ToolEvent::ExecutionDenied {
                tool_name,
                call_id,
                reason,
            }) => denied.push(DeniedRequest {
                tool: tool_name.clone(),
                input: inputs.get(call_id.as_str()).cloned().unwrap_or_default(),
                reason: clip(reason, INPUT_CHARS),
            }),
            _ => {}
        }
    }
    denied
}

/// Evidence that the replay needed something outside the evaluation
/// environment: denied requests, failed web tools, and shell commands whose
/// output reads like a network or DNS failure. At most `EXTERNAL_FLAGS` lines.
pub fn external_dependencies(record: &RunRecord, outcome: &Outcome) -> Vec<String> {
    let mut flags: Vec<String> = outcome
        .denied_requests
        .iter()
        .map(|denied| {
            format!(
                "denied {}: {}",
                denied.tool,
                clip(&denied.input, INPUT_CHARS)
            )
        })
        .collect();
    let mut inputs: HashMap<&str, Option<&serde_json::Value>> = HashMap::new();
    for event in &record.events {
        match &event.kind {
            EventKind::Tool(ToolEvent::ToolStarted { call_id, input, .. }) => {
                inputs.insert(call_id, input.as_ref());
            }
            EventKind::Tool(ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                is_error: true,
                output,
                ..
            }) => {
                let input = input_text(inputs.get(call_id.as_str()).copied().flatten());
                if WEB_TOOLS.contains(&tool_name.as_str()) {
                    flags.push(format!("failed {tool_name}: {input}"));
                } else if tool_name == "shell"
                    && output.as_deref().is_some_and(|output| {
                        let output = output.to_ascii_lowercase();
                        NETWORK_ERRORS.iter().any(|marker| output.contains(marker))
                    })
                {
                    flags.push(format!("shell network failure: {input}"));
                }
            }
            _ => {}
        }
    }
    if flags.len() > EXTERNAL_FLAGS {
        let more = flags.len() - EXTERNAL_FLAGS;
        flags.truncate(EXTERNAL_FLAGS);
        flags.push(format!("and {more} more"));
    }
    flags
}

/// A summarized, size-bounded timeline of the replay for the process analysis:
/// delegations, tool calls with short inputs, durations and errors, turns,
/// provider failures and notable diagnostics, after token totals per role.
pub fn timeline(record: &RunRecord) -> Vec<String> {
    timeline_within(record, TIMELINE_HEAD, TIMELINE_TAIL)
}

/// [`timeline`] keeping the first `head` and the last `tail` steps.
pub fn timeline_within(record: &RunRecord, head: usize, tail: usize) -> Vec<String> {
    let mut by_role: BTreeMap<String, Tokens> = BTreeMap::new();
    for request in record.agent_requests() {
        by_role
            .entry(request.role.clone().unwrap_or_else(|| "unknown".into()))
            .or_default()
            .add(request);
    }
    let mut lines: Vec<String> = by_role
        .iter()
        .map(|(role, tokens)| {
            format!(
                "tokens [{role}]: {} requests, input {} (cache read {}, cache write {}), output {}",
                tokens.requests,
                tokens.input(),
                tokens.cache_read,
                tokens.cache_write,
                tokens.output
            )
        })
        .collect();
    let start = record.events.first().map(|event| event.meta.wall_clock);
    let at = |event: &Event| {
        start
            .and_then(|start| event.meta.wall_clock.duration_since(start).ok())
            .unwrap_or_default()
            .as_secs_f64()
    };
    let mut roles: HashMap<&str, &str> = HashMap::new();
    let mut started: HashMap<&str, (SystemTime, String)> = HashMap::new();
    let mut steps = Vec::new();
    for event in &record.events {
        let line = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                role: started_role,
                ..
            }) => {
                roles.insert(run_id, started_role);
                Some(match parent_run_id {
                    Some(parent) => format!(
                        "[{}] delegated to {started_role} ({run_id})",
                        role_of(&roles, Some(parent))
                    ),
                    None => format!("[{started_role}] root run started ({run_id})"),
                })
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) if matches!(
                to,
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            ) =>
            {
                Some(format!(
                    "[{}] run {to:?}{}",
                    role_of(&roles, Some(run_id)),
                    reason
                        .as_deref()
                        .map(|reason| format!(": {}", clip(reason, INPUT_CHARS)))
                        .unwrap_or_default()
                ))
            }
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
                run_id,
                context_len,
            }) => Some(format!(
                "[{}] turn completed (context {context_len})",
                role_of(&roles, Some(run_id))
            )),
            EventKind::Tool(ToolEvent::ToolStarted { call_id, input, .. }) => {
                started.insert(call_id, (event.meta.wall_clock, input_text(input.as_ref())));
                None
            }
            EventKind::Tool(ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                is_error,
                run_id,
                ..
            }) => {
                let (began, input) = started
                    .remove(call_id.as_str())
                    .unwrap_or((event.meta.wall_clock, String::new()));
                let took = event
                    .meta
                    .wall_clock
                    .duration_since(began)
                    .unwrap_or_default()
                    .as_secs_f64();
                Some(format!(
                    "[{}] {tool_name} {input} -> {} in {took:.1}s",
                    role_of(&roles, run_id.as_ref()),
                    if *is_error { "error" } else { "ok" }
                ))
            }
            EventKind::Provider(ProviderEvent::RequestFailed {
                model,
                failure,
                run_id,
                purpose,
                ..
            }) if purpose.is_none_or(|purpose| {
                !matches!(
                    purpose,
                    event_bus::RequestPurpose::UserSimulation | event_bus::RequestPurpose::Judging
                )
            }) =>
            {
                Some(format!(
                    "[{}] provider request to {model} failed: {failure:?}",
                    role_of(&roles, run_id.as_ref())
                ))
            }
            EventKind::Provider(ProviderEvent::FallbackTriggered {
                to_provider,
                to_model,
                ..
            }) => Some(format!("provider fallback to {to_provider}/{to_model}")),
            EventKind::Diagnostic(diagnostic)
                if NOTABLE_DIAGNOSTICS.contains(&diagnostic.code.as_str()) =>
            {
                Some(format!(
                    "[{}] {}: {}",
                    role_of(&roles, diagnostic.run_id.as_ref()),
                    diagnostic.code,
                    clip(&diagnostic.detail, INPUT_CHARS)
                ))
            }
            _ => None,
        };
        if let Some(line) = line {
            steps.push(clip(&format!("+{:.1}s {line}", at(event)), LINE_CHARS));
        }
    }
    if steps.len() > head + tail {
        let omitted = steps.len() - head - tail;
        let tail = steps.split_off(steps.len() - tail);
        steps.truncate(head);
        steps.push(format!("[{omitted} events omitted]"));
        steps.extend(tail);
    }
    lines.extend(steps);
    lines
}

#[cfg(test)]
mod tests;
