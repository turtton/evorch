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
use tools::post_edit::{PostEditHook, PostEditInput, PostEditOutcome};
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
    fixture_with(launcher, policy, None, settings, responses)
}

fn fixture_with(
    launcher: WorkerLauncher,
    policy: Option<ApprovalPolicy>,
    hook: Option<Arc<dyn PostEditHook>>,
    settings: config::RunJsConfig,
    responses: Vec<providers::ChatResponse>,
) -> Fixture {
    let bus = Arc::new(EventBus::new(256));
    let mut executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    )
    .with_post_edit_hook(hook);
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
    assert!(
        text.ends_with(
            "[run_js: 1 calls, 1 failed (edit #1: tools.edit is not available in this run)]"
        ),
        "{text}"
    );
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

/// Every call that started must have exactly one terminal event.
fn open_tool_calls(events: &[Event]) -> Vec<String> {
    let mut open: Vec<String> = Vec::new();
    for event in events {
        match &event.kind {
            EventKind::Tool(ToolEvent::ToolStarted { call_id, .. }) => open.push(call_id.clone()),
            EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. }) => {
                let position = open
                    .iter()
                    .position(|id| id == call_id)
                    .unwrap_or_else(|| panic!("{call_id} completed without starting or twice"));
                open.remove(position);
            }
            _ => {}
        }
    }
    open
}

fn completed<'a>(events: &'a [Event], call: &str) -> Option<(&'a Option<String>, bool)> {
    events.iter().find_map(|event| match &event.kind {
        EventKind::Tool(ToolEvent::ToolCompleted {
            call_id,
            output,
            is_error,
            ..
        }) if call_id == call => Some((output, *is_error)),
        _ => None,
    })
}

#[tokio::test]
async fn a_command_cut_off_with_the_program_is_closed_and_keeps_its_effects_unobserved() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("started");
    let pid_file = directory.path().join("child.pid");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    // The child records its pid, tells the worker it runs, and then sleeps for
    // good; the worker finishes without waiting for it.
    let command = format!(
        "echo $$ > {}; echo go > {}; exec sleep 600",
        pid_file.display(),
        fifo.display()
    );
    let script = format!(
        r#"printf '%s\n' '{{"type":"tool_call","id":1,"name":"shell","args":{{"command":"{command}"}}}}'
read -r _ < {fifo}
printf '%s\n' '{{"type":"done","value":"left it running"}}'"#,
        fifo = fifo.display()
    );
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );

    // Checks the child at the moment the parent completes, not after the run ends:
    // by then its process must already be stopped and reaped.
    let observer = {
        let mut events = fixture.bus.subscribe();
        let pid_file = pid_file.clone();
        tokio::spawn(async move {
            loop {
                let event = events.recv().await.unwrap();
                if let EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. }) = &event.kind
                    && call_id == "js-1"
                {
                    let pid: i32 = std::fs::read_to_string(&pid_file)
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    return rustix::process::test_kill_process(
                        rustix::process::Pid::from_raw(pid).unwrap(),
                    )
                    .is_err();
                }
            }
        })
    };
    let mut receiver = fixture.bus.subscribe();
    let run =
        fixture
            .runtime
            .delegate_background(Role::Worker, "task".into(), RunConfig::default());
    // A command cut off with the program may have had effects nobody saw, so, as
    // for a cancelled model call, the run keeps its ownership instead of finishing
    // (this fixture has no durable store to record the snapshot in).
    let phase = fixture.runtime.wait(run).await;
    let events = drain_events(&mut receiver).await;
    assert_eq!(phase, Ok(AgentRunPhase::Error));
    let reason = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Error,
                reason,
                ..
            }) => reason.clone(),
            _ => None,
        })
        .unwrap();
    assert!(reason.contains("unobserved shell effects"), "{reason}");

    // The child's terminal event exists, is not duplicated, and precedes the parent's.
    assert_eq!(open_tool_calls(&events), Vec::<String>::new());
    let (output, is_error) = completed(&events, "js-1/1").expect("child completed");
    assert_eq!((output.as_deref(), is_error), (Some("cancelled"), true));
    let position = |call: &str| {
        events
            .iter()
            .position(|event| {
                matches!(&event.kind, EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. }) if call_id == call)
            })
            .unwrap()
    };
    assert!(position("js-1/1") < position("js-1"));
    // The parent says what was cut off, and the process was gone when it completed.
    let text = result_text(&fixture.model, "js-1").await;
    assert!(text.contains("=> left it running"), "{text}");
    assert!(
        text.contains("shell #1: cut off before it finished"),
        "{text}"
    );
    assert!(
        observer.await.unwrap(),
        "the child was still alive when run_js completed"
    );
}

/// 60 KiB of printed output: the output limiter replaces the result's detail with
/// an artifact reference.
const BIG_OUTPUT: &str =
    r#"printf '{"type":"output","text":"%s"}\n' "$(head -c 60000 /dev/zero | tr '\0' x)""#;

fn read_call_line(id: u64, path: &str) -> String {
    format!(
        "printf '%s\\n' '{{\"type\":\"tool_call\",\"id\":{id},\"name\":\"read\",\"args\":{{\"path\":\"{path}\"}}}}'\nread -r _\n"
    )
}

#[tokio::test]
async fn calls_are_counted_even_when_the_result_detail_is_replaced_by_an_artifact() {
    let directory = tempfile::tempdir().unwrap();
    let file = write_file(directory.path(), "a.txt", "x\n");
    let script = format!(
        "{}{}{BIG_OUTPUT}\nprintf '%s\\n' '{{\"type\":\"done\",\"value\":null}}'",
        read_call_line(1, &file),
        read_call_line(2, &file)
    );
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );
    let mut config = RunConfig::default();
    // run_js itself plus its two reads reach the limit; the program's calls count.
    config.budget.max_tool_calls = 3;

    let mut receiver = fixture.bus.subscribe();
    let run = fixture
        .runtime
        .delegate_background(Role::Worker, "task".into(), config);
    let phase = fixture.runtime.wait(run).await;
    let events = drain_events(&mut receiver).await;

    let detail = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Tool(ToolEvent::ToolCompleted {
                call_id, detail, ..
            }) if call_id == "js-1" => detail.clone(),
            _ => None,
        })
        .unwrap();
    assert!(detail["output_artifact"]["path"].is_string(), "{detail}");
    assert_eq!(detail["run_js"]["calls"].as_array().unwrap().len(), 2);
    assert_eq!(
        phase,
        Ok(AgentRunPhase::Error),
        "the tool-call budget was spent"
    );
}

#[tokio::test]
async fn identical_calls_made_inside_the_program_stop_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let file = write_file(directory.path(), "a.txt", "x\n");
    let script = format!(
        "{}{}{}printf '%s\\n' '{{\"type\":\"done\",\"value\":null}}'",
        read_call_line(1, &file),
        read_call_line(2, &file),
        read_call_line(3, &file)
    );
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );
    let mut config = RunConfig::default();
    config.budget.max_identical_tool_call_repeats = 3;

    let mut receiver = fixture.bus.subscribe();
    let run = fixture
        .runtime
        .delegate_background(Role::Worker, "task".into(), config);
    let phase = fixture.runtime.wait(run).await;
    let events = drain_events(&mut receiver).await;

    assert_eq!(phase, Ok(AgentRunPhase::Error));
    let reason = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Error,
                reason,
                ..
            }) => reason.clone(),
            _ => None,
        })
        .expect("the run ends with a reason");
    assert!(reason.starts_with("IdenticalToolCalls"), "{reason}");
    assert!(reason.contains("tool=read"), "{reason}");
}

#[tokio::test]
async fn different_calls_inside_the_program_are_not_a_repeat() {
    let directory = tempfile::tempdir().unwrap();
    let mut calls = String::new();
    for (id, name) in ["a", "b", "c", "d"].into_iter().enumerate() {
        let file = write_file(directory.path(), &format!("{name}.txt"), "x\n");
        calls.push_str(&read_call_line(id as u64 + 1, &file));
    }
    let script = format!("{calls}printf '%s\\n' '{{\"type\":\"done\",\"value\":null}}'");
    let fixture = fixture(
        worker(&script),
        None,
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );
    let mut config = RunConfig::default();
    config.budget.max_identical_tool_call_repeats = 3;

    let run = fixture
        .runtime
        .delegate_background(Role::Worker, "task".into(), config);
    assert_eq!(fixture.runtime.wait(run).await, Ok(AgentRunPhase::Done));
}

struct WarningHook;

#[async_trait::async_trait]
impl PostEditHook for WarningHook {
    async fn check(&self, _: &PostEditInput) -> PostEditOutcome {
        PostEditOutcome::Warning {
            message: "UNTRUSTED-REPOSITORY-COMMENT".into(),
        }
    }
}

#[tokio::test]
async fn the_summary_reports_failed_calls_and_checker_warnings_the_program_discarded() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory
        .path()
        .join("new.rs")
        .to_string_lossy()
        .into_owned();
    let missing = directory
        .path()
        .join("absent.txt")
        .to_string_lossy()
        .into_owned();
    // The program ignores every reply and returns nothing.
    let script = format!(
        r#"printf '%s\n' '{{"type":"tool_call","id":1,"name":"write","args":{{"path":"{target}","content":"// c\n"}}}}'
read -r _
printf '%s\n' '{{"type":"tool_call","id":2,"name":"read","args":{{"path":"{missing}"}}}}'
read -r _
printf '%s\n' '{{"type":"done","value":null}}'"#
    );
    let fixture = fixture_with(
        worker(&script),
        None,
        Some(Arc::new(WarningHook)),
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );

    run(&fixture, Role::Worker).await;

    let text = result_text(&fixture.model, "js-1").await;
    assert!(
        text.ends_with(&format!(
            "[run_js: 2 calls, 1 failed (read #2: {missing}: no such file or directory); wrote: {target}; warnings: write #1: post-edit checker warning on {target}]"
        )) || (text.contains("read #2:") && text.contains("warnings: write #1: post-edit checker warning")),
        "{text}"
    );
    assert!(text.contains(&format!("wrote: {target}")), "{text}");
    assert!(!text.contains("UNTRUSTED-REPOSITORY-COMMENT"), "{text}");
    assert_eq!(text.lines().count(), 1, "one summary line and nothing else");
}

#[tokio::test]
async fn a_checker_warning_survives_an_artifact_and_a_discarded_result() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory
        .path()
        .join("big.rs")
        .to_string_lossy()
        .into_owned();
    // 90 KB of file content and a 150 KB checker message: the executor turns the
    // child's output into an artifact, and the program ignores the reply anyway.
    let script = format!(
        r#"content=$(head -c 90000 /dev/zero | tr '\0' a)
printf '{{"type":"tool_call","id":1,"name":"write","args":{{"path":"{target}","content":"%s"}}}}\n' "$content"
read -r _
printf '%s\n' '{{"type":"done","value":null}}'"#
    );
    let fixture = fixture_with(
        worker(&script),
        None,
        Some(Arc::new(HugeWarningHook)),
        enabled(),
        vec![run_js_call("js-1"), finish()],
    );

    let events = run(&fixture, Role::Worker).await;

    let child_detail = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Tool(ToolEvent::ToolCompleted {
                call_id, detail, ..
            }) if call_id == "js-1/1" => detail.clone(),
            _ => None,
        })
        .expect("child completed with a detail");
    assert!(
        child_detail["output_artifact"]["path"].is_string(),
        "the output limiter must have engaged: {child_detail}"
    );
    assert_eq!(
        child_detail["comment_checker"],
        json!({"outcome": "warning"}),
        "only the fact of the warning remains"
    );
    let text = result_text(&fixture.model, "js-1").await;
    assert!(
        text.contains(&format!(
            "warnings: write #1: post-edit checker warning on {target}"
        )),
        "{text}"
    );
    assert!(!text.contains("UNTRUSTED-REPOSITORY-COMMENT"), "{text}");
}

struct HugeWarningHook;

#[async_trait::async_trait]
impl PostEditHook for HugeWarningHook {
    async fn check(&self, _: &PostEditInput) -> PostEditOutcome {
        PostEditOutcome::Warning {
            message: "UNTRUSTED-REPOSITORY-COMMENT\n".repeat(5000),
        }
    }
}
