//! The LLM judge (ADR 0036 §7): a pairwise comparison asked in both orders,
//! and a process analysis of each baseline–candidate pair.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use event_bus::{EventBus, RequestPurpose};
use providers::{ContentBlock, JsonSchema, Message};
use runtime::{AgentInvocationContext, AgentModel, Role};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cost::RequestUsage;
use crate::simulator::json_text;

const ATTEMPTS: usize = 3;
const TIMEOUT: Duration = Duration::from_secs(600);
/// The judge's binding is invoked through the orchestrator slot.
pub const ROLE: Role = Role::Orchestrator;

pub const ASPECTS: [&str; 4] = ["requirement", "correctness", "scope", "efficiency"];
pub const PROBLEM_ASPECT: &str = "problem";

const COMPARE_INSTRUCTION: &str = "You compare two replays, A and B, of one past conversation between a user and a coding agent. Both started from the same repository state, and a simulated user replayed the original user's messages (`original_user_messages`) to each. For each replay you get its status, the conversation the user saw, the diff from the start state, the verification command's result when one ran, metrics, and its cost in USD. A replay that did not complete is still judged on what it achieved. Judge each aspect on its own and return the winner (\"A\", \"B\" or \"tie\") with a short reason that cites evidence:
- requirement: how fully the original requests were met.
- correctness: whether the changes are correct, from the diff and the verification result.
- scope: whether the replay kept to what was asked, without unrequested changes.
- efficiency: cost, tokens, tool calls and time spent for the result; spending more wins only when it bought a clearly better result.
- problem (only when `problem` is given): whether the described problem occurred; the replay where it did not occur, or that fixed it, wins.
Answer \"tie\" when the evidence does not separate the replays. The order in which they are presented carries no meaning.";

const ANALYZE_INSTRUCTION: &str = "You analyse how two replays, baseline and candidate, of one past conversation between a user and a coding agent went. The candidate's configuration differs from the baseline's only in `swapped_roles`. For each replay you get its status, metrics, and a bounded summary of its events: token totals per role, then timestamped delegations, tool calls with short inputs, durations and errors, turns, provider failures and notable diagnostics. Answer each field concisely with concrete evidence from the timelines:
- time_and_tokens: where time and tokens went in each replay.
- detours: detours, and duplicated or redundant tool calls.
- delegation: whether the delegations were appropriate.
- swapped_roles: how the swapped roles behaved in each replay.
- difference: the difference between the replays, focused on the swapped roles and, when given, on `problem`.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Winner {
    A,
    B,
    #[serde(rename = "tie")]
    Tie,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AspectVerdict {
    pub winner: Winner,
    pub reason: String,
}

/// One answer of the pairwise comparison, by aspect.
pub type Verdicts = BTreeMap<String, AspectVerdict>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessAnalysis {
    pub time_and_tokens: String,
    pub detours: String,
    pub delegation: String,
    pub swapped_roles: String,
    pub difference: String,
}

/// Which presentation order a comparison used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    BaselineFirst,
    CandidateFirst,
}

impl Order {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BaselineFirst => "baseline_first",
            Self::CandidateFirst => "candidate_first",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preferred {
    Baseline,
    Candidate,
    Tie,
}

impl Preferred {
    fn of(winner: Winner, order: Order) -> Self {
        match (winner, order) {
            (Winner::Tie, _) => Self::Tie,
            (Winner::A, Order::BaselineFirst) | (Winner::B, Order::CandidateFirst) => {
                Self::Baseline
            }
            (Winner::B, Order::BaselineFirst) | (Winner::A, Order::CandidateFirst) => {
                Self::Candidate
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AspectResult {
    pub aspect: String,
    pub winner: Preferred,
    pub baseline_first: AspectVerdict,
    pub candidate_first: AspectVerdict,
}

/// The aspects a comparison answers: the problem only when one is described.
pub fn aspects(problem: bool) -> Vec<&'static str> {
    ASPECTS
        .into_iter()
        .chain(problem.then_some(PROBLEM_ASPECT))
        .collect()
}

/// Map both orders back to baseline and candidate; an aspect whose two
/// answers disagree is a tie.
pub fn aggregate(
    baseline_first: &Verdicts,
    candidate_first: &Verdicts,
    aspects: &[&str],
) -> Vec<AspectResult> {
    aspects
        .iter()
        .filter_map(|aspect| {
            let first = baseline_first.get(*aspect)?;
            let second = candidate_first.get(*aspect)?;
            let left = Preferred::of(first.winner, Order::BaselineFirst);
            let right = Preferred::of(second.winner, Order::CandidateFirst);
            Some(AspectResult {
                aspect: (*aspect).to_owned(),
                winner: if left == right { left } else { Preferred::Tie },
                baseline_first: first.clone(),
                candidate_first: second.clone(),
            })
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("the judge did not answer within {0:?}")]
    TimedOut(Duration),
    #[error("the judge request failed: {0}")]
    Model(String),
    #[error("the judge returned an unusable answer: {0}")]
    Invalid(String),
}

/// What a judge call returned, with every request it made, failed attempts included.
pub struct Judged<T> {
    pub result: Result<T, JudgeError>,
    pub usage: Vec<RequestUsage>,
}

pub struct Judge {
    model: Arc<dyn AgentModel>,
    bus: Arc<EventBus>,
    run_id: String,
}

impl Judge {
    /// `bus` is the one the model reports its requests on.
    pub fn new(model: Arc<dyn AgentModel>, bus: Arc<EventBus>, thread_id: &str) -> Self {
        Self {
            model,
            bus,
            run_id: format!("judge:{thread_id}"),
        }
    }

    pub async fn compare(&self, payload: &serde_json::Value, aspects: &[&str]) -> Judged<Verdicts> {
        self.ask(
            COMPARE_INSTRUCTION,
            payload,
            &compare_schema(aspects),
            |verdicts: &Verdicts| match aspects
                .iter()
                .find(|aspect| !verdicts.contains_key(**aspect))
            {
                Some(missing) => Err(format!("no verdict for {missing}")),
                None if verdicts.len() > aspects.len() => Err("unexpected aspects".into()),
                None => Ok(()),
            },
        )
        .await
    }

    pub async fn analyze(&self, payload: &serde_json::Value) -> Judged<ProcessAnalysis> {
        self.ask(ANALYZE_INSTRUCTION, payload, &analysis_schema(), |_| Ok(()))
            .await
    }

    async fn ask<T: DeserializeOwned>(
        &self,
        instruction: &str,
        payload: &serde_json::Value,
        schema: &JsonSchema,
        validate: impl Fn(&T) -> Result<(), String>,
    ) -> Judged<T> {
        let messages = [
            Message {
                role: providers::Role::System,
                content: vec![ContentBlock::Text {
                    text: instruction.into(),
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
            purpose: RequestPurpose::Judging,
            turn: None,
        };
        let mut requests = self.bus.subscribe();
        let mut last = JudgeError::Invalid("no attempt was made".into());
        let mut answer = None;
        for _ in 0..ATTEMPTS {
            let response = tokio::time::timeout(
                TIMEOUT,
                self.model
                    .complete_structured(&invocation, ROLE, &messages, schema),
            )
            .await;
            last = match response {
                Err(_) => JudgeError::TimedOut(TIMEOUT),
                Ok(Err(error)) => JudgeError::Model(error.to_string()),
                Ok(Ok(response)) => {
                    match serde_json::from_str::<T>(&json_text(&response.message.content))
                        .map_err(|error| error.to_string())
                        .and_then(|value| validate(&value).map(|()| value))
                    {
                        Ok(value) => {
                            answer = Some(value);
                            break;
                        }
                        Err(error) => JudgeError::Invalid(error),
                    }
                }
            };
        }
        let usage = requests
            .drain_pending_snapshot()
            .iter()
            .filter_map(|event| RequestUsage::from_event(&event.kind))
            .filter(|request| request.run_id.as_deref() == Some(self.run_id.as_str()))
            .map(|mut request| {
                request.purpose = Some(RequestPurpose::Judging.as_str().to_owned());
                request
            })
            .collect();
        Judged {
            result: answer.ok_or(last),
            usage,
        }
    }
}

fn compare_schema(aspects: &[&str]) -> JsonSchema {
    let verdict = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["winner", "reason"],
        "properties": {
            "winner": {"type": "string", "enum": ["A", "B", "tie"]},
            "reason": {"type": "string"},
        },
    });
    let properties: serde_json::Map<String, serde_json::Value> = aspects
        .iter()
        .map(|aspect| ((*aspect).to_owned(), verdict.clone()))
        .collect();
    JsonSchema {
        name: "pairwise_judgment".into(),
        schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": aspects,
            "properties": properties,
        }),
    }
}

fn analysis_schema() -> JsonSchema {
    let fields = [
        "time_and_tokens",
        "detours",
        "delegation",
        "swapped_roles",
        "difference",
    ];
    let properties: serde_json::Map<String, serde_json::Value> = fields
        .iter()
        .map(|field| ((*field).to_owned(), serde_json::json!({"type": "string"})))
        .collect();
    JsonSchema {
        name: "process_analysis".into(),
        schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": fields,
            "properties": properties,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdicts(winners: &[(&str, Winner)]) -> Verdicts {
        winners
            .iter()
            .map(|(aspect, winner)| {
                (
                    (*aspect).to_owned(),
                    AspectVerdict {
                        winner: *winner,
                        reason: format!("{aspect} reason"),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn agreeing_orders_keep_their_winner_and_disagreeing_ones_tie() {
        // Given: the candidate wins requirement in both orders (B, then A), the baseline wins
        // correctness in both (A, then B), scope is a tie, and the orders disagree on efficiency
        // because the judge preferred whichever replay came first.
        let baseline_first = verdicts(&[
            ("requirement", Winner::B),
            ("correctness", Winner::A),
            ("scope", Winner::Tie),
            ("efficiency", Winner::A),
        ]);
        let candidate_first = verdicts(&[
            ("requirement", Winner::A),
            ("correctness", Winner::B),
            ("scope", Winner::Tie),
            ("efficiency", Winner::A),
        ]);

        // When: the two orders are aggregated.
        let results = aggregate(&baseline_first, &candidate_first, &aspects(false));

        // Then: agreement keeps the winner, the positional disagreement becomes a tie.
        let winners: Vec<(&str, Preferred)> = results
            .iter()
            .map(|result| (result.aspect.as_str(), result.winner))
            .collect();
        assert_eq!(
            winners,
            [
                ("requirement", Preferred::Candidate),
                ("correctness", Preferred::Baseline),
                ("scope", Preferred::Tie),
                ("efficiency", Preferred::Tie),
            ]
        );
        assert_eq!(results[0].candidate_first.reason, "requirement reason");
    }

    #[test]
    fn the_problem_aspect_is_asked_only_when_a_problem_is_described() {
        // Given / When: the aspects and schemas with and without a problem statement.
        let without = aspects(false);
        let with = aspects(true);

        // Then: only the second asks for and aggregates the problem aspect.
        assert!(!without.contains(&PROBLEM_ASPECT));
        assert_eq!(with.last(), Some(&PROBLEM_ASPECT));
        assert_eq!(
            compare_schema(&with).schema["required"],
            serde_json::json!([
                "requirement",
                "correctness",
                "scope",
                "efficiency",
                "problem"
            ])
        );
        assert!(
            compare_schema(&without).schema["properties"]
                .get(PROBLEM_ASPECT)
                .is_none()
        );
        let both = verdicts(&[("problem", Winner::B)]);
        let flipped = verdicts(&[("problem", Winner::A)]);
        let results = aggregate(&both, &flipped, &[PROBLEM_ASPECT]);
        assert_eq!(results[0].winner, Preferred::Candidate);
    }
}
