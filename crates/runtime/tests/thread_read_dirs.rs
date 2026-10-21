//! A thread's read-only directories reach its runs, their delegated children and
//! the thread an escalation hands off to, through the shared boundary and always
//! read-only (ADR 0037).
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use agents::Role;
use config::EscalationApproval;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::scratch::ThreadScratch;
use runtime::{AgentRuntime, RunConfig, RunId, SandboxFactory, WorkspaceMounts};
use sandbox::{BwrapConfig, BwrapSandbox, DirectSandbox};
use serde_json::json;
use support::{ScriptedModel, init_git_repo, recording_factory, text_response, tool_response};
use tools::ToolExecutor;

const SEED: &str = "draft-seed-content";

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

fn system_text(request: &[Message]) -> String {
    request
        .iter()
        .filter(|message| message.role == providers::Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Dirs {
    _temps: Vec<tempfile::TempDir>,
    repo: PathBuf,
    drafts: PathBuf,
    seed: PathBuf,
    scratch: Arc<ThreadScratch>,
}

impl Dirs {
    fn new() -> Self {
        let (repo_temp, repo) = init_git_repo();
        let drafts_temp = tempfile::tempdir().unwrap();
        let drafts = drafts_temp.path().canonicalize().unwrap().join("drafts");
        std::fs::create_dir(&drafts).unwrap();
        let seed = drafts.join("seed.txt");
        std::fs::write(&seed, SEED).unwrap();
        let scratch_temp = tempfile::tempdir().unwrap();
        let scratch = Arc::new(ThreadScratch::at(scratch_temp.path().join("scratch")));
        Self {
            _temps: vec![repo_temp, drafts_temp, scratch_temp],
            repo,
            drafts,
            seed,
            scratch,
        }
    }

    fn runtime(
        &self,
        model: Arc<ScriptedModel>,
        factory: Option<Arc<dyn SandboxFactory>>,
    ) -> (AgentRuntime, Arc<EventBus>) {
        let bus = Arc::new(EventBus::new(512));
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        ));
        let runtime = match factory {
            Some(factory) => {
                AgentRuntime::with_sandbox_factory(Arc::clone(&bus), executor, model, factory)
            }
            None => AgentRuntime::new(Arc::clone(&bus), executor, model),
        }
        .with_sequential_run_ids()
        .with_sandbox_root(self.repo.clone());
        runtime.set_thread_scratch(Arc::clone(&self.scratch));
        runtime.set_sandbox_escalation(EscalationApproval::Off, false);
        (runtime, bus)
    }
}

fn start_in(runtime: &AgentRuntime, thread: &str, role: Role, prompt: &str) -> RunId {
    let run = runtime.reserve_run_id();
    runtime.bind_thread_root(thread, run).unwrap();
    runtime.spawn_reserved(run, None, role, prompt, RunConfig::default());
    run
}

fn start_in_project(
    runtime: &AgentRuntime,
    thread: &str,
    role: Role,
    prompt: &str,
    project_root: &Path,
) -> RunId {
    let run = runtime.reserve_run_id();
    runtime.bind_thread_root(thread, run).unwrap();
    runtime.spawn_reserved(
        run,
        None,
        role,
        prompt,
        RunConfig {
            project_root: Some(project_root.to_owned()),
            ..RunConfig::default()
        },
    );
    run
}

fn ro_binds(mounts: &Mutex<Vec<WorkspaceMounts>>) -> Vec<Vec<PathBuf>> {
    mounts
        .lock()
        .unwrap()
        .iter()
        .map(|mounts| mounts.ro_binds.clone())
        .collect()
}

// Given: thread-a declares a drafts directory (once through a link and once missing),
//        and thread-b in the same project declares nothing
// When: a Worker of each thread reads, greps, writes and edits there with file tools
// Then: thread-a mounts the directory read-only and lists it in its system message, so
//       reads work and changes are refused, while thread-b's read is outside its boundary
#[tokio::test]
async fn a_threads_runs_read_its_directories_but_never_write_them_and_other_threads_cannot() {
    let dirs = Dirs::new();
    let link_temp = tempfile::tempdir().unwrap();
    let link = link_temp.path().join("drafts-link");
    std::os::unix::fs::symlink(&dirs.drafts, &link).unwrap();
    let created = dirs.drafts.join("new.txt");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("a-read", "read", json!({"path": dirs.seed}))),
        Ok(tool_response(
            "a-grep",
            "grep",
            json!({"pattern": "draft-seed", "path": dirs.drafts}),
        )),
        Ok(tool_response(
            "a-write",
            "write",
            json!({"path": created, "content": "x"}),
        )),
        Ok(tool_response(
            "a-edit",
            "edit",
            json!({"path": dirs.seed, "old_string": SEED, "new_string": "changed"}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
        Ok(tool_response("b-read", "read", json!({"path": dirs.seed}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (factory, mounts) = recording_factory();
    let (runtime, _) = dirs.runtime(model.clone(), Some(factory));
    runtime.set_thread_read_only_directories(
        "thread-a",
        vec![link, dirs.drafts.join("missing"), dirs.drafts.clone()],
    );

    for thread in ["thread-a", "thread-b"] {
        let run = start_in(&runtime, thread, Role::Worker, "WORK");
        assert_eq!(
            runtime.wait(run).await.unwrap(),
            AgentRunPhase::Done,
            "{thread}"
        );
    }

    assert_eq!(
        ro_binds(&mounts),
        vec![vec![dirs.drafts.clone()], Vec::new()]
    );
    let observed = model.observed().await;
    assert!(tool_output(&observed, "a-read").contains(SEED));
    assert!(tool_output(&observed, "a-grep").contains("seed.txt"));
    for call in ["a-write", "a-edit"] {
        let output = tool_output(&observed, call);
        assert!(output.contains("is read-only for file tools"), "{output}");
    }
    assert!(!created.exists());
    assert_eq!(std::fs::read_to_string(&dirs.seed).unwrap(), SEED);
    let denied = tool_output(&observed, "b-read");
    assert!(denied.contains("outside the sandbox boundary"), "{denied}");
    assert!(!denied.contains(SEED));
    let listed = format!("* {} (read-only)", dirs.drafts.display());
    assert!(system_text(&observed[0]).contains(&listed));
    assert!(!system_text(&observed[5]).contains(&dirs.drafts.display().to_string()));
}

// Given: thread-a declares a drafts directory
// When: its Orchestrator delegates to a Worker that reads and writes there
// Then: the child mounts and is told the same read-only directory, so it reads but cannot write
#[tokio::test]
async fn delegated_children_inherit_the_threads_directories_read_only() {
    let dirs = Dirs::new();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "delegate",
            "delegate",
            json!({"target": {"role": "worker"}, "prompt": "READ DRAFTS"}),
        )),
        Ok(tool_response(
            "child-read",
            "read",
            json!({"path": dirs.seed}),
        )),
        Ok(tool_response(
            "child-write",
            "write",
            json!({"path": dirs.seed, "content": "overwritten"}),
        )),
        Ok(text_response("child done", FinishReason::Stop)),
        Ok(tool_response("finish", "finish", json!({"result": "done"}))),
    ]));
    let (factory, mounts) = recording_factory();
    let (runtime, _) = dirs.runtime(model.clone(), Some(factory));
    runtime.set_thread_read_only_directories("thread-a", vec![dirs.drafts.clone()]);

    let root = start_in(&runtime, "thread-a", Role::Orchestrator, "COORDINATE");
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Done);

    assert_eq!(
        ro_binds(&mounts),
        vec![vec![dirs.drafts.clone()], vec![dirs.drafts.clone()]],
        "root, then its child"
    );
    let observed = model.observed().await;
    assert!(tool_output(&observed, "child-read").contains(SEED));
    let refused = tool_output(&observed, "child-write");
    assert!(refused.contains("is read-only for file tools"), "{refused}");
    assert_eq!(std::fs::read_to_string(&dirs.seed).unwrap(), SEED);
    let listed = format!("* {} (read-only)", dirs.drafts.display());
    assert!(system_text(&observed[1]).contains(&listed), "child system");
}

fn escalation_response() -> providers::ChatResponse {
    tool_response(
        "escalate",
        "escalate",
        json!({
            "original_request": "Triage the drafts",
            "escalation_reason": "needs coordination",
            "findings": ["read the drafts"],
            "files_touched": [],
            "blockers": ["a single run cannot split the work"],
            "workspace_state": "clean",
            "suggested_next": "split the triage"
        }),
    )
}

// Given: a thread-a Worker with a declared drafts directory
// When: it escalates into a new Orchestrator root on the child thread, and thread-a later
//       starts another run
// Then: the escalated run mounts and reads the copied directory, and thread-a keeps its own
#[tokio::test]
async fn an_escalation_carries_the_directories_to_its_child_thread() {
    let dirs = Dirs::new();
    let model = Arc::new(ScriptedModel::new([
        Ok(escalation_response()),
        Ok(tool_response(
            "escalated-read",
            "read",
            json!({"path": dirs.seed}),
        )),
        Ok(text_response("escalated done", FinishReason::Stop)),
        Ok(tool_response(
            "again-read",
            "read",
            json!({"path": dirs.seed}),
        )),
        Ok(text_response("done again", FinishReason::Stop)),
    ]));
    let (factory, mounts) = recording_factory();
    let (runtime, bus) = dirs.runtime(model.clone(), Some(factory));
    runtime.set_thread_read_only_directories("thread-a", vec![dirs.drafts.clone()]);
    let mut events = bus.subscribe();

    let source = start_in(&runtime, "thread-a", Role::Worker, "TRIAGE");
    let target = loop {
        let event = events.recv().await.expect("event bus remains open");
        if let EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
            source_run_id,
            new_run_id,
            ..
        }) = event.kind
            && source_run_id == source.to_string()
        {
            break new_run_id.parse::<RunId>().unwrap();
        }
    };
    assert_eq!(runtime.wait(target).await.unwrap(), AgentRunPhase::Done);
    let again = start_in(&runtime, "thread-a", Role::Worker, "AGAIN");
    assert_eq!(runtime.wait(again).await.unwrap(), AgentRunPhase::Done);

    assert_eq!(
        ro_binds(&mounts),
        vec![vec![dirs.drafts.clone()]; 3],
        "source, escalated root on the child thread, then thread-a again"
    );
    let observed = model.observed().await;
    assert!(tool_output(&observed, "escalated-read").contains(SEED));
    assert!(tool_output(&observed, "again-read").contains(SEED));
    let child = event_bus::escalation_thread_id(&target.to_string());
    assert_eq!(
        mounts.lock().unwrap()[1].scratch.as_deref(),
        Some(dirs.scratch.thread_dir(&child).as_path()),
        "the escalated run works in the child thread"
    );
}

// Given: thread-a declares a directory in one project
// When: another thread starts in a different workspace root and reads that path
// Then: neither its mounts, its Allowed directories section nor its file-tool boundary
//       includes thread-a's grant
#[tokio::test]
async fn thread_read_directories_do_not_leak_to_another_project() {
    let dirs = Dirs::new();
    let (_other_project_temp, other_project) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "read-in-other",
            "read",
            json!({"path": dirs.seed}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (factory, mounts) = recording_factory();
    let (runtime, _) = dirs.runtime(model.clone(), Some(factory));
    runtime.set_thread_read_only_directories("thread-a", vec![dirs.drafts.clone()]);

    let run = start_in_project(&runtime, "thread-b", Role::Worker, "WORK", &other_project);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);

    assert_eq!(ro_binds(&mounts), vec![Vec::<PathBuf>::new()]);
    let observed = model.observed().await;
    let denied = tool_output(&observed, "read-in-other");
    assert!(denied.contains("outside the sandbox boundary"), "{denied}");
    assert!(!denied.contains(SEED), "{denied}");
    let system = system_text(&observed[0]);
    assert!(
        !system.contains(&dirs.drafts.display().to_string()),
        "{system}"
    );
    assert!(
        !system.contains("Allowed directories (evorch)"),
        "unexpected Allowed directories section: {system}"
    );
}

// Given: a declared directory around the project root and one inside the scratch
// When: a run of the thread builds its sandbox
// Then: neither is mounted or listed, while an unrelated directory is
#[tokio::test]
async fn directories_around_or_inside_protected_paths_are_not_mounted() {
    let dirs = Dirs::new();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (factory, mounts) = recording_factory();
    let (runtime, _) = dirs.runtime(model.clone(), Some(factory));
    let thread_scratch = dirs.scratch.thread_dir("thread-a");
    let inside_scratch = thread_scratch.join("sub");
    std::fs::create_dir_all(&inside_scratch).unwrap();
    for private in [dirs.scratch.parent(), thread_scratch.as_path()] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let inside_scratch = inside_scratch.canonicalize().unwrap();
    let around_project = dirs.repo.parent().unwrap().canonicalize().unwrap();
    runtime.set_thread_read_only_directories(
        "thread-a",
        vec![
            around_project.clone(),
            inside_scratch.clone(),
            dirs.drafts.clone(),
        ],
    );

    let run = start_in(&runtime, "thread-a", Role::Worker, "WORK");
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);

    assert_eq!(ro_binds(&mounts), vec![vec![dirs.drafts.clone()]]);
    let system = system_text(&model.observed().await[0]);
    assert!(system.contains(&format!("* {} (read-only)", dirs.drafts.display())));
    for skipped in [&around_project, &inside_scratch] {
        assert!(
            !system.contains(&format!("* {} ", skipped.display())),
            "{system}"
        );
    }
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

async fn bwrap_scenario() {
    let dirs = Dirs::new();
    if let Err(error) = BwrapSandbox::detect(BwrapConfig::new(dirs.repo.clone())) {
        assert!(
            std::env::var_os("EVORCH_REQUIRE_BWRAP").is_none(),
            "EVORCH_REQUIRE_BWRAP: {error}"
        );
        eprintln!("SKIP: bwrap unavailable: {error}");
        return;
    }
    let written = dirs.drafts.join("from-shell.txt");
    let quoted = |path: &Path| format!("'{}'", path.display());
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "shell-cat",
            "shell",
            json!({"command": format!("cat {}", quoted(&dirs.seed))}),
        )),
        Ok(tool_response(
            "shell-write",
            "shell",
            json!({"command": format!("printf x > {}", quoted(&written))}),
        )),
        Ok(tool_response(
            "file-read",
            "read",
            json!({"path": dirs.seed}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
        Ok(tool_response(
            "other-cat",
            "shell",
            json!({"command": format!("cat {}", quoted(&dirs.seed))}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _) = dirs.runtime(model.clone(), None);
    runtime.set_thread_read_only_directories("thread-a", vec![dirs.drafts.clone()]);

    for thread in ["thread-a", "thread-b"] {
        let run = start_in(&runtime, thread, Role::Worker, "WORK");
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    }

    let observed = model.observed().await;
    let cat = tool_output(&observed, "shell-cat");
    assert!(
        cat.starts_with("exit_code: 0") && cat.contains(SEED),
        "{cat}"
    );
    let write = tool_output(&observed, "shell-write");
    assert!(!write.starts_with("exit_code: 0"), "{write}");
    assert!(write.contains("Read-only file system"), "{write}");
    assert!(!written.exists());
    assert!(tool_output(&observed, "file-read").contains(SEED));
    let other = tool_output(&observed, "other-cat");
    assert!(!other.contains(SEED), "{other}");
}

// Given: a Worker of a thread with a declared directory in a real bwrap sandbox, and a
//        Worker of another thread
// When: their shells read and write the directory
// Then: the declaring thread reads it but cannot write, and the other thread cannot see it
#[test]
#[ignore = "requires usable bwrap; re-exec with EVORCH_BWRAP_CHILD"]
fn thread_directories_are_read_only_mounts_in_bwrap() {
    if child("thread_directories_are_read_only_mounts_in_bwrap") {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(bwrap_scenario());
    }
}

// First read proves the run has started and grants D. Mutate it only after
// that read; later reads must not canonicalize the changed D into a new grant.
async fn replaced_directory_scenario(shell: bool, replacement: &str) {
    let dirs = Dirs::new();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().canonicalize().unwrap().join("secret.txt");
    std::fs::write(&secret, "replacement-secret").unwrap();
    let target = if replacement == "root" {
        Path::new("/")
    } else {
        secret.parent().unwrap()
    };
    let alias = if replacement == "root" {
        dirs.drafts.join(secret.strip_prefix("/").unwrap())
    } else {
        dirs.drafts.join("secret.txt")
    };
    let response = |id: &str, path: &Path| {
        if shell {
            tool_response(
                id,
                "shell",
                json!({"command": format!("cat '{}'", path.display())}),
            )
        } else {
            tool_response(id, "read", json!({"path": path}))
        }
    };
    let gate = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(response("before", &dirs.seed)),
            Ok(response("outside-after", &secret)),
            Ok(response("alias-after", &alias)),
            Ok(text_response("done", FinishReason::Stop)),
        ],
        gate.clone(),
    ));
    let factory = (!shell).then(|| recording_factory().0);
    let (runtime, _) = dirs.runtime(model.clone(), factory);
    runtime.set_thread_read_only_directories("thread-a", vec![dirs.drafts.clone()]);
    let run = start_in(&runtime, "thread-a", Role::Worker, "WORK");
    model.wait_for_request(0).await;
    gate.notify_one();
    model.wait_for_request(1).await;
    assert!(tool_output(&model.observed().await, "before").contains(SEED));
    // Keep the old inode alive, making the same-path recreation test deterministic.
    std::fs::rename(&dirs.drafts, dirs.drafts.with_extension("old")).unwrap();
    if replacement == "inode" {
        std::fs::create_dir(&dirs.drafts).unwrap();
        std::fs::write(&alias, "replacement-secret").unwrap();
    } else {
        std::os::unix::fs::symlink(target, &dirs.drafts).unwrap();
    }
    for index in 2..=3 {
        gate.notify_one();
        model.wait_for_request(index).await;
    }
    gate.notify_one();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let observed = model.observed().await;
    for id in ["outside-after", "alias-after"] {
        let result = tool_output(&observed, id);
        assert!(!result.contains("replacement-secret"), "{id}: {result}");
        if shell {
            assert!(!result.starts_with("exit_code: 0"), "{result}");
        } else {
            assert!(result.contains("outside the sandbox boundary"), "{result}");
        }
    }
}

#[tokio::test]
async fn a_running_threads_file_reads_reject_symlink_and_inode_replacements() {
    for replacement in ["root", "other", "inode"] {
        replaced_directory_scenario(false, replacement).await;
    }
}

#[test]
#[ignore = "requires usable bwrap; re-exec with EVORCH_BWRAP_CHILD"]
fn a_running_threads_shell_rejects_symlink_and_inode_replacements() {
    if child("a_running_threads_shell_rejects_symlink_and_inode_replacements") {
        let repo = tempfile::tempdir().unwrap();
        if let Err(error) = BwrapSandbox::detect(BwrapConfig::new(repo.path().to_owned())) {
            assert!(
                std::env::var_os("EVORCH_REQUIRE_BWRAP").is_none(),
                "EVORCH_REQUIRE_BWRAP: {error}"
            );
            eprintln!("SKIP: bwrap unavailable: {error}");
            return;
        }
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for replacement in ["root", "other", "inode"] {
                replaced_directory_scenario(true, replacement).await;
            }
        });
    }
}

#[tokio::test]
async fn redeclared_thread_directory_containing_the_current_database_is_not_mounted() {
    let dirs = Dirs::new();
    let config = storage::StorageConfig {
        db_path: dirs.drafts.join("candidate.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (factory, mounts) = recording_factory();
    let (runtime, _) = dirs.runtime(model.clone(), Some(factory));
    let runtime = runtime.with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
    // A saved declaration is re-sent after the DB moved under its directory.
    runtime.set_thread_read_only_directories("thread-a", vec![dirs.drafts.clone()]);
    let run = start_in(&runtime, "thread-a", Role::Worker, "WORK");
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert!(ro_binds(&mounts)[0].is_empty());
    assert!(
        !system_text(&model.observed().await[0])
            .contains(&format!("* {} (read-only)", dirs.drafts.display()))
    );
}
