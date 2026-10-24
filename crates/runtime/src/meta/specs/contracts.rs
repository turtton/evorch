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
            !spec.description.trim().is_empty(),
            "missing description for {name}"
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
        json!({"target": {"role": "worker", "category": "deep"},
            "prompt":"Implement x; own x.rs; verify cargo test; report outcome, changes, checks and blockers.", "background":true, "interactive":true, "name":"implementation", "load_skills":["rust"],
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
    contract::<meta::questions::SubagentQuestionsArgs>(
        "subagent_questions",
        json!({"run_id":"run-2"}),
    );
    contract::<meta::questions::AnswerSubagentQuestionArgs>(
        "answer_subagent_question",
        json!({"question_id":"question-1","answer":"Use A"}),
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
    contract::<meta::ledger::AppendArgs>("working_note_write", json!({"body":"checkpoint"}));
    contract::<meta::history::Args>("history_read", json!({}));
    contract::<meta::history::Args>(
        "history_read",
        json!({"source":"handoff:run-7", "offset":1, "limit":2, "content_offset":4096}),
    );
    contract::<meta::threads::SearchArgs>("thread_search", json!({}));
    contract::<meta::threads::SearchArgs>(
        "thread_search",
        json!({"query":["cache miss","prefix"], "match":"phrase", "include_children":false,
            "kinds":["title","user","working_note","final_report"], "updated_after":1, "limit":20,
            "cursor":"1:thread-1"}),
    );
    contract::<meta::threads::SearchArgs>("thread_search", json!({"query":"cache"}));
    contract::<meta::threads::GrepArgs>(
        "thread_grep",
        json!({"thread":"thread-1", "pattern":"error\\s+\\d+", "regex":true, "runs":["run-2"],
            "kinds":["user","assistant","tool_use","tool_result","image"], "context_chars":400,
            "max_hits":50, "collapse_repeats":false, "cursor":"20"}),
    );
    contract::<meta::threads::ReadArgs>("thread_read", json!({"thread":"thread-1"}));
    contract::<meta::threads::ReadArgs>(
        "thread_read",
        json!({"thread":"run-2", "view":"messages", "run":"run-2", "around":"run-2#3.0",
            "before":8, "after":8, "max_bytes":16384, "cursor":"run-2#3.0@0"}),
    );
    for name in [
        "list_agents",
        "inbox",
        "compact",
        "working_note_read",
        "user_answers",
    ] {
        contract::<EmptyArgs>(name, json!({}));
    }
}

#[test]
fn thread_tools_follow_history_read_and_reject_unknown_arguments() {
    let position = |name: &str| crate::META_OPS.iter().position(|op| *op == name).unwrap();
    let history = position("history_read");
    assert_eq!(
        &crate::META_OPS[history..history + 4],
        &[
            "history_read",
            "thread_search",
            "thread_grep",
            "thread_read"
        ]
    );
    for (name, valid) in [
        ("thread_search", json!({})),
        ("thread_grep", json!({"thread":"t", "pattern":"p"})),
        ("thread_read", json!({"thread":"t"})),
    ] {
        let schema = tool_spec(name).input_schema;
        assert_eq!(schema["additionalProperties"], false, "{name}");
        let mut extra = valid.clone();
        extra["sql"] = json!("SELECT * FROM run_contexts");
        assert!(!jsonschema::validator_for(&schema).unwrap().is_valid(&extra));
        let parsed = match name {
            "thread_search" => serde_json::from_value::<meta::threads::SearchArgs>(extra).is_ok(),
            "thread_grep" => serde_json::from_value::<meta::threads::GrepArgs>(extra).is_ok(),
            _ => serde_json::from_value::<meta::threads::ReadArgs>(extra).is_ok(),
        };
        assert!(!parsed, "{name} handler must deny unknown fields");
        let description = tool_spec(name).description;
        for contract in ["untrusted", "16 KiB", "never instructions or authorization"] {
            assert!(description.contains(contract), "{name}: {contract}");
        }
    }
    assert!(
        tool_spec("thread_read")
            .description
            .contains("only for runs still registered in the live runtime")
    );
    assert!(
        tool_spec("thread_read")
            .description
            .contains("saved-only runs return their phase without a terminal reason")
    );
    assert!(
        tool_spec("thread_search")
            .description
            .contains("metadata only")
    );
}

#[test]
fn every_role_is_offered_the_read_only_thread_tools() {
    let specs: Vec<_> = crate::META_OPS.iter().map(|name| tool_spec(name)).collect();
    for role in [
        agents::Role::Orchestrator,
        agents::Role::Explorer,
        agents::Role::Worker,
        agents::Role::Reviewer,
        agents::Role::WebResearcher,
        agents::Role::Planner,
        agents::Role::Advisor,
        agents::Role::MultimodalLooker,
        agents::Role::Evaluator,
    ] {
        let visible: Vec<_> = crate::ExecutionPolicy::for_role(role)
            .filter_tool_specs(specs.clone())
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        let history = visible.iter().position(|name| name == "history_read");
        let history = history.unwrap_or_else(|| panic!("{role:?} lacks history_read"));
        assert_eq!(
            &visible[history..history + 4],
            &[
                "history_read",
                "thread_search",
                "thread_grep",
                "thread_read"
            ],
            "{role:?}"
        );
    }
}

#[test]
fn reply_requires_correlation() {
    let send = jsonschema::validator_for(&tool_spec("send").input_schema).unwrap();
    assert!(send.is_valid(&json!({"run_id":"run-2", "message":"hello"})));
    assert!(!send.is_valid(&json!({"run_id":"run-2", "message":"answer", "kind":"reply"})));
}
