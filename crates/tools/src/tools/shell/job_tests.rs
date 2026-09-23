use super::*;
use sandbox::DirectSandbox;
use serde_json::{Value, json};

fn shell() -> Shell {
    Shell::new(Arc::new(DirectSandbox::new_unchecked()))
}
fn context(run_id: &str) -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: run_id.into(),
        thread_id: Some("thread".into()),
        call_id: Some("call".into()),
    }
}
async fn invoke(shell: &Shell, owner: &str, value: Value) -> ToolResult {
    shell
        .execute_with_context(&context(owner), value)
        .await
        .expect("shell result")
}
fn job(result: &ToolResult) -> &Value {
    &result.detail.as_ref().unwrap()["shell_job"]
}
fn id(result: &ToolResult) -> String {
    job(result)["job_id"].as_str().unwrap().into()
}
async fn finish(shell: &Shell, owner: &str, id: &str, mut cursor: u64) -> ToolResult {
    // Each wait is watch-driven; no sleeps or status-only busy loop.
    for _ in 0..20 {
        let result = invoke(
            shell,
            owner,
            json!({"action":"poll", "job_id":id, "cursor":cursor, "yield_ms":1000}),
        )
        .await;
        cursor = job(&result)["cursor"].as_u64().unwrap();
        if job(&result)["status"] != "running" {
            return result;
        }
    }
    panic!("job did not finish")
}

#[tokio::test]
async fn yielded_pipe_accepts_stdin_and_reports_completion_cursor() {
    let shell = shell();
    let start = invoke(&shell, "owner", json!({"command":"printf 'ready\\n'; IFS= read -r line; printf 'got:%s\\n' \"$line\"", "yield_ms":1000})).await;
    assert_eq!(job(&start)["status"], "running");
    assert!(start.content.contains("ready\n"));
    let cursor = job(&start)["cursor"].as_u64().unwrap();
    let answer = invoke(&shell, "owner", json!({"action":"stdin", "job_id":id(&start), "input":"hello\n", "cursor":cursor, "yield_ms":1000})).await;
    let end = if job(&answer)["status"] == "running" {
        finish(&shell, "owner", &id(&start), cursor).await
    } else {
        answer
    };
    assert_eq!(job(&end)["exit_code"], 0);
    let delta = invoke(
        &shell,
        "owner",
        json!({"action":"poll", "job_id":id(&start), "cursor":cursor}),
    )
    .await;
    assert!(delta.content.contains("got:hello"));
    assert!(!delta.content.contains("ready\n"));
    let empty = invoke(
        &shell,
        "owner",
        json!({"action":"poll", "job_id":id(&start), "cursor":job(&end)["cursor"]}),
    )
    .await;
    assert!(!empty.content.contains("got:hello"));
}

#[tokio::test]
async fn foreign_owner_and_restarted_registry_cannot_use_handle() {
    let shell = shell();
    let start = invoke(
        &shell,
        "owner",
        json!({"command":"read value", "yield_ms":0}),
    )
    .await;
    for other in ["another", ""] {
        assert!(
            shell
                .execute_with_context(
                    &context(other),
                    json!({"action":"stop", "job_id":id(&start)})
                )
                .await
                .is_err()
        );
    }
    let fresh = super::Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    assert!(
        fresh
            .execute_with_context(
                &context("owner"),
                json!({"action":"poll", "job_id":id(&start)})
            )
            .await
            .is_err()
    );
    shell.cancel_shell_jobs("owner");
    assert_eq!(
        job(&finish(&shell, "owner", &id(&start), 0).await)["status"],
        "cancelled"
    );
}

#[tokio::test]
async fn timeout_and_stop_terminate_job_without_replay() {
    for stop in [false, true] {
        let shell = shell();
        let start = invoke(
            &shell,
            "owner",
            json!({"command":"read value", "yield_ms":0, "timeout_ms":100}),
        )
        .await;
        if stop {
            invoke(
                &shell,
                "owner",
                json!({"action":"stop", "job_id":id(&start)}),
            )
            .await;
        }
        let end = finish(&shell, "owner", &id(&start), 0).await;
        assert!(end.is_error);
        assert_eq!(
            job(&end)["status"],
            if stop { "cancelled" } else { "timed_out" }
        );
        assert!(!shell.has_running_shell_jobs("owner"));
    }
}

#[tokio::test]
async fn snapshot_guard_outlives_yield_and_releases_after_stop() {
    struct Guard(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let shell = shell();
    let start = invoke(
        &shell,
        "owner",
        json!({"command":"read value", "yield_ms":0}),
    )
    .await;
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    shell.retain_shell_job_guard("owner", &id(&start), Box::new(Guard(Arc::clone(&released))));
    assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
    shell.cancel_shell_jobs("owner");
    finish(&shell, "owner", &id(&start), 0).await;
    assert!(released.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn pty_job_accepts_input_and_finishes() {
    let shell = shell();
    let start = invoke(&shell, "owner", json!({"command":"printf 'ready\\n'; IFS= read -r value; printf 'received:%s\\n' \"$value\"", "interactive":true, "yield_ms":1000})).await;
    assert_eq!(job(&start)["status"], "running");
    let response = invoke(
        &shell,
        "owner",
        json!({"action":"stdin", "job_id":id(&start), "input":"hello\n", "yield_ms":0}),
    )
    .await;
    let end = finish(
        &shell,
        "owner",
        &id(&start),
        job(&response)["cursor"].as_u64().unwrap(),
    )
    .await;
    assert_eq!(job(&end)["status"], "completed");
    let all = invoke(
        &shell,
        "owner",
        json!({"action":"poll", "job_id":id(&start)}),
    )
    .await;
    assert!(all.content.contains("received:hello"));
}

#[tokio::test]
async fn output_is_bounded_redacted_and_archived_once() {
    let shell = shell();
    let key = format!("sk-{}", "A".repeat(30));
    let command = format!(
        "printf '%s\\n' '{key}'; head -c 9000000 /dev/zero | tr '\\0' x; printf '\\ntail\\n'"
    );
    let start = invoke(&shell, "owner", json!({"command":command, "yield_ms":0})).await;
    let end = finish(&shell, "owner", &id(&start), 0).await;
    assert!(!end.content.contains(&key));
    assert!(end.content.len() < 16 * 1024);
    let detail = end.detail.as_ref().unwrap();
    assert_eq!(detail["output_artifact"]["complete"], false);
    let path = detail["output_artifact"]["path"].as_str().unwrap();
    assert!(path.starts_with("/var/tmp/evorch-output-"));
    let saved = std::fs::read_to_string(path).unwrap();
    assert!(!saved.contains(&key));
    assert!(saved.contains("[REDACTED:"));
    assert!(saved.len() <= crate::output::CAPTURE_BYTES);
    let again = invoke(
        &shell,
        "owner",
        json!({"action":"poll", "job_id":id(&start)}),
    )
    .await;
    assert_eq!(
        again.detail.as_ref().unwrap()["output_artifact"]["path"],
        path
    );
}

#[test]
fn job_schema_distinguishes_start_and_control() {
    let validator = jsonschema::validator_for(&shell().schema()).unwrap();
    for input in [
        json!({"command":"true", "yield_ms":0}),
        json!({"action":"poll", "job_id":"x", "cursor":3}),
        json!({"action":"stdin", "job_id":"x", "input":"a"}),
    ] {
        assert!(validator.is_valid(&input), "{input}");
    }
    for input in [
        json!({}),
        json!({"action":"poll"}),
        json!({"action":"poll", "job_id":"x", "command":"pwd"}),
        json!({"command":"true", "yield_ms":60001}),
        json!({"command":"true", "input":"a"}),
    ] {
        assert!(!validator.is_valid(&input), "{input}");
    }
}

#[tokio::test]
async fn drop_and_drain_reap_pipe_and_pty_before_releasing_guard() {
    struct Guard(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    for interactive in [false, true] {
        for drain in [false, true] {
            let shell = shell();
            let start = invoke(&shell, "owner", json!({"command":"printf 'pid:%s\\n' \"$$\"; read value", "interactive":interactive, "yield_ms":1000})).await;
            let pid: i32 = start
                .content
                .lines()
                .find_map(|line| line.strip_prefix("pid:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            shell.retain_shell_job_guard("owner", &id(&start), Box::new(Guard(Some(sender))));
            if drain {
                shell.drain_shell_jobs("owner").await.unwrap();
            }
            drop(shell);
            tokio::time::timeout(Duration::from_secs(5), receiver)
                .await
                .unwrap()
                .unwrap();
            assert!(
                rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap())
                    .is_err()
            );
        }
    }
}

#[tokio::test]
async fn live_private_key_body_is_redacted_across_lines() {
    let shell = shell();
    let start = invoke(&shell, "owner", json!({"command":"printf '%s\\n' '-----BEGIN PRIVATE KEY-----' 'DO_NOT_EXPOSE_THIS_BODY' '-----END PRIVATE KEY-----'; read value", "yield_ms":1000})).await;
    assert!(!start.content.contains("DO_NOT_EXPOSE"));
    shell.drain_shell_jobs("owner").await.unwrap();
    let end = invoke(
        &shell,
        "owner",
        json!({"action":"poll", "job_id":id(&start)}),
    )
    .await;
    assert!(!end.content.contains("DO_NOT_EXPOSE"));
}

#[tokio::test]
async fn escalated_stdin_requires_a_fresh_review() {
    struct Gate(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl ShellEscalationGate for Gate {
        async fn decide(
            &self,
            _ctx: &ToolExecutionContext,
            _command: &str,
            _justification: &str,
        ) -> EscalationDecision {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                EscalationDecision::Approve
            } else {
                EscalationDecision::Deny {
                    reason: "input review denied".into(),
                }
            }
        }
    }
    let shell = shell();
    let gate = Arc::new(Gate(std::sync::atomic::AtomicUsize::new(0)));
    shell.set_shell_escalation(gate.clone(), Arc::new(DirectSandbox::new_unchecked()));
    let start = invoke(&shell, "owner", json!({"command":"read value", "require_escalated":true, "justification":"test", "yield_ms":0})).await;
    let denied = invoke(
        &shell,
        "owner",
        json!({"action":"stdin", "job_id":id(&start), "input":"arbitrary code\n"}),
    )
    .await;
    assert!(denied.is_error);
    assert!(denied.content.contains("input review denied"));
    assert_eq!(gate.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    shell.drain_shell_jobs("owner").await.unwrap();
}

#[tokio::test]
async fn drain_does_not_acknowledge_unknown_side_effects() {
    let shell = shell();
    let start = invoke(
        &shell,
        "owner",
        json!({"command":"read value", "yield_ms":0}),
    )
    .await;
    assert!(shell.has_unobserved_shell_jobs("owner"));
    shell.drain_shell_jobs("owner").await.unwrap();
    assert!(!shell.has_running_shell_jobs("owner"));
    assert!(shell.has_unobserved_shell_jobs("owner"));
    let end = invoke(
        &shell,
        "owner",
        json!({"action":"poll", "job_id":id(&start)}),
    )
    .await;
    assert_eq!(job(&end)["status"], "cancelled");
    assert!(!shell.has_unobserved_shell_jobs("owner"));
}

#[tokio::test]
async fn running_job_capacity_is_bounded() {
    let shell = shell();
    for _ in 0..8 {
        let result = invoke(
            &shell,
            "owner",
            json!({"command":"read value", "yield_ms":0}),
        )
        .await;
        assert!(!result.is_error);
    }
    let excess = invoke(
        &shell,
        "owner",
        json!({"command":"echo forbidden", "yield_ms":0}),
    )
    .await;
    assert!(excess.is_error);
    assert!(excess.content.contains("capacity"));
    shell.drain_shell_jobs("owner").await.unwrap();
}

#[tokio::test]
async fn terminal_release_rejects_live_jobs_and_preserves_other_owners() {
    let shell = shell();
    let first = invoke(
        &shell,
        "first",
        json!({"command":"read value", "yield_ms":0}),
    )
    .await;
    invoke(
        &shell,
        "second",
        json!({"command":"read value", "yield_ms":0}),
    )
    .await;
    assert!(shell.release_shell_jobs("first").is_err());
    assert!(shell.has_running_shell_jobs("first"));
    shell.drain_shell_jobs("first").await.unwrap();
    assert!(shell.has_unobserved_shell_jobs("first"));
    shell.release_shell_jobs("first").unwrap();
    assert!(!shell.has_unobserved_shell_jobs("first"));
    assert!(shell.has_running_shell_jobs("second"));
    assert!(
        shell
            .execute_with_context(
                &context("first"),
                json!({"action":"poll", "job_id":id(&first)})
            )
            .await
            .is_err()
    );
    shell.drain_shell_jobs("second").await.unwrap();
    shell.release_shell_jobs("second").unwrap();
}
