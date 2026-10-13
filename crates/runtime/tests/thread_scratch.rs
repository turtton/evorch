//! Thread-scoped scratch shared through `$TMPDIR` (ADR 0033).
mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agents::Role;
use config::EscalationApproval;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::scratch::ThreadScratch;
use runtime::workspace::{Project, WorktreeManager};
use runtime::{AgentRuntime, ExecutionPolicy, IsolatedMounts, RunConfig, SandboxFactory};
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::json;
use support::{ScriptedModel, init_git_repo, text_response, tool_response};
use tools::ToolExecutor;

/// Exports the mounted scratch as `TMPDIR`, as the bwrap sandbox does.
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

#[derive(Default)]
struct ScratchFactory(Mutex<Vec<IsolatedMounts>>);

impl SandboxFactory for ScratchFactory {
    fn build(
        &self,
        _: &ExecutionPolicy,
        mounts: &IsolatedMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        self.0.lock().unwrap().push(mounts.clone());
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
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {call_id}"))
}

// Given: an orchestrator thread whose workers write and read $TMPDIR, and another thread
// When: the runs execute sandboxed, escalated and file-tool reads of the same file
// Then: every run of the first thread sees one directory and the other thread does not
#[tokio::test]
async fn thread_runs_share_tmpdir_with_children_escalation_and_file_tools() {
    let (_repo_temp, repo) = init_git_repo();
    let base = tempfile::tempdir().unwrap();
    let scratch = Arc::new(ThreadScratch::at(base.path().join("scratch")));
    let shared = scratch.thread_dir("thread-a");
    let other = scratch.thread_dir("thread-b");
    let file = shared.join("handoff.txt");
    let file = file.to_str().unwrap();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "delegate-writer",
            "delegate",
            json!({"target": {"role": "worker"}, "prompt": "WRITE"}),
        )),
        Ok(tool_response(
            "write",
            "shell",
            json!({"command": "printf shared > \"$TMPDIR/handoff.txt\""}),
        )),
        Ok(text_response("written", FinishReason::Stop)),
        Ok(tool_response("parent-read", "read", json!({"path": file}))),
        Ok(tool_response(
            "delegate-reader",
            "delegate",
            json!({"target": {"role": "worker"}, "prompt": "READ"}),
        )),
        Ok(tool_response(
            "sandboxed-cat",
            "shell",
            json!({"command": "printf '%s:' \"$TMPDIR\"; cat \"$TMPDIR/handoff.txt\""}),
        )),
        Ok(tool_response(
            "escalated-cat",
            "shell",
            json!({
                "command": "printf '%s:' \"$TMPDIR\"; cat \"$TMPDIR/handoff.txt\"",
                "sandbox_access": "unsandboxed",
                "justification": "read the shared temporary file on the host"
            }),
        )),
        Ok(text_response(
            r#"{"approve":true,"reason":"reads a file this thread wrote","risk_level":"low","authorization_level":"low"}"#,
            FinishReason::Stop,
        )),
        Ok(tool_response("child-read", "read", json!({"path": file}))),
        Ok(text_response("verified", FinishReason::Stop)),
        Ok(tool_response("finish", "finish", json!({"result": "done"}))),
        Ok(tool_response(
            "other-thread",
            "shell",
            json!({"command": "printf '%s:' \"$TMPDIR\"; test ! -e \"$TMPDIR/handoff.txt\" && printf absent"}),
        )),
        Ok(text_response("checked", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(256));
    let factory = Arc::new(ScratchFactory::default());
    let runtime = AgentRuntime::with_workspace_context(
        Arc::clone(&bus),
        Arc::new(ToolExecutor::with_standard_tools(
            bus,
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        model.clone(),
        WorktreeManager::new(Project::new(repo.clone()).unwrap()),
        factory.clone(),
    )
    .with_sequential_run_ids()
    .with_sandbox_root(repo);
    runtime.set_thread_scratch(Arc::clone(&scratch));
    runtime.set_sandbox_escalation(EscalationApproval::Auto, false);

    let first = runtime.reserve_run_id();
    runtime.bind_thread_root("thread-a", first).unwrap();
    runtime.spawn_reserved(
        first,
        None,
        Role::Orchestrator,
        "SHARE",
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(first).await, Ok(AgentRunPhase::Done));
    let second = runtime.reserve_run_id();
    runtime.bind_thread_root("thread-b", second).unwrap();
    runtime.spawn_reserved(second, None, Role::Worker, "CHECK", RunConfig::default());
    assert_eq!(runtime.wait(second).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let shared_text = shared.to_str().unwrap();
    assert_eq!(tool_output(&observed, "write"), "exit_code: 0\n");
    assert!(tool_output(&observed, "parent-read").contains("shared"));
    for call in ["sandboxed-cat", "escalated-cat"] {
        assert_eq!(
            tool_output(&observed, call),
            format!("exit_code: 0\n{shared_text}:shared"),
            "{call}"
        );
    }
    assert!(tool_output(&observed, "child-read").contains("shared"));
    assert_eq!(
        tool_output(&observed, "other-thread"),
        format!("exit_code: 0\n{}:absent", other.display())
    );
    let mounts = factory.0.lock().unwrap();
    assert_eq!(
        mounts
            .iter()
            .map(|mounts| mounts.scratch.clone().unwrap())
            .collect::<Vec<_>>(),
        vec![
            shared.clone(),
            shared.clone(),
            shared.clone(),
            other.clone()
        ],
        "root, both children, then the other thread"
    );
    let note = format!("Temporary files (evorch): $TMPDIR is {shared_text}.");
    let system = observed[0]
        .iter()
        .find(|message| message.role == providers::Role::System)
        .expect("system message");
    assert!(
        system
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text.starts_with(&note))),
        "{system:?}"
    );
}

// Given: a root run bound to a thread / When: the thread is rebound to a new root run
// Then: the new root resolves the same scratch directory
#[tokio::test]
async fn rebinding_a_thread_root_keeps_its_scratch_directory() {
    let (_repo_temp, repo) = init_git_repo();
    let base = tempfile::tempdir().unwrap();
    let scratch = Arc::new(ThreadScratch::at(base.path().join("scratch")));
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response("first", FinishReason::Stop)),
        Ok(text_response("second", FinishReason::Stop)),
        Ok(text_response("unbound", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(256));
    let factory = Arc::new(ScratchFactory::default());
    let runtime = AgentRuntime::with_workspace_context(
        Arc::clone(&bus),
        Arc::new(ToolExecutor::with_standard_tools(
            bus,
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        model,
        WorktreeManager::new(Project::new(repo.clone()).unwrap()),
        factory.clone(),
    )
    .with_sequential_run_ids()
    .with_sandbox_root(repo);
    runtime.set_thread_scratch(Arc::clone(&scratch));
    for _ in 0..2 {
        let root = runtime.reserve_run_id();
        runtime.bind_thread_root("thread", root).unwrap();
        runtime.spawn_reserved(root, None, Role::Worker, "WORK", RunConfig::default());
        assert_eq!(runtime.wait(root).await, Ok(AgentRunPhase::Done));
    }
    let unbound = runtime.delegate_background(Role::Worker, "WORK".into(), RunConfig::default());
    assert_eq!(runtime.wait(unbound).await, Ok(AgentRunPhase::Done));
    let dirs = factory
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|mounts| mounts.scratch.clone().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(dirs[0], scratch.thread_dir("thread"));
    assert_eq!(dirs[1], dirs[0]);
    assert_eq!(dirs[2], scratch.parent().join(format!("r-{unbound}")));
}
