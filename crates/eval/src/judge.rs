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

use storage::eval_sample::sha256_hex;

use crate::changes::{Changes, ToolDescriptionChange};
use crate::cost::RequestUsage;
use crate::structured::{self, ATTEMPTS, Unusable};

const TIMEOUT: Duration = Duration::from_secs(600);
/// The judge's binding is invoked through the orchestrator slot.
pub const ROLE: Role = Role::Orchestrator;

pub const ASPECTS: [&str; 4] = ["requirement", "correctness", "scope", "efficiency"];
pub const PROBLEM_ASPECT: &str = "problem";

/// The version of the judge's instructions and payloads. It is part of the
/// identity of every cached judgment: bump it with any change to what the judge
/// is asked or sent that the instruction texts below do not already show.
pub const PAYLOAD_VERSION: u32 = 2;

const COMPARE_INSTRUCTION: &str = "You compare two replays, A and B, of one past conversation between a user and a coding agent. Both started from the same repository state, and a simulated user replayed the original user's messages (`original_user_messages`) to each. For each replay you get its status, the conversation the user saw (the root run's messages only), `delegated_outputs` (the final output of each delegated child run, such as a Reviewer's or Worker's report; `delegated_by` names the delegating role), the diff from the start state, the verification command's result when one ran, metrics, and its cost in USD. `changes` states in what the configurations of A and B differ, and only in that; `focus` marks the delegated runs of the roles those differences can reach. A difference is one of: `role_bindings` (the listed roles are bound to other models, routes or presets), or `tool_descriptions` (for the listed tool, A and B send different description texts to the same model; the model is not changed, and `description_hash_A` / `description_hash_B` identify the two texts). Do not assume that a model differs unless `role_bindings` names the role. Judge what each replay did, not what its configuration was meant to achieve, and do not credit a difference the replays show no sign of. A replay that did not complete is still judged on what it achieved. When a delegated child's output is part of the evidence, judge its quality and whether the root relied on it correctly, rather than only the root's summary of it. Judge each aspect on its own and return the winner (\"A\", \"B\" or \"tie\") with a short reason that cites evidence:
- requirement: how fully the original requests were met.
- correctness: whether the changes are correct, from the diff and the verification result.
- scope: whether the replay kept to what was asked, without unrequested changes.
- efficiency: cost, tokens, tool calls and time spent for the result; spending more wins only when it bought a clearly better result.
- problem (only when `problem` is given): whether the described problem occurred; the replay where it did not occur, or that fixed it, wins.
Answer \"tie\" when the evidence does not separate the replays. The order in which they are presented carries no meaning.";

const ANALYZE_INSTRUCTION: &str = "You analyse how two replays, baseline and candidate, of one past conversation between a user and a coding agent went. The candidate's configuration differs from the baseline's only in `changes`: `role_bindings` (roles bound to other models, routes or presets) and `tool_descriptions` (another description of a tool for the same model; the model itself is not changed). `affected_roles` lists the roles those differences can reach, and `applied` says, for each changed description, whether the candidate's runs really used it and whether any request went to another model. Do not infer an effect from a difference that `applied` shows was not in force. For each replay you get its status, metrics, `delegated_outputs` (the final output of each delegated child run; `focus` marks the affected roles) and a bounded summary of its events: token totals per role, then timestamped delegations, tool calls with short inputs, durations and errors, turns, provider failures and notable diagnostics. Answer each field concisely with concrete evidence from the timelines:
- time_and_tokens: where time and tokens went in each replay.
- detours: detours, and duplicated or redundant tool calls.
- delegation: whether the delegations were appropriate.
- affected_roles: how the affected roles behaved in each replay, and whether the change showed in their behaviour.
- difference: the difference between the replays, focused on the affected roles and, when given, on `problem`.";

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
    pub affected_roles: String,
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

/// Why an aspect's winner is what it is, so that a tie the judge gave in both
/// orders is not confused with one that two disagreeing answers made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Both orders named the same winner.
    Agreed,
    /// Both orders answered tie: the replays are alike on this aspect.
    BothTie,
    /// The orders named different winners (or one named a winner and the
    /// other a tie): the answer followed the position, so there is no result.
    OrderDisagreement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AspectResult {
    pub aspect: String,
    pub winner: Preferred,
    pub basis: Basis,
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
/// answers disagree is a tie, recorded as a disagreement and not as a tie both
/// orders gave.
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
            let basis = match (left == right, left) {
                (true, Preferred::Tie) => Basis::BothTie,
                (true, _) => Basis::Agreed,
                (false, _) => Basis::OrderDisagreement,
            };
            Some(AspectResult {
                aspect: (*aspect).to_owned(),
                winner: if left == right { left } else { Preferred::Tie },
                basis,
                baseline_first: first.clone(),
                candidate_first: second.clone(),
            })
        })
        .collect()
}

type HashOf = fn(&ToolDescriptionChange) -> &str;

/// What the comparison tells the judge about the differences between A and B.
/// Each side is named by its position, never as baseline or candidate: with
/// `order`, the first replay is A, so the hashes follow the same order and the
/// payload reveals nothing about which configuration is which.
pub fn comparison_changes(changes: &Changes, order: Order) -> serde_json::Value {
    let (first, second): (HashOf, HashOf) = match order {
        Order::BaselineFirst => (|c| &c.baseline_hash, |c| &c.candidate_hash),
        Order::CandidateFirst => (|c| &c.candidate_hash, |c| &c.baseline_hash),
    };
    let descriptions: Vec<serde_json::Value> = changes
        .tool_descriptions
        .iter()
        .map(|change| {
            serde_json::json!({
                "tool": change.tool,
                "profile": change.profile,
                "model": change.route_model_id,
                "description_hash_A": first(change),
                "description_hash_B": second(change),
            })
        })
        .collect();
    serde_json::json!({
        "role_bindings": changes.bindings,
        "tool_descriptions": descriptions,
        "affected_roles": changes.affected_roles,
    })
}

fn identity(parts: &[&str]) -> String {
    sha256_hex(format!("judge-payload-{PAYLOAD_VERSION}\n{}", parts.join("\n")).as_bytes())
}

/// Names the instruction, the payload version, the aspects asked and the
/// differences told, so that a judgment made under any other is not reused.
pub fn compare_identity(aspects: &[&str], changes: &Changes) -> String {
    identity(&[
        COMPARE_INSTRUCTION,
        &aspects.join(","),
        &serde_json::to_string(changes).unwrap_or_default(),
    ])
}

/// [`compare_identity`] for the process analysis.
pub fn analysis_identity(changes: &Changes) -> String {
    identity(&[
        ANALYZE_INSTRUCTION,
        &serde_json::to_string(changes).unwrap_or_default(),
    ])
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
        let mut messages = vec![
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
            btw: None,
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
        for attempt in 1..=ATTEMPTS {
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
                    let content = &response.message.content;
                    match structured::parse::<T>(content).and_then(|value| {
                        validate(&value)
                            .map(|()| value)
                            .map_err(|reason| Unusable::new(&reason, content))
                    }) {
                        Ok(value) => {
                            answer = Some(value);
                            break;
                        }
                        Err(unusable) => {
                            let described = unusable.describe(attempt);
                            tracing::warn!(run_id = %self.run_id, "unusable judge answer: {described}");
                            unusable.append_retry(&mut messages);
                            JudgeError::Invalid(described)
                        }
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
        "affected_roles",
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
    fn a_tie_in_both_orders_is_told_apart_from_a_tie_the_orders_made() {
        // Given: scope is a tie in both orders; efficiency is won by A in both, so each order
        // names another side; correctness is a tie in one order and a win in the other;
        // requirement agrees on the candidate.
        let baseline_first = verdicts(&[
            ("requirement", Winner::B),
            ("correctness", Winner::Tie),
            ("scope", Winner::Tie),
            ("efficiency", Winner::A),
        ]);
        let candidate_first = verdicts(&[
            ("requirement", Winner::A),
            ("correctness", Winner::A),
            ("scope", Winner::Tie),
            ("efficiency", Winner::A),
        ]);

        // When: the two orders are aggregated.
        let results = aggregate(&baseline_first, &candidate_first, &aspects(false));

        // Then: every tie is a tie, and only the basis says which was the judge's and which the
        // orders'.
        let seen: Vec<(&str, Preferred, Basis)> = results
            .iter()
            .map(|result| (result.aspect.as_str(), result.winner, result.basis))
            .collect();
        assert_eq!(
            seen,
            [
                ("requirement", Preferred::Candidate, Basis::Agreed),
                ("correctness", Preferred::Tie, Basis::OrderDisagreement),
                ("scope", Preferred::Tie, Basis::BothTie),
                ("efficiency", Preferred::Tie, Basis::OrderDisagreement),
            ]
        );
    }

    fn description_change() -> Changes {
        Changes {
            bindings: Vec::new(),
            tool_descriptions: vec![ToolDescriptionChange {
                profile: "p".into(),
                route_model_id: "m".into(),
                tool: "read".into(),
                baseline_hash: "hash-of-baseline-text".into(),
                candidate_hash: "hash-of-candidate-text".into(),
            }],
            affected_roles: vec!["worker".into()],
        }
    }

    #[test]
    fn the_changes_follow_the_presentation_order_and_never_name_the_configurations() {
        // Given: a description change, shown with each configuration first in turn.
        let changes = description_change();

        // When: the judge's view of the changes is built for both orders.
        let baseline_first = comparison_changes(&changes, Order::BaselineFirst);
        let candidate_first = comparison_changes(&changes, Order::CandidateFirst);

        // Then: the hashes sit under the side that sends them, and nothing says which side is the
        // baseline or the candidate.
        assert_eq!(
            baseline_first["tool_descriptions"][0]["description_hash_A"],
            "hash-of-baseline-text"
        );
        assert_eq!(
            candidate_first["tool_descriptions"][0]["description_hash_A"],
            "hash-of-candidate-text"
        );
        assert_eq!(
            candidate_first["tool_descriptions"][0]["description_hash_B"],
            "hash-of-baseline-text"
        );
        assert_eq!(baseline_first["role_bindings"], serde_json::json!([]));
        assert_eq!(
            candidate_first["affected_roles"],
            serde_json::json!(["worker"])
        );
        for view in [&baseline_first, &candidate_first] {
            let keys = serde_json::to_string(&view.as_object().unwrap().keys().collect::<Vec<_>>())
                .unwrap();
            assert!(
                !keys.contains("baseline") && !keys.contains("candidate"),
                "{keys}"
            );
            let tool = view["tool_descriptions"][0].as_object().unwrap();
            assert!(
                tool.keys()
                    .all(|key| !key.contains("baseline") && !key.contains("candidate"))
            );
        }
    }

    #[test]
    fn a_judgment_is_reused_only_under_the_same_instructions_aspects_and_changes() {
        // Given: the identity of a comparison and of an analysis.
        let changes = description_change();
        let known = compare_identity(&aspects(false), &changes);

        // When / Then: another aspect set, another change, or the other kind of question is
        // another identity, and the same question is the same one.
        assert_eq!(known, compare_identity(&aspects(false), &changes));
        assert_ne!(known, compare_identity(&aspects(true), &changes));
        let mut other = changes.clone();
        other.tool_descriptions[0].candidate_hash = "another".into();
        assert_ne!(known, compare_identity(&aspects(false), &other));
        assert_ne!(analysis_identity(&changes), analysis_identity(&other));
        assert_ne!(known, analysis_identity(&changes));
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

    fn judge(model: &Arc<crate::test_support::ScriptedModel>) -> Judge {
        Judge::new(
            Arc::clone(model) as Arc<dyn AgentModel>,
            Arc::new(EventBus::new(16)),
            "thread-1",
        )
    }

    const ANALYSIS: &str = r#"{"time_and_tokens":"a","detours":"b","delegation":"c","affected_roles":"d","difference":"e"}"#;

    #[tokio::test]
    async fn an_analysis_with_prose_around_it_or_asked_again_is_used() {
        // Given: an answer that introduces the object, then one that is rejected before a
        // proper object.
        let wrapped = crate::test_support::ScriptedModel::new([crate::test_support::text(
            &format!("Here is my analysis:\n```json\n{ANALYSIS}\n```"),
        )]);
        let retried = crate::test_support::ScriptedModel::new([
            crate::test_support::text("I will analyse the replays now."),
            crate::test_support::text(ANALYSIS),
        ]);

        // When: the analysis is requested.
        let from_wrapped = judge(&wrapped).analyze(&serde_json::json!({})).await;
        let from_retried = judge(&retried).analyze(&serde_json::json!({})).await;

        // Then: both give the analysis; the retry showed the rejected answer.
        assert_eq!(from_wrapped.result.unwrap().difference, "e");
        assert_eq!(wrapped.requests().len(), 1);
        assert_eq!(from_retried.result.unwrap().difference, "e");
        let requests = retried.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].len(), 4);
    }

    #[tokio::test]
    async fn retries_append_to_the_history_already_sent() {
        // Given: two different unusable answers in a row, then the analysis.
        let model = crate::test_support::ScriptedModel::new([
            crate::test_support::text("I will analyse the replays now."),
            crate::test_support::text(r#"{"time_and_tokens":"only one field"}"#),
            crate::test_support::text(ANALYSIS),
        ]);

        // When: the analysis is requested.
        let judged = judge(&model).analyze(&serde_json::json!({})).await;

        // Then: it is used, and each request is the previous one with the rejection appended.
        assert_eq!(judged.result.unwrap().difference, "e");
        let requests = model.requests();
        assert_eq!(requests.iter().map(Vec::len).collect::<Vec<_>>(), [2, 4, 6]);
        crate::test_support::assert_prefix_kept(&requests);
    }

    #[tokio::test]
    async fn a_comparison_missing_an_aspect_is_asked_again_and_then_fails_with_the_cause() {
        // Given: answers that always omit the efficiency verdict.
        let partial = r#"{"requirement":{"winner":"A","reason":"r"},"correctness":{"winner":"A","reason":"r"},"scope":{"winner":"A","reason":"r"}}"#;
        let model = crate::test_support::ScriptedModel::new([
            crate::test_support::text(partial),
            crate::test_support::text(partial),
            crate::test_support::text(partial),
        ]);

        // When: the comparison is requested.
        let judged = judge(&model)
            .compare(&serde_json::json!({}), &aspects(false))
            .await;

        // Then: it fails after every attempt, naming the missing aspect.
        assert_eq!(model.requests().len(), ATTEMPTS);
        let message = judged.result.unwrap_err().to_string();
        assert!(message.contains("no verdict for efficiency"), "{message}");
        assert!(message.contains("attempt 3 of 3"), "{message}");
    }
}
