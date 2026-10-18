//! Direct file tools in production runs are confined to the sandbox boundary
//! and leave it only through the shell escalation gate (ADR 0033).
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use agents::Role;
use config::EscalationApproval;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, ToolEvent};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::scratch::ThreadScratch;
use runtime::{AgentRuntime, ExecutionPolicy, RunConfig, SandboxFactory, WorkspaceMounts};
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::json;
use support::{ScriptedModel, init_git_repo, text_response, tool_response};
use tools::ToolExecutor;

const SECRET: &str = "outside-secret-token";

struct ScratchSandbox(Option<PathBuf>);

impl Sandbox for ScratchSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        let mut wrapped = DirectSandbox::new_unchecked().wrap(spec)?;
        if let Some(dir) = &self.0 {
            wrapped
                .env
                .push(("TMPDIR".into(), dir.to_string_lossy().into_owned()));
        }
        Ok(wrapped)
    }
}

struct ScratchFactory;

impl SandboxFactory for ScratchFactory {
    fn build(
        &self,
        _: &ExecutionPolicy,
        mounts: &WorkspaceMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(ScratchSandbox(mounts.scratch.clone())))
    }
}

fn tool_output(observed: &[Vec<Message>], call_id: &str) -> String {
    observed
        .iter()
        .flatten()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                ..
            } if tool_call_id == call_id => content.first().map(|item| match item {
                ToolResultContent::Text { text } => text.clone(),
                ToolResultContent::Image { .. } => panic!("unexpected image result"),
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {call_id}"))
}

struct Paths {
    _temps: Vec<tempfile::TempDir>,
    repo: PathBuf,
    outside: PathBuf,
    scratch: Arc<ThreadScratch>,
}

impl Paths {
    fn new() -> Self {
        let (repo_temp, repo) = init_git_repo();
        std::fs::write(repo.join("inside.txt"), "project file").unwrap();
        let outside_temp = tempfile::tempdir().unwrap();
        let outside = outside_temp.path().canonicalize().unwrap();
        std::fs::write(outside.join("secret.txt"), SECRET).unwrap();
        let scratch_temp = tempfile::tempdir().unwrap();
        let scratch = Arc::new(ThreadScratch::at(scratch_temp.path().join("scratch")));
        Self {
            _temps: vec![repo_temp, outside_temp, scratch_temp],
            repo,
            outside,
            scratch,
        }
    }

    fn runtime(
        &self,
        model: Arc<ScriptedModel>,
        approval: EscalationApproval,
    ) -> (AgentRuntime, Arc<EventBus>) {
        let bus = Arc::new(EventBus::new(512));
        let runtime = AgentRuntime::with_sandbox_factory(
            Arc::clone(&bus),
            Arc::new(ToolExecutor::with_standard_tools(
                Arc::clone(&bus),
                Arc::new(DirectSandbox::new_unchecked()),
            )),
            model,
            Arc::new(ScratchFactory),
        )
        .with_sequential_run_ids()
        .with_sandbox_root(self.repo.clone());
        runtime.set_thread_scratch(Arc::clone(&self.scratch));
        runtime.set_sandbox_escalation(approval, false);
        (runtime, bus)
    }
}

async fn run(runtime: &AgentRuntime, role: Role) -> AgentRunPhase {
    let run = runtime
        .delegate_chat("thread-a", role, "WORK".into(), RunConfig::default())
        .unwrap();
    runtime.wait(run).await.unwrap()
}

fn escalated(path: &std::path::Path) -> serde_json::Value {
    json!({"path": path, "sandbox_access": "unsandboxed", "justification": "the user asked for this file"})
}

fn assert_guided_denial(output: &str) {
    assert!(
        output.contains("sandbox_access: \"unsandboxed\""),
        "{output}"
    );
    assert!(output.contains("allowed directory"), "{output}");
    assert!(!output.contains(SECRET), "{output}");
}

const LOW_RISK: &str = r#"{"approve":true,"reason":"the user asked","risk_level":"low","authorization_level":"medium"}"#;

// Given: a Worker in a production-shaped run with automatic review
// When: it uses file tools inside, outside and on read-only parts of the boundary
// Then: only boundary paths work without review, and outside access needs an approval
#[tokio::test]
async fn worker_file_tools_follow_the_sandbox_boundary_and_reviewed_escalation() {
    let paths = Paths::new();
    let secret = paths.outside.join("secret.txt");
    let new_outside = paths.outside.join("new.txt");
    let denied_outside = paths.outside.join("denied.txt");
    let scratch_note = paths.scratch.thread_dir("thread-a").join("note.txt");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "outside-read",
            "read",
            json!({"path": secret}),
        )),
        Ok(tool_response("escalated-read", "read", escalated(&secret))),
        Ok(text_response(LOW_RISK, FinishReason::Stop)),
        Ok(tool_response(
            "outside-write",
            "write",
            json!({"path": new_outside, "content": "x"}),
        )),
        Ok(tool_response(
            "declined-write",
            "write",
            json!({"path": denied_outside, "content": "x", "sandbox_access": "unsandboxed", "justification": "store notes"}),
        )),
        Ok(text_response(
            r#"{"approve":false,"reason":"not requested by the user","risk_level":"medium","authorization_level":"none"}"#,
            FinishReason::Stop,
        )),
        Ok(tool_response(
            "escalated-write",
            "write",
            json!({"path": new_outside, "content": "approved", "sandbox_access": "unsandboxed", "justification": "the user asked for notes"}),
        )),
        Ok(text_response(LOW_RISK, FinishReason::Stop)),
        Ok(tool_response(
            "project-read",
            "read",
            json!({"path": "inside.txt"}),
        )),
        Ok(tool_response(
            "scratch-write",
            "shell",
            json!({"command": "printf scratch > \"$TMPDIR/note.txt\""}),
        )),
        Ok(tool_response(
            "scratch-read",
            "read",
            json!({"path": scratch_note}),
        )),
        Ok(tool_response(
            "system-read",
            "read",
            json!({"path": "/etc/passwd"}),
        )),
        Ok(tool_response(
            "readonly-write",
            "write",
            json!({"path": "/etc/evorch-boundary-probe", "content": "x"}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _) = paths.runtime(model.clone(), EscalationApproval::Auto);

    assert_eq!(run(&runtime, Role::Worker).await, AgentRunPhase::Done);

    let observed = model.observed().await;
    assert_guided_denial(&tool_output(&observed, "outside-read"));
    assert!(tool_output(&observed, "escalated-read").contains(SECRET));
    assert_guided_denial(&tool_output(&observed, "outside-write"));
    let declined = tool_output(&observed, "declined-write");
    assert!(declined.contains("not requested by the user"), "{declined}");
    assert!(!denied_outside.exists());
    assert_eq!(std::fs::read_to_string(&new_outside).unwrap(), "approved");
    assert!(tool_output(&observed, "project-read").contains("project file"));
    assert!(tool_output(&observed, "scratch-read").contains("scratch"));
    assert!(tool_output(&observed, "system-read").contains("root"));
    let read_only = tool_output(&observed, "readonly-write");
    assert!(read_only.contains("read-only"), "{read_only}");
    assert!(!PathBuf::from("/etc/evorch-boundary-probe").exists());
}

fn respond_to_file_access(
    bus: &Arc<EventBus>,
    answers: Vec<bool>,
) -> tokio::task::JoinHandle<Vec<serde_json::Value>> {
    let bus = Arc::clone(bus);
    let mut events = bus.subscribe();
    tokio::spawn(async move {
        let mut seen = Vec::new();
        for approved in answers {
            loop {
                let event = events.recv().await.expect("approval request");
                if let EventKind::Tool(ToolEvent::ApprovalRequested {
                    call_id,
                    input: Some(input),
                    ..
                }) = event.kind
                    && input["kind"] == "file_access"
                {
                    seen.push(input);
                    bus.emit(Event::new(ToolEvent::ApprovalResolved {
                        call_id,
                        approved,
                    }));
                    break;
                }
            }
        }
        seen
    })
}

// Given: user approval mode / When: the user rejects and then approves outside reads
// Then: only the approved call returns content, and the request names tool, path and reason
#[tokio::test]
async fn user_mode_decides_each_outside_access_from_the_approval_answer() {
    let paths = Paths::new();
    let secret = paths.outside.join("secret.txt");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("rejected", "read", escalated(&secret))),
        Ok(tool_response("approved", "read", escalated(&secret))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = paths.runtime(model.clone(), EscalationApproval::User);
    let responder = respond_to_file_access(&bus, vec![false, true]);

    assert_eq!(run(&runtime, Role::Worker).await, AgentRunPhase::Done);

    let requests = responder.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["tool"], "read");
    assert_eq!(requests[0]["access"], "read");
    assert_eq!(requests[0]["path"], secret.to_string_lossy().as_ref());
    assert_eq!(requests[0]["justification"], "the user asked for this file");
    let observed = model.observed().await;
    let rejected = tool_output(&observed, "rejected");
    assert!(
        rejected.contains("denied by user") && !rejected.contains(SECRET),
        "{rejected}"
    );
    assert!(tool_output(&observed, "approved").contains(SECRET));
}

// Given: escalation disabled / When: a Worker asks for an outside read / Then: it is denied
#[tokio::test]
async fn off_mode_denies_every_outside_access() {
    let paths = Paths::new();
    let secret = paths.outside.join("secret.txt");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("off", "read", escalated(&secret))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _) = paths.runtime(model.clone(), EscalationApproval::Off);

    assert_eq!(run(&runtime, Role::Worker).await, AgentRunPhase::Done);

    let output = tool_output(&model.observed().await, "off");
    assert!(
        output.contains("disabled") && !output.contains(SECRET),
        "{output}"
    );
}

// Given: a read-only role and an approving reviewer / When: it requests an escalated write
// Then: role capabilities still refuse the tool, and the reviewer is never consulted
#[tokio::test]
async fn escalation_never_grants_a_tool_the_role_lacks() {
    let paths = Paths::new();
    let target = paths.repo.join("explorer.txt");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "explorer-write",
            "write",
            json!({"path": target, "content": "x", "sandbox_access": "unsandboxed", "justification": "need it"}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _) = paths.runtime(model.clone(), EscalationApproval::Auto);

    assert_eq!(run(&runtime, Role::Explorer).await, AgentRunPhase::Done);

    let observed = model.observed().await;
    assert_eq!(observed.len(), 2, "no reviewer request was made");
    let output = tool_output(&observed, "explorer-write");
    assert!(!output.contains("file access escalation"), "{output}");
    assert!(!target.exists());
}
