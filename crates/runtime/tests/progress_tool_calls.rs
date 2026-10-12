//! Identical calls that report progress are judged by their results, not their input.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use event_bus::{AgentRunPhase, EventBus, EventKind};
use providers::{ChatResponse, FinishReason};
use runtime::{AgentRuntime, Role, RunConfig};
use serde_json::{Value, json};
use support::{ScriptedModel, drain_events, text_response, tool_response};
use tools::{Permissions, Progress, Tool, ToolError, ToolExecutor, ToolResult};

/// Reports the scripted progress for each call; an `Err` entry is a failed
/// result that still claims progress.
struct Probe {
    results: Mutex<VecDeque<Result<Progress, Progress>>>,
    calls: Mutex<u32>,
}

#[async_trait::async_trait]
impl Tool for Probe {
    fn name(&self) -> &'static str {
        // A name the worker role can call.
        "read"
    }

    fn schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    fn reports_progress(&self, _: &Value) -> bool {
        true
    }

    async fn execute(&self, _: Value) -> Result<ToolResult, ToolError> {
        *self.calls.lock().unwrap() += 1;
        let next = self.results.lock().unwrap().pop_front();
        let (mut result, progress) = match next.unwrap_or(Ok(Progress::Stalled)) {
            Ok(progress) => (ToolResult::success("status"), progress),
            Err(progress) => (ToolResult::error("status"), progress),
        };
        result.progress = Some(progress);
        Ok(result)
    }
}

struct Outcome {
    phase: AgentRunPhase,
    requests: usize,
    probe_calls: u32,
    diagnostics: Vec<event_bus::DiagnosticEvent>,
}

async fn run(results: Vec<Result<Progress, Progress>>, script: Vec<ChatResponse>) -> Outcome {
    let bus = Arc::new(EventBus::new(1024));
    let mut events = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    let probe = Arc::new(Probe {
        results: Mutex::new(results.into()),
        calls: Mutex::new(0),
    });
    executor.register(probe.clone()).unwrap();
    let model = Arc::new(ScriptedModel::new(script.into_iter().map(Ok)));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone());
    let run = runtime.delegate_background(Role::Worker, "repeat".into(), RunConfig::default());
    let phase = runtime.wait(run).await.unwrap();
    let diagnostics = drain_events(&mut events)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Diagnostic(d) if d.code == "IdenticalToolCalls" => Some(d),
            _ => None,
        })
        .collect();
    let probe_calls = *probe.calls.lock().unwrap();
    Outcome {
        phase,
        requests: model.observed().await.len(),
        probe_calls,
        diagnostics,
    }
}

/// One call per turn with identical input: `read` is the probe, `grep` is an
/// unregistered call that does not report progress.
fn turns(names: &[&str]) -> Vec<ChatResponse> {
    names
        .iter()
        .enumerate()
        .map(|(id, name)| tool_response(&id.to_string(), name, json!({})))
        .chain([text_response("done", FinishReason::Stop)])
        .collect()
}

#[tokio::test]
async fn advancing_identical_calls_continue_past_the_repeat_limit() {
    // Given: twelve identical calls that each report progress.
    let outcome = run(vec![Ok(Progress::Advanced); 12], turns(&["read"; 12])).await;
    // Then: progress keeps them apart from a loop; every call runs.
    assert_eq!(outcome.phase, AgentRunPhase::Done);
    assert_eq!(outcome.probe_calls, 12);
    assert_eq!(outcome.requests, 13);
    assert!(outcome.diagnostics.is_empty());
}

#[tokio::test]
async fn stalled_identical_calls_stop_after_the_limit_call_runs() {
    // Given: progress resets the streak, then failures claiming progress and
    // stalls repeat without progress.
    let mut results = vec![Ok(Progress::Advanced); 3];
    results.push(Err(Progress::Advanced));
    results.extend(vec![Ok(Progress::Stalled); 9]);
    let outcome = run(results, turns(&["read"; 15])).await;
    // Then: the tenth call without progress runs, is recorded, and stops the run.
    assert_eq!(outcome.phase, AgentRunPhase::Error);
    assert_eq!(outcome.probe_calls, 13);
    assert_eq!(outcome.requests, 13);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics[0].call_id.as_deref(), Some("12"));
    assert!(outcome.diagnostics[0].detail.contains("without progress"));
}

#[tokio::test]
async fn each_kind_of_call_ends_the_other_kinds_streak() {
    // Given: identical stalled probes and identical calls that do not report
    // progress, each interrupted by the other kind before reaching ten.
    let mut names = vec!["grep"; 6];
    names.extend(["read"; 6]);
    names.push("grep");
    names.extend(["read"; 6]);
    names.extend(["grep"; 6]);
    let outcome = run(Vec::new(), turns(&names)).await;
    // Then: neither streak reaches the limit.
    assert_eq!(outcome.phase, AgentRunPhase::Done);
    assert_eq!(outcome.probe_calls, 12);
    assert!(outcome.diagnostics.is_empty());
}
