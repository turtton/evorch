use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::EventBus;
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::{Value, json};
use tools::tools::shell_escalation::{EscalationDecision, ShellAccess, ShellEscalationGate};
use tools::{ToolExecutionContext, ToolExecutor, ToolResult};

#[derive(Default)]
struct ProbeSandbox(Mutex<Vec<CommandSpec>>, Option<PathBuf>);

impl Sandbox for ProbeSandbox {
    fn wrap(&self, mut spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        if let Some(bin) = &self.1 {
            // Only the fixture directory is searched, so these tests can never
            // fall back to an installed gh or its authentication/network access.
            spec.program = bin.join("sh").to_string_lossy().into_owned();
            spec.extra_env
                .push(("PATH".into(), bin.to_string_lossy().into_owned()));
        }
        self.0.lock().expect("probe lock").push(spec.clone());
        DirectSandbox::new_unchecked().wrap(spec)
    }
}

struct FakeGh(tempfile::TempDir);

impl FakeGh {
    fn new() -> Self {
        // Nix sandboxes do not provide /bin/sh; resolve the actual executable
        // before restricting PATH to this fixture's gh and sh.
        let search_path = std::env::var_os("PATH").expect("shell search path");
        let shell = std::env::split_paths(&search_path)
            .filter_map(|dir| std::fs::canonicalize(dir.join("sh")).ok())
            .find(|path| {
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .expect("executable sh in PATH");
        let dir = tempfile::tempdir().expect("fake gh directory");
        let gh = dir.path().join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!{}\nprintf '%s\\n' \"$@\" > \"$0.called\"\nprintf 'merge executed\\n'\n",
                shell.display()
            ),
        )
        .expect("fake gh script");
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700))
            .expect("executable fake gh");
        std::os::unix::fs::symlink(shell, dir.path().join("sh"))
            .expect("fixture shell interpreter");
        Self(dir)
    }

    fn fixture(&self, reason: Option<&str>) -> Fixture {
        Fixture::with_bin(reason, Some(self.0.path()))
    }

    fn invocation(&self) -> PathBuf {
        self.0.path().join("gh.called")
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
        Self::with_bin(reason, None)
    }

    fn with_bin(reason: Option<&str>, bin: Option<&Path>) -> Self {
        let default = Arc::new(ProbeSandbox(Mutex::default(), bin.map(Path::to_path_buf)));
        let unsandboxed = Arc::new(ProbeSandbox(Mutex::default(), bin.map(Path::to_path_buf)));
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
async fn reviewed_call_errors_before_any_review_when_justification_is_absent_or_blank() {
    for access in ["network", "unsandboxed"] {
        for justification in [None, Some(""), Some(" \t\n")] {
            let fake = FakeGh::new();
            let fixture = fake.fixture(None);
            let mut args = json!({"command": "gh pr merge 123", "sandbox_access": access});
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
            assert!(!fake.invocation().exists());
        }
    }
}

// Given: a merge request / When: review approves / Then: fake gh runs with the reviewed arguments.
#[tokio::test]
async fn approved_merge_reaches_review_and_runs_only_in_the_selected_sandbox() {
    let sha = "a".repeat(40);
    let command = format!("gh pr merge 123 --repo owner/repo --squash --match-head-commit {sha}");
    for interactive in [false, true] {
        for wrapped in [false, true] {
            let fake = FakeGh::new();
            let fixture = fake.fixture(None);
            let mut args = json!({
                "command": command,
                "sandbox_access": "unsandboxed",
                "justification": "User requested merging this PR after CI passed",
                "interactive": interactive
            });
            if wrapped {
                args["command"] = json!("sh");
                args["args"] = json!(["-c", format!("'{command}'")]);
            }
            let result = fixture.execute(args).await;
            assert!(!result.is_error, "{}", result.content);
            assert_eq!(
                std::fs::read_to_string(fake.invocation()).expect("fake gh executed"),
                format!(
                    "pr\nmerge\n123\n--repo\nowner/repo\n--squash\n--match-head-commit\n{sha}\n"
                )
            );
            fixture.assert_wraps(0, 1);
            let calls = fixture.gate.calls.lock().expect("gate lock");
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].0,
                ToolExecutionContext {
                    run_id: "run-escalation".into(),
                    thread_id: Some("thread-escalation".into()),
                    call_id: Some("call-escalation".into()),
                }
            );
            let reviewed_command = if wrapped {
                format!("sh -c '{}'", command)
            } else {
                command.clone()
            };
            assert_eq!(calls[0].1, reviewed_command);
            assert_eq!(calls[0].2, "User requested merging this PR after CI passed");
            assert_eq!(
                *fixture.gate.scopes.lock().expect("gate lock"),
                vec![ShellAccess::Host]
            );
        }
    }
}

// Given: a merge request / When: review denies / Then: neither sandbox nor fake gh executes.
#[tokio::test]
async fn denied_merge_is_reviewed_without_spawning_gh() {
    let fake = FakeGh::new();
    let fixture = fake.fixture(Some("merge refused by reviewer"));
    let result = fixture
        .execute(json!({
            "command": "gh pr", "args": ["merge", "123"],
            "sandbox_access": "unsandboxed", "justification": "merge requested PR"
        }))
        .await;
    assert!(result.is_error);
    assert_eq!(result.content, "merge refused by reviewer");
    fixture.assert_wraps(0, 0);
    assert_eq!(fixture.gate.calls.lock().expect("gate lock").len(), 1);
    assert!(!fake.invocation().exists());
}

// Given: no reviewer / When: a merge asks for host access / Then: escalation fails before spawn.
#[tokio::test]
async fn merge_escalation_without_a_reviewer_fails_closed() {
    let fake = FakeGh::new();
    let mut fixture = fake.fixture(None);
    fixture.executor =
        ToolExecutor::with_standard_tools(Arc::new(EventBus::new(16)), fixture.default.clone());
    let result = fixture
        .execute(json!({
            "command": "gh pr merge 123", "sandbox_access": "unsandboxed",
            "justification": "merge requested PR"
        }))
        .await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "shell escalation denied: no escalation gate configured"
    );
    fixture.assert_wraps(0, 0);
    assert!(!fake.invocation().exists());
}

// Given: a contract-denied command / When: escalation is requested / Then: review is never invoked.
#[tokio::test]
async fn contract_denial_precedes_escalation_review_when_command_is_forbidden() {
    for justification in ["host access", ""] {
        let fixture = Fixture::new(None);
        let result = fixture
            .execute(json!({
                "command": "gh issue", "args": ["close", "123"],
                "sandbox_access": "unsandboxed", "justification": justification
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

// Given: a merge request / When: access is isolated or omitted / Then: only the default sandbox wraps.
#[tokio::test]
async fn isolated_or_omitted_merge_access_uses_default_sandbox_without_review() {
    for access in [None, Some("isolated")] {
        let fake = FakeGh::new();
        let fixture = fake.fixture(Some("must not review"));
        let mut args = json!({"command": "gh pr merge 123"});
        if let Some(value) = access {
            args["sandbox_access"] = json!(value);
        }
        let result = fixture.execute(args).await;
        assert_eq!(result.content, "exit_code: 0\nmerge executed\n");
        assert_eq!(
            std::fs::read_to_string(fake.invocation()).expect("fake gh executed"),
            "pr\nmerge\n123\n"
        );
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
            "command": "pwd", "cwd": "nested", "sandbox_access": "unsandboxed",
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
        json!({"command":"printf network", "sandbox_access":"network", "justification":"fetch dependencies"}),
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
async fn network_only_denial_and_invalid_scope_never_spawn() {
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
        "command":"printf denied", "sandbox_access":"network", "justification":"fetch dependencies"
    })).await.expect("tool result");
    assert!(denied.is_error);
    assert_eq!(denied.content, "network refused");
    let invalid = executor
        .execute(
            &ctx,
            "shell",
            "call-invalid",
            json!({
                "command":"printf denied", "sandbox_access":"network_and_unsandboxed",
                "justification":"fetch dependencies"
            }),
        )
        .await
        .expect_err("invalid access value");
    assert!(matches!(invalid, tools::ToolError::InvalidArgs { .. }));
    assert_eq!(gate.scopes.lock().expect("gate").len(), 1);
    assert!(isolated.0.lock().expect("probe").is_empty());
    assert!(network.0.lock().expect("probe").is_empty());
    assert!(host.0.lock().expect("probe").is_empty());
}
