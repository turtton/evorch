//! Identical calls that report progress are judged by their results, not their input.

use crate::support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
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
    /// The reason of the terminal `Error` transition.
    error_reason: Option<String>,
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
    let events = drain_events(&mut events).await;
    let error_reason = events.iter().find_map(|event| match &event.kind {
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            to: AgentRunPhase::Error,
            reason,
            ..
        }) => reason.clone(),
        _ => None,
    });
    let diagnostics = events
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
        error_reason,
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
    // The last stalled result succeeded, so the reason names only the tool.
    let reason = outcome.error_reason.unwrap();
    assert!(
        reason.starts_with("IdenticalToolCalls: tool=read; "),
        "{reason}"
    );
}

#[tokio::test]
async fn stalled_failing_calls_name_the_tool_and_error_class_in_the_reason() {
    // Given: identical calls that keep failing without progress.
    let outcome = run(vec![Err(Progress::Stalled); 10], turns(&["read"; 12])).await;
    // Then: the stop reason names the tool and the class of its last failure.
    assert_eq!(outcome.phase, AgentRunPhase::Error);
    assert_eq!(outcome.probe_calls, 10);
    let reason = outcome.error_reason.unwrap();
    assert!(
        reason.starts_with("IdenticalToolCalls: tool=read last_error=other; "),
        "{reason}"
    );
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

// Real SQLite reports are changed at provider request boundaries, never by time.
#[derive(Clone, Copy)]
enum LaneScript {
    Artifacts,
    Noise,
    Oscillation,
    Pages,
    FailThenSuccess,
    Failures,
    EmptyPages,
    StorageFailures,
    StorageThenSuccess,
}
struct LaneModel {
    store: storage::StorageHandle,
    step: Mutex<usize>,
    script: LaneScript,
    database: std::path::PathBuf,
    brief_json: String,
    seen: Mutex<Vec<Value>>,
}
#[async_trait::async_trait]
impl runtime::AgentModel for LaneModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "lane-script".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<ChatResponse, runtime::RuntimeError> {
        let mut step = self.step.lock().unwrap();
        let n = *step;
        *step += 1;
        for block in messages.last().into_iter().flat_map(|m| &m.content) {
            if let providers::ContentBlock::ToolResult { content, .. } = block {
                for part in content {
                    if let providers::ToolResultContent::Text { text } = part
                        && let Ok(value) = serde_json::from_str::<Value>(text)
                    {
                        self.seen.lock().unwrap().push(value);
                    }
                }
            }
        }
        if n == 14 {
            return Ok(text_response("done", FinishReason::Stop));
        }
        if matches!(
            self.script,
            LaneScript::StorageFailures | LaneScript::StorageThenSuccess
        ) {
            rusqlite::Connection::open(&self.database)
                .unwrap()
                .execute(
                    "UPDATE lanes SET brief_json=?1 WHERE lane_id='observed-lane-0'",
                    [&self.brief_json],
                )
                .unwrap();
        }
        if !matches!(self.script, LaneScript::Pages) {
            let status = if matches!(self.script, LaneScript::Oscillation) && !n.is_multiple_of(2) {
                event_bus::LaneStatus::Waiting
            } else {
                event_bus::LaneStatus::Working
            };
            self.store
                .append_lane_report(
                    "observed-lane-0",
                    1,
                    event_bus::LaneReportInput {
                        status,
                        summary: format!("summary {n}"),
                        artifact_refs: vec![format!("artifact:{n}")],
                        artifact_revision: Some(event_bus::lane::ArtifactRevision::Sha256(
                            format!(
                                "{:064x}",
                                if matches!(
                                    self.script,
                                    LaneScript::Artifacts
                                        | LaneScript::FailThenSuccess
                                        | LaneScript::StorageThenSuccess
                                ) {
                                    n
                                } else {
                                    0
                                }
                            ),
                        )),
                        needs_human: false,
                        question_refs: vec![],
                        reason_code: None,
                        failure_episode: None,
                        producer: event_bus::LaneReportProducer::LaneAgent,
                    },
                    n as i64 + 20,
                )
                .unwrap();
        }
        if matches!(self.script, LaneScript::StorageFailures)
            || (matches!(self.script, LaneScript::StorageThenSuccess) && n < 5)
        {
            // Inject a row-decoding failure, not a fake progress flag. The next
            // request repairs the same row before deciding whether to fail again.
            rusqlite::Connection::open(&self.database)
                .unwrap()
                .execute(
                    "UPDATE lanes SET brief_json='{invalid' WHERE lane_id='observed-lane-0'",
                    [],
                )
                .unwrap();
        }
        let input = match self.script {
            LaneScript::Noise => {
                if n.is_multiple_of(2) {
                    json!({"limit": n % 18 + 3, "filter":"attached"})
                } else {
                    json!({"limit": n % 18 + 3})
                }
            }
            LaneScript::FailThenSuccess if n < 5 => json!({"cursor":"invalid"}),
            LaneScript::Failures => json!({"cursor":format!("invalid-{n}")}),
            LaneScript::EmptyPages => json!({"filter":"summary","limit":n+1}),
            LaneScript::Pages => {
                let seen = self.seen.lock().unwrap();
                if n == 0 {
                    json!({"limit":1})
                } else if n < 3 {
                    json!({"limit":1,"cursor": seen.last().unwrap()["next_cursor"]})
                } else {
                    json!({"limit": if n.is_multiple_of(2) { 20 } else { 1 }})
                }
            }
            _ => json!({}),
        };
        Ok(tool_response(&format!("lane-{n}"), "lane_list", input))
    }
}
async fn lane_script(script: LaneScript) -> (AgentRunPhase, usize, Vec<Value>) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("lanes.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let handle = db.handle();
    for n in 0..3 {
        handle.create_lane(storage::lanes::NewLane {
            lane_id: format!("observed-lane-{n}"), thread_id: format!("observed-thread-{n}"),
            origin_thread_id: "observing-lead".into(), origin_run_id: "origin-run".into(),
            creation_key: format!("create-{n}"),
            brief: serde_json::from_value(json!({"objective":"observe", "inputs":[],"deliverables":[],
                "acceptance_criteria":["observe"],"constraints":[],"workspace":{},"report_policy":"attached"})).unwrap(),
            start_seed: "seed".into(), project_id: None, project_root: None, title: None, at_ns: n,
        }).unwrap();
    }
    let model = Arc::new(LaneModel {
        store: handle.clone(),
        step: Mutex::new(0),
        script,
        database: config.db_path.clone(),
        brief_json: serde_json::to_string(&handle.lane("observed-lane-0").unwrap().unwrap().brief)
            .unwrap(),
        seen: Mutex::new(vec![]),
    });
    let bus = Arc::new(EventBus::new(1024));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(runtime::RunStore::open(&config, handle).unwrap());
    let run = runtime
        .delegate_chat(
            "observing-lead",
            Role::Orchestrator,
            "observe".into(),
            RunConfig::default(),
        )
        .unwrap();
    let phase = runtime.wait(run).await.unwrap();
    let count = *model.step.lock().unwrap();
    let seen = model.seen.lock().unwrap().clone();
    (phase, count, seen)
}
#[tokio::test]
async fn lane_list_new_artifacts_with_identical_arguments_continue() {
    let (phase, count, _) = lane_script(LaneScript::Artifacts).await;
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(count, 15);
}
#[tokio::test]
async fn lane_list_noise_and_argument_changes_cannot_avoid_stopping() {
    let (phase, count, _) = lane_script(LaneScript::Noise).await;
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(count, 11); // initial observation + ten stalls
}
#[tokio::test]
async fn lane_list_previously_seen_statuses_do_not_reset_stalls() {
    let (phase, count, _) = lane_script(LaneScript::Oscillation).await;
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(count, 12); // two distinct states + ten stalls
}
#[tokio::test]
async fn lane_list_next_pages_advance_but_repartitioning_does_not() {
    let (phase, count, seen) = lane_script(LaneScript::Pages).await;
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(count, 13); // three distinct lanes + ten stalls
    let ids: Vec<_> = seen[..3]
        .iter()
        .map(|v| v["items"][0]["lane_id"].clone())
        .collect();
    assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);
}
#[tokio::test]
async fn lane_list_success_after_failures_is_reevaluated() {
    let (phase, count, _) = lane_script(LaneScript::FailThenSuccess).await;
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(count, 15);
}

#[tokio::test]
async fn lane_list_failures_and_empty_pages_stop_even_with_argument_changes() {
    for script in [LaneScript::Failures, LaneScript::EmptyPages] {
        let (phase, count, _) = lane_script(script).await;
        assert_eq!(phase, AgentRunPhase::Error);
        assert_eq!(count, 10);
    }
}

#[tokio::test]
async fn lane_list_storage_failures_do_not_observe_and_repair_is_reevaluated() {
    let (phase, count, seen) = lane_script(LaneScript::StorageFailures).await;
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(count, 10);
    assert!(seen.iter().all(|v| v.get("items").is_none()));
    let (phase, count, seen) = lane_script(LaneScript::StorageThenSuccess).await;
    assert_eq!(phase, AgentRunPhase::Done);
    assert_eq!(count, 15);
    assert!(seen[..5].iter().all(|v| v.get("items").is_none()));
    assert!(seen[5..].iter().all(|v| v.get("items").is_some()));
}

/// Real file tools; the injected run_js worker only speaks the host protocol.
async fn real_reads(
    script: Vec<ChatResponse>,
    child_calls: Option<Vec<(&str, Value)>>,
) -> (AgentRunPhase, Vec<event_bus::Event>) {
    let bus = Arc::new(EventBus::new(4096));
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    );
    if let Some(calls) = child_calls {
        let mut program = "IFS= read -r run\n".to_owned();
        for (index, (name, args)) in calls.into_iter().enumerate() {
            let packet = json!({"type":"tool_call", "id":index+1, "name":name, "args":args});
            program.push_str(&format!(
                "printf '%s\\n' '{}'; IFS= read -r reply\n",
                packet.to_string().replace('\'', "'\\''"),
            ));
        }
        program.push_str("printf '%s\\n' '{\"type\":\"done\",\"value\":\"done\"}'\n");
        let launcher = tools::tools::run_js::WorkerLauncher::injected(
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
            "sh".into(),
            vec!["-c".into(), program],
        );
        executor
            .register(Arc::new(tools::RunJs::with_launcher(launcher)))
            .unwrap();
    }
    let model = Arc::new(ScriptedModel::new(script.into_iter().map(Ok)));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model);
    runtime.set_run_js_settings(config::RunJsConfig {
        enabled: true,
        scopes: vec!["worker".into()],
        ..Default::default()
    });
    let run = runtime.delegate_background(Role::Worker, "read files".into(), RunConfig::default());
    let phase = runtime.wait(run).await.unwrap();
    (phase, drain_events(&mut receiver).await)
}

fn reread_stopped(phase: AgentRunPhase, events: &[event_bus::Event]) {
    assert_eq!(phase, AgentRunPhase::Error);
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Diagnostic(d)
        if d.detail.contains("max_file_rereads=20") && d.detail.contains("action=stop_run"))),
        "missing reread diagnostic: {events:?}"
    );
}

fn read_script(path: &std::path::Path, args: impl Fn(u64) -> Value) -> Vec<ChatResponse> {
    (0..24)
        .map(|n| {
            let mut input = args(n);
            input["path"] = json!(path);
            tool_response(&format!("read-{n}"), "read", input)
        })
        .chain([text_response("done", FinishReason::Stop)])
        .collect()
}

#[tokio::test]
async fn real_image_reads_ignore_ineffective_range_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pixel.png");
    image::RgbaImage::new(1, 1).save(&path).unwrap();
    let script = read_script(&path, |n| json!({"offset":n+1,"limit":n+1,"byte_offset":n}));
    let (phase, events) = real_reads(script, None).await;
    reread_stopped(phase, &events);
}

#[tokio::test]
async fn real_empty_eof_reads_ignore_ineffective_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("small.txt");
    std::fs::write(&path, "row\n").unwrap();
    let script = read_script(&path, |n| json!({"offset":n+100,"limit":n+1}));
    let (phase, events) = real_reads(script, None).await;
    reread_stopped(phase, &events);
}

#[tokio::test]
async fn real_truncated_reads_ignore_limits_that_return_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.txt");
    std::fs::write(&path, "x".repeat(20_000)).unwrap();
    let script = read_script(&path, |n| json!({"limit":n+1}));
    let (phase, events) = real_reads(script, None).await;
    reread_stopped(phase, &events);
}

#[tokio::test]
async fn limited_read_artifacts_do_not_create_new_observations() {
    // A valid long path makes the read footer exceed the generic output ceiling.
    // The limiter replaces the read metadata and generates a new artifact per call.
    let dir = tempfile::tempdir().unwrap();
    let mut path = dir.path().to_path_buf();
    for _ in 0..21 {
        path.push("p".repeat(120));
    }
    std::fs::create_dir_all(&path).unwrap();
    path.push("long.txt");
    assert!((2500..3000).contains(&path.as_os_str().len()));
    std::fs::write(&path, "x".repeat(20_000)).unwrap();
    let script = read_script(&path, |n| json!({"limit":n+1}));
    let (phase, events) = real_reads(script, None).await;
    let artifacts: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                tool_name,
                detail: Some(detail),
                is_error: false,
                ..
            }) if tool_name == "read" => {
                assert!(detail.get("bytes_returned").is_none());
                Some(detail["output_artifact"]["path"].as_str().unwrap())
            }
            _ => None,
        })
        .collect();
    assert!(artifacts.len() > 20);
    assert_eq!(
        artifacts
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        artifacts.len(),
        "limiter artifacts must be distinct for this regression"
    );
    reread_stopped(phase, &events);
    assert_eq!(artifacts.len(), 22); // first read + 20 permitted rereads + breach
}

#[tokio::test]
async fn real_distinct_file_ranges_are_not_rereads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rows.txt");
    std::fs::write(&path, "row\n".repeat(24)).unwrap();
    let script = read_script(&path, |n| json!({"offset":n+1,"limit":1}));
    let (phase, _) = real_reads(script, None).await;
    assert_eq!(phase, AgentRunPhase::Done);
}

#[tokio::test]
async fn run_js_read_grep_alternation_does_not_forgive_rereads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unchanged.txt");
    std::fs::write(&path, "row\n").unwrap();
    let children = (0..24)
        .flat_map(|_| {
            [
                ("read", json!({"path":path})),
                ("grep", json!({"path":path,"pattern":"row"})),
            ]
        })
        .collect();
    let (phase, events) = real_reads(
        vec![
            tool_response("program", "run_js", json!({"code":"/* injected worker */"})),
            text_response("done", FinishReason::Stop),
        ],
        Some(children),
    )
    .await;
    reread_stopped(phase, &events);
}

#[tokio::test]
async fn direct_and_run_js_reads_share_the_reread_allowance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.txt");
    std::fs::write(&path, "row\n").unwrap();
    let script = (0..24)
        .map(|n| {
            if n % 2 == 0 {
                tool_response(&format!("direct-{n}"), "read", json!({"path":path}))
            } else {
                tool_response(
                    &format!("program-{n}"),
                    "run_js",
                    json!({"code":"/* injected worker */"}),
                )
            }
        })
        .chain([text_response("done", FinishReason::Stop)])
        .collect();
    let (phase, events) = real_reads(script, Some(vec![("read", json!({"path":path}))])).await;
    reread_stopped(phase, &events);
    let reads = events.iter().filter(|e| matches!(&e.kind, EventKind::Tool(event_bus::ToolEvent::ToolStarted{tool_name,..}) if tool_name=="read")).count();
    assert_eq!(reads, 22); // first observation + 20 permitted rereads + breach
}
