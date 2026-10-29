//! The model that plays the original user during a replay (ADR 0036 §5).

use std::sync::Arc;
use std::time::Duration;

use providers::{ContentBlock, JsonSchema, Message};
use runtime::{AgentInvocationContext, AgentModel, Role};
use serde::{Deserialize, Serialize};

use crate::conversation::Entry;
use crate::source::UserTurn;
use crate::structured::{self, ATTEMPTS, Unusable};

const TIMEOUT: Duration = Duration::from_secs(300);

const INSTRUCTION: &str = "You play the user of a coding agent while one of the user's past conversations is replayed to evaluate the agent. You receive the original user's messages from that conversation in order, and the replay's conversation so far as the user saw it. Choose the user's next step:
- \"answer\": answer `pending_question` as the original user's messages indicate.
- \"reply\": send the message the original user would send now.
- \"finish\": end the conversation, because the original requests are met or cannot be met in this replay.
Convey only the original user's intent and constraints. Never give solutions, code, commands, file names or diagnoses that the original messages do not contain; the agent must find those itself. Do not repeat requests the agent already completed, and keep the original wording and language where it fits. When the original messages do not settle a question, tell the agent to decide or restate the relevant original constraint. `text` is sent to the agent verbatim and must be empty for \"finish\". `reason` is only recorded for the evaluation.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Reply,
    Answer,
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub action: Action,
    pub text: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingQuestion {
    pub id: String,
    pub title: String,
    pub options: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SimulatorError {
    #[error("the user simulator did not answer within {0:?}")]
    TimedOut(Duration),
    #[error("the user simulator request failed: {0}")]
    Model(String),
    #[error("the user simulator returned an unusable decision: {0}")]
    Invalid(String),
}

pub struct UserSimulator {
    model: Arc<dyn AgentModel>,
    run_id: String,
}

impl UserSimulator {
    pub fn new(model: Arc<dyn AgentModel>, thread_id: &str) -> Self {
        Self {
            model,
            run_id: format!("user-simulator:{thread_id}"),
        }
    }

    /// Ask for the user's next step, retrying unusable output a bounded number of times.
    ///
    /// # Errors
    /// Fails after the last attempt times out, errors or returns an invalid decision.
    pub async fn decide(
        &self,
        original: &[UserTurn],
        replay: &[Entry],
        pending: Option<&PendingQuestion>,
    ) -> Result<Decision, SimulatorError> {
        let payload = serde_json::json!({
            "original_user_messages": original,
            "replay_conversation": replay,
            "pending_question": pending,
        });
        let mut messages = vec![
            Message {
                role: providers::Role::System,
                content: vec![ContentBlock::Text {
                    text: INSTRUCTION.into(),
                }],
            },
            Message {
                role: providers::Role::User,
                content: vec![ContentBlock::Text {
                    text: payload.to_string(),
                }],
            },
        ];
        let invocation = AgentInvocationContext {
            attempts: None,
            run_id: self.run_id.clone(),
            model_preference: None,
            category: None,
            purpose: event_bus::RequestPurpose::UserSimulation,
            turn: None,
        };
        let mut last = SimulatorError::Invalid("no attempt was made".into());
        for attempt in 1..=ATTEMPTS {
            let response = tokio::time::timeout(
                TIMEOUT,
                self.model
                    .complete_structured(&invocation, Role::Worker, &messages, &schema()),
            )
            .await;
            last = match response {
                Err(_) => SimulatorError::TimedOut(TIMEOUT),
                Ok(Err(error)) => SimulatorError::Model(error.to_string()),
                Ok(Ok(response)) => match parse(&response.message.content, pending.is_some()) {
                    Ok(decision) => return Ok(decision),
                    Err(unusable) => {
                        let described = unusable.describe(attempt);
                        tracing::warn!(run_id = %self.run_id, "unusable user simulator decision: {described}");
                        unusable.append_retry(&mut messages);
                        SimulatorError::Invalid(described)
                    }
                },
            };
        }
        Err(last)
    }
}

fn parse(content: &[ContentBlock], question_pending: bool) -> Result<Decision, Unusable> {
    let mut decision: Decision = structured::parse(content)?;
    decision.text = decision.text.trim().to_owned();
    if decision.action == Action::Answer && !question_pending {
        decision.action = Action::Reply;
    }
    if decision.action != Action::Finish && decision.text.is_empty() {
        return Err(Unusable::new("a reply or answer needs text", content));
    }
    Ok(decision)
}

fn schema() -> JsonSchema {
    JsonSchema {
        name: "user_turn".into(),
        schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["action", "text", "reason"],
            "properties": {
                "action": {"type": "string", "enum": ["reply", "answer", "finish"]},
                "text": {"type": "string"},
                "reason": {"type": "string"},
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: value.into() }]
    }

    #[test]
    fn decisions_parse_and_answers_without_a_question_become_replies() {
        // Given: an answer while no question is pending, a fenced finish and an empty reply.
        let answer = r#"{"action":"answer","text":" yes ","reason":"r"}"#;
        let finish = "```json\n{\"action\":\"finish\",\"text\":\"\",\"reason\":\"done\"}\n```";
        let empty = r#"{"action":"reply","text":"  ","reason":"r"}"#;

        // When / Then: answers need a question, finish needs no text, replies do.
        assert_eq!(
            parse(&text(answer), false).unwrap(),
            Decision {
                action: Action::Reply,
                text: "yes".into(),
                reason: "r".into(),
            }
        );
        assert_eq!(parse(&text(answer), true).unwrap().action, Action::Answer);
        assert_eq!(parse(&text(finish), false).unwrap().action, Action::Finish);
        assert!(parse(&text(empty), false).is_err());
        assert!(parse(&text("not json"), false).is_err());
    }

    fn simulator(model: &Arc<crate::test_support::ScriptedModel>) -> UserSimulator {
        UserSimulator::new(Arc::clone(model) as Arc<dyn AgentModel>, "thread-1")
    }

    #[tokio::test]
    async fn an_unusable_decision_is_asked_again_with_the_rejected_answer() {
        // Given: a model that first wraps the decision in prose without a usable object, then
        // answers properly.
        let model = crate::test_support::ScriptedModel::new([
            crate::test_support::text("Let me think about what the user would say."),
            crate::test_support::decision("reply", "go on"),
        ]);

        // When: the next step is requested.
        let decision = simulator(&model).decide(&[], &[], None).await.unwrap();

        // Then: the second request carries the rejected answer and the correction request.
        assert_eq!(decision.text, "go on");
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].len(), 2);
        assert_eq!(requests[1].len(), 4);
        assert_eq!(
            requests[1][2].content,
            text("Let me think about what the user would say.")
        );
    }

    #[tokio::test]
    async fn retries_append_to_the_history_already_sent() {
        // Given: two different unusable answers in a row (prose; a tool call with no text),
        // then a proper decision.
        let model = crate::test_support::ScriptedModel::new([
            crate::test_support::text("Let me think about what the user would say."),
            crate::test_support::tool("secret_tool_name", serde_json::json!({"k": "secret_input"})),
            crate::test_support::decision("reply", "go on"),
        ]);

        // When: the next step is requested.
        let decision = simulator(&model).decide(&[], &[], None).await.unwrap();

        // Then: each request is the previous one plus the new rejected answer (when it had
        // text) and a request to answer again; the tool call is named only by its kind.
        assert_eq!(decision.text, "go on");
        let requests = model.requests();
        assert_eq!(requests.iter().map(Vec::len).collect::<Vec<_>>(), [2, 4, 5]);
        crate::test_support::assert_prefix_kept(&requests);
        let resent = serde_json::to_string(&requests[2]).unwrap();
        assert!(resent.contains("only tool_use"), "{resent}");
        assert!(!resent.contains("secret_tool_name"), "{resent}");
        assert!(!resent.contains("secret_input"), "{resent}");
    }

    #[tokio::test]
    async fn two_decisions_in_one_answer_are_asked_again() {
        // Given: an example decision followed by the real one.
        let model = crate::test_support::ScriptedModel::new([
            crate::test_support::text(
                "Example: {\"action\":\"finish\",\"text\":\"\",\"reason\":\"e\"}\nMine: {\"action\":\"reply\",\"text\":\"x\",\"reason\":\"r\"}",
            ),
            crate::test_support::decision("reply", "go on"),
        ]);

        // When / Then: neither is taken, and the second answer is used.
        let decision = simulator(&model).decide(&[], &[], None).await.unwrap();
        assert_eq!(decision.text, "go on");
        assert_eq!(model.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_decision_surrounded_by_prose_is_used_without_asking_again() {
        // Given: a model that explains itself around the decision object.
        let model = crate::test_support::ScriptedModel::new([crate::test_support::text(
            "Sure.\n{\"action\":\"finish\",\"text\":\"\",\"reason\":\"done\"}\nThat is all.",
        )]);

        // When / Then: the decision is read from the first request.
        let decision = simulator(&model).decide(&[], &[], None).await.unwrap();
        assert_eq!(decision.action, Action::Finish);
        assert_eq!(model.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_decision_that_stays_unusable_fails_with_the_answer_and_attempts() {
        // Given: a model that never returns an object.
        let model = crate::test_support::ScriptedModel::new([
            crate::test_support::text("first refusal"),
            crate::test_support::text("second refusal"),
            crate::test_support::text("third refusal"),
        ]);

        // When: the next step is requested.
        let error = simulator(&model).decide(&[], &[], None).await.unwrap_err();

        // Then: every attempt was made and the error shows the last answer.
        assert_eq!(model.requests().len(), ATTEMPTS);
        let message = error.to_string();
        assert!(message.contains("unusable decision"), "{message}");
        assert!(message.contains("third refusal"), "{message}");
        assert!(message.contains("attempt 3 of 3"), "{message}");
    }
}
