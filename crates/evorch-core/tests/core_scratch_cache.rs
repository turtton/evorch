//! DC-25: production cold bootstrap, scratch sweep and ordinary host continuation.
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, ProviderEvent};
use mock_openai::{
    ScriptedResponse, StreamingMockOpenAi,
    cache_contract::{CacheProtocol, assert_append_only},
};
use runtime::RunId;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use workbench_host::{
    bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams},
    provenance::local_desktop,
};
use workbench_protocol::{
    commands::{Command, ComposerRole, ContinueChat, SendChat, ThreadTarget},
    ids::{ProjectId, SessionId, SubmissionId, ThreadId},
    namespaces::{
        ConversationCommand, ExecutionCommand, WorkspaceCommand, WorkspaceQuery, WorkspaceReply,
        workspace::{ProjectTarget, SetProjectTrust, TrustLevel},
    },
    queries::{Query, QueryReply},
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

const THREADS: [&str; 2] = ["scratch-one", "scratch-two"];
const MODEL: &str = "mock-model";
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
fn approve_project(core: &BootstrappedHost) {
    let mut host = core.host.lock().unwrap();
    let QueryReply::Workspace(WorkspaceReply::ProjectAccess(access)) = host
        .handle_query(
            &SessionId::new("scratch-cache").unwrap(),
            &Query::Workspace(WorkspaceQuery::GetProjectAccess(ProjectTarget {
                project: ProjectId::new("p").unwrap(),
            })),
            &local_desktop(),
        )
        .unwrap()
    else {
        panic!("project access reply");
    };
    host.handle_command(
        Command::Workspace(WorkspaceCommand::SetProjectTrust(SetProjectTrust {
            project: access.project,
            expected_revision: access.revision,
            trust: TrustLevel::Approved,
        })),
        local_desktop(),
    )
    .unwrap();
}
struct Fixture {
    directory: test_socket_dir::SplitDir,
    user: PathBuf,
    root: PathBuf,
    mock: StreamingMockOpenAi,
    bus: Arc<EventBus>,
}
impl Fixture {
    fn new() -> Self {
        let directory = test_socket_dir::SplitDir::new();
        let root = directory.path().canonicalize().unwrap();
        let user = root.join("user");
        std::fs::create_dir(&user).unwrap();
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(vec![], vec![MODEL.into()]);
        std::fs::write(
            user.join("config.toml"),
            format!(
                r#"
[providers.local]
type = "openai"
api_protocol = "openai-responses"
base_url = "{}"
api_key_env = "SCRATCH_CACHE_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.worker]]
profile = "local"
[compaction]
context_window_tokens = 1000000
"#,
                mock.base_url()
            ),
        )
        .unwrap();
        let store = SidebarStore::new(user.join("sidebar.json"));
        let mut sidebar = SidebarDocument::default();
        sidebar
            .sidebar
            .add_project(StateProject::new("p"), "repo", &root)
            .unwrap();
        for thread in THREADS {
            sidebar.sidebar.threads.push(ThreadRecord::new(
                StateThread::new(thread),
                StateProject::new("p"),
                thread,
            ));
        }
        sidebar.sidebar.refresh_derived();
        let lock = store.lock().unwrap();
        store.write(&lock, &sidebar).unwrap();
        Self {
            directory,
            user,
            root,
            mock,
            bus: Arc::new(EventBus::new(4096)),
        }
    }
    async fn open(&self) -> BootstrappedHost {
        let options = config::LoadOptions {
            user_config_dir: Some(self.user.clone()),
            project_dir: Some(self.root.clone()),
            read_env: false,
            ..Default::default()
        };
        let config = config::Config::load(&options).unwrap();
        let model = workbench_host::bootstrap::models::compose_production_model(
            &config,
            &workbench_host::bootstrap::models::ProductionModel {
                load_options: options,
                credential_store: Arc::new(
                    sandbox::credential::FileCredentialStore::open(self.user.join("credentials"))
                        .unwrap(),
                ),
                bus: self.bus.clone(),
                env: Arc::new(routing::MapEnv::from_iter([(
                    "SCRATCH_CACHE_KEY",
                    "offline-test-key",
                )])),
                model_catalog_dir: Some(test_support::model_catalog_dir()),
            },
        )
        .unwrap();
        BootstrappedHost::open_for_core_with_test_seams(
            BootstrapOptions {
                config_dir: self.user.clone(),
                project_root: self.root.clone(),
                ownership_root: self.directory.sockets().into(),
            },
            TestSeams {
                model,
                sandbox: Arc::new(Direct),
            },
            &evorch_core::recovery_service::CoreGraphRecovery,
        )
        .await
        .unwrap()
    }
    fn response(&self, id: &str) {
        self.mock.enqueue([ScriptedResponse::text_stream(
            id,
            MODEL,
            ["Evidence retained without changing earlier instructions. ".repeat(150)],
        )]);
    }
    fn requests(&self) -> Vec<Value> {
        self.mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == "/v1/responses")
            .map(|r| r.body)
            .collect()
    }
}
async fn phase(events: &mut event_bus::EventReceiver, run: RunId, expected: AgentRunPhase) {
    loop {
        let event = events.recv().await.unwrap();
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to,
            reason,
            ..
        }) = event.kind
        {
            if run_id == run.to_string() {
                assert_ne!(to, AgentRunPhase::Error, "{reason:?}");
                if to == expected {
                    return;
                }
            }
        }
    }
}
fn send(thread: &str, turn: usize) -> SendChat {
    SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        submission_id: SubmissionId::new(format!("{thread}-{turn}")).unwrap(),
        composer_role: ComposerRole::Worker,
        text: format!("{thread}: ordinary turn {turn}"),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }
}
async fn input(f: &Fixture, core: &BootstrappedHost, thread: &str, turn: usize) -> RunId {
    let mut events = core.runtime.event_bus().subscribe();
    f.response(&format!("{thread}-{turn}"));
    let request = send(thread, turn);
    let reply = {
        let mut host = core.host.lock().unwrap();
        if turn == 0 {
            host.reserve_new_chat_thread(&request).unwrap();
        }
        host.handle_command(
            Command::Conversation(ConversationCommand::SendChat(request)),
            local_desktop(),
        )
        .unwrap()
    };
    let run: RunId = reply
        .run_id
        .expect("accepted host input")
        .as_str()
        .parse()
        .unwrap();
    phase(&mut events, run, AgentRunPhase::Waiting).await;
    run
}
async fn resume(core: &BootstrappedHost, thread: &str, run: RunId) {
    let mut events = core.runtime.event_bus().subscribe();
    let reply = core
        .host
        .lock()
        .unwrap()
        .handle_command(
            Command::Conversation(ConversationCommand::ContinueChat(ContinueChat {
                project: ProjectId::new("p").unwrap(),
                thread: ThreadId::new(thread).unwrap(),
                composer_role: ComposerRole::Worker,
                model_preference: None,
            })),
            local_desktop(),
        )
        .unwrap();
    assert_eq!(reply.run_id.unwrap().as_str(), run.to_string());
    phase(&mut events, run, AgentRunPhase::Waiting).await;
}
async fn stop(core: &BootstrappedHost, thread: &str, run: RunId) {
    let mut events = core.runtime.event_bus().subscribe();
    core.host
        .lock()
        .unwrap()
        .handle_command(
            Command::Execution(ExecutionCommand::StopChat(ThreadTarget {
                thread: ThreadId::new(thread).unwrap(),
            })),
            local_desktop(),
        )
        .unwrap();
    phase(&mut events, run, AgentRunPhase::Stopped).await;
    core.runtime.wait(run).await.unwrap();
}
fn notices(request: &Value, directory: &Path) -> usize {
    let marker = format!(
        "[runtime temporary files] {} was emptied",
        directory.display()
    );
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| {
            if let Some(text) = m["content"].as_str() {
                return text.matches(&marker).count();
            }
            m["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|b| b["text"].as_str())
                .map(|text| text.matches(&marker).count())
                .sum()
        })
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_sweep_notifies_each_restored_run_once_and_shared_root_never_claims_a_loss() {
    for shared_root in [false, true] {
        let f = Fixture::new();
        let mut provider_events = f.bus.subscribe();
        let mut first = f.open().await;
        approve_project(&first);
        let mut roots = BTreeMap::new();
        let mut directories = BTreeMap::new();
        let mut affinities = BTreeMap::new();
        for thread in THREADS {
            let run = input(&f, &first, thread, 0).await;
            let directory = first.scratch.thread_dir(thread);
            std::fs::write(
                directory.join("historical-output"),
                "must be retained or actually swept",
            )
            .unwrap();
            let affinity = f.requests().last().unwrap()["prompt_cache_key"]
                .as_str()
                .expect("wire affinity")
                .to_owned();
            assert!(!affinity.is_empty());
            roots.insert(thread, run);
            directories.insert(thread, directory);
            affinities.insert(thread, affinity);
        }
        assert_ne!(affinities[THREADS[0]], affinities[THREADS[1]]);
        first.shutdown().await.unwrap();
        drop(first);
        for directory in directories.values() {
            assert!(directory.join("historical-output").exists());
        }
        // A genuine root usage lease, not a fake sweep flag: the next bootstrap
        // must fall back to shared maintenance and leave every scratch tree alone.
        let shared = shared_root.then(|| {
            storage::shared_with_wait(
                &f.user.join("other.db"),
                f.directory.sockets(),
                Duration::ZERO,
            )
            .unwrap()
        });
        let mut second = f.open().await;
        for thread in THREADS {
            let directory = &directories[thread];
            assert_eq!(
                directory.join("historical-output").exists(),
                shared_root,
                "cold bootstrap sweep"
            );
            resume(&second, thread, roots[thread]).await;
            assert_eq!(input(&f, &second, thread, 1).await, roots[thread]);
            assert_eq!(input(&f, &second, thread, 2).await, roots[thread]);
            stop(&second, thread, roots[thread]).await;
            resume(&second, thread, roots[thread]).await;
            assert_eq!(input(&f, &second, thread, 3).await, roots[thread]);
        }
        let requests = f.requests();
        assert_eq!(requests.len(), THREADS.len() * 4);
        assert_eq!(f.mock.remaining_scripts(), 0);
        let mut usage: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
        let mut completed = 0;
        while completed < requests.len() {
            match provider_events.recv().await.unwrap().kind {
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    run_id: Some(run),
                    input_tokens,
                    cache_read_tokens,
                    ..
                }) => {
                    usage
                        .entry(run)
                        .or_default()
                        .push((input_tokens, cache_read_tokens));
                    completed += 1;
                }
                EventKind::Diagnostic(d) => assert_ne!(d.code, "CacheRegression", "{d:?}"),
                _ => (),
            }
        }
        for thread in THREADS {
            let trace: Vec<_> = requests
                .iter()
                .filter(|r| r["prompt_cache_key"] == affinities[thread])
                .collect();
            assert_eq!(trace.len(), 4, "the restored run retains its wire affinity");
            assert_eq!(notices(trace[0], &directories[thread]), 0);
            for request in &trace[1..] {
                assert_eq!(
                    notices(request, &directories[thread]),
                    usize::from(!shared_root),
                    "one notice per actual cleanup, never per restore/turn"
                );
            }
            for pair in trace.windows(2) {
                assert_append_only(CacheProtocol::Codex, pair[0], pair[1]).unwrap();
            }
            let tokens = &usage[&roots[thread].to_string()];
            assert_eq!(tokens.len(), 4);
            assert_eq!(tokens[0].1, 0);
            for pair in tokens.windows(2) {
                assert!(
                    pair[1].1 >= pair[0].0,
                    "input-derived cache reuses the independently checked sent prefix: {pair:?}"
                );
            }
        }
        second.shutdown().await.unwrap();
        drop(second);
        drop(shared);
    }
}
