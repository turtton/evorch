use super::*;
use config::EscalationApproval;
use event_bus::{Event, EventBus, EventKind, ToolEvent};
use std::sync::Mutex;
use tools::ToolExecutionContext;
use tools::tools::shell_escalation::{EscalationDecision, ShellAccess, ShellEscalationGate};

fn context() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "run-review".into(),
        thread_id: None,
        call_id: Some("call-review".into()),
    }
}

fn fixture(
    mode: EscalationApproval,
    fallback: bool,
    text: Option<&str>,
) -> (SandboxEscalationGate, Arc<support::ScriptedModel>) {
    let model =
        Arc::new(support::ScriptedModel::new(text.map(|text| {
            Ok(support::text_response(text, providers::FinishReason::Stop))
        })));
    let gate = SandboxEscalationGate::new(
        Arc::new(Mutex::new((mode, fallback))),
        Some(Arc::new(QuickModelReviewer::new(model.clone()))),
        Arc::new(EventBus::new(32)),
    );
    (gate, model)
}

// Given: Off / When: deciding / Then: review is never invoked.
#[tokio::test]
async fn escalation_off_denies_without_calling_reviewer() {
    let (gate, model) = fixture(EscalationApproval::Off, true, None);
    assert!(matches!(
        gate.decide(&context(), "pwd", "inspect").await,
        EscalationDecision::Deny { .. }
    ));
    assert!(model.observed().await.is_empty());
}

// Given: a structured approval / When: deciding / Then: escalation is granted.
#[tokio::test]
async fn auto_approve_grants_escalation() {
    let (gate, _) = fixture(EscalationApproval::Auto, false, Some(r#"{"approve":true}"#));
    assert!(matches!(
        gate.decide(&context(), "pwd", "inspect").await,
        EscalationDecision::Approve
    ));
}

// Given: a denial / When: fallback is disabled / Then: its reason is preserved.
#[tokio::test]
async fn auto_deny_without_fallback_denies_with_reason() {
    let (gate, _) = fixture(
        EscalationApproval::Auto,
        false,
        Some(r#"{"approve":false,"reason":"unsafe"}"#),
    );
    assert!(
        matches!(gate.decide(&context(), "pwd", "inspect").await, EscalationDecision::Deny { reason } if reason == "unsafe")
    );
}

// Given: a failed provider / When: deciding / Then: failure cannot approve.
#[tokio::test]
async fn auto_error_without_fallback_denies_fail_closed() {
    let (gate, _) = fixture(EscalationApproval::Auto, false, None);
    assert!(matches!(
        gate.decide(&context(), "pwd", "inspect").await,
        EscalationDecision::Deny { .. }
    ));
}

// Given: invalid JSON / When: deciding / Then: parsing fails closed.
#[tokio::test]
async fn auto_invalid_verdict_denies_fail_closed() {
    let (gate, _) = fixture(EscalationApproval::Auto, false, Some("invalid"));
    assert!(matches!(
        gate.decide(&context(), "pwd", "inspect").await,
        EscalationDecision::Deny { .. }
    ));
}

async fn respond(gate: &SandboxEscalationGate, approved: bool) -> tokio::task::JoinHandle<()> {
    let bus = gate.bus.clone();
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            if let EventKind::Tool(ToolEvent::ApprovalRequested {
                tool_name,
                call_id,
                input,
            }) = rx.recv().await.expect("event").kind
            {
                assert_eq!(tool_name, "shell");
                assert_eq!(call_id, "run-review:call-review");
                assert_eq!(
                    input,
                    Some(
                        serde_json::json!({"command":"pwd", "justification":"inspect", "kind":"shell_escalation"})
                    )
                );
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved,
                }));
                break;
            }
        }
    })
}

// Given: auto non-approval and fallback / When: human responds / Then: human outcome is final.
#[tokio::test]
async fn auto_non_approve_with_fallback_asks_user_then_applies_outcome() {
    for text in [Some(r#"{"approve":false}"#), Some("invalid"), None] {
        for approved in [false, true] {
            let (gate, _) = fixture(EscalationApproval::Auto, true, text);
            let responder = respond(&gate, approved).await;
            let decision = gate.decide(&context(), "pwd", "inspect").await;
            assert_eq!(matches!(decision, EscalationDecision::Approve), approved);
            responder.await.expect("responder");
        }
    }
}

// An automatic decline followed by user approval is a pending handoff, not a final warning.
#[tokio::test]
async fn fallback_approval_records_info_then_final_approval() {
    let (gate, _) = fixture(
        EscalationApproval::Auto,
        true,
        Some(r#"{"approve":false,"reason":"needs user review"}"#),
    );
    let mut events = gate.bus.subscribe();
    let responder = respond(&gate, true).await;
    assert!(matches!(
        gate.decide(&context(), "pwd", "inspect").await,
        EscalationDecision::Approve
    ));
    responder.await.expect("responder");
    let mut diagnostics = Vec::new();
    while diagnostics.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("diagnostic deadline")
            .expect("diagnostic event");
        if let EventKind::Diagnostic(diagnostic) = event.kind {
            diagnostics.push(diagnostic);
        }
    }
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].severity, event_bus::DiagnosticSeverity::Info);
    assert!(diagnostics[0].detail.contains("requesting user approval"));
    assert_eq!(diagnostics[1].severity, event_bus::DiagnosticSeverity::Info);
    assert!(diagnostics[1].detail.ends_with("approved"));
}

// Given: user mode / When: human approves / Then: no model is called.
#[tokio::test]
async fn user_mode_asks_user_without_reviewer() {
    let (gate, model) = fixture(EscalationApproval::User, false, None);
    let responder = respond(&gate, true).await;
    assert!(matches!(
        gate.decide(&context(), "pwd", "inspect").await,
        EscalationDecision::Approve
    ));
    responder.await.expect("responder");
    assert!(model.observed().await.is_empty());
}

// Given: no human responder / When: deadline expires / Then: deny.
#[tokio::test]
async fn user_mode_timeout_denies() {
    let (mut gate, _) = fixture(EscalationApproval::User, false, None);
    gate.human_timeout = Duration::ZERO;
    assert!(matches!(
        gate.decide(&context(), "pwd", "inspect").await,
        EscalationDecision::Deny { .. }
    ));
}

// Given: both verdicts / When: deciding / Then: correlated diagnostic severity matches.
#[tokio::test]
async fn approved_and_denied_decisions_emit_escalation_diagnostics() {
    for (text, severity) in [
        (r#"{"approve":true}"#, event_bus::DiagnosticSeverity::Info),
        (
            r#"{"approve":false}"#,
            event_bus::DiagnosticSeverity::Warning,
        ),
    ] {
        let (gate, _) = fixture(EscalationApproval::Auto, false, Some(text));
        let mut rx = gate.bus.subscribe();
        gate.decide(&context(), "pwd", "inspect").await;
        let EventKind::Diagnostic(event) = rx.recv().await.expect("diagnostic").kind else {
            panic!("diagnostic")
        };
        assert_eq!(event.source, "sandbox");
        assert_eq!(event.code, "escalation_review");
        assert_eq!(event.severity, severity);
        assert_eq!(event.run_id.as_deref(), Some("run-review"));
        assert_eq!(event.call_id.as_deref(), Some("call-review"));
        assert!(!event.detail.contains("run_id="));
        assert!(!event.detail.contains("call_id="));
        if severity == event_bus::DiagnosticSeverity::Info {
            assert_eq!(event.detail, "approved");
        } else {
            assert!(event.detail.starts_with("denied: "));
        }
    }
}

// Given: default policy / When: builders override it / Then: defaults and explicit settings differ.
#[test]
fn execution_policy_escalation_defaults_and_builders() {
    let policy = crate::ExecutionPolicy::for_role(agents::Role::Worker);
    assert_eq!(policy.escalation_approval, EscalationApproval::Auto);
    assert!(!policy.escalate_to_user_on_deny);
    let policy = policy
        .with_escalation_approval(EscalationApproval::User)
        .with_escalate_to_user_on_deny(true);
    assert_eq!(policy.escalation_approval, EscalationApproval::User);
    assert!(policy.escalate_to_user_on_deny);
}

#[tokio::test]
async fn network_only_human_approval_identifies_its_scope() {
    let (gate, model) = fixture(EscalationApproval::User, false, None);
    let bus = gate.bus.clone();
    let mut rx = bus.subscribe();
    let responder = tokio::spawn(async move {
        loop {
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, input, .. }) =
                rx.recv().await.expect("approval event").kind
            {
                assert_eq!(input.expect("input")["kind"], "shell_network");
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved: true,
                }));
                break;
            }
        }
    });
    let decision = gate
        .decide_scoped_with_cwd(
            &context(),
            "git pull --ff-only",
            "sandbox DNS failed",
            None,
            ShellAccess::Network,
        )
        .await;
    assert!(matches!(decision, EscalationDecision::Approve));
    responder.await.expect("responder");
    assert!(model.observed().await.is_empty());
}

fn file_request() -> tools::tools::shell_escalation::FileAccessRequest {
    tools::tools::shell_escalation::FileAccessRequest {
        tool: "write".into(),
        paths: vec![std::path::PathBuf::from("/home/user/notes/todo.md")],
        write: true,
        justification: "user asked to update the notes".into(),
        change: Some("- [ ] ship".into()),
    }
}

const LOW_RISK_APPROVAL: &str =
    r#"{"approve":true,"reason":"requested","risk_level":"low","authorization_level":"low"}"#;

// Given: Off / When: a file access is requested / Then: it is denied without review.
#[tokio::test]
async fn file_access_off_denies_without_calling_reviewer() {
    let (gate, model) = fixture(EscalationApproval::Off, true, Some(LOW_RISK_APPROVAL));
    let decision = gate.decide_file_access(&context(), &file_request()).await;
    assert!(
        matches!(decision, EscalationDecision::Deny { reason } if reason == "file access escalation is disabled")
    );
    assert!(model.observed().await.is_empty());
}

// Given: auto mode / When: reviewing / Then: the reviewer sees tool, path, reason and change.
#[tokio::test]
async fn file_access_auto_review_receives_request_and_applies_verdict() {
    for (verdict, approved) in [
        (LOW_RISK_APPROVAL, true),
        (
            r#"{"approve":true,"reason":"credentials","risk_level":"critical","authorization_level":"high"}"#,
            false,
        ),
        (
            r#"{"approve":true,"reason":"broad","risk_level":"high","authorization_level":"low"}"#,
            false,
        ),
        (r#"{"approve":true}"#, false),
    ] {
        let (gate, model) = fixture(EscalationApproval::Auto, false, Some(verdict));
        let decision = gate.decide_file_access(&context(), &file_request()).await;
        assert_eq!(
            matches!(decision, EscalationDecision::Approve),
            approved,
            "{verdict}"
        );
        let observed = model.observed().await;
        let providers::ContentBlock::Text { text } = &observed[0][1].content[0] else {
            panic!("review payload")
        };
        let payload: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["tool"], "write");
        assert_eq!(payload["access"], "write");
        assert_eq!(
            payload["paths"],
            serde_json::json!(["/home/user/notes/todo.md"])
        );
        assert_eq!(payload["justification"], "user asked to update the notes");
        assert_eq!(payload["change"], "- [ ] ship");
    }
}

async fn respond_file_access(
    gate: &SandboxEscalationGate,
    approved: bool,
) -> tokio::task::JoinHandle<()> {
    let bus = gate.bus.clone();
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            if let EventKind::Tool(ToolEvent::ApprovalRequested {
                tool_name,
                call_id,
                input,
            }) = rx.recv().await.expect("event").kind
            {
                assert_eq!(tool_name, "write");
                assert_eq!(call_id, "run-review:call-review");
                assert_eq!(
                    input,
                    Some(serde_json::json!({
                        "kind":"file_access", "tool":"write", "path":"/home/user/notes/todo.md",
                        "access":"write", "justification":"user asked to update the notes",
                        "change":"- [ ] ship",
                    }))
                );
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved,
                }));
                break;
            }
        }
    })
}

// Given: user mode, or auto with a decline and fallback / When: the user answers /
// Then: the user's answer decides, and user mode never calls the reviewer.
#[tokio::test]
async fn file_access_user_and_fallback_follow_the_user_answer() {
    for approved in [false, true] {
        let (gate, model) = fixture(EscalationApproval::User, false, None);
        let responder = respond_file_access(&gate, approved).await;
        let decision = gate.decide_file_access(&context(), &file_request()).await;
        assert_eq!(matches!(decision, EscalationDecision::Approve), approved);
        responder.await.expect("responder");
        assert!(model.observed().await.is_empty());

        let (gate, model) = fixture(
            EscalationApproval::Auto,
            true,
            Some(
                r#"{"approve":false,"reason":"ask","risk_level":"medium","authorization_level":"none"}"#,
            ),
        );
        let responder = respond_file_access(&gate, approved).await;
        let decision = gate.decide_file_access(&context(), &file_request()).await;
        assert_eq!(matches!(decision, EscalationDecision::Approve), approved);
        responder.await.expect("responder");
        assert_eq!(model.observed().await.len(), 1);
    }
}

// Given: auto decline without fallback / When: deciding / Then: the reviewer's reason is final.
#[tokio::test]
async fn file_access_auto_decline_without_fallback_keeps_reason() {
    let (gate, _) = fixture(
        EscalationApproval::Auto,
        false,
        Some(
            r#"{"approve":false,"reason":"unrelated to the task","risk_level":"medium","authorization_level":"none"}"#,
        ),
    );
    assert!(matches!(
        gate.decide_file_access(&context(), &file_request()).await,
        EscalationDecision::Deny { reason } if reason == "unrelated to the task"
    ));
}
