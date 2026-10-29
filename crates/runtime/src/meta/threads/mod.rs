//! Read-only thread tools (ADR 0041): `thread_search`, `thread_grep` and `thread_read`.
//!
//! Every target passes [`visibility::resolve`] before any of its records are read.
//! Conversation text is normalized and redacted before it is matched or returned,
//! so a pattern can never probe a secret. Every success response carries
//! [`normalize::UNTRUSTED_NOTE`] and stays within [`MAX_OUTPUT_BYTES`].

mod conversation;
mod cursor;
mod grep;
mod host;
pub(super) mod normalize;
mod read;
mod search;
#[cfg(test)]
mod tests;
pub(super) mod visibility;

#[cfg(test)]
pub(super) use grep::GrepArgs;
pub use host::{HOST_SEARCH_MAX_LIMIT, HostSearchIndex, HostThreadHit, HostThreadSearch};
#[cfg(test)]
pub(super) use read::ReadArgs;
#[cfg(test)]
pub(super) use search::SearchArgs;

use std::collections::HashMap;

use regex::{Regex, RegexBuilder};
use secret_guard::SecretRedactor;
use serde::de::DeserializeOwned;
use serde_json::Value;
use storage::threads::{RunTerminalSummary, ThreadRow, ThreadRunRow};

use super::{DispatchResult, parse, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, RunStore};
use visibility::{Caller, Target};

/// Hard cap of every thread-tool response.
pub(crate) const MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// Longest accepted search term or grep pattern, in bytes.
const MAX_PATTERN_BYTES: usize = 1024;

/// Longest accepted `thread` argument. Real thread and run IDs are far shorter;
/// the limit keeps a denial, which echoes the ID, within the output cap.
pub(crate) const MAX_THREAD_ARG_BYTES: usize = 256;

/// Compiled-program limit for model-supplied regular expressions.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Storage failures after authorization never echo stored text.
const STORE_ERROR: &str = "thread_unavailable: the thread's saved records could not be read";

/// Thread errors can contain serde/schema diagnostics echoing arbitrary input.
/// Redact before clipping, so a cut cannot expose an unrecognized secret prefix.
fn error(reason: impl Into<String>) -> DispatchResult {
    super::error(safe_error(&reason.into()))
}

fn safe_error(reason: &str) -> String {
    clip_bytes(
        &SecretRedactor::from_env().redact(reason).text,
        MAX_OUTPUT_BYTES,
    )
}

pub(crate) fn bound_result(name: &str, mut result: tools::ToolResult) -> tools::ToolResult {
    if matches!(name, "thread_search" | "thread_grep" | "thread_read") {
        if result.is_error {
            result.content = safe_error(&result.content);
            result.detail = None;
        } else if result.content.len() > MAX_OUTPUT_BYTES {
            return tools::ToolResult::error("thread_output_too_large: output exceeds 16 KiB");
        }
    }
    result
}

pub(super) fn search(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    invoke(state, runtime, input, search::run)
}

pub(super) fn grep(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    invoke(state, runtime, input, grep::run)
}

pub(super) fn read(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    invoke(state, runtime, input, read::run)
}

fn invoke<A: DeserializeOwned>(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: Value,
    handler: fn(&Scope<'_>, A) -> Result<Value, String>,
) -> DispatchResult {
    let args = match parse::<A>(input) {
        Ok(args) => args,
        Err(reason) => return error(reason),
    };
    let Some(store) = runtime.shared.run_store.get() else {
        return error("thread_store_unavailable: thread tools need the persistent run store");
    };
    let run_id = state.caller_run_id();
    // A root restored after a restart may have lost its live thread binding; the
    // saved binding was written by the runtime itself, never by a tool argument.
    let thread_id = state.thread_id.clone().or_else(|| {
        store
            .thread_of_run(&run_id.to_string())
            .ok()
            .flatten()
            .map(|binding| binding.thread_id)
    });
    let caller = Caller::new(
        run_id,
        thread_id,
        state.run_config().project_root.as_deref(),
    );
    let scope = Scope::new(store, runtime, caller, SecretRedactor::from_env());
    match handler(&scope, args) {
        Ok(value) => success(scope.output(value).to_string()),
        Err(reason) => error(scope.redact(&reason)),
    }
}

/// One invocation. Caller identity comes from the run loop, never from arguments.
pub(crate) struct Scope<'a> {
    pub(crate) store: &'a RunStore,
    pub(crate) runtime: &'a AgentRuntime,
    pub(crate) caller: Caller,
    pub(crate) redactor: SecretRedactor,
}

impl<'a> Scope<'a> {
    pub(crate) fn new(
        store: &'a RunStore,
        runtime: &'a AgentRuntime,
        caller: Caller,
        redactor: SecretRedactor,
    ) -> Self {
        Self {
            store,
            runtime,
            caller,
            redactor,
        }
    }

    /// The only way a tool turns an ID into a thread.
    fn resolve(&self, id: &str) -> Result<ThreadRow, String> {
        visibility::resolve(self.store, self.runtime, &self.caller, Target::parse(id))
    }

    /// Resolve a model-supplied `thread` argument. The length check depends on
    /// the argument alone, so it reveals nothing about which threads exist.
    fn resolve_argument(&self, id: &str) -> Result<ThreadRow, String> {
        if id.is_empty() || id.len() > MAX_THREAD_ARG_BYTES {
            return Err(format!(
                "thread_args_invalid: thread must be a thread or run ID of 1-{MAX_THREAD_ARG_BYTES} bytes"
            ));
        }
        self.resolve(id)
    }

    fn redact(&self, text: &str) -> String {
        self.redactor.redact(text).text
    }

    /// Redact keys as well as values after composing metadata; raw rows remain
    /// separate for authorization and cursor comparisons.
    fn output(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.redact(&text)),
            Value::Array(values) => {
                Value::Array(values.into_iter().map(|v| self.output(v)).collect())
            }
            Value::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (self.redact(&key), self.output(value)))
                    .collect(),
            ),
            other => other,
        }
    }

    /// Runs of an authorized thread with their saved terminal state.
    fn members(&self, row: &ThreadRow) -> Result<Members, String> {
        self.load_members(row, true)
    }

    /// Search chooses roots and displays phases using only narrow headers.
    /// Full final reports are loaded later, only for examined metadata.
    fn search_members(&self, row: &ThreadRow) -> Result<Members, String> {
        self.load_members(row, false)
    }

    fn load_members(&self, row: &ThreadRow, with_summaries: bool) -> Result<Members, String> {
        let runs = self
            .store
            .runs_of_thread(&row.thread_id)
            .map_err(|_| STORE_ERROR.to_owned())?;
        let ids: Vec<String> = runs.iter().map(|run| run.run_id.clone()).collect();
        let summaries: HashMap<String, RunTerminalSummary> = if with_summaries {
            self.store
                .terminal_summaries(&ids)
                .map_err(|_| STORE_ERROR.to_owned())?
        } else {
            Vec::new()
        }
        .into_iter()
        .map(|summary| (summary.run_id.clone(), summary))
        .collect();
        let headers: HashMap<String, storage::RunContextHeader> = if with_summaries {
            summaries
                .values()
                .map(|summary| storage::RunContextHeader {
                    run_id: summary.run_id.clone(),
                    terminal_phase: summary.terminal_phase.clone(),
                    updated_at_ns: summary.updated_at_ns,
                })
                .collect()
        } else {
            self.store
                .run_context_headers(&ids)
                .map_err(|_| STORE_ERROR.to_owned())?
        }
        .into_iter()
        .map(|header| (header.run_id.clone(), header))
        .collect();
        // Each chat turn saves a new root whose history extends the previous one.
        let primary = runs
            .iter()
            .filter(|run| run.depth == 0)
            .filter_map(|run| headers.get(&run.run_id))
            .max_by(|left, right| {
                (left.updated_at_ns, &left.run_id).cmp(&(right.updated_at_ns, &right.run_id))
            })
            .map(|summary| summary.run_id.clone())
            .or_else(|| row.root_run_id.clone());
        Ok(Members {
            runs,
            headers,
            summaries,
            primary,
        })
    }

    /// Live phase and reason when registered, else the saved terminal phase.
    fn status(&self, run_id: &str, saved_phase: Option<&str>) -> (Option<String>, Option<String>) {
        if let Some((phase, reason)) = run_id
            .parse()
            .ok()
            .and_then(|run| self.runtime.live_run_status(run))
        {
            return (
                Some(format!("{phase:?}")),
                reason.map(|reason| self.redact(&reason)),
            );
        }
        (saved_phase.map(str::to_owned), None)
    }
}

/// A thread's runs: conversation roots (depth 0) and delegated descendants.
pub(crate) struct Members {
    runs: Vec<ThreadRunRow>,
    headers: HashMap<String, storage::RunContextHeader>,
    summaries: HashMap<String, RunTerminalSummary>,
    /// The latest saved root, whose history contains the earlier turns.
    primary: Option<String>,
}

impl Members {
    fn is_saved_member(&self, run_id: &str) -> bool {
        self.headers.contains_key(run_id) && self.runs.iter().any(|run| run.run_id == run_id)
    }

    /// The current root and every delegated run; earlier roots are prefixes of it.
    fn default_scan(&self) -> Vec<String> {
        self.primary
            .iter()
            .cloned()
            .chain(
                self.runs
                    .iter()
                    .filter(|run| run.depth > 0)
                    .map(|run| run.run_id.clone()),
            )
            .collect()
    }
}

fn kind_label(kind: normalize::BlockKind) -> &'static str {
    match kind {
        normalize::BlockKind::Text => "text",
        normalize::BlockKind::ToolUse => "tool_use",
        normalize::BlockKind::ToolResult => "tool_result",
        normalize::BlockKind::ImageNote => "image",
    }
}

fn role_label(role: providers::Role) -> &'static str {
    match role {
        providers::Role::System => "system",
        providers::Role::User => "user",
        providers::Role::Assistant => "assistant",
    }
}

/// Bounded diagnostics, so notes can never crowd out the content they describe.
fn bounded_notes(mut notes: Vec<String>) -> Vec<String> {
    const MAX_NOTES: usize = 8;
    if notes.len() > MAX_NOTES {
        let more = notes.len() - MAX_NOTES;
        notes.truncate(MAX_NOTES);
        notes.push(format!("{more} more notes omitted"));
    }
    notes
}

fn not_in_thread(run: &str, thread: &str) -> String {
    // `run` comes from tool arguments; keep the echo short.
    let run = clip_chars(run, 80);
    format!("thread_run_not_found: {run} is not a saved run of thread {thread}")
}

fn yes() -> bool {
    true
}

/// Deserializes an optional string argument, treating an empty string like an
/// omitted one. Model clients commonly fill every schema property, sending `""`
/// for the ones they have nothing to pass.
fn empty_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.filter(|text| !text.is_empty()))
}

fn check_pattern(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() || pattern.len() > MAX_PATTERN_BYTES {
        return Err(format!(
            "thread_pattern_invalid: patterns must be 1-{MAX_PATTERN_BYTES} bytes and not blank"
        ));
    }
    Ok(())
}

/// Case-insensitive literal.
fn literal(pattern: &str) -> Result<Regex, String> {
    check_pattern(pattern)?;
    build(&regex::escape(pattern), true)
}

/// Model-supplied regular expression, size-limited.
fn expression(pattern: &str) -> Result<Regex, String> {
    check_pattern(pattern)?;
    build(pattern, false)
}

fn build(pattern: &str, case_insensitive: bool) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|reason| format!("thread_pattern_invalid: {reason}"))
}

/// At most `max` characters, marking a cut with an ellipsis.
fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// Long identifiers are abbreviated only for display, never for pairing or
/// repeat collapsing (different tools can share the same displayed prefix).
fn identifier(text: &str) -> String {
    clip_chars(text, 80)
}

/// At most `max` UTF-8 bytes, marking a cut with an ellipsis.
fn clip_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// `context` characters on each side of `text[start..end]`, the match itself
/// capped at `match_chars`; cuts are marked with an ellipsis.
fn snippet(text: &str, start: usize, end: usize, context: usize, match_chars: usize) -> String {
    let before: Vec<char> = text[..start].chars().rev().take(context).collect();
    let before: String = before.into_iter().rev().collect();
    let mut result = String::new();
    if before.len() < start {
        result.push('…');
    }
    result.push_str(&before);
    result.push_str(&clip_chars(&text[start..end], match_chars));
    let after: String = text[end..].chars().take(context).collect();
    result.push_str(&after);
    if end + after.len() < text.len() {
        result.push('…');
    }
    result
}

/// The largest nonempty page that fits, or an error rather than a stuck cursor.
fn fit(budget: usize, total: usize, render: impl Fn(usize) -> Value) -> Result<Value, String> {
    let mut count = total;
    loop {
        let value = render(count);
        if value.to_string().len() <= budget && (total == 0 || count > 0) {
            return Ok(value);
        }
        if count == 0 {
            return Err("thread_output_too_large: metadata cannot fit the output budget".into());
        }
        count -= 1;
    }
}
