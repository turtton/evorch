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
    run: runtime::RunId,
    executor: Arc<ToolExecutor>,
    model: Arc<ScriptedModel>,
    bus: Arc<EventBus>,
    sandbox: Arc<ProbeSandbox>,
    host: Arc<ProbeSandbox>,
}

impl Fixture {
    fn new(mode: EscalationApproval, verdict: &str) -> Self {
        Self::with_verdicts(mode, &[verdict])
    }

    fn with_verdicts(mode: EscalationApproval, verdicts: &[&str]) -> Self {
        let bus = Arc::new(EventBus::new(64));
        let sandbox = Arc::new(ProbeSandbox::default());
        let host = Arc::new(ProbeSandbox::default());
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            sandbox.clone(),
        ));
        let model = Arc::new(ScriptedModel::new(verdicts.iter().map(|verdict| {
            let verdict = if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(verdict)
            {
                value["risk_level"] = "low".into();
                value["authorization_level"] = "high".into();
                value.to_string()
            } else {
                (*verdict).to_owned()
            };
            Ok(text_response(&verdict, providers::FinishReason::Stop))
        })));
        let runtime = AgentRuntime::new(bus.clone(), executor.clone(), model.clone());
        let run = support::review_host_run(&runtime);
        runtime.set_sandbox_escalation(mode, false);
        let gate = runtime.shell_escalation_gate();
        executor.set_shell_escalation(gate, host.clone());
        Self {
            runtime,
            run,
            executor,
            model,
            bus,
            sandbox,
            host,
        }
    }

    async fn execute(&self) -> tools::ToolResult {
        self.execute_command("printf approved").await
    }

    async fn execute_command(&self, command: &str) -> tools::ToolResult {
        self.executor.execute(
            &ToolExecutionContext { run_id: self.run.to_string(), thread_id: None, call_id: None },
            "shell", "call-shell",
            json!({"command":command, "sandbox_access":"unsandboxed", "justification":"host inspection"}),
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
    let fixture = Fixture::new(
        EscalationApproval::Auto,
        r#"{"approve":true,"risk_level":"low","authorization_level":"high"}"#,
    );
    let result = fixture.execute().await;
    assert!(!result.is_error);
    assert_eq!(result.content, "exit_code: 0\napproved");
    fixture.assert_wraps(1);
}

// Given: denying reviewer / When: shell executes / Then: reason returns without wrapping or spawning.
#[tokio::test]
async fn escalated_shell_denied_returns_reason_and_spawn_never_happens() {
    let fixture = Fixture::new(
        EscalationApproval::Auto,
        r#"{"approve":false,"reason":"unsafe"}"#,
    );
    let result = fixture.execute().await;
    assert!(result.is_error);
    assert!(result.content.starts_with("unsafe\n"), "{}", result.content);
    fixture.assert_wraps(0);
}

// Given: a reviewer that denies three times / When: a run keeps asking with new commands /
// Then: the fourth is refused without asking the reviewer, and every denial carries guidance.
#[tokio::test]
async fn consecutive_reviewer_denials_suspend_automatic_review_for_the_run() {
    let deny = r#"{"approve":false,"reason":"not requested"}"#;
    let fixture = Fixture::with_verdicts(EscalationApproval::Auto, &[deny, deny, deny]);
    // The fixture cancels the run's agent loop; let it finish first so its
    // termination cannot clear the streak in the middle of the test.
    let _ = fixture.runtime.wait(fixture.run).await;
    for (index, command) in ["printf one", "printf two", "printf three", "printf four"]
        .into_iter()
        .enumerate()
    {
        let result = fixture.execute_command(command).await;
        assert!(result.is_error, "{command}");
        let expected = if index < 3 {
            "not requested\n"
        } else {
            "shell escalation refused without review: the automatic reviewer denied 3 consecutive requests in this run (last: not requested)."
        };
        assert!(result.content.starts_with(expected), "{}", result.content);
        use tools::tools::shell_escalation::{DENIAL_GUIDANCE, SUSPENSION_GUIDANCE};
        let guidance = if index < 3 {
            DENIAL_GUIDANCE
        } else {
            SUSPENSION_GUIDANCE
        };
        assert!(result.content.ends_with(guidance), "{}", result.content);
        assert_eq!(result.content.matches(DENIAL_GUIDANCE).count(), 1);
    }
    assert_eq!(fixture.model.observed().await.len(), 3);
    fixture.assert_wraps(0);
}

// Given: user mode and scoped responder / When: shell executes / Then: approval uses run correlation.
#[tokio::test]
async fn escalated_shell_user_mode_uses_scoped_approval_responder() {
    let fixture = Fixture::new(EscalationApproval::User, "unused");
    let responder = support::spawn_run_scoped_approval_responder(
        fixture.bus.clone(),
        fixture.bus.subscribe(),
        format!("{}:", fixture.run),
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
    let fixture = Fixture::new(
        EscalationApproval::Auto,
        r#"{"approve":true,"risk_level":"low","authorization_level":"high"}"#,
    );
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
async fn composed_executor_escalates_when_auto_reviewer_approves() {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(ProbeSandbox::default()),
    ));
    let dir = tempfile::tempdir().expect("credentials");
    let config = config::Config::default();
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(dir.path().join("empty-user-config")),
        config: &config,
        bus,
        executor: executor.clone(),
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path()).expect("store"),
        ),
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(Arc::new(ScriptedModel::new([Ok(text_response(
            r#"{"approve":true,"risk_level":"low","authorization_level":"high"}"#,
            providers::FinishReason::Stop,
        ))]))),
        workspace: None,
    })
    .expect("compose")
    .runtime;
    let run = support::review_host_run(&runtime);
    let result = executor.execute(&ToolExecutionContext { run_id: run.to_string(), thread_id: None, call_id: None }, "shell", "call", json!({"command":"printf composed", "sandbox_access":"unsandboxed","justification":"inspect"})).await.expect("execute");
    assert_eq!(result.content, "exit_code: 0\ncomposed");
    assert_eq!(
        runtime.execution_policy(agents::Role::Worker),
        ExecutionPolicy::for_role(agents::Role::Worker)
    );
}
