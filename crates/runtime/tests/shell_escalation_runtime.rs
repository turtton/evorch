mod support;

use std::sync::{Arc, Mutex};

use config::EscalationApproval;
use event_bus::EventBus;
use runtime::{AgentRuntime, ExecutionPolicy, ModelSource, RuntimeComposition, compose_runtime};
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::json;
use support::{ScriptedModel, text_response};
use tools::{ToolExecutionContext, ToolExecutor};

#[derive(Default)]
struct ProbeSandbox(Mutex<Vec<CommandSpec>>);

impl Sandbox for ProbeSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        self.0.lock().expect("probe").push(spec.clone());
        DirectSandbox::new_unchecked().wrap(spec)
    }
}

struct Fixture {
    runtime: AgentRuntime,
    executor: Arc<ToolExecutor>,
    model: Arc<ScriptedModel>,
    bus: Arc<EventBus>,
    sandbox: Arc<ProbeSandbox>,
    host: Arc<ProbeSandbox>,
}

impl Fixture {
    fn new(mode: EscalationApproval, verdict: &str) -> Self {
        let bus = Arc::new(EventBus::new(64));
        let sandbox = Arc::new(ProbeSandbox::default());
        let host = Arc::new(ProbeSandbox::default());
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            sandbox.clone(),
        ));
        let model = Arc::new(ScriptedModel::new([Ok(text_response(
            verdict,
            providers::FinishReason::Stop,
        ))]));
        let runtime = AgentRuntime::new(bus.clone(), executor.clone(), model.clone());
        runtime.set_sandbox_escalation(mode, false);
        let gate = runtime.shell_escalation_gate();
        executor.set_shell_escalation(gate, host.clone());
        Self {
            runtime,
            executor,
            model,
            bus,
            sandbox,
            host,
        }
    }

    async fn execute(&self) -> tools::ToolResult {
        self.executor.execute(
            &ToolExecutionContext { run_id: "run-shell".into(), thread_id: None, call_id: None },
            "shell", "call-shell",
            json!({"command":"printf approved", "require_escalated":true, "justification":"host inspection"}),
        ).await.expect("execute")
    }

    fn assert_wraps(&self, host: usize) {
        assert!(self.sandbox.0.lock().expect("probe").is_empty());
        assert_eq!(self.host.0.lock().expect("probe").len(), host);
    }
}

// Given: approving reviewer and two probes / When: shell executes / Then: only host path runs.
#[tokio::test]
async fn escalated_shell_approved_runs_via_unsandboxed_and_returns_output() {
    let fixture = Fixture::new(EscalationApproval::Quick, r#"{"approve":true}"#);
    let result = fixture.execute().await;
    assert!(!result.is_error);
    assert_eq!(result.content, "exit_code: 0\napproved");
    fixture.assert_wraps(1);
}

// Given: denying reviewer / When: shell executes / Then: reason returns without wrapping or spawning.
#[tokio::test]
async fn escalated_shell_denied_returns_reason_and_spawn_never_happens() {
    let fixture = Fixture::new(
        EscalationApproval::Quick,
        r#"{"approve":false,"reason":"unsafe"}"#,
    );
    let result = fixture.execute().await;
    assert!(result.is_error);
    assert_eq!(result.content, "unsafe");
    fixture.assert_wraps(0);
}

// Given: user mode and scoped responder / When: shell executes / Then: approval uses run correlation.
#[tokio::test]
async fn escalated_shell_user_mode_uses_scoped_approval_responder() {
    let fixture = Fixture::new(EscalationApproval::User, "unused");
    let responder = support::spawn_run_scoped_approval_responder(
        fixture.bus.clone(),
        fixture.bus.subscribe(),
        "run-shell:".into(),
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), fixture.execute())
        .await
        .expect("scoped response");
    assert!(!result.is_error);
    responder.await.expect("responder");
    assert!(fixture.model.observed().await.is_empty());
    fixture.assert_wraps(1);
}

// Given: an already wired executor / When: settings switch Off / Then: no reviewer or host execution.
#[tokio::test]
async fn escalated_shell_live_setting_change_takes_effect_without_rebuild() {
    let fixture = Fixture::new(EscalationApproval::Quick, r#"{"approve":true}"#);
    fixture
        .runtime
        .set_sandbox_escalation(EscalationApproval::Off, false);
    let result = fixture.execute().await;
    assert!(result.is_error);
    assert!(fixture.model.observed().await.is_empty());
    fixture.assert_wraps(0);
    assert_eq!(
        fixture
            .runtime
            .execution_policy(agents::Role::Worker)
            .escalation_approval,
        EscalationApproval::Off
    );
}

// Given: fixed model composition / When: using its supplied executor / Then: composition installs the gate.
#[tokio::test]
async fn composed_executor_escalates_when_quick_reviewer_approves() {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(ProbeSandbox::default()),
    ));
    let dir = tempfile::tempdir().expect("credentials");
    let config = config::Config::default();
    let runtime = compose_runtime(RuntimeComposition {
        config: &config,
        bus,
        executor: executor.clone(),
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path()).expect("store"),
        ),
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(Arc::new(ScriptedModel::new([Ok(text_response(
            r#"{"approve":true}"#,
            providers::FinishReason::Stop,
        ))]))),
        workspace: None,
    })
    .expect("compose")
    .runtime;
    let result = executor.execute(&ToolExecutionContext { run_id: "composed".into(), thread_id: None, call_id: None }, "shell", "call", json!({"command":"printf composed", "require_escalated":true,"justification":"inspect"})).await.expect("execute");
    assert_eq!(result.content, "exit_code: 0\ncomposed");
    assert_eq!(
        runtime.execution_policy(agents::Role::Worker),
        ExecutionPolicy::for_role(agents::Role::Worker)
    );
}
