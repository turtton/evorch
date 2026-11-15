use super::*;
use providers::FinishReason;
use serde_json::{Value, json};
use support::{ScriptedModel, text_response};
use tools::tools::shell_escalation::ShellExecutionContext;

fn explicit() -> Value {
    json!({
        "approve":true, "reason":"explicitly authorized",
        "risk_level":"critical", "authorization_level":"high", "procedure_step":"none",
        "authorization_basis":"explicit_permission", "authorization_refs":[0], "scope_match":"full"
    })
}

fn context() -> ReviewContext {
    let run = ReviewRunContext::empty("root");
    run.add_user_request(
        "root",
        "Delete /repo/a including its unique uncommitted data.",
    );
    review_context(run, None).unwrap().0
}

async fn review(value: Value, context: Option<ReviewContext>) -> ReviewVerdict {
    QuickModelReviewer::new(Arc::new(ScriptedModel::new([Ok(text_response(
        &value.to_string(),
        FinishReason::Stop,
    ))])))
    .review_with_context("root", "rm -rf /repo/a", "not authority", context)
    .await
    .unwrap()
}

#[tokio::test]
async fn explicit_critical_permission_does_not_require_rules_or_skills() {
    let context = context();
    assert!(!context.has_procedure_source());
    assert_eq!(
        review(explicit(), Some(context)).await,
        ReviewVerdict::Approve
    );
}

#[tokio::test]
async fn each_condition_of_the_explicit_route_is_required() {
    for (field, replacement) in [
        ("approve", json!(false)),
        ("authorization_level", json!("medium")),
        ("authorization_basis", json!("ordinary_task")),
        ("authorization_basis", json!("none")),
        ("authorization_refs", json!([])),
        ("authorization_refs", json!([1])),
        ("authorization_refs", json!([0, 1])),
        ("scope_match", json!("partial")),
        ("scope_match", json!("unknown")),
    ] {
        let mut value = explicit();
        value[field] = replacement;
        assert_ne!(
            review(value, Some(context())).await,
            ReviewVerdict::Approve,
            "{field}"
        );
    }
    for field in [
        "reason",
        "procedure_step",
        "authorization_basis",
        "authorization_refs",
        "scope_match",
    ] {
        for risk in ["low", "critical"] {
            let mut value = explicit();
            value["risk_level"] = risk.into();
            value.as_object_mut().unwrap().remove(field);
            assert_ne!(
                review(value, Some(context())).await,
                ReviewVerdict::Approve,
                "{field} {risk}"
            );
        }
    }
    assert_ne!(review(explicit(), None).await, ReviewVerdict::Approve);
}

#[tokio::test]
async fn a_model_denial_stays_a_denial_even_with_all_permission_claims() {
    let mut value = explicit();
    value["approve"] = false.into();
    value["reason"] = "extra side effect".into();
    assert_eq!(
        review(value, Some(context())).await,
        ReviewVerdict::Deny {
            reason: "extra side effect".into(),
        }
    );
}

#[tokio::test]
async fn references_are_validated_after_lineage_filtering_and_questions_are_preserved() {
    let run = ReviewRunContext::empty("root");
    run.add_user_request("sibling", "Delete every worktree");
    run.add_user_request("root", "はい");
    {
        let mut requests = run.user_requests.lock().unwrap();
        requests.requests[1].in_reply_to = Some(UserQuestionContext {
            id: "q1".into(),
            title: "Delete /repo/a only, including uncommitted data?".into(),
        });
    }
    let (context, stamp) = review_context(run, None).unwrap();
    assert_eq!(context.real_user_requests.len(), 1);
    assert_eq!(context.real_user_requests[0].text, "はい");
    assert_eq!(
        stamp.revision, 2,
        "lineage filtering never changes the evidence stamp"
    );
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        &explicit().to_string(),
        FinishReason::Stop,
    ))]));
    assert_eq!(
        QuickModelReviewer::new(model.clone())
            .review_with_context("root", "rm -rf /repo/a", "", Some(context.clone()))
            .await
            .unwrap(),
        ReviewVerdict::Approve
    );
    let calls = model.observed().await;
    let ContentBlock::Text { text } = &calls[0][1].content[0] else {
        panic!("payload")
    };
    let payload: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        payload["context"]["real_user_requests"][0]["in_reply_to"]["title"],
        "Delete /repo/a only, including uncommitted data?"
    );
    assert!(!text.contains("Delete every worktree"));
    let mut value = explicit();
    value["authorization_refs"] = json!([1]);
    assert!(matches!(
        review(value, Some(context)).await,
        ReviewVerdict::Deny { .. }
    ));
}

#[tokio::test]
async fn execution_paths_are_payload_only_and_do_not_change_review_instructions() {
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(&explicit().to_string(), FinishReason::Stop)),
        Ok(text_response(&explicit().to_string(), FinishReason::Stop)),
    ]));
    let reviewer = QuickModelReviewer::new(model.clone());
    for access in [ShellAccess::Host, ShellAccess::Network] {
        reviewer
            .review_execution_with_context(
                "root",
                "pwd",
                "not authority",
                Some(context()),
                ShellExecutionContext {
                    cwd: Some(std::path::Path::new("/repo/subdir")),
                    workspace_root: Some(std::path::Path::new("/repo")),
                    access,
                },
            )
            .await
            .unwrap();
    }
    let calls = model.observed().await;
    assert_eq!(calls[0][0], calls[1][0]);
    for (call, access) in calls.iter().zip(["host_unsandboxed", "network_only"]) {
        let ContentBlock::Text { text } = &call[1].content[0] else {
            panic!("payload")
        };
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["cwd"], "/repo/subdir");
        assert_eq!(payload["workspace_root"], "/repo");
        assert_eq!(payload["access"], access);
        assert!(payload.get("env").is_none());
        assert!(payload.get("credentials").is_none());
    }
}

#[test]
fn non_shell_thresholds_ignore_every_shell_claim() {
    let verdict: WireVerdict = serde_json::from_value(explicit()).unwrap();
    for action in ["file access", "web fetch", "upload"] {
        assert!(matches!(
            judge(&verdict, action, false),
            ReviewVerdict::RequiresUser { .. }
        ));
    }
    for name in ["authorization_basis", "authorization_refs", "scope_match"] {
        assert!(verdict_schema().schema["properties"].get(name).is_none());
        assert!(
            shell_verdict_schema().schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(name))
        );
    }
}

fn recorded_response(result: &Value) -> Value {
    let mut result = result.clone();
    result["risk_level"] = result["risk"].clone();
    result["authorization_level"] = result["authorization"].clone();
    for field in ["risk", "authorization", "decision"] {
        result.as_object_mut().unwrap().remove(field);
    }
    result
}

pub(super) fn uncertain_responses() -> Vec<Value> {
    let path = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/shell-review/uncertain-denials.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    fixture["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| recorded_response(&record["result"]))
        .collect()
}

#[tokio::test]
async fn recorded_uncertain_nonapprovals_request_clarification_instead_of_denying() {
    let responses = uncertain_responses();
    assert_eq!(responses.len(), 3);
    for response in responses {
        assert_eq!(response["approve"], false);
        assert_eq!(response["scope_match"], "unknown");
        let reason = response["reason"].as_str().unwrap().to_owned();
        assert_eq!(
            review(response, Some(context())).await,
            ReviewVerdict::RequiresUser { reason }
        );
    }
}

#[tokio::test]
async fn uncertain_permission_and_scope_never_automatically_approve() {
    for reason in [
        "The target is unspecified",
        "Real-user permission is uncertain",
    ] {
        let mut value = explicit();
        value["approve"] = false.into();
        value["reason"] = reason.into();
        value["authorization_level"] = "none".into();
        value["authorization_basis"] = "none".into();
        value["authorization_refs"] = json!([]);
        value["scope_match"] = "unknown".into();
        assert_eq!(
            review(value.clone(), Some(context())).await,
            ReviewVerdict::RequiresUser {
                reason: reason.into()
            }
        );
        for scope in ["partial", "full"] {
            value["scope_match"] = scope.into();
            assert_eq!(
                review(value.clone(), Some(context())).await,
                ReviewVerdict::Deny {
                    reason: reason.into()
                }
            );
        }
    }
}

#[test]
fn recorded_model_decisions_replay_through_the_current_policy() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/shell-review");
    let corpus_text = std::fs::read_to_string(root.join("corpus.json")).unwrap();
    let corpus: Value = serde_json::from_str(&corpus_text).unwrap();
    for (file, baseline) in [
        ("baseline-results.jsonl", true),
        ("candidate-results.jsonl", false),
    ] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let mut rows = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap());
        let header = rows.next().unwrap();
        assert_eq!(header["corpus_sha256"], sha256_hex(corpus_text.as_bytes()));
        if !baseline {
            assert_eq!(
                header["instruction_sha256"],
                sha256_hex(REVIEW_INSTRUCTION.as_bytes())
            );
            assert_eq!(header["schema"], shell_verdict_schema().schema);
        }
        let mut replayed = std::collections::BTreeMap::<String, usize>::new();
        for row in rows {
            if row["result"].get("error").is_some() {
                continue;
            }
            let wire: WireVerdict =
                serde_json::from_value(recorded_response(&row["result"])).unwrap();
            assert!(wire.risk_level.is_some() && wire.authorization_level.is_some());
            let case = corpus["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|case| case["id"] == row["id"])
                .unwrap();
            let context = &case["payload"]["context"];
            let procedure_source = context["project_rules"]["text"].is_string()
                || context["loaded_skills"]
                    .as_array()
                    .is_some_and(|s| !s.is_empty());
            let decision = if baseline {
                judge(&wire, "shell action", procedure_source)
            } else {
                judge_shell(
                    &wire,
                    procedure_source,
                    context["real_user_requests"].as_array().map_or(0, Vec::len),
                )
            };
            assert_eq!(
                format!("{decision:?}"),
                row["result"]["decision"].as_str().unwrap(),
                "{file} {}",
                row["id"]
            );
            *replayed
                .entry(row["profile"].as_str().unwrap().to_owned())
                .or_default() += 1;
        }
        assert_eq!(
            replayed.get("codex-personal"),
            Some(&51),
            "the primary measurement is complete"
        );
        assert!(replayed.values().all(|count| *count <= 51));
    }
    let mut malformed = explicit();
    malformed["procedure_step"] = Value::Null;
    assert!(serde_json::from_value::<WireVerdict>(malformed).is_err());
}
