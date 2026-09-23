use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::EventBus;
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::{Value, json};
use tools::tools::shell_escalation::{EscalationDecision, ShellAccess, ShellEscalationGate};
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
    cwd_calls: Mutex<Vec<Option<std::path::PathBuf>>>,
    scopes: Mutex<Vec<ShellAccess>>,
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

    async fn decide_with_cwd(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
        cwd: Option<&std::path::Path>,
    ) -> EscalationDecision {
        self.cwd_calls
            .lock()
            .expect("gate lock")
            .push(cwd.map(std::path::Path::to_path_buf));
        self.decide(ctx, command, justification).await
    }

    async fn decide_scoped_with_cwd(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
        cwd: Option<&std::path::Path>,
        access: ShellAccess,
    ) -> EscalationDecision {
        self.scopes.lock().expect("gate lock").push(access);
        self.decide_with_cwd(ctx, command, justification, cwd).await
    }
}

struct NetworkSandbox {
    isolated: Arc<ProbeSandbox>,
    network: Arc<ProbeSandbox>,
}

impl Sandbox for NetworkSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        self.isolated.wrap(spec)
    }

    fn with_network_access(&self) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(self.network.clone())
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

// Given: a relative shell cwd / When: escalation is reviewed / Then: the gate sees the resolved path.
#[tokio::test]
async fn escalation_review_receives_resolved_working_directory() {
    let fixture = Fixture::new(None);
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(root.path().join("nested")).expect("nested");
    fixture.executor.set_default_cwd(root.path().to_path_buf());
    let result = fixture
        .execute(json!({
            "command": "pwd", "cwd": "nested", "require_escalated": true,
            "justification": "inspect worktree"
        }))
        .await;
    assert!(!result.is_error);
    assert_eq!(
        *fixture.gate.cwd_calls.lock().expect("gate lock"),
        vec![Some(root.path().join("nested"))]
    );
}

// Network requests use the separate reviewed sandbox and never enter the host path.
#[tokio::test]
async fn network_only_request_keeps_the_host_path_unused() {
    let isolated = Arc::new(ProbeSandbox::default());
    let network = Arc::new(ProbeSandbox::default());
    let host = Arc::new(ProbeSandbox::default());
    let gate = Arc::new(Gate::default());
    let executor = ToolExecutor::with_standard_tools(
        Arc::new(EventBus::new(16)),
        Arc::new(NetworkSandbox {
            isolated: isolated.clone(),
            network: network.clone(),
        }),
    );
    executor.set_shell_escalation(gate.clone(), host.clone());
    let result = executor.execute(
        &ToolExecutionContext { run_id: "run-network".into(), thread_id: None, call_id: None },
        "shell", "call-network",
        json!({"command":"printf network", "require_network":true, "justification":"fetch dependencies"}),
    ).await.expect("tool result");
    assert_eq!(result.content, "exit_code: 0\nnetwork");
    assert!(isolated.0.lock().expect("probe").is_empty());
    assert_eq!(network.0.lock().expect("probe").len(), 1);
    assert!(host.0.lock().expect("probe").is_empty());
    assert_eq!(
        *gate.scopes.lock().expect("gate"),
        vec![ShellAccess::Network]
    );
}

#[tokio::test]
async fn network_only_denial_and_ambiguous_scope_never_spawn() {
    let isolated = Arc::new(ProbeSandbox::default());
    let network = Arc::new(ProbeSandbox::default());
    let host = Arc::new(ProbeSandbox::default());
    let gate = Arc::new(Gate {
        reason: Some("network refused".into()),
        ..Gate::default()
    });
    let executor = ToolExecutor::with_standard_tools(
        Arc::new(EventBus::new(16)),
        Arc::new(NetworkSandbox {
            isolated: isolated.clone(),
            network: network.clone(),
        }),
    );
    executor.set_shell_escalation(gate.clone(), host.clone());
    let ctx = ToolExecutionContext {
        run_id: "run-network".into(),
        thread_id: None,
        call_id: None,
    };
    let denied = executor.execute(&ctx, "shell", "call-denied", json!({
        "command":"printf denied", "require_network":true, "justification":"fetch dependencies"
    })).await.expect("tool result");
    assert!(denied.is_error);
    assert_eq!(denied.content, "network refused");
    let ambiguous = executor
        .execute(
            &ctx,
            "shell",
            "call-ambiguous",
            json!({
                "command":"printf denied", "require_network":true, "require_escalated":true,
                "justification":"fetch dependencies"
            }),
        )
        .await
        .expect("tool result");
    assert!(ambiguous.is_error);
    assert!(ambiguous.content.contains("mutually exclusive"));
    assert_eq!(gate.scopes.lock().expect("gate").len(), 1);
    assert!(isolated.0.lock().expect("probe").is_empty());
    assert!(network.0.lock().expect("probe").is_empty());
    assert!(host.0.lock().expect("probe").is_empty());
}
