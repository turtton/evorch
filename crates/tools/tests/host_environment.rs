//! Reviewed host execution uses the evorch process environment in every shell path.
use std::{ffi::OsStr, os::unix::ffi::OsStrExt, process::Command, sync::Arc};

use async_trait::async_trait;
use event_bus::{EventBus, EventReceiver};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use tools::tools::shell_escalation::{EscalationDecision, ShellEscalationGate};
use tools::{ToolExecutionContext, ToolExecutor, ToolResult};

const SECRET: &str = "opaque-fixture-credential-94376821";

struct Approve;
#[async_trait]
impl ShellEscalationGate for Approve {
    async fn decide(&self, _: &ToolExecutionContext, _: &str, _: &str) -> EscalationDecision {
        EscalationDecision::Approve
    }
}

fn assert_redacted(result: &ToolResult) {
    assert!(
        !format!("{result:?}").contains(SECRET),
        "credential escaped output redaction"
    );
}

async fn invoke(
    executor: &ToolExecutor,
    context: &ToolExecutionContext,
    events: &mut EventReceiver,
    args: Value,
) -> ToolResult {
    let result = executor
        .execute(context, "shell", "environment", args)
        .await
        .unwrap();
    assert_redacted(&result);
    for _ in 0..2 {
        let event = events.recv().await.unwrap();
        assert!(!format!("{event:?}").contains(SECRET));
    }
    result
}

#[test]
fn reviewed_shell_inherits_environment_and_redacts_pipe_pty_and_job_output() {
    if std::env::var_os("EVORCH_HOST_SHELL_FIXTURE").is_none() {
        let directory = tempfile::tempdir().unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "reviewed_shell_inherits_environment_and_redacts_pipe_pty_and_job_output",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("EVORCH_HOST_SHELL_FIXTURE", "1")
            .env("EVORCH_OUTPUT_DIR", directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("HOME", "/fixture/host-home")
            .env("GH_CONFIG_DIR", "/fixture/gh-config")
            .env("SSH_AUTH_SOCK", "/fixture/agent.sock")
            .env("HTTPS_PROXY", "http://fixture.invalid:8080")
            .env("EVORCH_CUSTOM_TOOL", "custom-tool-value")
            .env("EVORCH_NON_UTF8", OsStr::from_bytes(b"native-\xff"))
            .env("GH_TOKEN", SECRET)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_shell_cases());
}

async fn run_shell_cases() {
    let bus = Arc::new(EventBus::new(64));
    let mut events = bus.subscribe();
    let executor = ToolExecutor::with_standard_tools(bus, Arc::new(DirectSandbox::new_unchecked()));
    executor.set_shell_escalation(Arc::new(Approve), sandbox::composition::unsandboxed());
    let context = ToolExecutionContext {
        run_id: "host-environment".into(),
        thread_id: Some("host-environment".into()),
        call_id: None,
    };
    for interactive in [false, true] {
        for job in [false, true] {
            for large in [false, true] {
                let padding = if large {
                    "i=0; while [ $i -lt 400 ]; do printf 'padding line %s\\n' \"$i\"; i=$((i+1)); done; "
                } else {
                    ""
                };
                let script = format!(
                    "{padding}[ \"$EVORCH_NON_UTF8\" = \"$(printf 'native-\\377')\" ] || exit 9; printf '%s\\n' \"$HOME\" \"$GH_CONFIG_DIR\" \"$SSH_AUTH_SOCK\" \"$HTTPS_PROXY\" \"$EVORCH_CUSTOM_TOOL\" \"$GH_TOKEN\""
                );
                let mut args = json!({"command":script, "interactive":interactive, "timeout_ms":5000, "sandbox_access":"unsandboxed", "justification":"exercise the reviewed host environment fixture"});
                if job {
                    args["yield_ms"] = json!(0);
                }
                let mut result = invoke(&executor, &context, &mut events, args).await;
                assert_redacted(&result);
                if job {
                    let id = result.detail.as_ref().unwrap()["shell_job"]["job_id"].clone();
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        while result.detail.as_ref().unwrap()["shell_job"]["status"] == "running" {
                            let cursor =
                                result.detail.as_ref().unwrap()["shell_job"]["cursor"].clone();
                            result = invoke(
                            &executor,
                            &context,
                            &mut events,
                            json!({"action":"poll", "job_id":id, "cursor":cursor, "yield_ms":1000}),
                        )
                        .await;
                            assert_redacted(&result);
                        }
                    })
                    .await
                    .expect("job completion deadline");
                    assert_ne!(
                        result.detail.as_ref().unwrap()["shell_job"]["status"],
                        "running"
                    );
                    result = invoke(
                        &executor,
                        &context,
                        &mut events,
                        json!({"action":"poll", "job_id":id}),
                    )
                    .await;
                }
                assert!(!result.is_error, "{}", result.content);
                let output = if large {
                    let path = result.detail.as_ref().unwrap()["output_artifact"]["path"]
                        .as_str()
                        .unwrap();
                    std::fs::read_to_string(path).unwrap()
                } else {
                    result.content.clone()
                };
                assert!(!output.contains(SECRET));
                for expected in [
                    "/fixture/host-home",
                    "/fixture/gh-config",
                    "/fixture/agent.sock",
                    "http://fixture.invalid:8080",
                    "custom-tool-value",
                    "[REDACTED:known-credential-value]",
                ] {
                    assert!(
                        output.contains(expected),
                        "missing {expected}, interactive={interactive}, job={job}, large={large}: {output}"
                    );
                }
            }
        }
    }
}
