//! Trusted allowed directories reach a project's runs through the shared
//! boundary, with the run's role deciding their access (ADR 0032).
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agents::Role;
use config::EscalationApproval;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::scratch::ThreadScratch;
use runtime::{AgentRuntime, RunConfig};
use sandbox::{BwrapConfig, BwrapSandbox, DirectSandbox, PathAccess};
use serde_json::json;
use support::{ScriptedModel, git, init_git_repo, recording_factory, text_response, tool_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

const SEED: &str = "allowed-seed-content";

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

struct Dirs {
    _temps: Vec<tempfile::TempDir>,
    repo: PathBuf,
    other: PathBuf,
    allowed: PathBuf,
    scratch: Arc<ThreadScratch>,
}

impl Dirs {
    fn new() -> Self {
        let (repo_temp, repo) = init_git_repo();
        let (other_temp, other) = init_git_repo();
        let (allowed_temp, allowed) = init_git_repo();
        std::fs::write(allowed.join("seed.txt"), SEED).unwrap();
        git(&allowed, &["add", "seed.txt"]);
        git(&allowed, &["commit", "-m", "seed"]);
        std::fs::write(allowed.join("seed.txt"), format!("{SEED}\nchanged")).unwrap();
        let scratch_temp = tempfile::tempdir().unwrap();
        let scratch = Arc::new(ThreadScratch::at(scratch_temp.path().join("scratch")));
        Self {
            _temps: vec![repo_temp, other_temp, allowed_temp, scratch_temp],
            repo,
            other,
            allowed,
            scratch,
        }
    }
}

async fn run_in(runtime: &AgentRuntime, thread: &str, role: Role, root: &Path) -> AgentRunPhase {
    let run = runtime.reserve_run_id();
    runtime.bind_thread_root(thread, run).unwrap();
    let config = RunConfig {
        project_root: Some(root.to_path_buf()),
        ..RunConfig::default()
    };
    runtime.spawn_reserved(run, None, role, "WORK", config);
    runtime.wait(run).await.unwrap()
}

// Given: a project with one trusted allowed directory and one that no longer exists,
//        declared through a link to the project root, and a second project with none
// When: a Worker and a Reviewer run in the first project and a Worker in the second
// Then: each run mounts only its project's list, with read-write access for the role
//       that changes files and read-only access otherwise, and file tools follow it
#[tokio::test]
async fn runs_use_their_project_list_with_the_access_of_their_role() {
    let dirs = Dirs::new();
    let seed = dirs.allowed.join("seed.txt");
    let link_temp = tempfile::tempdir().unwrap();
    let link = link_temp.path().join("project-link");
    std::os::unix::fs::symlink(&dirs.repo, &link).unwrap();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("worker-read", "read", json!({"path": seed}))),
        Ok(text_response("done", FinishReason::Stop)),
        Ok(tool_response(
            "reviewer-read",
            "read",
            json!({"path": seed}),
        )),
        Ok(tool_response(
            "reviewer-diff",
            "git_diff",
            json!({"cwd": dirs.allowed}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
        Ok(tool_response("other-read", "read", json!({"path": seed}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(512));
    let (factory, mounts) = recording_factory();
    let runtime = AgentRuntime::with_sandbox_factory(
        Arc::clone(&bus),
        Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        model.clone(),
        factory,
    )
    .with_sequential_run_ids()
    .with_sandbox_root(dirs.repo.clone());
    runtime.set_thread_scratch(Arc::clone(&dirs.scratch));
    runtime.set_sandbox_escalation(EscalationApproval::Off, false);
    runtime.set_project_allowed_directories(
        link.clone(),
        vec![dirs.allowed.clone(), dirs.repo.join("missing")],
    );

    for (thread, role, root) in [
        ("thread-a", Role::Worker, &dirs.repo),
        ("thread-b", Role::Reviewer, &dirs.repo),
        ("thread-c", Role::Worker, &dirs.other),
    ] {
        assert_eq!(
            run_in(&runtime, thread, role, root).await,
            AgentRunPhase::Done,
            "{thread}"
        );
    }

    assert_eq!(
        &*runtime.project_allowed_directories(&dirs.repo).get(),
        std::slice::from_ref(&dirs.allowed)
    );
    let mounts = mounts.lock().unwrap().clone();
    let declared = |index: usize| {
        let allowed = mounts[index].allowed.as_ref().expect("sandboxed run");
        (allowed.paths.get().to_vec(), allowed.access)
    };
    assert_eq!(
        declared(0),
        (vec![dirs.allowed.clone()], PathAccess::ReadWrite)
    );
    assert_eq!(
        declared(1),
        (vec![dirs.allowed.clone()], PathAccess::ReadOnly)
    );
    assert_eq!(declared(2), (Vec::new(), PathAccess::ReadWrite));
    let observed = model.observed().await;
    assert!(tool_output(&observed, "worker-read").contains(SEED));
    assert!(tool_output(&observed, "reviewer-read").contains(SEED));
    assert!(tool_output(&observed, "reviewer-diff").contains("+changed"));
    let other = tool_output(&observed, "other-read");
    assert!(other.contains("outside the sandbox boundary"), "{other}");
}

// Given a read-only Orchestrator and its project's allowed directory
// When it delegates implementation to a Worker
// Then one parent explanation distinguishes their access and the child really writes.
#[tokio::test]
async fn orchestrator_is_told_that_a_delegated_worker_can_write() {
    let dirs = Dirs::new();
    let written = dirs.allowed.join("delegated.txt");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "delegate",
            "delegate",
            json!({
                "target": {"role": "worker"}, "prompt": "WRITE SHARED FILE"
            }),
        )),
        Ok(tool_response(
            "write",
            "write",
            json!({"path": written, "content": "delegated implementation"}),
        )),
        Ok(text_response("child done", FinishReason::Stop)),
        Ok(tool_response("finish", "finish", json!({"result": "done"}))),
    ]));
    let bus = Arc::new(EventBus::new(512));
    let (factory, _) = recording_factory();
    let runtime = AgentRuntime::with_sandbox_factory(
        Arc::clone(&bus),
        Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        model.clone(),
        factory,
    )
    .with_sequential_run_ids()
    .with_sandbox_root(dirs.repo.clone());
    runtime.set_thread_scratch(Arc::clone(&dirs.scratch));
    runtime.set_project_allowed_directories(dirs.repo.clone(), vec![dirs.allowed.clone()]);
    assert_eq!(
        run_in(&runtime, "thread", Role::Orchestrator, &dirs.repo).await,
        AgentRunPhase::Done
    );
    let observed = model.observed().await;
    let system = |request: usize| {
        observed[request]
            .iter()
            .filter(|message| message.role == providers::Role::System)
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let parent = system(0);
    assert!(parent.contains(&format!("* {} (read-only)", dirs.allowed.display())));
    assert!(parent.contains("Delegated Workers have read-write access to all of them."));
    assert_eq!(
        parent.matches(&dirs.allowed.display().to_string()).count(),
        1,
        "the path is listed once, not repeated in the explanation"
    );
    assert_eq!(parent.matches("Delegated Workers").count(), 1);
    let child = system(1);
    assert!(child.contains(&format!("* {} (read-write)", dirs.allowed.display())));
    assert!(!child.contains("Delegated Workers"));
    assert_eq!(
        std::fs::read_to_string(written).unwrap(),
        "delegated implementation"
    );
}

fn child(name: &str) -> bool {
    if std::env::var_os("EVORCH_BWRAP_CHILD").is_some() {
        return true;
    }
    if Command::new("bwrap").arg("--version").output().is_err() {
        assert!(
            std::env::var_os("EVORCH_REQUIRE_BWRAP").is_none(),
            "EVORCH_REQUIRE_BWRAP: bwrap is not installed"
        );
        eprintln!("SKIP: bwrap is not installed");
        return false;
    }
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", name, "--nocapture", "--include-ignored"])
        .env("EVORCH_BWRAP_CHILD", "1")
        .output()
        .expect("child");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

async fn running_worker_scenario() {
    let dirs = Dirs::new();
    if let Err(error) = BwrapSandbox::detect(BwrapConfig::new(dirs.repo.clone())) {
        assert!(
            std::env::var_os("EVORCH_REQUIRE_BWRAP").is_none(),
            "EVORCH_REQUIRE_BWRAP: {error}"
        );
        eprintln!("SKIP: bwrap unavailable: {error}");
        return;
    }
    let seed = dirs.allowed.join("seed.txt");
    let written = dirs.allowed.join("from-worker.txt");
    let show = format!("cat '{}'", seed.display());
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(tool_response(
                "before-shell",
                "shell",
                json!({"command": show}),
            )),
            Ok(tool_response(
                "declared-shell",
                "shell",
                json!({"command": format!("{show} && printf from-worker > '{}'", written.display())}),
            )),
            Ok(tool_response(
                "declared-read",
                "read",
                json!({"path": seed}),
            )),
            Ok(tool_response(
                "removed-shell",
                "shell",
                json!({"command": show}),
            )),
            Ok(tool_response("removed-read", "read", json!({"path": seed}))),
            Ok(text_response("done", FinishReason::Stop)),
        ],
        Arc::clone(&gate),
    ));
    let bus = Arc::new(EventBus::new(512));
    let runtime = AgentRuntime::new(
        Arc::clone(&bus),
        Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        model.clone(),
    )
    .with_sequential_run_ids()
    .with_sandbox_root(dirs.repo.clone());
    runtime.set_thread_scratch(Arc::clone(&dirs.scratch));
    runtime.set_sandbox_escalation(EscalationApproval::Off, false);
    let run = runtime.reserve_run_id();
    runtime.bind_thread_root("thread-a", run).unwrap();
    runtime.spawn_reserved(
        run,
        None,
        Role::Worker,
        "WORK",
        RunConfig {
            project_root: Some(dirs.repo.clone()),
            ..RunConfig::default()
        },
    );

    // Each request carries the previous tool result, so the list changes between calls.
    // A gate holds one permit, so each response is released only once its request waits.
    let answer = async |request: usize| {
        model.wait_for_request(request).await;
        gate.notify_one();
    };
    answer(0).await;
    model.wait_for_request(1).await;
    runtime.set_project_allowed_directories(dirs.repo.clone(), vec![dirs.allowed.clone()]);
    for request in 1..3 {
        answer(request).await;
    }
    model.wait_for_request(3).await;
    runtime.set_project_allowed_directories(dirs.repo.clone(), Vec::new());
    for request in 3..6 {
        answer(request).await;
    }
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);

    let observed = model.observed().await;
    let before = tool_output(&observed, "before-shell");
    assert!(!before.contains(SEED), "{before}");
    let declared = tool_output(&observed, "declared-shell");
    assert!(
        declared.starts_with("exit_code: 0") && declared.contains(SEED),
        "{declared}"
    );
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "from-worker");
    assert!(tool_output(&observed, "declared-read").contains(SEED));
    let removed = tool_output(&observed, "removed-shell");
    assert!(!removed.contains(SEED), "{removed}");
    let denied = tool_output(&observed, "removed-read");
    assert!(denied.contains("outside the sandbox boundary"), "{denied}");
}

// Given: a running Worker in a bwrap sandbox / When: its project's allowed directory is
// declared and later withdrawn / Then: the next shell and file tool calls follow each change
#[test]
#[ignore = "requires usable bwrap; re-exec with EVORCH_BWRAP_CHILD"]
fn running_worker_follows_declared_allowed_directories_in_bwrap() {
    if child("running_worker_follows_declared_allowed_directories_in_bwrap") {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(running_worker_scenario());
    }
}
