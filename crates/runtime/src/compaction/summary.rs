//! 圧縮サマリ生成（T6 で実装）。

use std::{collections::HashSet, sync::Arc};

use agents::Role;
use async_trait::async_trait;
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole, ToolResultContent};
use serde_json::Value;

use crate::error::RuntimeError;
use crate::model::{AgentInvocationContext, AgentModel};

const AGENT_MESSAGE_PREFIX: &str = "agent-message";
const SUMMARY_SYSTEM_PROMPT: &str = "Summarize the compacted conversation for continuation. Preserve the goal and contract, unfinished tasks, key decisions, changed files, verification results, unresolved items, recent context, and every agent message. Use terse markdown bullets and do not invoke tools.";
const SUMMARY_USER_PROMPT: &str = "Produce the continuation summary now. Preserve exact file paths, test result lines, unresolved markers, and agent-message identifiers.";

pub(crate) struct SummaryInput<'a> {
    pub goal: Option<&'a str>,
    pub compacted: &'a [Message],
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SummarizeError {
    #[error("summary model failed: {0}")]
    Model(#[from] RuntimeError),
    #[error("summary model returned abnormal finish reason: {reason}")]
    AbnormalFinish { reason: String },
    #[error("summary model returned empty summary")]
    EmptySummary,
}

#[async_trait]
pub(crate) trait Summarizer: Send + Sync {
    async fn summarize(&self, input: &SummaryInput<'_>) -> Result<String, SummarizeError>;
}

pub(crate) struct StructuralSummarizer;

pub(crate) struct ModelSummarizer {
    pub(crate) model: Arc<dyn AgentModel>,
    pub(crate) role: Role,
    pub(crate) run_id: String,
}

#[async_trait]
impl Summarizer for StructuralSummarizer {
    async fn summarize(&self, input: &SummaryInput<'_>) -> Result<String, SummarizeError> {
        let goal = input.goal.or_else(|| first_user_text(input.compacted));
        let decisions = assistant_lines(input.compacted, |line| {
            line.contains("Decision:") || line.contains("決定:")
        });
        let unresolved = assistant_lines(input.compacted, |line| {
            line.to_ascii_lowercase().contains("unresolved")
        });
        let unfinished = assistant_lines(input.compacted, is_unfinished_task_line)
            .into_iter()
            .take(8)
            .collect();
        let files = changed_files(input.compacted);
        let verification = verification_lines(input.compacted);
        let recent = recent_context(input.compacted);
        let agent_messages = agent_messages(input.compacted);

        let sections = [
            ("Goal / Contract", goal.into_iter().collect()),
            ("Unfinished Tasks", unfinished),
            ("Key Decisions", decisions),
            ("Changed Files", files.iter().map(String::as_str).collect()),
            ("Verification Results", verification),
            ("Unresolved Items", unresolved),
            (
                "Recent Context",
                recent.iter().map(String::as_str).collect(),
            ),
            ("Agent Messages", agent_messages),
        ];
        let mut summary = String::new();
        for (header, items) in sections {
            section(&mut summary, header, items.into_iter());
        }
        Ok(summary)
    }
}

#[async_trait]
impl Summarizer for ModelSummarizer {
    async fn summarize(&self, input: &SummaryInput<'_>) -> Result<String, SummarizeError> {
        let mut messages = Vec::with_capacity(input.compacted.len().saturating_add(2));
        messages.push(Message {
            role: MessageRole::System,
            content: vec![ContentBlock::Text {
                text: SUMMARY_SYSTEM_PROMPT.to_string(),
            }],
        });
        messages.extend_from_slice(input.compacted);
        // cut の protected floor が先頭 User を compacted slice から外すため、
        // goal は最終 User プロンプトへ明示同梱しないと summary モデルに届かない。
        let user_prompt = match input.goal {
            Some(goal) => format!(
                "{SUMMARY_USER_PROMPT}\n\nOriginal session goal (preserve verbatim in the summary):\n{goal}"
            ),
            None => SUMMARY_USER_PROMPT.to_string(),
        };
        messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text: user_prompt }],
        });
        let invocation = AgentInvocationContext {
            run_id: self.run_id.clone(),
        };
        let response = self
            .model
            .complete(&invocation, self.role, &messages, &[])
            .await?;
        let finish_reason = match response.finish_reason {
            FinishReason::Stop => None,
            FinishReason::Length => Some("length".to_string()),
            FinishReason::ToolUse => Some("tool_use".to_string()),
            FinishReason::ContentFilter => Some("content_filter".to_string()),
            FinishReason::Other(reason) => Some(format!("other: {reason}")),
        };
        if let Some(reason) = finish_reason {
            return Err(SummarizeError::AbnormalFinish { reason });
        }
        let summary: String = response
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
            .collect();
        if summary.trim().is_empty() {
            return Err(SummarizeError::EmptySummary);
        }
        Ok(summary)
    }
}

pub(crate) fn enforce_max_bytes(summary: &str, max_summary_bytes: u64) -> String {
    const MARKER: &str = "\n[truncated]";

    let Ok(limit) = usize::try_from(max_summary_bytes) else {
        return summary.to_string();
    };
    if summary.len() <= limit {
        return summary.to_string();
    }
    if limit < MARKER.len() {
        return MARKER[..limit].to_string();
    }
    let content_limit = limit - MARKER.len();
    let boundary = (0..=content_limit)
        .rev()
        .find(|index| summary.is_char_boundary(*index))
        .unwrap_or(0);
    format!("{}{MARKER}", &summary[..boundary])
}

fn first_user_text(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
}

fn assistant_lines(messages: &[Message], predicate: impl Fn(&str) -> bool) -> Vec<&str> {
    messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .flat_map(str::lines)
        .filter(|line| predicate(line))
        .collect()
}

fn is_unfinished_task_line(line: &str) -> bool {
    let normalized = line.to_ascii_lowercase();
    normalized.contains("unresolved")
        || normalized.contains("todo")
        || line.contains("次:")
        || line.contains("次は")
        || line.contains("残課題")
}

fn changed_files(messages: &[Message]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for block in messages.iter().flat_map(|message| &message.content) {
        let ContentBlock::ToolUse { name, input, .. } = block else {
            continue;
        };
        match name.as_str() {
            "read" | "edit" | "write" => collect_path_values(input, &mut seen, &mut files),
            "shell" => {
                if let Some(command) = input.get("command").and_then(Value::as_str) {
                    for token in command.split_whitespace() {
                        let candidate = token.trim_matches(|character: char| {
                            matches!(character, '`' | '\'' | '"' | ',' | ';')
                        });
                        if candidate.contains('/')
                            && candidate.contains('.')
                            && seen.insert(candidate.to_string())
                        {
                            files.push(candidate.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    files
}

fn collect_path_values(value: &Value, seen: &mut HashSet<String>, files: &mut Vec<String>) {
    for key in ["path", "filepath", "file"] {
        if let Some(path) = value.get(key).and_then(Value::as_str)
            && seen.insert(path.to_string())
        {
            files.push(path.to_string());
        }
    }
}

fn verification_lines(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. } => None,
        })
        .flatten()
        .flat_map(|content| match content {
            ToolResultContent::Text { text } => text.lines(),
        })
        .filter(|line| line.contains("test result:") || line.contains("passed;"))
        .collect()
}

fn recent_context(messages: &[Message]) -> Vec<String> {
    let snippets: Vec<&str> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect();
    snippets
        .into_iter()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|text| text.chars().take(200).collect())
        .collect()
}

fn agent_messages(messages: &[Message]) -> Vec<&str> {
    let prefix = format!("[{AGENT_MESSAGE_PREFIX} ");
    messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.starts_with(&prefix) => Some(text.as_str()),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect()
}

fn section<'a>(output: &mut String, header: &str, items: impl Iterator<Item = &'a str>) {
    output.push_str("## ");
    output.push_str(header);
    output.push('\n');
    let mut items = items.peekable();
    if items.peek().is_none() {
        output.push_str("- none detected\n");
    }
    for item in items {
        output.push_str("- ");
        output.push_str(item);
        output.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agents::Role;
    use async_trait::async_trait;
    use providers::{
        ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
    };

    use super::{
        ModelSummarizer, StructuralSummarizer, Summarizer, SummaryInput, enforce_max_bytes,
    };
    use crate::compaction::cut::select_cut;
    use crate::compaction::estimator::estimate_tokens;
    use crate::error::RuntimeError;
    use crate::model::{AgentInvocationContext, AgentModel};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Observation {
        run_id: String,
        role: Role,
        tool_count: usize,
        last_user_text: String,
    }

    struct StubModel {
        response: Result<ChatResponse, RuntimeError>,
        observed: Mutex<Vec<Observation>>,
    }

    #[async_trait]
    impl AgentModel for StubModel {
        async fn complete(
            &self,
            invocation: &AgentInvocationContext,
            role: Role,
            messages: &[Message],
            tools: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            let last_user_text = messages
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::User)
                .map(|message| {
                    message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            self.observed
                .lock()
                .expect("observation lock must not poison")
                .push(Observation {
                    run_id: invocation.run_id.clone(),
                    role,
                    tool_count: tools.len(),
                    last_user_text,
                });
            self.response.clone()
        }

        fn selected_model(&self, _role: Role) -> String {
            "stub-summary-model".to_string()
        }
    }

    fn fixture() -> Vec<Message> {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/compaction_long_session.json"
        ))
        .expect("long-session fixture must parse")
    }

    fn fixture_cut(messages: &[Message]) -> crate::compaction::cut::CutPlan {
        let keep_recent_tokens = estimate_tokens(&messages[22..]);
        select_cut(messages, keep_recent_tokens, 1).expect("fixture has a compactable prefix")
    }

    fn initial_goal(messages: &[Message]) -> &str {
        messages[1]
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
            .expect("fixture has an initial user prompt")
    }

    // Given: a safe cut through the long-session fixture
    // When: the compacted prefix is structurally summarized
    // Then: every AC3 fidelity class remains reachable in the summary
    #[tokio::test]
    async fn structural_summary_preserves_fixture_fidelity() {
        let messages = fixture();
        let plan = fixture_cut(&messages);
        let compacted = &messages[plan.start..plan.end];

        let summary = StructuralSummarizer
            .summarize(&SummaryInput {
                goal: Some(initial_goal(&messages)),
                compacted,
            })
            .await
            .expect("structural summary is infallible");

        assert!(summary.contains("Fix the flaky compaction retry"));
        for decision in compacted
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| {
                let ContentBlock::Text { text } = block else {
                    return None;
                };
                text.lines().find(|line| line.contains("Decision:"))
            })
        {
            assert!(summary.contains(decision));
        }
        for path in ["src/scheduler.rs", "src/backoff.rs", "tests/retry.rs"] {
            assert!(summary.contains(path), "missing changed file {path}");
        }
        assert!(summary.contains("test result: ok. 12 passed; 0 failed"));
        assert!(summary.contains(
            "Still unresolved: why the second retry occasionally skips the backoff under concurrent polls."
        ));

        let agent_ids: Vec<&str> = compacted
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } if text.starts_with("[agent-message ") => text
                    .split_whitespace()
                    .find(|token| token.starts_with("id=")),
                ContentBlock::Text { .. }
                | ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
            .collect();
        assert!(!agent_ids.is_empty());
        for id in agent_ids {
            assert!(summary.contains(id));
        }

        let kept = &messages[plan.end..];
        for tool_call_id in kept
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id),
                ContentBlock::Text { .. }
                | ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. } => None,
            })
        {
            assert!(
                kept.iter()
                    .flat_map(|message| &message.content)
                    .any(|block| {
                        matches!(block, ContentBlock::ToolUse { id, .. } if id == tool_call_id)
                    })
            );
        }
    }

    // Given: identical fixture input
    // When: structural summarization runs twice
    // Then: both outputs are byte-identical
    #[tokio::test]
    async fn structural_summary_is_deterministic() {
        let messages = fixture();
        let plan = fixture_cut(&messages);
        let input = SummaryInput {
            goal: Some(initial_goal(&messages)),
            compacted: &messages[plan.start..plan.end],
        };

        let first = StructuralSummarizer.summarize(&input).await.unwrap();
        let second = StructuralSummarizer.summarize(&input).await.unwrap();

        assert_eq!(first, second);
    }

    // Given: more than eight assistant lines carrying explicit pending markers
    // When: structural summarization renders unfinished tasks
    // Then: the oldest eight concrete items are listed and later items are omitted
    #[tokio::test]
    async fn structural_summary_lists_at_most_eight_unfinished_tasks_oldest_first() {
        let messages = (0..10)
            .map(|index| Message {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("TODO task-{index}"),
                }],
            })
            .collect::<Vec<_>>();

        let summary = StructuralSummarizer
            .summarize(&SummaryInput {
                goal: None,
                compacted: &messages,
            })
            .await
            .expect("structural summary is infallible");

        let unfinished = summary
            .split_once("## Unfinished Tasks\n")
            .and_then(|(_, rest)| rest.split_once("## Key Decisions\n"))
            .map(|(section, _)| section)
            .expect("summary contains the unfinished-task section");
        for index in 0..8 {
            assert!(unfinished.contains(&format!("- TODO task-{index}")));
        }
        assert!(!unfinished.contains("- TODO task-8"));
        assert!(!unfinished.contains("- TODO task-9"));
    }

    // Given: a successful model stub and a compacted message
    // When: model summarization runs
    // Then: text is returned verbatim and correlation fields cross an empty-tools boundary
    #[tokio::test]
    async fn model_summary_forwards_context_without_tools() {
        let model = Arc::new(StubModel {
            response: Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "first".to_string(),
                        },
                        ContentBlock::Reasoning {
                            text: "hidden".to_string(),
                        },
                        ContentBlock::Text {
                            text: " second".to_string(),
                        },
                    ],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            }),
            observed: Mutex::new(Vec::new()),
        });
        let summarizer = ModelSummarizer {
            model: model.clone(),
            role: Role::Worker,
            run_id: "run-summary-7".to_string(),
        };
        let messages = fixture();

        let summary = summarizer
            .summarize(&SummaryInput {
                goal: None,
                compacted: &messages[1..3],
            })
            .await
            .expect("stub response succeeds");

        assert_eq!(summary, "first second");
        assert_eq!(
            *model
                .observed
                .lock()
                .expect("observation lock must not poison"),
            vec![Observation {
                run_id: "run-summary-7".to_string(),
                role: Role::Worker,
                tool_count: 0,
                last_user_text: "Produce the continuation summary now. Preserve exact file paths, test result lines, unresolved markers, and agent-message identifiers.".to_string(),
            }]
        );
    }

    // Given: a goal that B4a cut-floor protection removed from the compacted slice
    // When: the model summarizer builds its request
    // Then: the final user prompt carries the goal verbatim so the model can preserve it
    #[tokio::test]
    async fn model_summary_request_contains_goal() {
        let model = Arc::new(StubModel {
            response: Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "summary".to_string(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            }),
            observed: Mutex::new(Vec::new()),
        });
        let summarizer = ModelSummarizer {
            model: model.clone(),
            role: Role::Worker,
            run_id: "run-summary-goal".to_string(),
        };
        let messages = fixture();

        summarizer
            .summarize(&SummaryInput {
                goal: Some("Fix the flaky compaction retry"),
                compacted: &messages[2..4],
            })
            .await
            .expect("stub response succeeds");

        let observed = model.observed.lock().expect("observation lock");
        let request = observed.last().expect("one summarizer call");
        assert!(
            request.last_user_text.contains("Produce the continuation summary now."),
            "instruction prompt must remain"
        );
        assert!(
            request
                .last_user_text
                .contains("Fix the flaky compaction retry"),
            "goal must reach the summary model"
        );
    }

    // Given: a model stub returning RuntimeError
    // When: model summarization runs
    // Then: the failure crosses the typed summarizer boundary
    #[tokio::test]
    async fn model_summary_propagates_model_error() {
        let model = Arc::new(StubModel {
            response: Err(RuntimeError::Model {
                reason: "summary unavailable".to_string(),
            }),
            observed: Mutex::new(Vec::new()),
        });
        let summarizer = ModelSummarizer {
            model,
            role: Role::Orchestrator,
            run_id: "run-summary-8".to_string(),
        };

        let error = summarizer
            .summarize(&SummaryInput {
                goal: None,
                compacted: &[],
            })
            .await
            .expect_err("model failure must propagate");

        assert!(error.to_string().contains("summary unavailable"));
    }

    // Given: a model response stopped by the token limit
    // When: model summarization validates the completion
    // Then: the partial response is rejected with a deterministic reason
    #[tokio::test]
    async fn model_summary_rejects_length_finish() {
        let model = Arc::new(StubModel {
            response: Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "partial summary".to_string(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Length,
            }),
            observed: Mutex::new(Vec::new()),
        });
        let summarizer = ModelSummarizer {
            model,
            role: Role::Worker,
            run_id: "run-summary-length".to_string(),
        };

        let error = summarizer
            .summarize(&SummaryInput {
                goal: None,
                compacted: &[],
            })
            .await
            .expect_err("length-limited summary must fail");

        assert_eq!(
            error.to_string(),
            "summary model returned abnormal finish reason: length"
        );
    }

    // Given: a model response stopped by content filtering
    // When: model summarization validates the completion
    // Then: the blocked response is rejected with a deterministic reason
    #[tokio::test]
    async fn model_summary_rejects_content_filter_finish() {
        let model = Arc::new(StubModel {
            response: Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "blocked summary".to_string(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::ContentFilter,
            }),
            observed: Mutex::new(Vec::new()),
        });
        let summarizer = ModelSummarizer {
            model,
            role: Role::Worker,
            run_id: "run-summary-filter".to_string(),
        };

        let error = summarizer
            .summarize(&SummaryInput {
                goal: None,
                compacted: &[],
            })
            .await
            .expect_err("content-filtered summary must fail");

        assert_eq!(
            error.to_string(),
            "summary model returned abnormal finish reason: content_filter"
        );
    }

    // Given: a stop response containing reasoning and tool use but no text
    // When: model summarization concatenates visible text
    // Then: the empty summary is rejected
    #[tokio::test]
    async fn model_summary_rejects_empty_text_output() {
        let model = Arc::new(StubModel {
            response: Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: " \n\t".to_string(),
                        },
                        ContentBlock::Reasoning {
                            text: "hidden".to_string(),
                        },
                        ContentBlock::ToolUse {
                            id: "unexpected-tool".to_string(),
                            name: "read".to_string(),
                            input: serde_json::json!({ "path": "src/lib.rs" }),
                        },
                    ],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            }),
            observed: Mutex::new(Vec::new()),
        });
        let summarizer = ModelSummarizer {
            model,
            role: Role::Worker,
            run_id: "run-summary-empty".to_string(),
        };

        let error = summarizer
            .summarize(&SummaryInput {
                goal: None,
                compacted: &[],
            })
            .await
            .expect_err("text-free summary must fail");

        assert_eq!(error.to_string(), "summary model returned empty summary");
    }

    // Given: non-terminal finish reasons not accepted by the summary boundary
    // When: model summarization validates each response
    // Then: tool-use and provider-specific stops fail closed with named reasons
    #[tokio::test]
    async fn model_summary_rejects_tool_use_and_other_finishes() {
        let cases = [
            (FinishReason::ToolUse, "tool_use"),
            (
                FinishReason::Other("provider_pause".to_string()),
                "other: provider_pause",
            ),
        ];

        for (finish_reason, expected_reason) in cases {
            let model = Arc::new(StubModel {
                response: Ok(ChatResponse {
                    message: Message {
                        role: MessageRole::Assistant,
                        content: vec![ContentBlock::Text {
                            text: "not terminal".to_string(),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason,
                }),
                observed: Mutex::new(Vec::new()),
            });
            let summarizer = ModelSummarizer {
                model,
                role: Role::Worker,
                run_id: "run-summary-abnormal".to_string(),
            };

            let error = summarizer
                .summarize(&SummaryInput {
                    goal: None,
                    compacted: &[],
                })
                .await
                .expect_err("non-stop summary must fail");

            assert_eq!(
                error.to_string(),
                format!("summary model returned abnormal finish reason: {expected_reason}")
            );
        }
    }

    // Given: summaries around a byte boundary
    // When: the byte limit is enforced
    // Then: exact/under limits stay intact and over-limit UTF-8 gets a marker
    #[test]
    fn max_bytes_preserves_boundaries_and_marks_truncation() {
        assert_eq!(enforce_max_bytes("abcd", 4), "abcd");
        assert_eq!(enforce_max_bytes("abc", 4), "abc");
        let truncated = enforce_max_bytes("éclair日本語", 15);
        assert!(truncated.ends_with("\n[truncated]"));
        assert!(truncated.len() <= 15);
        assert!(truncated.is_char_boundary(truncated.len()));
    }
}
