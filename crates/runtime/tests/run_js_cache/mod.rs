//! `run_js` and the prompt cache. The description is generated once per run, so
//! every request of a run carries byte-identical tool definitions; and a run in
//! which a program really runs (child calls, a result large enough to become an
//! artifact) still extends the previous wire input request after request.

use std::path::Path;
use std::sync::Arc;

use event_bus::AgentRunPhase;
use mock_openai::ScriptedResponse;
use runtime::{Role, RunConfig};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use tools::Tool;
use tools::tools::run_js::{WorkerLauncher, description as run_js_description};

use super::{
    BulkRead, Harness, MODEL, Wire, harness_with_model, read_response, text_response,
    through_phase, verify_trace, verify_trace_of,
};

/// A `sh` worker speaking the protocol: it reads a large file through the host,
/// writes one, prints far more than the output limit and returns a string.
fn worker(target: &Path) -> WorkerLauncher {
    let script = format!(
        r#"read -r run
printf '%s\n' '{{"type":"tool_call","id":1,"name":"read","args":{{"index":1}}}}'
read -r _
printf '%s\n' '{{"type":"tool_call","id":2,"name":"write","args":{{"path":"{target}","content":"written by run_js\n"}}}}'
read -r _
printf '{{"type":"output","text":"%s\\n"}}\n' "$(head -c 60000 /dev/zero | tr '\0' x)"
printf '%s\n' '{{"type":"done","value":"all done"}}'"#,
        target = target.display()
    );
    WorkerLauncher::injected(
        Arc::new(DirectSandbox::new_unchecked()),
        "sh".into(),
        vec!["-c".into(), script],
    )
}

fn shell() -> Arc<dyn Tool> {
    Arc::new(tools::Shell::new(Arc::new(DirectSandbox::new_unchecked())))
}

fn definition<'a>(request: &'a Value, name: &str) -> &'a Value {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == name)
        .unwrap_or_else(|| panic!("{name} is offered"))
}

fn description<'a>(request: &'a Value, name: &str) -> &'a str {
    definition(request, name)["function"]["description"]
        .as_str()
        .unwrap()
}

/// run_js switched `enabled` and scoped to `scopes`.
fn scoped(enabled: bool, scopes: &[&str]) -> config::RunJsConfig {
    config::RunJsConfig {
        enabled,
        scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        ..config::RunJsConfig::default()
    }
}

fn harness(enabled: bool, script: Vec<ScriptedResponse>, launcher: WorkerLauncher) -> Harness {
    harness_with(scoped(enabled, &["worker"]), script, launcher)
}

fn harness_with(
    settings: config::RunJsConfig,
    script: Vec<ScriptedResponse>,
    launcher: WorkerLauncher,
) -> Harness {
    harness_with_model(
        Wire::ChatCompletions,
        MODEL,
        script,
        1_000_000,
        Arc::new(BulkRead),
        vec![
            Arc::new(tools::Write) as Arc<dyn Tool>,
            Arc::new(tools::Edit),
            shell(),
            Arc::new(tools::RunJs::with_launcher(launcher)),
        ],
        None,
        0.5,
        |config| config.tools.run_js = settings,
    )
}

fn run_js_response(index: usize) -> ScriptedResponse {
    ScriptedResponse::tool_call(
        &format!("response-{index}"),
        MODEL,
        0,
        &format!("call-{index}"),
        "run_js",
        [json!({"code": "/* the worker script runs */"}).to_string()],
    )
}

fn requests(harness: &Harness) -> Vec<Value> {
    harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == harness.wire.path())
        .map(|request| request.body)
        .collect()
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

/// A program runs for real, its child calls and its large result enter the
/// history, and the run keeps extending the wire prefix.
#[tokio::test]
async fn a_run_that_executes_run_js_preserves_tools_and_the_wire_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("written.txt");
    let mut harness = harness(
        true,
        vec![run_js_response(0), read_response(3), text_response("done")],
        worker(&target),
    );

    let run =
        harness
            .runtime
            .delegate_background(Role::Worker, "work".into(), RunConfig::default());
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );

    // The program really ran: its write happened and the history holds its result.
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "written by run_js\n"
    );
    // The input-derived mock cache and the contract's append-only assertions.
    verify_trace(&harness, run, &events, 0);

    let requests = requests(&harness);
    assert_eq!(requests.len(), 3);
    let first = serde_json::to_string(&requests[0]["tools"]).unwrap();
    for request in &requests[1..] {
        assert_eq!(serde_json::to_string(&request["tools"]).unwrap(), first);
    }
    let history = requests[2]["messages"].as_array().unwrap();
    let result = history
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call-0")
        .expect("run_js result in the history")["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(result.contains("Output artifact"), "{result}");
    assert!(
        result.contains("=> all done\n[run_js: 2 calls, 0 failed; wrote: "),
        "{result}"
    );
    // The earlier requests' history is a strict prefix of the later ones.
    for pair in requests.windows(2) {
        let (before, after) = (
            pair[0]["messages"].as_array().unwrap(),
            pair[1]["messages"].as_array().unwrap(),
        );
        assert!(after.len() > before.len());
        assert_eq!(before[..], after[..before.len()]);
    }
}

/// The definition of `run_js` is generated from this run's tools.
#[tokio::test]
async fn the_run_js_definition_lists_the_callable_tools_of_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let mut harness = harness(
        true,
        vec![read_response(3), text_response("done")],
        worker(&directory.path().join("unused")),
    );
    let run =
        harness
            .runtime
            .delegate_background(Role::Worker, "work".into(), RunConfig::default());
    through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;

    let requests = requests(&harness);
    let run_js = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "run_js")
        .expect("run_js is offered when enabled");
    let description = run_js["function"]["description"].as_str().unwrap();
    assert!(description.contains("read(args: { index: number })"));
    assert!(description.contains("write(args: { content: string; path: string })"));
}

/// `shell` and `edit` point at run_js only while it is on; the pointers are part of
/// the run's fixed tool definitions, so they never change during the run.
#[tokio::test]
async fn the_shell_and_edit_hints_are_in_every_request_of_a_run_that_has_run_js() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("written.txt");
    let mut harness = harness(
        true,
        vec![run_js_response(0), read_response(3), text_response("done")],
        worker(&target),
    );
    let run =
        harness
            .runtime
            .delegate_background(Role::Worker, "work".into(), RunConfig::default());
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    verify_trace(&harness, run, &events, 0);

    let requests = requests(&harness);
    assert_eq!(requests.len(), 3);
    let shell_hinted = format!(
        "{}{}",
        shell().description(),
        run_js_description::SHELL_HINT
    );
    let edit_hinted = format!(
        "{}{}",
        tools::Edit.description(),
        run_js_description::EDIT_HINT
    );
    for request in &requests {
        assert_eq!(description(request, "shell"), shell_hinted);
        assert_eq!(description(request, "edit"), edit_hinted);
        assert!(!description(request, "write").contains("run_js"));
    }
    let first = serde_json::to_string(&requests[0]["tools"]).unwrap();
    for request in &requests[1..] {
        assert_eq!(serde_json::to_string(&request["tools"]).unwrap(), first);
    }
}

/// While it is off, nothing about the other tools differs.
#[tokio::test]
async fn disabled_run_js_leaves_the_other_tool_definitions_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let launcher = || worker(&directory.path().join("unused"));
    let script = || vec![read_response(3), text_response("done")];

    let mut off = harness(false, script(), launcher());
    let run = off
        .runtime
        .delegate_background(Role::Worker, "work".into(), RunConfig::default());
    through_phase(&mut off.receiver, run, AgentRunPhase::Done).await;
    let mut on = harness(true, script(), launcher());
    let run = on
        .runtime
        .delegate_background(Role::Worker, "work".into(), RunConfig::default());
    through_phase(&mut on.receiver, run, AgentRunPhase::Done).await;

    let (off, on) = (requests(&off), requests(&on));
    assert!(!tool_names(&off[0]).contains(&"run_js".to_owned()));
    assert!(!off[0]["tools"].to_string().contains("run_js"));
    // Off, shell and edit keep the descriptions their tools define.
    assert_eq!(description(&off[0], "shell"), shell().description());
    assert_eq!(description(&off[0], "edit"), tools::Edit.description());
    // On, only run_js is added and the two pointers appended; the rest is identical.
    let others = |request: &Value| -> Vec<Value> {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| {
                !matches!(
                    tool["function"]["name"].as_str(),
                    Some("run_js" | "shell" | "edit")
                )
            })
            .cloned()
            .collect()
    };
    assert_eq!(others(&on[0]), others(&off[0]));
    for name in ["shell", "edit"] {
        let (before, after) = (definition(&off[0], name), definition(&on[0], name));
        let mut hinted = after.clone();
        let original = description(&off[0], name);
        let added = description(&on[0], name)
            .strip_prefix(original)
            .expect("only appended");
        assert!(added.contains("run_js"), "{name}: {added}");
        hinted["function"]["description"] = Value::String(original.to_owned());
        assert_eq!(&hinted, before, "{name} differs beyond its description");
    }
}

/// Releases the mock's held response before it joins its workers, also on panic.
struct ReleaseOnDrop(mock_openai::ResponseGate);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// The wire requests of a run whose run_js switch flipped while it was waiting
/// for its first response, and of the next run started on the same runtime.
struct Switched {
    run: Vec<Value>,
    next_run: Vec<Value>,
}

/// Starts a worker run with run_js `enabled`, switches it to `!enabled` while the
/// mock holds the first response (as the GUI's Agent features settings switch a
/// live runtime), then starts one more run. The switched run must pass the
/// input-derived cache and append-only wire checks.
async fn switch_run_js_during_a_run(
    enabled: bool,
    script: Vec<ScriptedResponse>,
    next_run: Vec<ScriptedResponse>,
) -> Switched {
    switch_settings_during_a_run(
        scoped(enabled, &["worker"]),
        |settings| config::RunJsConfig {
            enabled: !enabled,
            ..settings
        },
        script,
        next_run,
    )
    .await
}

/// The same with any change of the run_js settings (its switch or its scopes).
async fn switch_settings_during_a_run(
    start: config::RunJsConfig,
    switch: impl FnOnce(config::RunJsConfig) -> config::RunJsConfig,
    mut script: Vec<ScriptedResponse>,
    next_run: Vec<ScriptedResponse>,
) -> Switched {
    let directory = tempfile::tempdir().unwrap();
    let (gate, arrived) = mock_openai::ResponseGate::new();
    script[0] = script[0].clone().with_gate(gate.clone());
    script.extend(next_run);
    let mut harness = harness_with(start, script, worker(&directory.path().join("unused")));
    let _release = ReleaseOnDrop(gate.clone());

    let run =
        harness
            .runtime
            .delegate_background(Role::Worker, "work".into(), RunConfig::default());
    tokio::task::spawn_blocking(move || arrived.recv().unwrap())
        .await
        .unwrap();
    harness
        .runtime
        .set_run_js_settings(switch(harness.runtime.run_js_settings()));
    gate.release();
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    let switched = requests(&harness);
    verify_trace_of(&harness, run, &events, 0, &switched);

    let next =
        harness
            .runtime
            .delegate_background(Role::Worker, "next".into(), RunConfig::default());
    through_phase(&mut harness.receiver, next, AgentRunPhase::Done).await;
    assert_eq!(harness.mock.remaining_scripts(), 0);
    let next_run = requests(&harness).split_off(switched.len());
    Switched {
        run: switched,
        next_run,
    }
}

fn assert_same_tools(requests: &[Value]) {
    let first = serde_json::to_string(&requests[0]["tools"]).unwrap();
    for request in &requests[1..] {
        assert_eq!(serde_json::to_string(&request["tools"]).unwrap(), first);
    }
}

fn tool_result<'a>(request: &'a Value, call_id: &str) -> &'a str {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
        .unwrap_or_else(|| panic!("result of {call_id} in the history"))["content"]
        .as_str()
        .unwrap()
}

/// Switching run_js off keeps the definitions the run already sent, refuses its
/// later calls, and leaves run_js out of the next run.
#[tokio::test]
async fn switching_run_js_off_during_a_run_keeps_its_tools_and_wire_prefix() {
    let switched = switch_run_js_during_a_run(
        true,
        vec![read_response(0), run_js_response(1), text_response("done")],
        vec![text_response("next")],
    )
    .await;

    assert_eq!(switched.run.len(), 3);
    assert_same_tools(&switched.run);
    let shell_hinted = format!(
        "{}{}",
        shell().description(),
        run_js_description::SHELL_HINT
    );
    for request in &switched.run {
        assert!(tool_names(request).contains(&"run_js".to_owned()));
        assert_eq!(description(request, "shell"), shell_hinted);
    }
    let refused = tool_result(&switched.run[2], "call-1");
    assert!(refused.contains("run_js is not enabled"), "{refused}");

    let next = &switched.next_run[0];
    assert!(!tool_names(next).contains(&"run_js".to_owned()));
    assert_eq!(description(next, "shell"), shell().description());
}

/// Switching run_js on leaves the running run without it and offers it to the
/// next run.
#[tokio::test]
async fn switching_run_js_on_during_a_run_keeps_its_tools_and_wire_prefix() {
    let switched = switch_run_js_during_a_run(
        false,
        vec![read_response(0), read_response(1), text_response("done")],
        vec![text_response("next")],
    )
    .await;

    assert_eq!(switched.run.len(), 3);
    assert_same_tools(&switched.run);
    for request in &switched.run {
        assert!(!request["tools"].to_string().contains("run_js"));
    }

    let next = &switched.next_run[0];
    assert!(tool_names(next).contains(&"run_js".to_owned()));
    assert_eq!(
        description(next, "shell"),
        format!(
            "{}{}",
            shell().description(),
            run_js_description::SHELL_HINT
        )
    );
}

/// Narrowing the scopes away from a running run keeps the definitions it already
/// sent, refuses its later calls, and leaves run_js out of the next run.
#[tokio::test]
async fn narrowing_the_scopes_during_a_run_keeps_its_tools_and_wire_prefix() {
    let switched = switch_settings_during_a_run(
        scoped(true, &["worker"]),
        |settings| config::RunJsConfig {
            scopes: vec!["explorer".into()],
            ..settings
        },
        vec![read_response(0), run_js_response(1), text_response("done")],
        vec![text_response("next")],
    )
    .await;

    assert_eq!(switched.run.len(), 3);
    assert_same_tools(&switched.run);
    assert!(tool_names(&switched.run[0]).contains(&"run_js".to_owned()));
    let refused = tool_result(&switched.run[2], "call-1");
    assert!(refused.contains("run_js is not enabled"), "{refused}");
    assert!(!tool_names(&switched.next_run[0]).contains(&"run_js".to_owned()));
}

/// Widening the scopes to a running run does not add run_js to it; the next
/// run has it.
#[tokio::test]
async fn widening_the_scopes_during_a_run_keeps_its_tools_and_wire_prefix() {
    let switched = switch_settings_during_a_run(
        scoped(true, &["explorer"]),
        |settings| config::RunJsConfig {
            scopes: vec!["worker".into()],
            ..settings
        },
        vec![read_response(0), run_js_response(1), text_response("done")],
        vec![text_response("next")],
    )
    .await;

    assert_eq!(switched.run.len(), 3);
    assert_same_tools(&switched.run);
    for request in &switched.run {
        assert!(!request["tools"].to_string().contains("run_js"));
    }
    let refused = tool_result(&switched.run[2], "call-1");
    assert!(refused.contains("run_js is not enabled"), "{refused}");
    assert!(tool_names(&switched.next_run[0]).contains(&"run_js".to_owned()));
}

/// The tool names of the first request of a run of `role` with `category`
/// under `scopes`, after the run passes the input-derived cache checks.
async fn tools_offered_to(scopes: &[&str], role: Role, category: Option<&str>) -> Vec<String> {
    let directory = tempfile::tempdir().unwrap();
    let mut harness = harness_with(
        scoped(true, scopes),
        vec![read_response(3), read_response(4), text_response("done")],
        worker(&directory.path().join("unused")),
    );
    let run = harness.runtime.delegate_background(
        role,
        "work".into(),
        RunConfig {
            category: category.map(str::to_owned),
            ..RunConfig::default()
        },
    );
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    verify_trace(&harness, run, &events, 0);
    tool_names(&requests(&harness)[0])
}

/// A scope names a role (every category and none) or a role with one category.
#[tokio::test]
async fn run_js_is_offered_only_to_the_roles_and_categories_in_the_scopes() {
    let defaults = ["worker:deep", "worker:high-reasoning"];
    let offered = |names: &[String]| names.contains(&"run_js".to_owned());

    for (scopes, role, category, expected) in [
        (&defaults[..], Role::Worker, Some("deep"), true),
        (&defaults[..], Role::Worker, Some("high-reasoning"), true),
        (&defaults[..], Role::Worker, None, false),
        (&defaults[..], Role::Worker, Some("quick"), false),
        (&["worker"][..], Role::Worker, None, true),
        (&["worker"][..], Role::Worker, Some("quick"), true),
        (&["worker:deep"][..], Role::Worker, Some("deep"), true),
        (&[][..], Role::Worker, Some("deep"), false),
    ] {
        let names = tools_offered_to(scopes, role, category).await;
        assert_eq!(
            offered(&names),
            expected,
            "{scopes:?} for {role:?} {category:?}"
        );
    }
}
