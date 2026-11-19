//! F2 turns the artifact provider on: the runtime is given an `ArtifactPages`
//! before any run exists, so `inspect_artifact` is part of the tool catalog from
//! the first request of every conversation and stays frozen whatever the helper
//! does (plan §8.2).
//!
//! The core is bootstrapped (the production composition, including the artifact
//! wiring) and the turns of one conversation cross the production routing and
//! HTTP adapters, for the Chat Completions and the subscription (Claude) wire.
//! The assertions read the recorded requests: the tool array is byte-identical
//! across the turns whether the helper is absent, not started, running, crashed
//! or restarted; the input-derived cache prefix only grows (tool success, a
//! re-open of the artifact, a failed call after a crash, a core restart); and a
//! conversation that last ran without the provider meets exactly one tools-only
//! boundary. Every wait is a lifecycle event of the run, never a duration.
use std::sync::Arc;

use event_bus::AgentRunPhase;
use sandbox::CredentialStore as _;
use serde_json::{Value, json};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::ids::{ProjectId, SubmissionId, ThreadId};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

#[allow(dead_code)]
#[path = "../../runtime/tests/mcp_cache_support/mod.rs"]
mod cache;

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

#[path = "renderer_support/without_artifact_tool.rs"]
mod without_artifact_tool;
use without_artifact_tool::WithoutArtifactTool;

fn tool_names(provider: &cache::Provider, request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .expect("the request carries its tools")
        .iter()
        .map(|tool| {
            let name = match provider {
                cache::Provider::Api(_) => &tool["function"]["name"],
                cache::Provider::Claude(_) => &tool["name"],
            };
            name.as_str().expect("a tool has a name").to_owned()
        })
        .collect()
}

fn offers_inspect(provider: &cache::Provider, request: &Value) -> usize {
    tool_names(provider, request)
        .iter()
        .filter(|name| name.ends_with(runtime::artifacts::INSPECT_ARTIFACT))
        .count()
}

/// The tool result the request carries last, as the provider received it.
fn last_tool_result(provider: &cache::Provider, request: &Value) -> String {
    let messages = request["messages"].as_array().expect("a message array");
    let message = match provider {
        cache::Provider::Api(_) => messages.iter().rev().find(|m| m["role"] == "tool"),
        cache::Provider::Claude(_) => messages.iter().rev().find(|m| {
            m["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
        }),
    };
    message.expect("a tool result was sent").to_string()
}

/// `artifact-<32 hex>` in a request that carries the capture's result.
fn artifact_id(requests: &[Value]) -> String {
    for body in requests.iter().map(Value::to_string) {
        for (at, _) in body.match_indices("artifact-") {
            let id: String = body[at..].chars().take("artifact-".len() + 32).collect();
            if id["artifact-".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit())
                && id.len() == "artifact-".len() + 32
            {
                return id;
            }
        }
    }
    panic!(
        "no request carries the captured artifact id: {}",
        requests.last().map(Value::to_string).unwrap_or_default()
    );
}

/// Which helper the core is told to start; nothing is started at bootstrap.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Helper {
    /// The configured path does not exist: opening a page fails, the catalog does not.
    Absent,
    /// The fake helper that serves the IPC protocol and renders nothing.
    #[cfg(feature = "renderer-test-helper")]
    Fake,
}

impl Helper {
    fn path(self) -> std::path::PathBuf {
        match self {
            Self::Absent => "/nonexistent-evorch-helper/evorch-renderer".into(),
            #[cfg(feature = "renderer-test-helper")]
            Self::Fake => std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper")
                .expect("the fake helper is built with the renderer-test-helper feature")
                .into(),
        }
    }
}

/// The core reads the helper path from the process environment while it is
/// bootstrapped; boots in this binary take turns so none sees another's value.
static HELPER_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct World {
    project: cache::Project,
    provider: cache::Provider,
    config: config::Config,
    user: std::path::PathBuf,
    root: std::path::PathBuf,
}

struct Running {
    core: BootstrappedHost,
    receiver: event_bus::EventReceiver,
    /// The run of the conversation on this core once a chat reached it.
    run: Option<runtime::RunId>,
}

impl World {
    async fn new(subscription: bool) -> Self {
        let project = cache::Project::new();
        let provider = cache::Provider::new(subscription).await;
        let config = cache::configuration(&provider);
        let root = project.root().canonicalize().unwrap();
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            root.join("page.html"),
            "<!doctype html><button>Add</button>",
        )
        .unwrap();
        let store = SidebarStore::new(user.join("sidebar.json"));
        let mut document = SidebarDocument::default();
        document
            .sidebar
            .add_project(StateProject::new("p"), "repo", &root)
            .unwrap();
        document.sidebar.threads.push(ThreadRecord::new(
            StateThread::new("t1"),
            StateProject::new("p"),
            "first",
        ));
        document.sidebar.refresh_derived();
        {
            let lock = store.lock().unwrap();
            store.write(&lock, &document).unwrap();
        }
        project
            .secrets
            .set(
                "claude",
                &sandbox::Secret::from(
                    json!({
                        "access_token": "fixture-mcp-token",
                        "refresh_token": "fixture-refresh",
                        "expires_at": 4_102_444_800u64
                    })
                    .to_string(),
                ),
            )
            .unwrap();
        Self {
            project,
            provider,
            config,
            user,
            root,
        }
    }

    /// A core over this world's directories. `hide_artifact_tool` wraps the
    /// model of a core that does not offer `inspect_artifact` while it is set.
    async fn boot(
        &self,
        helper: Helper,
        hide_artifact_tool: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Running {
        let bus = Arc::new(event_bus::EventBus::new(4096));
        let model = workbench_host::bootstrap::models::compose_production_model(
            &self.config,
            &workbench_host::bootstrap::models::ProductionModel {
                load_options: config::LoadOptions {
                    user_config_dir: Some(self.user.clone()),
                    ..Default::default()
                },
                credential_store: self.project.secrets.clone(),
                bus: bus.clone(),
                env: Arc::new(routing::MapEnv::from_iter([(
                    "MCP_CACHE_KEY",
                    "fixture-key",
                )])),
                model_catalog_dir: Some(test_support::model_catalog_dir()),
            },
        )
        .unwrap();
        let model: Arc<dyn runtime::AgentModel> = match hide_artifact_tool {
            Some(hidden) => Arc::new(WithoutArtifactTool(model, hidden)),
            None => model,
        };
        let _only_boot = HELPER_ENV.lock().await;
        // SAFETY: boots hold HELPER_ENV, and nothing else in this binary reads or
        // writes the variable while one runs.
        unsafe { std::env::set_var("EVORCH_RENDERER_EXEC", helper.path()) };
        let core = BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: self.user.clone(),
                project_root: self.root.clone(),
                ownership_root: self.project.sockets().to_path_buf(),
            },
            TestSeams {
                model,
                sandbox: Arc::new(Unisolated),
            },
        )
        .await
        .unwrap();
        let receiver = core.runtime.event_bus().subscribe();
        Running {
            core,
            receiver,
            run: None,
        }
    }

    /// One user turn on the thread: it scripts `steps`, sends `text`, and returns
    /// when the run waits again. The first call creates the thread.
    async fn turn(&self, running: &mut Running, first: bool, text: &str, steps: Vec<cache::Step>) {
        self.provider.enqueue(steps);
        if let Some(run) = running.run {
            running.core.runtime.send_message(run, text.into()).unwrap();
            cache::through(&mut running.receiver, run, AgentRunPhase::Waiting).await;
            return;
        }
        let send = SendChat {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t1").unwrap(),
            submission_id: SubmissionId::new(uuid::Uuid::new_v4().to_string()).unwrap(),
            composer_role: ComposerRole::Worker,
            text: text.into(),
            skills: vec![],
            attachments: vec![],
            model_preference: None,
            fork_seed: None,
        };
        let run: runtime::RunId = {
            let mut host = running.core.host.lock().unwrap();
            if first {
                host.reserve_new_chat_thread(&send).unwrap();
            }
            host.handle_command(
                Command::Conversation(
                    workbench_protocol::namespaces::ConversationCommand::SendChat(send),
                ),
                local_desktop(),
            )
            .unwrap()
            .run_id
            .expect("the chat was accepted")
            .as_str()
            .parse()
            .unwrap()
        };
        running.run = Some(run);
        cache::through(&mut running.receiver, run, AgentRunPhase::Waiting).await;
    }

    fn inspect(&self, arguments: Value) -> cache::Step {
        cache::call_with(runtime::artifacts::INSPECT_ARTIFACT, arguments)
    }

    async fn finish(self, mut running: Running) {
        self.provider.assert_drained();
        running.core.shutdown().await.unwrap();
    }
}

/// The tools of every request are one array, `inspect_artifact` is in it once,
/// and each request extends the previous input.
fn assert_frozen_with_artifact_tool(provider: &cache::Provider, requests: &[Value]) {
    cache::assert_frozen(provider, requests);
    for request in requests {
        assert_eq!(
            offers_inspect(provider, request),
            1,
            "the provider is Some: the tool is offered once: {:?}",
            tool_names(provider, request)
        );
    }
}

#[cfg(feature = "renderer-test-helper")]
#[path = "renderer_support/data_peer.rs"]
mod data_peer;

/// A client of the running core on its Unix socket opens a preview of the
/// agent's artifact and moves bytes over data connections: raw frames, a
/// refused binding and an artifact that has no image form. None of it may
/// reach the provider, so the caller's assertions on the recorded requests
/// cover the data plane too. Everything it opens is closed again.
#[cfg(feature = "renderer-test-helper")]
async fn data_plane_traffic(running: &Running, world: &World, artifact: &str) {
    use data_peer::{Peer, refusal};
    use sha2::{Digest, Sha256};
    use workbench_protocol::errors::ErrorCode;
    use workbench_protocol::ids::{ProjectId, ThreadId};
    use workbench_protocol::namespaces::artifacts::ArtifactRef;
    use workbench_protocol::namespaces::browser::*;

    let socket = world.project.sockets().join("data-plane-cache.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let backend = Arc::new(evorch_core::backend::HostBackend::new(
        running.core.host.clone(),
        "data-plane-cache".into(),
    ));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (backend, stopped) = (backend.clone(), stopped.clone());
            tokio::spawn(async move {
                let _ = evorch_core::unix::connection(stream, backend, stopped).await;
            });
        }
    });
    let digest = format!(
        "{:x}",
        Sha256::digest(std::fs::read(world.root.join("page.html")).unwrap())
    );
    let page = ArtifactRef {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t1").unwrap(),
        artifact_id: artifact.into(),
        digest,
    };
    let mut a = Peer::connect(&socket, "client-a").await;
    let b = Peer::connect(&socket, "client-b").await;
    let session = a.session().await;
    let tab = a.open(&page, &session).await;
    let subscribe = a
        .browser(BrowserCommand::SubscribeRendererFrames {
            tab: tab.reference.clone(),
        })
        .await
        .unwrap();
    let RendererOutput::Subscription {
        subscription,
        token,
        ..
    } = a.output(&subscribe).await
    else {
        panic!("not a subscription")
    };
    assert_eq!(
        refusal(
            b.channel()
                .await
                .bind_frames(&b.client, subscription.clone(), token.clone())
                .await
        ),
        ErrorCode::PermissionDenied
    );
    let mut frames = a
        .channel()
        .await
        .bind_frames(&a.client, subscription, token)
        .await
        .unwrap();
    a.browser(BrowserCommand::SetRendererStreaming {
        tab: tab.reference.clone(),
        enabled: true,
    })
    .await
    .unwrap();
    assert!(frames.next_frame().await.unwrap().is_some());
    assert_eq!(
        refusal(a.channel().await.bind_artifact(&a.client, page).await),
        ErrorCode::InvalidRequest
    );
    let close = a
        .browser(BrowserCommand::CloseRendererSession { session })
        .await
        .unwrap();
    a.output(&close).await;
    drop((a, b, frames));
    let _ = stop.send(true);
    server.abort();
}

/// The catalog is the same whether the helper was never started, is running, was
/// killed, or cannot be started at all; the tool results differ, the wire prefix
/// only grows.
async fn catalog_scenario(subscription: bool, helper: Helper) {
    let world = World::new(subscription).await;
    let mut running = world.boot(helper, None).await;
    let started_before = running.core.spawn_ledger.children().len();
    // Helper not started yet: a failed call, then a capture (no helper needed).
    world
        .turn(
            &mut running,
            true,
            cache::GOAL,
            vec![
                world.inspect(json!({"action": "snapshot"})),
                cache::call_with(
                    "render_artifact",
                    json!({"path": "page.html", "title": "page"}),
                ),
                cache::answer(),
            ],
        )
        .await;
    assert_eq!(running.core.spawn_ledger.children().len(), started_before);
    let id = artifact_id(&world.provider.requests());
    // Open the page: the helper starts (or cannot), the tool succeeds (or fails).
    world
        .turn(
            &mut running,
            false,
            "Open it",
            vec![
                world.inspect(json!({"action": "open", "artifact_id": id})),
                cache::answer(),
            ],
        )
        .await;
    let opened = running.core.spawn_ledger.children().len() > started_before;
    assert_eq!(opened, helper != Helper::Absent, "helper start");
    // Open the same artifact again, then drive the page.
    world
        .turn(
            &mut running,
            false,
            "Open it again",
            vec![
                world.inspect(json!({"action": "open", "artifact_id": id})),
                world.inspect(json!({"action": "snapshot"})),
                cache::answer(),
            ],
        )
        .await;
    if helper != Helper::Absent {
        #[cfg(feature = "renderer-test-helper")]
        data_plane_traffic(&running, &world, &id).await;
        // The helper is stopped cleanly and a later open starts a new one.
        let owner = running.core.host.lock().unwrap().renderer_owner().unwrap();
        owner.stop().await.unwrap();
        let before = running.core.spawn_ledger.children().len();
        world
            .turn(
                &mut running,
                false,
                "Open it after the stop",
                vec![
                    world.inspect(json!({"action": "open", "artifact_id": id})),
                    world.inspect(json!({"action": "snapshot"})),
                    cache::answer(),
                ],
            )
            .await;
        assert_eq!(
            running.core.spawn_ledger.children().len(),
            before + 1,
            "the reopened page runs on a new helper"
        );
        // The helper dies under the conversation: the next call fails, the next
        // request still carries the same catalog and an unchanged prefix.
        running.core.spawn_ledger.stop_all_children();
        world
            .turn(
                &mut running,
                false,
                "Look again",
                vec![
                    world.inspect(json!({"action": "snapshot"})),
                    cache::answer(),
                ],
            )
            .await;
    }
    let requests = world.provider.requests();
    assert_frozen_with_artifact_tool(&world.provider, &requests);
    assert!(
        requests.len() >= 8,
        "every scripted step was one request: {}",
        requests.len()
    );
    let first_open = last_tool_result(&world.provider, &requests[4]);
    if helper == Helper::Absent {
        assert!(
            first_open.contains("renderer is unavailable"),
            "{first_open}"
        );
    } else {
        assert!(!first_open.contains("is_error\":true"), "{first_open}");
        assert!(first_open.contains("button \\\"Add\\\""), "{first_open}");
        let after_crash = last_tool_result(&world.provider, requests.last().unwrap());
        assert!(
            after_crash.contains("no artifact page is open"),
            "the crashed helper's page is gone: {after_crash}"
        );
    }
    world.finish(running).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalog_and_prefix_stay_frozen_when_the_helper_cannot_be_started() {
    for subscription in [false, true] {
        catalog_scenario(subscription, Helper::Absent).await;
    }
}

#[cfg(feature = "renderer-test-helper")]
#[tokio::test(flavor = "multi_thread")]
async fn the_catalog_and_prefix_stay_frozen_while_the_helper_is_not_started_running_and_crashed() {
    for subscription in [false, true] {
        catalog_scenario(subscription, Helper::Fake).await;
    }
}

/// A conversation last sent by a core without the provider meets one tools-only
/// boundary when the current core takes it over: only `inspect_artifact` is
/// added, system, model, settings, affinity and history are unchanged, and the
/// next turns reuse the new prefix.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_from_before_the_provider_gets_one_tools_only_boundary() {
    for subscription in [false, true] {
        let world = World::new(subscription).await;
        let hidden = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut running = world.boot(Helper::Absent, Some(hidden.clone())).await;
        world
            .turn(&mut running, true, cache::GOAL, vec![cache::answer()])
            .await;
        world
            .turn(&mut running, false, "One more", vec![cache::answer()])
            .await;
        hidden.store(false, std::sync::atomic::Ordering::SeqCst);
        world
            .turn(
                &mut running,
                false,
                "After the upgrade",
                vec![cache::answer()],
            )
            .await;
        world
            .turn(&mut running, false, "And again", vec![cache::answer()])
            .await;
        let requests = world.provider.requests();
        assert_eq!(requests.len(), 4);
        for request in &requests[..2] {
            assert_eq!(
                offers_inspect(&world.provider, request),
                0,
                "pre-F2 catalog"
            );
        }
        cache::assert_frozen(&world.provider, &requests[..2]);
        // The boundary: the tool array differs by exactly `inspect_artifact`...
        let before = tool_names(&world.provider, &requests[1]);
        let after = tool_names(&world.provider, &requests[2]);
        let added: Vec<_> = after.iter().filter(|name| !before.contains(name)).collect();
        assert_eq!(added.len(), 1, "{added:?}");
        assert!(added[0].ends_with(runtime::artifacts::INSPECT_ARTIFACT));
        assert_eq!(after.len(), before.len() + 1);
        // ...and nothing else: with the old array put back, the request is an
        // ordinary append (same settings, system and affinity, same history).
        let mut as_before = requests[2].clone();
        as_before["tools"] = requests[1]["tools"].clone();
        mock_openai::cache_contract::assert_append_only(
            world.provider.protocol(),
            &requests[1],
            &as_before,
        )
        .unwrap();
        // After the boundary the catalog is frozen and the prefix reused.
        assert_frozen_with_artifact_tool(&world.provider, &requests[2..]);
        world.finish(running).await;
    }
}
