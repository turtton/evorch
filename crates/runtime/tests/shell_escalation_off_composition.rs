mod support;

use config::EscalationApproval;
use event_bus::EventBus;
use runtime::{ModelSource, RuntimeComposition, compose_runtime};
use sandbox::{CommandSpec, Sandbox, SandboxError, WrappedCommand};
use std::sync::{Arc, Mutex};
use tools::tools::{shell::Shell, shell_escalation::ShellEscalationGate};
use tools::{Permissions, Tool, ToolExecutionContext, ToolExecutor, ToolResult};

struct ProbeSandbox {
    inner: Arc<dyn Sandbox>,
    wraps: Arc<Mutex<usize>>,
}

impl Sandbox for ProbeSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        *self.wraps.lock().expect("probe") += 1;
        self.inner.wrap(spec)
    }
}

struct ObservedShell {
    shell: Shell,
    wraps: Arc<Mutex<usize>>,
}

#[async_trait::async_trait]
impl Tool for ObservedShell {
    fn name(&self) -> &str {
        self.shell.name()
    }
    fn schema(&self) -> serde_json::Value {
        self.shell.schema()
    }
    fn permissions(&self) -> Permissions {
        self.shell.permissions()
    }
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, tools::ToolError> {
        self.shell.execute(args).await
    }
    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, tools::ToolError> {
        self.shell.execute_with_context(ctx, args).await
    }
    fn set_shell_escalation(
        &self,
        gate: Arc<dyn ShellEscalationGate>,
        unsandboxed: Arc<dyn Sandbox>,
    ) {
        self.shell.set_shell_escalation(
            gate,
            Arc::new(ProbeSandbox {
                inner: unsandboxed,
                wraps: self.wraps.clone(),
            }),
        );
    }
}

#[tokio::test]
async fn off_mode_composes_gate_anyway_and_auto_enable_takes_effect_without_rebuild() {
    // Given: real composition starting Off, observing the unsandboxed path without replacing the gate.
    let bus = Arc::new(EventBus::new(64));
    let wraps = Arc::new(Mutex::new(0));
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(ObservedShell {
            shell: Shell::new(Arc::new(sandbox::DirectSandbox::new_unchecked())),
            wraps: wraps.clone(),
        }))
        .expect("register shell");
    let executor = Arc::new(executor);
    let dir = tempfile::tempdir().expect("credentials");
    let config = config::Config {
        sandbox: config::SandboxConfig {
            escalation_approval: EscalationApproval::Off,
            ..Default::default()
        },
        ..Default::default()
    };
    let model = Arc::new(support::ScriptedModel::new([Ok(support::text_response(
        r#"{"approve":true}"#,
        providers::FinishReason::Stop,
    ))]));
    let runtime = compose_runtime(RuntimeComposition {
        config: &config,
        bus,
        executor: executor.clone(),
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path()).expect("store"),
        ),
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(model.clone()),
        workspace: None,
    })
    .expect("compose")
    .runtime;
    let ctx = ToolExecutionContext {
        run_id: "off-composed".into(),
        thread_id: None,
        call_id: None,
    };
    // When: Off -> Auto -> Off is applied to the same executor.
    for (mode, expected_wraps) in [
        (EscalationApproval::Off, 0),
        (EscalationApproval::Auto, 1),
        (EscalationApproval::Off, 1),
    ] {
        runtime.set_sandbox_escalation(mode, false);
        let result = executor
            .execute(
                &ctx,
                "shell",
                "call",
                serde_json::json!({
                    "command":"printf composed", "sandbox_access":"unsandboxed", "justification":"inspect"
                }),
            )
            .await
            .expect("execute");
        // Then: only the enabled call reviews and wraps, while Off is denied by the installed gate.
        match mode {
            EscalationApproval::Off => {
                assert!(result.is_error);
                assert_eq!(result.content, "shell escalation is disabled");
            }
            EscalationApproval::Auto => {
                assert!(!result.is_error);
                assert_eq!(result.content, "exit_code: 0\ncomposed");
            }
            EscalationApproval::User => unreachable!("not part of this scenario"),
        }
        assert_eq!(*wraps.lock().expect("probe"), expected_wraps);
        assert_eq!(model.observed().await.len(), expected_wraps);
    }
}
