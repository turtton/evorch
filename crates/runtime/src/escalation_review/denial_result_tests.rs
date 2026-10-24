//! Gate reasons remain auditable; only rendered tool results include guidance.
use super::*;
use event_bus::ToolEvent;
use serde_json::json;
use tools::tools::shell_escalation::DENIAL_GUIDANCE;

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
                assert!(!reason.contains(DENIAL_GUIDANCE));
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
            } else {
                assert!(text.contains(&expected), "{text}");
            }
            let mut denied_events = 0;
            for event in events.drain_pending_snapshot() {
                match event.kind {
                    EventKind::Tool(ToolEvent::ExecutionDenied { reason, .. }) => {
                        denied_events += 1;
                        assert!(!reason.contains(DENIAL_GUIDANCE));
                        if outcome != "suspended" {
                            assert_eq!(reason, expected);
                        }
                    }
                    EventKind::Diagnostic(diagnostic) => {
                        assert!(!diagnostic.detail.contains(DENIAL_GUIDANCE))
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
