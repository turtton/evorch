//! `run_js` through the agent loop (ADR 0039): the calls a program makes are
//! ordinary tool calls bound by the same role limits, and the model gets only the
//! program's result.
//!
//! The worker is a `sh` script that speaks the JSON Lines protocol, so the tests
//! need neither the evorch binary nor bubblewrap.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, ToolEvent};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig};
use sandbox::{ApprovalMode, ApprovalPolicy, DirectSandbox, PolicyDecision};
use serde_json::json;
use support::{ScriptedModel, drain_events, text_response, tool_response};
use tools::tools::run_js::WorkerLauncher;
use tools::{RunJs, ToolExecutor};

/// Writes `pid` to `pid_file`-less workers; the script receives the `run` line in `$run`.
fn worker(script: &str) -> WorkerLauncher {
    WorkerLauncher::injected(
        Arc::new(DirectSandbox::new_unchecked()),
        PathBuf::from("sh"),
        vec!["-c".into(), format!("read -r run\n{script}")],
    )
}

/// A `done` message whose value is the text of `$reply`, escaped for JSON.
const DONE_WITH_REPLY: &str = r#"esc=$(printf '%s' "$reply" | sed 's/\\/\\\\/g; s/"/\\"/g')
printf '{"type":"done","value":"%s"}\n' "$esc""#;

struct Fixture {
    runtime: AgentRuntime,
    bus: Arc<EventBus>,
    model: Arc<ScriptedModel>,
}

fn fixture(
    launcher: WorkerLauncher,
    policy: Option<ApprovalPolicy>,
    settings: config::RunJsConfig,
    responses: Vec<providers::ChatResponse>,
) -> Fixture {
    let bus = Arc::new(EventBus::new(256));
    let mut executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    executor
        .register(Arc::new(RunJs::with_launcher(launcher)))
        .unwrap();
    if let Some(policy) = policy {
        executor.set_policy(policy);
    }
    let model = Arc::new(ScriptedModel::new(responses.into_iter().map(Ok)));
    let runtime = AgentRuntime::new(Arc::clone(&bus), Arc::new(executor), model.clone());
    runtime.set_run_js_settings(settings);
    Fixture {
        runtime,
        bus,
        model,
    }
}

fn enabled() -> config::RunJsConfig {
    config::RunJsConfig {
        enabled: true,
        ..config::RunJsConfig::default()
    }
}

fn run_js_call(id: &str) -> providers::ChatResponse {
    tool_response(
        id,
        "run_js",
        json!({"code": "/* the script is the worker */"}),
    )
}

async fn run(fixture: &Fixture, role: Role) -> Vec<Event> {
    let mut events = fixture.bus.subscribe();
    let run = fixture
        .runtime
        .delegate_background(role, "task".into(), RunConfig::default());
    assert_eq!(fixture.runtime.wait(run).await, Ok(AgentRunPhase::Done));
    drain_events(&mut events).await
}

fn started<'a>(events: &'a [Event], name: &str) -> Vec<(&'a str, Option<&'a str>)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(ToolEvent::ToolStarted {
                tool_name,
                call_id,
                parent_call_id,
                ..
            }) if tool_name == name => Some((call_id.as_str(), parent_call_id.as_deref())),
            _ => None,
        })
        .collect()
}

/// Text of the tool result for `call_id` in the model's last request.
async fn result_text(model: &ScriptedModel, call_id: &str) -> String {
    let observed = model.observed().await;
    let last = observed.last().expect("a request");
    last.iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                ..
            } if tool_call_id == call_id => Some(
                content
                    .iter()
                    .filter_map(|part| match part {
                        ToolResultContent::Text { text } => Some(text.as_str()),
                        ToolResultContent::Image { .. } => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {call_id}"))
}

fn finish() -> providers::ChatResponse {
    text_response("done", FinishReason::Stop)
}

fn write_file(directory: &Path, name: &str, text: &str) -> String {
    let path = directory.join(name);
    std::fs::write(&path, text).unwrap();
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn explorer_cannot_call_edit_and_nothing_is_started() {
    let script = format!(
        r#"printf '%s\n' '{{"type":"tool_call","id":1,"name":"edit","args":{{"path":"/x","old_string":"a","new_string":"b"}}}}'
read -r reply
{DONE_WITH_REPLY}"#
    );
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );

    let events = run(&fixture, Role::Explorer).await;

    assert!(started(&events, "edit").is_empty());
    assert_eq!(started(&events, "run_js").len(), 1);
    let text = result_text(&fixture.model, "js-1").await;
    assert!(text.contains("tools.edit is not available"), "{text}");
    assert!(text.ends_with("[run_js: 1 calls, 1 failed]"), "{text}");
}

#[tokio::test]
async fn parallel_reads_are_child_calls_and_their_text_stays_out_of_the_result() {
    let directory = tempfile::tempdir().unwrap();
    let first = write_file(directory.path(), "a.txt", "SECRET-CONTENT-A\n");
    let second = write_file(directory.path(), "b.txt", "SECRET-CONTENT-B\n");
    let script = format!(
        r#"printf '%s\n' '{{"type":"tool_call","id":1,"name":"read","args":{{"path":"{first}"}}}}'
printf '%s\n' '{{"type":"tool_call","id":2,"name":"read","args":{{"path":"{second}"}}}}'
read -r one
read -r two
printf '%s\n' '{{"type":"output","text":"both read\n"}}'
printf '%s\n' '{{"type":"done","value":{{"files":2}}}}'"#
    );
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );

    let events = run(&fixture, Role::Worker).await;

    let mut children = started(&events, "read");
    children.sort_unstable();
    assert_eq!(
        children,
        [("js-1/1", Some("js-1")), ("js-1/2", Some("js-1"))]
    );
    for call in ["js-1/1", "js-1/2"] {
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Tool(ToolEvent::ToolCompleted { call_id, parent_call_id, is_error: false, .. })
                if call_id == call && parent_call_id.as_deref() == Some("js-1")
        )));
    }
    let parent = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Tool(ToolEvent::ToolCompleted {
                call_id, detail, ..
            }) if call_id == "js-1" => detail.clone(),
            _ => None,
        })
        .expect("run_js completed with a detail");
    assert_eq!(parent["run_js"]["calls"].as_array().unwrap().len(), 2);
    assert_eq!(parent["run_js"]["calls"][0]["name"], "read");

    let text = result_text(&fixture.model, "js-1").await;
    assert_eq!(
        text,
        "both read\n=> {\"files\":2}\n[run_js: 2 calls, 0 failed]"
    );
    assert!(!text.contains("SECRET-CONTENT"));
}

#[tokio::test]
async fn shell_with_network_access_is_refused_without_running() {
    let script = format!(
        r#"printf '%s\n' '{{"type":"tool_call","id":1,"name":"shell","args":{{"command":"echo hi","sandbox_access":"network","justification":"x"}}}}'
read -r reply
{DONE_WITH_REPLY}"#
    );
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );

    let events = run(&fixture, Role::Worker).await;

    assert!(started(&events, "shell").is_empty());
    let text = result_text(&fixture.model, "js-1").await;
    assert!(
        text.contains("Make this call as a normal tool call"),
        "{text}"
    );
}

#[tokio::test]
async fn a_call_that_would_need_approval_fails_without_asking() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory
        .path()
        .join("new.txt")
        .to_string_lossy()
        .into_owned();
    let script = format!(
        r#"printf '%s\n' '{{"type":"tool_call","id":1,"name":"write","args":{{"path":"{target}","content":"x"}}}}'
read -r reply
{DONE_WITH_REPLY}"#
    );
    let policy = ApprovalPolicy::standard(ApprovalMode::OnRequest)
        .with_override("run_js", PolicyDecision::AutoAllow);
    let fixture = fixture(
        worker(&script),
        Some(policy),
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );

    let events = run(&fixture, Role::Worker).await;

    assert!(started(&events, "write").is_empty());
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ApprovalRequested { .. })
    )));
    assert!(!Path::new(&target).exists());
    let text = result_text(&fixture.model, "js-1").await;
    assert!(
        text.contains("Make this call as a normal tool call"),
        "{text}"
    );
}

#[tokio::test]
async fn disabled_run_js_is_hidden_and_rejected() {
    let fixture = fixture(
        worker("exit 0"),
        None,
        config::RunJsConfig::default(),
        vec![run_js_call("js-1"), finish()],
    );

    let events = run(&fixture, Role::Worker).await;

    assert!(started(&events, "run_js").is_empty());
    let offered = fixture.model.observed_tools().await;
    assert!(
        offered
            .iter()
            .flatten()
            .all(|spec| spec.name != "run_js" && !spec.description.contains("run_js")),
        "a disabled run_js leaves every tool definition untouched"
    );
    let text = result_text(&fixture.model, "js-1").await;
    assert!(text.contains("run_js is not enabled"), "{text}");
}

#[tokio::test]
async fn enabled_run_js_is_offered_with_generated_declarations() {
    let fixture = fixture(worker("exit 0"), None, enabled(), vec![finish()]);

    run(&fixture, Role::Worker).await;

    let offered = fixture.model.observed_tools().await;
    let run_js = offered[0]
        .iter()
        .find(|spec| spec.name == "run_js")
        .expect("run_js is offered");
    assert!(run_js.description.contains("shell(args: { command: string"));
    let shell = offered[0].iter().find(|spec| spec.name == "shell").unwrap();
    assert!(shell.description.contains("use run_js instead"));

    // Roles without the capability never see it.
    let orchestrator = self::fixture(worker("exit 0"), None, enabled(), vec![finish()]);
    run(&orchestrator, Role::Orchestrator).await;
    assert!(
        orchestrator.model.observed_tools().await[0]
            .iter()
            .all(|spec| spec.name != "run_js")
    );
}

#[tokio::test]
async fn calls_beyond_the_limit_fail_and_are_reported() {
    let script = format!(
        r#"printf '%s\n' '{{"type":"tool_call","id":1,"name":"grep","args":{{"pattern":"x","path":"/nonexistent-run-js-test"}}}}'
read -r first
printf '%s\n' '{{"type":"tool_call","id":2,"name":"grep","args":{{"pattern":"x","path":"/nonexistent-run-js-test"}}}}'
read -r reply
{DONE_WITH_REPLY}"#
    );
    let settings = config::RunJsConfig {
        max_tool_calls: 1,
        ..enabled()
    };
    let fixture = fixture(
        worker(&script),
        None,
        settings,
        vec![run_js_call("js-1"), finish()],
    );

    let events = run(&fixture, Role::Worker).await;

    assert_eq!(started(&events, "grep").len(), 1, "only the first call ran");
    let text = result_text(&fixture.model, "js-1").await;
    assert!(text.contains("tool call limit of 1"), "{text}");
    let detail = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Tool(ToolEvent::ToolCompleted {
                call_id, detail, ..
            }) if call_id == "js-1" => detail.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(detail["run_js"]["limit_hit"], "tool_calls");
}

#[tokio::test]
async fn a_timed_out_call_leaves_no_worker_process() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("worker.pid");
    // The worker records its pid and then ignores the run: only the host can end it.
    let script = format!("echo $$ > {}\nexec sleep 600", pid_file.display());
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![
            tool_response("js-1", "run_js", json!({"code": "x", "timeout_ms": 1})),
            finish(),
        ],
    );

    let events = run(&fixture, Role::Worker).await;

    let text = result_text(&fixture.model, "js-1").await;
    assert!(text.contains("timed out after 1 ms"), "{text}");
    let detail = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Tool(ToolEvent::ToolCompleted {
                call_id, detail, ..
            }) if call_id == "js-1" => detail.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(detail["run_js"]["timed_out"], true);
    // The worker was killed and reaped before the tool returned.
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_err(),
        "worker {pid} is still alive"
    );
}

#[tokio::test]
async fn a_child_shell_waits_for_completion_and_streams_under_the_parent() {
    let script = format!(
        r#"printf '%s\n' '{{"type":"tool_call","id":1,"name":"shell","args":{{"command":"echo child-output"}}}}'
read -r reply
{DONE_WITH_REPLY}"#
    );
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );

    let mut receiver = fixture.bus.subscribe();
    let run =
        fixture
            .runtime
            .delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(fixture.runtime.wait(run).await, Ok(AgentRunPhase::Done));
    // Live output is published by a task that outlives the call; wait for its
    // terminal event rather than for a quiet period.
    let mut events = Vec::new();
    let mut streamed = String::new();
    loop {
        let event = receiver.recv().await.unwrap();
        if let EventKind::Tool(ToolEvent::ShellJobOutput {
            call_id,
            chunk,
            status,
            ..
        }) = &event.kind
            && call_id.as_deref() == Some("js-1/1")
        {
            streamed.push_str(chunk);
            if status != "running" {
                events.push(event);
                break;
            }
        }
        events.push(event);
    }
    assert_eq!(started(&events, "shell"), [("js-1/1", Some("js-1"))]);
    assert_eq!(streamed, "child-output\n");
    let text = result_text(&fixture.model, "js-1").await;
    assert!(text.ends_with("[run_js: 1 calls, 0 failed]"), "{text}");
}
