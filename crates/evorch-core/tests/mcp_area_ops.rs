//! The `mcp` area on the production core composition (PR 11): operations are
//! accepted at once, finished through an explicit `operation_completed` event,
//! and can be read again after a disconnect. Real stdio MCP processes and the
//! real approval ledger sit behind it; a keychain wrapper injects failures.
//! Every wait observes an event, never a duration.
#[path = "support/mcp_traces.rs"]
mod traces;
use async_trait::async_trait;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use sandbox::{CredentialStore, FileCredentialStore, Secret, error::CredentialError};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::net::UnixListener;
use tools::mcp::secrets::credential_key;
use tools::mcp::types::ApprovalScope;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::subscriptions::Subscription;
use workbench_protocol::{
    commands::{Command, CommandDisposition, CommandResult},
    envelope::{Provenance, Surface},
    errors::{ErrorCode, WorkbenchError},
    events::{Subscribe, SubscriptionScope, UiEvent},
    ids::{ClientId, ProjectId, RequestId, SessionId},
    namespaces::{ProjectionQuery, mcp::*},
    queries::{Query, QueryReply},
};

const SECRET: &str = "mcp-area-sentinel-secret-must-never-be-observable";

/// A keychain that can be told to fail, so a half-finished save is observable.
struct Keychain {
    inner: FileCredentialStore,
    fail_set: AtomicBool,
    fail_delete: AtomicBool,
    /// Runs once right after a value was written (another writer's turn).
    after_set: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl CredentialStore for Keychain {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError> {
        if self.fail_set.load(Ordering::SeqCst) {
            return Err(CredentialError::KeychainLocked);
        }
        self.inner.set(key, value)?;
        if let Some(hook) = self.after_set.lock().unwrap().take() {
            hook();
        }
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(CredentialError::KeychainCancelled);
        }
        self.inner.delete(key)
    }
}

struct Idle;
#[async_trait]
impl AgentModel for Idle {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "mcp-area".into()
    }
}
struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

struct Area {
    boot: BootstrappedHost,
    keychain: Arc<Keychain>,
    sub: Subscription,
    /// Every event and reply the client could have seen, as JSON text.
    observed: Vec<String>,
    /// Peers that started: one per server process the core spawned.
    started: Arc<AtomicUsize>,
    root: PathBuf,
    next: usize,
    /// What the started peers report over the control socket.
    peer_events: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    _dir: tempfile::TempDir,
}

fn project() -> ProjectId {
    ProjectId::new("p").unwrap()
}
fn scope(server: &str) -> McpScope {
    McpScope {
        project: project(),
        server_id: server.into(),
    }
}
fn literal(text: &str) -> McpTemplate {
    McpTemplate(vec![McpTemplatePart::Literal { value: text.into() }])
}
fn session() -> SessionId {
    SessionId::new("mcp-area").unwrap()
}

impl Area {
    async fn open() -> Self {
        traces::capture();
        let dir = tempfile::Builder::new()
            .prefix("m11-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".evorch")).unwrap();
        std::fs::write(
            root.join("sidebar.json"),
            serde_json::json!({"projects":[{"id":"p","repo_root":root}]}).to_string(),
        )
        .unwrap();
        // The peer connects here when it starts; the count says whether a
        // server process was started at all.
        let listener = UnixListener::bind(root.join("control")).unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let counter = started.clone();
        let (events, peer_events) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let events = events.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut lines = tokio::io::BufReader::new(stream).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Ok(event) = serde_json::from_str(&line) {
                            let _ = events.send(event);
                        }
                    }
                });
            }
        });
        let command = PathBuf::from(std::env::var("CARGO_BIN_EXE_core-mcp-test-peer").unwrap())
            .canonicalize()
            .unwrap();
        std::fs::write(
            root.join(".evorch/mcp.toml"),
            format!(
                "version=1\n[servers.fixture]\ntype=\"stdio\"\ncommand={command:?}\nargs=[{:?}]\nroles=[\"worker\"]\n[servers.fixture.env]\nTOKEN={{keychain=\"token\"}}\n",
                root.join("control")
            ),
        )
        .unwrap();
        let keychain = Arc::new(Keychain {
            inner: FileCredentialStore::open(root.join("secrets")).unwrap(),
            fail_set: AtomicBool::new(false),
            fail_delete: AtomicBool::new(false),
            after_set: std::sync::Mutex::default(),
        });
        keychain
            .inner
            .set(
                &credential_key(
                    &ApprovalScope {
                        canonical_root: root.clone(),
                        server_id: "fixture".into(),
                    },
                    "token",
                ),
                &Secret::from("peer-token".to_owned()),
            )
            .unwrap();
        let mcp = runtime::mcp::McpHostServices::at_config_dir(
            runtime::mcp::McpCredentialBackend::Available(keychain.clone()),
            root.clone(),
        );
        let boot = BootstrappedHost::open_with_mcp_test_seams(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: root.clone(),
                ownership_root: root.join("owners"),
            },
            TestSeams {
                model: Arc::new(Idle),
                sandbox: Arc::new(Unisolated),
            },
            mcp,
        )
        .await
        .unwrap();
        let sub = boot
            .host
            .lock()
            .unwrap()
            .projection()
            .subscribe_with_snapshot(&Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .unwrap();
        Self {
            boot,
            keychain,
            sub,
            observed: Vec::new(),
            started,
            root,
            next: 0,
            peer_events,
            _dir: dir,
        }
    }

    fn subscribe(&self) -> Subscription {
        self.boot
            .host
            .lock()
            .unwrap()
            .projection()
            .subscribe_with_snapshot(&Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .unwrap()
    }

    fn seed_token(&self, server: &str) {
        self.keychain
            .inner
            .set(
                &credential_key(
                    &ApprovalScope {
                        canonical_root: self.root.clone(),
                        server_id: server.into(),
                    },
                    "token",
                ),
                &Secret::from("peer-token".to_owned()),
            )
            .unwrap();
    }

    fn id(&mut self, label: &str) -> McpOperationId {
        self.next += 1;
        McpOperationId::new(format!("{label}-{}", self.next)).unwrap()
    }

    /// Send a command the way a transport does (with a request id, so a receipt
    /// exists) and keep the reply as the client saw it.
    fn submit(
        &mut self,
        command: McpCommand,
    ) -> Result<(McpOperationView, CommandDisposition), WorkbenchError> {
        let request = RequestId::new(format!("req-{}", command.operation_id().as_str())).unwrap();
        let reply = self.boot.host.lock().unwrap().handle_request(
            &session(),
            &request,
            Command::Mcp(command),
            Provenance::human(Surface::Cli, ClientId::new("m11").unwrap()),
        )?;
        self.observed.push(serde_json::to_string(&reply).unwrap());
        let Some(result) = reply.result.as_deref() else {
            panic!("an MCP command answers with an operation");
        };
        let CommandResult::Mcp(McpResult::Operation(view)) = result else {
            panic!("an MCP command answers with an operation: {result:?}");
        };
        Ok((view.clone(), reply.disposition))
    }

    /// Submit and wait for the operation's `operation_completed` event.
    async fn run(&mut self, command: McpCommand) -> McpOperationView {
        let id = command.operation_id().clone();
        let (view, disposition) = self.submit(command).expect("the command is accepted");
        assert_eq!(disposition, CommandDisposition::Accepted);
        assert_eq!(view.state, McpOperationState::Running);
        self.completed(&id).await
    }

    async fn completed(&mut self, id: &McpOperationId) -> McpOperationView {
        loop {
            let envelope = self.sub.recv().await.expect("the subscription stays open");
            self.observed
                .push(serde_json::to_string(&envelope.event).unwrap());
            self.observed.push(format!("{:?}", envelope.event));
            if let UiEvent::Mcp(McpEvent::OperationCompleted { operation }) = envelope.event
                && &operation.operation_id == id
            {
                return operation;
            }
        }
    }

    fn query(&self, query: McpQuery) -> Result<McpReply, WorkbenchError> {
        match self
            .boot
            .host
            .lock()
            .unwrap()
            .handle_query(&session(), &Query::Mcp(query))?
        {
            QueryReply::Mcp(reply) => Ok(reply),
            other => panic!("not an MCP reply: {other:?}"),
        }
    }

    async fn settings(&mut self) -> McpSettings {
        let id = self.id("refresh");
        let done = self
            .run(McpCommand::Refresh(Refresh {
                operation_id: id,
                project: project(),
            }))
            .await;
        assert!(matches!(done.state, McpOperationState::Succeeded { .. }));
        let McpReply::Settings(settings) = self
            .query(McpQuery::GetSettings { project: project() })
            .unwrap()
        else {
            panic!("settings expected");
        };
        *settings
    }

    async fn definition(&mut self, server: &str) -> (McpSettings, McpServerDefinition) {
        let settings = self.settings().await;
        let definition = settings
            .servers
            .iter()
            .find(|s| s.server_id == server)
            .and_then(|s| s.definition.clone())
            .expect("the server is defined");
        (settings, definition)
    }

    fn save(
        &mut self,
        revision: &McpRevision,
        server: &str,
        definition: McpServerDefinition,
    ) -> McpCommand {
        McpCommand::SaveOverride(Box::new(SaveOverride {
            operation_id: self.id("save"),
            project: project(),
            expected_revision: revision.clone(),
            mutation: McpMutation::Upsert {
                server_id: server.into(),
                definition,
            },
        }))
    }

    /// Preview and confirm an approval for `server` at `revision`.
    async fn approve(&mut self, server: &str, revision: &McpRevision) {
        let id = self.id("preview");
        let done = self
            .run(McpCommand::PreviewApproval(PreviewApproval {
                operation_id: id,
                scope: scope(server),
                expected_revision: revision.clone(),
            }))
            .await;
        let McpOperationState::Succeeded {
            outcome: McpOutcome::ApprovalPreview { preview },
        } = done.state
        else {
            panic!("preview expected: {done:?}");
        };
        let id = self.id("confirm");
        let done = self.run(confirm(id, &preview)).await;
        assert_eq!(
            done.state,
            McpOperationState::Succeeded {
                outcome: McpOutcome::ApprovalConfirmed
            }
        );
    }

    /// Add `hang`: a copy of `fixture` whose peer never answers the handshake.
    /// Returns the revision it was saved at; the server is approved.
    async fn add_hanging(&mut self) -> McpRevision {
        let (settings, definition) = self.definition("fixture").await;
        let mut hang = definition.clone();
        let McpTransport::Stdio { env, .. } = &mut hang.transport else {
            panic!("fixture is a stdio server");
        };
        env.insert(
            "HANG".into(),
            McpValueSource::Template {
                template: literal("1"),
            },
        );
        let add = self.save(&settings.revision, "hang", hang);
        let done = self.run(add).await;
        let McpOperationState::Succeeded {
            outcome: McpOutcome::Settings { revision },
        } = done.state
        else {
            panic!("saved settings expected: {done:?}");
        };
        self.seed_token("hang");
        self.approve("hang", &revision).await;
        revision
    }

    /// Wait until a peer received the handshake request; its process id.
    async fn handshake_reached_a_peer(&mut self) -> u32 {
        let mut pid = None;
        loop {
            let event = self.peer_events.recv().await.expect("peers stay connected");
            match event["event"].as_str() {
                Some("started") => pid = event["pid"].as_u64().map(|p| p as u32),
                Some("discover") => return pid.expect("a peer starts before it is asked"),
                _ => {}
            }
        }
    }

    /// Wait, woken by each end of an operation, until the host may stop on idle.
    async fn until_idle(&self) {
        let ended = self.boot.area_work_notify();
        while self.boot.busy() {
            ended.notified().await;
        }
    }

    fn test(&mut self, server: &str, revision: &McpRevision) -> McpCommand {
        McpCommand::TestConnection(TestConnection {
            operation_id: self.id("test"),
            scope: scope(server),
            expected_revision: revision.clone(),
        })
    }

    /// Nothing the client could observe, nor the host log, holds the secret.
    fn assert_secret_absent(&self, secret: &str) {
        for seen in &self.observed {
            assert!(!seen.contains(secret), "secret reached the client: {seen}");
        }
        traces::assert_redacted(secret);
        let config = std::fs::read_to_string(self.root.join(".evorch/mcp.toml")).unwrap();
        assert!(!config.contains(secret), "secret reached the config file");
    }
}

fn confirm(id: McpOperationId, preview: &McpApprovalPreview) -> McpCommand {
    McpCommand::ConfirmApproval(ConfirmApproval {
        operation_id: id,
        preview_id: preview.preview_id.clone(),
        scope: preview.scope.clone(),
        expected_revision: preview.source_revision.clone(),
        expected_fingerprint: preview.fingerprint.clone(),
    })
}

fn roles(definition: &McpServerDefinition, roles: &[&str]) -> McpServerDefinition {
    McpServerDefinition {
        roles: roles.iter().map(|r| (*r).to_owned()).collect(),
        ..definition.clone()
    }
}

fn error(done: &McpOperationView) -> McpErrorCode {
    match &done.state {
        McpOperationState::Failed { error } => *error,
        other => panic!("a failure was expected: {other:?}"),
    }
}

#[tokio::test]
async fn a_command_quoting_an_older_revision_is_refused_before_anything_runs() {
    let mut area = Area::open().await;
    let (settings, definition) = area.definition("fixture").await;
    let first = area.save(
        &settings.revision,
        "fixture",
        roles(&definition, &["worker", "reviewer"]),
    );
    let done = area.run(first).await;
    assert!(matches!(done.state, McpOperationState::Succeeded { .. }));
    // The core now knows a newer revision: the old one is refused at the door.
    let stale = area.save(
        &settings.revision,
        "fixture",
        roles(&definition, &["reviewer"]),
    );
    let refused = area.submit(stale).expect_err("a stale revision is refused");
    assert_eq!(refused.code, ErrorCode::Conflict);
    let listed = area
        .query(McpQuery::ListOperations { project: None })
        .unwrap();
    let McpReply::Operations { operations } = listed else {
        panic!("operations expected");
    };
    assert!(
        operations
            .iter()
            .all(|op| op.kind != McpOperationKind::SaveOverride
                || op.state != McpOperationState::Running),
        "the refused command left no operation behind"
    );
}

#[tokio::test]
async fn an_edit_after_the_preview_makes_the_confirmation_fail_and_approves_nothing() {
    let mut area = Area::open().await;
    let settings = area.settings().await;
    let id = area.id("preview");
    let done = area
        .run(McpCommand::PreviewApproval(PreviewApproval {
            operation_id: id,
            scope: scope("fixture"),
            expected_revision: settings.revision.clone(),
        }))
        .await;
    let McpOperationState::Succeeded {
        outcome: McpOutcome::ApprovalPreview { preview },
    } = done.state
    else {
        panic!("preview expected");
    };
    // Another writer changes the project's configuration after the preview.
    let path = area.root.join(".evorch/mcp.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n# edited after the preview\n[servers.other]\ntype=\"stdio\"\ncommand=\"/bin/true\"\nroles=[\"worker\"]\n");
    std::fs::write(&path, text).unwrap();
    let id = area.id("confirm");
    let done = area.run(confirm(id, &preview)).await;
    assert!(
        matches!(
            error(&done),
            McpErrorCode::StalePreview | McpErrorCode::ConfigConflict
        ),
        "{done:?}"
    );
    // Nothing was approved: testing still needs an approval.
    let fresh = area.settings().await;
    let test = area.test("fixture", &fresh.revision);
    let done = area.run(test).await;
    assert_eq!(error(&done), McpErrorCode::NeedsApproval);
}

#[tokio::test]
async fn a_test_without_approval_is_refused_and_starts_no_process() {
    let mut area = Area::open().await;
    let settings = area.settings().await;
    let test = area.test("fixture", &settings.revision);
    let done = area.run(test).await;
    assert_eq!(error(&done), McpErrorCode::NeedsApproval);
    assert_eq!(area.started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn add_approve_test_revoke_delete_goes_through_the_same_approval_path() {
    let mut area = Area::open().await;
    let (settings, definition) = area.definition("fixture").await;
    // Add a second server (a copy under a new id), approve it, test it.
    let add = area.save(&settings.revision, "copy", definition.clone());
    let done = area.run(add).await;
    let McpOperationState::Succeeded {
        outcome: McpOutcome::Settings { revision },
    } = done.state
    else {
        panic!("saved settings expected: {done:?}");
    };
    area.seed_token("copy");
    area.approve("copy", &revision).await;
    let before = area.started.load(Ordering::SeqCst);
    let test = area.test("copy", &revision);
    let done = area.run(test).await;
    assert_eq!(
        done.state,
        McpOperationState::Succeeded {
            outcome: McpOutcome::Tested {
                status: McpServerStatus::Ready
            }
        },
        "{done:?}"
    );
    assert_eq!(area.started.load(Ordering::SeqCst), before + 1);
    // Revoke: the next test needs an approval again and starts nothing.
    let id = area.id("revoke");
    let done = area
        .run(McpCommand::RevokeApproval(RevokeApproval {
            operation_id: id,
            scope: scope("copy"),
        }))
        .await;
    assert_eq!(
        done.state,
        McpOperationState::Succeeded {
            outcome: McpOutcome::ApprovalRevoked
        }
    );
    let test = area.test("copy", &revision);
    let done = area.run(test).await;
    assert!(
        matches!(
            error(&done),
            McpErrorCode::NeedsApproval | McpErrorCode::ApprovalRevoked
        ),
        "{done:?}"
    );
    assert_eq!(area.started.load(Ordering::SeqCst), before + 1);
    // Delete the override again.
    let id = area.id("remove");
    let done = area
        .run(McpCommand::SaveOverride(Box::new(SaveOverride {
            operation_id: id,
            project: project(),
            expected_revision: revision,
            mutation: McpMutation::RemoveOverride {
                server_id: "copy".into(),
            },
        })))
        .await;
    assert!(
        matches!(done.state, McpOperationState::Succeeded { .. }),
        "{done:?}"
    );
    let after = area.settings().await;
    assert!(after.servers.iter().all(|s| s.server_id != "copy"));
}

#[tokio::test]
async fn cancel_ends_a_running_connection_test_reaps_its_child_and_returns_the_permit() {
    let mut area = Area::open().await;
    let revision = area.add_hanging().await;
    let mut last = None;
    // One more round than the project may run at once: a permit that was not
    // returned by a cancelled test would refuse the last one.
    for _ in 0..=tools::mcp::types::MAX_STDIO_PER_PROJECT {
        let test = area.test("hang", &revision);
        let id = test.operation_id().clone();
        let (view, _) = area.submit(test).unwrap();
        assert_eq!(view.state, McpOperationState::Running);
        // Cancel only once the mock holds the open handshake.
        let pid = area.handshake_reached_a_peer().await;
        let (cancelled, _) = area
            .submit(McpCommand::CancelOperation(CancelOperation {
                operation_id: id.clone(),
            }))
            .unwrap();
        assert_eq!(cancelled.operation_id, id);
        let done = area.completed(&id).await;
        assert!(
            matches!(
                done.state,
                McpOperationState::Cancelled
                    | McpOperationState::Failed {
                        error: McpErrorCode::Cancelled
                    }
            ),
            "{done:?}"
        );
        // A child that was not waited for would stay as a zombie entry.
        #[cfg(target_os = "linux")]
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the cancelled test's child process was reaped"
        );
        let _ = pid;
        last = Some((id, done));
    }
    // The outcome stays readable, and a finished operation cannot be cancelled again.
    let (id, done) = last.unwrap();
    let McpReply::Operation(again) = area
        .query(McpQuery::GetOperation { operation_id: id })
        .unwrap()
    else {
        panic!("operation expected");
    };
    assert_eq!(again, done);
}

#[tokio::test]
async fn a_running_operation_keeps_the_host_busy_and_a_status_watcher_does_not() {
    let mut area = Area::open().await;
    let revision = area.add_hanging().await;
    // Operations ran, so the status watcher of the project is up. It is not work.
    area.until_idle().await;
    assert!(!area.boot.busy());
    let test = area.test("hang", &revision);
    let id = test.operation_id().clone();
    area.submit(test).unwrap();
    assert!(area.boot.busy(), "a running operation stops an idle exit");
    area.handshake_reached_a_peer().await;
    area.submit(McpCommand::CancelOperation(CancelOperation {
        operation_id: id.clone(),
    }))
    .unwrap();
    area.completed(&id).await;
    // Its end wakes the idle decision, which then finds nothing running.
    area.until_idle().await;
    assert!(!area.boot.busy());
}

#[tokio::test]
async fn settings_tell_whether_each_keychain_reference_holds_a_value_and_never_the_value() {
    let mut area = Area::open().await;
    let (settings, definition) = area.definition("fixture").await;
    let fixture = settings
        .servers
        .iter()
        .find(|s| s.server_id == "fixture")
        .unwrap();
    assert_eq!(
        fixture.secret_states.get("token"),
        Some(&McpSecretState::Present)
    );
    // A copy refers to its own, not yet stored, entry.
    let add = area.save(&settings.revision, "copy", definition);
    area.run(add).await;
    let after = area.settings().await;
    let copy = after
        .servers
        .iter()
        .find(|s| s.server_id == "copy")
        .unwrap();
    assert_eq!(
        copy.secret_states.get("token"),
        Some(&McpSecretState::Missing)
    );
    area.seed_token("copy");
    let after = area.settings().await;
    let copy = after
        .servers
        .iter()
        .find(|s| s.server_id == "copy")
        .unwrap();
    assert_eq!(
        copy.secret_states.get("token"),
        Some(&McpSecretState::Present)
    );
    area.assert_secret_absent("peer-token");
}

#[tokio::test]
async fn a_client_that_disconnected_reads_the_outcome_and_a_resend_runs_nothing_twice() {
    let mut area = Area::open().await;
    let settings = area.settings().await;
    let (_, definition) = area.definition("fixture").await;
    let command = area.save(
        &settings.revision,
        "fixture",
        roles(&definition, &["worker", "planner"]),
    );
    let id = command.operation_id().clone();
    // The client's own subscription goes away before the core finishes; an
    // observer stands in for "the core is done".
    let mut observer = area.subscribe();
    let replacement = area.subscribe();
    drop(std::mem::replace(&mut area.sub, replacement));
    let (view, _) = area.submit(command.clone()).unwrap();
    assert_eq!(view.state, McpOperationState::Running);
    loop {
        let envelope = observer.recv().await.unwrap();
        if let UiEvent::Mcp(McpEvent::OperationCompleted { operation }) = envelope.event
            && operation.operation_id == id
        {
            break;
        }
    }
    let McpReply::Operation(finished) = area
        .query(McpQuery::GetOperation {
            operation_id: id.clone(),
        })
        .unwrap()
    else {
        panic!("operation expected");
    };
    assert!(finished.state.is_finished());
    assert!(
        matches!(finished.state, McpOperationState::Succeeded { .. }),
        "{finished:?}"
    );
    // The same id and request again: answered from the table, not run again.
    let (again, disposition) = area.submit(command).unwrap();
    assert_eq!(disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(again, finished);
    // The same id with a different request is a conflict.
    let other = McpCommand::SaveOverride(Box::new(SaveOverride {
        operation_id: id,
        project: project(),
        expected_revision: settings.revision,
        mutation: McpMutation::Disable {
            server_id: "fixture".into(),
        },
    }));
    assert_eq!(area.submit(other).unwrap_err().code, ErrorCode::Conflict);
}

#[tokio::test]
async fn a_failed_keychain_write_changes_nothing_and_a_failed_delete_is_partial() {
    let mut area = Area::open().await;
    let (settings, definition) = area.definition("fixture").await;
    let with_key = {
        let McpTransport::Stdio {
            command,
            args,
            cwd,
            env,
        } = definition.transport.clone()
        else {
            panic!("stdio fixture");
        };
        let mut env = env;
        env.insert(
            "API".into(),
            McpValueSource::Keychain { name: "api".into() },
        );
        McpServerDefinition {
            transport: McpTransport::Stdio {
                command,
                args,
                cwd,
                env,
            },
            ..definition.clone()
        }
    };
    let save_secret = |area: &mut Area, revision: &McpRevision, value: &str| {
        McpCommand::SaveSecret(Box::new(SaveSecret {
            operation_id: area.id("secret"),
            scope: scope("fixture"),
            logical_name: "api".into(),
            value: McpSecretValue::new(value),
            expected_revision: revision.clone(),
            mutation: McpMutation::Upsert {
                server_id: "fixture".into(),
                definition: with_key.clone(),
            },
        }))
    };
    // The keychain refuses: the save fails and the configuration is untouched.
    area.keychain.fail_set.store(true, Ordering::SeqCst);
    let command = save_secret(&mut area, &settings.revision, SECRET);
    let done = area.run(command).await;
    assert!(
        matches!(
            error(&done),
            McpErrorCode::SecretLocked | McpErrorCode::SecretUnavailable
        ),
        "{done:?}"
    );
    let unchanged = area.settings().await;
    assert_eq!(unchanged.revision, settings.revision);
    // The keychain works: the secret is stored and the configuration updated.
    area.keychain.fail_set.store(false, Ordering::SeqCst);
    let command = save_secret(&mut area, &settings.revision, SECRET);
    let done = area.run(command).await;
    let McpOperationState::Succeeded {
        outcome: McpOutcome::Settings { revision },
    } = done.state
    else {
        panic!("saved settings expected: {done:?}");
    };
    // Deleting while the keychain refuses: the configuration part is saved, the
    // key may still exist, and the answer says exactly that.
    area.keychain.fail_delete.store(true, Ordering::SeqCst);
    let id = area.id("delete");
    let done = area
        .run(McpCommand::DeleteSecret(Box::new(DeleteSecret {
            operation_id: id,
            scope: scope("fixture"),
            logical_name: "api".into(),
            expected_revision: revision,
            mutation: McpMutation::Upsert {
                server_id: "fixture".into(),
                definition: definition.clone(),
            },
        })))
        .await;
    let McpOperationState::Partial { keychain_error, .. } = done.state else {
        panic!("a partial success was expected: {done:?}");
    };
    assert!(matches!(
        keychain_error,
        McpErrorCode::SecretDenied | McpErrorCode::SecretUnavailable
    ));
    area.assert_secret_absent(SECRET);
}

#[tokio::test]
async fn a_secret_appears_in_no_event_reply_receipt_snapshot_or_log() {
    let mut area = Area::open().await;
    let (settings, definition) = area.definition("fixture").await;
    let McpTransport::Stdio {
        command,
        args,
        cwd,
        mut env,
    } = definition.transport.clone()
    else {
        panic!("stdio fixture");
    };
    env.insert(
        "API".into(),
        McpValueSource::Keychain { name: "api".into() },
    );
    let with_key = McpServerDefinition {
        transport: McpTransport::Stdio {
            command,
            args,
            cwd,
            env,
        },
        ..definition
    };
    let command = McpCommand::SaveSecret(Box::new(SaveSecret {
        operation_id: area.id("secret"),
        scope: scope("fixture"),
        logical_name: "api".into(),
        value: McpSecretValue::new(SECRET),
        expected_revision: settings.revision,
        mutation: McpMutation::Upsert {
            server_id: "fixture".into(),
            definition: with_key,
        },
    }));
    assert!(!format!("{command:?}").contains(SECRET));
    let id = command.operation_id().clone();
    let done = area.run(command).await;
    assert!(
        matches!(done.state, McpOperationState::Succeeded { .. }),
        "{done:?}"
    );
    // Every read the client has: settings, the operation, the list, the receipt.
    let settings = area.settings().await;
    area.observed
        .push(serde_json::to_string(&settings).unwrap());
    area.observed.push(format!("{settings:?}"));
    let operation = area
        .query(McpQuery::GetOperation {
            operation_id: id.clone(),
        })
        .unwrap();
    area.observed
        .push(serde_json::to_string(&operation).unwrap());
    let list = area
        .query(McpQuery::ListOperations { project: None })
        .unwrap();
    area.observed.push(serde_json::to_string(&list).unwrap());
    let receipt = area
        .boot
        .host
        .lock()
        .unwrap()
        .handle_query(
            &session(),
            &Query::Projection(ProjectionQuery::GetCommandReceipt {
                request_id: RequestId::new(format!("req-{}", id.as_str())).unwrap(),
            }),
        )
        .unwrap();
    area.observed.push(serde_json::to_string(&receipt).unwrap());
    area.observed.push(format!("{receipt:?}"));
    // A fresh subscription's snapshot.
    let snapshot = area
        .boot
        .host
        .lock()
        .unwrap()
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();
    area.observed
        .push(serde_json::to_string(&snapshot.started).unwrap());
    area.assert_secret_absent(SECRET);
}

#[tokio::test]
async fn two_writers_quoting_one_revision_cannot_both_succeed() {
    let mut area = Area::open().await;
    let (settings, definition) = area.definition("fixture").await;
    let first = area.save(
        &settings.revision,
        "fixture",
        roles(&definition, &["worker", "planner"]),
    );
    let second = area.save(
        &settings.revision,
        "fixture",
        roles(&definition, &["worker", "advisor"]),
    );
    let ids = [first.operation_id().clone(), second.operation_id().clone()];
    let mut outcomes = Vec::new();
    for command in [first, second] {
        match area.submit(command) {
            // Refused at the door: the core already knows a newer revision.
            Err(refused) => {
                assert_eq!(refused.code, ErrorCode::Conflict);
                outcomes.push(false);
            }
            Ok(_) => outcomes.push(true),
        }
    }
    let mut succeeded = 0;
    for (id, accepted) in ids.iter().zip(outcomes) {
        if !accepted {
            continue;
        }
        let done = area.completed(id).await;
        match done.state {
            McpOperationState::Succeeded { .. } => succeeded += 1,
            McpOperationState::Failed { error } => {
                assert_eq!(error, McpErrorCode::ConfigConflict, "{id:?}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(succeeded, 1, "exactly one of two writers wins");
}

#[tokio::test]
async fn a_secret_save_of_unknown_outcome_is_settled_by_reading_the_files_again() {
    let mut area = Area::open().await;
    let settings = area.settings().await;
    let (_, definition) = area.definition("fixture").await;
    // Another writer changes the files right after the keychain was written:
    // the settings save conflicts, and the rotated value stays in the keychain.
    let file = area.root.join(".evorch/mcp.toml");
    area.keychain
        .after_set
        .lock()
        .unwrap()
        .replace(Box::new(move || {
            let mut text = std::fs::read_to_string(&file).unwrap();
            text.push_str(
                "\n[servers.other]\ntype=\"stdio\"\ncommand=\"/bin/true\"\nroles=[\"worker\"]\n",
            );
            std::fs::write(&file, text).unwrap();
        }));
    let command = McpCommand::SaveSecret(Box::new(SaveSecret {
        operation_id: area.id("secret"),
        scope: scope("fixture"),
        logical_name: "token".into(),
        value: McpSecretValue::new("rotated-secret-value"),
        expected_revision: settings.revision.clone(),
        mutation: McpMutation::Upsert {
            server_id: "fixture".into(),
            definition,
        },
    }));
    let done = area.run(command).await;
    // The keychain changed and the settings did not: not "nothing was written".
    assert_eq!(error(&done), McpErrorCode::OutcomeUnknown, "{done:?}");
    // The core still holds what it read before the other writer's change.
    let McpReply::Settings(stale) = area
        .query(McpQuery::GetSettings { project: project() })
        .unwrap()
    else {
        panic!("settings expected");
    };
    assert_eq!(stale.revision, settings.revision);
    // Reading the files again (what the client asks for after an unknown
    // outcome) shows the latest revision and that the entry holds a value.
    let latest = area.settings().await;
    assert_ne!(latest.revision, settings.revision);
    assert!(latest.servers.iter().any(|s| s.server_id == "other"));
    let fixture = latest
        .servers
        .iter()
        .find(|s| s.server_id == "fixture")
        .unwrap();
    assert_eq!(
        fixture.secret_states.get("token"),
        Some(&McpSecretState::Present)
    );
    area.assert_secret_absent("rotated-secret-value");
}

#[tokio::test]
async fn a_shutdown_after_the_handshake_reaps_the_child_and_drains_the_task() {
    let mut area = Area::open().await;
    let revision = area.add_hanging().await;
    let test = area.test("hang", &revision);
    area.submit(test).unwrap();
    let pid = area.handshake_reached_a_peer().await;
    assert!(area.boot.busy());
    // The host stops while the handshake is open: its operation ends, the
    // child is waited for and the task is joined before shutdown returns.
    area.boot.shutdown().await.unwrap();
    #[cfg(target_os = "linux")]
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the child process was reaped"
    );
    let _ = pid;
    assert!(!area.boot.busy(), "no operation is left running");
}
