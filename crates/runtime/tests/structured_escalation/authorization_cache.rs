//! The new explicit critical route must leave the main agent's cache untouched.
use super::*;
use event_bus::{AgentRunPhase, EventKind, ProviderEvent};
use runtime::{AgentRuntime, RunConfig};
use std::sync::Arc;

#[tokio::test]
async fn explicit_shell_review_keeps_the_wire_prefix_and_reuses_the_input_cache() {
    const CRITICAL: &str = r#"{"approve":true,"reason":"explicit user permission","risk_level":"critical","authorization_level":"high","procedure_step":"none","authorization_basis":"explicit_permission","authorization_refs":[0],"scope_match":"full"}"#;
    let command = |cwd: &str| {
        json!({"command":"pwd", "cwd":cwd, "sandbox_access":"unsandboxed", "justification":"run the requested command"}).to_string()
    };
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::tool_call("one", MODEL, 0, "one", "shell", [command("a")]),
            ScriptedResponse::text_stream("review-one", MODEL, [CRITICAL]),
            ScriptedResponse::tool_call("two", MODEL, 0, "two", "shell", [command("b")]),
            ScriptedResponse::text_stream("review-two", MODEL, [CRITICAL]),
            ScriptedResponse::text_stream("done", MODEL, ["done"]),
        ],
        vec![MODEL.to_owned()],
    );
    let harness = harness(&mock.base_url(), false, Duration::from_secs(5));
    let bus = harness.bus.clone();
    let mut events = bus.subscribe();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("a")).unwrap();
    std::fs::create_dir(root.path().join("b")).unwrap();
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    executor.set_default_cwd(root.path().to_owned());
    let runtime = AgentRuntime::new(bus, executor.clone(), harness.model);
    runtime.set_sandbox_escalation(config::EscalationApproval::Auto, false);
    executor.set_shell_escalation(
        runtime.shell_escalation_gate(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    );
    let run = runtime.delegate_chat("explicit-review-cache", Role::Worker,
        "I explicitly authorize running pwd with host access in subdirectories a and b of this temporary workspace.".into(), RunConfig::default()).unwrap();
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    assert_eq!(mock.remaining_scripts(), 0);
    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect();
    assert_eq!(requests.len(), 5);
    let turns = [&requests[0], &requests[2], &requests[4]];
    for pair in turns.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, pair[0], pair[1]).unwrap();
        assert_eq!(pair[0]["tools"], pair[1]["tools"]);
        assert_eq!(pair[0]["messages"][0], pair[1]["messages"][0]);
    }
    let reviews = [&requests[1], &requests[3]];
    assert_eq!(reviews[0]["messages"][0], reviews[1]["messages"][0]);
    assert_eq!(reviews[0]["response_format"], reviews[1]["response_format"]);
    for (i, review) in reviews.iter().enumerate() {
        assert_schema(&review["response_format"]);
        let payload: Value =
            serde_json::from_str(review["messages"][1]["content"].as_str().unwrap()).unwrap();
        let cwd = root.path().join(if i == 0 { "a" } else { "b" });
        assert_eq!(payload["cwd"], json!(cwd));
        assert_eq!(payload["workspace_root"], json!(root.path()));
        assert_eq!(payload["access"], "host_unsandboxed");
        assert!(payload["context"]["project_rules"].is_null());
        assert!(payload["context"].get("loaded_skills").is_none());
        assert!(
            turns[i + 1]["messages"]
                .to_string()
                .contains(cwd.to_str().unwrap()),
            "the reviewed cwd is where the command actually ran"
        );
    }
    // These are derived from the real wire input, not canned cached-token fixtures.
    let mut usage = Vec::new();
    while usage.len() < 3 {
        if let EventKind::Provider(ProviderEvent::RequestCompleted {
            streaming: true,
            input_tokens,
            cache_read_tokens,
            ..
        }) = events.recv().await.unwrap().kind
        {
            usage.push((input_tokens, cache_read_tokens));
        }
    }
    for pair in usage.windows(2) {
        assert!(pair[0].0 > 0);
        assert_eq!(pair[1].1, pair[0].0);
    }
}
