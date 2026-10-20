//! The model that plays the original user during a replay (ADR 0036 §5).

use std::sync::Arc;
use std::time::Duration;

use providers::{ContentBlock, JsonSchema, Message};
use runtime::{AgentInvocationContext, AgentModel, Role};
use serde::{Deserialize, Serialize};

use crate::conversation::Entry;
use crate::source::UserTurn;

const ATTEMPTS: usize = 3;
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
        let messages = [
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
            run_id: self.run_id.clone(),
            model_preference: None,
            category: None,
            purpose: event_bus::RequestPurpose::UserSimulation,
            turn: None,
        };
        let mut last = SimulatorError::Invalid("no attempt was made".into());
        for _ in 0..ATTEMPTS {
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
                    Err(error) => error,
                },
            };
        }
        Err(last)
    }
}

fn parse(content: &[ContentBlock], question_pending: bool) -> Result<Decision, SimulatorError> {
    let text: String = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let json = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let mut decision: Decision =
        serde_json::from_str(json).map_err(|error| SimulatorError::Invalid(error.to_string()))?;
    decision.text = decision.text.trim().to_owned();
    if decision.action == Action::Answer && !question_pending {
        decision.action = Action::Reply;
    }
    if decision.action != Action::Finish && decision.text.is_empty() {
        return Err(SimulatorError::Invalid(
            "a reply or answer needs text".into(),
        ));
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
}
