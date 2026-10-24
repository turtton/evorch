mod support;

use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use serde_json::json;
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
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(dir.path().join("empty-user-config")),
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

/// The reason of the run's terminal `Error` transition.
fn error_reason(events: &[event_bus::Event]) -> String {
    events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Error,
                reason,
                ..
            }) => reason.clone(),
            _ => None,
        })
        .expect("the run ends in Error with a reason")
}

/// Marker inside the rejected arguments; it must never reach a stop reason.
const INPUT_MARKER: &str = "SECRET-EDIT-INPUT-MARKER";

/// An edit that schema validation rejects as InvalidArgs before it runs.
fn invalid_edit(id: usize) -> ChatResponse {
    tool_response(
        &id.to_string(),
        "edit",
        json!({
            "path": format!("/{INPUT_MARKER}/note.md"),
            "old_string": INPUT_MARKER,
            "new_string": INPUT_MARKER,
            "replace_all": "yes",
        }),
    )
}

fn assert_names_the_failing_edit(reason: &str, code: &str) {
    // The code stays first, the tool and failure class follow right after it, and no
    // input value is copied into the reason.
    assert!(
        reason.starts_with(&format!("{code}: tool=edit last_error=invalid_args")),
        "{reason}"
    );
    assert!(!reason.contains(INPUT_MARKER), "{reason}");
    assert!(!reason.contains("replace_all"), "{reason}");
}

#[tokio::test]
async fn identical_invalid_edits_name_the_tool_and_error_class_in_the_reason() {
    // Given: a model that repeats one edit that fails with InvalidArgs.
    let script = (0..11)
        .map(invalid_edit)
        .chain([text_response("done", FinishReason::Stop)])
        .collect();
    // When: the identical-call guard stops the run.
    let (phase, events, _) = run_script(script, config::Config::default()).await;
    // Then: the reason says which tool repeated and how it failed.
    assert_eq!(phase, AgentRunPhase::Error);
    assert_names_the_failing_edit(&error_reason(&events), "IdenticalToolCalls");
}

#[tokio::test]
async fn no_progress_names_the_last_tool_and_error_class_in_the_reason() {
    // Given: a no-progress limit reached long before the identical-call limit.
    let config = config::Config {
        budget: config::BudgetConfig {
            max_no_progress_rounds: 2,
            max_identical_tool_call_repeats: 100,
            ..Default::default()
        },
        ..Default::default()
    };
    let script = (0..6)
        .map(invalid_edit)
        .chain([text_response("done", FinishReason::Stop)])
        .collect();
    // When: failing edits never make progress.
    let (phase, events, _) = run_script(script, config).await;
    // Then: the NoProgress reason names the last tool and its failure class.
    assert_eq!(phase, AgentRunPhase::Error);
    assert_names_the_failing_edit(&error_reason(&events), "NoProgress");
}

#[tokio::test]
async fn identical_successful_calls_name_the_tool_without_an_error_class() {
    // Given: a model that keeps re-reading one existing file successfully.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("present.txt");
    std::fs::write(&path, "present\n").unwrap();
    let script = (0..11)
        .map(|id| tool_response(&id.to_string(), "read", json!({"path": path})))
        .chain([text_response("done", FinishReason::Stop)])
        .collect();
    // When: the identical-call guard stops the run.
    let (phase, events, _) = run_script(script, config::Config::default()).await;
    // Then: the tool is named, but a successful last result adds no error class.
    assert_eq!(phase, AgentRunPhase::Error);
    let reason = error_reason(&events);
    assert!(
        reason.starts_with("IdenticalToolCalls: tool=read; "),
        "{reason}"
    );
    assert!(!reason.contains("last_error"), "{reason}");
}

#[tokio::test]
async fn the_parent_sees_the_tool_and_error_class_of_a_child_stopped_by_repeats() {
    // Given: an Orchestrator that waits for a Worker repeating one invalid edit.
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "delegate",
                    "delegate",
                    json!({"target": {"role": "worker"}, "background": true, "prompt": "CHILD"}),
                )),
                Ok(tool_response(
                    "wait",
                    "wait",
                    json!({"run_ids": ["run-2"], "mode": "all"}),
                )),
                Ok(tool_response("finish", "finish", json!({"result": "done"}))),
            ],
        )
        .await;
    model
        .add_keyed("CHILD", (0..11).map(invalid_edit).map(Ok))
        .await;
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model.clone()).with_sequential_run_ids();
    // When: the child stops and the parent's wait returns its terminal snapshot.
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    let phase = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.wait(parent))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(phase, AgentRunPhase::Done);
    let wait_result = model
        .observed()
        .await
        .iter()
        .flat_map(|messages| messages.iter())
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                ..
            } if tool_call_id == "wait" => match content.first() {
                Some(ToolResultContent::Text { text }) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the wait result reaches the parent model");
    // Then: the reason the parent reads names the repeated tool and its failure class.
    let snapshot: serde_json::Value = serde_json::from_str(&wait_result).unwrap();
    let child = &snapshot["runs"][0];
    assert_eq!(child["status"], "failed", "{snapshot}");
    assert_eq!(child["reason_truncated"], false, "{snapshot}");
    assert_names_the_failing_edit(child["reason"].as_str().unwrap(), "IdenticalToolCalls");
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
async fn stops_at_ten_identical_calls_with_different_ids() {
    // Given: identical calls with unique IDs across turns.
    let script = calls(&[1; 11]);
    // When: the real agent loop consumes the script.
    let (phase, events, requests) = run_script(script, config::Config::default()).await;
    // Then: the tenth call stops the run with one correlated diagnostic.
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(requests, 10);
    let diagnostics: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic(d) if d.code == "IdenticalToolCalls" => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].call_id.as_deref(), Some("9"));
    assert_eq!(
        diagnostics[0].severity,
        event_bus::DiagnosticSeverity::Error
    );
    assert!(diagnostics[0].run_id.is_some());
    // The detail names the repeated tool and a stable input digest, never the input.
    let detail = &diagnostics[0].detail;
    let line = detail.lines().nth(1).unwrap();
    assert!(line.starts_with("tool=read input_sha256="), "{detail}");
    assert_eq!(line.rsplit('=').next().unwrap().len(), 16);
    assert!(!detail.contains("missing-repeat-fixture"));
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
    // Given: ten identical pairs followed by a different pair in one batch.
    let mut script = calls(&[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2]);
    let content = script.drain(..11).flat_map(|r| r.message.content).collect();
    let mut batch = text_response("", FinishReason::ToolUse);
    batch.message.content = content;
    script.insert(0, batch);
    // When: the batch is inspected.
    let (phase, events, requests) = run_script(script, config::Config::default()).await;
    // Then: the maximum streak is detected before the differing tail resets it.
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(requests, 1);
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code == "IdenticalToolCalls" && d.call_id.as_deref() == Some("9"))));
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
