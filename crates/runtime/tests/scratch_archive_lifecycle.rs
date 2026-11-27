//! Archived-thread scratch reclamation against the real run lifecycle (ADR 0033).
//!
//! The archive worker renames a thread's scratch only once no run, child run or
//! shell job uses it, and every way back into a run registers it and prepares
//! the same path before anything touches it. Every wait is an explicit signal:
//! an archive pass (`scratch_archive_barrier`), the saved-sidebar checker being
//! consulted (a pass reaches it only after every user left), a FIFO a running
//! command writes, a phase event or a model request.
#![cfg(feature = "idle-test-support")]
mod support;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use event_bus::{AgentMessageKind, AgentRunPhase, EventBus, EventKind, EventReceiver, ToolEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::ownership::OwnerHost;
use runtime::{
    AgentRuntime, ExecutionPolicy, Role, RunConfig, RunId, RunStore, SandboxFactory, StopScope,
    WorkspaceMounts,
};
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::json;
use support::{ScriptedModel, init_git_repo, text_response, tool_response};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tools::ToolExecutor;

const THREAD: &str = "thread";
const NOTICE: &str = "was removed after this thread was archived";
const KEEP: &str = "printf saved > \"$TMPDIR/keep\"";
/// Succeeds only in a scratch the resume recreated empty.
const FRESH: &str = "test ! -e \"$TMPDIR/keep\" && printf new > \"$TMPDIR/new\"";

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

/// A command the test holds open: it announces itself on `ready`, then blocks
/// until the test writes `go`.
struct Gate {
    ready: PathBuf,
    go: PathBuf,
}

impl Gate {
    fn new(dir: &Path, name: &str) -> Self {
        let gate = Self {
            ready: dir.join(format!("{name}.ready")),
            go: dir.join(format!("{name}.go")),
        };
        for fifo in [&gate.ready, &gate.go] {
            let status = std::process::Command::new("mkfifo")
                .arg(fifo)
                .status()
                .unwrap();
            assert!(status.success());
        }
        gate
    }

    fn call(&self, id: &str, before: &str) -> ChatResponse {
        tool_response(
            id,
            "shell",
            json!({
                "command": format!(
                    "{before}; echo up > '{}'; cat '{}' > /dev/null",
                    self.ready.display(),
                    self.go.display()
                ),
                "yield_ms": 1_800_000,
            }),
        )
    }

    async fn running(&self) {
        let ready = self.ready.clone();
        tokio::task::spawn_blocking(move || fs::read(ready).unwrap())
            .await
            .unwrap();
    }

    async fn release(&self) {
        let go = self.go.clone();
        tokio::task::spawn_blocking(move || fs::write(go, "go").unwrap())
            .await
            .unwrap();
    }
}

struct World {
    runtime: AgentRuntime,
    _host: OwnerHost,
    archive: runtime::scratch::archive::ArchiveService,
    model: Arc<ScriptedModel>,
    bus: Arc<EventBus>,
    events: EventReceiver,
    /// One message per consultation of the saved sidebar.
    consulted: UnboundedReceiver<()>,
    dir: PathBuf,
    _storage: storage::Storage,
    _database: tempfile::TempDir,
    _sockets: test_socket_dir::SocketDir,
    _repo: tempfile::TempDir,
}

impl World {
    /// `scripts` maps a prompt prefix to the model replies of the run it starts.
    async fn new(scripts: Vec<(&str, Vec<ChatResponse>)>) -> Self {
        let (repo_temp, repo) = init_git_repo();
        let sockets = test_socket_dir::SocketDir::new();
        let bus = Arc::new(EventBus::new(4096));
        let events = bus.subscribe();
        let host = OwnerHost::open(sockets.path(), Default::default(), bus.clone()).unwrap();
        host.start(THREAD).unwrap();
        let scratch = host.thread_scratch();
        let (consulted_tx, consulted) = unbounded_channel();
        let archive = runtime::scratch::archive::ArchiveService::new(
            &scratch,
            host.scratch_archive_authority(),
        )
        .unwrap();
        archive.set_checker(Arc::new(move |_| {
            let _ = consulted_tx.send(());
            Ok(true)
        }));
        let model = Arc::new(ScriptedModel::new([]));
        for (marker, script) in scripts {
            model.add_keyed(marker, script.into_iter().map(Ok)).await;
        }
        let database = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: database.path().join("runs.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime = AgentRuntime::with_sandbox_factory(
            bus.clone(),
            Arc::new(ToolExecutor::with_standard_tools(
                bus.clone(),
                Arc::new(DirectSandbox::new_unchecked()),
            )),
            model.clone(),
            Arc::new(ScratchFactory),
        )
        .with_sequential_run_ids()
        .with_sandbox_root(repo)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        runtime.set_thread_scratch(scratch.clone());
        Self {
            runtime,
            _host: host,
            archive,
            model,
            bus,
            events,
            consulted,
            dir: scratch.thread_dir(THREAD),
            _storage: storage,
            _database: database,
            _sockets: sockets,
            _repo: repo_temp,
        }
    }

    fn config() -> RunConfig {
        RunConfig {
            interactive: true,
            ..Default::default()
        }
    }

    fn root(&self, prompt: &str) -> RunId {
        let run = self.runtime.reserve_run_id();
        self.runtime.bind_thread_root(THREAD, run).unwrap();
        self.runtime
            .spawn_reserved(run, None, Role::Worker, prompt, Self::config())
            .unwrap();
        run
    }

    fn child(&self, parent: RunId, prompt: &str) -> RunId {
        let run = self.runtime.reserve_child_run_id(parent).unwrap();
        self.runtime
            .spawn_reserved_child(parent, run, Role::Worker, prompt, Self::config())
            .unwrap();
        run
    }

    async fn waiting(&self, run: RunId) {
        let mut events = self.bus.subscribe();
        while self.runtime.inspect_agent(run).unwrap().phase != AgentRunPhase::Waiting {
            events.recv().await.unwrap();
        }
    }

    /// Returns after one full pass of the archive worker.
    async fn pass(&self) {
        let barrier = self.archive.barrier();
        tokio::task::spawn_blocking(move || barrier.recv().unwrap())
            .await
            .unwrap();
    }

    /// The pass met a user: it read no sidebar and left `file` in place.
    async fn assert_held(&mut self, file: &str, why: &str) {
        self.archive.hint(THREAD);
        self.pass().await;
        assert!(self.consulted.try_recv().is_err(), "{why}: sidebar read");
        assert!(self.dir.join(file).exists(), "{why}: scratch renamed");
    }

    /// A hint outlives busy passes, so the sidebar is consulted once the last
    /// user left. The pass that consulted it also renamed the directory.
    async fn assert_reclaimed(&mut self) {
        self.archive.hint(THREAD);
        self.consulted.recv().await.unwrap();
        self.pass().await;
        assert!(!self.dir.exists(), "scratch was not reclaimed");
        // Passes started by the same hint have all finished; none consults later.
        while self.consulted.try_recv().is_ok() {}
    }

    async fn question(&mut self) -> String {
        loop {
            if let EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) =
                self.events.recv().await.unwrap().kind
                && question.answer.is_none()
            {
                return question.id;
            }
        }
    }

    async fn requests(&self, prompt: &str) -> Vec<Vec<Message>> {
        self.model
            .observed()
            .await
            .into_iter()
            .filter(|request| {
                user_texts(request)
                    .first()
                    .is_some_and(|t| t.starts_with(prompt))
            })
            .collect()
    }
}

fn user_texts(request: &[Message]) -> Vec<String> {
    request
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn notice_count(request: &[Message]) -> usize {
    user_texts(request)
        .iter()
        .filter(|text| text.contains(NOTICE))
        .count()
}

fn keep() -> ChatResponse {
    tool_response("keep", "shell", json!({"command": KEEP}))
}

fn done(text: &str) -> ChatResponse {
    text_response(text, FinishReason::Stop)
}

// Given: a run whose command is still running
// When: its thread is archived before and after the command ends
// Then: nothing is renamed while the command runs; once the idle run left, the scratch goes
#[tokio::test]
async fn a_run_waiting_on_a_tool_keeps_the_scratch_until_its_turn_ends() {
    let gates = tempfile::tempdir().unwrap();
    let gate = Gate::new(gates.path(), "tool");
    let mut world = World::new(vec![("ROOT", vec![gate.call("block", KEEP), done("idle")])]).await;
    world.root("ROOT");
    gate.running().await;
    world.assert_held("keep", "tool in flight").await;
    gate.release().await;
    world.assert_reclaimed().await;
}

// Given: a run that asked a question and went back to waiting for input
// When: its thread is archived while the question is open, then the question is answered
// Then: the open question keeps the scratch; the turn that follows the answer releases it
#[tokio::test]
async fn an_unanswered_question_keeps_the_scratch_until_the_answered_turn_ends() {
    let mut world = World::new(vec![(
        "ROOT",
        vec![
            keep(),
            tool_response(
                "ask",
                "ask_user",
                json!({"title": "Which?", "blocking": false}),
            ),
            done("asked"),
            done("answered"),
        ],
    )])
    .await;
    let run = world.root("ROOT");
    let question = world.question().await;
    world.waiting(run).await;
    world.assert_held("keep", "question open").await;
    world
        .runtime
        .answer_user_question(&question, "ANSWER")
        .unwrap();
    world.assert_reclaimed().await;
    assert_eq!(world.requests("ROOT").await.len(), 4);
}

// Given: a root run and a child run, each running a command
// When: they finish in either order
// Then: the scratch stays until the second of them left, and only then is renamed
#[tokio::test]
async fn run_and_child_run_release_independently_in_either_order() {
    for root_first in [true, false] {
        let gates = tempfile::tempdir().unwrap();
        let root_gate = Gate::new(gates.path(), "root");
        let child_gate = Gate::new(gates.path(), "child");
        let mut world = World::new(vec![
            (
                "ROOT",
                vec![
                    keep(),
                    root_gate.call("root-block", "true"),
                    done("root idle"),
                ],
            ),
            (
                "CHILD",
                vec![child_gate.call("child-block", "true"), done("child idle")],
            ),
        ])
        .await;
        let root = world.root("ROOT");
        root_gate.running().await;
        let child = world.child(root, "CHILD");
        child_gate.running().await;
        world.assert_held("keep", "both running").await;
        let (first, first_run, second) = if root_first {
            (&root_gate, root, &child_gate)
        } else {
            (&child_gate, child, &root_gate)
        };
        first.release().await;
        world.waiting(first_run).await;
        world.assert_held("keep", "one run left").await;
        second.release().await;
        world.assert_reclaimed().await;
    }
}

// Given: a run whose command is running
// When: the run is stopped and its shell job torn down
// Then: the scratch stays until that finalization, and is then reclaimed
#[tokio::test]
async fn a_stopped_run_releases_its_shell_job_with_the_run() {
    let gates = tempfile::tempdir().unwrap();
    let gate = Gate::new(gates.path(), "stopped");
    let mut world = World::new(vec![("ROOT", vec![gate.call("block", KEEP)])]).await;
    let run = world.root("ROOT");
    gate.running().await;
    world.assert_held("keep", "shell command running").await;
    world.runtime.stop(run, StopScope::SelfOnly).unwrap();
    world.runtime.wait(run).await.unwrap();
    world.assert_reclaimed().await;
}

/// Where the reclaimed run is picked up again.
#[derive(Clone, Copy, Debug)]
enum Resume {
    UserInput,
    ChildMessage,
    RestoredChat,
    UserAnswer,
}

// Given: an idle run whose archived thread's scratch was reclaimed
// When: each way back into the run is taken and its next command is still running
// Then: the same path exists again, empty, and an archive pass keeps it; the model is told once,
//       after the new input
#[tokio::test]
async fn every_resume_path_registers_and_prepares_the_reclaimed_path_before_use() {
    for resume in [
        Resume::UserInput,
        Resume::ChildMessage,
        Resume::RestoredChat,
        Resume::UserAnswer,
    ] {
        let gates = tempfile::tempdir().unwrap();
        let gate = Gate::new(gates.path(), "resumed");
        let mut world = World::new(vec![
            (
                "ROOT",
                vec![
                    keep(),
                    done("first"),
                    gate.call("fresh", FRESH),
                    done("second"),
                ],
            ),
            ("CHILD", vec![done("child idle")]),
        ])
        .await;
        let root = world.root("ROOT");
        world.waiting(root).await;
        let child = world.child(root, "CHILD");
        world.waiting(child).await;
        world.assert_reclaimed().await;

        let input = match resume {
            Resume::UserInput => {
                world.runtime.send_message(root, "NEXT".into()).unwrap();
                "NEXT"
            }
            Resume::ChildMessage => {
                world
                    .runtime
                    .send_agent_message(child, root, AgentMessageKind::Send, "CHILD-REPORT", None)
                    .unwrap();
                "CHILD-REPORT"
            }
            Resume::UserAnswer => {
                let question = world
                    .runtime
                    .request_user_question(root, "Which next step?".into(), vec![], false)
                    .unwrap();
                world
                    .runtime
                    .answer_user_question(&question.id, "ANSWER")
                    .unwrap();
                "ANSWER"
            }
            Resume::RestoredChat => {
                world.runtime.stop(root, StopScope::SelfOnly).unwrap();
                world.runtime.wait(root).await.unwrap();
                world.runtime.resume_chat(root, World::config()).unwrap();
                world.waiting(root).await;
                world.runtime.send_message(root, "NEXT".into()).unwrap();
                "NEXT"
            }
        };
        gate.running().await;
        assert_eq!(
            fs::read(world.dir.join("new")).unwrap(),
            b"new",
            "{resume:?}: recreated empty"
        );
        assert!(!world.dir.join("keep").exists(), "{resume:?}");
        world
            .assert_held("new", &format!("{resume:?}: resumed run"))
            .await;
        gate.release().await;
        world.assert_reclaimed().await;

        let requests = world.requests("ROOT").await;
        let resumed = &requests[2];
        let texts = user_texts(resumed);
        assert_eq!(notice_count(resumed), 1, "{resume:?}: {texts:?}");
        assert!(
            texts.last().unwrap().contains(NOTICE),
            "{resume:?}: the notice ends the request: {texts:?}"
        );
        assert!(
            texts[texts.len() - 2].contains(input),
            "{resume:?}: the notice follows the new input: {texts:?}"
        );
        assert_eq!(
            notice_count(requests.last().unwrap()),
            1,
            "{resume:?}: told once"
        );
    }
}
