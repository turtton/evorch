//! The renderer wiring of PR 16a-2's integration PR adds no tool and changes no
//! cache prefix: the artifact area is wired into the core but its provider
//! factory returns `None`, so the runtime is given no `ArtifactPages` and does not
//! offer `inspect_artifact`. This is the baseline the feature PRs extend:
//! integration-16a2 registers it in the cache gate and the later PRs (F1 owner,
//! F2 artifact API) add their scenarios to this binary. F2 turns the provider on
//! and changes the expectation here deliberately: the tool is then offered from
//! the first request and stays frozen whatever the helper does.
//!
//! The core is bootstrapped (the production composition, including the artifact
//! wiring) and two turns of one conversation cross the production routing and
//! HTTP adapters, for the Chat Completions and the subscription (Claude) wire.
//! The assertions read the recorded requests: the tool array is byte-identical
//! across the turns, the input-derived cache prefix only grows, and
//! `inspect_artifact` is absent. No helper is started and no test waits for time.
use std::sync::Arc;

use event_bus::AgentRunPhase;
use sandbox::CredentialStore as _;
use serde_json::Value;
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

#[tokio::test(flavor = "multi_thread")]
async fn artifact_wiring_leaves_the_tool_catalog_and_the_sent_prefix_unchanged() {
    for subscription in [false, true] {
        let project = cache::Project::new();
        let provider = cache::Provider::new(subscription).await;
        let config = cache::configuration(&provider);
        let root = project.root().canonicalize().unwrap();
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();
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
                    serde_json::json!({
                        "access_token": "fixture-mcp-token",
                        "refresh_token": "fixture-refresh",
                        "expires_at": 4_102_444_800u64
                    })
                    .to_string(),
                ),
            )
            .unwrap();
        let bus = Arc::new(event_bus::EventBus::new(4096));
        let model = workbench_host::bootstrap::models::compose_production_model(
            &config,
            &workbench_host::bootstrap::models::ProductionModel {
                load_options: config::LoadOptions {
                    user_config_dir: Some(user.clone()),
                    ..Default::default()
                },
                credential_store: project.secrets.clone(),
                bus: bus.clone(),
                env: Arc::new(routing::MapEnv::from_iter([(
                    "MCP_CACHE_KEY",
                    "fixture-key",
                )])),
                model_catalog_dir: Some(test_support::model_catalog_dir()),
            },
        )
        .unwrap();
        let mut core = BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: user.clone(),
                project_root: root.clone(),
                ownership_root: project.sockets().to_path_buf(),
            },
            TestSeams {
                model,
                sandbox: Arc::new(Unisolated),
            },
        )
        .await
        .unwrap();
        let mut h = cache::Harness {
            runtime: core.runtime.clone(),
            receiver: core.runtime.event_bus().subscribe(),
        };
        provider.enqueue(vec![cache::answer()]);
        let run: runtime::RunId = {
            let send = SendChat {
                project: ProjectId::new("p").unwrap(),
                thread: ThreadId::new("t1").unwrap(),
                submission_id: SubmissionId::new("00000000-0000-4000-8000-000000000009").unwrap(),
                composer_role: ComposerRole::Worker,
                text: cache::GOAL.into(),
                skills: vec![],
                attachments: vec![],
                model_preference: None,
                fork_seed: None,
            };
            let mut host = core.host.lock().unwrap();
            host.reserve_new_chat_thread(&send).unwrap();
            host.handle_command(
                Command::Conversation(
                    workbench_protocol::namespaces::ConversationCommand::SendChat(send),
                ),
                local_desktop(),
            )
            .unwrap()
            .run_id
            .unwrap()
            .as_str()
            .parse()
            .unwrap()
        };
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        provider.enqueue(vec![cache::answer()]);
        core.runtime
            .send_message(run, "Continue with the artifact area wired".into())
            .unwrap();
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        cache::assert_frozen(&provider, &requests);
        for request in &requests {
            let names = tool_names(&provider, request);
            assert!(!names.is_empty(), "the catalog is not empty");
            assert!(
                !names
                    .iter()
                    .any(|name| name.ends_with(runtime::artifacts::INSPECT_ARTIFACT)),
                "the artifact provider is None until the artifact API PR: {names:?}"
            );
        }
        provider.assert_drained();
        core.runtime
            .stop(run, runtime::StopScope::SelfOnly)
            .unwrap();
        core.runtime.wait(run).await.unwrap();
        core.shutdown().await.unwrap();
    }
}
