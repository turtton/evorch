mod support;

use config::EscalationApproval;
use event_bus::{DiagnosticSeverity, EventBus, EventKind, ToolEvent};
use providers::{ChatResponse, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use std::sync::{Arc, Mutex};
use tokio::sync::Barrier;
use tools::{ToolExecutionContext, ToolExecutor};

struct PausedReviewer {
    entered: Barrier,
    release: Barrier,
}

#[async_trait::async_trait]
impl AgentModel for PausedReviewer {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.entered.wait().await;
        self.release.wait().await;
        Ok(support::text_response(
            r#"{"approve":true}"#,
            FinishReason::Stop,
        ))
    }

    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "paused-reviewer".into()
    }
}

#[derive(Default)]
struct ProbeSandbox(Mutex<usize>);

impl Sandbox for ProbeSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        *self.0.lock().expect("probe") += 1;
        DirectSandbox::new_unchecked().wrap(spec)
    }
}

async fn review_with_settings(next: (EscalationApproval, bool)) {
    // Given: a shell executor whose approving reviewer pauses after taking the settings snapshot.
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    let host = Arc::new(ProbeSandbox::default());
    let executor = Arc::new(ToolExecutor::with_standard_tools(bus.clone(), host.clone()));
    let model = Arc::new(PausedReviewer {
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    let runtime = AgentRuntime::new(bus, executor.clone(), model.clone());
    executor.set_shell_escalation(runtime.shell_escalation_gate(), host.clone());
    let ctx = ToolExecutionContext {
        run_id: "race".into(),
        thread_id: None,
        call_id: None,
    };
    // When: settings are applied while review is suspended, then approval is released.
    let execution = executor.execute(
        &ctx,
        "shell",
        "call",
        serde_json::json!({
            "command":"printf approved", "require_escalated":true, "justification":"inspect"
        }),
    );
    let update = async {
        model.entered.wait().await;
        runtime.set_sandbox_escalation(next.0, next.1);
        model.release.wait().await;
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(execution, update)
    })
    .await
    .expect("review completes");
    let result = result.expect("execute");
    // Then: only an unchanged snapshot may reach unsandboxed execution.
    let changed = next != (EscalationApproval::Auto, false);
    assert_eq!(result.is_error, changed);
    assert_eq!(*host.0.lock().expect("probe"), usize::from(!changed));
    let diagnostic = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::Diagnostic(event) = events.recv().await.expect("event").kind
                && event.code == "escalation_review"
            {
                break event;
            }
        }
    })
    .await
    .expect("review diagnostic");
    assert_eq!(
        diagnostic.severity,
        if changed {
            DiagnosticSeverity::Warning
        } else {
            DiagnosticSeverity::Info
        }
    );
    if changed {
        assert!(diagnostic.detail.contains("settings changed during review"));
    }
}

#[tokio::test]
async fn escalation_approved_but_settings_flipped_to_off_during_review_is_denied() {
    review_with_settings((EscalationApproval::Off, false)).await;
}

#[tokio::test]
async fn escalation_approved_with_mode_changed_during_review_is_denied() {
    review_with_settings((EscalationApproval::User, false)).await;
}

#[tokio::test]
async fn escalation_approve_with_unchanged_settings_still_approves() {
    review_with_settings((EscalationApproval::Auto, false)).await;
}

#[tokio::test]
async fn escalation_approved_with_fallback_changed_during_review_is_denied() {
    review_with_settings((EscalationApproval::Auto, true)).await;
}

#[tokio::test]
async fn escalation_is_denied_when_settings_change_during_human_approval() {
    // Given: User approval or Auto denial followed by human fallback.
    for mode in [EscalationApproval::User, EscalationApproval::Auto] {
        let bus = Arc::new(EventBus::new(32));
        let mut events = bus.subscribe();
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
        ));
        let model = Arc::new(support::ScriptedModel::new([Ok(support::text_response(
            r#"{"approve":false}"#,
            FinishReason::Stop,
        ))]));
        let runtime = AgentRuntime::new(bus.clone(), executor, model);
        runtime.set_sandbox_escalation(mode, true);
        let gate = runtime.shell_escalation_gate();
        let ctx = ToolExecutionContext {
            run_id: "human".into(),
            thread_id: None,
            call_id: Some("call".into()),
        };
        // When: the operator disables escalation before resolving the human approval.
        let update = async {
            loop {
                if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) =
                    events.recv().await.expect("event").kind
                {
                    runtime.set_sandbox_escalation(EscalationApproval::Off, false);
                    bus.emit(event_bus::Event::new(ToolEvent::ApprovalResolved {
                        call_id,
                        approved: true,
                    }));
                    break;
                }
            }
        };
        use tools::tools::shell_escalation::{EscalationDecision, ShellEscalationGate};
        let (decision, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(gate.decide(&ctx, "pwd", "inspect"), update)
        })
        .await
        .expect("human review completes");
        // Then: stale human approval is denied as well.
        assert!(matches!(decision, EscalationDecision::Deny { .. }));
    }
}
