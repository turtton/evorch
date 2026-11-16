//! C10-C12: real Worker/tool execution, real FIFO-controlled processes, and
//! input-derived cache accounting plus independent assertions on all three wires.
use super::*;
use runtime::quiesce::{RestartAttempt, RestartAttemptState, RestartBlockerKind};
use std::sync::atomic::Ordering;

struct WaitingShell {
    _directory: tempfile::TempDir,
    _storage: storage::Storage,
    config: storage::StorageConfig,
    h: Harness,
    shell: Arc<GatedShell>,
    fifo: std::path::PathBuf,
    run: RunId,
    parent: Option<RunId>,
    events: Vec<Event>,
}

fn wait_call(id: &str, input: Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, MODEL, 0, id, "wait", [input.to_string()])
}

async fn waiting_shell(wire: Wire, form: usize) -> WaitingShell {
    let explicit = form != 0;
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("release");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let config = storage::StorageConfig {
        db_path: directory.path().join("wait.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let shell = Arc::new(GatedShell::new());
    let parent_read = Arc::new(GatedRead {
        started: Notify::new(),
        release: Notify::new(),
        child_started: Notify::new(),
    });
    // A separate, still-live parent makes an explicit target remain unfinished.
    // This also proves shell completion interrupts a run-completion predicate.
    parent_read.release.notify_one();
    let mut h = harness_with_wire(
        wire,
        Vec::new(),
        1_000_000,
        parent_read.clone(),
        vec![shell.clone()],
        None,
        0.9,
        |_| {},
    );
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let parent = if explicit {
        h.mock.enqueue([read_response(1)]);
        let parent = h.runtime.delegate_background(
            Role::Orchestrator,
            "remain alive".into(),
            RunConfig::default(),
        );
        parent_read.child_started.notified().await;
        loop {
            let event = h.receiver.recv().await.unwrap();
            if matches!(&event.kind, EventKind::Tool(event_bus::ToolEvent::ToolStarted { run_id: Some(id), .. }) if id == &parent.to_string())
            {
                break;
            }
        }
        Some(parent)
    } else {
        None
    };
    let command = json!({"command":format!("IFS= read -r line < '{}'; printf 'normal:%s' \"$line\"", fifo.display()),"yield_ms":0});
    h.mock.enqueue([
        read_response(0),
        ScriptedResponse::tool_call(
            "start-response",
            MODEL,
            0,
            "shell-start",
            "shell",
            [command.to_string()],
        ),
    ]);
    let config_run = RunConfig {
        interactive: true,
        keep_alive: true,
        ..Default::default()
    };
    let run = match parent {
        Some(parent) => h
            .runtime
            .delegate_background_as_child(parent, Role::Worker, "wait on shell", config_run)
            .unwrap(),
        None => h
            .runtime
            .delegate_background(Role::Worker, "wait on shell".into(), config_run),
    };
    shell.started.notified().await;
    h.mock.enqueue([wait_call(
        "shell-wait",
        parent.map_or_else(
            || json!({}),
            |p| match form {
                1 => json!({"run_id":p.to_string()}),
                2 => json!({"run_ids":[p.to_string()],"mode":"any"}),
                _ => json!({"run_ids":[p.to_string()],"mode":"all"}),
            },
        ),
    )]);
    shell.release.add_permits(1);
    let events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    assert_eq!(
        shell.shell_job_snapshots(&run.to_string())[0].state,
        tools::ShellJobState::Running
    );
    WaitingShell {
        _directory: directory,
        _storage: storage,
        config,
        h,
        shell,
        fifo,
        run,
        parent,
        events,
    }
}

async fn blocker(h: &Harness, run: RunId, kind: RestartBlockerKind) {
    let mut changes = h.runtime.subscribe_restart_state();
    loop {
        assert!(!h.runtime.restart_blockers().is_empty());
        if h.runtime
            .restart_blockers()
            .iter()
            .any(|b| b.run == run && b.kind == kind)
        {
            return;
        }
        changes.changed().await.unwrap();
    }
}

async fn release_job(f: &WaitingShell) {
    use std::io::Write;
    let fifo = f.fifo.clone();
    let writer = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .open(fifo)
            .unwrap()
            .write_all(b"released\n")
            .unwrap();
    });
    let identity = f.shell.shell_job_snapshots(&f.run.to_string())[0]
        .identity
        .clone();
    f.shell.wait_shell_job_terminal(&identity).await.unwrap();
    writer.await.unwrap();
}

fn snapshot(f: &WaitingShell) -> storage::RunContextRecord {
    storage::Database::open(&f.config)
        .unwrap()
        .run_context(&f.run.to_string())
        .unwrap()
        .unwrap()
}

async fn assert_no_safety_ack(f: &WaitingShell, attempt: &RestartAttempt) {
    assert_eq!(
        f.h.runtime.restart_attempt_state(attempt),
        RestartAttemptState::Active
    );
    assert!(
        f.h.runtime
            .restart_blockers()
            .iter()
            .any(|b| b.run == f.run)
    );
    let mut quiesced = Box::pin(f.h.runtime.wait_quiesced());
    assert!(futures_util::poll!(quiesced.as_mut()).is_pending());
}

// Independent of provider encoders: inspect the external protocol's own fields.
fn wire_result<'a>(wire: Wire, body: &'a Value, id: &str) -> &'a Value {
    let entries = match wire {
        Wire::ChatCompletions => &body["messages"],
        _ => &body["input"],
    }
    .as_array()
    .unwrap();
    let results: Vec<_> = entries
        .iter()
        .filter(|entry| match wire {
            Wire::ChatCompletions => entry["role"] == "tool" && entry["tool_call_id"] == id,
            _ => entry["type"] == "function_call_output" && entry["call_id"] == id,
        })
        .collect();
    assert_eq!(results.len(), 1, "one result per original call: {id}");
    results[0]
}

fn assert_wire(f: &WaitingShell) {
    assert_wire_at(f, "shell-wait");
}

fn assert_wire_at(f: &WaitingShell, shell_call: &str) {
    let requests: Vec<_> =
        f.h.mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == f.h.wire.path())
            .map(|r| r.body)
            .collect();
    let requests = if f.parent.is_some() {
        &requests[1..]
    } else {
        &requests[..]
    };
    verify_trace_of(&f.h, f.run, &f.events, 0, requests);
    assert_eq!(f.h.mock.remaining_scripts(), 0);
    let last = requests.last().unwrap();
    wire_result(f.h.wire, last, "shell-start");
    let result = wire_result(f.h.wire, last, shell_call);
    let output = match f.h.wire {
        Wire::ChatCompletions => &result["content"],
        _ => &result["output"],
    }
    .as_str()
    .unwrap();
    let wait: Value = serde_json::from_str(output).unwrap();
    assert_eq!(wait["shell_jobs"].as_array().unwrap().len(), 1);
    assert_eq!(wait["shell_jobs"][0]["call_id"], "shell-start");
    assert_eq!(wait["shell_jobs"][0]["exit_code"], 0);
    assert!(
        wait["shell_jobs"][0]["content"]
            .as_str()
            .unwrap()
            .contains("normal:released")
    );
    assert_eq!(last.to_string().matches("normal:released").count(), 1);
    assert!(
        !last
            .to_string()
            .contains("Shell job terminal result (original call:")
    );
    assert!(
        !last.to_string().contains("Shell job completed:"),
        "no post-tools completion notice after presentation"
    );
    let entries = match f.h.wire {
        Wire::ChatCompletions => &last["messages"],
        _ => &last["input"],
    }
    .as_array()
    .unwrap();
    let use_index = entries
        .iter()
        .position(|entry| match f.h.wire {
            Wire::ChatCompletions => entry["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|c| c["id"] == shell_call)),
            _ => entry["type"] == "function_call" && entry["call_id"] == shell_call,
        })
        .unwrap();
    assert_eq!(
        &entries[use_index + 1],
        result,
        "no injected user message in an open batch"
    );
    let again = wire_result(f.h.wire, last, "after-shell");
    let output = match f.h.wire {
        Wire::ChatCompletions => &again["content"],
        _ => &again["output"],
    }
    .as_str()
    .unwrap();
    let again: Value = serde_json::from_str(output).unwrap();
    assert!(again["shell_jobs"].as_array().unwrap().is_empty());
    assert!(f.shell.shell_job_snapshots(&f.run.to_string())[0].observed);
}

fn followups(f: &WaitingShell) {
    f.h.mock.enqueue([
        wait_call("after-shell", json!({})),
        text_response("complete"),
    ]);
}

async fn finish(f: &mut WaitingShell) {
    // A parent is a live source, so explicitly terminate this second wait with
    // host input rather than assuming an event will never arrive.
    f.events
        .extend(through_phase(&mut f.h.receiver, f.run, AgentRunPhase::Waiting).await);
    if f.parent.is_some() {
        f.h.runtime
            .send_message(f.run, "finish second wait".into())
            .unwrap();
    }
    f.events
        .extend(through_phase(&mut f.h.receiver, f.run, AgentRunPhase::Waiting).await);
    assert_wire(f);
    f.h.runtime
        .stop(f.run, runtime::StopScope::SelfOnly)
        .unwrap();
    f.h.runtime.wait(f.run).await.unwrap();
    if let Some(parent) = f.parent {
        f.h.runtime
            .stop(parent, runtime::StopScope::SelfOnly)
            .unwrap();
        f.h.runtime.wait(parent).await.unwrap();
    }
}

#[tokio::test]
async fn running_shell_wait_quiesces_only_after_its_tool_result_on_every_wire() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        for form in 0..4 {
            let mut f = waiting_shell(wire, form).await;
            let attempt = f.h.runtime.begin_restart_quiesce().unwrap();
            blocker(&f.h, f.run, RestartBlockerKind::ShellRunning).await;
            assert_no_safety_ack(&f, &attempt).await;
            release_job(&f).await;
            if form != 0 {
                // The parent's deliberately in-flight read is a separate safety
                // blocker; wait for this Worker's own durable boundary only.
                let mut changes = f.h.runtime.subscribe_restart_state();
                loop {
                    if f.h.runtime.resume_intents().unwrap().iter().any(|i| {
                        i.run_id == f.run
                            && i.checkpoint.as_ref().is_some_and(|c| {
                                c.boundary == runtime::checkpoint::CheckpointBoundary::ToolsApplied
                            })
                    }) {
                        break;
                    }
                    changes.changed().await.unwrap();
                }
            } else {
                f.h.runtime.wait_quiesced().await.unwrap();
            }
            let intent =
                f.h.runtime
                    .resume_intents()
                    .unwrap()
                    .into_iter()
                    .find(|i| i.run_id == f.run)
                    .unwrap();
            assert_eq!(
                intent.checkpoint.unwrap().boundary,
                runtime::checkpoint::CheckpointBoundary::ToolsApplied
            );
            assert_eq!(
                snapshot(&f)
                    .messages_json
                    .matches("normal:released")
                    .count(),
                1
            );
            followups(&f);
            f.h.runtime.cancel_restart_quiesce(&attempt).unwrap();
            finish(&mut f).await;
        }
    }
}

#[tokio::test]
async fn cancelling_before_shell_presentation_does_not_ack_or_duplicate_on_every_wire() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let mut f = waiting_shell(wire, 0).await;
        let attempt = f.h.runtime.begin_restart_quiesce().unwrap();
        blocker(&f.h, f.run, RestartBlockerKind::ShellRunning).await;
        followups(&f);
        f.h.runtime.cancel_restart_quiesce(&attempt).unwrap();
        assert!(!f.shell.shell_job_snapshots(&f.run.to_string())[0].observed);
        release_job(&f).await;
        finish(&mut f).await;
    }
}

#[tokio::test]
async fn presented_shell_ack_failure_is_retryable_without_safety_ack_on_every_wire() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let mut f = waiting_shell(wire, 0).await;
        let attempt = f.h.runtime.begin_restart_quiesce().unwrap();
        blocker(&f.h, f.run, RestartBlockerKind::ShellRunning).await;
        let before = snapshot(&f);
        f.shell.ack_blocked.store(true, Ordering::SeqCst);
        release_job(&f).await;
        blocker(&f.h, f.run, RestartBlockerKind::ShellResultUnobserved).await;
        assert_no_safety_ack(&f, &attempt).await;
        assert_eq!(
            snapshot(&f),
            before,
            "failed ACK cannot commit a checkpoint"
        );
        assert!(!f.shell.shell_job_snapshots(&f.run.to_string())[0].observed);
        f.shell.allow_ack();
        f.h.runtime.wait_quiesced().await.unwrap();
        assert_eq!(
            snapshot(&f)
                .messages_json
                .matches("normal:released")
                .count(),
            1
        );
        followups(&f);
        f.h.runtime.cancel_restart_quiesce(&attempt).unwrap();
        finish(&mut f).await;
    }
}

#[tokio::test]
async fn cancelling_after_shell_presentation_before_save_retains_receipt_on_every_wire() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let mut f = waiting_shell(wire, 0).await;
        let attempt = f.h.runtime.begin_restart_quiesce().unwrap();
        blocker(&f.h, f.run, RestartBlockerKind::ShellRunning).await;
        let before = snapshot(&f);
        f.shell.ack_blocked.store(true, Ordering::SeqCst);
        release_job(&f).await;
        blocker(&f.h, f.run, RestartBlockerKind::ShellResultUnobserved).await;
        assert_no_safety_ack(&f, &attempt).await;
        assert_eq!(snapshot(&f), before);
        followups(&f);
        f.shell.allow_ack();
        f.h.runtime.cancel_restart_quiesce(&attempt).unwrap();
        finish(&mut f).await;
    }
}

#[tokio::test]
async fn another_owners_terminal_job_cannot_wake_or_ack_worker_wait_on_every_wire() {
    use futures_util::FutureExt;
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let mut f = waiting_shell(wire, 0).await;
        let foreign = tools::ToolExecutionContext {
            run_id: "foreign-shell-owner".into(),
            call_id: Some("foreign-call".into()),
            ..Default::default()
        };
        f.shell
            .shell
            .execute_with_context(
                &foreign,
                json!({"command":"printf foreign-output", "yield_ms":0}),
            )
            .await
            .unwrap();
        let identity = f.shell.shell_job_snapshots(&foreign.run_id)[0]
            .identity
            .clone();
        f.shell
            .shell
            .wait_shell_job_terminal(&identity)
            .await
            .unwrap();
        // Cause a fresh predicate pass after the foreign terminal event. Its
        // completion must leave this wait unresolved until the caller's job ends.
        let _ = f.shell.snapshot_checked.notified().now_or_never();
        f.shell.ack_changes.send_modify(|v| *v += 1);
        f.shell.snapshot_checked.notified().await;
        assert_eq!(f.shell.ack_attempts.load(Ordering::SeqCst), 0);
        assert!(!f.shell.shell.shell_job_snapshots(&foreign.run_id)[0].observed);
        followups(&f);
        release_job(&f).await;
        finish(&mut f).await;
        assert!(!f.shell.shell.shell_job_snapshots(&foreign.run_id)[0].observed);
        assert!(
            f.h.mock
                .recorded_requests()
                .iter()
                .all(|r| !r.body.to_string().contains("foreign-output"))
        );
        f.shell.shell.release_shell_jobs(&foreign.run_id).unwrap();
    }
}

#[tokio::test]
async fn terminal_without_teardown_cannot_wake_or_ack_until_confirmed_on_every_wire() {
    use futures_util::FutureExt;
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let mut f = waiting_shell(wire, 0).await;
        f.shell.teardown_blocked.store(true, Ordering::SeqCst);
        release_job(&f).await;
        let _ = f.shell.snapshot_checked.notified().now_or_never();
        f.shell.ack_changes.send_modify(|v| *v += 1);
        f.shell.snapshot_checked.notified().await;
        assert_eq!(f.shell.ack_attempts.load(Ordering::SeqCst), 0);
        assert!(!f.shell.shell.shell_job_snapshots(&f.run.to_string())[0].observed);
        let (gate, arrived) = mock_openai::ResponseGate::new();
        f.h.mock
            .enqueue([wait_call("confirmed-shell", json!({})).with_gate(gate.clone())]);
        f.h.runtime
            .send_message(f.run, "host input, not shell completion".into())
            .unwrap();
        tokio::task::spawn_blocking(move || arrived.recv().unwrap())
            .await
            .unwrap();
        let requests = f.h.mock.recorded_requests();
        let last = requests
            .iter()
            .rev()
            .find(|r| r.path == wire.path())
            .unwrap();
        let result = wire_result(wire, &last.body, "shell-wait");
        let output = match wire {
            Wire::ChatCompletions => &result["content"],
            _ => &result["output"],
        }
        .as_str()
        .unwrap();
        let result: Value = serde_json::from_str(output).unwrap();
        assert_eq!(result["user_input_ready"], true);
        assert!(result["shell_jobs"].as_array().unwrap().is_empty());
        assert_eq!(f.shell.ack_attempts.load(Ordering::SeqCst), 0);
        followups(&f);
        f.shell.teardown_blocked.store(false, Ordering::SeqCst);
        f.shell.ack_changes.send_modify(|v| *v += 1);
        gate.release();
        // confirmed-shell wait, after-shell wait, then interactive idle.
        for _ in 0..3 {
            f.events
                .extend(through_phase(&mut f.h.receiver, f.run, AgentRunPhase::Waiting).await);
        }
        assert_wire_at(&f, "confirmed-shell");
        f.h.runtime
            .stop(f.run, runtime::StopScope::SelfOnly)
            .unwrap();
        f.h.runtime.wait(f.run).await.unwrap();
    }
}
