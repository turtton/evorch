use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::EventBus;
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::{Value, json};
use tools::tools::shell_escalation::{EscalationDecision, ShellEscalationGate};
use tools::{ToolExecutionContext, ToolExecutor, ToolResult};

#[derive(Default)]
struct ProbeSandbox(Mutex<Vec<CommandSpec>>);

impl Sandbox for ProbeSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        self.0.lock().expect("probe lock").push(spec.clone());
        DirectSandbox::new_unchecked().wrap(spec)
    }
}

#[derive(Default)]
struct Gate {
    reason: Option<String>,
    calls: Mutex<Vec<(ToolExecutionContext, String, String)>>,
}

#[async_trait]
impl ShellEscalationGate for Gate {
    async fn decide(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
    ) -> EscalationDecision {
        self.calls.lock().expect("gate lock").push((
            ctx.clone(),
            command.to_owned(),
            justification.to_owned(),
        ));
        match &self.reason {
            Some(reason) => EscalationDecision::Deny {
                reason: reason.clone(),
            },
            None => EscalationDecision::Approve,
        }
    }
}

struct Fixture {
    executor: ToolExecutor,
    default: Arc<ProbeSandbox>,
    unsandboxed: Arc<ProbeSandbox>,
    gate: Arc<Gate>,
}

impl Fixture {
    fn new(reason: Option<&str>) -> Self {
        let default = Arc::new(ProbeSandbox::default());
        let unsandboxed = Arc::new(ProbeSandbox::default());
        let gate = Arc::new(Gate {
            reason: reason.map(str::to_owned),
            ..Gate::default()
        });
        let executor =
            ToolExecutor::with_standard_tools(Arc::new(EventBus::new(16)), default.clone());
        executor.set_shell_escalation(gate.clone(), unsandboxed.clone());
        Self {
            executor,
            default,
            unsandboxed,
            gate,
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        self.executor
            .execute(
                &ToolExecutionContext {
                    run_id: "run-escalation".into(),
                    thread_id: Some("thread-escalation".into()),
                    call_id: None,
                },
                "shell",
                "call-escalation",
                args,
            )
            .await
            .expect("tool result")
    }

    fn assert_wraps(&self, default: usize, unsandboxed: usize) {
        assert_eq!(self.default.0.lock().expect("probe lock").len(), default);
        assert_eq!(
            self.unsandboxed.0.lock().expect("probe lock").len(),
            unsandboxed
        );
    }
}

// Given: an approving gate / When: justification is absent or blank / Then: no review or wrap occurs.
#[tokio::test]
async fn escalated_call_errors_before_any_review_when_justification_is_absent_or_blank() {
    for justification in [None, Some(""), Some(" \t\n")] {
        let fixture = Fixture::new(None);
        let mut args = json!({"command": "printf forbidden", "require_escalated": true});
        if let Some(justification) = justification {
            args["justification"] = json!(justification);
        }
        let result = fixture.execute(args).await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "shell escalation denied: justification is required"
        );
        assert!(fixture.gate.calls.lock().expect("gate lock").is_empty());
        fixture.assert_wraps(0, 0);
    }
}

// Given: two sandbox probes / When: review approves / Then: only the unsandboxed probe wraps.
#[tokio::test]
async fn approved_escalation_wraps_with_unsandboxed_sandbox_when_gate_approves() {
    for interactive in [false, true] {
        let fixture = Fixture::new(None);
        let result = fixture
            .execute(json!({
                "command": "printf", "args": ["approved"], "interactive": interactive,
                "require_escalated": true, "justification": "host access", "timeout_ms": 1000
            }))
            .await;
        assert!(!result.is_error);
        assert_eq!(
            result
                .content
                .strip_prefix("exit_code: 0\n")
                .expect("successful exit")
                .trim(),
            "approved"
        );
        fixture.assert_wraps(0, 1);
        assert_eq!(
            *fixture.gate.calls.lock().expect("gate lock"),
            vec![(
                ToolExecutionContext {
                    run_id: "run-escalation".into(),
                    thread_id: Some("thread-escalation".into()),
                    call_id: Some("call-escalation".into()),
                },
                "printf approved".into(),
                "host access".into(),
            )]
        );
    }
}

// Given: a denying gate / When: escalation is requested / Then: the exact reason returns without wrap/spawn.
#[tokio::test]
async fn denied_escalation_surfaces_reason_without_spawn_when_gate_denies() {
    let fixture = Fixture::new(Some("operator refused"));
    let result = fixture
        .execute(json!({
            "command": "printf forbidden", "require_escalated": true, "justification": "host access"
        }))
        .await;
    assert!(result.is_error);
    assert_eq!(result.content, "operator refused");
    fixture.assert_wraps(0, 0);
    assert_eq!(fixture.gate.calls.lock().expect("gate lock").len(), 1);
}

// Given: a contract-denied command / When: escalation is requested / Then: review is never invoked.
#[tokio::test]
async fn contract_denial_precedes_escalation_review_when_command_is_forbidden() {
    for justification in ["host access", ""] {
        let fixture = Fixture::new(None);
        let result = fixture
            .execute(json!({
                "command": "gh pr", "args": ["merge", "123"],
                "require_escalated": true, "justification": justification
            }))
            .await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .starts_with("shell command denied by contract:")
        );
        assert!(fixture.gate.calls.lock().expect("gate lock").is_empty());
        fixture.assert_wraps(0, 0);
    }
}

// Given: a configured gate / When: escalation is false or omitted / Then: only the default sandbox wraps.
#[tokio::test]
async fn non_escalated_call_uses_default_sandbox_when_escalation_is_false_or_omitted() {
    for require_escalated in [None, Some(false)] {
        let fixture = Fixture::new(Some("must not review"));
        let mut args = json!({"command": "printf normal"});
        if let Some(value) = require_escalated {
            args["require_escalated"] = json!(value);
        }
        let result = fixture.execute(args).await;
        assert_eq!(result.content, "exit_code: 0\nnormal");
        fixture.assert_wraps(1, 0);
        assert!(fixture.gate.calls.lock().expect("gate lock").is_empty());
    }
}
