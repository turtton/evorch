//! Headers for runtime-owned coordination tools: delegation, agent messages,
//! waits, questions, todos and team tasks.

use serde_json::Value;

use super::header::{ToolHeader, one_line};
use crate::model::tasks::TaskRow;
use crate::theme::icons;

const PREVIEW_CHARS: usize = 60;

pub(super) fn header(
    tool_name: &str,
    input: Option<&Value>,
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
            header.meta = count.map(|count| match count {
                0 => "cleared".to_owned(),
                1 => "1 item".to_owned(),
                count => format!("{count} items"),
            });
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
            header.subject = runs.join(", ");
            if runs.len() > 1 {
                header.meta = Some(match str_arg("mode") {
                    Some("all") => "all".to_owned(),
                    _ => "any".to_owned(),
                });
            }
            header
        }
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
        "escalate" => {
            let mut header = new(icons::ARROW_FAT_LINE_UP, "Escalate");
            header.subject = str_arg("escalation_reason")
                .map(preview)
                .unwrap_or_default();
            header
        }
        "compact" => new(icons::ARROWS_IN, "Compact context"),
        "ledger_append" => {
            let mut header = new(icons::NOTEBOOK, "Note");
            header.subject = str_arg("body").map(preview).unwrap_or_default();
            header
        }
        "ledger_read" => new(icons::NOTEBOOK, "Read ledger"),
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
        header(tool, Some(&input), agents).map(|header| header.text())
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
                "task_claim",
                json!({"task_id": "task-1"}),
                "Claim task task-1",
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
                header(tool, None, &[])
                    .map(|header| header.text())
                    .as_deref(),
                Some(verb)
            );
        }
        assert_eq!(header("custom_tool", None, &[]), None);
    }
}
