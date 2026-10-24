//! Gate reasons remain auditable; only rendered tool results include guidance.
use super::*;
use event_bus::ToolEvent;
use serde_json::json;
use tools::tools::shell_escalation::{DENIAL_GUIDANCE, SUSPENSION_GUIDANCE};

fn assert_no_agent_instructions(reason: &str) {
    for instruction in [
        DENIAL_GUIDANCE,
        "do not reword",
        "Work inside the sandbox",
        "ask_user",
        "consult instead",
    ] {
        assert!(!reason.contains(instruction), "{reason}");
    }
}

#[tokio::test]
async fn shell_and_file_denials_keep_event_reasons_bare_and_guidance_once() {
    for outcome in ["off", "policy", "human_timeout", "suspended"] {
        for tool in ["shell", "read", "write", "edit", "grep"] {
            let mut fixture = Fixture::new(false);
            fixture
                .run
                .add_user_request(&fixture.ctx.run_id, "inspect the file");
            fixture.gate.human_timeout = Duration::ZERO;
            let expected = match outcome {
                "off" => {
                    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::Off, false);
                    format!(
                        "{} is disabled",
                        if tool == "shell" {
                            "shell escalation"
                        } else {
                            "file access escalation"
                        }
                    )
                }
                "human_timeout" => {
                    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::User, false);
                    format!(
                        "{} approval timed out",
                        if tool == "shell" {
                            "shell escalation"
                        } else {
                            "file access escalation"
                        }
                    )
                }
                "suspended" => {
                    fixture.suspend().await;
                    String::new()
                }
                _ => {
                    fixture.gate.reviewer = Some(Arc::new(QuickModelReviewer::new(Arc::new(
                        ScriptedModel::new([Ok(super::super::super::support::text_response(
                            r#"{"approve":false,"reason":"policy denial","risk_level":"low","authorization_level":"high"}"#,
                            providers::FinishReason::Stop,
                        ))]),
                    ))));
                    if tool == "shell" {
                        "policy denial".into()
                    } else {
                        "file access escalation denied: policy denial".into()
                    }
                }
            };
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let outside = dir.path().join("outside");
            std::fs::write(&outside, "must not be read or changed").unwrap();
            let executor = tools::ToolExecutor::with_standard_tools(
                fixture.gate.bus.clone(),
                Arc::new(sandbox::DirectSandbox::new_unchecked()),
            );
            executor.set_file_boundary(&sandbox::SandboxBoundary::new(project));
            executor.set_shell_escalation(
                Arc::new(fixture.gate),
                Arc::new(sandbox::DirectSandbox::new_unchecked()),
            );
            let mut events = fixture.runtime.shared.bus.subscribe();
            let mut args =
                json!({"path":outside, "sandbox_access":"unsandboxed", "justification":"inspect"});
            match tool {
                "shell" => {
                    args = json!({"command":"true", "sandbox_access":"unsandboxed", "justification":"inspect"})
                }
                "write" => args["content"] = json!("new"),
                "edit" => {
                    args["old_string"] = json!("must");
                    args["new_string"] = json!("new");
                }
                "grep" => args["pattern"] = json!("must"),
                _ => {}
            }
            let result = executor.execute(&fixture.ctx, tool, "denied", args).await;
            let text = if tool == "shell" {
                let result = result.unwrap();
                assert!(result.is_error);
                result.content
            } else {
                let error = result.unwrap_err();
                let tools::ToolError::ExecutionDenied { reason, .. } = &error else {
                    panic!("{error:?}")
                };
                assert_no_agent_instructions(reason);
                if outcome != "suspended" {
                    assert_eq!(reason, &expected);
                }
                assert!(!error.to_string().contains(DENIAL_GUIDANCE));
                error.agent_message()
            };
            assert_eq!(
                text.matches(DENIAL_GUIDANCE).count(),
                1,
                "{tool}/{outcome}: {text}"
            );
            if outcome == "suspended" {
                assert!(text.contains("refused without review"));
                assert_eq!(text.matches(SUSPENSION_GUIDANCE).count(), 1);
                assert_eq!(
                    text.matches("Work inside the sandbox can continue.")
                        .count(),
                    1
                );
            } else {
                assert!(text.contains(&expected), "{text}");
            }
            let mut denied_events = 0;
            for event in events.drain_pending_snapshot() {
                match event.kind {
                    EventKind::Tool(ToolEvent::ExecutionDenied { reason, .. }) => {
                        denied_events += 1;
                        assert_no_agent_instructions(&reason);
                        if outcome != "suspended" {
                            assert_eq!(reason, expected);
                        }
                    }
                    EventKind::Diagnostic(diagnostic) => {
                        assert_no_agent_instructions(&diagnostic.detail)
                    }
                    EventKind::Tool(ToolEvent::ToolCompleted { output, .. }) if tool != "shell" => {
                        assert!(!output.unwrap().contains(DENIAL_GUIDANCE));
                    }
                    _ => {}
                }
            }
            assert_eq!(denied_events, usize::from(tool != "shell"));
            assert_eq!(
                std::fs::read_to_string(outside).unwrap(),
                "must not be read or changed"
            );
        }
    }
}

#[tokio::test]
async fn ordinary_policy_and_approval_gates_still_have_no_escalation_guidance() {
    use sandbox::{ApprovalMode, ApprovalPolicy, PolicyDecision};

    for prepared in [false, true] {
        for outcome in ["policy", "missing_gate", "timeout"] {
            let bus = Arc::new(EventBus::new(32));
            let mut events = bus.subscribe();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file");
            std::fs::write(&path, "not executed").unwrap();
            let mut executor = tools::ToolExecutor::with_standard_tools(
                bus.clone(),
                Arc::new(sandbox::DirectSandbox::new_unchecked()),
            );
            executor.set_policy(
                ApprovalPolicy::standard(ApprovalMode::OnRequest).with_override(
                    "read",
                    if outcome == "policy" {
                        PolicyDecision::Deny
                    } else {
                        PolicyDecision::Ask
                    },
                ),
            );
            if outcome == "timeout" {
                executor.set_approval_gate(ApprovalGate::new(bus, Duration::ZERO));
            }
            let executor = Arc::new(executor);
            let ctx = ToolExecutionContext {
                run_id: "ordinary-gate".into(),
                thread_id: None,
                call_id: None,
            };
            let args = json!({"path":path});
            let error = if prepared {
                let result = executor
                    .validate_call(ctx, "read".into(), "denied".into(), args)
                    .unwrap()
                    .authorize()
                    .await;
                let Err(error) = result else {
                    panic!("ordinary gate must deny");
                };
                error
            } else {
                executor
                    .execute(&ctx, "read", "denied", args)
                    .await
                    .unwrap_err()
            };
            assert!(matches!(
                &error,
                tools::ToolError::ExecutionDenied { guidance: None, .. }
            ));
            assert_eq!(error.agent_message(), error.to_string());
            assert_no_agent_instructions(&error.agent_message());
            for event in events.drain_pending_snapshot() {
                if let EventKind::Tool(ToolEvent::ExecutionDenied { reason, .. }) = event.kind {
                    assert_no_agent_instructions(&reason);
                }
            }
        }
    }
}

#[tokio::test]
async fn empty_justification_preflight_still_has_no_guidance() {
    let fixture = Fixture::new(false);
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let executor = tools::ToolExecutor::with_standard_tools(
        fixture.gate.bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    );
    executor.set_file_boundary(&sandbox::SandboxBoundary::new(project));
    executor.set_shell_escalation(
        Arc::new(fixture.gate),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    );
    for tool in ["shell", "read"] {
        let args = if tool == "shell" {
            json!({"command":"true", "sandbox_access":"unsandboxed", "justification":" "})
        } else {
            json!({"path":dir.path().join("outside"), "sandbox_access":"unsandboxed", "justification":" "})
        };
        let text = match executor.execute(&fixture.ctx, tool, "invalid", args).await {
            Ok(result) => {
                assert!(result.is_error);
                result.content
            }
            Err(error) => error.agent_message(),
        };
        assert!(text.contains("justification is required"));
        assert!(!text.contains(DENIAL_GUIDANCE));
    }
}
