//! Headers for runtime-owned coordination tools: delegation, agent messages,
//! waits, questions, todos, team tasks and reads of other threads.

use serde_json::Value;

use super::header::{ToolHeader, one_line};
use crate::model::tasks::TaskRow;
use crate::theme::icons;

const PREVIEW_CHARS: usize = 60;

/// Why a finished `wait` returned: the distinct event kinds it reported, or
/// that nothing was left to wait for. Phase-only and unparsable results have no
/// reason to show.
fn wait_reason(result: &str) -> Option<String> {
    const SHOWN_KINDS: usize = 3;
    let result: Value = serde_json::from_str(result).ok()?;
    if result.get("status")?.as_str()? == "no_wait_sources" {
        return Some("nothing to wait for".to_owned());
    }
    let mut labels: Vec<&str> = Vec::new();
    for kind in result
        .get("events")?
        .as_array()?
        .iter()
        .filter_map(|event| event.get("kind")?.as_str())
    {
        let label = match kind {
            "message" => "message",
            "run_completed" => "run finished",
            "turn_completed" => "turn finished",
            "parent_terminated" => "parent ended",
            "question" => "question",
            "answer_ready" => "answer",
            "lane" => "lane",
            "shell" => "shell job",
            _ => continue,
        };
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        return None;
    }
    let hidden = labels.len().saturating_sub(SHOWN_KINDS);
    let mut text = labels[..labels.len() - hidden].join(", ");
    if hidden > 0 {
        text.push_str(&format!(" +{hidden}"));
    }
    Some(text)
}

pub(super) fn header(
    tool_name: &str,
    input: Option<&Value>,
    result: Option<&str>,
    agents: &[TaskRow],
) -> Option<ToolHeader> {
    let str_arg = |key: &str| input.and_then(|input| input.get(key)?.as_str());
    let agent = |key: &str| str_arg(key).map(|run| agent_name(agents, run));
    let new = |icon, verb: &str| ToolHeader {
        icon,
        verb: verb.to_owned(),
        subject: String::new(),
        location: None,
        meta: None,
    };
    let mut header = match tool_name {
        "delegate" => {
            let mut header = new(icons::USERS_THREE, "Delegate");
            header.subject = str_arg("name")
                .map(one_line)
                .or_else(|| str_arg("prompt").map(preview))
                .unwrap_or_default();
            header.location = input
                .and_then(|input| input.pointer("/target/role")?.as_str())
                .map(|role| format!("({role})"));
            header.meta = input
                .and_then(|input| input.get("background")?.as_bool())
                .filter(|background| *background)
                .map(|_| "background".to_owned());
            header
        }
        "send_message" | "send" => {
            let verb = match str_arg("kind") {
                Some("reply") => "Reply →",
                Some("steering") => "Steer →",
                _ => "Message →",
            };
            let mut header = new(icons::CHAT_CIRCLE_TEXT, verb);
            header.subject = agent("run_id").unwrap_or_default();
            header.meta = str_arg("message").map(preview);
            header
        }
        "todo_write" => {
            let count = input
                .and_then(|input| input.get("items")?.as_array())
                .map(Vec::len);
            let mut header = new(icons::LIST_CHECKS, "Update todos");
            let update = input.and_then(|input| input.get("update"));
            header.meta = if input
                .and_then(|input| input.get("advance")?.as_bool())
                .is_some_and(|advance| advance)
            {
                Some("advance".to_owned())
            } else if let Some(update) = update {
                update
                    .get("index")
                    .and_then(Value::as_u64)
                    .zip(update.get("status").and_then(Value::as_str))
                    .map(|(index, status)| format!("[{index}] → {status}"))
            } else {
                count.map(|count| match count {
                    0 => "cleared".to_owned(),
                    1 => "1 item".to_owned(),
                    count => format!("{count} items"),
                })
            };
            header
        }
        "ask_user" => {
            let mut header = new(icons::QUESTION, "Ask");
            header.subject = str_arg("title").map(one_line).unwrap_or_default();
            header.meta = input
                .and_then(|input| input.get("blocking")?.as_bool())
                .filter(|blocking| !blocking)
                .map(|_| "non-blocking".to_owned());
            header
        }
        "answer_subagent_question" => {
            let mut header = new(icons::CHAT_TEXT, "Answer subagent");
            header.subject = str_arg("answer").map(preview).unwrap_or_default();
            header
        }
        "subagent_questions" => {
            let mut header = new(icons::QUESTION, "Read questions from");
            header.subject = agent("run_id").unwrap_or_default();
            header
        }
        "user_answers" => new(icons::CHAT_TEXT, "Read answers"),
        "wait" => {
            let mut header = new(icons::HOURGLASS, "Wait for");
            let runs: Vec<String> = match input.and_then(|input| input.get("run_ids")?.as_array()) {
                Some(runs) => runs
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|run| agent_name(agents, run))
                    .collect(),
                None => agent("run_id").into_iter().collect(),
            };
            // Without targets `wait` takes any event addressed to this run. While the
            // input is still streaming (`None`) the verb alone is shown.
            let untargeted = input.is_some_and(|input| {
                input.get("run_id").is_none() && input.get("run_ids").is_none()
            });
            header.subject = if untargeted {
                "events".to_owned()
            } else {
                runs.join(", ")
            };
            let mode = (runs.len() > 1).then(|| match str_arg("mode") {
                Some("all") => "all",
                _ => "any",
            });
            header.meta = match (mode, result.and_then(wait_reason)) {
                (Some(mode), Some(reason)) => Some(format!("{mode} · {reason}")),
                (Some(mode), None) => Some(mode.to_owned()),
                (None, reason) => reason,
            };
            header
        }
        // Retired tool; kept so transcripts recorded before `wait` replaced it still read well.
        "wait_reply" => new(icons::HOURGLASS, "Wait for reply"),
        "inbox" => new(icons::TRAY, "Read inbox"),
        "run_output" => {
            let mut header = new(icons::FILE_TEXT, "Read output of");
            header.subject = agent("run_id").unwrap_or_default();
            header
        }
        "cancel" => {
            let mut header = new(icons::STOP_CIRCLE, "Cancel");
            header.subject = agent("run_id").unwrap_or_default();
            header
        }
        "list_agents" => new(icons::USERS, "List agents"),
        "inspect_agent" => {
            let mut header = new(icons::USERS, "Inspect");
            header.subject = agent("run_id").unwrap_or_default();
            header
        }
        "finish" => {
            let mut header = new(icons::FLAG_CHECKERED, "Finish");
            header.subject = str_arg("result").map(preview).unwrap_or_default();
            header
        }
        "handoff_to_lane" => {
            let mut header = new(icons::ARROW_FAT_LINE_UP, "Hand off to lane");
            header.subject = str_arg("escalation_reason")
                .map(preview)
                .unwrap_or_default();
            header
        }
        "compact" => new(icons::ARROWS_IN, "Compact context"),
        "working_note_write" => {
            let mut header = new(icons::NOTEBOOK, "Note");
            header.subject = str_arg("body").map(preview).unwrap_or_default();
            header
        }
        "working_note_read" => new(icons::NOTEBOOK, "Read ledger"),
        "get_goal" => new(icons::TARGET, "Read goal"),
        "submit_goal_check" => new(icons::CHECK_SQUARE, "Submit goal check"),
        "submit_goal_review" => new(icons::SEAL_CHECK, "Submit goal review"),
        "task_claim" | "task_complete" => {
            let (icon, verb) = if tool_name == "task_claim" {
                (icons::HAND_GRABBING, "Claim task")
            } else {
                (icons::CHECK_SQUARE, "Complete task")
            };
            let mut header = new(icon, verb);
            header.subject = str_arg("task_id").map(one_line).unwrap_or_default();
            header
        }
        "finding_append" => {
            let mut header = new(icons::LIGHTBULB, "Record finding");
            header.subject = str_arg("content").map(preview).unwrap_or_default();
            header.meta = str_arg("task_id").map(one_line);
            header
        }
        "thread_search" => {
            // `query` is one string or alternatives; without it the newest threads are listed.
            let queries: Vec<&str> = match input.and_then(|input| input.get("query")) {
                Some(Value::String(query)) => vec![query],
                Some(Value::Array(queries)) => queries.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            let mut header = new(icons::LIST_MAGNIFYING_GLASS, "Search threads");
            header.subject = one_line(
                &queries
                    .iter()
                    .map(|query| format!("\"{query}\""))
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
            header
        }
        "thread_grep" => {
            let mut header = new(icons::FILE_MAGNIFYING_GLASS, "Grep thread");
            header.subject = str_arg("pattern")
                .map(|pattern| one_line(&format!("\"{pattern}\"")))
                .unwrap_or_default();
            header.location = agent("thread").map(|thread| format!("in {thread}"));
            header
        }
        "thread_read" => {
            let mut header = new(icons::BOOK_OPEN_TEXT, "Read thread");
            header.subject = agent("thread").unwrap_or_default();
            header.meta = str_arg("view").map(one_line);
            header
        }
        _ => return None,
    };
    header.subject = header.subject.trim().to_owned();
    Some(header)
}

/// The agent's display name, or a shortened run ID for unknown runs.
pub fn agent_name(agents: &[TaskRow], run_id: &str) -> String {
    agents
        .iter()
        .find(|row| row.run_id.to_string() == run_id)
        .map_or_else(|| crate::format::short_id(run_id), |row| row.name.clone())
}

fn preview(text: &str) -> String {
    let line = one_line(text);
    let mut chars = line.chars();
    let mut bounded: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(run: u64, name: &str, role: &str) -> TaskRow {
        TaskRow {
            run_id: runtime::RunId::new(run),
            name: name.into(),
            role: role.into(),
            status: event_bus::AgentRunPhase::Running,
            model: String::new(),
            category: None,
            cancelled: false,
        }
    }

    fn text(tool: &str, input: Value, agents: &[TaskRow]) -> Option<String> {
        header(tool, Some(&input), None, agents).map(|header| header.text())
    }

    fn wait_text(input: Value, result: &str, agents: &[TaskRow]) -> String {
        header("wait", Some(&input), Some(result), agents)
            .unwrap()
            .text()
    }

    #[test]
    fn wait_without_targets_names_the_events_it_listens_to() {
        assert_eq!(
            text("wait", json!({}), &[]).as_deref(),
            Some("Wait for events")
        );
        // The input is still streaming: only the verb is known.
        assert_eq!(
            header("wait", None, None, &[])
                .map(|header| header.text())
                .as_deref(),
            Some("Wait for")
        );
        assert_eq!(
            text(
                "wait",
                json!({"run_id": "run-3"}),
                &[row(3, "reviewer-r1", "reviewer")]
            )
            .as_deref(),
            Some("Wait for reviewer-r1")
        );
    }

    #[test]
    fn wait_result_shows_why_it_returned() {
        let agents = [row(3, "reviewer-r1", "reviewer")];
        for (input, result, expected) in [
            (
                json!({}),
                json!({"status": "ready", "events": [
                    {"kind": "message", "message_id": "msg-1"},
                    {"kind": "message", "message_id": "msg-2"},
                    {"kind": "shell", "job_id": "job-1"}]}),
                "Wait for events · message, shell job",
            ),
            (
                json!({}),
                json!({"status": "ready", "events": [
                    {"kind": "parent_terminated"}, {"kind": "lane"},
                    {"kind": "question"}, {"kind": "answer_ready"}]}),
                "Wait for events · parent ended, lane, question +1",
            ),
            (
                json!({}),
                json!({"status": "no_wait_sources", "events": [], "reasons": ["no_live_parent"]}),
                "Wait for events · nothing to wait for",
            ),
            (
                json!({"run_id": "run-3"}),
                json!({"status": "ready", "events": [{"kind": "run_completed"}]}),
                "Wait for reviewer-r1 · run finished",
            ),
            (
                json!({"run_ids": ["run-3", "run-9"], "mode": "all"}),
                json!({"status": "ready", "events": [{"kind": "message"}]}),
                "Wait for reviewer-r1, run-9 · all · message",
            ),
        ] {
            assert_eq!(wait_text(input, &result.to_string(), &agents), expected);
        }
    }

    #[test]
    fn wait_results_without_a_reason_keep_the_plain_header() {
        for result in [
            r#"{"run_id":"run-3","phase":"Done"}"#,
            r#"{"status":"ready","events":[]}"#,
            "not json",
            "wait failed",
        ] {
            assert_eq!(wait_text(json!({}), result, &[]), "Wait for events");
        }
    }

    #[test]
    fn retired_wait_reply_calls_in_old_transcripts_keep_their_label() {
        assert_eq!(
            text("wait_reply", json!({"message_id": "msg-35"}), &[]).as_deref(),
            Some("Wait for reply")
        );
    }

    #[test]
    fn delegation_and_messages_name_the_agents_involved() {
        let agents = [row(3, "reviewer-r1", "reviewer")];
        for (tool, input, expected) in [
            (
                "delegate",
                json!({"target": {"role": "worker"}, "background": true,
                    "prompt": "DEMO-W1", "name": "worker-w1"}),
                "Delegate worker-w1 (worker) · background",
            ),
            (
                "delegate",
                json!({"target": {"role": "explorer"}, "prompt": "Find the\nentry point"}),
                "Delegate Find the entry point (explorer)",
            ),
            (
                "send_message",
                json!({"run_id": "run-3", "message": "review\nrun-2"}),
                "Message → reviewer-r1 · review run-2",
            ),
            (
                "send",
                json!({"run_id": "run-9", "message": "ok", "kind": "reply"}),
                "Reply → run-9 · ok",
            ),
            (
                "wait",
                json!({"run_ids": ["run-3", "run-9"], "mode": "all"}),
                "Wait for reviewer-r1, run-9 · all",
            ),
            (
                "todo_write",
                json!({"items": [{"content": "a", "status": "pending"},
                    {"content": "b", "status": "completed"}]}),
                "Update todos · 2 items",
            ),
            (
                "todo_write",
                json!({"advance": true, "expected_revision": 3}),
                "Update todos · advance",
            ),
            (
                "todo_write",
                json!({"update": {"index": 2, "status": "completed"}, "expected_revision": 3}),
                "Update todos · [2] → completed",
            ),
            (
                "todo_write",
                json!({"update": {"index": "x"}, "expected_revision": 3}),
                "Update todos",
            ),
            (
                "task_claim",
                json!({"task_id": "task-1"}),
                "Claim task task-1",
            ),
        ] {
            assert_eq!(text(tool, input, &agents).as_deref(), Some(expected));
        }
    }

    #[test]
    fn thread_reads_name_the_query_and_thread() {
        let agents = [row(3, "reviewer-r1", "reviewer")];
        for (tool, input, expected) in [
            (
                "thread_search",
                json!({"query": ["login", "auth flow"]}),
                "Search threads \"login\" | \"auth flow\"",
            ),
            ("thread_search", json!({"limit": 5}), "Search threads"),
            (
                "thread_grep",
                json!({"thread": "thread-2", "pattern": "panic"}),
                "Grep thread \"panic\" in thread-2",
            ),
            (
                "thread_read",
                json!({"thread": "run-3", "view": "messages"}),
                "Read thread reviewer-r1 · messages",
            ),
        ] {
            assert_eq!(text(tool, input, &agents).as_deref(), Some(expected));
        }
    }

    #[test]
    fn unexpected_input_keeps_the_verb() {
        for (tool, verb) in [
            ("delegate", "Delegate"),
            ("send_message", "Message →"),
            ("todo_write", "Update todos"),
            ("wait", "Wait for"),
        ] {
            assert_eq!(
                text(tool, json!({"run_id": 3, "items": "x"}), &[]).as_deref(),
                Some(verb)
            );
            assert_eq!(
                header(tool, None, None, &[])
                    .map(|header| header.text())
                    .as_deref(),
                Some(verb)
            );
        }
        assert_eq!(header("custom_tool", None, None, &[]), None);
    }
}
