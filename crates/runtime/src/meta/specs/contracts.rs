use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::tool_spec;
use crate::meta::{self, EmptyArgs};

fn contract<T: DeserializeOwned>(name: &str, input: Value) {
    let schema = tool_spec(name).input_schema;
    let validator = jsonschema::validator_for(&schema).expect("valid meta schema");
    assert!(
        validator.is_valid(&input),
        "{name} advertised arguments: {input}"
    );
    assert!(
        serde_json::from_value::<T>(input.clone()).is_ok(),
        "{name} handler accepts advertised arguments"
    );
    for required in schema["required"].as_array().expect("required list") {
        let mut missing = input.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove(required.as_str().unwrap());
        assert!(
            !validator.is_valid(&missing),
            "{name}: schema missing {required}"
        );
        assert!(
            serde_json::from_value::<T>(missing).is_err(),
            "{name}: handler missing {required}"
        );
    }
}

#[test]
fn every_registered_meta_operation_has_a_complete_valid_contract() {
    for name in crate::META_OPS {
        let spec = tool_spec(name);
        assert_eq!(spec.name, *name);
        assert!(
            spec.description.len() > 100,
            "missing meaningful description for {name}"
        );
        assert_eq!(spec.input_schema["type"], "object");
        assert!(
            spec.input_schema["properties"].is_object(),
            "missing properties for {name}"
        );
        assert!(
            spec.input_schema["required"].is_array(),
            "missing required list for {name}"
        );
        jsonschema::validator_for(&spec.input_schema)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

#[test]
fn schemas_and_actual_argument_parsers_agree_on_required_fields() {
    contract::<meta::delegation::DelegateArgs>(
        "delegate",
        json!({
            "prompt":"Implement x; own x.rs; verify cargo test; report outcome, changes, checks and blockers.",
            "role":"worker", "background":true, "interactive":true, "name":"implementation",
            "category":"deep", "workspace_mode":"isolated", "workspace_branch":"topic", "load_skills":["rust"],
            "task":{"id":"task-1","paths":["src/x.rs"]}
        }),
    );
    contract::<meta::runs::RunArgs>("run_output", json!({"run_id":"run-2"}));
    contract::<meta::runs::RunArgs>("cancel", json!({"run_id":"run-2"}));
    contract::<meta::runs::RunArgs>("inspect_agent", json!({"run_id":"run-2"}));
    contract::<meta::messaging::SendMessageArgs>(
        "send_message",
        json!({"run_id":"run-2","message":"blocker"}),
    );
    contract::<meta::messaging::SendArgs>(
        "send",
        json!({"run_id":"run-2","message":"answer","kind":"reply","reply_to":"msg-1"}),
    );
    contract::<meta::messaging::WaitReplyArgs>(
        "wait_reply",
        json!({"message_id":"msg-1","timeout_ms":10000}),
    );
    contract::<meta::skills::SkillLoadArgs>(
        "skill_load",
        json!({"name":"rust","resource":"references/guide.md"}),
    );
    contract::<meta::questions::AskArgs>(
        "ask_user",
        json!({"title":"Choose implementation scope", "options":["A","B"], "blocking":false}),
    );
    contract::<meta::FinishArgs>(
        "finish",
        json!({"result":"Completed, files x, tested y, no open blockers."}),
    );
    contract::<meta::escalation::EscalateArgs>(
        "escalate",
        json!({
            "original_request":"fix bug", "escalation_reason":"requires coordinated work", "findings":["fact"],
            "files_touched":["src/x.rs"], "blockers":["decision"], "workspace_state":"dirty", "suggested_next":"inspect y"
        }),
    );
    contract::<meta::ledger::AppendArgs>("ledger_append", json!({"body":"checkpoint"}));
    contract::<crate::orchestration::review::ReviewResult>(
        "submit_review",
        json!({
            "verdict":"request-update", "findings":["missing test"], "criteria":[{
                "id":"AC1", "status":"unmet", "note":"test fails", "evidence":{
                    "command":"cargo test", "exit_status":1, "target_sha":"abc", "diff_ref":null,
                    "artifact_path":"/var/tmp/test.log", "red_evidence":"failure output"
                }
            }]
        }),
    );
    for name in [
        "list_agents",
        "inbox",
        "compact",
        "ledger_read",
        "user_answers",
    ] {
        contract::<EmptyArgs>(name, json!({}));
    }
}

#[test]
fn reply_requires_correlation_and_review_evidence_is_typed() {
    let send = jsonschema::validator_for(&tool_spec("send").input_schema).unwrap();
    assert!(send.is_valid(&json!({"run_id":"run-2", "message":"hello"})));
    assert!(!send.is_valid(&json!({"run_id":"run-2", "message":"answer", "kind":"reply"})));
    let review = jsonschema::validator_for(&tool_spec("submit_review").input_schema).unwrap();
    for evidence in [
        json!({"command":"test", "exit_status":"0", "target_sha":"abc"}),
        json!({"command":"test", "exit_status":0}),
    ] {
        let input = json!({"verdict":"approve", "criteria":[{"id":"AC1","status":"met","note":"ok","evidence":evidence}]});
        assert!(!review.is_valid(&input));
        assert!(
            serde_json::from_value::<crate::orchestration::review::ReviewResult>(input).is_err()
        );
    }
}
