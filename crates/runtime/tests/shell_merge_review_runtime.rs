mod support;

use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use agents::Role;
use config::EscalationApproval;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, ToolEvent};
use providers::{ContentBlock, FinishReason, Role as MessageRole};
use runtime::{AgentRuntime, RunConfig, escalation_review::REVIEW_INSTRUCTION};
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::{Value, json};
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

struct MergeProbeSandbox {
    commands: Mutex<Vec<CommandSpec>>,
    bin: PathBuf,
}

impl Sandbox for MergeProbeSandbox {
    fn wrap(&self, mut spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        self.commands.lock().expect("commands").push(spec.clone());
        // No real gh can be reached, even if the wrong sandbox path is selected.
        spec.program = self.bin.join("sh").display().to_string();
        spec.extra_env
            .push(("PATH".into(), self.bin.display().to_string()));
        spec.extra_env.push((
            "EVORCH_MERGE_PROBE_MARKER".into(),
            self.bin.join("merged").display().to_string(),
        ));
        DirectSandbox::new_unchecked().wrap(spec)
    }
}

// Scripted verdicts verify runtime authorization evidence and execution routing,
// not whether a real model follows the reviewer instruction.
#[tokio::test]
async fn delegated_merge_uses_real_user_context_and_executes_only_on_review_approval() {
    const REQUEST: &str = "Implement the fix, open a PR, and merge it after CI passes.";
    const DELEGATION: &str = "Merge PR 42 after confirming its CI and head SHA.";
    const COMMAND: &str = "gh pr merge 42 --repo example/project --squash --match-head-commit 0123456789abcdef0123456789abcdef01234567";
    let shell = std::env::split_paths(&std::env::var_os("PATH").expect("host PATH"))
        .filter_map(|dir| std::fs::canonicalize(dir.join("sh")).ok())
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .expect("executable sh on host PATH");

    for approve in [true, false] {
        let dir = tempfile::tempdir().expect("fake gh directory");
        symlink(&shell, dir.path().join("sh")).expect("fixture shell");
        let gh = dir.path().join("gh");
        std::fs::write(&gh, format!("#!{}\nprintf '%s\\n' \"$*\" > \"$EVORCH_MERGE_PROBE_MARKER\"\nprintf 'fake merge completed'\n", shell.display())).expect("fake gh");
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).expect("executable");
        let sandbox = Arc::new(MergeProbeSandbox {
            commands: Mutex::new(Vec::new()),
            bin: dir.path().into(),
        });
        let host = Arc::new(MergeProbeSandbox {
            commands: Mutex::new(Vec::new()),
            bin: dir.path().into(),
        });
        let bus = Arc::new(EventBus::new(128));
        let mut events = bus.subscribe();
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            sandbox.clone(),
        ));
        let model = Arc::new(ScriptedModel::new([Ok(text_response(
            &json!({"approve": approve, "reason": "reviewed merge", "risk_level": "high", "authorization_level": "high"}).to_string(),
            FinishReason::Stop,
        ))]));
        model
            .add_keyed(
                REQUEST,
                [Ok(text_response("delegation prepared", FinishReason::Stop))],
            )
            .await;
        model.add_keyed(DELEGATION, [
            Ok(tool_response("merge-call", "shell", json!({"command": COMMAND, "sandbox_access": "unsandboxed", "justification": "deliver the reviewed change"}))),
            Ok(text_response("finished", FinishReason::Stop)),
        ]).await;
        let runtime = AgentRuntime::new(bus, executor.clone(), model.clone());
        runtime.set_sandbox_escalation(EscalationApproval::Auto, false);
        executor.set_shell_escalation(runtime.shell_escalation_gate(), host.clone());

        let root =
            runtime.delegate_background(Role::Orchestrator, REQUEST.into(), RunConfig::default());
        assert_eq!(runtime.wait(root).await, Ok(AgentRunPhase::Done));
        let child = runtime
            .delegate_background_as_child(root, Role::Worker, DELEGATION, RunConfig::default())
            .expect("registered child");
        assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));

        let observed = model.observed().await;
        let reviews: Vec<_> = observed
            .iter()
            .filter(|messages| {
                messages.iter().any(|message| {
                    message.role == MessageRole::System
                        && message.content
                            == vec![ContentBlock::Text {
                                text: REVIEW_INSTRUCTION.into(),
                            }]
                })
            })
            .collect();
        assert_eq!(reviews.len(), 1, "merge must reach the automatic reviewer");
        let payload: Value = serde_json::from_str(
            reviews[0]
                .iter()
                .find_map(|message| {
                    (message.role == MessageRole::User)
                        .then(|| {
                            message.content.iter().find_map(|block| match block {
                                ContentBlock::Text { text } => Some(text),
                                _ => None,
                            })
                        })
                        .flatten()
                })
                .expect("review payload"),
        )
        .expect("payload JSON");
        assert_eq!(payload["command"], COMMAND);
        assert_eq!(payload["access"], "host_unsandboxed");
        assert_eq!(payload["context"]["root_run_id"], root.to_string());
        assert_eq!(
            payload["context"]["real_user_requests"],
            json!([{ "target_run_id": root.to_string(), "text": REQUEST }])
        );
        assert_eq!(payload["context"]["delegation_chain"], json!([DELEGATION]));
        assert!(sandbox.commands.lock().expect("sandbox probe").is_empty());
        {
            let commands = host.commands.lock().expect("host probe");
            assert_eq!(commands.len(), usize::from(approve));
            if approve {
                assert_eq!(commands[0].args, vec!["-c", COMMAND]);
                assert_eq!(
                    std::fs::read_to_string(dir.path().join("merged")).expect("fake gh ran"),
                    format!("{}\n", COMMAND.strip_prefix("gh ").expect("gh command"))
                );
            } else {
                assert!(
                    !dir.path().join("merged").exists(),
                    "denial must not spawn fake gh"
                );
            }
        }

        let mut output = None;
        loop {
            let event = events.recv().await.expect("run events");
            match event.kind {
                EventKind::Tool(ToolEvent::ApprovalRequested { .. }) => {
                    panic!("automatic merge review must not request GUI approval")
                }
                EventKind::Tool(ToolEvent::ToolCompleted {
                    call_id,
                    is_error,
                    output: text,
                    ..
                }) if call_id == "merge-call" => {
                    assert_eq!(is_error, !approve);
                    output = text;
                }
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id,
                    to: AgentRunPhase::Done,
                    ..
                }) if run_id == child.to_string() => break,
                _ => {}
            }
        }
        assert_eq!(
            output.as_deref(),
            Some(if approve {
                "exit_code: 0\nfake merge completed"
            } else {
                "reviewed merge"
            })
        );
    }
}
