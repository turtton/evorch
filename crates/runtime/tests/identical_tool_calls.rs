mod support;

use event_bus::{AgentRunPhase, EventKind};
use providers::{ChatResponse, FinishReason};
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use std::sync::Arc;
use support::{ScriptedModel, drain_events, text_response, tool_response};

async fn run_script(
    script: Vec<ChatResponse>,
    config: config::Config,
) -> (AgentRunPhase, Vec<event_bus::Event>, usize) {
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(script.into_iter().map(Ok)));
    let composed = compose_runtime(RuntimeComposition {
        config: &config,
        executor: Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        bus,
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path()).unwrap(),
        ),
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(model.clone()),
        workspace: None,
    })
    .unwrap();
    let run = composed.runtime.delegate_background(
        Role::Worker,
        "repeat guard".into(),
        RunConfig::default(),
    );
    let phase = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        composed.runtime.wait(run),
    )
    .await
    .unwrap()
    .unwrap();
    (
        phase,
        drain_events(&mut receiver).await,
        model.observed().await.len(),
    )
}

fn calls(inputs: &[u32]) -> Vec<ChatResponse> {
    inputs
        .iter()
        .enumerate()
        .map(|(id, input)| {
            tool_response(
                &id.to_string(),
                "read",
                serde_json::json!({"path": format!("/missing-repeat-fixture-{input}")}),
            )
        })
        .chain([text_response("done", FinishReason::Stop)])
        .collect()
}

#[tokio::test]
async fn stops_at_five_identical_calls_with_different_ids() {
    // Given: identical calls with unique IDs across turns.
    let script = calls(&[1; 6]);
    // When: the real agent loop consumes the script.
    let (phase, events, requests) = run_script(script, config::Config::default()).await;
    // Then: the fifth call stops the run with one correlated diagnostic.
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(requests, 5);
    let diagnostics: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic(d) if d.code == "IdenticalToolCalls" => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].call_id.as_deref(), Some("4"));
    assert_eq!(
        diagnostics[0].severity,
        event_bus::DiagnosticSeverity::Error
    );
    assert!(diagnostics[0].run_id.is_some());
}

#[tokio::test]
async fn continues_when_arguments_differ() {
    // Given: one tool name but a different argument on every call.
    let script = calls(&[1, 2, 3, 4, 5, 6]);
    // When: the script completes.
    let (phase, events, requests) = run_script(script, config::Config::default()).await;
    // Then: every call runs without a repeat diagnostic.
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(requests, 7);
    assert!(
        !events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code == "IdenticalToolCalls"))
    );
}

#[tokio::test]
async fn configured_limit_stops_at_two() {
    // Given: a non-default threshold provided through the root configuration.
    let config = config::Config {
        budget: config::BudgetConfig {
            max_identical_tool_call_repeats: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    // When: the composed runtime receives identical calls.
    let (phase, events, requests) = run_script(calls(&[1; 3]), config).await;
    // Then: configuration reaches the loop and stops at exactly two.
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(requests, 2);
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code == "IdenticalToolCalls" && d.call_id.as_deref() == Some("1"))));
}

#[tokio::test]
async fn stops_on_streak_inside_batch_even_when_tail_differs() {
    // Given: five identical pairs followed by a different pair in one batch.
    let mut script = calls(&[1, 1, 1, 1, 1, 2]);
    let content = script.drain(..6).flat_map(|r| r.message.content).collect();
    let mut batch = text_response("", FinishReason::ToolUse);
    batch.message.content = content;
    script.insert(0, batch);
    // When: the batch is inspected.
    let (phase, events, requests) = run_script(script, config::Config::default()).await;
    // Then: the maximum streak is detected before the differing tail resets it.
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(requests, 1);
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code == "IdenticalToolCalls" && d.call_id.as_deref() == Some("4"))));
}

#[tokio::test]
async fn resets_when_assistant_turn_has_no_tools() {
    // Given: two four-call streaks separated by an empty tool-use turn.
    let mut script = calls(&[1; 8]);
    script.insert(4, text_response("thinking", FinishReason::ToolUse));
    // When: the loop observes the empty turn.
    let (phase, _, requests) = run_script(script, config::Config::default()).await;
    // Then: neither streak reaches five.
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(requests, 10);
}

#[tokio::test]
async fn resets_when_tool_name_differs() {
    // Given: identical arguments but a different tool name between streaks.
    let mut script = calls(&[1; 9]);
    script[4].message.content = tool_response(
        "4",
        "other",
        serde_json::json!({"path": "/missing-repeat-fixture-1"}),
    )
    .message
    .content;
    // When: the script completes.
    let (phase, _, requests) = run_script(script, config::Config::default()).await;
    // Then: different names reset the streak even with equal arguments.
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(requests, 10);
}
