//! What each committed event contributes to a transcript, as pure functions of the event.
//! Tool rows join their completion with the start that carried the input; every other
//! row comes from one event. Shared by the live scanner and the events-only index.
use event_bus::{
    AgentRunPhase, EventKind, LifecycleEvent, MessageEvent, ProviderEvent, ProviderFailureKind,
    ToolEvent,
};
use serde_json::Value;

use super::Role;

/// Longest one-line input summary kept per tool row. The protocol bounds what a client
/// receives; this only keeps the stored row small.
pub const SUMMARY_BYTES: usize = 16 * 1024;

type Parts = Vec<(String, bool)>;

/// Match the nanosecond precision and range of stored event headers.
pub fn wall_clock_ns(at: std::time::SystemTime) -> i64 {
    match at.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

/// Shared by indexed history, live history and live activity; backwards clocks are unknown.
pub fn duration_ms(start_ns: i64, end_ns: i64) -> Option<u64> {
    let nanos = i128::from(end_ns) - i128::from(start_ns);
    (nanos >= 0).then_some((nanos / 1_000_000) as u64)
}

/// The arguments of one tool call as shown in a transcript.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolInput {
    /// One line naming what the call acts on, when its arguments have an obvious subject.
    pub summary: Option<String>,
    /// The arguments as text; long strings stay references.
    pub parts: Parts,
}

/// Tool facts a `ToolCompleted` row carries, plus the input of its start when one was seen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolRow {
    pub tool_name: String,
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub is_error: bool,
    /// Wall-clock difference between the matching start and completion, when nonnegative.
    pub duration_ms: Option<u64>,
    /// `None` when no start event with arguments precedes the completion.
    pub input: Option<ToolInput>,
    pub detail: Option<Parts>,
}

/// One transcript row before it receives its position.
#[derive(Debug)]
pub struct Committed<'a> {
    pub run: &'a str,
    pub role: Role,
    pub parts: Parts,
    pub tool: Option<ToolRow>,
}

/// A `ToolStarted` of a run: `(run, call_id, input)`; the input is absent for old events.
pub fn tool_start(kind: &EventKind, tokenized: bool) -> Option<(&str, &str, Option<ToolInput>)> {
    let EventKind::Tool(ToolEvent::ToolStarted {
        tool_name,
        call_id,
        input,
        run_id: Some(run),
        ..
    }) = kind
    else {
        return None;
    };
    Some((
        run,
        call_id,
        input
            .as_ref()
            .map(|input| tool_input(tool_name, input, tokenized)),
    ))
}

/// The row `kind` adds to its run's transcript, if it adds one. `start` is the input of
/// the matching `ToolStarted` and only matters to `ToolCompleted`.
pub fn committed(
    kind: &EventKind,
    tokenized: bool,
    start: Option<ToolInput>,
) -> Option<Committed<'_>> {
    let text = |run, role, text: &str| {
        Some(Committed {
            run,
            role,
            parts: vec![(text.to_owned(), tokenized)],
            tool: None,
        })
    };
    match kind {
        EventKind::Message(
            MessageEvent::MessageCompleted { run_id, text: body }
            | MessageEvent::FinalResultPublished { run_id, text: body },
        ) => text(run_id, Role::Assistant, body),
        EventKind::Message(MessageEvent::ReasoningCompleted { run_id, text: body }) => {
            text(run_id, Role::Reasoning, body)
        }
        EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished {
            run_id,
            prompt,
            user_text,
            ..
        }) => text(run_id, Role::User, user_text.as_deref().unwrap_or(prompt)),
        EventKind::Tool(ToolEvent::ToolCompleted {
            tool_name,
            call_id,
            is_error,
            output,
            detail,
            run_id: Some(run),
            parent_call_id,
        }) => Some(Committed {
            run,
            role: Role::Tool,
            parts: output
                .as_ref()
                .map(|output| vec![(output.clone(), tokenized)])
                .unwrap_or_default(),
            tool: Some(ToolRow {
                tool_name: tool_name.clone(),
                call_id: call_id.clone(),
                parent_call_id: parent_call_id.clone(),
                is_error: *is_error,
                duration_ms: None,
                input: start,
                detail: detail
                    .as_ref()
                    .map(|detail| super::normalize(render(detail, tokenized))),
            }),
        }),
        _ => notice(kind, tokenized),
    }
}

pub fn tool_input(tool_name: &str, input: &Value, tokenized: bool) -> ToolInput {
    let parts = match input {
        // The source of a script is the argument worth reading verbatim, whatever else the call sets.
        Value::Object(map)
            if tool_name == "run_js" && map.get("code").is_some_and(Value::is_string) =>
        {
            let code = map["code"].as_str().unwrap_or_default();
            vec![(code.to_owned(), tokenized)]
        }
        _ => render(input, tokenized),
    };
    ToolInput {
        summary: summarize(tool_name, input, tokenized),
        parts: super::normalize(parts),
    }
}

fn token_bytes(value: &str, tokenized: bool) -> Option<u64> {
    tokenized
        .then(|| crate::BodyRef::from_token(value))
        .flatten()
        .map(|reference| reference.byte_len)
}

/// Compact JSON as parts. A long string stays a deferred part between two quote marks, so
/// nothing nested in an argument is lost and no body is read to build a row.
fn json_parts(value: &Value, tokenized: bool) -> Parts {
    fn walk(value: &Value, tokenized: bool, text: &mut String, parts: &mut Parts) {
        match value {
            Value::String(s) if token_bytes(s, tokenized).is_some() => {
                text.push('"');
                parts.push((std::mem::take(text), false));
                parts.push((s.clone(), true));
                text.push('"');
            }
            Value::Array(items) => {
                text.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        text.push(',');
                    }
                    walk(item, tokenized, text, parts);
                }
                text.push(']');
            }
            Value::Object(map) => {
                text.push('{');
                for (index, (key, item)) in map.iter().enumerate() {
                    if index > 0 {
                        text.push(',');
                    }
                    text.push_str(&Value::String(key.clone()).to_string());
                    text.push(':');
                    walk(item, tokenized, text, parts);
                }
                text.push('}');
            }
            other => text.push_str(&other.to_string()),
        }
    }
    let mut parts = Vec::new();
    let mut text = String::new();
    walk(value, tokenized, &mut text, &mut parts);
    parts.push((text, false));
    parts
}

/// An object becomes one `key: value` line per entry with strings shown raw, anything else
/// compact JSON. A long string stays a deferred part so no body is read to build a row.
fn render(value: &Value, tokenized: bool) -> Parts {
    let one = |value: &Value| match value {
        Value::String(s) => vec![(s.clone(), token_bytes(s, tokenized).is_some())],
        other => json_parts(other, tokenized),
    };
    let Value::Object(map) = value else {
        return one(value);
    };
    let mut parts = Vec::with_capacity(map.len() * 3);
    for (index, (key, value)) in map.iter().enumerate() {
        parts.push((
            if index == 0 { "" } else { "\n" }.to_owned() + key + ": ",
            false,
        ));
        parts.extend(one(value));
    }
    parts
}

const FILE_KEYS: [&str; 6] = [
    "path",
    "file_path",
    "filePath",
    "file",
    "filename",
    "target",
];
const SUBJECT_KEYS: [&str; 10] = [
    "command",
    "url",
    "query",
    "pattern",
    "name",
    "objective",
    "code",
    "prompt",
    "description",
    "text",
];

fn first_line(value: &str) -> Option<String> {
    let line = value.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut end = line.len().min(SUMMARY_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    Some(
        line[..end]
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
    )
}

fn summarize(tool_name: &str, input: &Value, tokenized: bool) -> Option<String> {
    let map = input.as_object()?;
    let string = |key: &str| -> Option<String> {
        let value = map.get(key)?.as_str()?;
        match token_bytes(value, tokenized) {
            Some(bytes) => Some(format!("{key} ({bytes} bytes)")),
            None => first_line(value),
        }
    };
    if matches!(tool_name, "shell" | "bash")
        && let (Some(action), Some(job)) = (string("action"), string("job_id"))
        && matches!(action.as_str(), "poll" | "stdin" | "stop")
    {
        return Some(format!("{action} {job}"));
    }
    if let (Some(pattern), Some(path)) = (string("pattern"), string("path")) {
        return Some(format!("\"{pattern}\" in {path}"));
    }
    if let Some(found) = FILE_KEYS
        .iter()
        .chain(&SUBJECT_KEYS)
        .find_map(|key| string(key))
    {
        return Some(found);
    }
    let scalars: Vec<String> = map
        .iter()
        .filter_map(|(key, value)| match value {
            Value::String(_) => string(key).map(|s| format!("{key}={s}")),
            Value::Number(_) | Value::Bool(_) => Some(format!("{key}={value}")),
            _ => None,
        })
        .take(4)
        .collect();
    (!scalars.is_empty()).then(|| scalars.join(", "))
}

fn failure(kind: &ProviderFailureKind) -> String {
    match kind {
        ProviderFailureKind::RateLimited => "rate limited".into(),
        ProviderFailureKind::Http { status } => format!("HTTP {status}"),
        ProviderFailureKind::Timeout => "timeout".into(),
        ProviderFailureKind::InvalidResponse => "invalid response".into(),
        ProviderFailureKind::Transport => "transport error".into(),
        ProviderFailureKind::Server => "server error".into(),
        ProviderFailureKind::Quota => "quota exceeded".into(),
        ProviderFailureKind::Auth => "authentication failed".into(),
        ProviderFailureKind::Other => "unclassified failure".into(),
    }
}

fn named(profile: &str, model: Option<&str>) -> String {
    model.map_or_else(|| profile.to_owned(), |model| format!("{profile}/{model}"))
}

/// Provider and run-state events a reader should see in the transcript, with the text the
/// core wrote for them. The set is closed: any other event is not a transcript row.
fn notice(kind: &EventKind, tokenized: bool) -> Option<Committed<'_>> {
    let one = |run, role, text: String| {
        Some(Committed {
            run,
            role,
            parts: vec![(text, false)],
            tool: None,
        })
    };
    match kind {
        EventKind::Provider(ProviderEvent::RequestFailed {
            provider,
            model,
            failure: kind,
            detail,
            run_id: Some(run),
            ..
        }) => {
            let mut text = format!("Request to {provider} ({model}) failed: {}", failure(kind));
            if let Some(detail) = detail {
                text.push_str(": ");
                text.push_str(detail);
            }
            one(run, Role::Error, text)
        }
        EventKind::Provider(ProviderEvent::FallbackTriggered {
            from_provider,
            from_model,
            to_provider,
            to_model,
            failure: kind,
            session_id,
            ..
        }) => one(
            session_id,
            Role::Notice,
            format!(
                "Switched from {} to {} after {}",
                named(from_provider, from_model.as_deref()),
                named(to_provider, Some(to_model)),
                failure(kind)
            ),
        ),
        EventKind::Provider(ProviderEvent::CooldownStarted {
            profile,
            model,
            failure: kind,
            cooldown_ms,
            run_id: Some(run),
        }) => one(
            run,
            Role::Notice,
            format!(
                "Paused {} after {} {}",
                named(profile, model.as_deref()),
                failure(kind),
                cooldown_ms.map_or_else(
                    || "until reconfigured".to_owned(),
                    |ms| format!("for {}s", ms.div_ceil(1000))
                )
            ),
        ),
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            from,
            to: to @ (AgentRunPhase::Error | AgentRunPhase::Stopped),
            reason,
            ..
        }) if from != to => {
            let (role, head) = if *to == AgentRunPhase::Error {
                (Role::Error, "Run failed")
            } else {
                (Role::Notice, "Run stopped")
            };
            let mut parts = vec![(head.to_owned(), false)];
            if let Some(reason) = reason {
                parts.push((": ".to_owned(), false));
                parts.push((reason.clone(), tokenized));
            }
            Some(Committed {
                run: run_id,
                role,
                parts: super::normalize(parts),
                tool: None,
            })
        }
        _ => None,
    }
}
