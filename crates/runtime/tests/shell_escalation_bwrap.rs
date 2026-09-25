mod support;

use event_bus::EventBus;
use runtime::AgentRuntime;
use sandbox::{BwrapConfig, BwrapSandbox};
use serde_json::json;
use std::{process::Command, sync::Arc};
use tools::{ToolExecutionContext, ToolExecutor};

fn child(name: &str) -> bool {
    if std::env::var_os("EVORCH_BWRAP_CHILD").is_some() {
        return true;
    }
    if Command::new("bwrap").arg("--version").output().is_err() {
        eprintln!("SKIP: bwrap is not installed");
        return false;
    }
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", name, "--nocapture", "--include-ignored"])
        .env("EVORCH_BWRAP_CHILD", "1")
        .output()
        .expect("child");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

async fn scenario(approved: bool) {
    let workspace = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("workspace");
    let sandbox = match BwrapSandbox::detect(BwrapConfig::new(workspace.path().into())) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            eprintln!("SKIP: bwrap unavailable: {error}");
            return;
        }
    };
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox),
    ));
    let model = Arc::new(support::ScriptedModel::new([Ok(support::text_response(
        &json!({"approve":approved,"reason":"denied"}).to_string(),
        providers::FinishReason::Stop,
    ))]));
    let runtime = AgentRuntime::new(bus, executor.clone(), model);
    executor.set_shell_escalation(
        runtime.shell_escalation_gate(),
        sandbox::composition::unsandboxed(),
    );
    let marker = tempfile::NamedTempFile::new_in("/tmp").expect("host marker");
    std::fs::write(marker.path(), "host-escalation-marker").expect("marker");
    let ctx = ToolExecutionContext {
        run_id: "bwrap".into(),
        thread_id: None,
        call_id: None,
    };
    let command = format!("cat '{}'", marker.path().display());
    let control = executor
        .execute(&ctx, "shell", "control", json!({"command":command}))
        .await
        .expect("control");
    assert!(control.is_error);
    assert!(!control.content.contains("host-escalation-marker"));
    let result = executor
        .execute(
            &ctx,
            "shell",
            "escalated",
            json!({"command":command,"sandbox_access":"unsandboxed","justification":"read host marker"}),
        )
        .await
        .expect("escalation");
    assert_eq!(!result.is_error, approved);
    if approved {
        assert_eq!(result.content, "exit_code: 0\nhost-escalation-marker");
    } else {
        assert_eq!(result.content, "denied");
    }
}

// Given: hidden host marker / When: approved escalation / Then: only escalated shell reads it.
#[test]
#[ignore = "requires usable bwrap; re-exec with EVORCH_BWRAP_CHILD"]
fn approved_escalation_reads_host_tmp_marker_outside_bwrap() {
    if child("approved_escalation_reads_host_tmp_marker_outside_bwrap") {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(scenario(true));
    }
}

// Given: hidden host marker / When: denied escalation / Then: host contents never return.
#[test]
#[ignore = "requires usable bwrap; re-exec with EVORCH_BWRAP_CHILD"]
fn denied_escalation_never_reaches_host() {
    if child("denied_escalation_never_reaches_host") {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(scenario(false));
    }
}
