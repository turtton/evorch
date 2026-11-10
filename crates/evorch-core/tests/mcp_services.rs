//! Production core bootstrap, real stdio processes and explicit control events.
#[path = "support/mcp_traces.rs"]
mod traces;
use event_bus::{AgentRunPhase, Event, EventKind, EventReceiver, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, Role, RunId, RuntimeError};
use sandbox::{CredentialStore, Secret};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tools::mcp::{
    approval::{ApprovalLedger, inspect_stdio},
    secrets::credential_key,
    types::{ApprovalScope, ConfirmMcpApproval, McpApprovalStore},
};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    commands::{Command, ComposerRole, SendChat},
    envelope::{Provenance, Surface},
    ids::{ClientId, ProjectId, SubmissionId, ThreadId},
};

const SECRET: &str = "core-mcp-credential-must-never-be-observable";
const TOOL: &str = "mcp__fixture__echo";

#[derive(Default)]
struct Model {
    steps: Mutex<VecDeque<ContentBlock>>,
    inputs: Mutex<Vec<Value>>,
}
impl Model {
    fn call(&self, id: &str) {
        self.steps.lock().unwrap().extend([
            ContentBlock::ToolUse {
                id: id.into(),
                name: TOOL.into(),
                input: json!({}),
            },
            ContentBlock::Text {
                text: "Evidence collected".into(),
            },
        ]);
    }
}
#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.inputs
            .lock()
            .unwrap()
            .push(json!({"messages":messages,"tools":tools}));
        let block = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ContentBlock::Text {
                text: "Ready".into(),
            });
        let tool = matches!(block, ContentBlock::ToolUse { .. });
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![block],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: if tool {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "core-mcp".into()
    }
}
pub struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
struct Core {
    boot: BootstrappedHost,
    model: Arc<Model>,
    events: EventReceiver,
    listener: UnixListener,
    dir: tempfile::TempDir,
}
impl Core {
    async fn open() -> Self {
        Self::open_with_backend(None).await
    }
    async fn open_with_backend(backend: Option<runtime::mcp::McpCredentialBackend>) -> Self {
        // Owner IPC and the test control socket both need short sockaddr_un paths.
        traces::capture();
        let dir = tempfile::Builder::new()
            .prefix("03c-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".evorch")).unwrap();
        std::fs::write(
            root.join("sidebar.json"),
            json!({
                "projects":[{"id":"p","repo_root":root}]
            })
            .to_string(),
        )
        .unwrap();
        let listener = UnixListener::bind(root.join("control")).unwrap();
        let command = PathBuf::from(std::env::var("CARGO_BIN_EXE_core-mcp-test-peer").unwrap())
            .canonicalize()
            .unwrap();
        std::fs::write(root.join(".evorch/mcp.toml"), format!(
            "version=1\n[servers.fixture]\ntype=\"stdio\"\ncommand={command:?}\nargs=[{:?}]\nroles=[\"worker\"]\n[servers.fixture.env]\nTOKEN={{keychain=\"token\"}}\n", root.join("control")
        )).unwrap();
        let store = Arc::new(sandbox::FileCredentialStore::open(root.join("secrets")).unwrap());
        store
            .set(
                &credential_key(&scope(&root), "token"),
                &Secret::from(SECRET.to_owned()),
            )
            .unwrap();
        let mcp = runtime::mcp::McpHostServices::at_config_dir(
            backend.unwrap_or(runtime::mcp::McpCredentialBackend::Available(store)),
            root.clone(),
        );
        let config = config::mcp::load_project(&root).unwrap();
        let ledger = ApprovalLedger::at_user_config_dir(root.clone()).unwrap();
        let inspection = inspect_stdio(
            scope(&root),
            config.revision,
            config.servers["fixture"].definition.as_ref().unwrap(),
            &command,
            &root,
        )
        .unwrap();
        let preview = ledger.preview(&inspection).unwrap();
        ledger
            .approve(
                &ConfirmMcpApproval {
                    scope: preview.scope,
                    preview_id: preview.id,
                    expected_revision: preview.source_revision,
                    expected_fingerprint: preview.fingerprint,
                },
                &inspection,
            )
            .unwrap();
        let model = Arc::new(Model::default());
        let boot = BootstrappedHost::open_with_mcp_test_seams(
            options(&root),
            TestSeams {
                model: model.clone(),
                sandbox: Arc::new(Unisolated),
            },
            mcp,
        )
        .await
        .unwrap();
        let events = boot.runtime.event_bus().subscribe();
        Self {
            boot,
            model,
            events,
            listener,
            dir,
        }
    }
    fn send(&self, thread: &str, submission: &str) -> RunId {
        send(&self.boot, thread, submission).unwrap()
    }
    async fn peer(&mut self, run: RunId) -> (BufReader<UnixStream>, u64) {
        accept_peer(&self.listener, &mut self.events, run).await
    }
    async fn called(&mut self, peer: &mut BufReader<UnixStream>, run: RunId) {
        await_call(peer, &mut self.events, run).await;
    }
    async fn waiting(&mut self, run: RunId) -> Vec<Event> {
        through(&mut self.events, run, AgentRunPhase::Waiting).await
    }
}
async fn accept_peer(
    listener: &UnixListener,
    events: &mut EventReceiver,
    run: RunId,
) -> (BufReader<UnixStream>, u64) {
    let (stream, _) = loop {
        tokio::select! {
            accepted = listener.accept() => break accepted.unwrap(),
            event = events.recv() => check_progress(event.unwrap(), run),
        }
    };
    let mut peer = BufReader::new(stream);
    let event = control(&mut peer).await;
    assert_eq!(event["event"], "started");
    assert_eq!(event["credential_present"], true);
    (peer, event["pid"].as_u64().unwrap())
}
async fn await_call(peer: &mut BufReader<UnixStream>, events: &mut EventReceiver, run: RunId) {
    loop {
        tokio::select! {
            event = control(peer) => { assert_eq!(event["event"], "call"); return; },
            event = events.recv() => check_progress(event.unwrap(), run),
        }
    }
}

fn check_progress(event: Event, run: RunId) {
    let expected = run.to_string();
    match event.kind {
        EventKind::Diagnostic(diagnostic)
            if diagnostic.run_id.as_deref() == Some(expected.as_str())
                && diagnostic.severity != event_bus::DiagnosticSeverity::Info =>
        {
            panic!("MCP preparation/call failed: {diagnostic:?}")
        }
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
            if run_id == expected
                && matches!(
                    to,
                    AgentRunPhase::Waiting
                        | AgentRunPhase::Done
                        | AgentRunPhase::Error
                        | AgentRunPhase::Stopped
                ) =>
        {
            panic!("run ended before MCP control event: {to:?}")
        }
        _ => (),
    }
}
fn scope(root: &Path) -> ApprovalScope {
    ApprovalScope {
        canonical_root: root.into(),
        server_id: "fixture".into(),
    }
}
fn options(root: &Path) -> BootstrapOptions {
    BootstrapOptions {
        config_dir: root.into(),
        project_root: root.into(),
        ownership_root: root.join("owners"),
    }
}
fn send(
    boot: &BootstrappedHost,
    thread: &str,
    submission: &str,
) -> Result<RunId, workbench_protocol::errors::WorkbenchError> {
    let input = SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Worker,
        text: "Inspect MCP evidence".into(),
        skills: vec![],
        images: vec![],
        model_preference: None,
        fork_seed: None,
    };
    let mut host = boot.host.lock().unwrap();
    host.reserve_new_chat_thread(&input)?;
    let reply = host.handle_command(
        Command::SendChat(input),
        Provenance::human(Surface::Cli, ClientId::new("03c").unwrap()),
    )?;
    Ok(reply.run_id.unwrap().as_str().parse().unwrap())
}
async fn control(peer: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    assert_ne!(
        peer.read_line(&mut line).await.unwrap(),
        0,
        "peer closed before control event"
    );
    serde_json::from_str(&line).unwrap()
}
async fn through(events: &mut EventReceiver, run: RunId, expected: AgentRunPhase) -> Vec<Event> {
    let mut seen = vec![];
    loop {
        let event = events.recv().await.unwrap();
        let reached = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                if run_id == &run.to_string() =>
            {
                if matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                ) {
                    assert_eq!(*to, expected, "{event:?}");
                }
                *to == expected
            }
            _ => false,
        };
        seen.push(event);
        if reached {
            return seen;
        }
    }
}

#[tokio::test]
async fn core_mcp_execution_credentials_and_settings_during_call() {
    let mut core = Core::open().await;
    core.model.call("first");
    let run = core.send("thread", "first");
    let (mut peer, pid) = core.peer(run).await;
    core.called(&mut peer, run).await;
    let root = core.dir.path().canonicalize().unwrap();
    let old = core.boot.mcp.gui.settings(root.clone()).await.unwrap();
    core.boot
        .mcp
        .gui
        .save_override(
            root.clone(),
            old.config.revision,
            config::mcp::McpOverrideMutation::Disable {
                server_id: "fixture".into(),
            },
        )
        .await
        .unwrap();
    // Already-dispatched calls finish against their original generation, never replay.
    peer.get_mut().write_all(b"continue\n").await.unwrap();
    let mut events = core.waiting(run).await;
    assert!(
        core.model
            .inputs
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .to_string()
            .contains("mcp-ok")
    );
    core.model.call("after-disable");
    assert_eq!(core.send("thread", "second"), run);
    events.extend(core.waiting(run).await);
    let inputs = core.model.inputs.lock().unwrap().clone();
    assert!(
        inputs
            .iter()
            .all(|input| input["tools"] == inputs[0]["tools"])
    );
    assert!(!serde_json::to_string(&events).unwrap().contains(SECRET));
    assert!(!serde_json::to_string(&inputs).unwrap().contains(SECRET));
    core.boot
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    core.boot.runtime.wait(run).await.unwrap();
    let disabled = core.send("disabled", "disabled");
    core.waiting(disabled).await;
    assert!(
        !core.model.inputs.lock().unwrap().last().unwrap()["tools"]
            .to_string()
            .contains(TOOL)
    );
    let current = core.boot.mcp.gui.settings(root.clone()).await.unwrap();
    let removed = core
        .boot
        .mcp
        .gui
        .save_override(
            root.clone(),
            current.config.revision,
            config::mcp::McpOverrideMutation::RemoveOverride {
                server_id: "fixture".into(),
            },
        )
        .await
        .unwrap();
    assert!(!removed.config.servers.contains_key("fixture"));
    let restored = core
        .boot
        .mcp
        .gui
        .save_override(
            root.clone(),
            removed.config.revision,
            config::mcp::McpOverrideMutation::Upsert {
                server_id: "fixture".into(),
                definition: old.config.servers["fixture"].definition.clone().unwrap(),
            },
        )
        .await
        .unwrap();
    let preview = core
        .boot
        .mcp
        .gui
        .preview_approval(scope(&root), restored.config.revision)
        .await
        .unwrap();
    core.boot
        .mcp
        .gui
        .confirm_approval(ConfirmMcpApproval {
            scope: preview.scope,
            preview_id: preview.id,
            expected_revision: preview.source_revision,
            expected_fingerprint: preview.fingerprint,
        })
        .await
        .unwrap();
    core.model.call("re-added");
    let readded = core.send("re-added", "re-added");
    let (mut fresh_peer, fresh_pid) = core.peer(readded).await;
    core.called(&mut fresh_peer, readded).await;
    fresh_peer.get_mut().write_all(b"continue\n").await.unwrap();
    core.waiting(readded).await;
    core.boot.shutdown().await.unwrap();
    assert_reaped_on_linux(fresh_pid);
    let mut eof = String::new();
    assert_eq!(peer.read_line(&mut eof).await.unwrap(), 0);
    assert_reaped_on_linux(pid);
    let db = storage::Database::open(&storage::StorageConfig {
        db_path: root.join("evorch-events.db"),
        ..Default::default()
    })
    .unwrap();
    assert!(
        !serde_json::to_string(
            &db.events_all_ordered()
                .unwrap()
                .into_iter()
                .map(|stored| stored.event)
                .collect::<Vec<_>>()
        )
        .unwrap()
        .contains(SECRET)
    );
    traces::assert_redacted(SECRET);
}

#[tokio::test]
async fn reconnect_is_for_a_later_explicit_call_and_does_not_replay_unknown_outcome() {
    let mut core = Core::open().await;
    core.model.call("unknown");
    let run = core.send("thread", "first");
    let (mut peer, old_pid) = core.peer(run).await;
    core.called(&mut peer, run).await;
    peer.get_mut().write_all(b"crash\n").await.unwrap();
    core.waiting(run).await;
    core.model.call("explicit-next-call");
    core.send("thread", "second");
    let (mut replacement, pid) = core.peer(run).await;
    assert_ne!(old_pid, pid);
    core.called(&mut replacement, run).await;
    replacement
        .get_mut()
        .write_all(b"continue\n")
        .await
        .unwrap();
    core.waiting(run).await;
    assert!(
        core.model
            .inputs
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .to_string()
            .contains("mcp-ok")
    );
    core.boot.shutdown().await.unwrap();
    assert_reaped_on_linux(pid);
}

async fn legacy_host(core: &Core, legacy_model: Arc<Model>) -> BootstrappedHost {
    BootstrappedHost::open_with_mcp_test_seams(
        options(core.dir.path()),
        TestSeams {
            model: legacy_model,
            sandbox: Arc::new(Unisolated),
        },
        runtime::mcp::McpHostServices::at_config_dir(
            runtime::mcp::McpCredentialBackend::Available(Arc::new(
                sandbox::FileCredentialStore::open(core.dir.path().join("secrets")).unwrap(),
            )),
            core.dir.path().to_path_buf(),
        ),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn parallel_legacy_host_cannot_connect_or_execute_the_core_owned_thread() {
    let mut core = Core::open().await;
    core.model.call("owned");
    let run = core.send("thread", "first");
    let (mut peer, _) = core.peer(run).await;
    core.called(&mut peer, run).await;
    // The legacy GUI uses the same CommandService/OwnerHost boundary. Its MCP
    // service is inert until admission; a shared DB does not confer ownership.
    let legacy_model = Arc::new(Model::default());
    legacy_model.call("must-not-run");
    let mut legacy = legacy_host(&core, legacy_model.clone()).await;
    assert!(send(&legacy, "thread", "legacy").is_err());
    assert!(legacy.runtime.list_agents().is_empty());
    assert!(legacy_model.inputs.lock().unwrap().is_empty());
    peer.get_mut().write_all(b"continue\n").await.unwrap();
    core.waiting(run).await;
    legacy.shutdown().await.unwrap();
    core.boot.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_handoff_fences_old_calls_and_drains_the_previous_host() {
    let mut core = Core::open().await;
    core.model.call("before-handoff");
    let run = core.send("thread", "first");
    let (mut peer, old_pid) = core.peer(run).await;
    core.called(&mut peer, run).await;
    peer.get_mut().write_all(b"continue\n").await.unwrap();
    core.waiting(run).await;
    let old = core.boot.owner.owned_permit("thread").unwrap();
    let inputs = core.model.inputs.lock().unwrap().len();
    let model = Arc::new(Model::default());
    let mut legacy = legacy_host(&core, model.clone()).await;
    let _next = core.boot.owner.handoff(&old, &legacy.owner).unwrap();
    // No async yield since handoff: the previous socket is still alive, but
    // the exact mutation fence used before tool calls already rejects it.
    #[cfg(target_os = "linux")]
    assert!(Path::new(&format!("/proc/{old_pid}")).exists());
    assert!(old.validate_mutation().is_err());
    core.model.call("must-not-execute-after-handoff");
    assert!(
        core.boot
            .runtime
            .send_message(run, "try old MCP tool".into())
            .is_err()
    );
    let mut line = String::new();
    assert_eq!(
        peer.read_line(&mut line).await.unwrap(),
        0,
        "no stale tools/call before EOF"
    );
    // EOF precedes waitpid: use the host's completed drain signal, not a race
    // against OS process reaping or a timed poll of /proc.
    loop {
        let event = core.events.recv().await.unwrap();
        if matches!(event.kind, EventKind::Diagnostic(diagnostic)
            if diagnostic.code == "mcp.owner_drained" && diagnostic.thread_id.as_deref() == Some("thread"))
        {
            break;
        }
    }
    assert_reaped_on_linux(old_pid);
    assert_eq!(core.model.inputs.lock().unwrap().len(), inputs);
    let mut events = legacy.runtime.event_bus().subscribe();
    model.call("new-owner");
    let successor = send(&legacy, "thread", "successor").unwrap();
    let (mut next_peer, pid) = accept_peer(&core.listener, &mut events, successor).await;
    await_call(&mut next_peer, &mut events, successor).await;
    next_peer.get_mut().write_all(b"continue\n").await.unwrap();
    through(&mut events, successor, AgentRunPhase::Waiting).await;
    legacy.shutdown().await.unwrap();
    core.boot.shutdown().await.unwrap();
    assert_reaped_on_linux(pid);
}

fn assert_reaped_on_linux(pid: u64) {
    #[cfg(target_os = "linux")]
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "child must have been reaped"
    );
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

#[cfg(target_os = "linux")]
fn fd_targets() -> Vec<PathBuf> {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|fd| match std::fs::read_link(fd.unwrap().path()) {
            Ok(target) => Some(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("could not inspect process FDs: {error}"),
        })
        .collect()
}

#[tokio::test]
async fn core_shutdown_drains_inflight_mcp_and_reaps_child_and_fds() {
    let mut core = Core::open().await;
    #[cfg(target_os = "linux")]
    let before = fd_targets();
    core.model.call("blocked");
    let run = core.send("thread", "first");
    let (mut peer, pid) = core.peer(run).await;
    core.called(&mut peer, run).await;
    #[cfg(target_os = "linux")]
    let stdio = [0, 1, 2].map(|fd| std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).unwrap());
    core.boot.shutdown().await.unwrap();
    assert!(matches!(
        core.boot
            .runtime
            .list_agents()
            .into_iter()
            .find(|agent| agent.run_id == run)
            .unwrap()
            .phase,
        AgentRunPhase::Stopped | AgentRunPhase::Done
    ));
    let mut eof = String::new();
    assert_eq!(
        peer.read_line(&mut eof).await.unwrap(),
        0,
        "all child-owned control FDs closed"
    );
    assert_reaped_on_linux(pid);
    #[cfg(target_os = "linux")]
    {
        let after = fd_targets();
        for target in stdio {
            // stderr is /dev/null, shared with unrelated parent descriptors.
            // Unique pipes must disappear; shared targets return to baseline.
            let initial = before.iter().filter(|fd| **fd == target).count();
            let remaining = after.iter().filter(|fd| **fd == target).count();
            assert!(remaining <= initial, "MCP stdio FD retained: {target:?}");
        }
    }
    assert_eq!(
        core.boot.owner.attach("thread").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
    core.boot.mcp.shutdown().await.unwrap();
    assert!(matches!(
        core.boot
            .mcp
            .runs
            .prepare_new_run(runtime::mcp::PrepareMcpRun {
                canonical_root: core.dir.path().to_path_buf(),
                run_id: "after-stop".into(),
                role: Role::Worker,
                cancel: Default::default(),
            })
            .await,
        Err(runtime::mcp::McpErrorKind::Cancelled)
    ));
}

#[tokio::test]
async fn unavailable_core_keychain_is_classified_without_file_fallback_or_connection() {
    use runtime::mcp::{McpCredentialBackend, McpErrorKind, SecretUnavailableReason};
    for (reason, kind) in [
        (SecretUnavailableReason::Locked, McpErrorKind::SecretLocked),
        (SecretUnavailableReason::Denied, McpErrorKind::SecretDenied),
        (
            SecretUnavailableReason::Unavailable,
            McpErrorKind::SecretUnavailable,
        ),
    ] {
        let mut core =
            Core::open_with_backend(Some(McpCredentialBackend::Unavailable(reason))).await;
        let run = core.send("thread", "first");
        let events = core.waiting(run).await;
        let view = core
            .boot
            .mcp
            .gui
            .settings(core.dir.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(view.statuses[0].reason, Some(kind));
        assert!(events.iter().any(|event| matches!(&event.kind,
            EventKind::Diagnostic(diagnostic) if diagnostic.code == format!("mcp.{kind}"))));
        assert!(
            !core.model.inputs.lock().unwrap()[0]["tools"]
                .to_string()
                .contains(TOOL)
        );
        assert!(
            matches!(core.listener.into_std().unwrap().accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        core.boot.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn credential_bearing_server_error_is_not_exposed_in_core_events_or_history() {
    let mut core = Core::open().await;
    core.model.call("secret-error");
    let run = core.send("thread", "first");
    let (mut peer, _) = core.peer(run).await;
    core.called(&mut peer, run).await;
    peer.get_mut().write_all(b"secret-error\n").await.unwrap();
    let events = core.waiting(run).await;
    assert!(!serde_json::to_string(&events).unwrap().contains(SECRET));
    assert!(
        !serde_json::to_string(&*core.model.inputs.lock().unwrap())
            .unwrap()
            .contains(SECRET)
    );
    core.boot.shutdown().await.unwrap();
    traces::assert_redacted(SECRET);
}

#[tokio::test]
async fn server_error_content_is_not_copied_into_host_diagnostics_or_logs() {
    let mut core = Core::open().await;
    core.model.call("content-error");
    let run = core.send("thread", "first");
    let (mut peer, _) = core.peer(run).await;
    core.called(&mut peer, run).await;
    peer.get_mut().write_all(b"content-error\n").await.unwrap();
    let events = core.waiting(run).await;
    // Server-returned content remains model input, including isError=true.
    // Host-generated diagnostics and logs must not copy that content.
    assert!(
        serde_json::to_string(&*core.model.inputs.lock().unwrap())
            .unwrap()
            .contains(SECRET)
    );
    for event in events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::Diagnostic(_)))
    {
        assert!(!serde_json::to_string(event).unwrap().contains(SECRET));
    }
    core.boot.shutdown().await.unwrap();
    traces::assert_redacted(SECRET);
}
